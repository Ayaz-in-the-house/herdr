use super::*;

impl HeadlessServer {
    pub(super) fn focused_pane_graphics_demand(&self) -> bool {
        let Some(ws_idx) = self.app.state.active else {
            return false;
        };
        let Some(pane_id) = self
            .app
            .state
            .workspaces
            .get(ws_idx)
            .and_then(crate::workspace::Workspace::focused_pane_id)
        else {
            return false;
        };
        // Two independent graphics paths, exactly as in `pane_is_painting_graphics`: an API layer
        // streamed by a client, or the pane's own terminal emitting Kitty transmissions through
        // its PTY. Terminal-browser and every other native producer only ever takes the second
        // path, and reading just the first one left them on cell-quantised mouse reports.
        if self.app.pane_graphics.active_for_pane(pane_id) {
            return true;
        }
        self.app
            .state
            .runtime_for_pane_in_workspace(&self.app.terminal_runtimes, ws_idx, pane_id)
            .is_some_and(|runtime| runtime.kitty_graphics_generation() > 0)
    }

    fn shell_focused_runtime(
        &self,
        client_id: u64,
    ) -> Option<(&crate::terminal::TerminalRuntime, crate::layout::PaneId)> {
        if self.popup_owner_tab_id == self.shell_tab_id_for_client(client_id) {
            if let Some(popup) = &self.app.state.popup_pane {
                return self
                    .app
                    .terminal_runtimes
                    .get(&popup.terminal_id)
                    .map(|runtime| (runtime, popup.pane_id));
            }
        }
        let target = self.shell_target_for_client(client_id)?;
        let tab = self
            .app
            .state
            .workspaces
            .get(target.workspace_index)?
            .tabs
            .get(target.tab_index)?;
        let pane_id = tab.layout.focused();
        self.app
            .state
            .runtime_for_pane_in_workspace(
                &self.app.terminal_runtimes,
                target.workspace_index,
                pane_id,
            )
            .map(|runtime| (runtime, pane_id))
    }

    pub(super) fn stream_host_mouse_capture_mode(&mut self) {
        let app_mouse_capture = self
            .app
            .state
            .should_capture_host_mouse_from(&self.app.terminal_runtimes);
        let pixel_mouse_requested = self
            .clients
            .values()
            .any(|client| client.is_app_surface_client() && client.pixel_mouse);
        let app_sgr_pixels = pixel_mouse_requested
            && self.focused_pane_graphics_demand()
            && self
                .app
                .state
                .active
                .and_then(|ws_idx| {
                    self.app
                        .state
                        .workspaces
                        .get(ws_idx)
                        .and_then(crate::workspace::Workspace::focused_pane_id)
                        .and_then(|pane_id| {
                            self.app.state.runtime_for_pane_in_workspace(
                                &self.app.terminal_runtimes,
                                ws_idx,
                                pane_id,
                            )
                        })
                })
                .is_some_and(crate::terminal::TerminalRuntime::sgr_pixel_mouse_enabled);
        let requested = self
            .clients
            .iter()
            .filter_map(|(&client_id, client)| match &client.mode {
                ClientConnectionMode::App => Some((
                    client_id,
                    app_mouse_capture,
                    app_sgr_pixels && client.pixel_mouse,
                )),
                ClientConnectionMode::MediaListen => None,
                ClientConnectionMode::ClientShell => {
                    // Each shell reads the tab it is on (U3b), with the fork's two graphics paths:
                    // an API layer, or the pane's own Kitty transmissions through its PTY.
                    let focused = client
                        .shell_surface_active
                        .then(|| self.shell_focused_runtime(client_id))
                        .flatten();
                    let child_requests_mouse =
                        focused.is_some_and(|(runtime, _)| runtime.mouse_reporting_enabled());
                    let sgr_pixels = client.pixel_mouse
                        && focused.is_some_and(|(runtime, pane_id)| {
                            (self.app.pane_graphics.active_for_pane(pane_id)
                                || runtime.kitty_graphics_generation() > 0)
                                && runtime.sgr_pixel_mouse_enabled()
                        });
                    Some((
                        client_id,
                        client.shell_surface_active
                            && (client.shell_mouse_capture || child_requests_mouse),
                        client.shell_surface_active && sgr_pixels,
                    ))
                }
                ClientConnectionMode::TerminalAttach { terminal_id } => {
                    let runtime = self.runtime_for_terminal_id_string(terminal_id);
                    let child_requests_mouse = runtime
                        .is_some_and(crate::terminal::TerminalRuntime::mouse_reporting_enabled);
                    let sgr_pixels = child_requests_mouse
                        && client.pixel_mouse
                        && runtime
                            .is_some_and(crate::terminal::TerminalRuntime::sgr_pixel_mouse_enabled);
                    Some((client_id, child_requests_mouse, sgr_pixels))
                }
                ClientConnectionMode::TerminalPending
                | ClientConnectionMode::TerminalObserve { .. } => None,
            })
            .collect::<Vec<_>>();

        let mut broken_clients = Vec::new();
        for (client_id, enabled, sgr_pixels) in requested {
            let Some(client) = self.clients.get_mut(&client_id) else {
                continue;
            };
            if client.host_mouse_capture_active == Some(enabled)
                && client.host_sgr_pixels_active == Some(sgr_pixels)
            {
                continue;
            }
            let Some(writer) = &client.writer else {
                continue;
            };
            let serialized = match Self::frame_server_message(&ServerMessage::MouseCapture {
                enabled,
                sgr_pixels,
            }) {
                Ok(framed) => framed,
                Err(err) => {
                    warn!(err = %err, "failed to serialize mouse capture mode for client");
                    continue;
                }
            };
            if writer.control.send(serialized).is_err() {
                debug!(
                    client_id,
                    "client writer channel closed during mouse capture update"
                );
                broken_clients.push(client_id);
                continue;
            }
            client.host_mouse_capture_active = Some(enabled);
            client.host_sgr_pixels_active = Some(sgr_pixels);
        }

        for client_id in broken_clients {
            self.remove_client_and_resize_if_needed(client_id);
        }
    }

    pub(super) fn stream_direct_terminal_keyboard_mode(&mut self) {
        let shell_modes = self
            .clients
            .iter()
            .filter(|(_, client)| client.is_shell_client())
            .map(|(&client_id, client)| {
                let report_all = client.shell_surface_active
                    && self
                        .shell_focused_runtime(client_id)
                        .is_some_and(|(runtime, _)| {
                            let protocol = runtime.keyboard_protocol();
                            protocol.reports_all_keys()
                                || (protocol.reports_event_types()
                                    && runtime.modify_other_keys_level() > 0)
                        });
                (client_id, report_all)
            })
            .collect::<Vec<_>>();
        let mut broken_clients = Vec::new();
        for (client_id, report_all) in shell_modes {
            let Some(client) = self.clients.get_mut(&client_id) else {
                continue;
            };
            if client.host_keyboard_report_all_active == Some(report_all) {
                continue;
            }
            let Some(writer) = &client.writer else {
                continue;
            };
            let serialized = match Self::frame_server_message(
                &ServerMessage::ClientShellKeyboardReportAll {
                    enabled: report_all,
                },
            ) {
                Ok(serialized) => serialized,
                Err(err) => {
                    warn!(err = %err, "failed to serialize client shell keyboard report-all mode");
                    continue;
                }
            };
            if writer.control.send(serialized).is_err() {
                broken_clients.push(client_id);
                continue;
            }
            client.host_keyboard_report_all_active = Some(report_all);
        }

        let requested = self
            .clients
            .iter()
            .filter_map(|(&client_id, client)| {
                let ClientConnectionMode::TerminalAttach { terminal_id } = &client.mode else {
                    return None;
                };
                let (flags, modify_other_keys_level) = self
                    .runtime_for_terminal_id_string(terminal_id)
                    .map_or((0, 0), |runtime| {
                        let flags = match runtime.keyboard_protocol() {
                            crate::input::KeyboardProtocol::Legacy => 0,
                            crate::input::KeyboardProtocol::Kitty { flags } => flags,
                        };
                        (flags, runtime.modify_other_keys_level())
                    });
                Some((client_id, flags, modify_other_keys_level))
            })
            .collect::<Vec<_>>();

        for (client_id, flags, modify_other_keys_level) in requested {
            let Some(client) = self.clients.get_mut(&client_id) else {
                continue;
            };
            if client.host_keyboard_protocol_active == Some((flags, modify_other_keys_level)) {
                continue;
            }
            let Some(writer) = &client.writer else {
                continue;
            };
            let serialized =
                match Self::frame_server_message(&ServerMessage::DirectTerminalKeyboardProtocol {
                    flags,
                    modify_other_keys_level,
                }) {
                    Ok(framed) => framed,
                    Err(err) => {
                        warn!(err = %err, "failed to serialize direct terminal keyboard mode");
                        continue;
                    }
                };
            if writer.control.send(serialized).is_err() {
                debug!(
                    client_id,
                    "client writer channel closed during direct terminal keyboard update"
                );
                broken_clients.push(client_id);
                continue;
            }
            client.host_keyboard_protocol_active = Some((flags, modify_other_keys_level));
        }

        for client_id in broken_clients {
            self.remove_client_and_resize_if_needed(client_id);
        }
    }

    pub(super) fn has_pending_presentation_work(
        &self,
        needs_full_render: bool,
        needs_graphics_render: bool,
    ) -> bool {
        needs_full_render || needs_graphics_render || self.app.render_dirty.has_immediate_work()
    }

    pub(super) fn sync_immediate_pty_sources(&self) {
        let (has_app_target, direct_terminal_targets) = self.pty_render_targets();
        // App displays watch the session surface (fork); each shell watches its own tab (U3b).
        let mut pane_ids = if has_app_target {
            self.app.state.app_surface_pane_ids()
        } else {
            HashSet::new()
        };
        if has_app_target {
            for (&client_id, client) in &self.clients {
                if !client.is_active_shell_client() || client.writer.is_none() {
                    continue;
                }
                let Some(target) = self.shell_target_for_client(client_id) else {
                    continue;
                };
                let Some(tab) = self
                    .app
                    .state
                    .workspaces
                    .get(target.workspace_index)
                    .and_then(|workspace| workspace.tabs.get(target.tab_index))
                else {
                    continue;
                };
                if tab.zoomed {
                    pane_ids.insert(tab.layout.focused());
                } else {
                    pane_ids.extend(tab.layout.pane_ids());
                }
                if self.popup_owner_tab_id == self.shell_tab_id_for_client(client_id) {
                    if let Some(popup) = &self.app.state.popup_pane {
                        pane_ids.insert(popup.pane_id);
                    }
                }
            }
        }
        if !direct_terminal_targets.is_empty() {
            for workspace in &self.app.state.workspaces {
                for tab in &workspace.tabs {
                    pane_ids.extend(tab.panes.iter().filter_map(|(&pane_id, pane)| {
                        direct_terminal_targets
                            .contains(pane.attached_terminal_id.as_str())
                            .then_some(pane_id)
                    }));
                }
            }
            if let Some(popup) = &self.app.state.popup_pane {
                if direct_terminal_targets.contains(popup.terminal_id.as_str()) {
                    pane_ids.insert(popup.pane_id);
                }
            }
        }
        self.app.render_dirty.set_immediate_pty_sources(pane_ids);
    }

    fn pty_render_targets(&self) -> (bool, HashSet<&str>) {
        let mut has_app_target = false;
        let mut direct_terminal_targets = HashSet::new();
        for client in self
            .clients
            .values()
            .filter(|client| client.writer.is_some())
        {
            match &client.mode {
                ClientConnectionMode::App | ClientConnectionMode::ClientShell
                    if client.is_app_surface_client() =>
                {
                    has_app_target = true;
                }
                ClientConnectionMode::TerminalAttach { terminal_id }
                | ClientConnectionMode::TerminalObserve { terminal_id } => {
                    direct_terminal_targets.insert(terminal_id.as_str());
                }
                // Neither an app target nor a terminal's: a media listener
                // asked for audio, so no pty output is rendered on its
                // account. Same answer as an app client mid-attach. A client-
                // rendered shell draws its own surfaces (U2a).
                ClientConnectionMode::App
                | ClientConnectionMode::ClientShell
                | ClientConnectionMode::MediaListen
                | ClientConnectionMode::TerminalPending => {}
            }
        }
        (has_app_target, direct_terminal_targets)
    }

    fn pty_source_visible_to_render_targets(
        &self,
        pane_id: crate::layout::PaneId,
        has_app_target: bool,
        direct_terminal_targets: &HashSet<&str>,
    ) -> bool {
        let terminal_id = self.terminal_id_for_pane(pane_id);
        (has_app_target
            && (terminal_id.is_none()
                || self.app_surface_contains_pane(pane_id)
                || self.any_shell_surface_contains_pane(pane_id)))
            || terminal_id.is_none_or(|source| direct_terminal_targets.contains(source.as_str()))
    }

    pub(super) fn pty_sources_visible_to_any_render_target(
        &self,
        sources: &HashSet<crate::layout::PaneId>,
    ) -> bool {
        let (has_app_target, direct_terminal_targets) = self.pty_render_targets();
        if !has_app_target && direct_terminal_targets.is_empty() {
            return false;
        }

        sources.iter().copied().any(|pane_id| {
            self.pty_source_visible_to_render_targets(
                pane_id,
                has_app_target,
                &direct_terminal_targets,
            )
        })
    }

    fn terminal_id_for_pane(
        &self,
        pane_id: crate::layout::PaneId,
    ) -> Option<&crate::terminal::TerminalId> {
        if let Some(popup) = self
            .app
            .state
            .popup_pane
            .as_ref()
            .filter(|popup| popup.pane_id == pane_id)
        {
            return Some(&popup.terminal_id);
        }
        self.app
            .find_pane(pane_id)
            .map(|(_, pane)| &pane.attached_terminal_id)
    }

    fn app_surface_contains_pane(&self, pane_id: crate::layout::PaneId) -> bool {
        if self
            .app
            .state
            .popup_pane
            .as_ref()
            .is_some_and(|popup| popup.pane_id == pane_id)
        {
            return true;
        }
        let Some(workspace) = self
            .app
            .state
            .active
            .and_then(|ws_idx| self.app.state.workspaces.get(ws_idx))
        else {
            return false;
        };
        let Some(tab) = workspace.active_tab() else {
            return false;
        };
        if !tab.panes.contains_key(&pane_id) {
            return false;
        }
        !tab.zoomed || tab.layout.focused() == pane_id
    }

    fn any_shell_surface_contains_pane(&self, pane_id: crate::layout::PaneId) -> bool {
        self.clients.iter().any(|(&client_id, client)| {
            if !client.is_active_shell_client() || client.writer.is_none() {
                return false;
            }
            if self
                .app
                .state
                .popup_pane
                .as_ref()
                .is_some_and(|popup| popup.pane_id == pane_id)
            {
                return self.popup_owner_tab_id == self.shell_tab_id_for_client(client_id);
            }
            let Some(target) = self.shell_target_for_client(client_id) else {
                return false;
            };
            let Some(tab) = self
                .app
                .state
                .workspaces
                .get(target.workspace_index)
                .and_then(|workspace| workspace.tabs.get(target.tab_index))
            else {
                return false;
            };
            tab.panes.contains_key(&pane_id) && (!tab.zoomed || tab.layout.focused() == pane_id)
        })
    }

    pub(super) fn render_and_stream(&mut self) {
        // Consumed per call and re-armed: callers outside the loop (tests,
        // deferred paths) keep the default re-seat. TP-GFX-REPLAY-01
        let reseat_placements = std::mem::replace(&mut self.full_render_reseats_placements, true);
        let full_started = crate::render_prof::timer();
        let render_targets = render_targets(&self.clients, self.foreground_client_id);
        let negotiated_tab_sizes = self.negotiated_tab_sizes();

        if render_targets.is_empty() {
            // Geometry is established once so the API and the first attach
            // see real pane sizes; after that a tick with no client attached
            // has nobody to draw for, and PTY output alone must not buy a
            // full compute-and-discard frame — measured live at up to 62
            // thrown-away frames a second on detached servers. Size changes
            // while detached re-enter through the size-change event path,
            // which recomputes the view itself. TP-MCF-SIZE-06
            let resize_panes = self.app.state.view.pane_infos.is_empty();
            if !resize_panes {
                self.app.full_redraw_pending = false;
                return;
            }
            let (cols, rows) = self.effective_size;
            let area = Rect::new(0, 0, cols, rows);
            #[cfg(test)]
            {
                self.watcherless_virtual_frames += 1;
            }
            let render_started = crate::render_prof::timer();
            let _ = crate::server::render_stream::render_virtual_with_runtime_registry(
                &mut self.app.state,
                &self.app.terminal_runtimes,
                area,
                resize_panes,
                crate::kitty_graphics::HostCellSize::default(),
            );
            crate::render_prof::duration_since("full_render.render_virtual", render_started);
            self.app.full_redraw_pending = false;
            crate::render_prof::duration_since("full_render.total", full_started);
            debug!(
                cols,
                rows, resize_panes, "rendered virtual frame with no attached clients"
            );
            return;
        }

        let mut broken_clients: Vec<u64> = Vec::new();
        // Set only where a graphics continuation is deferred WITHOUT a send: nothing
        // reaches the writer, so no `ClientWriterDrained` will bring the client back.
        // Every send path recovers per connection instead (upstream #3487).
        let mut deferred_frame = false;
        // Pane size no longer follows the foreground client: it follows the
        // set of displays watching each tab. TP-MCF-SIZE-01
        for (client_id, (cols, rows), cell_size, is_foreground, mode) in render_targets {
            let area = Rect::new(0, 0, cols, rows);
            // Filled inside the App arm, where the encode runs in this
            // client's viewer window; committed after a successful send.
            let mut encoded_graphics_cache: Option<crate::kitty_graphics::HostGraphicsCache> = None;
            // Rides beside the cache: whether the app-frame encoding above
            // left transactions for a later tick (upstream incomplete-loop),
            // reset by the same take() rhythm the cache uses.
            let mut frame_graphics_incomplete = false;
            let mut attach_wheel_routing: Option<crate::protocol::TerminalWheelRouting> = None;

            let shell_target = self.shell_target_for_client(client_id);
            let shell_tab_id = self.shell_tab_id_for_client(client_id);
            let shell_shows_popup = shell_tab_id.as_deref() == self.popup_owner_tab_id.as_deref();
            let mut shell_projection_revision = 0;
            if matches!(mode, ClientConnectionMode::ClientShell) {
                let location = self
                    .clients
                    .get(&client_id)
                    .and_then(|client| client.shell_location.clone());
                let Some(client) = self.clients.get_mut(&client_id) else {
                    continue;
                };
                let mut candidate = client_shell_snapshot(
                    &self.app,
                    &self.client_shell_boot_id,
                    client.shell_projection_revision,
                    None,
                    location.as_ref(),
                );
                candidate.config_diagnostic = if client.shell_uses_endpoint_keybindings {
                    self.server_config_diagnostic.clone()
                } else {
                    self.server_config_diagnostic_without_keybindings.clone()
                };
                candidate.revision = client.shell_projection_revision;
                if client.shell_snapshot.as_ref() != Some(&candidate) {
                    client.shell_projection_revision =
                        client.shell_projection_revision.saturating_add(1);
                    candidate.revision = client.shell_projection_revision;
                    let message = match crate::protocol::endpoint::snapshot_message(&candidate) {
                        Ok(message) => message,
                        Err(err) => {
                            warn!(client_id, err = %err, "failed to encode endpoint snapshot");
                            broken_clients.push(client_id);
                            continue;
                        }
                    };
                    let framed = match Self::frame_server_message(&message) {
                        Ok(framed) => framed,
                        Err(err) => {
                            warn!(client_id, err = %err, "failed to frame endpoint snapshot");
                            broken_clients.push(client_id);
                            continue;
                        }
                    };
                    let Some(writer) = client.writer.as_ref() else {
                        broken_clients.push(client_id);
                        continue;
                    };
                    if writer.control.send(framed).is_err() {
                        broken_clients.push(client_id);
                        continue;
                    }
                    client.shell_snapshot = Some(candidate);
                }
                shell_projection_revision = client.shell_projection_revision;
                if !client.shell_surface_active {
                    client.clear_deferred_render();
                    continue;
                }
            }
            let shell_graphics_delivery = self
                .clients
                .get(&client_id)
                .map(|client| client.shell_graphics_delivery.clone())
                .unwrap_or_default();
            let mut surface_parts = None;
            let mut frame = match mode {
                // A media listener is never rendered for. `render_targets`
                // already excludes it, so this arm is the second lock rather
                // than a duplicate of the first: if that filter ever loosens,
                // the cost is a frame encoded and sent to a consumer that
                // discards it, and nothing would report it. An arm rather than
                // a guard above the match, because then the compiler is what
                // keeps the two in step. `continue` rather than
                // `unreachable!` — an invariant worth asserting is not worth
                // crashing a session over.
                ClientConnectionMode::MediaListen | ClientConnectionMode::TerminalPending => {
                    continue
                }
                ClientConnectionMode::App => {
                    // Render resolves this client's view for the whole arm.
                    // The arm has no early exit, so the single restore below
                    // always runs. TP-MCF-CTX-03
                    let previous_viewer = self.app.state.enter_viewer(Some(client_id));
                    let render_started = crate::render_prof::timer();
                    let render_cell_size =
                        if self.app.state.kitty_graphics_enabled && cell_size.is_known() {
                            cell_size
                        } else {
                            crate::kitty_graphics::HostCellSize::default()
                        };
                    // Resize this client's tab first when it owns the size,
                    // then draw at the client's own area without resizing
                    // again. Splitting the two is what lets a tab be sized to
                    // the smallest display watching it while every display
                    // still draws at its own dimensions.
                    let negotiated = negotiated_tab_sizes.get(&client_id).copied();
                    // A tab only this display is watching negotiates to this
                    // display's own size, so the resize can happen inside the
                    // render pass exactly as it always did. The separate pass
                    // is only for a shared tab, where the negotiated size is
                    // not the size being drawn -- that is the one case herdr
                    // cannot express with a single area.
                    let resize_while_rendering = negotiated == Some((cols, rows));
                    if let Some((resize_cols, resize_rows)) = negotiated {
                        if !resize_while_rendering {
                            crate::ui::compute_view_skipping_background_tabs(
                                &mut self.app.state,
                                &self.app.terminal_runtimes,
                                Rect::new(0, 0, resize_cols, resize_rows),
                                render_cell_size,
                            );
                        }
                    }
                    let preserved_scroll = (!is_foreground).then_some((
                        self.app.state.workspace_scroll,
                        self.app.state.agent_panel_scroll,
                        self.app.state.tab_scroll,
                        self.app.state.mobile_switcher_scroll,
                    ));
                    let (buffer, cursor) =
                        crate::server::render_stream::render_virtual_with_runtime_registry(
                            &mut self.app.state,
                            &self.app.terminal_runtimes,
                            area,
                            resize_while_rendering,
                            render_cell_size,
                        );
                    self.view_owner = Some((client_id, (cols, rows)));
                    if let Some((workspace, agent_panel, tab, mobile_switcher)) = preserved_scroll {
                        self.app.state.workspace_scroll = workspace;
                        self.app.state.agent_panel_scroll = agent_panel;
                        self.app.state.tab_scroll = tab;
                        self.app.state.mobile_switcher_scroll = mobile_switcher;
                    }
                    crate::render_prof::duration_since(
                        "full_render.render_virtual",
                        render_started,
                    );
                    let hyperlinks_started = crate::render_prof::timer();
                    let hyperlinks = crate::server::render_stream::visible_hyperlinks(
                        &self.app.state,
                        &self.app.terminal_runtimes,
                    );
                    crate::render_prof::duration_since(
                        "full_render.visible_hyperlinks",
                        hyperlinks_started,
                    );
                    let frame_started = crate::render_prof::timer();
                    let mut frame = FrameData::from_ratatui_buffer_with_hyperlinks(
                        &buffer,
                        cursor,
                        &hyperlinks,
                    );
                    crate::render_prof::duration_since("full_render.frame_build", frame_started);
                    // Graphics are derived from this client's view exactly as
                    // the text cells are, so they are encoded inside the same
                    // viewer window the frame was rendered in. Encoding after
                    // the restore reads whichever view the restore installed —
                    // with several displays that is the session default, whose
                    // owned surfaces hold no file browser, so the preview a
                    // display was looking at never reached it. TP-MCF-CTX-06
                    if let Some(client) = self.clients.get_mut(&client_id) {
                        let mut next_graphics_cache = client.graphics_cache.clone();
                        if self.app.state.kitty_graphics_enabled && cell_size.is_known() {
                            if client.graphics_surface_reset_pending {
                                frame.graphics = crate::kitty_graphics::reset_barrier_bytes(
                                    &mut next_graphics_cache,
                                );
                            }
                            // Whether this frame re-seats cached pictures:
                            // every real full cause does — its text blit
                            // overdraws the placements (TP-GFX-RESIZE-01
                            // reseat half; the divider-release repaint rides
                            // this road) — while a text-refresh-only frame
                            // keeps them seated: the semantic client blits
                            // only the changed status cells, and a re-seat
                            // per tick is graphics bytes on the wire reading
                            // as the picture blinking. TP-GFX-REPLAY-01
                            let reseat = reseat_placements || client.graphics_surface_reset_pending;
                            let graphics_started = crate::render_prof::timer();
                            let encoded = crate::kitty_graphics::encode_local_pane_graphics(
                                &self.app.state,
                                &self.app.pane_graphics,
                                &self.app.terminal_runtimes,
                                self.app.state.view.tab_surface(),
                                cell_size,
                                Some(crate::kitty_graphics::HEADLESS_GRAPHICS_TRANSACTION_BUDGET),
                                reseat,
                                client_id,
                                &mut next_graphics_cache,
                            );
                            frame.graphics.extend(encoded.bytes);
                            frame_graphics_incomplete = encoded.incomplete;
                            crate::render_prof::duration_since(
                                "full_render.graphics_encode",
                                graphics_started,
                            );
                        } else {
                            frame.graphics = next_graphics_cache.clear_bytes();
                        }
                        encoded_graphics_cache = Some(next_graphics_cache);
                    }
                    self.app.state.restore_viewer(previous_viewer);
                    frame
                }
                ClientConnectionMode::ClientShell => {
                    let render_started = crate::render_prof::timer();
                    let render_cell_size = if cell_size.is_known() {
                        cell_size
                    } else {
                        crate::kitty_graphics::HostCellSize::default()
                    };
                    let crate::server::client_shell::RenderedPaneSurface {
                        frame,
                        panes,
                        splits,
                        popup,
                        graphics,
                        graphics_delivery: next_graphics_delivery,
                    } = render_client_shell_pane_surface(
                        &mut self.app,
                        shell_target,
                        area,
                        false,
                        shell_shows_popup,
                        render_cell_size,
                        &shell_graphics_delivery,
                        client_id,
                    );
                    crate::render_prof::duration_since(
                        "full_render.render_tab_surface_virtual",
                        render_started,
                    );
                    surface_parts = Some((panes, splits, popup, graphics, next_graphics_delivery));
                    frame
                }
                ClientConnectionMode::TerminalAttach { terminal_id }
                | ClientConnectionMode::TerminalObserve { terminal_id } => {
                    let Some(runtime) = self.runtime_for_terminal_id_string(&terminal_id) else {
                        self.send_to_client(
                            client_id,
                            ServerMessage::ServerShutdown {
                                reason: Some(format!(
                                    "terminal attach ended: terminal {terminal_id} not found"
                                )),
                            },
                        );
                        broken_clients.push(client_id);
                        continue;
                    };
                    // The routing travels with the render tick: a mode flip
                    // (vim opening, claude entering its alt screen) always
                    // redraws, so piggybacking on the tick catches every
                    // change a client could observe — and the None-to-Some
                    // edge on the first tick doubles as the attach greeting.
                    attach_wheel_routing = runtime
                        .wheel_routing()
                        .map(crate::protocol::TerminalWheelRouting::from);
                    let render_started = crate::render_prof::timer();
                    let (buffer, cursor) =
                        crate::server::render_stream::render_terminal_virtual(runtime, area);
                    crate::render_prof::duration_since(
                        "full_render.render_terminal_virtual",
                        render_started,
                    );
                    let hyperlinks_started = crate::render_prof::timer();
                    let hyperlinks = runtime.visible_hyperlinks(area);
                    crate::render_prof::duration_since(
                        "full_render.visible_hyperlinks",
                        hyperlinks_started,
                    );
                    let frame_started = crate::render_prof::timer();
                    let frame = FrameData::from_ratatui_buffer_with_hyperlinks(
                        &buffer,
                        cursor,
                        &hyperlinks,
                    );
                    crate::render_prof::duration_since("full_render.frame_build", frame_started);
                    frame
                }
            };

            if let Some(now) = attach_wheel_routing {
                let already = self
                    .clients
                    .get(&client_id)
                    .and_then(|client| client.terminal_wheel_routing_sent);
                if already != Some(now) {
                    self.send_to_client(client_id, ServerMessage::TerminalRouting { routing: now });
                    if let Some(client) = self.clients.get_mut(&client_id) {
                        client.terminal_wheel_routing_sent = Some(now);
                    }
                }
            }

            let Some(client) = self.clients.get_mut(&client_id) else {
                continue;
            };
            // What this link can carry inside a quarter second: the measured
            // drain rate turns the fixed graphics ceiling into a per-client
            // one. Unmeasured (local, fresh) keeps the full ceiling —
            // fail-open by design (TP-GFX-BUDGET-02).
            let client_graphics_max = client
                .writer
                .as_ref()
                .map_or(MAX_GRAPHICS_FRAME_SIZE, |writer| {
                    writer.rate.effective_graphics_max()
                });
            // App frames arrive with their graphics already encoded (inside
            // the viewer window, above). Everything else carries none and
            // clears its cache exactly as before.
            let next_graphics_cache = match encoded_graphics_cache.take() {
                Some(cache) => cache,
                None => {
                    let mut cache = client.graphics_cache.clone();
                    frame.graphics = cache.clear_bytes();
                    cache
                }
            };
            let mut encoded_incomplete = std::mem::take(&mut frame_graphics_incomplete);

            let Some(writer) = client.writer.as_ref().cloned() else {
                crate::render_prof::event("full_render.writer_missing");
                continue;
            };
            let mut commit_graphics_cache = true;
            if frame.graphics.len() > MAX_GRAPHICS_FRAME_SIZE {
                warn!(
                    client_id,
                    graphics_bytes = frame.graphics.len(),
                    max = MAX_GRAPHICS_FRAME_SIZE,
                    "dropping oversized graphics payload for client frame"
                );
                frame.graphics.clear();
                commit_graphics_cache = false;
                encoded_incomplete = false;
                // Dropped deletes would orphan the outer terminal's images
                // forever; the next frame rebuilds from the barrier instead.
                // TP-GFX-RESET-01
                client.graphics_surface_reset_pending = true;
            }
            let has_graphics = !frame.graphics.is_empty()
                || surface_parts
                    .as_ref()
                    .is_some_and(|(_, _, _, graphics, _)| {
                        !graphics.assets.is_empty()
                            || !graphics.placements.is_empty()
                            || !graphics.retained_assets.is_empty()
                    });
            let mut next_shell_graphics_delivery = None;
            // U2a: a client-rendered shell gets upstream's pane-surface frame (an identical one is skipped as
            // upstream skips it); an App client keeps the fork's frame path exactly (TP-GFX-CONVERGE-01).
            let mut prepared =
                if let Some((panes, splits, popup, graphics, delivery)) = surface_parts {
                    next_shell_graphics_delivery = Some(delivery);
                    let Some(prepared) =
                        client
                            .render_state
                            .prepare_pane_surface(protocol::PaneSurfaceFrame {
                                boot_id: self.client_shell_boot_id.clone(),
                                projection_revision: shell_projection_revision,
                                surface_revision: 0,
                                frame,
                                panes,
                                splits,
                                popup,
                                graphics,
                            })
                    else {
                        if commit_graphics_cache {
                            client.graphics_cache = next_graphics_cache;
                            client.graphics_surface_reset_pending = false;
                        }
                        if encoded_incomplete {
                            client.defer_full_render();
                            deferred_frame = true;
                        } else {
                            client.clear_deferred_render();
                        }
                        crate::render_prof::event("full_render.skip_identical");
                        continue;
                    };
                    prepared
                } else {
                    match client.render_state.prepare_frame(frame) {
                        Ok(prepared) => prepared,
                        Err(same_frame) => {
                            // The text cells match the last sent frame, but the
                            // graphics bytes may still carry work the outer terminal
                            // has not seen: the re-clip after a pane resize, or the
                            // delete for a vanished source. Dropping them while
                            // committing the cache convinced the server the client
                            // was clean, and the wide placement stayed on the user's
                            // screen forever (the stale strip in the 2026-08-27
                            // screenshot). TP-GFX-CONVERGE-01
                            let graphics = same_frame.graphics;
                            if graphics.is_empty() {
                                if commit_graphics_cache {
                                    client.graphics_cache = next_graphics_cache;
                                    client.graphics_surface_reset_pending = false;
                                }
                                if encoded_incomplete {
                                    client.defer_full_render();
                                    deferred_frame = true;
                                } else {
                                    client.clear_deferred_render();
                                }
                                crate::render_prof::event("full_render.skip_identical");
                                continue;
                            }
                            match Self::frame_server_message_with_max(
                                &ServerMessage::Graphics {
                                    bytes: pane_graphics::frame_pane_graphics(graphics),
                                },
                                client_graphics_max,
                            ) {
                                Ok(serialized) => match writer.render.try_send(serialized) {
                                    Ok(()) => {
                                        if commit_graphics_cache {
                                            client.graphics_cache = next_graphics_cache;
                                            client.graphics_surface_reset_pending = false;
                                        }
                                        if encoded_incomplete {
                                            client.defer_full_render();
                                        } else {
                                            client.clear_deferred_render();
                                        }
                                        crate::render_prof::event(
                                            "full_render.graphics_after_identical_text",
                                        );
                                    }
                                    Err(std::sync::mpsc::TrySendError::Full(_)) => {
                                        client.defer_full_render();
                                    }
                                    Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                                        broken_clients.push(client_id);
                                    }
                                },
                                Err(_) => {
                                    // Same orphan risk as the oversized drop above:
                                    // the next frame rebuilds from the barrier.
                                    // TP-GFX-RESET-01
                                    client.graphics_surface_reset_pending = true;
                                }
                            }
                            continue;
                        }
                    }
                };
            let max = if has_graphics {
                client_graphics_max
            } else {
                crate::protocol::MAX_FRAME_SIZE
            };
            let mut shell_assets_deferred = false;
            let serialized = match Self::frame_server_message_with_max(prepared.message(), max) {
                Ok(frame) => frame,
                Err(protocol::FramingError::Oversized { claimed, max }) if has_graphics => {
                    warn!(
                        client_id,
                        claimed, max, "dropping graphics from oversized frame for client"
                    );
                    let framed = if prepared.strip_pane_surface_assets() {
                        next_shell_graphics_delivery = None;
                        shell_assets_deferred = true;
                        Self::frame_server_message(prepared.message())
                    } else {
                        let Some(mut text_only_frame) = prepared.into_frame() else {
                            crate::render_prof::event("full_render.serialize_error");
                            continue;
                        };
                        text_only_frame.graphics.clear();
                        let Ok(text_only_prepared) =
                            client.render_state.prepare_frame(text_only_frame)
                        else {
                            client.clear_deferred_render();
                            crate::render_prof::event("full_render.skip_identical_text_only");
                            continue;
                        };
                        let result = Self::frame_server_message(text_only_prepared.message());
                        prepared = text_only_prepared;
                        result
                    };
                    let framed = match framed {
                        Ok(framed) => framed,
                        Err(err) => {
                            warn!(client_id, err = %err, "failed to serialize text-only frame for client");
                            broken_clients.push(client_id);
                            crate::render_prof::event("full_render.serialize_error");
                            continue;
                        }
                    };
                    commit_graphics_cache = false;
                    encoded_incomplete = false;
                    // Same orphan risk as the oversized drop above. TP-GFX-RESET-01
                    client.graphics_surface_reset_pending = true;
                    framed
                }
                Err(protocol::FramingError::Oversized { claimed, max }) => {
                    warn!(
                        client_id,
                        claimed, max, "skipping oversized frame for client"
                    );
                    crate::render_prof::event("full_render.serialize_oversized");
                    continue;
                }
                Err(err) => {
                    warn!(client_id, err = %err, "failed to serialize frame");
                    broken_clients.push(client_id);
                    crate::render_prof::event("full_render.serialize_error");
                    continue;
                }
            };
            let shell_graphics_pending = next_shell_graphics_delivery
                .as_ref()
                .is_some_and(crate::kitty_graphics::surface::DeliveryCache::has_pending);
            match writer.render.try_send(serialized) {
                Ok(()) => {
                    if commit_graphics_cache {
                        client.graphics_cache = next_graphics_cache;
                        client.graphics_surface_reset_pending = false;
                    }
                    if let Some(delivery) = next_shell_graphics_delivery {
                        client.shell_graphics_delivery = delivery;
                    }
                    client.render_state.commit_sent_frame(prepared);
                    if encoded_incomplete || shell_graphics_pending || shell_assets_deferred {
                        client.defer_full_render();
                    } else {
                        client.clear_deferred_render();
                    }
                    crate::render_prof::event("full_render.sent");
                }
                Err(std::sync::mpsc::TrySendError::Full(_)) => {
                    client.defer_full_render();
                }
                Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                    broken_clients.push(client_id);
                }
            }
        }

        if !broken_clients.is_empty() {
            for client_id in broken_clients {
                self.remove_client_and_resize_if_needed(client_id);
            }
        }

        let (cols, rows) = self.effective_size;
        if !deferred_frame {
            self.app.full_redraw_pending = false;
        }

        // Publish where the panes ended up, to whoever asked for that.
        //
        // Here rather than at a list of layout-event sites: `view.pane_infos`
        // is final at this point, and a comparison against the last published
        // set cannot miss an event the way an enumeration of events can. A
        // frame that moved nothing sends nothing. TP-GEOM-REACH-01
        self.fan_out_pane_geometry();

        crate::render_prof::duration_since("full_render.total", full_started);
        debug!(cols, rows, foreground_client_id = ?self.foreground_client_id, "rendered virtual frame(s)");
    }
}
