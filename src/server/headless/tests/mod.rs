use super::*;
use crate::app::test_wait::LoadAwareDeadline;

use crate::app::AppState;
use crate::protocol::{CellData, CursorState};
use unicode_width::UnicodeWidthStr;

#[path = "pane_audio.rs"]
mod pane_audio_tests;
#[path = "pane_geometry.rs"]
mod pane_geometry_tests;
#[path = "pane_graphics.rs"]
mod pane_graphics_tests;
#[path = "pane_video.rs"]
mod pane_video_tests;

#[test]
fn retained_render_plan_covers_each_render_path() {
    assert_eq!(
        retained_render_plan(RetainedRenderInput {
            needs_full_render: true,
            needs_graphics_render: true,
            pty: PtyRenderState::Hidden,
        }),
        RetainedRenderPlan::Full
    );
    assert_eq!(
        retained_render_plan(RetainedRenderInput {
            needs_full_render: false,
            needs_graphics_render: true,
            pty: PtyRenderState::Hidden,
        }),
        RetainedRenderPlan::Graphics
    );
    assert_eq!(
        retained_render_plan(RetainedRenderInput {
            needs_full_render: false,
            needs_graphics_render: false,
            pty: PtyRenderState::Visible,
        }),
        RetainedRenderPlan::Pty
    );
    assert_eq!(
        retained_render_plan(RetainedRenderInput {
            needs_full_render: false,
            needs_graphics_render: false,
            pty: PtyRenderState::Hidden,
        }),
        RetainedRenderPlan::HiddenPty
    );
}

// TP-SRV-HK-01: the housekeeping pass (scheduled tasks + deferred
// requests) runs at most once per HEADLESS_HOUSEKEEPING_MIN_INTERVAL.
// Without the gate, every PTY wake pays the full per-display surface
// swap, which was measured saturating one core under busy panes.
#[test]
fn housekeeping_pass_is_rate_limited_within_the_min_interval() {
    let mut server = test_headless_server();
    let t0 = Instant::now();
    assert!(
        server.housekeeping_pass_due(t0),
        "the first pass must always run"
    );
    assert!(
        !server.housekeeping_pass_due(t0 + Duration::from_millis(2)),
        "a second pass inside the interval must be skipped"
    );
    assert!(
        server.housekeeping_pass_due(t0 + HEADLESS_HOUSEKEEPING_MIN_INTERVAL),
        "the pass must run again once the interval has elapsed"
    );
}

// TP-SRV-HK-02: a skipped pass schedules a wake within the interval, so
// deferred work is never starved; a pass that ran schedules nothing, so
// an idle server still sleeps indefinitely.
#[test]
fn a_skipped_housekeeping_pass_schedules_a_wake_within_the_interval() {
    let mut server = test_headless_server();
    let t0 = Instant::now();
    assert!(server.housekeeping_pass_due(t0));
    assert_eq!(
        server.housekeeping_wake_after(true),
        None,
        "a pass that ran must not force the loop awake"
    );
    assert!(!server.housekeeping_pass_due(t0 + Duration::from_millis(1)));
    assert_eq!(
        server.housekeeping_wake_after(false),
        Some(t0 + HEADLESS_HOUSEKEEPING_MIN_INTERVAL),
        "a skipped pass must be retried within the interval"
    );
}

// TP-SRV-HK-03: a loop deadline already in the past is clamped to the
// housekeeping cadence. Every deadline in the select chain is consumed
// by the housekeeping pass, so a past-due deadline can only mean "the
// pass will handle it on its next run" — waking sooner is a pure spin.
// Measured live: one unserviced deadline turned the loop 167,000
// times per second.
#[test]
fn a_past_due_loop_deadline_is_clamped_to_the_housekeeping_cadence() {
    let mut server = test_headless_server();
    let now = Instant::now();
    assert!(server.housekeeping_pass_due(now));
    let past = now - Duration::from_secs(1);
    assert_eq!(
        server.clamp_due_deadline(past, now),
        now + HEADLESS_HOUSEKEEPING_MIN_INTERVAL,
        "a past-due deadline must wait for the next housekeeping run"
    );
    let future = now + Duration::from_secs(1);
    assert_eq!(
        server.clamp_due_deadline(future, now),
        future,
        "a future deadline must pass through untouched"
    );
}
#[path = "surface_interest.rs"]
mod surface_interest_tests;

fn client_shell_snapshot(message: ServerMessage) -> Box<crate::protocol::ClientShellSnapshot> {
    let ServerMessage::EndpointControl { kind, data } = message else {
        panic!("expected client shell snapshot");
    };
    assert_eq!(kind, crate::protocol::endpoint::ENDPOINT_SNAPSHOT_KIND);
    Box::new(serde_json::from_str(&data).expect("decode client shell snapshot"))
}

fn test_headless_server() -> HeadlessServer {
    test_headless_server_with_event_hub(api::EventHub::default())
}

fn test_headless_server_with_event_hub(event_hub: api::EventHub) -> HeadlessServer {
    let config = crate::config::Config::default();
    let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
    // `mut`: the fork wires the video wake into `app.pane_graphics` below.
    let mut app = crate::app::App::new(
        &config,
        crate::app::AppPolicy::TEST,
        None,
        api_rx,
        event_hub,
    );

    app.state.default_shell = crate::app::exiting_test_command().into();
    let dir = std::env::temp_dir().join(format!(
        "hh-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = fs::create_dir_all(&dir);
    let socket_path = dir.join("client.sock");
    let _ = fs::remove_file(&socket_path);
    let listener = bind_local_listener(&socket_path).expect("bind test listener");
    let client_socket_identity =
        socket_file_identity(&socket_path).expect("test listener socket identity");
    #[cfg(unix)]
    listener
        .set_nonblocking(ListenerNonblockingMode::Accept)
        .expect("set listener nonblocking");
    let (server_event_tx, server_event_rx) = mpsc::channel(64);
    let should_quit = Arc::new(AtomicBool::new(false));
    #[cfg(windows)]
    spawn_windows_client_accept_thread(listener, should_quit.clone(), server_event_tx.clone());
    let server_keybindings = app_keybindings(&app);
    let pane_video_wake = Arc::new(tokio::sync::Notify::new());
    app.pane_graphics
        .set_video_wake(Arc::clone(&pane_video_wake));
    let headless_size = app.state.headless_size;

    HeadlessServer {
        app,
        #[cfg(unix)]
        api_tx: None,
        api_server: None,
        #[cfg(unix)]
        client_listener: listener,
        client_socket_path: socket_path,
        client_socket_identity,
        clients: HashMap::new(),
        #[cfg(unix)]
        next_client_id: 1,
        foreground_client_id: None,
        last_housekeeping_pass: None,
        pane_audio_capture: None,
        pane_audio_wake: Arc::new(tokio::sync::Notify::new()),
        pane_video_wake: Arc::clone(&pane_video_wake),
        pane_geometry_generation: 0,
        last_pane_geometry: HashMap::new(),
        pane_video: pane_video_capture::VideoSupervisor::new()
            .wake_on(Arc::clone(&pane_video_wake)),
        pane_video_streams: HashMap::new(),
        // Stream ids start at one because zero is the protocol's "no
        // stream" and a consumer that saw it would route to nothing.
        next_video_stream_id: 1,
        pane_video_drops_reported: pane_video::VideoDropReport::default(),
        pane_video_lane_dropped: 0,
        view_owner: None,
        #[cfg(test)]
        view_recomputes_for_input: 0,
        #[cfg(test)]
        watcherless_virtual_frames: 0,
        tab_geometry_controllers: HashMap::new(),
        popup_owner_tab_id: None,
        client_shell_boot_id: "test-boot".into(),
        sent_window_title: None,
        api_window_title: None,
        server_keybindings,
        server_config_diagnostic: None,
        server_config_diagnostic_without_keybindings: None,
        terminal_attach_owners: HashMap::new(),
        pending_alt_screen_reads: Vec::new(),
        deferred_alt_screen_reads: Vec::new(),
        next_activity_stamp: 1,
        headless_size,
        effective_size: headless_size,
        shutting_down: false,
        handoff_in_progress: false,
        scheduled_text_refresh_pending: false,
        full_render_reseats_placements: true,
        #[cfg(unix)]
        pending_handoff_repaint_nudge: false,
        should_quit,
        server_event_rx,
        server_event_tx,
    }
}

fn shutdown_test_runtimes(server: &mut HeadlessServer) {
    for (_, runtime) in server.app.terminal_runtimes.drain() {
        runtime.shutdown();
    }
}

fn read_server_message(bytes: Vec<u8>) -> ServerMessage {
    let mut cursor = std::io::Cursor::new(bytes);
    protocol::read_message(&mut cursor, MAX_FRAME_SIZE).expect("decode server message")
}

// Fork behaviour: every client connection starts with a delete-all
// barrier, because the outer terminal may still be showing images from a
// previous life this server never tracked (a lost delete frame, an
// earlier server, another program's leftovers). The barrier is the only
// delete that reaches ids nobody remembers. TP-GFX-RESET-01
#[tokio::test]
async fn a_fresh_client_first_frame_opens_with_a_delete_all_barrier() {
    let (mut server, client_rx, _pane_id) = retained_test_server(b"aaaa");
    server.app.state.kitty_graphics_enabled = true;
    server.clients.get_mut(&1).unwrap().cell_size = crate::kitty_graphics::HostCellSize {
        width_px: 10,
        height_px: 20,
    };
    assert!(
        server
            .clients
            .get(&1)
            .unwrap()
            .graphics_surface_reset_pending,
        "a new connection must be born with the barrier armed"
    );

    server.render_and_stream();
    let frame = read_server_frame(
        client_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("first frame"),
    );
    assert!(
        frame
            .graphics
            .starts_with(crate::kitty_graphics::KITTY_DELETE_ALL),
        "first graphics bytes must open with the delete-all barrier, got {:?}",
        String::from_utf8_lossy(&frame.graphics)
    );
    assert!(
        !server
            .clients
            .get(&1)
            .unwrap()
            .graphics_surface_reset_pending,
        "a delivered barrier must disarm the flag"
    );
}

// The drop-sites re-arm the same flag when a graphics payload is thrown
// away; the next frame must rebuild the outer terminal from the barrier.
// TP-GFX-RESET-01
#[tokio::test]
async fn a_rearmed_barrier_fires_again_on_the_next_frame() {
    let (mut server, client_rx, pane_id) = retained_test_server(b"aaaa");
    server.app.state.kitty_graphics_enabled = true;
    server.clients.get_mut(&1).unwrap().cell_size = crate::kitty_graphics::HostCellSize {
        width_px: 10,
        height_px: 20,
    };
    server.render_and_stream();
    let _ = client_rx
        .recv_timeout(Duration::from_millis(100))
        .expect("first frame");

    server
        .clients
        .get_mut(&1)
        .unwrap()
        .graphics_surface_reset_pending = true;
    let runtime = server
        .app
        .state
        .runtime_for_pane_in_workspace(&server.app.terminal_runtimes, 0, pane_id)
        .expect("runtime");
    runtime.test_process_pty_bytes(b"\rZ");
    server.render_and_stream();
    let frame = read_server_frame(
        client_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("second frame"),
    );
    assert!(
        frame
            .graphics
            .starts_with(crate::kitty_graphics::KITTY_DELETE_ALL),
        "a re-armed barrier must open the next frame"
    );
}

fn read_server_frame(bytes: Vec<u8>) -> FrameData {
    match protocol::read_message(&mut std::io::Cursor::new(bytes), MAX_GRAPHICS_FRAME_SIZE)
        .expect("decode server frame")
    {
        ServerMessage::Frame(frame) => frame,
        other => panic!("expected frame, got {other:?}"),
    }
}

fn frame_text(frame: &FrameData) -> String {
    frame
        .cells
        .chunks(usize::from(frame.width))
        .map(|row| {
            row.iter()
                .map(|cell| cell.symbol.as_str())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn read_server_shutdown_reason(bytes: Vec<u8>) -> Option<String> {
    match read_server_message(bytes) {
        ServerMessage::ServerShutdown { reason } => reason,
        other => panic!("expected shutdown, got {other:?}"),
    }
}

#[test]
fn completed_handoff_disables_only_old_server_session_persistence() {
    let mut server = test_headless_server();
    server.app.policy = crate::app::AppPolicy::PRODUCTION;

    server.finish_live_handoff_shutdown();

    assert!(!server.app.policy.persist_session);
    assert!(server.app.policy.restore_session);
    assert!(server.app.policy.persist_plugin_registry);
    assert!(server.app.policy.background_updates);
}

#[test]
fn default_headless_size_is_effective_without_clients() {
    let server = test_headless_server();

    assert_eq!(
        server.headless_size,
        (
            crate::config::DEFAULT_HEADLESS_COLS,
            crate::config::DEFAULT_HEADLESS_ROWS
        )
    );
    assert_eq!(server.effective_size, server.headless_size);
}

#[tokio::test]
async fn headless_api_reads_latest_title_without_spinner_event_flooding() {
    let event_hub = api::EventHub::default();
    let mut server = test_headless_server_with_event_hub(event_hub.clone());
    server.app.state.workspaces = vec![crate::workspace::Workspace::test_new("one")];
    server.app.state.ensure_test_terminals();
    server.app.state.active = Some(0);
    server.app.state.selected = 0;
    server.app.state.mode = crate::app::Mode::Terminal;
    server.app.state.sidebar_agents.rows = vec![vec![
        crate::config::AgentSidebarToken::TerminalTitleStripped,
    ]];
    let pane_id = server.app.state.workspaces[0].tabs[0].root_pane;
    let terminal_id = server.app.state.workspaces[0].tabs[0].panes[&pane_id]
        .attached_terminal_id
        .clone();
    server
        .app
        .state
        .terminals
        .get_mut(&terminal_id)
        .unwrap()
        .detected_agent = Some(crate::detect::Agent::Claude);
    let runtime = crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 24, b"");
    runtime.test_process_pty_bytes(b"\x1b]0;\xe2\xa0\x8b task\x07");
    server
        .app
        .terminal_runtimes
        .insert(terminal_id.clone(), runtime);
    server.app.render_dirty.request_terminal_title(pane_id);

    let first = headless_pane_list(&mut server).pop().unwrap();
    assert_eq!(first.terminal_title.as_deref(), Some("⠋ task"));
    assert_eq!(first.terminal_title_stripped.as_deref(), Some("task"));
    assert_eq!(pane_updated_events(&event_hub), 1);
    let (buffer, _) = crate::server::render_stream::render_virtual_with_runtime_registry(
        &mut server.app.state,
        &server.app.terminal_runtimes,
        Rect::new(0, 0, 100, 30),
        true,
        crate::kitty_graphics::HostCellSize::default(),
    );
    let rendered = buffer
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(rendered.contains("task"), "rendered frame: {rendered:?}");

    server
        .app
        .terminal_runtimes
        .get(&terminal_id)
        .unwrap()
        .test_process_pty_bytes(b"\x1b]2;\xe2\xa0\x99 task\x1b\\");
    server.app.render_dirty.request_terminal_title(pane_id);
    let second = headless_pane_list(&mut server).pop().unwrap();
    assert_eq!(second.terminal_title.as_deref(), Some("⠙ task"));
    assert_eq!(second.terminal_title_stripped.as_deref(), Some("task"));
    assert_eq!(pane_updated_events(&event_hub), 1);
}

fn headless_pane_list(server: &mut HeadlessServer) -> Vec<api::schema::PaneInfo> {
    let (respond_to, response_rx) = std::sync::mpsc::channel();
    server.handle_api_request_with_shutdown_check(api::ApiRequestMessage {
        request: api::schema::Request {
            id: "list-titles".into(),
            method: api::schema::Method::PaneList(api::schema::PaneListParams::default()),
        },
        respond_to,
        response_write_complete: None,
        stream_active: None,
    });
    let response: api::schema::SuccessResponse =
        serde_json::from_str(&response_rx.recv().unwrap()).unwrap();
    let api::schema::ResponseResult::PaneList { panes } = response.result else {
        panic!("expected pane list");
    };
    panes
}

fn pane_updated_events(event_hub: &api::EventHub) -> usize {
    event_hub
        .events_after(0)
        .iter()
        .filter(|(_, event)| event.event == api::schema::EventKind::PaneUpdated)
        .count()
}

#[test]
fn server_stop_interrupts_server_event_backlog() {
    let mut server = test_headless_server();
    for client_id in 1..=64 {
        server
            .server_event_tx
            .try_send(ServerEvent::ClientDisconnected { client_id })
            .unwrap();
    }

    server.should_quit.store(true, Ordering::Release);

    assert!(!server.drain_server_events());
    assert!(server.server_event_rx.try_recv().is_ok());
    shutdown_test_runtimes(&mut server);
}

#[test]
fn headless_api_request_drains_all_pending_internal_events_before_reading_state() {
    let mut server = test_headless_server();
    for i in 0..=crate::app::APP_EVENT_DRAIN_LIMIT {
        server
            .app
            .event_tx
            .try_send(AppEvent::UpdateReady {
                version: format!("4.0.{i}"),
                install_command: "herdr install".into(),
            })
            .unwrap();
    }

    let (respond_to, response_rx) = std::sync::mpsc::channel();
    assert!(
        server.handle_api_request_with_shutdown_check(api::ApiRequestMessage {
            request: api::schema::Request {
                id: "headless_stop_after_events".into(),
                method: api::schema::Method::ServerStop(api::schema::ServerStopParams::default()),
            },
            respond_to,
            response_write_complete: None,
            stream_active: None,
        })
    );
    let response = response_rx
        .recv_timeout(Duration::from_millis(100))
        .unwrap();
    let response: serde_json::Value = serde_json::from_str(&response).unwrap();

    assert_eq!(response["result"]["type"], "ok");
    let expected_version = format!("4.0.{}", crate::app::APP_EVENT_DRAIN_LIMIT);
    assert_eq!(
        server.app.state.update_available.as_deref(),
        Some(expected_version.as_str())
    );
    assert!(server.app.event_rx.try_recv().is_err());
}

#[tokio::test]
async fn headless_deferred_workspace_create_uses_runtime_events() {
    let event_hub = api::EventHub::default();
    let mut server = test_headless_server_with_event_hub(event_hub.clone());

    server.app.state.request_new_workspace = true;

    assert!(server.handle_deferred_requests_headless());
    assert!(!server.app.state.request_new_workspace);
    assert_eq!(
        event_hub
            .events_after(0)
            .into_iter()
            .map(|(_, event)| event.event)
            .collect::<Vec<_>>(),
        vec![
            api::schema::EventKind::WorkspaceCreated,
            api::schema::EventKind::TabCreated,
            api::schema::EventKind::PaneCreated,
            api::schema::EventKind::LayoutUpdated,
        ]
    );
    shutdown_test_runtimes(&mut server);
}

// TP-DAILY-30: the daily menu's "Merge workspaces here" parks its request
// and the server — the loop a live herdr runs — does the merge. Only the
// in-process loop drained it until U2b removed that loop; in the server
// the menu closed and nothing happened.
#[tokio::test]
async fn the_server_merges_the_daily_workspaces_its_menu_asked_for() {
    let mut server = test_headless_server();
    let daily = std::path::PathBuf::from("/herdr-test-daily-merge");
    server.app.state.daily_chat_cwd = Some(daily.clone());
    server.app.state.workspaces = (0..3)
        .map(|_| {
            let mut ws = crate::workspace::Workspace::test_new("user");
            ws.custom_name = None;
            ws.identity_cwd = daily.clone();
            ws
        })
        .collect();
    server.app.state.ensure_test_terminals();
    server.app.state.active = Some(0);
    server.app.state.selected = 0;
    assert_eq!(
        server.app.state.mergeable_daily_workspaces().len(),
        3,
        "precondition: three unnamed workspaces stand in the daily directory"
    );

    server.app.state.request_merge_daily_workspaces = true;
    assert!(server.handle_deferred_requests_headless());

    assert!(
        !server.app.state.request_merge_daily_workspaces,
        "the server drains the request"
    );
    assert_eq!(
        server.app.state.mergeable_daily_workspaces().len(),
        1,
        "the server merged the daily workspaces into one"
    );
    shutdown_test_runtimes(&mut server);
}

// TP-AGPANEL-75: the graveyard menu's Revive parks its request and the
// server revives. The left-click road always called the API directly; the
// menu's road reached only the in-process loop until U2b removed it, so in
// the server Revive from the menu did nothing.
#[tokio::test]
async fn the_server_revives_the_ghost_its_menu_asked_for() {
    let mut server = test_headless_server();
    server.app.state.workspaces = vec![crate::workspace::Workspace::test_new("revive")];
    server.app.state.ensure_test_terminals();
    server.app.state.active = Some(0);
    // The ghost's conversation already runs in a wired tab, so the revival
    // lands by focusing it rather than spawning a process (TP-AGPANEL-20).
    server.app.state.workspaces[0].test_add_tab(None);
    server.app.state.ensure_test_terminals();
    server.app.state.workspaces[0].tabs[1].resumed_session_id = Some("session-open".into());
    let running = server.app.state.workspaces[0].tabs[1].panes
        [&server.app.state.workspaces[0].tabs[1].root_pane]
        .attached_terminal_id
        .clone();
    server
        .app
        .state
        .terminals
        .get_mut(&running)
        .expect("test terminal")
        .set_agent_name("session-open".to_string());
    server
        .app
        .state
        .closed_agents
        .record_closed(crate::app::closed_agents::ClosedAgentRecord {
            agent_id: "open".into(),
            label: "agent open".into(),
            cwd: Some(std::path::PathBuf::from("/tmp")),
            workspace_key: None,
            session: crate::agent_resume::AgentSessionRef::id("session-open").map(|session_ref| {
                crate::agent_resume::PersistedAgentSession {
                    source: "herdr:claude".into(),
                    agent: "claude".into(),
                    session_ref,
                }
            }),
            closed_at: 1,
            revival: crate::app::closed_agents::RevivalState::Dormant,
            label_explicit: false,
            never_spoke: false,
        });

    server.app.state.request_revive_closed_agent = Some("open".into());
    assert!(server.handle_deferred_requests_headless());

    assert!(
        server.app.state.request_revive_closed_agent.is_none(),
        "the server drains the request"
    );
    assert!(
        server.app.state.closed_agents.entries().next().is_none(),
        "a revival that landed leaves the graveyard"
    );
    assert_eq!(
        server.app.state.workspaces[0].active_tab_index(),
        1,
        "the wired tab took focus"
    );
    shutdown_test_runtimes(&mut server);
}

// TP-SRV-DEFER-01: a key, menu or click that cannot act on the spot parks a
// one-shot `request_*` on AppState, and something the server runs must
// consume it. Two were drained only by the in-process loop — the daily
// merge and the graveyard menu's revive (TP-DAILY-30, TP-AGPANEL-75) — and
// did nothing in a live herdr until U2b removed that loop and dead code
// gave them away. Every request field is named here: the deferred ones
// must appear in the server's deferred drain; the rest name their consumer.
#[test]
fn every_parked_request_has_a_consumer_the_server_runs() {
    const STATE_SOURCE: &str = include_str!("../../../app/state.rs");
    const HEADLESS_SOURCE: &str = include_str!("../../headless.rs");
    const DRAINED_BY_THE_SERVER: [&str; 19] = [
        "request_chat_label",
        "request_chat_move",
        "request_chat_rename",
        "request_complete_onboarding",
        "request_merge_daily_workspaces",
        "request_module_branch_workspace",
        "request_module_git_init",
        "request_new_linked_worktree",
        "request_new_tab",
        "request_new_workspace",
        "request_new_workspace_cwd",
        "request_open_existing_worktree",
        "request_open_web_pane",
        "request_reload_config",
        "request_remove_linked_worktree",
        "request_revive_closed_agent",
        "request_submit_worktree_create",
        "request_submit_worktree_open",
        "request_submit_worktree_remove",
    ];
    const CONSUMED_ELSEWHERE: [(&str, &str); 13] = [
        (
            "request_agent_attachment_delivery",
            "App::sync_agent_attachment_delivery",
        ),
        (
            "request_client_config_reload",
            "HeadlessServer::drain_client_config_reload_request",
        ),
        (
            "request_clipboard_write",
            "App::dispatch_pending_clipboard_write",
        ),
        (
            "request_file_manager_agent_handoff",
            "App::sync_file_manager_agent_handoff_send",
        ),
        (
            "request_file_manager_bulk_rename",
            "App::consume_file_manager_bulk_rename_request",
        ),
        (
            "request_file_manager_context_action",
            "the per-display pass (App::sync_per_display_surfaces)",
        ),
        (
            "request_file_manager_create_directory",
            "App::consume_file_manager_create_directory_request",
        ),
        (
            "request_file_manager_delete",
            "App::consume_file_manager_delete_request",
        ),
        (
            "request_file_manager_location_navigation",
            "App::sync_file_manager_location_request",
        ),
        (
            "request_file_manager_rename",
            "App::consume_file_manager_rename_request",
        ),
        (
            "request_file_manager_reveal",
            "App::apply_pending_file_manager_reveal",
        ),
        (
            "request_preview_show",
            "App::handle_preview_show_request_with",
        ),
        (
            "request_project_chat_tab",
            "App::handle_project_chat_tab_request",
        ),
    ];

    let fields: std::collections::BTreeSet<String> = STATE_SOURCE
        .lines()
        .filter_map(|line| {
            let line = line.trim_start();
            let rest = line
                .strip_prefix("pub request_")
                .or_else(|| line.strip_prefix("pub(crate) request_"))?;
            let name = rest.split(':').next()?.trim();
            Some(format!("request_{name}"))
        })
        .collect();
    let named: std::collections::BTreeSet<String> = DRAINED_BY_THE_SERVER
        .iter()
        .map(|flag| (*flag).to_string())
        .chain(
            CONSUMED_ELSEWHERE
                .iter()
                .map(|(flag, _)| (*flag).to_string()),
        )
        .collect();
    assert_eq!(
        fields, named,
        "every request field is named once: drained by the server or consumed elsewhere"
    );

    let at = HEADLESS_SOURCE
        .find("fn handle_deferred_requests_in_viewer(&mut self)")
        .expect("the server's deferred drain");
    let body = &HEADLESS_SOURCE[at..];
    let body = &body[..body.find("\n    }\n").expect("the drain closes")];
    let drains = |flag: &str| {
        let needle = format!("self.app.state.{flag}");
        body.match_indices(&needle).any(|(at, _)| {
            !body[at + needle.len()..].starts_with(|c: char| c.is_ascii_alphanumeric() || c == '_')
        })
    };
    let missing: Vec<&str> = DRAINED_BY_THE_SERVER
        .iter()
        .copied()
        .filter(|flag| !drains(flag))
        .collect();
    assert!(
        missing.is_empty(),
        "requests the server's deferred drain never consumes: {missing:?}"
    );
}

// TP-TAB-BROWSER-02: the key road parks its ask on the state; the viewer
// drains it onto `pane.web.open`, and a pane is born beside the focused
// one running the configured browser.
#[tokio::test]
async fn headless_deferred_web_pane_request_births_a_browser_pane() {
    let event_hub = api::EventHub::default();
    let mut server = test_headless_server_with_event_hub(event_hub.clone());
    server.app.state.workspaces = vec![crate::workspace::Workspace::test_new("web")];
    server.app.state.ensure_test_terminals();
    server.app.state.active = Some(0);
    server.app.state.selected = 0;
    server.app.state.web_browser_command = vec!["/usr/bin/true".into()];
    let panes_before = server.app.state.workspaces[0].tabs[0]
        .layout
        .pane_ids()
        .len();

    server.app.state.request_open_web_pane = true;

    assert!(server.handle_deferred_requests_headless());
    assert!(!server.app.state.request_open_web_pane);
    assert_eq!(
        server.app.state.workspaces[0].tabs[0]
            .layout
            .pane_ids()
            .len(),
        panes_before + 1,
        "one browser pane was born"
    );
    assert!(
        event_hub
            .events_after(0)
            .into_iter()
            .map(|(_, event)| event.event)
            .any(|kind| kind == api::schema::EventKind::PaneCreated),
        "the birth was announced like any other pane's"
    );
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn headless_deferred_named_tab_create_uses_runtime_events() {
    let event_hub = api::EventHub::default();
    let mut server = test_headless_server_with_event_hub(event_hub.clone());
    keep_test_panes_alive(&mut server);
    server
        .app
        .create_workspace_with_options(std::env::temp_dir(), true)
        .unwrap();
    let after_setup = event_hub.current_sequence();

    server.app.state.request_new_tab = true;
    server.app.state.requested_new_tab_name = Some("ops".into());

    assert!(server.handle_deferred_requests_headless());
    assert!(!server.app.state.request_new_tab);
    assert_eq!(server.app.state.requested_new_tab_name, None);
    let events = event_hub.events_after(after_setup);
    assert_eq!(
        events
            .iter()
            .map(|(_, event)| event.event)
            .collect::<Vec<_>>(),
        vec![
            api::schema::EventKind::TabCreated,
            api::schema::EventKind::PaneCreated,
            api::schema::EventKind::LayoutUpdated,
        ]
    );
    let tab_created = events
        .iter()
        .find_map(|(_, event)| match &event.data {
            api::schema::EventData::TabCreated { tab } => Some(tab),
            _ => None,
        })
        .expect("tab created event");
    assert_eq!(tab_created.label, "ops");
    shutdown_test_runtimes(&mut server);
}

// TP-DEFER-PERSON-01: the requests a person raises from a chat row —
// rename, move, label — are consumed by the headless server's deferred
// pass, not only by the monolithic loop. Measured live 2026-08-20: all
// three were set by the input road and never taken on the server every
// installed herdr actually runs, so renaming a chat did nothing at all.
#[tokio::test]
async fn a_deferred_chat_rename_reaches_the_ledger_on_the_headless_server() {
    let event_hub = api::EventHub::default();
    let mut server = test_headless_server_with_event_hub(event_hub.clone());

    server.app.state.request_chat_rename = Some(("sess-1".into(), "yeni ad".into()));
    assert!(server.handle_deferred_requests_headless());

    assert!(server.app.state.request_chat_rename.is_none());
    assert_eq!(
        server
            .app
            .workspace_chat_ledger
            .names
            .get("sess-1")
            .map(String::as_str),
        Some("yeni ad")
    );
    shutdown_test_runtimes(&mut server);
}

// TP-DEFER-PERSON-01: the move half of the same gap — the reported
// shape was "the chat does not appear under the module I moved it to",
// because the move was never applied at all.
#[tokio::test]
async fn a_deferred_chat_move_reaches_the_ledger_on_the_headless_server() {
    let event_hub = api::EventHub::default();
    let mut server = test_headless_server_with_event_hub(event_hub.clone());

    server.app.state.request_chat_move = Some(("sess-2".into(), Some("modul-hedef".into())));
    assert!(server.handle_deferred_requests_headless());

    assert!(server.app.state.request_chat_move.is_none());
    assert_eq!(
        server
            .app
            .workspace_chat_ledger
            .moves
            .get("sess-2")
            .map(String::as_str),
        Some("modul-hedef")
    );
    shutdown_test_runtimes(&mut server);
}

// TP-DEFER-PERSON-01: and the label half — hiding a chore from the
// drawer went the same dead road.
#[tokio::test]
async fn a_deferred_chat_label_reaches_the_ledger_on_the_headless_server() {
    let event_hub = api::EventHub::default();
    let mut server = test_headless_server_with_event_hub(event_hub.clone());

    server.app.state.request_chat_label = Some((
        "sess-3".into(),
        Some(crate::chat_labels::ChatLabel::Routine),
    ));
    assert!(server.handle_deferred_requests_headless());

    assert!(server.app.state.request_chat_label.is_none());
    assert!(server
        .app
        .workspace_chat_ledger
        .labels
        .contains_key("sess-3"));
    shutdown_test_runtimes(&mut server);
}

// TP-CHAT-NAME-02: the open tab wearing the conversation follows the
// rename — the row and the tab are two views of one name, and the
// reported half of the defect was exactly "the tab's reference label
// does not rename either".
#[tokio::test]
async fn a_rename_reaches_the_open_tab_wearing_that_conversation() {
    let event_hub = api::EventHub::default();
    let mut server = test_headless_server_with_event_hub(event_hub.clone());
    keep_test_panes_alive(&mut server);
    server
        .app
        .create_workspace_with_options(std::env::temp_dir(), true)
        .unwrap();
    let ws_idx = server.app.state.active.expect("workspace");
    server.app.state.workspaces[ws_idx].tabs[0].resumed_session_id = Some("sess-t".into());

    server.app.state.request_chat_rename = Some(("sess-t".into(), "taze ad".into()));
    assert!(server.handle_deferred_requests_headless());

    // TP-TAB-CHAT-01: the tab wears the renamed conversation's name, and
    // it does so through the chat title — `custom_name` stays clear so an
    // explicit tab rename would still outrank it.
    assert_eq!(
        server.app.state.workspaces[ws_idx]
            .tab_display_name(0)
            .as_deref(),
        Some("taze ad")
    );
    assert!(
        server.app.state.workspaces[ws_idx].tabs[0]
            .custom_name
            .is_none(),
        "a chat rename follows via the chat title, not by stamping custom_name",
    );
    shutdown_test_runtimes(&mut server);
}

// TP-DEFER-PERSON-01: the module verbs ride the same pass — consumed,
// not parked forever.
#[tokio::test]
async fn deferred_module_requests_are_consumed_on_the_headless_server() {
    let event_hub = api::EventHub::default();
    let mut server = test_headless_server_with_event_hub(event_hub.clone());

    server.app.state.request_module_git_init = Some("mod-a".into());
    server.app.state.request_module_branch_workspace = Some("mod-b".into());
    assert!(server.handle_deferred_requests_headless());

    assert!(server.app.state.request_module_git_init.is_none());
    assert!(server.app.state.request_module_branch_workspace.is_none());
    shutdown_test_runtimes(&mut server);
}

/// Upstream's fixture launches panes with a command that exits at once, so
/// tests leak no shells — and a pane that exits closes its workspace. A test
/// whose workspace must outlive a deferred pass gives its panes a shell that
/// waits instead; `shutdown_test_runtimes` still reaps them.
fn keep_test_panes_alive(server: &mut HeadlessServer) {
    server.app.state.default_shell = if cfg!(windows) { "cmd.exe" } else { "/bin/sh" }.into();
}

/// One display's tab focus, read through its own surface window.
fn display_tab_focus(server: &mut HeadlessServer, client: u64) -> (usize, usize) {
    let previous = server.app.state.enter_viewer(Some(client));
    let ws_idx = server.app.state.active.unwrap_or(usize::MAX);
    let tab_idx = server
        .app
        .state
        .workspaces
        .get(ws_idx)
        .map(|ws| ws.active_tab_index())
        .unwrap_or(usize::MAX);
    server.app.state.restore_viewer(previous);
    (ws_idx, tab_idx)
}

// TP-MCF-CTX-07: a queued request is a display's own click, deferred —
// consuming it outside that display's window writes the focus move into
// whichever registers happen to be installed, and the display that asked
// watches nothing happen. The same rule TP-SUR-BROADCAST-05 pinned for
// plugin-raised popups, extended to the deferred-request pass.
#[tokio::test]
async fn a_deferred_new_tab_lands_in_the_display_that_asked_for_it() {
    let event_hub = api::EventHub::default();
    let mut server = test_headless_server_with_event_hub(event_hub.clone());
    keep_test_panes_alive(&mut server);
    server
        .app
        .create_workspace_with_options(std::env::temp_dir(), true)
        .unwrap();

    // Two displays known to the session, each standing on tab 0 by its
    // own hand — a per-display focus entry, not the shared default.
    for client in [1u64, 2u64] {
        let previous = server.app.state.enter_viewer(Some(client));
        let ws_idx = server.app.state.active.expect("a workspace is active");
        server.app.state.workspaces[ws_idx].set_active_tab(0);
        server.app.state.restore_viewer(previous);
    }
    let before_1 = display_tab_focus(&mut server, 1);
    let before_2 = display_tab_focus(&mut server, 2);
    assert_eq!(before_1, before_2, "precondition: both start together");

    // Display 1 holds the terminal focus; its + press queued the request.
    server.foreground_client_id = Some(1);
    server.app.state.request_new_tab = true;
    assert!(server.handle_deferred_requests_headless());

    let after_1 = display_tab_focus(&mut server, 1);
    let after_2 = display_tab_focus(&mut server, 2);
    assert_eq!(
        after_1,
        (before_1.0, before_1.1 + 1),
        "the display that asked stands on the new tab"
    );
    assert_eq!(after_2, before_2, "the display that did not ask stays put");
    shutdown_test_runtimes(&mut server);
}

fn window_title_test_server() -> (HeadlessServer, std::sync::mpsc::Receiver<Vec<u8>>) {
    let mut server = test_headless_server();
    server.app.state.workspaces = vec![crate::workspace::Workspace::test_new("herd")];
    server.app.state.active = Some(0);
    server.app.state.selected = 0;

    let (client_tx, control_rx, _render_rx) = test_client_writer();
    server.clients.insert(
        1,
        ClientConnection::app(
            (80, 24),
            crate::kitty_graphics::HostCellSize::default(),
            crate::terminal_theme::TerminalTheme::default(),
            None,
            1,
            RenderEncoding::SemanticFrame,
            Some(client_tx),
        ),
    );
    server.promote_client_to_foreground(1);
    drain_window_titles(&control_rx);
    (server, control_rx)
}

/// The test client writer drains its queue on a background thread, so
/// reading a pushed message needs a timeout rather than `try_recv`.
fn next_window_title(control_rx: &std::sync::mpsc::Receiver<Vec<u8>>) -> Option<Option<String>> {
    let deadline = Instant::now() + Duration::from_secs(5);
    while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
        let Ok(bytes) = control_rx.recv_timeout(remaining) else {
            return None;
        };
        if let ServerMessage::WindowTitle { title } = read_server_message(bytes) {
            return Some(title);
        }
    }
    None
}

fn drain_window_titles(control_rx: &std::sync::mpsc::Receiver<Vec<u8>>) {
    while control_rx.recv_timeout(Duration::from_millis(50)).is_ok() {}
}

fn no_window_title(control_rx: &std::sync::mpsc::Receiver<Vec<u8>>) -> bool {
    while let Ok(bytes) = control_rx.recv_timeout(Duration::from_millis(200)) {
        if let ServerMessage::WindowTitle { .. } = read_server_message(bytes) {
            return false;
        }
    }
    true
}

#[test]
fn window_title_waits_for_a_foreground_client_to_exist() {
    let mut server = test_headless_server();
    server.app.state.workspaces = vec![crate::workspace::Workspace::test_new("herd")];
    server.app.state.active = Some(0);
    server.app.configure_window_title("{workspace}");

    // The server renders before the first client attaches. Nothing was
    // delivered, so nothing may be recorded as delivered either.
    server.sync_window_title();
    assert_eq!(server.sent_window_title, None);

    let (client_tx, control_rx, _render_rx) = test_client_writer();
    server.clients.insert(
        1,
        ClientConnection::app(
            (80, 24),
            crate::kitty_graphics::HostCellSize::default(),
            crate::terminal_theme::TerminalTheme::default(),
            None,
            1,
            RenderEncoding::SemanticFrame,
            Some(client_tx),
        ),
    );
    server.promote_client_to_foreground(1);
    server.sync_window_title();

    assert_eq!(
        next_window_title(&control_rx),
        Some(Some("herd".to_string()))
    );
    shutdown_test_runtimes(&mut server);
}

#[test]
fn an_attaching_client_gets_the_title_even_when_it_has_not_changed() {
    let (mut server, first_control_rx) = window_title_test_server();
    server.app.configure_window_title("{workspace}");
    server.sync_window_title();
    assert_eq!(
        next_window_title(&first_control_rx),
        Some(Some("herd".to_string()))
    );

    // ClientConnected assigns the foreground client directly rather than
    // going through promote_client_to_foreground, so the cache must notice
    // the new client on its own.
    let (client_tx, second_control_rx, _render_rx) = test_client_writer();
    server.clients.insert(
        2,
        ClientConnection::app(
            (80, 24),
            crate::kitty_graphics::HostCellSize::default(),
            crate::terminal_theme::TerminalTheme::default(),
            None,
            2,
            RenderEncoding::SemanticFrame,
            Some(client_tx),
        ),
    );
    server.foreground_client_id = Some(2);
    server.sync_window_title();

    assert_eq!(
        next_window_title(&second_control_rx),
        Some(Some("herd".to_string()))
    );
    shutdown_test_runtimes(&mut server);
}

#[test]
fn configured_window_title_reaches_the_foreground_client_once_per_change() {
    let (mut server, control_rx) = window_title_test_server();
    server.app.configure_window_title("{workspace}/{tab}");

    server.sync_window_title();
    assert_eq!(
        next_window_title(&control_rx),
        Some(Some("herd/1".to_string()))
    );

    // An unchanged title must not re-emit an OSC on every render.
    server.sync_window_title();
    assert!(no_window_title(&control_rx));

    server.app.state.workspaces[0].tabs[0].custom_name = Some("build".into());
    server.sync_window_title();
    assert_eq!(
        next_window_title(&control_rx),
        Some(Some("herd/build".to_string()))
    );

    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn focused_terminal_title_syncs_without_requesting_a_sidebar_render() {
    let (mut server, control_rx) = window_title_test_server();
    server.app.configure_window_title("{terminal_title}");
    server.app.state.ensure_test_terminals();
    let pane_id = server.app.state.workspaces[0].tabs[0].root_pane;
    let terminal_id = server.app.state.workspaces[0]
        .terminal_id(pane_id)
        .expect("terminal")
        .clone();
    let runtime = crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 24, b"");
    runtime.test_process_pty_bytes("\x1b]0;⠋ building\x07".as_bytes());
    server
        .app
        .terminal_runtimes
        .insert(terminal_id.clone(), runtime);

    assert_eq!(
        server.sync_terminal_title_sources(&HashSet::from([pane_id])),
        (false, true)
    );
    assert_eq!(
        next_window_title(&control_rx),
        Some(Some("building".to_string()))
    );

    server
        .app
        .terminal_runtimes
        .get(&terminal_id)
        .expect("runtime")
        .test_process_pty_bytes("\x1b]0;⠙ building\x07".as_bytes());
    assert_eq!(
        server.sync_terminal_title_sources(&HashSet::from([pane_id])),
        (false, true)
    );
    assert!(no_window_title(&control_rx));

    shutdown_test_runtimes(&mut server);
}

#[test]
fn a_foreground_client_without_a_writer_does_not_cache_the_window_title() {
    let (mut server, _control_rx) = window_title_test_server();
    server.app.configure_window_title("{workspace}");

    // A detached client keeps its entry but loses its writer, so nothing
    // reaches a terminal even though the targeted send reports success.
    if let Some(client) = server.clients.get_mut(&1) {
        client.writer = None;
    }
    server.sync_window_title();
    assert!(server.sent_window_title.is_none());

    // Attaching again has to deliver the title rather than skip it as sent.
    let (client_tx, control_rx, _render_rx) = test_client_writer();
    if let Some(client) = server.clients.get_mut(&1) {
        client.writer = Some(client_tx);
    }
    server.sync_window_title();
    assert_eq!(
        next_window_title(&control_rx),
        Some(Some("herd".to_string()))
    );

    shutdown_test_runtimes(&mut server);
}

#[test]
fn empty_window_title_config_leaves_the_outer_title_alone() {
    let (mut server, control_rx) = window_title_test_server();
    server.app.configure_window_title("");

    server.sync_window_title();

    assert!(no_window_title(&control_rx));
    shutdown_test_runtimes(&mut server);
}

#[test]
fn api_window_title_wins_until_it_is_cleared() {
    let (mut server, control_rx) = window_title_test_server();
    server.app.configure_window_title("{workspace}");

    server.handle_client_window_title_api("set".into(), Some("herdr api".into()));
    assert_eq!(
        next_window_title(&control_rx),
        Some(Some("herdr api".to_string()))
    );

    server.app.state.workspaces[0].custom_name = Some("ops".into());
    server.sync_window_title();
    assert!(no_window_title(&control_rx));

    // Clearing hands the title back to ui.window_title, not to "herdr".
    server.handle_client_window_title_api("clear".into(), None);
    assert_eq!(
        next_window_title(&control_rx),
        Some(Some("ops".to_string()))
    );

    shutdown_test_runtimes(&mut server);
}

#[test]
fn clearing_the_api_title_falls_back_to_herdr_when_window_titles_are_disabled() {
    let (mut server, control_rx) = window_title_test_server();
    server.app.configure_window_title("");

    server.handle_client_window_title_api("set".into(), Some("herdr api".into()));
    assert_eq!(
        next_window_title(&control_rx),
        Some(Some("herdr api".to_string()))
    );

    server.handle_client_window_title_api("clear".into(), None);
    assert_eq!(next_window_title(&control_rx), Some(None));

    shutdown_test_runtimes(&mut server);
}

#[test]
fn a_newly_promoted_client_gets_the_window_title_again() {
    let (mut server, first_control_rx) = window_title_test_server();
    server.app.configure_window_title("{workspace}");
    server.sync_window_title();
    assert_eq!(
        next_window_title(&first_control_rx),
        Some(Some("herd".to_string()))
    );

    // A second terminal starts on whatever its shell or ssh left behind.
    let (client_tx, second_control_rx, _render_rx) = test_client_writer();
    server.clients.insert(
        2,
        ClientConnection::app(
            (80, 24),
            crate::kitty_graphics::HostCellSize::default(),
            crate::terminal_theme::TerminalTheme::default(),
            None,
            2,
            RenderEncoding::SemanticFrame,
            Some(client_tx),
        ),
    );
    server.promote_client_to_foreground(2);
    server.sync_window_title();

    assert_eq!(
        next_window_title(&second_control_rx),
        Some(Some("herd".to_string()))
    );
    shutdown_test_runtimes(&mut server);
}

fn test_client_writer() -> (
    ClientWriter,
    std::sync::mpsc::Receiver<Vec<u8>>,
    std::sync::mpsc::Receiver<Vec<u8>>,
) {
    let (control_tx, control_rx) = std::sync::mpsc::channel();
    let (render_tx, render_rx) = std::sync::mpsc::sync_channel(1);
    (
        ClientWriter::test_channel(control_tx, render_tx),
        control_rx,
        render_rx,
    )
}

#[tokio::test]
async fn client_shell_attach_seeds_workspace() {
    let mut server = test_headless_server();
    server.app.state.workspaces.clear();
    server.app.state.active = None;
    server.app.state.mode = crate::app::Mode::Navigate;
    let (writer, _control_rx, _render_rx) = test_client_writer();

    assert!(
        server.handle_server_event(ServerEvent::ClientShellConnected {
            client_id: 6,
            surface_cols: 80,
            surface_rows: 23,
            cell_width_px: 0,
            cell_height_px: 0,
            pixel_mouse: false,
            direct_graphics: false,
            endpoint_keybindings: false,
            mouse_capture: false,
            surface_active: true,
            writer,
        })
    );

    assert_eq!(server.app.state.mode, crate::app::Mode::Terminal);
    assert_eq!(server.app.state.workspaces.len(), 1);
    assert_eq!(server.app.state.active, Some(0));
    shutdown_test_runtimes(&mut server);
}

fn open_virtual_miller_files(server: &mut HeadlessServer) {
    let cwd = PathBuf::from("/virtual/miller-transport");
    let mut file_manager = crate::fm::FmState::test_empty(cwd.clone());
    file_manager.entries = ["alpha.txt", "bravo.txt"]
        .into_iter()
        .map(|name| crate::fm::FileEntry {
            name: name.to_owned(),
            path: cwd.join(name),
            kind: if false {
                crate::fm::entry_kind::FileEntryKind::Directory
            } else {
                crate::fm::entry_kind::FileEntryKind::RegularFile
            },
            modified: None,
        })
        .collect();
    file_manager.sync_trail_bridge_for_test();
    server.app.state.mobile_width_threshold = 0;
    server.app.state.sidebar_collapsed = true;
    server
        .app
        .state
        .try_open_file_manager_with(|_| Some(file_manager))
        .expect("open virtual Miller Files");
}

#[tokio::test]
async fn client_shell_attach_seeds_workspace_without_consuming_legacy_onboarding() {
    let mut server = test_headless_server();
    server.app.state.workspaces.clear();
    server.app.state.active = None;
    server.app.state.mode = crate::app::Mode::Onboarding;
    let (writer, _control_rx, _render_rx) = test_client_writer();

    assert!(
        server.handle_server_event(ServerEvent::ClientShellConnected {
            client_id: 6,
            surface_cols: 80,
            surface_rows: 23,
            cell_width_px: 0,
            cell_height_px: 0,
            pixel_mouse: false,
            direct_graphics: false,
            endpoint_keybindings: false,
            mouse_capture: false,
            surface_active: true,
            writer,
        })
    );

    assert_eq!(server.app.state.mode, crate::app::Mode::Onboarding);
    assert_eq!(server.app.state.workspaces.len(), 1);
    assert_eq!(server.app.state.active, Some(0));
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn client_shell_endpoint_request_uses_the_selected_connection() {
    let mut server = test_headless_server();
    server.app.state.workspaces = vec![crate::workspace::Workspace::test_new("endpoint")];
    server.app.state.ensure_test_terminals();
    server.app.state.active = Some(0);
    let (writer, control_rx, _render_rx) = test_client_writer();
    let client_id = 41;
    assert!(
        server.handle_server_event(ServerEvent::ClientShellConnected {
            client_id,
            surface_cols: 80,
            surface_rows: 23,
            cell_width_px: 0,
            cell_height_px: 0,
            pixel_mouse: false,
            direct_graphics: false,
            endpoint_keybindings: false,
            mouse_capture: false,
            surface_active: true,
            writer,
        })
    );
    let _initial_snapshot = control_rx.recv().expect("initial shell snapshot");
    let boot_id = server.client_shell_boot_id.clone();

    assert!(
        !server.handle_server_event(ServerEvent::ClientShellEndpointRequest {
            client_id,
            boot_id: boot_id.clone(),
            request: Box::new(api::schema::Request {
                id: "client-shell:1".into(),
                method: api::schema::Method::IntegrationList(api::schema::EmptyParams::default(),),
            }),
        })
    );
    assert!(server.clients[&client_id].shell_endpoint_command_in_flight);

    assert!(
        !server.handle_server_event(ServerEvent::ClientShellEndpointRequest {
            client_id,
            boot_id: boot_id.clone(),
            request: Box::new(api::schema::Request {
                id: "client-shell:busy".into(),
                method: api::schema::Method::IntegrationList(api::schema::EmptyParams::default()),
            }),
        })
    );
    assert!(server.clients.contains_key(&client_id));
    let ServerMessage::ClientShellEndpointResponseChunk { data, .. } =
        read_server_message(control_rx.recv().expect("busy endpoint response"))
    else {
        panic!("expected busy endpoint response");
    };
    let response =
        serde_json::from_slice::<api::schema::ErrorResponse>(&data).expect("typed busy response");
    assert_eq!(response.error.code, "endpoint_busy");

    let response_ready = server
        .server_event_rx
        .recv()
        .await
        .expect("endpoint response ready");
    assert!(!server.handle_server_event(response_ready));
    assert!(!server.clients[&client_id].shell_endpoint_command_in_flight);

    match read_server_message(control_rx.recv().expect("endpoint response")) {
        ServerMessage::ClientShellEndpointResponseChunk {
            boot_id: response_boot_id,
            request_id,
            final_chunk,
            data,
        } => {
            assert_eq!(response_boot_id, boot_id);
            assert_eq!(request_id, "client-shell:1");
            assert!(final_chunk);
            let response = serde_json::from_slice::<api::schema::SuccessResponse>(&data)
                .expect("success response");
            assert_eq!(response.id, "client-shell:1");
            assert!(matches!(
                response.result,
                api::schema::ResponseResult::IntegrationList { .. }
            ));
        }
        other => panic!("expected client shell endpoint response, got {other:?}"),
    }
    shutdown_test_runtimes(&mut server);
}

#[test]
fn terminal_client_endpoint_request_error_removes_client() {
    let mut server = test_headless_server();
    let (writer, _control_rx, _render_rx) = test_client_writer();
    let client_id = 42;
    assert!(!server.handle_server_event(ServerEvent::ClientConnected {
        client_id,
        cols: 80,
        rows: 24,
        cell_width_px: 0,
        cell_height_px: 0,
        render_encoding: RenderEncoding::TerminalAnsi,
        keybindings: None,
        direct_attach_requested: true,
        direct_graphics: false,
        media_listen: false,
        pixel_mouse: false,
        writer,
        capabilities: Vec::new(),
    }));

    assert!(
        server.handle_server_event(ServerEvent::ClientShellEndpointRequestError {
            client_id,
            boot_id: "boot".into(),
            request_id: "request".into(),
            code: "unsupported_method",
            message: "unsupported".into(),
        })
    );
    assert!(!server.clients.contains_key(&client_id));
}

#[tokio::test]
async fn client_shell_receives_metadata_then_shell_free_pane_surface() {
    let mut server = test_headless_server();
    let mut workspace = crate::workspace::Workspace::test_new("shell-only-label");
    let pane_id = workspace.focused_pane_id().expect("focused pane");
    workspace.insert_test_runtime(
        pane_id,
        crate::terminal::TerminalRuntime::test_with_screen_bytes(
            80,
            23,
            b"\x1b[?1003h\x1b[?1006h\x1b[?1016hCLIENT_SHELL_LIVE",
        ),
    );
    server.app.state.workspaces = vec![workspace];
    server.app.state.active = Some(0);
    server.app.state.selected = 0;
    server.app.state.mode = crate::app::Mode::Terminal;
    server.app.state.product_announcement = Some(crate::app::state::ProductAnnouncementState {
        version: "0.8.2".into(),
        id: "client-shell".into(),
        title: "Client shell".into(),
        body: "announcement".into(),
        scroll: 0,
        preview: true,
    });
    server.server_config_diagnostic_without_keybindings = Some("endpoint config warning".into());

    let (writer, control_rx, render_rx) = test_client_writer();
    assert!(
        server.handle_server_event(ServerEvent::ClientShellConnected {
            client_id: 7,
            surface_cols: 80,
            surface_rows: 23,
            cell_width_px: 10,
            cell_height_px: 20,
            pixel_mouse: true,
            direct_graphics: false,
            endpoint_keybindings: false,
            mouse_capture: false,
            surface_active: true,
            writer,
        })
    );
    let snapshot = client_shell_snapshot(read_server_message(
        control_rx.recv().expect("shell snapshot"),
    ));
    assert_eq!(snapshot.workspaces.len(), 1);
    assert_eq!(snapshot.workspaces[0].label, "shell-only-label");
    assert_eq!(
        snapshot.config_diagnostic.as_deref(),
        Some("endpoint config warning")
    );
    assert_eq!(
        snapshot.product_announcement.as_ref().map(|announcement| (
            announcement.version.as_str(),
            announcement.id.as_str(),
            announcement.preview,
        )),
        Some(("0.8.2", "client-shell", true))
    );

    server.render_and_stream();
    let initial_surface = match read_server_message(render_rx.recv().expect("pane surface")) {
        ServerMessage::PaneSurface(surface) => {
            assert_eq!((surface.frame.width, surface.frame.height), (80, 23));
            let text = frame_text(&surface.frame);
            assert!(text.contains("CLIENT_SHELL_LIVE"), "surface: {text:?}");
            assert!(!text.contains("shell-only-label"), "surface: {text:?}");
            assert_eq!(surface.panes.len(), 1);
            assert_eq!(surface.panes[0].rect.x, 0);
            assert_eq!(surface.panes[0].rect.y, 0);
            assert!(surface.panes[0].sgr_pixel_mouse);
            assert_eq!(
                surface.panes[0].pixel_width,
                u32::from(surface.panes[0].inner_rect.width) * 10
            );
            assert_eq!(
                surface.panes[0].pixel_height,
                u32::from(surface.panes[0].inner_rect.height) * 20
            );
            surface
        }
        other => panic!("expected pane surface, got {other:?}"),
    };

    server
        .app
        .state
        .runtime_for_pane_in_workspace(&server.app.terminal_runtimes, 0, pane_id)
        .expect("pane runtime")
        .test_process_pty_bytes(b"\rPATCHED");
    let sources = std::collections::HashSet::from([pane_id]);
    assert!(server.render_retained_pane_surface_and_stream(&sources));
    match read_server_message(render_rx.recv().expect("pane surface patch")) {
        ServerMessage::PaneSurfacePatch(patch) => {
            assert_eq!(
                patch.base_surface_revision,
                initial_surface.surface_revision
            );
            assert_eq!(patch.surface_revision, initial_surface.surface_revision + 1);
            assert_eq!(patch.panes.len(), 1);
            assert!(!patch.rows.is_empty());
            assert!(patch
                .rows
                .iter()
                .flat_map(|row| &row.cells)
                .any(|cell| cell.symbol == "P"));
        }
        other => panic!("expected pane surface patch, got {other:?}"),
    }
    server
        .app
        .state
        .runtime_for_pane_in_workspace(&server.app.terminal_runtimes, 0, pane_id)
        .expect("pane runtime")
        .test_process_pty_bytes(b"\x1b[?1003l\x1b[?1006l\x1b[?1016l");
    assert!(server.render_retained_pane_surface_and_stream(&sources));
    match read_server_message(render_rx.recv().expect("metadata-only pane surface patch")) {
        ServerMessage::PaneSurfacePatch(patch) => {
            assert_eq!(patch.panes.len(), 1);
            assert!(!patch.panes[0].mouse_reporting);
            assert!(!patch.panes[0].sgr_pixel_mouse);
        }
        other => panic!("expected metadata-only pane surface patch, got {other:?}"),
    }
    let retained = server.clients[&7]
        .render_state
        .last_pane_surface()
        .expect("committed retained surface")
        .clone();
    server
        .clients
        .get_mut(&7)
        .unwrap()
        .render_state
        .request_repaint();
    server.render_and_stream();
    let full = match read_server_message(render_rx.recv().expect("full comparison surface")) {
        ServerMessage::PaneSurface(surface) => surface,
        other => panic!("expected full comparison surface, got {other:?}"),
    };
    assert!(full.surface_revision > retained.surface_revision);
    assert_eq!(retained.frame, full.frame);
    assert_eq!(retained.panes, full.panes);
    assert_eq!(retained.splits, full.splits);
    shutdown_test_runtimes(&mut server);
}

fn install_shared_view_test_runtime(server: &mut HeadlessServer) -> crate::layout::PaneId {
    let mut workspace = crate::workspace::Workspace::test_new("shared-view");
    let pane_id = workspace.focused_pane_id().expect("focused pane");
    workspace.insert_test_runtime(
        pane_id,
        crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 23, b"BASE"),
    );
    server.app.state.workspaces = vec![workspace];
    server.app.state.active = Some(0);
    server.app.state.selected = 0;
    server.app.state.mode = crate::app::Mode::Terminal;
    pane_id
}

fn connect_test_shell(
    server: &mut HeadlessServer,
    client_id: u64,
    surface_cols: u16,
    surface_rows: u16,
) -> (
    std::sync::mpsc::Receiver<Vec<u8>>,
    std::sync::mpsc::Receiver<Vec<u8>>,
) {
    let (writer, control, render) = test_client_writer();
    assert!(
        server.handle_server_event(ServerEvent::ClientShellConnected {
            client_id,
            surface_cols,
            surface_rows,
            cell_width_px: 0,
            cell_height_px: 0,
            pixel_mouse: false,
            direct_graphics: false,
            endpoint_keybindings: false,
            mouse_capture: false,
            surface_active: true,
            writer,
        })
    );
    (control, render)
}

fn connect_matching_test_shell(
    server: &mut HeadlessServer,
    client_id: u64,
) -> (
    std::sync::mpsc::Receiver<Vec<u8>>,
    std::sync::mpsc::Receiver<Vec<u8>>,
) {
    connect_test_shell(server, client_id, 80, 23)
}

fn write_shared_test_pane(
    server: &mut HeadlessServer,
    pane_id: crate::layout::PaneId,
    bytes: &[u8],
) {
    server
        .app
        .state
        .runtime_for_pane_in_workspace(&server.app.terminal_runtimes, 0, pane_id)
        .expect("pane runtime")
        .test_process_pty_bytes(bytes);
}

fn recv_pane_surface(
    receiver: &std::sync::mpsc::Receiver<Vec<u8>>,
    context: &str,
) -> crate::protocol::PaneSurfaceFrame {
    match read_server_message(
        receiver
            .recv()
            .unwrap_or_else(|error| panic!("{context}: {error}")),
    ) {
        ServerMessage::PaneSurface(surface) => surface,
        other => panic!("{context}: expected pane surface, got {other:?}"),
    }
}

fn recv_pane_surface_patch(
    receiver: &std::sync::mpsc::Receiver<Vec<u8>>,
    context: &str,
) -> crate::protocol::PaneSurfacePatch {
    match read_server_message(
        receiver
            .recv()
            .unwrap_or_else(|error| panic!("{context}: {error}")),
    ) {
        ServerMessage::PaneSurfacePatch(patch) => patch,
        other => panic!("{context}: expected pane surface patch, got {other:?}"),
    }
}

#[tokio::test]
async fn different_size_shells_receive_geometry_specific_patches_from_one_dirty_collection() {
    let mut server = test_headless_server();
    let pane_id = install_shared_view_test_runtime(&mut server);
    let (large_control, large_render) = connect_test_shell(&mut server, 7, 80, 23);
    let (small_control, small_render) = connect_test_shell(&mut server, 8, 68, 17);
    let _ = large_control.recv().expect("large snapshot");
    let _ = small_control.recv().expect("small snapshot");
    server.render_and_stream();
    let large_initial = recv_pane_surface(&large_render, "large initial surface");
    let small_initial = recv_pane_surface(&small_render, "small initial surface");
    assert_eq!(
        (large_initial.frame.width, large_initial.frame.height),
        (80, 23)
    );
    assert_eq!(
        (small_initial.frame.width, small_initial.frame.height),
        (68, 17)
    );
    assert_ne!(
        large_initial.panes[0].inner_rect,
        small_initial.panes[0].inner_rect
    );

    write_shared_test_pane(&mut server, pane_id, b"\rMIXED");
    assert!(server.render_retained_pane_surface_and_stream(&HashSet::from([pane_id])));

    let large_patch = recv_pane_surface_patch(&large_render, "large retained patch");
    let small_patch = recv_pane_surface_patch(&small_render, "small retained patch");
    assert_eq!(
        large_patch.base_surface_revision,
        large_initial.surface_revision
    );
    assert_eq!(
        small_patch.base_surface_revision,
        small_initial.surface_revision
    );
    assert!(large_patch.rows.iter().all(|row| {
        row.x
            .saturating_add(u16::try_from(row.cells.len()).unwrap_or(u16::MAX))
            <= large_initial.frame.width
            && row.y < large_initial.frame.height
    }));
    assert!(small_patch.rows.iter().all(|row| {
        row.x
            .saturating_add(u16::try_from(row.cells.len()).unwrap_or(u16::MAX))
            <= small_initial.frame.width
            && row.y < small_initial.frame.height
    }));
    assert_eq!(large_patch.rows, small_patch.rows);
    assert_ne!(
        large_patch.panes[0].inner_rect,
        small_patch.panes[0].inner_rect
    );
    assert!(frame_text(
        &server.clients[&7]
            .render_state
            .last_pane_surface()
            .expect("large retained surface")
            .frame
    )
    .contains("MIXED"));
    assert!(frame_text(
        &server.clients[&8]
            .render_state
            .last_pane_surface()
            .expect("small retained surface")
            .frame
    )
    .contains("MIXED"));

    write_shared_test_pane(&mut server, pane_id, b"\x1b[?1049hALT");
    assert!(!server.render_retained_pane_surface_and_stream(&HashSet::from([pane_id])));
    server.render_and_stream();
    let large_alt = recv_pane_surface(&large_render, "large alternate-screen surface");
    let small_alt = recv_pane_surface(&small_render, "small alternate-screen surface");
    assert!(large_alt.panes[0].alternate_screen_active);
    assert!(small_alt.panes[0].alternate_screen_active);
    assert_eq!(
        large_alt.panes[0].inner_rect.width,
        large_initial.panes[0].inner_rect.width + 1
    );
    assert_eq!(
        small_alt.panes[0].inner_rect.width,
        small_initial.panes[0].inner_rect.width + 1
    );

    write_shared_test_pane(&mut server, pane_id, b"\x1b[?1049l");
    assert!(!server.render_retained_pane_surface_and_stream(&HashSet::from([pane_id])));
    server.render_and_stream();
    let large_main = recv_pane_surface(&large_render, "large restored main-screen surface");
    let small_main = recv_pane_surface(&small_render, "small restored main-screen surface");
    assert!(!large_main.panes[0].alternate_screen_active);
    assert!(!small_main.panes[0].alternate_screen_active);
    assert_eq!(
        large_main.panes[0].inner_rect,
        large_initial.panes[0].inner_rect
    );
    assert_eq!(
        small_main.panes[0].inner_rect,
        small_initial.panes[0].inner_rect
    );

    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn retained_patches_only_reach_shells_viewing_the_dirty_tab() {
    let mut server = test_headless_server();
    let mut workspace = crate::workspace::Workspace::test_new("divergent-retained");
    let first_pane = workspace.tabs[0].root_pane;
    let second_tab = workspace.test_add_tab(Some("second"));
    let second_pane = workspace.tabs[second_tab].root_pane;
    workspace.insert_test_runtime(
        first_pane,
        crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 23, b"FIRST"),
    );
    workspace.insert_test_runtime(
        second_pane,
        crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 23, b"SECOND"),
    );
    server.app.state.workspaces = vec![workspace];
    server.app.state.active = Some(0);
    server.app.state.selected = 0;
    server.app.state.mode = crate::app::Mode::Terminal;
    let second_tab_id = server.app.public_tab_id(0, second_tab).unwrap();

    let (first_control, first_render) = connect_matching_test_shell(&mut server, 7);
    let (second_control, second_render) = connect_matching_test_shell(&mut server, 8);
    let _ = first_control.recv().expect("first snapshot");
    let _ = second_control.recv().expect("second snapshot");
    assert!(server.focus_shell_client_on_tab(8, &second_tab_id));
    assert!(server.claim_shell_tab_geometry(8, false));
    assert!(
        server.pty_sources_visible_to_any_render_target(&HashSet::from([first_pane, second_pane,]))
    );
    server.render_and_stream();
    let _ = recv_pane_surface(&first_render, "first baseline");
    let _ = recv_pane_surface(&second_render, "second baseline");

    server.app.state.workspaces[0].test_runtimes[&first_pane]
        .test_process_pty_bytes(b"\rFIRST_PATCH");
    assert!(server.render_retained_pane_surface_and_stream(&HashSet::from([first_pane])));
    let first_patch = recv_pane_surface_patch(&first_render, "first patch");
    assert_eq!(first_patch.panes.len(), 1);
    assert!(second_render.try_recv().is_err());

    server.app.state.workspaces[0].test_runtimes[&second_pane]
        .test_process_pty_bytes(b"\rSECOND_PATCH");
    assert!(server.render_retained_pane_surface_and_stream(&HashSet::from([second_pane])));
    let second_patch = recv_pane_surface_patch(&second_render, "second patch");
    assert_eq!(second_patch.panes.len(), 1);
    assert!(first_render.try_recv().is_err());

    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn backpressured_shell_does_not_disable_retained_patches_for_responsive_peer() {
    let mut server = test_headless_server();
    let pane_id = install_shared_view_test_runtime(&mut server);
    let (responsive_control, responsive_render) = connect_matching_test_shell(&mut server, 7);
    let (slow_control, slow_render) = connect_matching_test_shell(&mut server, 8);
    let _ = responsive_control.recv().expect("responsive snapshot");
    let _ = slow_control.recv().expect("slow snapshot");
    server.render_and_stream();
    let _ = responsive_render
        .recv()
        .expect("responsive initial surface");
    let _ = slow_render.recv().expect("slow initial surface");

    let sources = HashSet::from([pane_id]);
    write_shared_test_pane(&mut server, pane_id, b"\rONE");
    assert!(server.render_retained_pane_surface_and_stream(&sources));
    assert!(matches!(
        read_server_message(responsive_render.recv().expect("responsive first patch")),
        ServerMessage::PaneSurfacePatch(_)
    ));

    write_shared_test_pane(&mut server, pane_id, b"\rTWO");
    assert!(server.render_retained_pane_surface_and_stream(&sources));
    assert!(matches!(
        read_server_message(responsive_render.recv().expect("responsive second patch")),
        ServerMessage::PaneSurfacePatch(_)
    ));
    assert_eq!(server.clients[&8].deferred_render(), DeferredRender::Full);

    write_shared_test_pane(&mut server, pane_id, b"\rTHREE");
    assert!(server.render_retained_pane_surface_and_stream(&sources));
    assert!(matches!(
        read_server_message(responsive_render.recv().expect("responsive third patch")),
        ServerMessage::PaneSurfacePatch(_)
    ));

    assert!(matches!(
        read_server_message(slow_render.recv().expect("slow queued first patch")),
        ServerMessage::PaneSurfacePatch(_)
    ));
    assert!(server.handle_server_event(ServerEvent::ClientWriterDrained { client_id: 8 }));
    server.render_and_stream();
    assert!(matches!(
        read_server_message(slow_render.recv().expect("slow full recovery surface")),
        ServerMessage::PaneSurface(_)
    ));

    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn full_render_backpressure_does_not_disable_responsive_peer_patches() {
    let mut server = test_headless_server();
    let pane_id = install_shared_view_test_runtime(&mut server);
    let (responsive_control, responsive_render) = connect_matching_test_shell(&mut server, 7);
    let (slow_control, slow_render) = connect_matching_test_shell(&mut server, 8);
    let _ = responsive_control.recv().expect("responsive snapshot");
    let _ = slow_control.recv().expect("slow snapshot");
    server.render_and_stream();
    let _ = responsive_render
        .recv()
        .expect("responsive initial surface");
    // Keep the slow client's initial surface queued, then force another full
    // replacement for both clients.
    server.clients.get_mut(&7).unwrap().request_repaint();
    server.clients.get_mut(&8).unwrap().request_repaint();
    server.app.full_redraw_pending = true;
    server.render_and_stream();
    let _ = responsive_render
        .recv()
        .expect("responsive full replacement");
    assert_eq!(server.clients[&8].deferred_render(), DeferredRender::Full);
    assert!(!server.app.full_redraw_pending);

    write_shared_test_pane(&mut server, pane_id, b"\rPATCH");
    assert!(server.render_retained_pane_surface_and_stream(&HashSet::from([pane_id])));
    assert!(matches!(
        read_server_message(responsive_render.recv().expect("responsive retained patch")),
        ServerMessage::PaneSurfacePatch(_)
    ));

    let _ = slow_render.recv().expect("slow queued initial surface");
    assert!(server.handle_server_event(ServerEvent::ClientWriterDrained { client_id: 8 }));
    server.render_and_stream();
    assert!(matches!(
        read_server_message(slow_render.recv().expect("slow full recovery surface")),
        ServerMessage::PaneSurface(_)
    ));

    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn client_shell_config_diagnostics_follow_keybinding_ownership() {
    let mut server = test_headless_server();
    server.server_config_diagnostic = Some("server keybinding warning\ntheme warning".into());
    server.server_config_diagnostic_without_keybindings = Some("theme warning".into());

    let (local_writer, local_control, _local_render) = test_client_writer();
    assert!(
        server.handle_server_event(ServerEvent::ClientShellConnected {
            client_id: 13,
            surface_cols: 80,
            surface_rows: 23,
            cell_width_px: 0,
            cell_height_px: 0,
            pixel_mouse: false,
            direct_graphics: false,
            endpoint_keybindings: false,
            mouse_capture: false,
            surface_active: true,
            writer: local_writer,
        })
    );
    let local_snapshot = client_shell_snapshot(read_server_message(
        local_control.recv().expect("local shell snapshot"),
    ));
    assert_eq!(
        local_snapshot.config_diagnostic.as_deref(),
        Some("theme warning")
    );

    let (endpoint_writer, endpoint_control, _endpoint_render) = test_client_writer();
    assert!(
        server.handle_server_event(ServerEvent::ClientShellConnected {
            client_id: 14,
            surface_cols: 80,
            surface_rows: 23,
            cell_width_px: 0,
            cell_height_px: 0,
            pixel_mouse: false,
            direct_graphics: false,
            endpoint_keybindings: true,
            mouse_capture: false,
            surface_active: true,
            writer: endpoint_writer,
        })
    );
    let endpoint_snapshot = client_shell_snapshot(read_server_message(
        endpoint_control.recv().expect("endpoint shell snapshot"),
    ));
    assert_eq!(
        endpoint_snapshot.config_diagnostic.as_deref(),
        Some("server keybinding warning\ntheme warning")
    );

    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn client_shell_tab_focus_changes_only_the_source_connection() {
    let mut server = test_headless_server();
    let mut workspace = crate::workspace::Workspace::test_new("independent-tabs");
    let second_tab = workspace.test_add_tab(Some("second"));
    server.app.state.workspaces = vec![workspace];
    server.app.state.ensure_test_terminals();
    server.app.state.active = Some(0);
    server.app.state.selected = 0;
    server.app.state.mode = crate::app::Mode::Terminal;
    let tab_ids = server
        .app
        .session_snapshot()
        .tabs
        .into_iter()
        .map(|tab| tab.tab_id)
        .collect::<Vec<_>>();
    let first_tab_id = tab_ids[0].clone();
    let second_tab_id = tab_ids[second_tab].clone();

    let (first_control, _first_render) = connect_test_shell(&mut server, 7, 100, 30);
    let (second_control, _second_render) = connect_test_shell(&mut server, 8, 80, 24);
    let first_initial = client_shell_snapshot(read_server_message(
        first_control.recv().expect("first snapshot"),
    ));
    let second_initial = client_shell_snapshot(read_server_message(
        second_control.recv().expect("second snapshot"),
    ));
    assert_eq!(
        first_initial.focused_tab_id.as_deref(),
        Some(first_tab_id.as_str())
    );
    assert_eq!(
        second_initial.focused_tab_id.as_deref(),
        Some(first_tab_id.as_str())
    );

    assert!(
        server.handle_server_event(ServerEvent::ClientShellEndpointRequest {
            client_id: 8,
            boot_id: server.client_shell_boot_id.clone(),
            request: Box::new(api::schema::Request {
                id: "focus-second".into(),
                method: api::schema::Method::TabFocus(api::schema::TabTarget {
                    tab_id: second_tab_id.clone(),
                }),
            }),
        })
    );
    let response_ready = server
        .server_event_rx
        .recv()
        .await
        .expect("focus response ready");
    assert!(!server.handle_server_event(response_ready));
    let _ = second_control.recv().expect("focus response");

    server.render_and_stream();

    assert!(
        first_control.try_recv().is_err(),
        "another shell must not receive a navigation replacement"
    );
    let second_replacement = client_shell_snapshot(read_server_message(
        second_control.recv().expect("second replacement snapshot"),
    ));
    assert_eq!(
        second_replacement.focused_tab_id.as_deref(),
        Some(second_tab_id.as_str())
    );
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn deferred_worktree_response_moves_only_its_source_client() {
    let mut server = test_headless_server();
    let mut workspace = crate::workspace::Workspace::test_new("deferred-worktree");
    let created_tab = workspace.test_add_tab(Some("created"));
    server.app.state.workspaces = vec![workspace];
    server.app.state.ensure_test_terminals();
    server.app.state.active = Some(0);
    server.app.state.selected = 0;
    server.app.state.mode = crate::app::Mode::Terminal;
    let created_tab_id = server.app.public_tab_id(0, created_tab).unwrap();

    let (source_control, _) = connect_matching_test_shell(&mut server, 51);
    let (other_control, _) = connect_matching_test_shell(&mut server, 52);
    let _ = source_control.recv().expect("source snapshot");
    let _ = other_control.recv().expect("other snapshot");
    let original_tab_id = server.shell_tab_id_for_client(51).unwrap();
    server
        .clients
        .get_mut(&51)
        .unwrap()
        .shell_endpoint_command_in_flight = true;
    let source_surface_revision = server.clients[&51].shell_projection_revision;
    server
        .clients
        .get_mut(&51)
        .unwrap()
        .shell_endpoint_command_surface_revision = Some(source_surface_revision);
    server
        .clients
        .get_mut(&51)
        .unwrap()
        .shell_deferred_navigation_response = Some(Vec::new());
    let response = serde_json::json!({
        "id": "create-worktree",
        "result": {
            "type": "worktree_created",
            "tab": { "tab_id": created_tab_id }
        }
    });

    assert!(
        server.handle_server_event(ServerEvent::ClientShellEndpointResponseChunkReady {
            client_id: 51,
            boot_id: server.client_shell_boot_id.clone(),
            request_id: "create-worktree".into(),
            final_chunk: true,
            data: serde_json::to_vec(&response).unwrap(),
        })
    );
    assert_eq!(
        server.shell_tab_id_for_client(51).as_deref(),
        response
            .pointer("/result/tab/tab_id")
            .and_then(serde_json::Value::as_str)
    );
    assert_eq!(
        server.shell_tab_id_for_client(52).as_deref(),
        Some(original_tab_id.as_str())
    );

    assert!(server.focus_shell_client_on_tab(51, &original_tab_id));
    // A source command begun in the old presentation epoch may finish after source-off and
    // source-on rollback. Its response remains endpoint-local, but it must not apply deferred
    // client navigation to the restored source.
    assert!(server.set_client_shell_surface_active(51, false).is_some());
    assert!(server.set_client_shell_surface_active(51, true).is_some());
    server
        .clients
        .get_mut(&51)
        .unwrap()
        .shell_endpoint_command_in_flight = true;
    server
        .clients
        .get_mut(&51)
        .unwrap()
        .shell_endpoint_command_surface_revision = Some(source_surface_revision);
    server
        .clients
        .get_mut(&51)
        .unwrap()
        .shell_deferred_navigation_request_id = Some("background-worktree".into());
    server
        .clients
        .get_mut(&51)
        .unwrap()
        .shell_deferred_navigation_response = Some(Vec::new());
    assert!(
        !server.handle_server_event(ServerEvent::ClientShellEndpointResponseChunkReady {
            client_id: 51,
            boot_id: server.client_shell_boot_id.clone(),
            request_id: "background-worktree".into(),
            final_chunk: true,
            data: serde_json::to_vec(&response).unwrap(),
        })
    );
    assert_eq!(
        server.shell_tab_id_for_client(51).as_deref(),
        Some(original_tab_id.as_str())
    );
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn client_local_navigation_does_not_emit_global_focus_transitions() {
    let mut server = test_headless_server();
    let mut workspace = crate::workspace::Workspace::test_new("independent-focus");
    let first_pane = workspace.tabs[0].root_pane;
    let second_tab = workspace.test_add_tab(Some("second"));
    let second_pane = workspace.tabs[second_tab].root_pane;
    let (first_runtime, mut first_input) =
        crate::terminal::TerminalRuntime::test_with_channel_and_scrollback_bytes(
            80,
            24,
            0,
            b"\x1b[?1004h",
            4,
        );
    let (second_runtime, mut second_input) =
        crate::terminal::TerminalRuntime::test_with_channel_and_scrollback_bytes(
            80,
            24,
            0,
            b"\x1b[?1004h",
            4,
        );
    workspace.insert_test_runtime(first_pane, first_runtime);
    workspace.insert_test_runtime(second_pane, second_runtime);
    server.app.state.workspaces = vec![workspace];
    server.app.state.active = Some(0);
    server.app.state.selected = 0;
    server.app.state.mode = crate::app::Mode::Terminal;
    let second_tab_id = server.app.public_tab_id(0, second_tab).unwrap();

    let (first_control, _) = connect_matching_test_shell(&mut server, 61);
    let (second_control, _) = connect_matching_test_shell(&mut server, 62);
    let _ = first_control.recv().expect("first snapshot");
    let _ = second_control.recv().expect("second snapshot");
    assert!(server.focus_shell_client_on_tab(62, &second_tab_id));
    server.clients.get_mut(&61).unwrap().outer_terminal_focus = Some(true);
    server.clients.get_mut(&62).unwrap().outer_terminal_focus = Some(true);

    let (respond_to, _response_rx) = std::sync::mpsc::channel();
    server.handle_client_shell_api_request(
        62,
        crate::api::ApiRequestMessage {
            request: crate::api::schema::Request {
                id: "focus-own-tab".into(),
                method: crate::api::schema::Method::TabFocus(crate::api::schema::TabTarget {
                    tab_id: second_tab_id,
                }),
            },
            respond_to,
            response_write_complete: None,
            stream_active: None,
        },
    );
    server.app.sync_focus_events();
    assert!(first_input.try_recv().is_err());
    assert!(second_input.try_recv().is_err());
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn public_focus_moves_shell_focus_between_tabs() {
    let mut server = test_headless_server();
    let mut workspace = crate::workspace::Workspace::test_new("public-focus-events");
    let first_pane = workspace.tabs[0].root_pane;
    let second_tab = workspace.test_add_tab(Some("second"));
    let second_pane = workspace.tabs[second_tab].root_pane;
    let (first_runtime, mut first_input) =
        crate::terminal::TerminalRuntime::test_with_channel_and_scrollback_bytes(
            80,
            24,
            0,
            b"\x1b[?1004h",
            4,
        );
    let (second_runtime, mut second_input) =
        crate::terminal::TerminalRuntime::test_with_channel_and_scrollback_bytes(
            80,
            24,
            0,
            b"\x1b[?1004h",
            4,
        );
    workspace.insert_test_runtime(first_pane, first_runtime);
    workspace.insert_test_runtime(second_pane, second_runtime);
    server.app.state.workspaces = vec![workspace];
    server.app.state.active = Some(0);
    server.app.state.selected = 0;
    server.app.state.mode = crate::app::Mode::Terminal;
    let second_tab_id = server
        .app
        .public_tab_id(0, second_tab)
        .expect("second tab id");

    let (first_control, _) = connect_matching_test_shell(&mut server, 63);
    let (second_control, _) = connect_matching_test_shell(&mut server, 64);
    let _ = first_control.recv().expect("first snapshot");
    let _ = second_control.recv().expect("second snapshot");
    assert!(server.focus_shell_client_on_tab(64, &second_tab_id));
    server.clients.get_mut(&63).unwrap().outer_terminal_focus = Some(true);
    server.clients.get_mut(&64).unwrap().outer_terminal_focus = Some(true);
    assert!(server.app.state.switch_workspace_tab(0, second_tab));

    server.focus_all_shell_clients_on_default_target();

    assert_eq!(
        server.shell_tab_id_for_client(63).as_deref(),
        Some(second_tab_id.as_str())
    );
    assert_eq!(
        first_input.try_recv().expect("previous tab focus lost"),
        Bytes::from_static(b"\x1b[O")
    );
    assert!(
        second_input.try_recv().is_err(),
        "focus gain was duplicated"
    );
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn repeated_layout_action_reapplies_controller_geometry() {
    let mut server = test_headless_server();
    let mut workspace = crate::workspace::Workspace::test_new("layout-geometry");
    let first_pane = workspace.tabs[0].root_pane;
    let second_pane = workspace.test_split(ratatui::layout::Direction::Horizontal);
    workspace.insert_test_runtime(
        first_pane,
        crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 24, b""),
    );
    workspace.insert_test_runtime(
        second_pane,
        crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 24, b""),
    );
    server.app.state.workspaces = vec![workspace];
    server.app.state.active = Some(0);
    server.app.state.selected = 0;
    server.app.state.mode = crate::app::Mode::Terminal;
    let tab_id = server.app.public_tab_id(0, 0).expect("tab id");

    let (control, _) = connect_test_shell(&mut server, 65, 100, 30);
    let _ = control.recv().expect("snapshot");
    let before = server.app.state.workspaces[0].test_runtimes[&first_pane].current_size();
    let (respond_to, _response_rx) = std::sync::mpsc::channel();

    assert!(server.handle_client_shell_api_request(
        65,
        crate::api::ApiRequestMessage {
            request: crate::api::schema::Request {
                id: "resize-layout".into(),
                method: crate::api::schema::Method::LayoutSetSplitRatio(
                    crate::api::schema::LayoutSetSplitRatioParams {
                        tab_id: Some(tab_id),
                        pane_id: None,
                        path: Vec::new(),
                        ratio: 0.8,
                    },
                ),
            },
            respond_to,
            response_write_complete: None,
            stream_active: None,
        },
    ));

    let after = server.app.state.workspaces[0].test_runtimes[&first_pane].current_size();
    assert_ne!(after, before);
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn public_close_reapplies_controller_geometry() {
    let mut server = test_headless_server();
    let mut workspace = crate::workspace::Workspace::test_new("public-close-geometry");
    let first_pane = workspace.tabs[0].root_pane;
    let second_pane = workspace.test_split(ratatui::layout::Direction::Vertical);
    workspace.insert_test_runtime(
        first_pane,
        crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 24, b""),
    );
    workspace.insert_test_runtime(
        second_pane,
        crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 24, b""),
    );
    server.app.state.workspaces = vec![workspace];
    server.app.state.active = Some(0);
    server.app.state.selected = 0;
    server.app.state.mode = crate::app::Mode::Terminal;
    let second_pane_id = server.app.public_pane_id(0, second_pane).unwrap();

    let (control, _) = connect_test_shell(&mut server, 66, 100, 30);
    let _ = control.recv().expect("snapshot");
    let shrunk = server.app.state.workspaces[0].test_runtimes[&first_pane].current_size();
    assert!(shrunk.0 < 30);

    let (respond_to, _response_rx) = std::sync::mpsc::channel();
    assert!(
        server.handle_api_request_with_shutdown_check(crate::api::ApiRequestMessage {
            request: crate::api::schema::Request {
                id: "public-close-geometry".into(),
                method: crate::api::schema::Method::PaneClose(crate::api::schema::PaneTarget {
                    pane_id: second_pane_id,
                }),
            },
            respond_to,
            response_write_complete: None,
            stream_active: None,
        })
    );

    let runtime = &server.app.state.workspaces[0].test_runtimes[&first_pane];
    let grown = runtime.current_size();
    assert!(grown.0 > shrunk.0);
    assert_eq!(runtime.terminal_dimensions(), Some((grown.1, grown.0)));
    assert_eq!(
        runtime.scroll_metrics().unwrap().viewport_rows,
        grown.0 as usize
    );
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn geometry_reapply_replaces_a_controller_that_left_the_tab() {
    let mut server = test_headless_server();
    let mut workspace = crate::workspace::Workspace::test_new("geometry-controller-viewer");
    let first_pane = workspace.tabs[0].root_pane;
    let second_tab = workspace.test_add_tab(Some("second"));
    let second_pane = workspace.tabs[second_tab].root_pane;
    let third_tab = workspace.test_add_tab(Some("third"));
    let third_pane = workspace.tabs[third_tab].root_pane;
    for pane_id in [first_pane, second_pane, third_pane] {
        workspace.insert_test_runtime(
            pane_id,
            crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 24, b""),
        );
    }
    server.app.state.workspaces = vec![workspace];
    server.app.state.active = Some(0);
    server.app.state.selected = 0;
    server.app.state.mode = crate::app::Mode::Terminal;
    let second_tab_id = server.app.public_tab_id(0, second_tab).unwrap();
    let third_tab_id = server.app.public_tab_id(0, third_tab).unwrap();

    let (first_control, _) = connect_test_shell(&mut server, 67, 100, 30);
    let (second_control, _) = connect_test_shell(&mut server, 68, 70, 20);
    let _ = first_control.recv().expect("first snapshot");
    let _ = second_control.recv().expect("second snapshot");

    assert!(server.focus_shell_client_on_tab(67, &second_tab_id));
    assert!(server.claim_shell_tab_geometry(67, false));
    assert!(server.focus_shell_client_on_tab(67, &third_tab_id));
    assert!(server.claim_shell_tab_geometry(67, false));
    assert!(server.focus_shell_client_on_tab(68, &second_tab_id));
    assert_eq!(
        server.tab_geometry_controllers.get(&second_tab_id),
        Some(&67)
    );
    let stale_size = server.app.state.workspaces[0].test_runtimes[&second_pane].current_size();

    assert!(server.reapply_controlled_shell_tab_geometry(false));

    assert_eq!(
        server.tab_geometry_controllers.get(&second_tab_id),
        Some(&68)
    );
    assert_ne!(
        server.app.state.workspaces[0].test_runtimes[&second_pane].current_size(),
        stale_size
    );
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn client_shell_tabs_render_accept_input_and_resize_independently() {
    let mut server = test_headless_server();
    let mut workspace = crate::workspace::Workspace::test_new("independent-geometry");
    let first_pane = workspace.tabs[0].root_pane;
    let second_tab = workspace.test_add_tab(Some("second"));
    let second_pane = workspace.tabs[second_tab].root_pane;
    workspace.insert_test_runtime(
        first_pane,
        crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 24, b"FIRST_TAB"),
    );
    let (second_runtime, mut second_input) =
        crate::terminal::TerminalRuntime::test_with_channel_and_scrollback_bytes(
            80,
            24,
            0,
            b"SECOND_TAB",
            4,
        );
    workspace.insert_test_runtime(second_pane, second_runtime);
    server.app.state.workspaces = vec![workspace];
    server.app.state.active = Some(0);
    server.app.state.selected = 0;
    server.app.state.mode = crate::app::Mode::Terminal;
    let second_tab_id = server.app.public_tab_id(0, second_tab).unwrap();
    let second_pane_id = server.app.public_pane_id(0, second_pane).unwrap();
    let initial_second_size =
        server.app.state.workspaces[0].test_runtimes[&second_pane].current_size();

    let (first_control, first_render) = connect_test_shell(&mut server, 21, 100, 30);
    let _ = first_control.recv().expect("first snapshot");
    let first_size = server.app.state.workspaces[0].test_runtimes[&first_pane].current_size();
    let singleton_second_size =
        server.app.state.workspaces[0].test_runtimes[&second_pane].current_size();
    assert_ne!(singleton_second_size, initial_second_size);
    assert_eq!(singleton_second_size, first_size);

    let (second_control, second_render) = connect_test_shell(&mut server, 22, 70, 20);
    let _ = second_control.recv().expect("second snapshot");

    assert!(server.focus_shell_client_on_tab(22, &second_tab_id));
    assert!(server.claim_shell_tab_geometry(22, false));
    let second_size = server.app.state.workspaces[0].test_runtimes[&second_pane].current_size();
    assert_ne!(first_size, second_size);
    assert_eq!(
        server.app.state.workspaces[0].test_runtimes[&first_pane].current_size(),
        first_size
    );

    server.handle_server_event(ServerEvent::ClientShellPaneInput {
        client_id: 22,
        pane_id: second_pane_id,
        events: vec![crate::protocol::ClientPaneInputEvent::TextCommit(
            "typed".into(),
        )],
    });
    assert_eq!(
        second_input.try_recv().expect("second tab input"),
        Bytes::from_static(b"typed")
    );

    server.render_and_stream();
    let first_surface = match read_server_message(first_render.recv().expect("first surface")) {
        ServerMessage::PaneSurface(surface) => surface,
        other => panic!("expected first pane surface, got {other:?}"),
    };
    let second_surface = match read_server_message(second_render.recv().expect("second surface")) {
        ServerMessage::PaneSurface(surface) => surface,
        other => panic!("expected second pane surface, got {other:?}"),
    };
    assert!(frame_text(&first_surface.frame).contains("FIRST_TAB"));
    assert!(frame_text(&second_surface.frame).contains("SECOND_TAB"));

    assert!(server.handle_server_event(ServerEvent::ClientShellResize {
        client_id: 22,
        surface_cols: 60,
        surface_rows: 16,
        cell_width_px: 0,
        cell_height_px: 0,
        pixel_mouse: false,
    }));
    let resized_second = server.app.state.workspaces[0].test_runtimes[&second_pane].current_size();
    assert_ne!(resized_second, second_size);
    assert_eq!(
        server.app.state.workspaces[0].test_runtimes[&first_pane].current_size(),
        first_size
    );

    assert!(server.focus_shell_client_on_tab(21, &second_tab_id));
    assert!(server.claim_shell_tab_geometry(21, false));
    assert_ne!(
        server.app.state.workspaces[0].test_runtimes[&second_pane].current_size(),
        resized_second
    );

    server.remove_client_and_resize_if_needed(21);
    let singleton_first = server.app.state.workspaces[0].test_runtimes[&first_pane].current_size();
    let singleton_second =
        server.app.state.workspaces[0].test_runtimes[&second_pane].current_size();
    assert_ne!(singleton_first, first_size);
    assert_eq!(singleton_first, singleton_second);
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn public_background_tab_create_preserves_client_locations() {
    let mut server = test_headless_server();
    let mut workspace = crate::workspace::Workspace::test_new("background-create");
    let second_tab = workspace.test_add_tab(Some("second"));
    server.app.state.workspaces = vec![workspace];
    server.app.state.ensure_test_terminals();
    server.app.state.active = Some(0);
    server.app.state.selected = 0;
    server.app.state.mode = crate::app::Mode::Terminal;
    let workspace_id = server.app.public_workspace_id(0);
    let first_tab_id = server.app.public_tab_id(0, 0).unwrap();
    let second_tab_id = server.app.public_tab_id(0, second_tab).unwrap();

    let (first_control, _) = connect_matching_test_shell(&mut server, 71);
    let (second_control, _) = connect_matching_test_shell(&mut server, 72);
    let _ = first_control.recv().expect("first snapshot");
    let _ = second_control.recv().expect("second snapshot");
    assert!(server.focus_shell_client_on_tab(71, &second_tab_id));

    let (respond_to, _response_rx) = std::sync::mpsc::channel();
    server.handle_api_request_with_shutdown_check(crate::api::ApiRequestMessage {
        request: crate::api::schema::Request {
            id: "create-background-tab".into(),
            method: crate::api::schema::Method::TabCreate(crate::api::schema::TabCreateParams {
                workspace_id: Some(workspace_id),
                cwd: None,
                focus: false,
                label: Some("background".into()),
                env: std::collections::HashMap::new(),
            }),
        },
        respond_to,
        response_write_complete: None,
        stream_active: None,
    });

    assert_eq!(
        server.shell_tab_id_for_client(71).as_deref(),
        Some(second_tab_id.as_str())
    );
    assert_eq!(
        server.shell_tab_id_for_client(72).as_deref(),
        Some(first_tab_id.as_str())
    );
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn public_workspace_focus_preserves_each_clients_remembered_tabs() {
    let mut server = test_headless_server();
    let mut first = crate::workspace::Workspace::test_new("first");
    let second_tab = first.test_add_tab(Some("second"));
    let second = crate::workspace::Workspace::test_new("second");
    server.app.state.workspaces = vec![first, second];
    server.app.state.ensure_test_terminals();
    server.app.state.active = Some(0);
    server.app.state.selected = 0;
    server.app.state.mode = crate::app::Mode::Terminal;
    let first_workspace_id = server.app.public_workspace_id(0);
    let second_workspace_id = server.app.public_workspace_id(1);
    let first_tab_id = server.app.public_tab_id(0, 0).unwrap();
    let second_tab_id = server.app.public_tab_id(0, second_tab).unwrap();

    let (first_control, _) = connect_test_shell(&mut server, 41, 100, 30);
    let (second_control, _) = connect_test_shell(&mut server, 42, 80, 24);
    let _ = first_control.recv().expect("first snapshot");
    let _ = second_control.recv().expect("second snapshot");
    assert!(server.focus_shell_client_on_tab(41, &second_tab_id));

    let (respond_to, _response_rx) = std::sync::mpsc::channel();
    server.handle_api_request_with_shutdown_check(crate::api::ApiRequestMessage {
        request: crate::api::schema::Request {
            id: "focus-second-workspace".into(),
            method: crate::api::schema::Method::WorkspaceFocus(
                crate::api::schema::WorkspaceTarget {
                    workspace_id: second_workspace_id.clone(),
                },
            ),
        },
        respond_to,
        response_write_complete: None,
        stream_active: None,
    });

    let first_location = server.clients[&41].shell_location.as_ref().unwrap();
    let second_location = server.clients[&42].shell_location.as_ref().unwrap();
    assert_eq!(
        first_location.focused_workspace_id.as_deref(),
        Some(second_workspace_id.as_str())
    );
    assert_eq!(
        second_location.focused_workspace_id.as_deref(),
        Some(second_workspace_id.as_str())
    );
    assert_eq!(
        first_location.active_tab_ids[&first_workspace_id],
        second_tab_id
    );
    assert_eq!(
        second_location.active_tab_ids[&first_workspace_id],
        first_tab_id
    );
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn public_api_focus_replaces_every_client_shell_projection() {
    let mut server = test_headless_server();
    let first = crate::workspace::Workspace::test_new("first");
    let second = crate::workspace::Workspace::test_new("second");
    server.app.state.workspaces = vec![first, second];
    server.app.state.ensure_test_terminals();
    server.app.state.active = Some(0);
    server.app.state.selected = 0;
    server.app.state.mode = crate::app::Mode::Terminal;
    let second_id = server.app.session_snapshot().workspaces[1]
        .workspace_id
        .clone();

    let (writer, control_rx, render_rx) = test_client_writer();
    assert!(
        server.handle_server_event(ServerEvent::ClientShellConnected {
            client_id: 9,
            surface_cols: 80,
            surface_rows: 23,
            cell_width_px: 0,
            cell_height_px: 0,
            pixel_mouse: false,
            direct_graphics: false,
            endpoint_keybindings: false,
            mouse_capture: false,
            surface_active: true,
            writer,
        })
    );
    let initial_revision = client_shell_snapshot(read_server_message(
        control_rx.recv().expect("initial snapshot"),
    ))
    .revision;

    let (respond_to, _response_rx) = std::sync::mpsc::channel();
    server.handle_api_request_with_shutdown_check(crate::api::ApiRequestMessage {
        request: crate::api::schema::Request {
            id: "test.client.shell.workspace.focus".into(),
            method: crate::api::schema::Method::WorkspaceFocus(
                crate::api::schema::WorkspaceTarget {
                    workspace_id: second_id.clone(),
                },
            ),
        },
        respond_to,
        response_write_complete: None,
        stream_active: None,
    });
    assert_eq!(server.app.state.active, Some(1));
    server.render_and_stream();

    let replacement = client_shell_snapshot(read_server_message(
        control_rx.recv().expect("replacement snapshot"),
    ));
    assert!(replacement.revision > initial_revision);
    assert_eq!(
        replacement.focused_workspace_id.as_deref(),
        Some(second_id.as_str())
    );
    match read_server_message(render_rx.recv().expect("replacement pane surface")) {
        ServerMessage::PaneSurface(surface) => {
            assert_eq!(surface.projection_revision, replacement.revision);
        }
        other => panic!("expected replacement pane surface, got {other:?}"),
    }
    shutdown_test_runtimes(&mut server);
}

// TP-CLIENT-MODE-05
#[tokio::test]
async fn app_client_clipboard_image_pastes_the_staged_path_into_the_focused_pane() {
    let mut server = test_headless_server();
    let mut input_rx = install_focused_test_runtime(&mut server, b"");
    let pane_id = server.app.session_snapshot().focused_pane_id.unwrap();
    server.clients.insert(
        11,
        ClientConnection::app(
            (80, 24),
            crate::kitty_graphics::HostCellSize::default(),
            crate::terminal_theme::TerminalTheme::default(),
            None,
            1,
            RenderEncoding::SemanticFrame,
            None,
        ),
    );

    assert!(server.client_clipboard_image_target_is_valid(
        11,
        &crate::protocol::ClientClipboardImageTarget::DirectTerminal,
    ));
    assert!(!server.client_clipboard_image_target_is_valid(
        11,
        &crate::protocol::ClientClipboardImageTarget::Pane(pane_id),
    ));
    assert!(server.paste_client_clipboard_image_path(
        11,
        crate::protocol::ClientClipboardImageTarget::DirectTerminal,
        "/tmp/app-client-image.png".into(),
    ));
    assert_eq!(server.foreground_client_id, Some(11));
    assert_eq!(
        input_rx.try_recv().expect("app client image path"),
        Bytes::from_static(b"/tmp/app-client-image.png")
    );
}

#[tokio::test]
async fn client_shell_input_targets_runtime_without_server_shell_classification() {
    let mut server = test_headless_server();
    let mut input_rx = install_focused_test_runtime(&mut server, b"\x1b[?1000h\x1b[?1006h");
    let pane_id = server.app.session_snapshot().focused_pane_id.unwrap();
    server.clients.insert(
        11,
        ClientConnection::new_with_mode(
            ClientConnectionMode::ClientShell,
            (80, 24),
            crate::kitty_graphics::HostCellSize::default(),
            1,
            RenderEncoding::SemanticFrame,
            None,
        ),
    );

    assert!(
        server.handle_server_event(ServerEvent::ClientShellPaneInput {
            client_id: 11,
            pane_id,
            events: vec![
                crate::protocol::ClientPaneInputEvent::Key {
                    code: crate::protocol::ClientKeyCode::Char('c'),
                    modifiers: crossterm::event::KeyModifiers::CONTROL.bits(),
                    kind: crate::protocol::ClientKeyKind::Press,
                    repeat_count: 1,
                    shifted_codepoint: None,
                    generated_text: None,
                    tracks_release: true,
                    physical_key_id: None,
                    windows_record: None,
                },
                crate::protocol::ClientPaneInputEvent::Key {
                    code: crate::protocol::ClientKeyCode::Char('c'),
                    modifiers: crossterm::event::KeyModifiers::CONTROL.bits(),
                    kind: crate::protocol::ClientKeyKind::Release,
                    repeat_count: 1,
                    shifted_codepoint: None,
                    generated_text: None,
                    tracks_release: true,
                    physical_key_id: None,
                    windows_record: None,
                },
                crate::protocol::ClientPaneInputEvent::Key {
                    code: crate::protocol::ClientKeyCode::Char('x'),
                    modifiers: crossterm::event::KeyModifiers::ALT.bits(),
                    kind: crate::protocol::ClientKeyKind::Press,
                    repeat_count: 1,
                    shifted_codepoint: None,
                    generated_text: None,
                    tracks_release: true,
                    physical_key_id: None,
                    windows_record: None,
                },
                crate::protocol::ClientPaneInputEvent::Mouse {
                    kind: crate::protocol::ClientMouseKind::Down(
                        crate::protocol::ClientMouseButton::Left,
                    ),
                    position: crate::protocol::ClientMousePosition::Cell { column: 2, row: 1 },
                    geometry: None,
                    modifiers: 0,
                    lines: 3,
                },
            ],
        })
    );
    assert_eq!(
        input_rx.try_recv().expect("targeted pane interrupt"),
        Bytes::from_static(&[0x03])
    );
    assert_eq!(
        input_rx.try_recv().expect("targeted pane alt key"),
        Bytes::from_static(b"\x1bx")
    );
    assert_eq!(
        input_rx.try_recv().expect("targeted pane mouse click"),
        Bytes::from_static(b"\x1b[<0;3;2M")
    );
    assert_eq!(server.foreground_client_id, Some(11));
    let pane_id = server.app.session_snapshot().focused_pane_id.unwrap();
    assert!(server.paste_client_clipboard_image_path(
        11,
        crate::protocol::ClientClipboardImageTarget::Pane(pane_id.clone()),
        "/tmp/client-image.png".into(),
    ));
    assert_eq!(
        input_rx.try_recv().expect("targeted clipboard image path"),
        Bytes::from_static(b"/tmp/client-image.png")
    );
    assert!(!server.paste_client_clipboard_image_path(
        11,
        crate::protocol::ClientClipboardImageTarget::DirectTerminal,
        "/tmp/wrong-target.png".into(),
    ));
    assert!(!server.paste_client_clipboard_image_path(
        11,
        crate::protocol::ClientClipboardImageTarget::Popup("missing-popup".into()),
        "/tmp/wrong-target.png".into(),
    ));
    assert!(input_rx.try_recv().is_err());

    let (workspace_index, runtime_pane_id) = server
        .app
        .parse_pane_id(&pane_id)
        .expect("runtime pane target");
    let runtime = server
        .app
        .state
        .runtime_for_pane_in_workspace(
            &server.app.terminal_runtimes,
            workspace_index,
            runtime_pane_id,
        )
        .expect("focused runtime");
    assert_eq!(runtime.current_size(), (24, 79));
    assert!(input_rx.try_recv().is_err(), "legacy release emitted bytes");
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn client_shell_hidden_pane_rejects_presses_but_accepts_releases() {
    let mut server = test_headless_server();
    let mut workspace = crate::workspace::Workspace::test_new("hidden-input");
    let hidden_tab = workspace.test_add_tab(Some("hidden"));
    let hidden_pane = workspace.tabs[hidden_tab].root_pane;
    let (runtime, mut input_rx) =
        crate::terminal::TerminalRuntime::test_with_channel_and_scrollback_bytes(
            80,
            24,
            0,
            b"\x1b[>3u",
            4,
        );
    workspace.insert_test_runtime(hidden_pane, runtime);
    server.app.state.workspaces = vec![workspace];
    server.app.state.active = Some(0);
    server.app.state.selected = 0;
    let pane_id = server.app.public_pane_id(0, hidden_pane).unwrap();
    server.clients.insert(
        11,
        ClientConnection::new(
            (80, 24),
            crate::kitty_graphics::HostCellSize::default(),
            1,
            RenderEncoding::SemanticFrame,
            None,
        ),
    );
    let key = |kind| crate::protocol::ClientPaneInputEvent::Key {
        code: crate::protocol::ClientKeyCode::Char('x'),
        modifiers: 0,
        kind,
        repeat_count: 1,
        shifted_codepoint: None,
        generated_text: None,
        tracks_release: true,
        physical_key_id: Some(0x2d),
        windows_record: None,
    };

    assert!(
        !server.handle_server_event(ServerEvent::ClientShellPaneInput {
            client_id: 11,
            pane_id: pane_id.clone(),
            events: vec![key(crate::protocol::ClientKeyKind::Press)],
        })
    );
    assert!(input_rx.try_recv().is_err());
    assert!(
        !server.handle_server_event(ServerEvent::ClientShellPaneInput {
            client_id: 11,
            pane_id,
            events: vec![key(crate::protocol::ClientKeyKind::Release)],
        })
    );
    assert!(!input_rx.recv().await.expect("encoded release").is_empty());
    assert_eq!(server.foreground_client_id, None);
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn client_shell_streams_and_targets_popup_terminal_content() {
    let mut server = test_headless_server();
    let mut pane_input = install_focused_test_runtime(&mut server, b"base-pane");
    let (popup_runtime, mut popup_input) =
        crate::terminal::TerminalRuntime::test_with_channel_and_scrollback_bytes(
            40,
            12,
            0,
            b"POPUP_SHELL_LIVE\x1b_Ga=T,f=32,t=d,i=9,p=4,s=1,v=1,c=1,r=1,q=2;/wAA/w==\x1b\\",
            4,
        );
    let (_, popup_terminal_id) = server.app.install_test_popup_runtime(popup_runtime);

    let (writer, control_rx, render_rx) = test_client_writer();
    assert!(
        server.handle_server_event(ServerEvent::ClientShellConnected {
            client_id: 12,
            surface_cols: 80,
            surface_rows: 23,
            cell_width_px: 10,
            cell_height_px: 20,
            pixel_mouse: false,
            direct_graphics: false,
            endpoint_keybindings: false,
            mouse_capture: false,
            surface_active: true,
            writer,
        })
    );
    let _snapshot = client_shell_snapshot(read_server_message(
        control_rx.recv().expect("shell snapshot"),
    ));

    server.render_and_stream();
    let ServerMessage::PaneSurface(surface) =
        read_server_message(render_rx.recv().expect("popup surface"))
    else {
        panic!("expected pane surface");
    };
    let popup = surface.popup.as_deref().expect("popup terminal surface");
    assert_eq!(popup.terminal_id, popup_terminal_id.as_str());
    assert!(frame_text(&popup.frame).contains("POPUP_SHELL_LIVE"));
    assert_eq!((popup.frame.width, popup.frame.height), (37, 9));
    assert!(popup.frame.cursor.is_some());
    assert_eq!(surface.graphics.assets.len(), 1);
    assert_eq!(surface.graphics.placements.len(), 1);
    assert!(matches!(
        surface.graphics.placements[0].asset.source,
        crate::protocol::SurfaceGraphicsSource::Terminal {
            target: crate::protocol::SurfaceGraphicsTarget::Popup { .. },
            image_id: 9,
        }
    ));

    assert!(
        !server.handle_server_event(ServerEvent::ClientShellPaneInput {
            client_id: 12,
            pane_id: server.app.session_snapshot().focused_pane_id.unwrap(),
            events: vec![crate::protocol::ClientPaneInputEvent::TextCommit(
                "must-not-leak".into(),
            )],
        })
    );
    assert!(pane_input.try_recv().is_err());

    assert!(server.handle_server_event(ServerEvent::ClientShellResize {
        client_id: 12,
        surface_cols: 60,
        surface_rows: 15,
        cell_width_px: 0,
        cell_height_px: 0,
        pixel_mouse: false,
    }));
    assert_eq!(
        server
            .app
            .terminal_runtimes
            .get(&popup_terminal_id)
            .expect("popup runtime")
            .current_size(),
        (5, 27)
    );

    assert!(
        !server.handle_server_event(ServerEvent::ClientShellPopupInput {
            client_id: 12,
            terminal_id: popup_terminal_id.to_string(),
            events: vec![crate::protocol::ClientPaneInputEvent::TextCommit(
                "typed".into()
            )],
        })
    );
    assert_eq!(
        popup_input.try_recv().expect("popup input"),
        Bytes::from_static(b"typed")
    );
    assert!(server.paste_client_clipboard_image_path(
        12,
        crate::protocol::ClientClipboardImageTarget::Popup(popup_terminal_id.to_string()),
        "/tmp/popup-image.png".into(),
    ));
    assert_eq!(
        popup_input.try_recv().expect("popup clipboard image path"),
        Bytes::from_static(b"/tmp/popup-image.png")
    );
    assert!(!server.paste_client_clipboard_image_path(
        12,
        crate::protocol::ClientClipboardImageTarget::Popup("stale-popup".into()),
        "/tmp/wrong-popup.png".into(),
    ));
    assert!(popup_input.try_recv().is_err());

    assert!(
        !server.handle_server_event(ServerEvent::ClientShellPopupInput {
            client_id: 12,
            terminal_id: "stale-popup".into(),
            events: vec![crate::protocol::ClientPaneInputEvent::TextCommit(
                "wrong".into()
            )],
        })
    );
    assert!(popup_input.try_recv().is_err());

    assert!(server.app.close_popup_pane());
    server.render_and_stream();
    let ServerMessage::PaneSurface(surface) =
        read_server_message(render_rx.recv().expect("popup close surface"))
    else {
        panic!("expected pane surface after popup close");
    };
    assert!(surface.popup.is_none());
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn terminal_popup_is_visible_and_modal_only_on_its_owning_tab() {
    let mut server = test_headless_server();
    let mut workspace = crate::workspace::Workspace::test_new("tab-popup");
    let first_pane = workspace.tabs[0].root_pane;
    let second_tab = workspace.test_add_tab(Some("second"));
    let second_pane = workspace.tabs[second_tab].root_pane;
    workspace.insert_test_runtime(
        first_pane,
        crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 23, b"FIRST"),
    );
    let (second_runtime, mut second_input) =
        crate::terminal::TerminalRuntime::test_with_channel_and_scrollback_bytes(
            80, 23, 0, b"SECOND", 4,
        );
    workspace.insert_test_runtime(second_pane, second_runtime);
    server.app.state.workspaces = vec![workspace];
    server.app.state.active = Some(0);
    server.app.state.selected = 0;
    server.app.state.mode = crate::app::Mode::Terminal;
    let first_tab_id = server.app.public_tab_id(0, 0).unwrap();
    let second_tab_id = server.app.public_tab_id(0, second_tab).unwrap();
    let second_pane_id = server.app.public_pane_id(0, second_pane).unwrap();

    let (first_control, first_render) = connect_matching_test_shell(&mut server, 31);
    let (second_control, second_render) = connect_matching_test_shell(&mut server, 32);
    let _ = first_control.recv().expect("first snapshot");
    let _ = second_control.recv().expect("second snapshot");
    assert!(server.focus_shell_client_on_tab(32, &second_tab_id));
    assert!(server.claim_shell_tab_geometry(32, false));

    let (popup_runtime, mut popup_input) =
        crate::terminal::TerminalRuntime::test_with_channel_and_scrollback_bytes(
            40,
            12,
            0,
            b"POPUP\x1b[>3u",
            4,
        );
    let (_, popup_terminal_id) = server.app.install_test_popup_runtime(popup_runtime);
    server.popup_owner_tab_id = Some(first_tab_id);
    assert!(server.apply_shell_tab_geometry(31, false));
    let popup_size = server
        .app
        .terminal_runtimes
        .get(&popup_terminal_id)
        .unwrap()
        .current_size();
    connect_pending_terminal_client(&mut server, 33);
    assert!(
        server.handle_server_event(ServerEvent::ClientAttachTerminal {
            client_id: 33,
            terminal_id: popup_terminal_id.to_string(),
            takeover: false,
        })
    );
    assert_ne!(
        server
            .app
            .terminal_runtimes
            .get(&popup_terminal_id)
            .unwrap()
            .current_size(),
        popup_size
    );
    assert!(server.handle_server_event(ServerEvent::ClientDisconnected { client_id: 33 }));
    assert_eq!(
        server
            .app
            .terminal_runtimes
            .get(&popup_terminal_id)
            .unwrap()
            .current_size(),
        popup_size
    );
    server.render_and_stream();
    assert!(recv_pane_surface(&first_render, "popup owner surface")
        .popup
        .is_some());
    assert!(recv_pane_surface(&second_render, "other tab surface")
        .popup
        .is_none());

    server.handle_server_event(ServerEvent::ClientShellPaneInput {
        client_id: 32,
        pane_id: second_pane_id,
        events: vec![crate::protocol::ClientPaneInputEvent::TextCommit(
            "typed".into(),
        )],
    });
    assert_eq!(
        second_input.try_recv().expect("other tab input"),
        Bytes::from_static(b"typed")
    );

    let popup_key = |kind| crate::protocol::ClientPaneInputEvent::Key {
        code: crate::protocol::ClientKeyCode::Char('x'),
        modifiers: 0,
        kind,
        repeat_count: 1,
        shifted_codepoint: None,
        generated_text: (kind == crate::protocol::ClientKeyKind::Press).then(|| "x".into()),
        tracks_release: true,
        physical_key_id: Some(0x2d),
        windows_record: None,
    };
    server.handle_server_event(ServerEvent::ClientShellPopupInput {
        client_id: 31,
        terminal_id: popup_terminal_id.to_string(),
        events: vec![popup_key(crate::protocol::ClientKeyKind::Press)],
    });
    assert!(
        !tokio::time::timeout(Duration::from_secs(1), popup_input.recv())
            .await
            .expect("popup press timed out")
            .expect("popup press")
            .is_empty()
    );
    assert!(server.focus_shell_client_on_tab(31, &second_tab_id));
    server.handle_server_event(ServerEvent::ClientShellPopupInput {
        client_id: 31,
        terminal_id: popup_terminal_id.to_string(),
        events: vec![popup_key(crate::protocol::ClientKeyKind::Release)],
    });
    assert!(
        !tokio::time::timeout(Duration::from_secs(1), popup_input.recv())
            .await
            .expect("popup release timed out")
            .expect("popup release after navigation")
            .is_empty()
    );

    assert!(
        !server.handle_server_event(ServerEvent::ClientShellPopupInput {
            client_id: 32,
            terminal_id: popup_terminal_id.to_string(),
            events: vec![crate::protocol::ClientPaneInputEvent::TextCommit(
                "wrong".into()
            )],
        })
    );
    assert!(popup_input.try_recv().is_err());

    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn client_shell_release_under_popup_renders_when_it_resets_scrollback() {
    let mut server = test_headless_server();
    let mut workspace = crate::workspace::Workspace::test_new("popup-release-scroll");
    let pane_id = workspace.tabs[0].root_pane;
    let (runtime, mut input_rx) =
        crate::terminal::TerminalRuntime::test_with_channel_and_scrollback_bytes(
            80,
            2,
            10_000,
            b"one\r\ntwo\r\nthree\r\n\x1b[>3u",
            4,
        );
    runtime.scroll_up(1);
    assert!(runtime
        .scroll_metrics()
        .is_some_and(|metrics| metrics.offset_from_bottom > 0));
    workspace.insert_test_runtime(pane_id, runtime);
    server.app.state.workspaces = vec![workspace];
    server.app.state.active = Some(0);
    server.app.state.selected = 0;
    let public_pane_id = server.app.public_pane_id(0, pane_id).unwrap();
    let popup_runtime = crate::terminal::TerminalRuntime::test_with_screen_bytes(20, 5, b"");
    server.app.install_test_popup_runtime(popup_runtime);
    server.clients.insert(
        11,
        ClientConnection::new_with_mode(
            ClientConnectionMode::ClientShell,
            (80, 24),
            crate::kitty_graphics::HostCellSize::default(),
            1,
            RenderEncoding::SemanticFrame,
            None,
        ),
    );
    server.foreground_client_id = Some(11);

    let render_impact =
        server.handle_server_event_with_render_impact(ServerEvent::ClientShellPaneInput {
            client_id: 11,
            pane_id: public_pane_id,
            events: vec![crate::protocol::ClientPaneInputEvent::Key {
                code: crate::protocol::ClientKeyCode::Char('x'),
                modifiers: 0,
                kind: crate::protocol::ClientKeyKind::Release,
                repeat_count: 1,
                shifted_codepoint: None,
                generated_text: None,
                tracks_release: true,
                physical_key_id: Some(0x2d),
                windows_record: None,
            }],
        });

    assert_eq!(render_impact, RenderImpact::Full);
    assert!(!input_rx.recv().await.expect("encoded release").is_empty());
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn client_shell_text_input_renders_only_when_resetting_scrollback() {
    let mut server = test_headless_server();
    let mut workspace = crate::workspace::Workspace::test_new("scrolled-input");
    let pane_id = workspace.tabs[0].root_pane;
    let (runtime, mut input_rx) =
        crate::terminal::TerminalRuntime::test_with_channel_and_scrollback_bytes(
            80,
            2,
            10_000,
            b"one\r\ntwo\r\nthree\r\n",
            4,
        );
    runtime.scroll_up(1);
    assert!(runtime
        .scroll_metrics()
        .is_some_and(|metrics| metrics.offset_from_bottom > 0));
    workspace.insert_test_runtime(pane_id, runtime);
    server.app.state.workspaces = vec![workspace];
    server.app.state.active = Some(0);
    server.app.state.selected = 0;
    let public_pane_id = server.app.public_pane_id(0, pane_id).unwrap();
    server.clients.insert(
        11,
        ClientConnection::new_with_mode(
            ClientConnectionMode::ClientShell,
            (80, 24),
            crate::kitty_graphics::HostCellSize::default(),
            1,
            RenderEncoding::SemanticFrame,
            None,
        ),
    );
    server.foreground_client_id = Some(11);

    let render_impact =
        server.handle_server_event_with_render_impact(ServerEvent::ClientShellPaneInput {
            client_id: 11,
            pane_id: public_pane_id.clone(),
            events: vec![crate::protocol::ClientPaneInputEvent::TextCommit(
                "x".to_owned(),
            )],
        });

    assert_eq!(render_impact, RenderImpact::Full);
    assert_eq!(
        input_rx.try_recv().expect("text must reach the PTY"),
        Bytes::from_static(b"x")
    );
    assert_eq!(
        server
            .app
            .state
            .runtime_for_pane_in_workspace(&server.app.terminal_runtimes, 0, pane_id)
            .and_then(|runtime| runtime.scroll_metrics())
            .map(|metrics| metrics.offset_from_bottom),
        Some(0)
    );

    let render_impact =
        server.handle_server_event_with_render_impact(ServerEvent::ClientShellPaneInput {
            client_id: 11,
            pane_id: public_pane_id,
            events: vec![crate::protocol::ClientPaneInputEvent::TextCommit(
                "y".to_owned(),
            )],
        });
    assert_eq!(render_impact, RenderImpact::None);
    assert_eq!(
        input_rx.try_recv().expect("second text must reach the PTY"),
        Bytes::from_static(b"y")
    );
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn client_shell_mouse_motion_delivers_without_render_when_foreground() {
    let mut server = test_headless_server();
    let mut input_rx = install_focused_test_runtime(&mut server, b"\x1b[?1003h\x1b[?1006h");
    let pane_id = server.app.session_snapshot().focused_pane_id.unwrap();
    server.clients.insert(
        11,
        ClientConnection::new_with_mode(
            ClientConnectionMode::ClientShell,
            (80, 24),
            crate::kitty_graphics::HostCellSize::default(),
            1,
            RenderEncoding::SemanticFrame,
            None,
        ),
    );
    server.foreground_client_id = Some(11);
    assert!(server.claim_unowned_shell_tab_geometry(11, false));

    let render_impact =
        server.handle_server_event_with_render_impact(ServerEvent::ClientShellPaneInput {
            client_id: 11,
            pane_id,
            events: vec![crate::protocol::ClientPaneInputEvent::Mouse {
                kind: crate::protocol::ClientMouseKind::Moved,
                position: crate::protocol::ClientMousePosition::Cell { column: 2, row: 1 },
                geometry: None,
                modifiers: 0,
                lines: 0,
            }],
        });

    assert_eq!(render_impact, RenderImpact::None);
    assert!(
        input_rx.try_recv().is_ok(),
        "motion must still reach the PTY"
    );
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn client_shell_mouse_motion_promotes_and_requests_render() {
    let mut server = test_headless_server();
    let mut input_rx = install_focused_test_runtime(&mut server, b"\x1b[?1003h\x1b[?1006h");
    let pane_id = server.app.session_snapshot().focused_pane_id.unwrap();
    server.clients.insert(
        11,
        ClientConnection::new_with_mode(
            ClientConnectionMode::ClientShell,
            (80, 24),
            crate::kitty_graphics::HostCellSize::default(),
            1,
            RenderEncoding::SemanticFrame,
            None,
        ),
    );

    let render_impact =
        server.handle_server_event_with_render_impact(ServerEvent::ClientShellPaneInput {
            client_id: 11,
            pane_id,
            events: vec![crate::protocol::ClientPaneInputEvent::Mouse {
                kind: crate::protocol::ClientMouseKind::Moved,
                position: crate::protocol::ClientMousePosition::Cell { column: 2, row: 1 },
                geometry: None,
                modifiers: 0,
                lines: 0,
            }],
        });

    assert_eq!(render_impact, RenderImpact::Full);
    assert_eq!(server.foreground_client_id, Some(11));
    assert!(
        input_rx.try_recv().is_ok(),
        "motion must still reach the PTY"
    );
    shutdown_test_runtimes(&mut server);
}

fn retained_test_server(
    initial_screen: &[u8],
) -> (
    HeadlessServer,
    std::sync::mpsc::Receiver<Vec<u8>>,
    crate::layout::PaneId,
) {
    let (server, _control_rx, render_rx, pane_id) =
        retained_test_server_with_control(initial_screen);
    (server, render_rx, pane_id)
}

fn retained_test_server_with_control(
    initial_screen: &[u8],
) -> (
    HeadlessServer,
    std::sync::mpsc::Receiver<Vec<u8>>,
    std::sync::mpsc::Receiver<Vec<u8>>,
    crate::layout::PaneId,
) {
    let mut server = test_headless_server();
    let mut workspace = crate::workspace::Workspace::test_new("test");
    let pane_id = workspace.focused_pane_id().expect("focused pane");
    workspace.insert_test_runtime(
        pane_id,
        crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 24, initial_screen),
    );
    server.app.state.workspaces = vec![workspace];
    server.app.state.active = Some(0);
    server.app.state.selected = 0;
    server.app.state.mode = crate::app::Mode::Terminal;

    let (client_tx, client_control_rx, client_rx) = test_client_writer();
    server.clients.insert(
        1,
        ClientConnection::app(
            (80, 24),
            crate::kitty_graphics::HostCellSize::default(),
            crate::terminal_theme::TerminalTheme::default(),
            None,
            1,
            RenderEncoding::SemanticFrame,
            Some(client_tx),
        ),
    );
    server.foreground_client_id = Some(1);
    server.sync_foreground_client_state();
    assert!(server.claim_unowned_shell_tab_geometry(1, true));

    (server, client_control_rx, client_rx, pane_id)
}

/// `retained_test_server`, but the client hangs off the REAL writer queue
/// (single-slot render lane, Full instead of blocking) rather than the
/// `test_render` bypass — the H49-1 requirement: the queue is driven, not
/// simulated (H49-1).
fn retained_test_server_through_queue(
    initial_screen: &[u8],
) -> (
    HeadlessServer,
    crate::server::client_transport::TestQueueDrain,
    crate::layout::PaneId,
) {
    let mut server = test_headless_server();
    let mut workspace = crate::workspace::Workspace::test_new("test");
    let pane_id = workspace.focused_pane_id().expect("focused pane");
    workspace.insert_test_runtime(
        pane_id,
        crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 24, initial_screen),
    );
    server.app.state.workspaces = vec![workspace];
    server.app.state.active = Some(0);
    server.app.state.selected = 0;
    server.app.state.mode = crate::app::Mode::Terminal;

    let (client_tx, drain) = ClientWriter::test_channel_through_queue();
    server.clients.insert(
        1,
        ClientConnection::app(
            (80, 24),
            crate::kitty_graphics::HostCellSize::default(),
            crate::terminal_theme::TerminalTheme::default(),
            None,
            1,
            RenderEncoding::SemanticFrame,
            Some(client_tx),
        ),
    );
    server.foreground_client_id = Some(1);
    server.sync_foreground_client_state();
    server.resize_shared_runtime_to_effective_size();

    (server, drain, pane_id)
}

/// `retained_test_server`, plus the control-lane receiver — the resize
/// sweep travels as a cleanup on the control lane, and a test that only
/// listens to the render lane calls a working sweep silent.
fn resize_test_server(
    initial_screen: &[u8],
) -> (
    HeadlessServer,
    std::sync::mpsc::Receiver<Vec<u8>>,
    std::sync::mpsc::Receiver<Vec<u8>>,
    crate::layout::PaneId,
) {
    let mut server = test_headless_server();
    let mut workspace = crate::workspace::Workspace::test_new("test");
    let pane_id = workspace.focused_pane_id().expect("focused pane");
    workspace.insert_test_runtime(
        pane_id,
        crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 24, initial_screen),
    );
    server.app.state.workspaces = vec![workspace];
    server.app.state.active = Some(0);
    server.app.state.selected = 0;
    server.app.state.mode = crate::app::Mode::Terminal;

    let (client_tx, control_rx, client_rx) = test_client_writer();
    server.clients.insert(
        1,
        ClientConnection::app(
            (80, 24),
            crate::kitty_graphics::HostCellSize::default(),
            crate::terminal_theme::TerminalTheme::default(),
            None,
            1,
            RenderEncoding::SemanticFrame,
            Some(client_tx),
        ),
    );
    server.foreground_client_id = Some(1);
    server.sync_foreground_client_state();
    server.resize_shared_runtime_to_effective_size();

    (server, control_rx, client_rx, pane_id)
}

/// The test `ClientWriter` forwards every lane through a spawned drain
/// thread (`ClientWriter::test_channel`), so a message handed to the
/// writer is only *eventually* visible on these receivers. Wire asserts
/// must wait for the forwarder — the `next_window_title` convention — or
/// they lose that race on a saturated machine: a 5ms delay injected into
/// the forwarder fails a bare `try_recv` drain on every single run.
fn collect_wire_messages(
    receivers: [&std::sync::mpsc::Receiver<Vec<u8>>; 2],
    window: Duration,
    done: impl Fn(&[ServerMessage]) -> bool,
) -> Vec<ServerMessage> {
    let deadline = Instant::now() + window;
    let mut out = Vec::new();
    loop {
        for rx in receivers {
            while let Ok(bytes) = rx.try_recv() {
                out.push(read_server_message(bytes));
            }
        }
        if done(&out) || Instant::now() >= deadline {
            return out;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// A delete-all reaches the wire on either lane within the deadline.
fn delete_all_arrives(
    control_rx: &std::sync::mpsc::Receiver<Vec<u8>>,
    render_rx: &std::sync::mpsc::Receiver<Vec<u8>>,
) -> bool {
    any_delete_all(&collect_wire_messages(
        [control_rx, render_rx],
        Duration::from_secs(5),
        any_delete_all,
    ))
}

/// Exact quiescence for an absence assert: a sentinel sent down the
/// control lane after the work under test must come out the far end
/// before silence is judged. The forwarder is a single thread and a lane
/// is FIFO, so the sentinel's arrival proves every control message
/// enqueued before it was delivered — no sleep, no bet. These absence
/// tests trigger no render, so the control lane is the only road a
/// delete could take; the render receiver is still scanned for
/// completeness, and a sentinel that never returns fails loudly instead
/// of passing quietly.
fn no_delete_all_arrives(
    writer: &ClientWriter,
    control_rx: &std::sync::mpsc::Receiver<Vec<u8>>,
    render_rx: &std::sync::mpsc::Receiver<Vec<u8>>,
) -> bool {
    const SENTINEL: &str = "__drain_sentinel__";
    let framed = HeadlessServer::frame_server_message(&ServerMessage::WindowTitle {
        title: Some(SENTINEL.to_owned()),
    })
    .expect("frame the drain sentinel");
    let _ = writer.control.send(framed);
    let is_sentinel = |message: &ServerMessage| matches!(message, ServerMessage::WindowTitle { title: Some(t) } if t == SENTINEL);
    let messages = collect_wire_messages(
        [control_rx, render_rx],
        Duration::from_secs(5),
        |messages| messages.iter().any(is_sentinel),
    );
    assert!(
        messages.iter().any(is_sentinel),
        "the drain sentinel must come back — is the test writer's forwarder dead?"
    );
    !any_delete_all(&messages)
}

fn wire_graphics_bytes(messages: &[ServerMessage]) -> Vec<u8> {
    messages
        .iter()
        .flat_map(|message| match message {
            ServerMessage::Frame(frame) => frame.graphics.clone(),
            ServerMessage::Graphics { bytes } => bytes.clone(),
            _ => Vec::new(),
        })
        .collect()
}

/// Graphics bytes from both lanes, collected until one of `patterns`
/// appears or the deadline passes.
fn graphics_bytes_arrive(
    control_rx: &std::sync::mpsc::Receiver<Vec<u8>>,
    render_rx: &std::sync::mpsc::Receiver<Vec<u8>>,
    patterns: &[&[u8]],
) -> Vec<u8> {
    let messages = collect_wire_messages(
        [control_rx, render_rx],
        Duration::from_secs(5),
        |messages| {
            let bytes = wire_graphics_bytes(messages);
            patterns
                .iter()
                .any(|pattern| bytes.windows(pattern.len()).any(|w| w == *pattern))
        },
    );
    wire_graphics_bytes(&messages)
}

fn any_delete_all(messages: &[ServerMessage]) -> bool {
    messages.iter().any(|message| match message {
        ServerMessage::Graphics { bytes } => bytes.windows(3).any(|w| w == b"a=d"),
        ServerMessage::Frame(frame) => frame.graphics.windows(3).any(|w| w == b"a=d"),
        _ => false,
    })
}

// TP-GFX-RESIZE-01: a resize breaks every placement's geometry — the
// client's graphics slate is wiped (a=d reaches the terminal) so the next
// paint rebuilds clean instead of leaving ghosts at the old coordinates.
#[tokio::test]
async fn a_client_resize_sweeps_the_stale_graphics_off_the_terminal() {
    let (mut server, control_rx, client_rx, _pane_id) = resize_test_server(b"aaaa");
    server.app.state.kitty_graphics_enabled = true;
    {
        let client = server.clients.get_mut(&1).unwrap();
        client.cell_size = crate::kitty_graphics::HostCellSize {
            width_px: 10,
            height_px: 20,
        };
        client.graphics_cache.test_mark_non_empty();
    }

    assert!(server.handle_server_event(ServerEvent::ClientResize {
        client_id: 1,
        cols: 100,
        rows: 30,
        cell_width_px: 10,
        cell_height_px: 20,
        pixel_mouse: false,
    }));

    assert!(
        server.clients.get(&1).unwrap().graphics_cache.is_empty(),
        "the slate is wiped with the geometry that placed it"
    );
    assert!(
        delete_all_arrives(&control_rx, &client_rx),
        "the delete reaches the terminal, not only the ledger"
    );
}

// TP-GFX-RESIZE-01: a cell-size change (retina to external display)
// invalidates every pixel computation the placements were made with.
#[tokio::test]
async fn a_cell_size_change_sweeps_the_graphics_too() {
    let (mut server, control_rx, client_rx, _pane_id) = resize_test_server(b"aaaa");
    server.app.state.kitty_graphics_enabled = true;
    {
        let client = server.clients.get_mut(&1).unwrap();
        client.cell_size = crate::kitty_graphics::HostCellSize {
            width_px: 10,
            height_px: 20,
        };
        client.graphics_cache.test_mark_non_empty();
    }

    assert!(server.handle_server_event(ServerEvent::ClientResize {
        client_id: 1,
        cols: 80,
        rows: 24,
        cell_width_px: 16,
        cell_height_px: 32,
        pixel_mouse: false,
    }));

    assert!(server.clients.get(&1).unwrap().graphics_cache.is_empty());
    assert!(delete_all_arrives(&control_rx, &client_rx));
}

// TP-GFX-RESIZE-01: a resize that changes nothing moves no bytes — the
// slate stays, the wire stays quiet (bytes follow change, not events).
#[tokio::test]
async fn a_no_op_resize_leaves_the_graphics_alone() {
    let (mut server, control_rx, client_rx, _pane_id) = resize_test_server(b"aaaa");
    server.app.state.kitty_graphics_enabled = true;
    {
        let client = server.clients.get_mut(&1).unwrap();
        client.cell_size = crate::kitty_graphics::HostCellSize {
            width_px: 10,
            height_px: 20,
        };
        client.graphics_cache.test_mark_non_empty();
    }

    server.handle_server_event(ServerEvent::ClientResize {
        client_id: 1,
        cols: 80,
        rows: 24,
        cell_width_px: 10,
        cell_height_px: 20,
        pixel_mouse: false,
    });

    assert!(
        !server.clients.get(&1).unwrap().graphics_cache.is_empty(),
        "an unchanged geometry keeps its slate"
    );
    let writer = server
        .clients
        .get(&1)
        .unwrap()
        .writer
        .clone()
        .expect("test client writer");
    assert!(
        no_delete_all_arrives(&writer, &control_rx, &client_rx),
        "no change, no bytes"
    );
}

// TP-GFX-RESIZE-01: the sweep is a scalpel — the pane-layer stream's
// entries survive a resize untouched, because that road replays its
// placements without retransmitting and a delete here would force the
// full upload the road exists to avoid.
#[tokio::test]
async fn a_resize_leaves_the_pane_layer_stream_alone() {
    let (mut server, control_rx, client_rx, _pane_id) = resize_test_server(b"aaaa");
    server.app.state.kitty_graphics_enabled = true;
    {
        let client = server.clients.get_mut(&1).unwrap();
        client.cell_size = crate::kitty_graphics::HostCellSize {
            width_px: 10,
            height_px: 20,
        };
        client.graphics_cache.test_mark_pane_layer_entry();
    }

    server.handle_server_event(ServerEvent::ClientResize {
        client_id: 1,
        cols: 100,
        rows: 30,
        cell_width_px: 10,
        cell_height_px: 20,
        pixel_mouse: false,
    });

    assert!(
        !server.clients.get(&1).unwrap().graphics_cache.is_empty(),
        "the stream entry keeps its seat"
    );
    let writer = server
        .clients
        .get(&1)
        .unwrap()
        .writer
        .clone()
        .expect("test client writer");
    assert!(
        no_delete_all_arrives(&writer, &control_rx, &client_rx),
        "no delete travels for a slate the replay road owns"
    );
}

// TP-GFX-RESIZE-01: a full redraw re-seats the terminal-native picture
// a PTY painted (the browser pane) from the terminal's own state — the
// road the divider-release repaint rides so the pane does not sit blank
// until a tab switch forces the same full render.
#[tokio::test]
async fn a_full_redraw_reseats_the_terminal_native_picture() {
    let (mut server, control_rx, client_rx, _pane_id) =
        resize_test_server(b"\x1b_Ga=T,f=32,t=d,i=7,s=1,v=1,q=2;/wAA/w==\x1b\\");
    server.app.state.kitty_graphics_enabled = true;
    server.clients.get_mut(&1).unwrap().cell_size = crate::kitty_graphics::HostCellSize {
        width_px: 10,
        height_px: 20,
    };

    server.render_and_stream();
    let first = graphics_bytes_arrive(
        &control_rx,
        &client_rx,
        &[b"a=T,".as_slice(), b"a=t,".as_slice()],
    );
    assert!(
        first.windows(4).any(|w| w == b"a=T,") || first.windows(4).any(|w| w == b"a=t,"),
        "the first full render carries the PTY's picture"
    );

    server.app.full_redraw_pending = true;
    server.render_and_stream();
    let second = graphics_bytes_arrive(
        &control_rx,
        &client_rx,
        &[b"a=p,".as_slice(), b"a=T,".as_slice(), b"a=t,".as_slice()],
    );
    assert!(
        second.windows(4).any(|w| w == b"a=p,")
            || second.windows(4).any(|w| w == b"a=T,")
            || second.windows(4).any(|w| w == b"a=t,"),
        "a later full render re-seats the picture instead of leaving the pane blank"
    );
}

// TP-GFX-REPLAY-01: the re-seat decision — every real full cause
// re-seats (and a real cause in the same turn as a text refresh wins);
// only a frame whose sole cause is a scheduled text refresh keeps the
// placements seated.
#[test]
fn full_render_reseat_policy_lets_only_a_text_refresh_skip() {
    assert!(full_render_reseat_policy(true, false));
    assert!(full_render_reseat_policy(true, true));
    assert!(!full_render_reseat_policy(false, true));
    assert!(full_render_reseat_policy(false, false));
}

// TP-GFX-REPLAY-01: the sampler and the clock signal through the
// text-refresh channel, not the full-impact return — the frame they ask
// for must not re-seat placements, while every other scheduled member
// still claims the full-impact return.
#[test]
fn scheduled_status_text_signals_text_refresh_not_full() {
    let mut server = test_headless_server();
    // A bar-less config: `clock_tick()` is None, so a lingering reading
    // is dropped — the clock's cheapest visible change.
    server.app.state.clock_now = Some(time::OffsetDateTime::now_utc());
    let full = server.handle_scheduled_tasks_headless(Instant::now(), false);
    assert!(!full, "status text must not claim the full-impact return");
    assert!(
        server.scheduled_text_refresh_pending,
        "status text signals through the text-refresh channel"
    );

    // A full-impact member still claims the return: an expired toast.
    server.scheduled_text_refresh_pending = false;
    let now = Instant::now();
    server.app.toast_deadline = Some(now);
    assert!(
        server.handle_scheduled_tasks_headless(now + Duration::from_millis(1), false),
        "a full-impact member keeps the full-impact return"
    );
}

// TP-GFX-REPLAY-01: a frame whose only cause is refreshed status text
// keeps the picture seated — zero graphics bytes on the wire — while
// still delivering the changed cells (a frame that renders nothing
// cannot pass), and the seat itself survives for the next real full
// render to re-seat.
#[tokio::test]
async fn a_text_refresh_only_frame_moves_no_graphics_bytes() {
    let (mut server, control_rx, client_rx, pane_id) =
        resize_test_server(b"\x1b_Ga=T,f=32,t=d,i=7,s=1,v=1,q=2;/wAA/w==\x1b\\");
    server.app.state.kitty_graphics_enabled = true;
    server.clients.get_mut(&1).unwrap().cell_size = crate::kitty_graphics::HostCellSize {
        width_px: 10,
        height_px: 20,
    };

    server.render_and_stream();
    let first = graphics_bytes_arrive(
        &control_rx,
        &client_rx,
        &[b"a=T,".as_slice(), b"a=t,".as_slice()],
    );
    assert!(
        first.windows(4).any(|w| w == b"a=T,") || first.windows(4).any(|w| w == b"a=t,"),
        "the baseline full render carries the PTY's picture"
    );

    // The same picture plus one changed text cell: the status-text shape.
    server.app.state.workspaces[0].insert_test_runtime(
        pane_id,
        crate::terminal::TerminalRuntime::test_with_screen_bytes(
            80,
            24,
            b"\x1b_Ga=T,f=32,t=d,i=7,s=1,v=1,q=2;/wAA/w==\x1b\\Z",
        ),
    );

    server.full_render_reseats_placements = false;
    server.render_and_stream();
    let messages = collect_wire_messages([&control_rx, &client_rx], Duration::from_secs(5), |m| {
        m.iter().any(
            |msg| matches!(msg, ServerMessage::Frame(f) if f.cells.iter().any(|c| c.symbol == "Z")),
        )
    });
    assert!(
        messages.iter().any(|msg| {
            matches!(msg, ServerMessage::Frame(f) if f.cells.iter().any(|c| c.symbol == "Z"))
        }),
        "the changed status cell must reach the wire — a frame that renders nothing may not pass"
    );
    let graphics = wire_graphics_bytes(&messages);
    for pattern in [
        b"a=T,".as_slice(),
        b"a=t,".as_slice(),
        b"a=p,".as_slice(),
        b"a=d".as_slice(),
    ] {
        assert!(
            !graphics.windows(pattern.len()).any(|w| w == pattern),
            "a text-refresh-only frame must move no graphics bytes (found {:?})",
            String::from_utf8_lossy(pattern)
        );
    }
    // Quiescence for the absence half, and no delete swept the seat away.
    let writer = server.clients.get(&1).unwrap().writer.clone().unwrap();
    assert!(no_delete_all_arrives(&writer, &control_rx, &client_rx));

    // The exemption is per frame, not a lost capability: a later real
    // full render still re-seats the picture.
    server.app.full_redraw_pending = true;
    server.render_and_stream();
    let reseated = graphics_bytes_arrive(
        &control_rx,
        &client_rx,
        &[b"a=p,".as_slice(), b"a=T,".as_slice(), b"a=t,".as_slice()],
    );
    assert!(
        reseated.windows(4).any(|w| w == b"a=p,")
            || reseated.windows(4).any(|w| w == b"a=T,")
            || reseated.windows(4).any(|w| w == b"a=t,"),
        "a later full render re-seats the picture — the exemption must not outlive its frame"
    );
}

fn hidden_pty_visibility_test_server(
    client_sizes: &[(u16, u16)],
) -> (HeadlessServer, crate::layout::PaneId) {
    let mut server = test_headless_server();
    let mut workspace = crate::workspace::Workspace::test_new("test");
    let background_tab = workspace.test_add_tab(Some("background"));
    let background_pane = workspace.tabs[background_tab].root_pane;
    server.app.state.workspaces = vec![workspace];
    server.app.state.active = Some(0);
    server.app.state.selected = 0;
    server.app.state.mode = crate::app::Mode::Terminal;

    for (index, &terminal_size) in client_sizes.iter().enumerate() {
        let client_id = index as u64 + 1;
        let (client_tx, _client_control_rx, _client_rx) = test_client_writer();
        server.clients.insert(
            client_id,
            ClientConnection::app(
                terminal_size,
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                None,
                client_id,
                RenderEncoding::SemanticFrame,
                Some(client_tx),
            ),
        );
    }

    (server, background_pane)
}

fn assert_frame_data_eq(actual: &FrameData, expected: &FrameData) {
    assert_eq!(
        (actual.width, actual.height),
        (expected.width, expected.height)
    );
    assert_eq!(actual.cursor, expected.cursor, "cursor mismatch");
    assert_eq!(actual.hyperlinks, expected.hyperlinks, "hyperlink mismatch");
    assert_eq!(actual.graphics, expected.graphics, "graphics mismatch");
    assert_eq!(
        actual.cells.len(),
        expected.cells.len(),
        "cell length mismatch"
    );
    for (idx, (actual_cell, expected_cell)) in
        actual.cells.iter().zip(expected.cells.iter()).enumerate()
    {
        if cells_equivalent_for_frame_compare(
            &actual.cells,
            &expected.cells,
            usize::from(actual.width),
            idx,
            actual_cell,
            expected_cell,
        ) {
            continue;
        }
        assert_eq!(
            actual_cell,
            expected_cell,
            "cell mismatch at index {idx} (x={}, y={})",
            idx % usize::from(actual.width),
            idx / usize::from(actual.width),
        );
    }
}

fn cells_equivalent_for_frame_compare(
    actual_cells: &[CellData],
    expected_cells: &[CellData],
    width: usize,
    idx: usize,
    actual: &CellData,
    expected: &CellData,
) -> bool {
    if actual == expected {
        return true;
    }
    if !cell_style_without_symbol_eq(actual, expected) {
        return false;
    }
    if !matches!(
        (actual.symbol.as_str(), expected.symbol.as_str()),
        ("", " ") | (" ", "")
    ) {
        return false;
    }
    covered_by_previous_wide_cell(actual_cells, width, idx)
        || covered_by_previous_wide_cell(expected_cells, width, idx)
}

fn cell_style_without_symbol_eq(a: &CellData, b: &CellData) -> bool {
    a.fg == b.fg
        && a.bg == b.bg
        && a.modifier == b.modifier
        && a.skip == b.skip
        && a.hyperlink == b.hyperlink
}

fn covered_by_previous_wide_cell(cells: &[CellData], width: usize, idx: usize) -> bool {
    if idx == 0 || idx.is_multiple_of(width) {
        return false;
    }
    frame_cell_display_width(&cells[idx - 1]) > 1
}

fn frame_cell_display_width(cell: &CellData) -> usize {
    if is_halfwidth_katakana_voiced_grapheme(&cell.symbol) {
        return 2;
    }
    cell.symbol.width()
}

fn is_halfwidth_katakana_voiced_grapheme(symbol: &str) -> bool {
    let mut chars = symbol.chars();
    let Some(base) = chars.next() else {
        return false;
    };
    let Some(mark) = chars.next() else {
        return false;
    };
    chars.next().is_none()
        && ('\u{ff66}'..='\u{ff9d}').contains(&base)
        && matches!(mark, '\u{ff9e}' | '\u{ff9f}')
}

// TP-GFX-MULTI-01
#[test]
fn direct_graphics_requires_one_negotiated_app_client() {
    let mut server = test_headless_server();
    let (writer_a, _control_a, _render_a) = test_client_writer();
    assert!(server.handle_server_event(ServerEvent::ClientConnected {
        client_id: 1,
        cols: 80,
        rows: 24,
        cell_width_px: 10,
        cell_height_px: 20,
        render_encoding: RenderEncoding::SemanticFrame,
        keybindings: None,
        direct_attach_requested: false,
        direct_graphics: true,
        media_listen: false,
        pixel_mouse: true,
        writer: writer_a,
        capabilities: Vec::new(),
    }));
    assert!(server.clients[&1].direct_graphics);
    assert!(server.clients[&1].pixel_mouse);
    assert!(server.direct_graphics_available());

    let (writer_b, _control_b, _render_b) = test_client_writer();
    assert!(server.handle_server_event(ServerEvent::ClientConnected {
        client_id: 2,
        cols: 80,
        rows: 24,
        cell_width_px: 10,
        cell_height_px: 20,
        render_encoding: RenderEncoding::SemanticFrame,
        keybindings: None,
        direct_attach_requested: false,
        direct_graphics: false,
        media_listen: false,
        pixel_mouse: false,
        writer: writer_b,
        capabilities: Vec::new(),
    }));
    assert!(!server.direct_graphics_available());
}

#[test]
fn foreground_client_applies_client_keybindings() {
    let mut server = test_headless_server();
    let local_config: crate::config::Config = toml::from_str(
        r#"
[keys]
prefix = "ctrl+a"
new_tab = "prefix+t"
"#,
    )
    .unwrap();
    let local_keybindings = local_config.live_keybinds().unwrap();
    let (writer_a, _control_a, _render_a) = test_client_writer();
    let (writer_b, _control_b, _render_b) = test_client_writer();

    assert!(server.handle_server_event(ServerEvent::ClientConnected {
        client_id: 1,
        cols: 80,
        rows: 24,
        cell_width_px: 0,
        cell_height_px: 0,
        render_encoding: RenderEncoding::SemanticFrame,
        keybindings: Some(Box::new(local_keybindings)),
        direct_attach_requested: false,
        direct_graphics: false,
        media_listen: false,
        pixel_mouse: false,
        writer: writer_a,
        capabilities: Vec::new(),
    }));
    assert_eq!(
        server.app.state.prefix_code,
        crossterm::event::KeyCode::Char('a')
    );
    assert!(server
        .app
        .state
        .keybinds
        .new_tab
        .bindings
        .iter()
        .any(|binding| binding.label == "prefix+t"));

    assert!(server.handle_server_event(ServerEvent::ClientConnected {
        client_id: 2,
        cols: 80,
        rows: 24,
        cell_width_px: 0,
        cell_height_px: 0,
        render_encoding: RenderEncoding::SemanticFrame,
        keybindings: None,
        direct_attach_requested: false,
        direct_graphics: false,
        media_listen: false,
        pixel_mouse: false,
        writer: writer_b,
        capabilities: Vec::new(),
    }));
    assert_eq!(
        server.app.state.prefix_code,
        crossterm::event::KeyCode::Char('b')
    );
    assert!(server
        .app
        .state
        .keybinds
        .new_tab
        .bindings
        .iter()
        .any(|binding| binding.label == "prefix+c"));
}

#[test]
fn server_keybinding_filter_keeps_whole_config_failures() {
    assert!(!config::is_keybinding_config_diagnostic(
        "config parse error: invalid value at `keys.new_tab = @`; using defaults"
    ));
    assert!(!config::is_keybinding_config_diagnostic(
        "config read error: permission denied at keys.toml; using defaults"
    ));
    assert!(config::is_keybinding_config_diagnostic(
        "unsafe direct keybinding: keys.close_pane would intercept typing"
    ));
}

#[test]
fn client_shell_host_theme_follows_foreground_client() {
    let mut server = test_headless_server();
    server.clients.insert(
        1,
        ClientConnection::new(
            (80, 24),
            crate::kitty_graphics::HostCellSize::default(),
            1,
            RenderEncoding::SemanticFrame,
            None,
        ),
    );
    server.clients.insert(
        2,
        ClientConnection::new(
            (80, 24),
            crate::kitty_graphics::HostCellSize::default(),
            2,
            RenderEncoding::SemanticFrame,
            None,
        ),
    );
    server.foreground_client_id = Some(1);

    let dark = protocol::ClientHostColor {
        r: 20,
        g: 30,
        b: 40,
    };
    let blue = protocol::ClientHostColor {
        r: 10,
        g: 20,
        b: 200,
    };
    assert!(
        server.handle_server_event(ServerEvent::ClientShellHostTheme {
            client_id: 1,
            update: protocol::ClientHostThemeUpdate::DefaultColor {
                kind: protocol::ClientHostDefaultColorKind::Background,
                color: dark,
            },
        })
    );
    assert!(
        server.handle_server_event(ServerEvent::ClientShellHostTheme {
            client_id: 1,
            update: protocol::ClientHostThemeUpdate::PaletteColors(vec![(4, blue)]),
        })
    );
    server.handle_server_event(ServerEvent::ClientShellHostTheme {
        client_id: 1,
        update: protocol::ClientHostThemeUpdate::Appearance(protocol::ClientHostAppearance::Dark),
    });
    assert_eq!(
        server.app.state.host_terminal_theme.background,
        Some(dark.into())
    );
    assert_eq!(
        server.app.state.host_terminal_theme.palette[4],
        Some(blue.into())
    );
    assert_eq!(
        server.app.state.host_terminal_appearance,
        Some(crate::terminal_theme::HostAppearance::Dark)
    );
    assert!(server.app.state.host_terminal_appearance_explicit);

    let light = protocol::ClientHostColor {
        r: 240,
        g: 230,
        b: 220,
    };
    assert!(
        !server.handle_server_event(ServerEvent::ClientShellHostTheme {
            client_id: 2,
            update: protocol::ClientHostThemeUpdate::DefaultColor {
                kind: protocol::ClientHostDefaultColorKind::Background,
                color: light,
            },
        })
    );
    assert_eq!(
        server.app.state.host_terminal_theme.background,
        Some(dark.into())
    );

    server.foreground_client_id = Some(2);
    server.sync_foreground_client_state();
    assert_eq!(
        server.app.state.host_terminal_theme.background,
        Some(light.into())
    );
    assert_eq!(
        server.app.state.host_terminal_appearance,
        Some(crate::terminal_theme::HostAppearance::Light)
    );
    assert!(!server.app.state.host_terminal_appearance_explicit);
}

#[test]
fn terminal_clients_store_known_cell_geometry_independently_of_pixel_mouse() {
    let mut server = test_headless_server();

    let (writer, _control_rx, _render_rx) = test_client_writer();
    assert!(!server.handle_server_event(ServerEvent::ClientConnected {
        client_id: 7,
        cols: 80,
        rows: 24,
        cell_width_px: 0,
        cell_height_px: 0,
        pixel_mouse: true,
        render_encoding: protocol::RenderEncoding::TerminalAnsi,
        keybindings: None,
        direct_attach_requested: true,
        direct_graphics: false,
        media_listen: false,
        capabilities: Vec::new(),
        writer,
    }));
    assert!(!server.clients[&7].pixel_mouse);
    assert_eq!(
        server.clients[&7].cell_size,
        crate::kitty_graphics::HostCellSize::default()
    );

    let (writer, _control_rx, _render_rx) = test_client_writer();
    assert!(!server.handle_server_event(ServerEvent::ClientConnected {
        client_id: 8,
        cols: 80,
        rows: 24,
        cell_width_px: 10,
        cell_height_px: 20,
        pixel_mouse: false,
        render_encoding: protocol::RenderEncoding::TerminalAnsi,
        keybindings: None,
        direct_attach_requested: true,
        direct_graphics: false,
        media_listen: false,
        capabilities: Vec::new(),
        writer,
    }));
    assert!(!server.clients[&8].pixel_mouse);
    assert_eq!(
        server.clients[&8].cell_size,
        crate::kitty_graphics::HostCellSize {
            width_px: 10,
            height_px: 20,
        }
    );
}

#[test]
fn local_keybinding_client_hides_server_keybinding_warnings() {
    let mut server = test_headless_server();
    let diagnostics = vec![
        "unsafe direct keybinding: keys.close_pane = \"x\" would intercept typing".to_owned(),
        "theme warning".to_owned(),
    ];
    let (full, without_keybindings) = server_config_diagnostic_summaries(&diagnostics);
    server.server_config_diagnostic = full.clone();
    server.server_config_diagnostic_without_keybindings = without_keybindings.clone();
    server.app.state.config_diagnostic = full;
    let local_keybindings = crate::config::Config::default().live_keybinds().unwrap();
    let (writer_a, _control_a, _render_a) = test_client_writer();
    let (writer_b, _control_b, _render_b) = test_client_writer();

    assert!(server.handle_server_event(ServerEvent::ClientConnected {
        client_id: 1,
        cols: 80,
        rows: 24,
        cell_width_px: 0,
        cell_height_px: 0,
        render_encoding: RenderEncoding::SemanticFrame,
        keybindings: Some(Box::new(local_keybindings)),
        direct_attach_requested: false,
        direct_graphics: false,
        media_listen: false,
        pixel_mouse: false,
        writer: writer_a,
        capabilities: Vec::new(),
    }));
    assert_eq!(server.app.state.config_diagnostic, without_keybindings);

    assert!(server.handle_server_event(ServerEvent::ClientConnected {
        client_id: 2,
        cols: 80,
        rows: 24,
        cell_width_px: 0,
        cell_height_px: 0,
        render_encoding: RenderEncoding::SemanticFrame,
        keybindings: None,
        direct_attach_requested: false,
        direct_graphics: false,
        media_listen: false,
        pixel_mouse: false,
        writer: writer_b,
        capabilities: Vec::new(),
    }));
    assert_eq!(
        server.app.state.config_diagnostic,
        server.server_config_diagnostic
    );
}

#[test]
fn local_keybinding_client_keeps_local_keybindings_after_settings_save() {
    let path = std::env::temp_dir().join(format!(
        "herdr-headless-settings-{}-{}.toml",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::write(&path, "onboarding = false\n").unwrap();
    let _guard = crate::config::test_config_env_lock().lock().unwrap();
    std::env::set_var(crate::config::CONFIG_PATH_ENV_VAR, &path);

    let mut server = test_headless_server();
    let local_config: crate::config::Config = toml::from_str(
        r#"
[keys]
prefix = "ctrl+a"
new_workspace = "prefix+n"
next_tab = ""
"#,
    )
    .unwrap();
    let local_keybindings = local_config.live_keybinds().unwrap();
    let (writer, _control, _render) = test_client_writer();
    assert!(server.handle_server_event(ServerEvent::ClientConnected {
        client_id: 1,
        cols: 80,
        rows: 24,
        cell_width_px: 0,
        cell_height_px: 0,
        render_encoding: RenderEncoding::SemanticFrame,
        keybindings: Some(Box::new(local_keybindings)),
        direct_attach_requested: false,
        direct_graphics: false,
        media_listen: false,
        pixel_mouse: false,
        writer,
        capabilities: Vec::new(),
    }));
    server.app.state.mode = crate::app::Mode::Settings;
    server.app.state.settings.section = crate::app::state::SettingsSection::Toast;
    server.app.state.settings.list.selected = 1;

    assert!(server.handle_server_event(ServerEvent::ClientInput {
        client_id: 1,
        data: b"\r".to_vec(),
    }));

    assert_eq!(
        server.app.state.prefix_code,
        crossterm::event::KeyCode::Char('a')
    );
    assert!(server
        .app
        .state
        .keybinds
        .new_workspace
        .bindings
        .iter()
        .any(|binding| binding.label == "prefix+n"));
    assert!(server.app.state.toast.is_none());
    let content = std::fs::read_to_string(&path).unwrap();
    assert!(content.contains("delivery = \"herdr\""));

    std::env::remove_var(crate::config::CONFIG_PATH_ENV_VAR);
    let _ = std::fs::remove_file(path);
}

#[test]
fn invalid_server_keybindings_apply_valid_subset_after_settings_save_without_caching_local_keybindings(
) {
    let path = std::env::temp_dir().join(format!(
        "herdr-headless-invalid-settings-{}-{}.toml",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::write(
        &path,
        "onboarding = false\n[keys]\nnew_workspace = \"x\"\n[ui.toast]\ndelivery = \"off\"\n",
    )
    .unwrap();
    let _guard = crate::config::test_config_env_lock().lock().unwrap();
    std::env::set_var(crate::config::CONFIG_PATH_ENV_VAR, &path);

    let mut server = test_headless_server();
    let previous_server_config: crate::config::Config =
        toml::from_str("[keys]\nprefix = \"ctrl+c\"\nnew_workspace = \"prefix+m\"\n").unwrap();
    server.server_keybindings = previous_server_config.live_keybinds().unwrap();
    let local_config: crate::config::Config = toml::from_str(
        r#"
[keys]
prefix = "ctrl+a"
new_workspace = "prefix+n"
next_tab = ""
"#,
    )
    .unwrap();
    let (writer_a, _control_a, _render_a) = test_client_writer();
    let (writer_b, _control_b, _render_b) = test_client_writer();

    assert!(server.handle_server_event(ServerEvent::ClientConnected {
        client_id: 1,
        cols: 80,
        rows: 24,
        cell_width_px: 0,
        cell_height_px: 0,
        render_encoding: RenderEncoding::SemanticFrame,
        keybindings: Some(Box::new(local_config.live_keybinds().unwrap())),
        direct_attach_requested: false,
        direct_graphics: false,
        media_listen: false,
        pixel_mouse: false,
        writer: writer_a,
        capabilities: Vec::new(),
    }));
    server.app.state.mode = crate::app::Mode::Settings;
    server.app.state.settings.section = crate::app::state::SettingsSection::Toast;
    server.app.state.settings.list.selected = 1;

    assert!(server.handle_server_event(ServerEvent::ClientInput {
        client_id: 1,
        data: b"\r".to_vec(),
    }));

    assert!(server.handle_server_event(ServerEvent::ClientConnected {
        client_id: 2,
        cols: 80,
        rows: 24,
        cell_width_px: 0,
        cell_height_px: 0,
        render_encoding: RenderEncoding::SemanticFrame,
        keybindings: None,
        direct_attach_requested: false,
        direct_graphics: false,
        media_listen: false,
        pixel_mouse: false,
        writer: writer_b,
        capabilities: Vec::new(),
    }));
    assert_eq!(
        server.app.state.prefix_code,
        crossterm::event::KeyCode::Char('b')
    );
    assert!(!server
        .app
        .state
        .keybinds
        .new_workspace
        .bindings
        .iter()
        .any(|binding| binding.label == "prefix+n"));
    assert!(server.app.state.keybinds.new_workspace.bindings.is_empty());

    std::env::remove_var(crate::config::CONFIG_PATH_ENV_VAR);
    let _ = std::fs::remove_file(path);
}

#[test]
fn terminal_attach_rejects_missing_terminal_and_removes_client() {
    let mut server = test_headless_server();
    let (writer, control_rx, _render_rx) = test_client_writer();

    assert!(!server.handle_server_event(ServerEvent::ClientConnected {
        client_id: 7,
        cols: 80,
        rows: 24,
        cell_width_px: 0,
        cell_height_px: 0,
        render_encoding: RenderEncoding::TerminalAnsi,
        keybindings: None,
        direct_attach_requested: true,
        direct_graphics: false,
        media_listen: false,
        pixel_mouse: false,
        writer,
        capabilities: Vec::new(),
    }));
    assert!(matches!(
        server.clients.get(&7).map(|client| &client.mode),
        Some(ClientConnectionMode::TerminalPending)
    ));

    assert!(
        !server.handle_server_event(ServerEvent::ClientAttachTerminal {
            client_id: 7,
            terminal_id: "term_missing".to_owned(),
            takeover: false,
        })
    );
    assert!(!server.clients.contains_key(&7));
    let reason = read_server_shutdown_reason(control_rx.recv().expect("shutdown message"));
    assert_eq!(
        reason,
        Some("terminal attach failed: terminal term_missing not found".to_owned())
    );
}

fn with_terminal_session_test_server(
    test: impl FnOnce(&mut HeadlessServer, crate::terminal::TerminalId, String, String),
) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    let _runtime_guard = rt.enter();
    let mut server = test_headless_server();
    let workspace = crate::workspace::Workspace::test_new("test");
    let pane_id = workspace.tabs[0].root_pane;
    let terminal_id = workspace.terminal_id(pane_id).expect("terminal id").clone();
    let terminal_id_string = terminal_id.to_string();
    let public_pane_id = format!("{}:p1", workspace.id);
    server.app.state.workspaces = vec![workspace];
    server.app.state.ensure_test_terminals();
    server.app.terminal_runtimes.insert(
        terminal_id.clone(),
        crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 24, b""),
    );

    test(&mut server, terminal_id, terminal_id_string, public_pane_id);

    drop(server);
    drop(_runtime_guard);
    rt.shutdown_timeout(Duration::from_millis(100));
}

fn connect_pending_terminal_client(server: &mut HeadlessServer, client_id: u64) {
    let _control_rx = connect_pending_terminal_client_with_control_rx(server, client_id);
}

fn connect_pending_terminal_client_with_control_rx(
    server: &mut HeadlessServer,
    client_id: u64,
) -> std::sync::mpsc::Receiver<Vec<u8>> {
    let (writer, control_rx, _render_rx) = test_client_writer();
    assert!(!server.handle_server_event(ServerEvent::ClientConnected {
        client_id,
        cols: 100,
        rows: 30,
        cell_width_px: 0,
        cell_height_px: 0,
        render_encoding: RenderEncoding::TerminalAnsi,
        keybindings: None,
        direct_attach_requested: true,
        direct_graphics: false,
        media_listen: false,
        pixel_mouse: false,
        writer,
        capabilities: Vec::new(),
    }));
    assert!(matches!(
        server.clients.get(&client_id).map(|client| &client.mode),
        Some(ClientConnectionMode::TerminalPending)
    ));
    control_rx
}

#[test]
fn explicit_agent_history_read_requires_idle_on_alternate_screen() {
    with_terminal_session_test_server(
        |server, terminal_id, _terminal_id_string, public_pane_id| {
            let terminal = server
                .app
                .state
                .terminals
                .get_mut(&terminal_id)
                .expect("terminal");
            terminal.detected_agent = Some(crate::detect::Agent::Claude);
            terminal.state = crate::detect::AgentState::Working;
            server.app.terminal_runtimes.insert(
                terminal_id,
                crate::terminal::TerminalRuntime::test_with_screen_bytes(
                    80,
                    24,
                    b"\x1b[?1049hworking",
                ),
            );
            let request = api::schema::Request {
                id: "read".into(),
                method: api::schema::Method::AgentRead(api::schema::AgentReadParams {
                    target: public_pane_id.clone(),
                    source: api::schema::ReadSource::Recent,
                    lines: Some(200),
                    format: api::schema::ReadFormat::Text,
                    strip_ansi: true,
                }),
            };

            assert_eq!(
                    server.agent_read_not_idle_error(&request),
                    Some(api::schema::ErrorBody {
                        code: "agent_not_idle".into(),
                        message: format!(
                            "cannot read 200 lines while {public_pane_id} is working: its alternate-screen history can only be captured by scrolling while idle. Wait and retry, or use --source visible"
                        ),
                    })
                );

            let mut default_request = request.clone();
            let api::schema::Method::AgentRead(params) = &mut default_request.method else {
                unreachable!();
            };
            params.lines = None;
            assert_eq!(server.agent_read_not_idle_error(&default_request), None);

            let mut visible_request = request;
            let api::schema::Method::AgentRead(params) = &mut visible_request.method else {
                unreachable!();
            };
            params.source = api::schema::ReadSource::Visible;
            assert_eq!(server.agent_read_not_idle_error(&visible_request), None);
        },
    );
}

#[test]
fn terminal_attach_disconnect_restores_client_shell_pane_size() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    let _runtime_guard = rt.enter();
    let mut server = test_headless_server();
    let mut workspace = crate::workspace::Workspace::test_new("test");
    let second_tab = workspace.test_add_tab(Some("second"));
    let pane_id = workspace.tabs[0].root_pane;
    let terminal_id = workspace.terminal_id(pane_id).expect("terminal id").clone();
    let terminal_id_string = terminal_id.to_string();
    server.app.state.workspaces = vec![workspace];
    server.app.state.ensure_test_terminals();
    server.app.state.active = Some(0);
    server.app.state.selected = 0;
    let second_tab_id = server
        .app
        .public_tab_id(0, second_tab)
        .expect("second tab id");
    server.app.terminal_runtimes.insert(
        terminal_id.clone(),
        crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 24, b""),
    );
    server.clients.insert(
        1,
        ClientConnection::new(
            (120, 40),
            crate::kitty_graphics::HostCellSize::default(),
            1,
            RenderEncoding::SemanticFrame,
            None,
        ),
    );
    server.foreground_client_id = Some(1);
    server.sync_foreground_client_state();
    server.reconcile_client_shell_locations();
    assert!(server.claim_unowned_shell_tab_geometry(1, true));
    let expected_shell_size = server
        .app
        .terminal_runtimes
        .get(&terminal_id)
        .expect("runtime")
        .current_size();

    connect_pending_terminal_client(&mut server, 2);
    assert!(
        server.handle_server_event(ServerEvent::ClientAttachTerminal {
            client_id: 2,
            terminal_id: terminal_id_string,
            takeover: false,
        })
    );
    assert_eq!(server.foreground_client_id, Some(1));
    assert!(server
        .app
        .state
        .direct_attach_resize_locks
        .contains(&terminal_id));
    assert_eq!(
        server
            .app
            .terminal_runtimes
            .get(&terminal_id)
            .expect("runtime")
            .current_size(),
        (30, 100)
    );

    assert!(server.focus_shell_client_on_tab(1, &second_tab_id));
    assert!(server.handle_server_event(ServerEvent::ClientDisconnected { client_id: 2 }));
    assert!(!server
        .app
        .state
        .direct_attach_resize_locks
        .contains(&terminal_id));
    assert_eq!(
        server
            .app
            .terminal_runtimes
            .get(&terminal_id)
            .expect("runtime")
            .current_size(),
        expected_shell_size
    );

    drop(server);
    drop(_runtime_guard);
    rt.shutdown_timeout(Duration::from_millis(100));
}

#[test]
fn terminal_observe_allows_multiple_clients_without_attach_ownership() {
    with_terminal_session_test_server(|server, terminal_id, terminal_id_string, _| {
        let initial_size = server
            .app
            .terminal_runtimes
            .get(&terminal_id)
            .expect("runtime")
            .current_size();

        for client_id in [7, 8] {
            connect_pending_terminal_client(server, client_id);
            assert!(
                server.handle_server_event(ServerEvent::ClientObserveTerminal {
                    client_id,
                    target: terminal_id_string.clone(),
                })
            );
        }

        assert!(server.terminal_attach_owners.is_empty());
        assert!(!server
            .app
            .state
            .direct_attach_resize_locks
            .contains(&terminal_id));
        assert_eq!(
            server
                .app
                .terminal_runtimes
                .get(&terminal_id)
                .expect("runtime")
                .current_size(),
            initial_size
        );
        assert_eq!(
            terminal_stream_client_ids(&server.clients, &terminal_id_string).len(),
            2
        );
    });
}

#[test]
fn direct_terminal_observer_keeps_hidden_pty_source_renderable_with_client_shell() {
    let mut server = test_headless_server();
    let mut workspace = crate::workspace::Workspace::test_new("test");
    let background_tab = workspace.test_add_tab(Some("background"));
    let background_pane = workspace.tabs[background_tab].root_pane;
    let hidden_pane = workspace.tabs[0].root_pane;
    let terminal_id = workspace
        .terminal_id(background_pane)
        .expect("background terminal id")
        .to_string();
    server.app.state.workspaces = vec![workspace];
    server.app.state.active = Some(0);
    server.app.state.selected = 0;
    server.app.state.ensure_test_terminals();

    let (shell_writer, _shell_control_rx, _shell_render_rx) = test_client_writer();
    server.clients.insert(
        1,
        ClientConnection::new(
            (120, 40),
            crate::kitty_graphics::HostCellSize::default(),
            1,
            RenderEncoding::SemanticFrame,
            Some(shell_writer),
        ),
    );
    assert!(!server.pty_sources_visible_to_any_render_target(&HashSet::from([background_pane])));

    let (observer_writer, _observer_control_rx, _observer_render_rx) = test_client_writer();
    server.clients.insert(
        2,
        ClientConnection::new_with_mode(
            ClientConnectionMode::TerminalObserve { terminal_id },
            (80, 24),
            crate::kitty_graphics::HostCellSize::default(),
            2,
            RenderEncoding::SemanticFrame,
            Some(observer_writer),
        ),
    );

    assert!(server.pty_sources_visible_to_any_render_target(&HashSet::from([background_pane])));
    server.sync_immediate_pty_sources();
    assert!(server.app.render_dirty.request_pty(background_pane));
    assert!(server.has_pending_presentation_work(false, false));
    assert!(server.app.render_dirty.request_pty(hidden_pane));
}

#[test]
fn terminal_observe_resolves_public_pane_id() {
    with_terminal_session_test_server(|server, terminal_id, _, public_pane_id| {
        connect_pending_terminal_client(server, 7);
        assert!(
            server.handle_server_event(ServerEvent::ClientObserveTerminal {
                client_id: 7,
                target: public_pane_id,
            })
        );

        assert!(matches!(
            server.clients.get(&7).map(|client| &client.mode),
            Some(ClientConnectionMode::TerminalObserve { terminal_id: observed })
                if observed == &terminal_id.to_string()
        ));
    });
}

#[test]
fn terminal_control_resolves_public_pane_id_and_takes_ownership() {
    with_terminal_session_test_server(|server, terminal_id, terminal_id_string, public_pane_id| {
        connect_pending_terminal_client(server, 7);
        assert!(
            server.handle_server_event(ServerEvent::ClientControlTerminal {
                client_id: 7,
                target: public_pane_id,
                takeover: false,
            })
        );

        assert!(matches!(
            server.clients.get(&7).map(|client| &client.mode),
            Some(ClientConnectionMode::TerminalAttach { terminal_id: attached })
                if attached == &terminal_id_string
        ));
        assert_eq!(
            server.terminal_attach_owners.get(&terminal_id_string),
            Some(&7)
        );
        assert!(server
            .app
            .state
            .direct_attach_resize_locks
            .contains(&terminal_id));
    });
}

#[test]
fn terminal_control_rejects_attach_during_alt_screen_read() {
    with_terminal_session_test_server(|server, terminal_id, terminal_id_string, _| {
        let (respond_to, _response_rx) = std::sync::mpsc::channel();
        server.pending_alt_screen_reads.push(
            crate::server::alt_screen_read::PendingAltScreenRead::start(
                terminal_id,
                "read".into(),
                respond_to,
                "fallback".into(),
                api::schema::PaneReadResult {
                    pane_id: "w1:p1".into(),
                    workspace_id: "w1".into(),
                    tab_id: "w1:t1".into(),
                    source: api::schema::ReadSource::Recent,
                    format: api::schema::ReadFormat::Text,
                    text: String::new(),
                    revision: 0,
                    truncated: false,
                },
                120,
                false,
                crate::terminal::ScreenSnapshot {
                    cols: 80,
                    rows: Vec::new(),
                },
                0,
                Instant::now(),
            ),
        );
        let control_rx = connect_pending_terminal_client_with_control_rx(server, 7);

        assert!(
            !server.handle_server_event(ServerEvent::ClientControlTerminal {
                client_id: 7,
                target: terminal_id_string.clone(),
                takeover: false,
            })
        );
        assert!(!server.clients.contains_key(&7));
        assert!(!server
            .terminal_attach_owners
            .contains_key(&terminal_id_string));
        let reason = read_server_shutdown_reason(control_rx.recv().expect("shutdown message"));
        assert_eq!(
                reason,
                Some(format!(
                    "terminal attach failed: terminal {terminal_id_string} has a read in progress; retry"
                ))
            );
    });
}

#[test]
fn terminal_control_rejects_second_controller_without_takeover() {
    with_terminal_session_test_server(|server, _terminal_id, terminal_id_string, _| {
        connect_pending_terminal_client(server, 7);
        assert!(
            server.handle_server_event(ServerEvent::ClientControlTerminal {
                client_id: 7,
                target: terminal_id_string.clone(),
                takeover: false,
            })
        );

        connect_pending_terminal_client(server, 8);
        assert!(
            !server.handle_server_event(ServerEvent::ClientControlTerminal {
                client_id: 8,
                target: terminal_id_string.clone(),
                takeover: false,
            })
        );

        assert!(server.clients.contains_key(&7));
        assert!(!server.clients.contains_key(&8));
        assert_eq!(
            server.terminal_attach_owners.get(&terminal_id_string),
            Some(&7)
        );
    });
}

#[test]
fn terminal_control_takeover_replaces_existing_controller() {
    with_terminal_session_test_server(|server, _terminal_id, terminal_id_string, _| {
        connect_pending_terminal_client(server, 7);
        assert!(
            server.handle_server_event(ServerEvent::ClientControlTerminal {
                client_id: 7,
                target: terminal_id_string.clone(),
                takeover: false,
            })
        );

        connect_pending_terminal_client(server, 8);
        assert!(
            server.handle_server_event(ServerEvent::ClientControlTerminal {
                client_id: 8,
                target: terminal_id_string.clone(),
                takeover: true,
            })
        );

        assert!(!server.clients.contains_key(&7));
        assert!(server.clients.contains_key(&8));
        assert_eq!(
            server.terminal_attach_owners.get(&terminal_id_string),
            Some(&8)
        );
    });
}

#[test]
fn terminal_observe_can_coexist_with_terminal_control() {
    with_terminal_session_test_server(|server, _terminal_id, terminal_id_string, _| {
        connect_pending_terminal_client(server, 7);
        assert!(
            server.handle_server_event(ServerEvent::ClientControlTerminal {
                client_id: 7,
                target: terminal_id_string.clone(),
                takeover: false,
            })
        );

        connect_pending_terminal_client(server, 8);
        assert!(
            server.handle_server_event(ServerEvent::ClientObserveTerminal {
                client_id: 8,
                target: terminal_id_string.clone(),
            })
        );

        assert_eq!(
            server.terminal_attach_owners.get(&terminal_id_string),
            Some(&7)
        );
        assert!(matches!(
            server.clients.get(&8).map(|client| &client.mode),
            Some(ClientConnectionMode::TerminalObserve { terminal_id })
                if terminal_id == &terminal_id_string
        ));
        assert_eq!(
            terminal_stream_client_ids(&server.clients, &terminal_id_string).len(),
            2
        );
    });
}

#[test]
fn terminal_control_detach_sends_shutdown_before_removal() {
    with_terminal_session_test_server(|server, _terminal_id, terminal_id_string, _| {
        let control_rx = connect_pending_terminal_client_with_control_rx(server, 7);
        assert!(
            server.handle_server_event(ServerEvent::ClientControlTerminal {
                client_id: 7,
                target: terminal_id_string.clone(),
                takeover: false,
            })
        );

        assert!(server.handle_server_event(ServerEvent::ClientDetach { client_id: 7 }));

        assert!(!server.clients.contains_key(&7));
        assert!(!server
            .terminal_attach_owners
            .contains_key(&terminal_id_string));
        let reason = read_server_shutdown_reason(control_rx.recv().expect("shutdown message"));
        assert_eq!(reason, Some("detached".to_owned()));
    });
}

#[test]
fn terminal_observe_rejects_later_attach_upgrade() {
    with_terminal_session_test_server(|server, terminal_id, terminal_id_string, _| {
        connect_pending_terminal_client(server, 7);
        assert!(
            server.handle_server_event(ServerEvent::ClientObserveTerminal {
                client_id: 7,
                target: terminal_id_string.clone(),
            })
        );
        assert!(
            !server.handle_server_event(ServerEvent::ClientAttachTerminal {
                client_id: 7,
                terminal_id: terminal_id_string,
                takeover: true,
            })
        );

        assert!(!server.clients.contains_key(&7));
        assert!(server.terminal_attach_owners.is_empty());
        assert!(!server
            .app
            .state
            .direct_attach_resize_locks
            .contains(&terminal_id));
    });
}

#[test]
fn terminal_attach_rejects_later_observe_and_clears_ownership() {
    with_terminal_session_test_server(|server, terminal_id, terminal_id_string, _| {
        connect_pending_terminal_client(server, 7);
        assert!(
            server.handle_server_event(ServerEvent::ClientAttachTerminal {
                client_id: 7,
                terminal_id: terminal_id_string.clone(),
                takeover: false,
            })
        );
        assert_eq!(
            server.terminal_attach_owners.get(&terminal_id_string),
            Some(&7)
        );
        assert!(server
            .app
            .state
            .direct_attach_resize_locks
            .contains(&terminal_id));

        assert!(
            !server.handle_server_event(ServerEvent::ClientObserveTerminal {
                client_id: 7,
                target: terminal_id_string.clone(),
            })
        );

        assert!(!server.clients.contains_key(&7));
        assert!(server.terminal_attach_owners.is_empty());
        assert!(!server
            .app
            .state
            .direct_attach_resize_locks
            .contains(&terminal_id));
    });
}

fn app_client_marks_git_refresh_due_on_first_attach(render_encoding: RenderEncoding) {
    let mut server = test_headless_server();
    server
        .app
        .state
        .workspaces
        .push(crate::workspace::Workspace::test_new("test"));
    let future = Instant::now() + Duration::from_secs(60);
    server.app.last_git_remote_status_refresh = future;
    let (writer, _control_rx, _render_rx) = test_client_writer();

    assert!(server.handle_server_event(ServerEvent::ClientConnected {
        client_id: 7,
        cols: 80,
        rows: 24,
        cell_width_px: 0,
        cell_height_px: 0,
        render_encoding,
        keybindings: None,
        direct_attach_requested: false,
        direct_graphics: false,
        media_listen: false,
        pixel_mouse: false,
        writer,
        capabilities: Vec::new(),
    }));

    assert!(server.has_app_client());
    assert!(server
        .app
        .git_refresh_deadline()
        .is_some_and(|deadline| deadline <= Instant::now()));
}

#[test]
fn terminal_ansi_app_client_enables_headless_git_refresh() {
    app_client_marks_git_refresh_due_on_first_attach(RenderEncoding::TerminalAnsi);
}

#[test]
fn pending_terminal_attach_client_does_not_enable_headless_git_refresh() {
    let mut server = test_headless_server();
    server
        .app
        .state
        .workspaces
        .push(crate::workspace::Workspace::test_new("test"));
    let (writer, _control_rx, _render_rx) = test_client_writer();

    assert!(!server.handle_server_event(ServerEvent::ClientConnected {
        client_id: 7,
        cols: 80,
        rows: 24,
        cell_width_px: 0,
        cell_height_px: 0,
        render_encoding: RenderEncoding::TerminalAnsi,
        keybindings: None,
        direct_attach_requested: true,
        direct_graphics: false,
        media_listen: false,
        pixel_mouse: false,
        writer,
        capabilities: Vec::new(),
    }));

    assert!(!server.has_app_client());
    assert_eq!(
        server.app.next_headless_loop_deadline_with_git_refresh(
            Instant::now(),
            false,
            server.has_app_client()
        ),
        None
    );
}

#[test]
fn writerless_app_client_does_not_enable_headless_git_refresh() {
    let mut server = test_headless_server();
    server
        .app
        .state
        .workspaces
        .push(crate::workspace::Workspace::test_new("test"));
    let (writer, _control_rx, _render_rx) = test_client_writer();

    assert!(server.handle_server_event(ServerEvent::ClientConnected {
        client_id: 7,
        cols: 80,
        rows: 24,
        cell_width_px: 0,
        cell_height_px: 0,
        render_encoding: RenderEncoding::SemanticFrame,
        keybindings: None,
        direct_attach_requested: false,
        direct_graphics: false,
        media_listen: false,
        pixel_mouse: false,
        writer,
        capabilities: Vec::new(),
    }));
    assert!(server.has_app_client());

    server.clients.get_mut(&7).expect("client").writer = None;

    assert!(!server.has_app_client());
    assert_eq!(
        server.app.next_headless_loop_deadline_with_git_refresh(
            Instant::now(),
            false,
            server.has_app_client()
        ),
        None
    );
}

#[test]
fn semantic_app_client_marks_git_refresh_due_on_first_attach() {
    app_client_marks_git_refresh_due_on_first_attach(RenderEncoding::SemanticFrame);
}

#[test]
fn unchanged_git_refresh_does_not_request_headless_render() {
    let mut server = test_headless_server();
    server.app.git_refresh_in_flight = true;
    let mut workspace = crate::workspace::Workspace::test_new("one");
    let workspace_id = workspace.id.clone();
    let cwd = workspace.identity_cwd.clone();
    workspace.cached_auto_label = "cached".into();
    workspace.cached_git_status_key = cwd.clone();
    workspace.cached_git_branch = None;
    server.app.state.workspaces.push(workspace);

    let changed = server.handle_internal_event_with_forwarding(AppEvent::GitStatusRefreshed {
        results: vec![crate::workspace::WorkspaceGitStatus {
            workspace_id,
            resolved_identity_cwd: cwd.clone(),
            status_cache_key: cwd,
            demand: crate::workspace::GitStatusRefreshDemand::ALL,
            auto_label: "cached".into(),
            branch: None,
            ahead_behind: None,
            space: None,
        }],
        cache_updates: Vec::new(),
    });

    assert!(!changed);
    assert!(!server.app.git_refresh_in_flight);
}

#[test]
fn changed_git_refresh_requests_headless_render() {
    let mut server = test_headless_server();
    let workspace = crate::workspace::Workspace::test_new("one");
    let workspace_id = workspace.id.clone();
    let cwd = workspace.identity_cwd.clone();
    server.app.state.workspaces.push(workspace);

    let changed = server.handle_internal_event_with_forwarding(AppEvent::GitStatusRefreshed {
        results: vec![crate::workspace::WorkspaceGitStatus {
            workspace_id,
            resolved_identity_cwd: cwd.clone(),
            status_cache_key: cwd,
            demand: crate::workspace::GitStatusRefreshDemand::ALL,
            auto_label: "one".into(),
            branch: Some("changed".into()),
            ahead_behind: None,
            space: None,
        }],
        cache_updates: Vec::new(),
    });

    assert!(changed);
}

#[tokio::test]
async fn pane_death_reconciles_each_client_view_and_focus() {
    let mut server = test_headless_server();
    let mut workspace = crate::workspace::Workspace::test_new("pane-death-views");
    let dead_pane = workspace.tabs[0].root_pane;
    let second_tab = workspace.test_add_tab(Some("second"));
    let second_pane = workspace.tabs[second_tab].root_pane;
    let (second_runtime, mut second_input) =
        crate::terminal::TerminalRuntime::test_with_channel_and_scrollback_bytes(
            80,
            24,
            0,
            b"\x1b[?1004h",
            4,
        );
    workspace.insert_test_runtime(second_pane, second_runtime);
    server.app.state.workspaces = vec![workspace];
    server.app.state.ensure_test_terminals();
    server.app.state.active = Some(0);
    server.app.state.selected = 0;
    server.app.state.mode = crate::app::Mode::Terminal;
    let second_tab_id = server
        .app
        .public_tab_id(0, second_tab)
        .expect("second tab id");

    let (first_control, _) = connect_test_shell(&mut server, 71, 100, 30);
    let (second_control, _) = connect_test_shell(&mut server, 72, 70, 20);
    let _ = first_control.recv().expect("first snapshot");
    let _ = second_control.recv().expect("second snapshot");
    assert!(server.focus_shell_client_on_tab(72, &second_tab_id));
    server.clients.get_mut(&71).unwrap().outer_terminal_focus = Some(true);
    server.clients.get_mut(&72).unwrap().outer_terminal_focus = Some(false);

    assert!(server.handle_internal_event_with_forwarding(AppEvent::PaneDied { pane_id: dead_pane }));

    assert_eq!(
        server.shell_tab_id_for_client(71).as_deref(),
        Some(second_tab_id.as_str())
    );
    assert_eq!(
        server.shell_tab_id_for_client(72).as_deref(),
        Some(second_tab_id.as_str())
    );
    assert_eq!(
        second_input
            .try_recv()
            .expect("fallback focus gained input"),
        Bytes::from_static(b"\x1b[I")
    );
    assert!(
        second_input.try_recv().is_err(),
        "focus gain was duplicated"
    );
    assert_eq!(
        server.tab_geometry_controllers.get(&second_tab_id),
        Some(&71)
    );
    let before_resize = server.app.state.workspaces[0].test_runtimes[&second_pane].current_size();
    assert!(server.handle_server_event(ServerEvent::ClientShellResize {
        client_id: 71,
        surface_cols: 90,
        surface_rows: 25,
        cell_width_px: 0,
        cell_height_px: 0,
        pixel_mouse: false,
    }));
    assert_ne!(
        server.app.state.workspaces[0].test_runtimes[&second_pane].current_size(),
        before_resize
    );
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn pane_death_reapplies_controller_geometry() {
    let mut server = test_headless_server();
    let mut workspace = crate::workspace::Workspace::test_new("pane-death-geometry");
    let first_pane = workspace.tabs[0].root_pane;
    let dead_pane = workspace.test_split(ratatui::layout::Direction::Vertical);
    workspace.insert_test_runtime(
        first_pane,
        crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 24, b""),
    );
    workspace.insert_test_runtime(
        dead_pane,
        crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 24, b""),
    );
    server.app.state.workspaces = vec![workspace];
    server.app.state.active = Some(0);
    server.app.state.selected = 0;
    server.app.state.mode = crate::app::Mode::Terminal;

    let (control, _) = connect_test_shell(&mut server, 73, 185, 46);
    let _ = control.recv().expect("snapshot");
    let shrunk = server.app.state.workspaces[0].test_runtimes[&first_pane].current_size();
    assert!(shrunk.0 < 46);

    assert!(server.handle_internal_event_with_forwarding(AppEvent::PaneDied { pane_id: dead_pane }));

    let runtime = &server.app.state.workspaces[0].test_runtimes[&first_pane];
    let grown = runtime.current_size();
    assert!(grown.0 > shrunk.0);
    assert_eq!(runtime.terminal_dimensions(), Some((grown.1, grown.0)));
    assert_eq!(
        runtime.scroll_metrics().unwrap().viewport_rows,
        grown.0 as usize
    );
    shutdown_test_runtimes(&mut server);
}

#[test]
fn terminal_attach_client_exits_when_worktree_runtime_restore_fails() {
    let mut server = test_headless_server();
    let workspace = crate::workspace::Workspace::test_new("attached");
    let pane_id = workspace.tabs[0].root_pane;
    server.app.state.workspaces = vec![workspace];
    server.app.state.ensure_test_terminals();
    let terminal_id = server.app.state.workspaces[0]
        .pane_state(pane_id)
        .expect("pane")
        .attached_terminal_id
        .clone();
    server
        .app
        .state
        .terminals
        .get_mut(&terminal_id)
        .unwrap()
        .set_detected_state(
            Some(crate::detect::Agent::Codex),
            crate::detect::AgentState::Working,
        );
    let terminal_id = terminal_id.to_string();
    let (writer, control_rx, _render_rx) = test_client_writer();

    assert!(!server.handle_server_event(ServerEvent::ClientConnected {
        client_id: 7,
        cols: 80,
        rows: 24,
        cell_width_px: 0,
        cell_height_px: 0,
        render_encoding: RenderEncoding::TerminalAnsi,
        keybindings: None,
        direct_attach_requested: true,
        direct_graphics: false,
        media_listen: false,
        pixel_mouse: false,
        writer,
        capabilities: Vec::new(),
    }));
    assert!(
        server.handle_server_event(ServerEvent::ClientAttachTerminal {
            client_id: 7,
            terminal_id: terminal_id.clone(),
            takeover: false,
        })
    );
    assert_eq!(server.terminal_attach_owners.get(&terminal_id), Some(&7));
    server
        .app
        .pending_worktree_remove_runtime_exits
        .insert(pane_id, 1);
    server
        .app
        .pending_worktree_remove_runtime_restores
        .insert(pane_id, 7);

    assert!(
        server.handle_internal_event_with_forwarding(AppEvent::WorktreeRuntimeRestoreFailed {
            pane_id,
            operation_id: 7,
        })
    );

    assert!(!server.clients.contains_key(&7));
    assert!(!server.terminal_attach_owners.contains_key(&terminal_id));
    let reason = read_server_shutdown_reason(control_rx.recv().expect("shutdown message"));
    assert_eq!(reason, Some(format!("terminal {terminal_id} exited")));
}

#[test]
fn terminal_attach_client_exits_when_worktree_remove_succeeds() {
    let mut server = test_headless_server();
    let checkout = PathBuf::from("/repo/herdr-issue");
    let parent = crate::workspace::Workspace::test_new("parent");
    let mut workspace = crate::workspace::Workspace::test_new("worktree");
    workspace.worktree_space = Some(crate::workspace::WorktreeSpaceMembership {
        key: "repo-key".into(),
        label: "herdr".into(),
        repo_root: "/repo/herdr".into(),
        checkout_path: checkout.clone(),
        is_linked_worktree: true,
    });
    let workspace_id = workspace.id.clone();
    let pane_id = workspace.tabs[0].root_pane;
    let terminal_id = workspace.terminal_id(pane_id).cloned().unwrap();
    server.app.state.workspaces = vec![parent, workspace];
    server.app.state.ensure_test_terminals();
    server.app.state.active = Some(1);
    server.app.state.selected = 1;
    let checkout_key = crate::worktree::canonical_or_original(&checkout);
    server
        .app
        .pending_api_worktree_removes
        .insert(workspace_id.clone(), 7);
    server
        .app
        .pending_api_worktree_remove_paths
        .insert(checkout_key.clone(), 7);
    server
        .app
        .pending_worktree_remove_runtime_exits
        .insert(pane_id, 1);
    let terminal_id = terminal_id.to_string();
    let (writer, control_rx, _render_rx) = test_client_writer();

    assert!(!server.handle_server_event(ServerEvent::ClientConnected {
        client_id: 7,
        cols: 80,
        rows: 24,
        cell_width_px: 0,
        cell_height_px: 0,
        render_encoding: RenderEncoding::TerminalAnsi,
        keybindings: None,
        direct_attach_requested: true,
        direct_graphics: false,
        media_listen: false,
        pixel_mouse: false,
        writer,
        capabilities: Vec::new(),
    }));
    assert!(
        server.handle_server_event(ServerEvent::ClientAttachTerminal {
            client_id: 7,
            terminal_id: terminal_id.clone(),
            takeover: false,
        })
    );
    let (respond_to, _response_rx) = std::sync::mpsc::channel();

    assert!(
        server.handle_internal_event_with_forwarding(AppEvent::WorktreeRemoveFinished(Box::new(
            crate::events::WorktreeRemoveResult {
                workspace_id,
                path: checkout,
                workspace: None,
                worktree: None,
                forced: true,
                api_request: Some(crate::events::ApiWorktreeRemoveRequest {
                    id: "req".into(),
                    operation_id: 7,
                    checkout_key,
                    shutdown_panes: vec![pane_id],
                    respond_to,
                }),
                result: Ok(()),
            }
        )))
    );

    assert!(!server.clients.contains_key(&7));
    assert!(!server.terminal_attach_owners.contains_key(&terminal_id));
    let reason = read_server_shutdown_reason(control_rx.recv().expect("shutdown message"));
    assert_eq!(reason, Some(format!("terminal {terminal_id} exited")));
}

#[test]
fn expected_worktree_runtime_exit_does_not_release_agent() {
    let mut server = test_headless_server();
    let workspace = crate::workspace::Workspace::test_new("worktree");
    let pane_id = workspace.tabs[0].root_pane;
    let terminal_id = workspace.terminal_id(pane_id).cloned().unwrap();
    server.app.state.workspaces = vec![workspace];
    server.app.state.ensure_test_terminals();
    server
        .app
        .state
        .terminals
        .get_mut(&terminal_id)
        .unwrap()
        .set_detected_state(
            Some(crate::detect::Agent::Codex),
            crate::detect::AgentState::Working,
        );
    server
        .app
        .pending_worktree_remove_runtime_exits
        .insert(pane_id, 1);

    assert!(server.handle_internal_event_with_forwarding(AppEvent::PaneDied { pane_id }));

    assert_eq!(
        server.app.state.terminals[&terminal_id].state,
        crate::detect::AgentState::Working
    );
    assert!(server.app.find_pane(pane_id).is_some());
}

#[test]
fn terminal_attach_scroll_moves_attached_runtime_viewport() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    let _runtime_guard = rt.enter();
    let mut bytes = Vec::new();
    for line in 0..80 {
        bytes.extend_from_slice(format!("line {line:02}\r\n").as_bytes());
    }
    let runtime = crate::terminal::TerminalRuntime::test_with_scrollback_bytes(20, 5, 4096, &bytes);

    apply_terminal_attach_scroll(
        &runtime,
        AttachScrollSource::Wheel,
        AttachScrollDirection::Up,
        3,
        None,
        None,
        0,
    )
    .expect("scroll up");
    let metrics = runtime.scroll_metrics().expect("scroll metrics");
    assert_eq!(metrics.offset_from_bottom, 3);

    apply_terminal_attach_scroll(
        &runtime,
        AttachScrollSource::Wheel,
        AttachScrollDirection::Down,
        2,
        None,
        None,
        0,
    )
    .expect("scroll down");
    let metrics = runtime.scroll_metrics().expect("scroll metrics");
    assert_eq!(metrics.offset_from_bottom, 1);
    drop(runtime);
    drop(_runtime_guard);
    rt.shutdown_timeout(Duration::from_millis(100));
}

#[test]
fn client_pane_pixel_mouse_uses_runtime_pixel_encoding() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    let _runtime_guard = rt.enter();
    let (runtime, mut input_rx) =
        crate::terminal::TerminalRuntime::test_with_channel_and_scrollback_bytes(
            20,
            5,
            0,
            b"\x1b[?1003h\x1b[?1006h\x1b[?1016h",
            4,
        );
    runtime.resize(5, 20, 10, 20);

    apply_client_pane_input_events(
        &runtime,
        &[crate::protocol::ClientPaneInputEvent::Mouse {
            kind: crate::protocol::ClientMouseKind::Moved,
            position: crate::protocol::ClientMousePosition::Pixels {
                x: 21,
                y: 22,
                column: 2,
                row: 1,
            },
            geometry: None,
            modifiers: 0,
            lines: 3,
        }],
    )
    .expect("pixel mouse input");
    assert_eq!(
        input_rx.try_recv().expect("encoded pixel mouse"),
        Bytes::from_static(b"\x1b[<35;21;22M")
    );
    drop(runtime);
    drop(_runtime_guard);
    rt.shutdown_timeout(Duration::from_millis(100));
}

#[test]
fn client_pane_pixel_mouse_stays_pixel_scaled_when_sgr_is_reasserted() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    let _runtime_guard = rt.enter();
    let (runtime, mut input_rx) =
        crate::terminal::TerminalRuntime::test_with_channel_and_scrollback_bytes(
            80,
            24,
            0,
            b"\x1b[?1003h\x1b[?1006h\x1b[?1016h\x1b[?1006h",
            4,
        );
    runtime.resize(24, 80, 10, 20);

    apply_client_pane_input_events(
        &runtime,
        &[crate::protocol::ClientPaneInputEvent::Mouse {
            kind: crate::protocol::ClientMouseKind::Down(crate::protocol::ClientMouseButton::Left),
            position: crate::protocol::ClientMousePosition::Pixels {
                x: 403,
                y: 240,
                column: 40,
                row: 12,
            },
            geometry: None,
            modifiers: 0,
            lines: 1,
        }],
    )
    .expect("pixel mouse input");
    assert_eq!(
        input_rx.try_recv().expect("encoded pixel mouse"),
        Bytes::from_static(b"\x1b[<0;403;240M")
    );
    drop(runtime);
    drop(_runtime_guard);
    rt.shutdown_timeout(Duration::from_millis(100));
}

#[test]
fn client_pane_pixel_mouse_falls_back_to_canonical_cell_position() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    let _runtime_guard = rt.enter();
    let (runtime, mut input_rx) =
        crate::terminal::TerminalRuntime::test_with_channel_and_scrollback_bytes(
            20,
            5,
            0,
            b"\x1b[?1003h\x1b[?1006h",
            4,
        );
    runtime.resize(5, 20, 10, 20);

    apply_client_pane_input_events(
        &runtime,
        &[crate::protocol::ClientPaneInputEvent::Mouse {
            kind: crate::protocol::ClientMouseKind::Moved,
            position: crate::protocol::ClientMousePosition::Pixels {
                x: 21,
                y: 22,
                column: 2,
                row: 1,
            },
            geometry: None,
            modifiers: 0,
            lines: 3,
        }],
    )
    .expect("cell mouse fallback");
    assert_eq!(
        input_rx.try_recv().expect("encoded cell mouse"),
        Bytes::from_static(b"\x1b[<35;3;2M")
    );
    drop(runtime);
    drop(_runtime_guard);
    rt.shutdown_timeout(Duration::from_millis(100));
}

#[test]
fn client_pane_wheel_input_accumulates_scrollback_offset() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    let _runtime_guard = rt.enter();
    let mut bytes = Vec::new();
    for line in 0..80 {
        bytes.extend_from_slice(format!("line {line:02}\r\n").as_bytes());
    }
    let (runtime, mut input_rx) =
        crate::terminal::TerminalRuntime::test_with_channel_and_scrollback_bytes(
            20, 5, 4096, &bytes, 4,
        );
    let scroll = |kind| crate::protocol::ClientPaneInputEvent::Mouse {
        kind,
        position: crate::protocol::ClientMousePosition::Cell { column: 2, row: 1 },
        geometry: None,
        modifiers: 0,
        lines: 3,
    };

    apply_client_pane_input_events(
        &runtime,
        &[scroll(crate::protocol::ClientMouseKind::ScrollUp)],
    )
    .expect("first scroll up");
    apply_client_pane_input_events(
        &runtime,
        &[scroll(crate::protocol::ClientMouseKind::ScrollUp)],
    )
    .expect("second scroll up");
    assert_eq!(
        runtime
            .scroll_metrics()
            .expect("scroll metrics")
            .offset_from_bottom,
        6
    );

    apply_client_pane_input_events(
        &runtime,
        &[scroll(crate::protocol::ClientMouseKind::ScrollDown)],
    )
    .expect("scroll down");
    assert_eq!(
        runtime
            .scroll_metrics()
            .expect("scroll metrics")
            .offset_from_bottom,
        3
    );

    runtime.test_process_pty_bytes(b"\x1b[?1003h\x1b[?1006h");
    apply_client_pane_input_events(
        &runtime,
        &[crate::protocol::ClientPaneInputEvent::Mouse {
            kind: crate::protocol::ClientMouseKind::Moved,
            position: crate::protocol::ClientMousePosition::Cell { column: 2, row: 1 },
            geometry: None,
            modifiers: 0,
            lines: 3,
        }],
    )
    .expect("reported mouse motion");
    assert_eq!(
        runtime
            .scroll_metrics()
            .expect("scroll metrics")
            .offset_from_bottom,
        3
    );
    assert_eq!(
        input_rx.try_recv().expect("reported mouse motion"),
        Bytes::from_static(b"\x1b[<35;3;2M")
    );

    apply_client_pane_input_events(
        &runtime,
        &[crate::protocol::ClientPaneInputEvent::Mouse {
            kind: crate::protocol::ClientMouseKind::Down(crate::protocol::ClientMouseButton::Left),
            position: crate::protocol::ClientMousePosition::Cell { column: 2, row: 1 },
            geometry: None,
            modifiers: 0,
            lines: 3,
        }],
    )
    .expect("mouse button");
    assert_eq!(
        runtime
            .scroll_metrics()
            .expect("scroll metrics")
            .offset_from_bottom,
        0
    );
    assert_eq!(
        input_rx.try_recv().expect("reported mouse button"),
        Bytes::from_static(b"\x1b[<0;3;2M")
    );
    drop(runtime);
    drop(_runtime_guard);
    rt.shutdown_timeout(Duration::from_millis(100));
}

#[test]
fn terminal_attach_input_resets_scrolled_viewport() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    let _runtime_guard = rt.enter();
    let mut bytes = Vec::new();
    for line in 0..80 {
        bytes.extend_from_slice(format!("line {line:02}\r\n").as_bytes());
    }
    let (runtime, mut input_rx) =
        crate::terminal::TerminalRuntime::test_with_channel_and_scrollback_bytes(
            20, 5, 4096, &bytes, 4,
        );

    runtime.scroll_up(4);
    assert_eq!(
        runtime
            .scroll_metrics()
            .expect("scroll metrics")
            .offset_from_bottom,
        4
    );

    apply_terminal_attach_input(&runtime, b"x".to_vec()).expect("attach input");
    assert_eq!(
        runtime
            .scroll_metrics()
            .expect("scroll metrics")
            .offset_from_bottom,
        0
    );
    assert_eq!(
        input_rx.try_recv().expect("forwarded input"),
        Bytes::from("x")
    );

    drop(runtime);
    drop(_runtime_guard);
    rt.shutdown_timeout(Duration::from_millis(100));
}

fn with_terminal_attach_runtime(
    initial_bytes: &[u8],
    initial_scroll: usize,
    test: impl FnOnce(&crate::terminal::TerminalRuntime, &mut mpsc::Receiver<Bytes>),
) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    let _runtime_guard = rt.enter();
    let mut bytes = initial_bytes.to_vec();
    for line in 0..80 {
        bytes.extend_from_slice(format!("line {line:02}\r\n").as_bytes());
    }
    let (runtime, mut input_rx) =
        crate::terminal::TerminalRuntime::test_with_channel_and_scrollback_bytes(
            20, 5, 4096, &bytes, 4,
        );
    if initial_scroll > 0 {
        runtime.scroll_up(initial_scroll);
    }

    test(&runtime, &mut input_rx);

    drop(runtime);
    drop(_runtime_guard);
    rt.shutdown_timeout(Duration::from_millis(100));
}

fn apply_terminal_attach_page_up(runtime: &crate::terminal::TerminalRuntime) {
    apply_terminal_attach_scroll(
        runtime,
        AttachScrollSource::PageKey {
            input: b"\x1b[5~".to_vec(),
        },
        AttachScrollDirection::Up,
        4,
        None,
        None,
        0,
    )
    .expect("page key");
}

fn client_page_key(
    code: crate::protocol::ClientKeyCode,
    modifiers: crossterm::event::KeyModifiers,
    kind: crate::protocol::ClientKeyKind,
) -> crate::protocol::ClientPaneInputEvent {
    crate::protocol::ClientPaneInputEvent::Key {
        code,
        modifiers: modifiers.bits(),
        kind,
        repeat_count: 1,
        shifted_codepoint: None,
        generated_text: None,
        tracks_release: true,
        physical_key_id: None,
        windows_record: None,
    }
}

#[test]
fn client_plain_page_keys_scroll_shell_transcript_by_pane_height() {
    with_terminal_attach_runtime(b"", 0, |runtime, input_rx| {
        apply_client_pane_input_events(
            runtime,
            &[client_page_key(
                crate::protocol::ClientKeyCode::PageUp,
                crossterm::event::KeyModifiers::empty(),
                crate::protocol::ClientKeyKind::Press,
            )],
        )
        .expect("pane PageUp");
        assert_eq!(
            runtime
                .scroll_metrics()
                .expect("scroll metrics")
                .offset_from_bottom,
            5
        );

        apply_client_pane_input_events(
            runtime,
            &[client_page_key(
                crate::protocol::ClientKeyCode::PageUp,
                crossterm::event::KeyModifiers::empty(),
                crate::protocol::ClientKeyKind::Release,
            )],
        )
        .expect("pane PageUp release");
        assert_eq!(
            runtime
                .scroll_metrics()
                .expect("scroll metrics")
                .offset_from_bottom,
            5
        );

        apply_client_pane_input_events(
            runtime,
            &[client_page_key(
                crate::protocol::ClientKeyCode::PageDown,
                crossterm::event::KeyModifiers::empty(),
                crate::protocol::ClientKeyKind::Press,
            )],
        )
        .expect("pane PageDown");
        assert_eq!(
            runtime
                .scroll_metrics()
                .expect("scroll metrics")
                .offset_from_bottom,
            0
        );
        assert!(input_rx.try_recv().is_err(), "page keys reached the shell");
    });
}

#[test]
fn client_page_keys_forward_when_modified_or_owned_by_application() {
    with_terminal_attach_runtime(b"", 0, |runtime, input_rx| {
        apply_client_pane_input_events(
            runtime,
            &[client_page_key(
                crate::protocol::ClientKeyCode::PageUp,
                crossterm::event::KeyModifiers::CONTROL,
                crate::protocol::ClientKeyKind::Press,
            )],
        )
        .expect("modified pane PageUp");
        assert!(
            input_rx.try_recv().is_ok(),
            "modified PageUp was not forwarded"
        );
        assert_eq!(
            runtime
                .scroll_metrics()
                .expect("scroll metrics")
                .offset_from_bottom,
            0
        );
    });

    with_terminal_attach_runtime(b"\x1b[?1h", 0, |runtime, input_rx| {
        apply_client_pane_input_events(
            runtime,
            &[client_page_key(
                crate::protocol::ClientKeyCode::PageUp,
                crossterm::event::KeyModifiers::empty(),
                crate::protocol::ClientKeyKind::Press,
            )],
        )
        .expect("application PageUp");
        assert_eq!(
            input_rx.try_recv().expect("forwarded application PageUp"),
            Bytes::from_static(b"\x1b[5~")
        );
        assert_eq!(
            runtime
                .scroll_metrics()
                .expect("scroll metrics")
                .offset_from_bottom,
            0
        );
    });
}

#[test]
fn client_popup_plain_page_key_remains_popup_input() {
    with_terminal_attach_runtime(b"", 0, |runtime, input_rx| {
        apply_client_popup_input_events(
            runtime,
            &[client_page_key(
                crate::protocol::ClientKeyCode::PageUp,
                crossterm::event::KeyModifiers::empty(),
                crate::protocol::ClientKeyKind::Press,
            )],
        )
        .expect("popup PageUp");
        assert_eq!(
            input_rx.try_recv().expect("forwarded popup PageUp"),
            Bytes::from_static(b"\x1b[5~")
        );
        assert_eq!(
            runtime
                .scroll_metrics()
                .expect("scroll metrics")
                .offset_from_bottom,
            0
        );
    });
}

#[test]
fn terminal_attach_paste_uses_plain_text_when_runtime_did_not_enable_brackets() {
    with_terminal_attach_runtime(b"", 0, |runtime, input_rx| {
        apply_terminal_attach_input(runtime, b"\x1b[200~line one\nline two\x1b[201~".to_vec())
            .expect("attach paste");

        assert_eq!(
            input_rx.try_recv().expect("forwarded paste"),
            Bytes::from_static(if cfg!(windows) {
                b"line one\r\nline two"
            } else {
                b"line one\nline two"
            })
        );
    });
}

#[test]
fn terminal_attach_paste_preserves_brackets_when_runtime_enabled_them() {
    with_terminal_attach_runtime(b"\x1b[?2004h", 0, |runtime, input_rx| {
        apply_terminal_attach_input(runtime, b"\x1b[200~line one\nline two\x1b[201~".to_vec())
            .expect("attach paste");

        assert_eq!(
            input_rx.try_recv().expect("forwarded paste"),
            Bytes::from_static(if cfg!(windows) {
                b"\x1b[200~line one\r\nline two\x1b[201~"
            } else {
                b"\x1b[200~line one\nline two\x1b[201~"
            })
        );
    });
}

#[test]
fn terminal_attach_page_key_host_scrolls_plain_terminal() {
    with_terminal_attach_runtime(b"", 0, |runtime, input_rx| {
        apply_terminal_attach_page_up(runtime);

        assert_eq!(
            runtime
                .scroll_metrics()
                .expect("scroll metrics")
                .offset_from_bottom,
            4
        );
        assert!(input_rx.try_recv().is_err());
    });
}

#[test]
fn terminal_attach_page_key_forwards_when_mouse_reporting() {
    with_terminal_attach_runtime(b"\x1b[?1000h", 3, |runtime, input_rx| {
        apply_terminal_attach_page_up(runtime);

        assert_eq!(
            runtime
                .scroll_metrics()
                .expect("scroll metrics")
                .offset_from_bottom,
            0
        );
        assert_eq!(
            input_rx.try_recv().expect("forwarded page key"),
            Bytes::from_static(b"\x1b[5~")
        );
    });
}

#[test]
fn terminal_attach_page_key_forwards_when_application_cursor() {
    with_terminal_attach_runtime(b"\x1b[?1h", 3, |runtime, input_rx| {
        apply_terminal_attach_page_up(runtime);

        assert_eq!(
            runtime
                .scroll_metrics()
                .expect("scroll metrics")
                .offset_from_bottom,
            0
        );
        assert_eq!(
            input_rx.try_recv().expect("forwarded page key"),
            Bytes::from_static(b"\x1b[5~")
        );
    });
}

#[test]
fn terminal_attach_page_key_host_scrolls_shell_like_decckm_with_bracketed_paste() {
    with_terminal_attach_runtime(b"\x1b[?1h\x1b[?2004h", 0, |runtime, input_rx| {
        apply_terminal_attach_page_up(runtime);

        assert_eq!(
            runtime
                .scroll_metrics()
                .expect("scroll metrics")
                .offset_from_bottom,
            4
        );
        assert!(input_rx.try_recv().is_err());
    });
}

#[test]
fn terminal_attach_page_key_forwards_in_alternate_screen_without_mouse_reporting() {
    with_terminal_attach_runtime(b"\x1b[?1049h", 3, |runtime, input_rx| {
        apply_terminal_attach_page_up(runtime);

        assert_eq!(
            runtime
                .scroll_metrics()
                .expect("scroll metrics")
                .offset_from_bottom,
            0
        );
        assert_eq!(
            input_rx.try_recv().expect("forwarded page key"),
            Bytes::from_static(b"\x1b[5~")
        );
    });
}

#[test]
fn headless_scheduled_tasks_expire_agent_metadata() {
    let mut server = test_headless_server();
    let workspace = crate::workspace::Workspace::test_new("metadata");
    let pane_id = workspace.tabs[0].root_pane;
    server.app.state.workspaces = vec![workspace];
    server.app.state.ensure_test_terminals();

    assert!(
        server.handle_internal_event_with_forwarding(AppEvent::HookStateReported {
            pane_id,
            source: "custom:pi".into(),
            agent_label: "pi".into(),
            state: crate::detect::AgentState::Working,
            message: None,
            seq: None,
            session_ref: None,
        })
    );
    assert!(
        server.handle_internal_event_with_forwarding(AppEvent::HookMetadataReported {
            pane_id,
            source: "user:pi-display".into(),
            agent_label: Some("pi".into()),
            applies_to_source: Some("custom:pi".into()),
            title: Some("short lived".into()),
            display_agent: None,
            state_labels: HashMap::new(),
            clear_title: false,
            clear_display_agent: false,
            clear_state_labels: false,
            seq: None,
            // Expiry is advanced with the captured deadline below; keep the
            // pre-expiry assertion independent of wall-clock scheduling.
            ttl: Some(Duration::from_secs(60)),
        })
    );

    let deadline = server
        .app
        .agent_metadata_deadline
        .expect("metadata deadline");
    let terminal_id = server.app.state.workspaces[0]
        .pane_state(pane_id)
        .expect("pane")
        .attached_terminal_id
        .clone();
    assert_eq!(
        server
            .app
            .state
            .terminals
            .get(&terminal_id)
            .expect("terminal")
            .effective_title()
            .as_deref(),
        Some("short lived")
    );

    assert!(server.handle_scheduled_tasks_headless(deadline + Duration::from_millis(1), false));

    assert_eq!(server.app.agent_metadata_deadline, None);
    assert_eq!(
        server
            .app
            .state
            .terminals
            .get(&terminal_id)
            .expect("terminal")
            .effective_title(),
        None
    );
    assert!(server
        .app
        .event_hub
        .events_after(0)
        .iter()
        .any(|(_, event)| {
            event.event == crate::api::schema::EventKind::PaneAgentStatusChanged
                && matches!(
                    &event.data,
                    crate::api::schema::EventData::PaneAgentStatusChanged {
                        title,
                        ..
                    } if title.is_none()
                )
        }));
}

// FM-PERF-TEXT-11: the server-owned runtime must pump the same bounded
// text-preview worker as monolithic App::run. A file click is intentionally
// disk-free, so omitting this adapter would leave headless Files stuck in
// PendingText forever.
#[test]
fn headless_scheduled_tasks_pump_pending_text_preview_worker() {
    struct RemoveOnDrop(PathBuf);

    impl Drop for RemoveOnDrop {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    let root = std::env::temp_dir().join(format!(
        "headless-text-preview-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    let _cleanup = RemoveOnDrop(root.clone());
    fs::create_dir_all(&root).expect("create headless preview fixture");
    let path = root.join("clicked.rs");
    fs::write(&path, "fn headless_preview() {}\n").expect("write preview fixture");

    let mut server = test_headless_server();
    server
        .app
        .state
        .try_open_file_manager_with(|_| Some(crate::fm::FmState::new(&root)))
        .expect("open Files instance");
    let file_manager = server.app.state.file_manager.as_mut().expect("open Files");
    let entry_index = file_manager.trail_snapshots.cols()[0]
        .entries()
        .iter()
        .position(|entry| entry.path == path)
        .expect("resident file row");
    assert_eq!(
        file_manager.activate_trail_entry(0, entry_index, &path),
        crate::fm::trail_snapshots::TrailActivateOutcome::SelectedFile
    );
    assert!(matches!(
        &file_manager.preview,
        crate::fm::FmPreview::File(crate::fm::FmFilePreview::PendingText {
            source_path,
            ..
        }) if source_path == &path
    ));

    let (_, profile) = crate::render_prof::observe_for_test(|| {
        let _ = server.handle_scheduled_tasks_headless(Instant::now(), false);
    });
    assert_eq!(
        profile.counter("fm.text_worker.submitted"),
        1,
        "headless scheduling must submit the pending exact-path preview"
    );

    let wait = LoadAwareDeadline::new(5, "the headless text preview to be applied");
    loop {
        let _ = server.handle_scheduled_tasks_headless(Instant::now(), false);
        let resolved = server
            .app
            .state
            .file_manager
            .as_ref()
            .is_some_and(|file_manager| {
                matches!(
                    &file_manager.preview,
                    crate::fm::FmPreview::File(crate::fm::FmFilePreview::Text(preview))
                        if preview.source_path == path
                            && preview.content == "fn headless_preview() {}\n"
                            && preview.highlighted.is_some()
                )
            });
        if resolved {
            break;
        }
        wait.check();
        std::thread::yield_now();
    }
}

#[test]
fn headless_scheduled_tasks_clears_disabled_agent_manifest_update_deadline() {
    let mut server = test_headless_server();
    let now = Instant::now();
    server.app.next_agent_manifest_update_check = Some(now - Duration::from_millis(1));

    assert!(!server.handle_scheduled_tasks_headless(now, false));
    assert_eq!(server.app.next_agent_manifest_update_check, None);
}

#[tokio::test]
async fn headless_scheduled_tasks_do_not_start_pending_agent_resume_when_geometry_dirty() {
    let mut server = test_headless_server();
    let workspace = crate::workspace::Workspace::test_new("restored");
    let pane_id = workspace.tabs[0].root_pane;
    let terminal_id = workspace.terminal_id(pane_id).cloned().unwrap();
    server.app.state.view.pane_infos = workspace.tabs[0]
        .layout
        .panes(ratatui::layout::Rect::new(0, 0, 100, 30));
    server.app.state.workspaces = vec![workspace];
    server.app.state.active = Some(0);
    server.app.state.ensure_test_terminals();
    server.clients.insert(
        1,
        ClientConnection::app(
            (100, 30),
            crate::kitty_graphics::HostCellSize::default(),
            server.app.state.host_terminal_theme,
            Some(true),
            1,
            RenderEncoding::SemanticFrame,
            None,
        ),
    );
    server.foreground_client_id = Some(1);
    server.effective_size = (100, 30);
    server.app.state.host_terminal_theme = crate::terminal_theme::TerminalTheme {
        foreground: Some(crate::terminal_theme::RgbColor {
            r: 220,
            g: 220,
            b: 220,
        }),
        background: Some(crate::terminal_theme::RgbColor {
            r: 20,
            g: 20,
            b: 20,
        }),
        ..Default::default()
    };
    server
        .app
        .state
        .terminals
        .get_mut(&terminal_id)
        .expect("test terminal should exist")
        .pending_agent_resume_plan = Some(crate::agent_resume::AgentResumePlan {
        agent: "codex".into(),
        argv: vec!["/bin/sh".into(), "-c".into(), "sleep 5".into()],
        dedupe_key: "herdr:codex\0codex\0Id\0codex-session".into(),
    });
    server.app.pending_agent_resume_deadline = Some(Instant::now() - Duration::from_millis(1));

    assert!(!server.handle_scheduled_tasks_headless(Instant::now(), true));
    assert!(server.app.terminal_runtimes.get(&terminal_id).is_none());
    assert!(server
        .app
        .state
        .terminals
        .get(&terminal_id)
        .expect("test terminal should still exist")
        .pending_agent_resume_plan
        .is_some());
    assert!(server.app.pending_agent_resume_deadline.is_none());
}

#[cfg(unix)]
#[tokio::test]
async fn headless_scheduled_tasks_start_pending_agent_resume_without_foreground_client() {
    let mut server = test_headless_server();
    let workspace = crate::workspace::Workspace::test_new("restored");
    let pane_id = workspace.tabs[0].root_pane;
    let terminal_id = workspace.terminal_id(pane_id).cloned().unwrap();
    server.app.state.workspaces = vec![workspace];
    server.app.state.active = Some(0);
    server.app.state.ensure_test_terminals();
    server
        .app
        .state
        .terminals
        .get_mut(&terminal_id)
        .expect("test terminal should exist")
        .pending_agent_resume_plan = Some(crate::agent_resume::AgentResumePlan {
        agent: "codex".into(),
        argv: vec!["/bin/sh".into(), "-c".into(), "sleep 5".into()],
        dedupe_key: "herdr:codex\0codex\0Id\0codex-session".into(),
    });

    server.render_and_stream();
    assert_ne!(server.app.state.view.terminal_area, Rect::default());

    let now = Instant::now();
    assert!(!server.handle_scheduled_tasks_headless(now, false));
    assert!(server.app.terminal_runtimes.get(&terminal_id).is_none());
    let deadline = server
        .app
        .pending_agent_resume_deadline
        .expect("clientless resume should wait briefly for a host theme");

    assert!(server.handle_scheduled_tasks_headless(deadline, false));
    assert!(server.app.terminal_runtimes.get(&terminal_id).is_some());
    assert!(server
        .app
        .state
        .terminals
        .get(&terminal_id)
        .expect("test terminal should still exist")
        .pending_agent_resume_plan
        .is_none());
    shutdown_test_runtimes(&mut server);
}

#[tokio::test]
async fn headless_pre_input_resize_does_not_start_pending_agent_resume() {
    let mut server = test_headless_server();
    let workspace = crate::workspace::Workspace::test_new("restored");
    let pane_id = workspace.tabs[0].root_pane;
    let terminal_id = workspace.terminal_id(pane_id).cloned().unwrap();
    server.app.state.view.pane_infos = workspace.tabs[0]
        .layout
        .panes(ratatui::layout::Rect::new(0, 0, 100, 30));
    server.app.state.workspaces = vec![workspace];
    server.app.state.active = Some(0);
    server.app.state.ensure_test_terminals();
    server.clients.insert(
        1,
        ClientConnection::app(
            (100, 30),
            crate::kitty_graphics::HostCellSize::default(),
            server.app.state.host_terminal_theme,
            Some(true),
            1,
            RenderEncoding::SemanticFrame,
            None,
        ),
    );
    server.foreground_client_id = Some(1);
    server.effective_size = (100, 30);
    server.app.state.host_terminal_theme = crate::terminal_theme::TerminalTheme {
        foreground: Some(crate::terminal_theme::RgbColor {
            r: 220,
            g: 220,
            b: 220,
        }),
        background: Some(crate::terminal_theme::RgbColor {
            r: 20,
            g: 20,
            b: 20,
        }),
        ..Default::default()
    };
    server
        .app
        .state
        .terminals
        .get_mut(&terminal_id)
        .expect("test terminal should exist")
        .pending_agent_resume_plan = Some(crate::agent_resume::AgentResumePlan {
        agent: "codex".into(),
        argv: vec!["/bin/sh".into(), "-c".into(), "sleep 5".into()],
        dedupe_key: "herdr:codex\0codex\0Id\0codex-session".into(),
    });
    server.app.pending_agent_resume_deadline = Some(Instant::now() - Duration::from_millis(1));

    server.resize_shared_runtime_to_effective_size_before_input();

    assert!(server.app.terminal_runtimes.get(&terminal_id).is_none());
    assert!(server
        .app
        .state
        .terminals
        .get(&terminal_id)
        .expect("test terminal should still exist")
        .pending_agent_resume_plan
        .is_some());
    assert!(server.app.pending_agent_resume_deadline.is_none());
}

#[test]
fn terminal_attach_resize_uses_known_cell_geometry_without_pixel_mouse() {
    with_terminal_session_test_server(|server, _other_terminal_id, terminal_id, _pane_id| {
        let mut client = ClientConnection::new(
            (80, 24),
            crate::kitty_graphics::HostCellSize::default(),
            1,
            RenderEncoding::SemanticFrame,
            None,
        );
        client.mode = ClientConnectionMode::TerminalAttach {
            terminal_id: terminal_id.clone(),
        };
        server.clients.insert(1, client);

        assert!(server.handle_server_event(ServerEvent::ClientResize {
            client_id: 1,
            cols: 100,
            rows: 30,
            cell_width_px: 8,
            cell_height_px: 16,
            pixel_mouse: false,
        }));

        assert_eq!(
            server
                .runtime_for_terminal_id_string(&terminal_id)
                .unwrap()
                .pixel_size(),
            Some((800, 480))
        );
        assert_eq!(
            server.clients[&1].cell_size,
            crate::kitty_graphics::HostCellSize {
                width_px: 8,
                height_px: 16,
            }
        );
        assert!(!server.clients[&1].pixel_mouse);

        assert!(server.handle_server_event(ServerEvent::ClientResize {
            client_id: 1,
            cols: 100,
            rows: 30,
            cell_width_px: 0,
            cell_height_px: 0,
            pixel_mouse: false,
        }));
        assert_eq!(
            server
                .runtime_for_terminal_id_string(&terminal_id)
                .unwrap()
                .pixel_size(),
            Some((800, 480))
        );
        assert_eq!(
            server.clients[&1].cell_size,
            crate::kitty_graphics::HostCellSize {
                width_px: 8,
                height_px: 16,
            }
        );
        assert!(!server.clients[&1].pixel_mouse);
    });
}

#[test]
fn pending_terminal_resize_does_not_take_shell_foreground_or_geometry() {
    let mut server = test_headless_server();
    server.clients.insert(
        1,
        ClientConnection::new(
            (100, 30),
            crate::kitty_graphics::HostCellSize::default(),
            2,
            RenderEncoding::SemanticFrame,
            None,
        ),
    );
    server.clients.insert(
        2,
        ClientConnection::new_with_mode(
            ClientConnectionMode::TerminalPending,
            (80, 24),
            crate::kitty_graphics::HostCellSize::default(),
            1,
            RenderEncoding::TerminalAnsi,
            None,
        ),
    );
    server.foreground_client_id = Some(1);
    server.sync_foreground_client_state();
    let shell_size = server.effective_size;

    assert!(server.handle_server_event(ServerEvent::ClientResize {
        client_id: 2,
        cols: 200,
        rows: 60,
        cell_width_px: 10,
        cell_height_px: 20,
        pixel_mouse: false,
    }));

    assert_eq!(server.foreground_client_id, Some(1));
    assert_eq!(server.effective_size, shell_size);
    assert_eq!(server.clients[&2].terminal_size, (200, 60));
}

#[test]
fn client_shell_streams_focused_pane_report_all_demand() {
    with_terminal_session_test_server(|server, _other_terminal_id, terminal_id, _pane_id| {
        let (client_tx, client_control_rx, _client_rx) = test_client_writer();
        server.clients.insert(
            1,
            ClientConnection::new(
                (80, 24),
                crate::kitty_graphics::HostCellSize::default(),
                1,
                RenderEncoding::SemanticFrame,
                Some(client_tx),
            ),
        );
        server.app.state.active = Some(0);
        server
            .runtime_for_terminal_id_string(&terminal_id)
            .expect("focused runtime")
            .test_process_pty_bytes(b"\x1b[>15u");

        server.stream_direct_terminal_keyboard_mode();

        assert!(matches!(
            read_server_message(
                client_control_rx
                    .recv_timeout(Duration::from_millis(100))
                    .expect("shell keyboard mode message")
            ),
            ServerMessage::ClientShellKeyboardReportAll { enabled: true }
        ));
    });
}

#[tokio::test]
async fn client_shell_release_cleanup_does_not_promote_and_survives_disconnect() {
    let mut server = test_headless_server();
    let mut input_rx = install_focused_test_runtime(&mut server, b"\x1b[>3u");
    let pane_id = server.app.session_snapshot().focused_pane_id.unwrap();
    for client_id in [1, 2] {
        server.clients.insert(
            client_id,
            ClientConnection::new(
                (80, 24),
                crate::kitty_graphics::HostCellSize::default(),
                client_id,
                RenderEncoding::SemanticFrame,
                None,
            ),
        );
    }
    let key = |kind| crate::protocol::ClientPaneInputEvent::Key {
        code: crate::protocol::ClientKeyCode::Char('x'),
        modifiers: 0,
        kind,
        repeat_count: 1,
        shifted_codepoint: None,
        generated_text: (kind == crate::protocol::ClientKeyKind::Press).then(|| "x".to_owned()),
        tracks_release: true,
        physical_key_id: Some(0x2d),
        windows_record: None,
    };

    assert!(
        server.handle_server_event(ServerEvent::ClientShellPaneInput {
            client_id: 1,
            pane_id: pane_id.clone(),
            events: vec![key(crate::protocol::ClientKeyKind::Press)],
        })
    );
    assert!(!input_rx.recv().await.expect("encoded press").is_empty());
    assert!(server.promote_client_to_foreground(2));

    assert!(
        !server.handle_server_event(ServerEvent::ClientShellPaneInput {
            client_id: 1,
            pane_id: pane_id.clone(),
            events: vec![key(crate::protocol::ClientKeyKind::Release)],
        })
    );
    assert!(!input_rx.recv().await.expect("encoded release").is_empty());
    assert_eq!(server.foreground_client_id, Some(2));

    assert!(
        server.handle_server_event(ServerEvent::ClientShellPaneInput {
            client_id: 1,
            pane_id,
            events: vec![key(crate::protocol::ClientKeyKind::Press)],
        })
    );
    assert!(!input_rx
        .recv()
        .await
        .expect("second encoded press")
        .is_empty());
    assert!(server.handle_server_event(ServerEvent::ClientDisconnected { client_id: 1 }));
    assert!(!input_rx
        .recv()
        .await
        .expect("disconnect synthesized release")
        .is_empty());
    shutdown_test_runtimes(&mut server);
}

#[test]
fn client_shell_mouse_capture_combines_local_preference_with_endpoint_demand() {
    let mut server = test_headless_server();
    let (writer, control_rx, _render_rx) = test_client_writer();
    server.clients.insert(
        1,
        ClientConnection::new(
            (80, 24),
            crate::kitty_graphics::HostCellSize::default(),
            1,
            RenderEncoding::SemanticFrame,
            Some(writer),
        ),
    );

    server.stream_host_mouse_capture_mode();
    assert!(matches!(
        read_server_message(control_rx.recv().expect("initial mouse mode")),
        ServerMessage::MouseCapture {
            enabled: false,
            sgr_pixels: false
        }
    ));
    assert!(
        server.handle_server_event(ServerEvent::ClientShellMouseCapture {
            client_id: 1,
            enabled: true,
        })
    );
    server.stream_host_mouse_capture_mode();
    assert!(matches!(
        read_server_message(control_rx.recv().expect("preferred mouse mode")),
        ServerMessage::MouseCapture {
            enabled: true,
            sgr_pixels: false
        }
    ));
}

#[test]
fn client_shell_focus_promotes_and_reaches_reporting_pane() {
    with_terminal_session_test_server(|server, terminal_id, _other_terminal_id, _pane_id| {
        let (runtime, mut input_rx) =
            crate::terminal::TerminalRuntime::test_with_channel_and_scrollback_bytes(
                80,
                24,
                0,
                b"\x1b[?1004h",
                4,
            );
        server
            .app
            .terminal_runtimes
            .insert(terminal_id.clone(), runtime);
        server.app.state.active = Some(0);
        server.clients.insert(
            1,
            ClientConnection::new(
                (80, 24),
                crate::kitty_graphics::HostCellSize::default(),
                1,
                RenderEncoding::SemanticFrame,
                None,
            ),
        );
        server.clients.insert(
            2,
            ClientConnection::new(
                (100, 30),
                crate::kitty_graphics::HostCellSize::default(),
                2,
                RenderEncoding::SemanticFrame,
                None,
            ),
        );
        server.foreground_client_id = Some(2);
        server.sync_foreground_client_state();
        assert!(server.claim_unowned_shell_tab_geometry(2, true));
        assert_eq!(
            server
                .app
                .terminal_runtimes
                .get(&terminal_id)
                .expect("focused runtime")
                .current_size(),
            (30, 99)
        );

        assert!(server.handle_server_event(ServerEvent::ClientShellFocus {
            client_id: 1,
            focused: true,
        }));
        assert_eq!(server.foreground_client_id, Some(1));
        assert_eq!(server.app.state.outer_terminal_focus, Some(true));
        assert_eq!(
            server
                .app
                .terminal_runtimes
                .get(&terminal_id)
                .expect("focused runtime")
                .current_size(),
            (24, 79)
        );
        assert_eq!(
            input_rx.try_recv().expect("focus gained input"),
            Bytes::from_static(b"\x1b[I")
        );

        assert!(server.handle_server_event(ServerEvent::ClientShellFocus {
            client_id: 2,
            focused: true,
        }));
        assert!(
            input_rx.try_recv().is_err(),
            "second viewer duplicated focus gain"
        );
        assert!(server.handle_server_event(ServerEvent::ClientShellFocus {
            client_id: 1,
            focused: false,
        }));
        assert!(
            input_rx.try_recv().is_err(),
            "remaining viewer lost tab focus"
        );
        assert!(server.handle_server_event(ServerEvent::ClientShellFocus {
            client_id: 2,
            focused: false,
        }));
        assert_eq!(server.app.state.outer_terminal_focus, Some(false));
        assert_eq!(
            input_rx.try_recv().expect("last viewer focus lost input"),
            Bytes::from_static(b"\x1b[O")
        );
    });
}

#[test]
fn direct_terminal_streams_child_keyboard_and_mouse_modes() {
    with_terminal_session_test_server(|server, _other_terminal_id, terminal_id, _pane_id| {
        let (client_tx, client_control_rx, _client_rx) = test_client_writer();
        server.clients.insert(
            1,
            ClientConnection::new_with_mode(
                ClientConnectionMode::TerminalAttach {
                    terminal_id: terminal_id.clone(),
                },
                (80, 24),
                crate::kitty_graphics::HostCellSize::default(),
                1,
                RenderEncoding::TerminalAnsi,
                Some(client_tx),
            ),
        );
        server
            .clients
            .get_mut(&1)
            .expect("direct attach client")
            .pixel_mouse = true;
        server
            .runtime_for_terminal_id_string(&terminal_id)
            .expect("attached runtime")
            .test_process_pty_bytes(b"\x1b[>15u\x1b[?1000h");

        server.stream_direct_terminal_keyboard_mode();
        assert!(matches!(
            read_server_message(
                client_control_rx
                    .recv_timeout(Duration::from_millis(100))
                    .expect("keyboard mode message")
            ),
            ServerMessage::DirectTerminalKeyboardProtocol {
                flags: 15,
                modify_other_keys_level: 0
            }
        ));

        server
            .runtime_for_terminal_id_string(&terminal_id)
            .expect("attached runtime")
            .test_process_pty_bytes(b"\x1b[<u\x1b[>3u\x1b[>4;1m");
        server.stream_direct_terminal_keyboard_mode();
        assert!(matches!(
            read_server_message(
                client_control_rx
                    .recv_timeout(Duration::from_millis(100))
                    .expect("modifyOtherKeys mode-one keyboard message")
            ),
            ServerMessage::DirectTerminalKeyboardProtocol {
                flags: 3,
                modify_other_keys_level: 1
            }
        ));

        server
            .runtime_for_terminal_id_string(&terminal_id)
            .expect("attached runtime")
            .test_process_pty_bytes(b"\x1b[>4;2m");
        server.stream_direct_terminal_keyboard_mode();
        assert!(matches!(
            read_server_message(
                client_control_rx
                    .recv_timeout(Duration::from_millis(100))
                    .expect("modifyOtherKeys mode-two keyboard message")
            ),
            ServerMessage::DirectTerminalKeyboardProtocol {
                flags: 3,
                modify_other_keys_level: 2
            }
        ));

        server
            .runtime_for_terminal_id_string(&terminal_id)
            .expect("attached runtime")
            .test_process_pty_bytes(b"\x1b[<u");
        server.stream_direct_terminal_keyboard_mode();
        assert!(matches!(
            read_server_message(
                client_control_rx
                    .recv_timeout(Duration::from_millis(100))
                    .expect("modifyOtherKeys-only keyboard mode message")
            ),
            ServerMessage::DirectTerminalKeyboardProtocol {
                flags: 0,
                modify_other_keys_level: 2
            }
        ));

        server.stream_host_mouse_capture_mode();
        assert!(matches!(
            read_server_message(
                client_control_rx
                    .recv_timeout(Duration::from_millis(100))
                    .expect("mouse capture message")
            ),
            ServerMessage::MouseCapture {
                enabled: true,
                sgr_pixels: false
            }
        ));

        server
            .runtime_for_terminal_id_string(&terminal_id)
            .expect("attached runtime")
            .test_process_pty_bytes(b"\x1b[?1016h");
        server.stream_host_mouse_capture_mode();
        assert!(matches!(
            read_server_message(
                client_control_rx
                    .recv_timeout(Duration::from_millis(100))
                    .expect("pixel mouse capture message")
            ),
            ServerMessage::MouseCapture {
                enabled: true,
                sgr_pixels: true
            }
        ));

        server
            .runtime_for_terminal_id_string(&terminal_id)
            .expect("attached runtime")
            .test_process_pty_bytes(b"\x1b[?1000l\x1b[?1016l");
        server.stream_host_mouse_capture_mode();
        assert!(matches!(
            read_server_message(
                client_control_rx
                    .recv_timeout(Duration::from_millis(100))
                    .expect("child mouse disable message")
            ),
            ServerMessage::MouseCapture {
                enabled: false,
                sgr_pixels: false
            }
        ));
    });
}

#[test]
fn direct_terminal_mouse_uses_runtime_protocol_encoding() {
    with_terminal_session_test_server(|server, runtime_terminal_id, terminal_id, _pane_id| {
        let (runtime, mut input_rx) =
            crate::terminal::TerminalRuntime::test_with_channel_and_scrollback_bytes(
                80,
                24,
                0,
                b"\x1b[?1000h\x1b[?1006h",
                4,
            );
        server
            .app
            .terminal_runtimes
            .insert(runtime_terminal_id, runtime);
        server.clients.insert(
            1,
            ClientConnection::new_with_mode(
                ClientConnectionMode::TerminalAttach {
                    terminal_id: terminal_id.clone(),
                },
                (80, 24),
                crate::kitty_graphics::HostCellSize::default(),
                1,
                RenderEncoding::TerminalAnsi,
                None,
            ),
        );

        assert!(server.handle_server_event(ServerEvent::ClientAttachMouse {
            client_id: 1,
            kind: protocol::ClientMouseKind::Down(protocol::ClientMouseButton::Left),
            position: protocol::ClientMousePosition::Cell { column: 10, row: 5 },
            geometry: None,
            modifiers: 0,
            lines: 1,
        }));
        assert_eq!(
            input_rx.try_recv().expect("encoded direct mouse input"),
            Bytes::from_static(b"\x1b[<0;11;6M")
        );
    });
}

#[test]
fn direct_terminal_pixel_mouse_uses_runtime_tracking_and_coordinates() {
    with_terminal_session_test_server(|server, runtime_terminal_id, terminal_id, _pane_id| {
        let (runtime, mut input_rx) =
            crate::terminal::TerminalRuntime::test_with_channel_and_scrollback_bytes(
                80,
                24,
                0,
                b"\x1b[?1000h\x1b[?1006h\x1b[?1016h",
                4,
            );
        runtime.resize(24, 80, 10, 20);
        server
            .app
            .terminal_runtimes
            .insert(runtime_terminal_id, runtime);
        server.clients.insert(
            1,
            ClientConnection::new_with_mode(
                ClientConnectionMode::TerminalAttach {
                    terminal_id: terminal_id.clone(),
                },
                (80, 24),
                crate::kitty_graphics::HostCellSize {
                    width_px: 10,
                    height_px: 20,
                },
                1,
                RenderEncoding::TerminalAnsi,
                None,
            ),
        );
        let client = server.clients.get_mut(&1).expect("direct attach client");
        client.pixel_mouse = true;
        client.host_sgr_pixels_active = Some(true);

        assert!(!server.handle_server_event(ServerEvent::ClientAttachMouse {
            client_id: 1,
            kind: protocol::ClientMouseKind::Down(protocol::ClientMouseButton::Left),
            position: protocol::ClientMousePosition::Pixels {
                x: 21,
                y: 22,
                column: 3,
                row: 1,
            },
            geometry: Some(protocol::ClientMouseGeometry {
                cols: 80,
                rows: 24,
                width_px: 800,
                height_px: 480,
            }),
            modifiers: 0,
            lines: 1,
        }));
        assert!(input_rx.try_recv().is_err());

        assert!(server.handle_server_event(ServerEvent::ClientAttachMouse {
            client_id: 1,
            kind: protocol::ClientMouseKind::Down(protocol::ClientMouseButton::Left),
            position: protocol::ClientMousePosition::Pixels {
                x: 21,
                y: 22,
                column: 2,
                row: 1,
            },
            geometry: Some(protocol::ClientMouseGeometry {
                cols: 80,
                rows: 24,
                width_px: 805,
                height_px: 485,
            }),
            modifiers: 0,
            lines: 1,
        }));
        assert_eq!(
            input_rx
                .try_recv()
                .expect("proportionally mapped direct pixel mouse input"),
            Bytes::from_static(b"\x1b[<0;21;22M")
        );

        assert!(server.handle_server_event(ServerEvent::ClientAttachMouse {
            client_id: 1,
            kind: protocol::ClientMouseKind::Moved,
            position: protocol::ClientMousePosition::Pixels {
                x: 21,
                y: 22,
                column: 2,
                row: 1,
            },
            geometry: Some(protocol::ClientMouseGeometry {
                cols: 80,
                rows: 24,
                width_px: 800,
                height_px: 480,
            }),
            modifiers: 0,
            lines: 1,
        }));
        assert!(input_rx.try_recv().is_err());

        assert!(server.handle_server_event(ServerEvent::ClientAttachMouse {
            client_id: 1,
            kind: protocol::ClientMouseKind::Down(protocol::ClientMouseButton::Left),
            position: protocol::ClientMousePosition::Pixels {
                x: 21,
                y: 22,
                column: 2,
                row: 1,
            },
            geometry: Some(protocol::ClientMouseGeometry {
                cols: 80,
                rows: 24,
                width_px: 800,
                height_px: 480,
            }),
            modifiers: 0,
            lines: 1,
        }));
        assert_eq!(
            input_rx
                .try_recv()
                .expect("encoded direct pixel mouse input"),
            Bytes::from_static(b"\x1b[<0;21;22M")
        );
    });
}

#[test]
fn virtual_render_produces_nonempty_buffer() {
    let mut state = AppState::test_new();
    let area = Rect::new(0, 0, 80, 24);
    let (buffer, _cursor) = crate::server::render_stream::render_virtual(&mut state, area, true);
    assert_eq!(buffer.area.width, 80);
    assert_eq!(buffer.area.height, 24);
}

#[test]
fn virtual_render_without_frame_cursor_keeps_cursor_hidden() {
    let mut state = AppState::test_new();
    let area = Rect::new(0, 0, 80, 24);
    let (_buffer, cursor) = crate::server::render_stream::render_virtual(&mut state, area, true);

    assert_eq!(cursor, None);
}

#[tokio::test]
async fn virtual_render_preserves_explicit_frame_cursor_position() {
    let mut state = AppState::test_new();
    let mut ws = crate::workspace::Workspace::test_new("test");
    let pane_id = ws.tabs[0].root_pane;
    ws.insert_test_runtime(
        pane_id,
        crate::terminal::TerminalRuntime::test_with_screen_bytes(20, 5, b"left"),
    );

    state.workspaces = vec![ws];
    state.active = Some(0);
    state.selected = 0;
    state.mode = crate::app::Mode::Terminal;

    let area = Rect::new(0, 0, 80, 24);
    let (_buffer, cursor) = crate::server::render_stream::render_virtual(&mut state, area, true);
    let pane = state
        .view
        .pane_infos
        .iter()
        .find(|info| info.id == pane_id)
        .expect("focused pane info");

    assert_eq!(
        cursor,
        Some(CursorState {
            x: pane.inner_rect.x + 4,
            y: pane.inner_rect.y,
            visible: true,
            shape: cursor.as_ref().map(|c| c.shape).unwrap_or(0),
        })
    );
}

#[tokio::test]
async fn virtual_render_preserves_hidden_focused_pane_cursor_position() {
    let mut state = AppState::test_new();
    let mut ws = crate::workspace::Workspace::test_new("test");
    let pane_id = ws.tabs[0].root_pane;
    ws.insert_test_runtime(
        pane_id,
        crate::terminal::TerminalRuntime::test_with_screen_bytes(20, 5, b"left\x1b[?25l"),
    );

    state.workspaces = vec![ws];
    state.active = Some(0);
    state.selected = 0;
    state.mode = crate::app::Mode::Terminal;

    let area = Rect::new(0, 0, 80, 24);
    let (_buffer, cursor) = crate::server::render_stream::render_virtual(&mut state, area, true);
    let pane = state
        .view
        .pane_infos
        .iter()
        .find(|info| info.id == pane_id)
        .expect("focused pane info");

    assert_eq!(
        cursor,
        Some(CursorState {
            x: pane.inner_rect.x + 4,
            y: pane.inner_rect.y,
            visible: false,
            shape: cursor.as_ref().map(|c| c.shape).unwrap_or(0),
        })
    );
}

#[tokio::test]
async fn virtual_render_hides_focused_pane_cursor_during_synchronized_output() {
    let mut state = AppState::test_new();
    state.reveal_hidden_cursor_for_cjk_ime = true;
    let mut ws = crate::workspace::Workspace::test_new("test");
    let pane_id = ws.tabs[0].root_pane;
    let runtime = crate::terminal::TerminalRuntime::test_with_screen_bytes(20, 5, b"left");
    ws.insert_test_runtime(pane_id, runtime);

    state.workspaces = vec![ws];
    state.active = Some(0);
    state.selected = 0;
    state.mode = crate::app::Mode::Terminal;

    let area = Rect::new(0, 0, 80, 24);
    let _ = crate::server::render_stream::render_virtual(&mut state, area, true);
    let terminal_runtimes = crate::terminal::TerminalRuntimeRegistry::new();
    let runtime = state
        .runtime_for_pane(&terminal_runtimes, pane_id)
        .expect("pane runtime after initial render");
    runtime.test_process_pty_bytes(b"\x1b[?2026h\x1b[2;3H");
    assert!(runtime.synchronized_output_active());

    let (_buffer, cursor) = crate::server::render_stream::render_virtual(&mut state, area, false);

    assert_eq!(
        cursor, None,
        "child cursor positions are unstable while synchronized output is active"
    );
}

#[tokio::test]
async fn virtual_render_hides_focused_pane_cursor_during_synchronized_output_resize() {
    let mut state = AppState::test_new();
    let mut ws = crate::workspace::Workspace::test_new("test");
    let pane_id = ws.tabs[0].root_pane;
    let runtime = crate::terminal::TerminalRuntime::test_with_screen_bytes(20, 5, b"left");
    ws.insert_test_runtime(pane_id, runtime);

    state.workspaces = vec![ws];
    state.active = Some(0);
    state.selected = 0;
    state.mode = crate::app::Mode::Terminal;

    let initial_area = Rect::new(0, 0, 80, 24);
    let _ = crate::server::render_stream::render_virtual(&mut state, initial_area, true);
    let terminal_runtimes = crate::terminal::TerminalRuntimeRegistry::new();
    let runtime = state
        .runtime_for_pane(&terminal_runtimes, pane_id)
        .expect("pane runtime after initial render");
    runtime.test_process_pty_bytes(b"\x1b[?2026h\x1b[2;3H");
    assert!(runtime.synchronized_output_active());

    let resized_area = Rect::new(0, 0, 100, 30);
    let (_buffer, cursor) =
        crate::server::render_stream::render_virtual(&mut state, resized_area, true);

    assert_eq!(
        cursor, None,
        "pre-resize synchronized output should suppress the cursor even if resize clears the mode"
    );
}

#[tokio::test]
async fn virtual_render_exposes_hidden_pane_cursor_when_reveal_hidden_for_cjk_ime() {
    let mut state = AppState::test_new();
    state.reveal_hidden_cursor_for_cjk_ime = true;
    let mut ws = crate::workspace::Workspace::test_new("test");
    let pane_id = ws.tabs[0].root_pane;
    ws.insert_test_runtime(
        pane_id,
        crate::terminal::TerminalRuntime::test_with_screen_bytes(20, 5, b"left\x1b[?25l"),
    );

    state.workspaces = vec![ws];
    state.active = Some(0);
    state.selected = 0;
    state.mode = crate::app::Mode::Terminal;

    let area = Rect::new(0, 0, 80, 24);
    let (_buffer, cursor) = crate::server::render_stream::render_virtual(&mut state, area, true);
    let pane = state
        .view
        .pane_infos
        .iter()
        .find(|info| info.id == pane_id)
        .expect("focused pane info");

    assert_eq!(
        cursor,
        Some(CursorState {
            x: pane.inner_rect.x + 4,
            y: pane.inner_rect.y,
            visible: true,
            shape: state.cjk_ime_cursor_shape,
        })
    );
}

#[tokio::test]
async fn virtual_render_keeps_cursor_hidden_when_scrolled_back_even_with_reveal_hidden_for_cjk_ime()
{
    let mut state = AppState::test_new();
    state.reveal_hidden_cursor_for_cjk_ime = true;
    let mut ws = crate::workspace::Workspace::test_new("test");
    let pane_id = ws.tabs[0].root_pane;
    let mut bytes = Vec::new();
    for line in 0..80 {
        bytes.extend_from_slice(format!("line {line:02}\r\n").as_bytes());
    }
    let runtime = crate::terminal::TerminalRuntime::test_with_scrollback_bytes(20, 5, 4096, &bytes);
    ws.insert_test_runtime(pane_id, runtime);

    state.workspaces = vec![ws];
    state.active = Some(0);
    state.selected = 0;
    state.mode = crate::app::Mode::Terminal;

    let area = Rect::new(0, 0, 80, 24);
    let _ = crate::server::render_stream::render_virtual(&mut state, area, true);
    let terminal_runtimes = crate::terminal::TerminalRuntimeRegistry::new();
    let runtime = state
        .runtime_for_pane(&terminal_runtimes, pane_id)
        .expect("pane runtime after initial render");
    runtime.scroll_up(6);
    assert!(crate::ui::pane_is_scrolled_back(runtime));

    let (_buffer, cursor) = crate::server::render_stream::render_virtual(&mut state, area, true);

    assert!(
        cursor.as_ref().is_none_or(|cursor| !cursor.visible),
        "scrolled-back focused pane should keep the cursor hidden even when reveal_hidden_cursor_for_cjk_ime is true; got {cursor:?}",
    );
}

#[tokio::test]
async fn virtual_render_fallback_cursor_when_viewport_none_and_reveal_hidden_for_cjk_ime() {
    let mut state = AppState::test_new();
    state.reveal_hidden_cursor_for_cjk_ime = true;
    let mut ws = crate::workspace::Workspace::test_new("test");
    let pane_id = ws.tabs[0].root_pane;
    // Feed only ?25l with no prior cursor movement — exercises the fallback
    // path for TUIs whose viewport has no cursor position.
    ws.insert_test_runtime(
        pane_id,
        crate::terminal::TerminalRuntime::test_with_screen_bytes(20, 5, b"\x1b[?25l"),
    );

    state.workspaces = vec![ws];
    state.active = Some(0);
    state.selected = 0;
    state.mode = crate::app::Mode::Terminal;

    let area = Rect::new(0, 0, 80, 24);
    let (_buffer, cursor) = crate::server::render_stream::render_virtual(&mut state, area, true);
    let pane = state
        .view
        .pane_infos
        .iter()
        .find(|info| info.id == pane_id)
        .expect("focused pane info");

    assert_eq!(
        cursor,
        Some(CursorState {
            x: pane.inner_rect.x,
            y: pane.inner_rect.y,
            visible: true,
            shape: state.cjk_ime_cursor_shape,
        }),
        "fallback should anchor at pane top-left with the configured shape",
    );
}

#[tokio::test]
async fn virtual_render_skips_reveal_when_focused_pane_has_no_detected_agent() {
    let mut state = AppState::test_new();
    state.reveal_hidden_cursor_for_cjk_ime = true;
    // Filter only Claude, but the test pane has no detected agent, so the
    // reveal must not apply.
    state.cjk_ime_agent_filter_configured = true;
    state.cjk_ime_agents = vec![crate::detect::Agent::Claude];
    let mut ws = crate::workspace::Workspace::test_new("test");
    let pane_id = ws.tabs[0].root_pane;
    ws.insert_test_runtime(
        pane_id,
        crate::terminal::TerminalRuntime::test_with_screen_bytes(20, 5, b"left\x1b[?25l"),
    );

    state.workspaces = vec![ws];
    state.active = Some(0);
    state.selected = 0;
    state.mode = crate::app::Mode::Terminal;

    let area = Rect::new(0, 0, 80, 24);
    let (_buffer, cursor) = crate::server::render_stream::render_virtual(&mut state, area, true);

    assert!(
        cursor.as_ref().is_none_or(|cursor| !cursor.visible),
        "agent filter should suppress reveal when the focused pane's detected agent is not on the list; got {cursor:?}",
    );
}

#[tokio::test]
async fn virtual_render_skips_reveal_when_agent_filter_has_no_valid_entries() {
    let mut state = AppState::test_new();
    state.reveal_hidden_cursor_for_cjk_ime = true;
    state.cjk_ime_agent_filter_configured = true;
    state.cjk_ime_agents = Vec::new();
    let mut ws = crate::workspace::Workspace::test_new("test");
    let pane_id = ws.tabs[0].root_pane;
    ws.insert_test_runtime(
        pane_id,
        crate::terminal::TerminalRuntime::test_with_screen_bytes(20, 5, b"left\x1b[?25l"),
    );

    state.workspaces = vec![ws];
    state.active = Some(0);
    state.selected = 0;
    state.mode = crate::app::Mode::Terminal;

    let area = Rect::new(0, 0, 80, 24);
    let (_buffer, cursor) = crate::server::render_stream::render_virtual(&mut state, area, true);

    assert!(
        cursor.as_ref().is_none_or(|cursor| !cursor.visible),
        "agent filter with no valid entries should suppress reveal; got {cursor:?}",
    );
}

#[tokio::test]
async fn virtual_render_omits_focused_pane_cursor_while_mobile_switcher_open() {
    let mut state = AppState::test_new();
    let mut ws = crate::workspace::Workspace::test_new("test");
    let pane_id = ws.tabs[0].root_pane;
    ws.insert_test_runtime(
        pane_id,
        crate::terminal::TerminalRuntime::test_with_screen_bytes(20, 5, b"left"),
    );

    state.workspaces = vec![ws];
    state.active = Some(0);
    state.selected = 0;
    state.mode = crate::app::Mode::Navigate;

    let area = Rect::new(0, 0, 44, 24);
    let (_buffer, cursor) = crate::server::render_stream::render_virtual(&mut state, area, true);

    assert_eq!(cursor, None);
}

#[tokio::test]
async fn virtual_render_hides_focused_pane_cursor_while_scrolled_back() {
    let mut state = AppState::test_new();
    let mut ws = crate::workspace::Workspace::test_new("test");
    let pane_id = ws.tabs[0].root_pane;
    let mut bytes = Vec::new();
    for line in 0..80 {
        bytes.extend_from_slice(format!("line {line:02}\r\n").as_bytes());
    }
    let runtime = crate::terminal::TerminalRuntime::test_with_scrollback_bytes(20, 5, 4096, &bytes);
    ws.insert_test_runtime(pane_id, runtime);

    state.workspaces = vec![ws];
    state.active = Some(0);
    state.selected = 0;
    state.mode = crate::app::Mode::Terminal;

    let area = Rect::new(0, 0, 80, 24);
    let _ = crate::server::render_stream::render_virtual(&mut state, area, true);
    let terminal_runtimes = crate::terminal::TerminalRuntimeRegistry::new();
    let runtime = state
        .runtime_for_pane(&terminal_runtimes, pane_id)
        .expect("pane runtime after initial render");
    runtime.scroll_up(6);
    assert!(crate::ui::pane_is_scrolled_back(runtime));

    let (_buffer, cursor) = crate::server::render_stream::render_virtual(&mut state, area, true);

    assert!(
        cursor.as_ref().is_none_or(|cursor| !cursor.visible),
        "cursor: {cursor:?}"
    );
}

#[test]
fn latest_active_client_drives_shared_size_theme_and_fallback() {
    let mut server = test_headless_server();

    server.clients.insert(
        1,
        ClientConnection::app(
            (160, 45),
            crate::kitty_graphics::HostCellSize::default(),
            crate::terminal_theme::TerminalTheme {
                foreground: Some(crate::terminal_theme::RgbColor {
                    r: 0xaa,
                    g: 0xbb,
                    b: 0xcc,
                }),
                background: Some(crate::terminal_theme::RgbColor {
                    r: 0x11,
                    g: 0x22,
                    b: 0x33,
                }),
                ..Default::default()
            },
            None,
            1,
            RenderEncoding::SemanticFrame,
            None,
        ),
    );
    server.clients.insert(
        2,
        ClientConnection::app(
            (80, 24),
            crate::kitty_graphics::HostCellSize::default(),
            crate::terminal_theme::TerminalTheme {
                foreground: Some(crate::terminal_theme::RgbColor {
                    r: 0x10,
                    g: 0x20,
                    b: 0x30,
                }),
                background: Some(crate::terminal_theme::RgbColor {
                    r: 0xdd,
                    g: 0xee,
                    b: 0xff,
                }),
                ..Default::default()
            },
            None,
            2,
            RenderEncoding::SemanticFrame,
            None,
        ),
    );

    assert!(server.promote_client_to_foreground(1));
    assert_eq!(server.foreground_client_id, Some(1));
    assert_eq!(server.effective_size, (160, 45));
    assert_eq!(
        server.app.state.host_terminal_theme,
        server.clients[&1].host_terminal_theme
    );

    assert!(server.promote_client_to_foreground(2));
    assert_eq!(server.foreground_client_id, Some(2));
    assert_eq!(server.effective_size, (80, 24));
    assert_eq!(
        server.app.state.host_terminal_theme,
        server.clients[&2].host_terminal_theme
    );

    assert!(server.remove_client(2));
    assert_eq!(server.foreground_client_id, Some(1));
    assert_eq!(server.effective_size, (160, 45));
    assert_eq!(
        server.app.state.host_terminal_theme,
        server.clients[&1].host_terminal_theme
    );
}

#[test]
fn foreground_client_without_host_theme_clears_previous_host_theme() {
    let mut server = test_headless_server();
    let known_theme = crate::terminal_theme::TerminalTheme {
        foreground: Some(crate::terminal_theme::RgbColor {
            r: 0x10,
            g: 0x20,
            b: 0x30,
        }),
        background: Some(crate::terminal_theme::RgbColor {
            r: 0x40,
            g: 0x50,
            b: 0x60,
        }),
        ..Default::default()
    };
    server.clients.insert(
        1,
        ClientConnection::app(
            (120, 40),
            crate::kitty_graphics::HostCellSize::default(),
            known_theme,
            None,
            1,
            RenderEncoding::SemanticFrame,
            None,
        ),
    );
    server.clients.insert(
        2,
        ClientConnection::app(
            (120, 40),
            crate::kitty_graphics::HostCellSize::default(),
            crate::terminal_theme::TerminalTheme::default(),
            None,
            2,
            RenderEncoding::SemanticFrame,
            None,
        ),
    );

    assert!(server.promote_client_to_foreground(1));
    assert_eq!(server.app.state.host_terminal_theme, known_theme);

    assert!(server.promote_client_to_foreground(2));
    assert_eq!(
        server.app.state.host_terminal_theme,
        crate::terminal_theme::TerminalTheme::default()
    );
}

#[test]
fn foreground_client_appearance_controls_auto_theme() {
    let mut server = test_headless_server();
    server.app.state.theme_runtime.auto_switch = true;
    server.app.state.theme_runtime.dark_name = "catppuccin".to_string();
    server.app.state.theme_runtime.light_name = "catppuccin-latte".to_string();
    server.clients.insert(
        1,
        ClientConnection::app(
            (120, 40),
            crate::kitty_graphics::HostCellSize::default(),
            crate::terminal_theme::TerminalTheme {
                foreground: None,
                background: Some(crate::terminal_theme::RgbColor { r: 0, g: 0, b: 0 }),
                ..Default::default()
            },
            None,
            1,
            RenderEncoding::SemanticFrame,
            None,
        ),
    );
    server.clients.insert(
        2,
        ClientConnection::app(
            (120, 40),
            crate::kitty_graphics::HostCellSize::default(),
            crate::terminal_theme::TerminalTheme {
                foreground: None,
                background: Some(crate::terminal_theme::RgbColor {
                    r: 255,
                    g: 255,
                    b: 255,
                }),
                ..Default::default()
            },
            None,
            2,
            RenderEncoding::SemanticFrame,
            None,
        ),
    );

    assert!(server.promote_client_to_foreground(1));
    assert_eq!(server.app.state.theme_name, "catppuccin");

    assert!(server.promote_client_to_foreground(2));
    assert_eq!(server.app.state.theme_name, "catppuccin-latte");
}

#[test]
fn color_scheme_change_event_is_inert_on_server() {
    let mut server = test_headless_server();
    let initial_theme = crate::terminal_theme::TerminalTheme {
        foreground: Some(crate::terminal_theme::RgbColor {
            r: 0x10,
            g: 0x20,
            b: 0x30,
        }),
        background: Some(crate::terminal_theme::RgbColor {
            r: 0x40,
            g: 0x50,
            b: 0x60,
        }),
        ..Default::default()
    };
    server.app.state.host_terminal_theme = initial_theme;
    server.clients.insert(
        1,
        ClientConnection::app(
            (120, 40),
            crate::kitty_graphics::HostCellSize::default(),
            initial_theme,
            None,
            1,
            RenderEncoding::SemanticFrame,
            None,
        ),
    );

    let changed = server.handle_server_event(ServerEvent::ClientInput {
        client_id: 1,
        data: crate::raw_input::GHOSTTY_COLOR_SCHEME_DARK_REPORT.to_vec(),
    });

    assert!(!changed);
    assert_eq!(server.foreground_client_id, None);
    assert_eq!(server.clients[&1].host_terminal_theme, initial_theme);
    assert_eq!(server.app.state.host_terminal_theme, initial_theme);
}

#[test]
fn focus_lost_updates_client_without_promoting_foreground() {
    let mut server = test_headless_server();

    server.clients.insert(
        1,
        ClientConnection::app(
            (120, 40),
            crate::kitty_graphics::HostCellSize::default(),
            crate::terminal_theme::TerminalTheme::default(),
            None,
            1,
            RenderEncoding::SemanticFrame,
            None,
        ),
    );
    server.clients.insert(
        2,
        ClientConnection::app(
            (80, 24),
            crate::kitty_graphics::HostCellSize::default(),
            crate::terminal_theme::TerminalTheme::default(),
            Some(true),
            2,
            RenderEncoding::SemanticFrame,
            None,
        ),
    );
    server.foreground_client_id = Some(2);
    server.sync_foreground_client_state();

    let changed = server.handle_server_event(ServerEvent::ClientInput {
        client_id: 1,
        data: b"\x1b[O".to_vec(),
    });

    assert!(!changed);
    assert_eq!(server.foreground_client_id, Some(2));
    assert_eq!(server.clients[&1].outer_terminal_focus, Some(false));
    assert_eq!(server.app.state.outer_terminal_focus, Some(true));
}

#[test]
fn focus_gained_promotes_client_to_foreground() {
    let mut server = test_headless_server();

    server.clients.insert(
        1,
        ClientConnection::app(
            (120, 40),
            crate::kitty_graphics::HostCellSize::default(),
            crate::terminal_theme::TerminalTheme::default(),
            None,
            1,
            RenderEncoding::SemanticFrame,
            None,
        ),
    );
    server.clients.insert(
        2,
        ClientConnection::app(
            (80, 24),
            crate::kitty_graphics::HostCellSize::default(),
            crate::terminal_theme::TerminalTheme::default(),
            Some(true),
            2,
            RenderEncoding::SemanticFrame,
            None,
        ),
    );
    server.foreground_client_id = Some(2);
    server.sync_foreground_client_state();

    let changed = server.handle_server_event(ServerEvent::ClientInput {
        client_id: 1,
        data: b"\x1b[I".to_vec(),
    });

    assert!(changed);
    assert_eq!(server.foreground_client_id, Some(1));
    assert_eq!(server.clients[&1].outer_terminal_focus, Some(true));
    assert_eq!(server.app.state.outer_terminal_focus, Some(true));
}

#[tokio::test]
async fn foreground_focus_gained_reaches_pane_with_focus_reporting() {
    let mut server = test_headless_server();
    let mut input_rx = install_focused_test_runtime(&mut server, b"\x1b[?1004h");

    server.clients.insert(1, test_app_client(Some(false), 1));
    server.foreground_client_id = Some(1);
    server.sync_foreground_client_state();

    assert!(server.handle_server_event(ServerEvent::ClientInput {
        client_id: 1,
        data: b"\x1b[I".to_vec(),
    }));
    assert_eq!(
        input_rx.try_recv().expect("forwarded focus gained report"),
        Bytes::from_static(b"\x1b[I")
    );

    assert!(!server.handle_server_event(ServerEvent::ClientInput {
        client_id: 1,
        data: b"\x1b[O".to_vec(),
    }));
    assert_eq!(
        input_rx.try_recv().expect("forwarded focus lost report"),
        Bytes::from_static(b"\x1b[O")
    );
}

#[tokio::test]
async fn outer_focus_events_do_not_reach_pane_without_focus_reporting() {
    let mut server = test_headless_server();
    let mut input_rx = install_focused_test_runtime(&mut server, b"");
    server.clients.insert(1, test_app_client(Some(false), 1));
    server.foreground_client_id = Some(1);
    server.sync_foreground_client_state();

    assert!(server.handle_server_event(ServerEvent::ClientInput {
        client_id: 1,
        data: b"\x1b[I".to_vec(),
    }));
    assert!(matches!(
        input_rx.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    ));
}

#[tokio::test]
async fn background_focus_batch_only_forwards_events_after_promotion() {
    let mut server = test_headless_server();
    let mut input_rx = install_focused_test_runtime(&mut server, b"\x1b[?1004h");
    server.clients.insert(1, test_app_client(Some(true), 1));
    server.clients.insert(2, test_app_client(Some(false), 2));
    server.foreground_client_id = Some(1);
    server.sync_foreground_client_state();

    assert!(server.handle_server_event(ServerEvent::ClientInput {
        client_id: 2,
        data: b"\x1b[O\x1b[I".to_vec(),
    }));
    assert_eq!(server.foreground_client_id, Some(2));
    assert_eq!(server.app.state.outer_terminal_focus, Some(true));
    assert_eq!(
        input_rx
            .try_recv()
            .expect("focus gained after client promotion"),
        Bytes::from_static(b"\x1b[I")
    );
    assert!(matches!(
        input_rx.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    ));
}

#[tokio::test]
async fn background_client_focus_loss_releases_its_owned_keys() {
    let mut server = test_headless_server();
    let mut input_rx = install_focused_test_runtime(&mut server, b"\x1b[>15u");
    server.clients.insert(1, test_app_client(Some(true), 1));
    server.clients.insert(2, test_app_client(Some(true), 2));
    server.foreground_client_id = Some(1);
    server.sync_foreground_client_state();

    assert!(server.handle_server_event(ServerEvent::ClientInputEvents {
        client_id: 1,
        events: vec![crate::protocol::ClientInputEvent::Key {
            code: crate::protocol::ClientKeyCode::Char('j'),
            modifiers: 0,
            kind: crate::protocol::ClientKeyKind::Press,

            repeat_count: 1,
            generated_text: None,
            source: crate::protocol::ClientKeySource::Synthesized,
        }],
    }));
    server.foreground_client_id = Some(2);
    server.sync_foreground_client_state();

    assert!(!server.handle_server_event(ServerEvent::ClientInputEvents {
        client_id: 1,
        events: vec![crate::protocol::ClientInputEvent::FocusLost],
    }));
    assert_eq!(
        input_rx.try_recv().expect("forwarded press"),
        Bytes::from_static(b"\x1b[106;1:1u")
    );
    assert_eq!(
        input_rx
            .try_recv()
            .expect("synthetic release from background client"),
        Bytes::from_static(b"\x1b[106;1:3u")
    );
    assert!(server.app.input_leases.is_empty());
}

#[tokio::test]
async fn structured_outer_focus_events_reach_reporting_pane() {
    let mut server = test_headless_server();
    let mut input_rx = install_focused_test_runtime(&mut server, b"\x1b[?1004h");
    server.clients.insert(1, test_app_client(Some(true), 1));
    server.foreground_client_id = Some(1);
    server.sync_foreground_client_state();

    assert!(server.handle_server_event(ServerEvent::ClientInputEvents {
        client_id: 1,
        events: vec![
            crate::protocol::ClientInputEvent::FocusGained,
            crate::protocol::ClientInputEvent::FocusLost,
        ],
    }));
    assert_eq!(
        input_rx.try_recv().expect("structured focus gained report"),
        Bytes::from_static(b"\x1b[I")
    );
    assert_eq!(
        input_rx.try_recv().expect("structured focus lost report"),
        Bytes::from_static(b"\x1b[O")
    );
}

#[tokio::test]
async fn background_key_makes_later_focus_lost_eligible() {
    let mut server = test_headless_server();
    let mut input_rx = install_focused_test_runtime(&mut server, b"\x1b[?1004h");
    server.clients.insert(1, test_app_client(Some(true), 1));
    server.clients.insert(2, test_app_client(Some(true), 2));
    server.foreground_client_id = Some(1);
    server.sync_foreground_client_state();

    assert!(server.handle_server_event(ServerEvent::ClientInputEvents {
        client_id: 2,
        events: vec![
            crate::protocol::ClientInputEvent::Key {
                code: crate::protocol::ClientKeyCode::Char('x'),
                modifiers: 0,
                kind: crate::protocol::ClientKeyKind::Release,

                repeat_count: 1,
                generated_text: None,
                source: crate::protocol::ClientKeySource::Synthesized,
            },
            crate::protocol::ClientInputEvent::FocusLost,
        ],
    }));
    assert_eq!(server.foreground_client_id, Some(2));
    assert_eq!(
        input_rx.try_recv().expect("focus lost after promotion"),
        Bytes::from_static(b"\x1b[O")
    );
}

#[tokio::test]
async fn structured_non_app_focus_is_ignored_without_suppressing_keys() {
    let mut server = test_headless_server();
    let mut input_rx = install_focused_test_runtime(&mut server, b"\x1b[?1004h");
    server.clients.insert(1, test_app_client(Some(true), 1));

    let mut attached = test_app_client(Some(false), 2);
    attached.mode = ClientConnectionMode::TerminalAttach {
        terminal_id: "attached".to_owned(),
    };
    server.clients.insert(2, attached);

    let mut pending = test_app_client(Some(false), 3);
    pending.mode = ClientConnectionMode::TerminalPending;
    server.clients.insert(3, pending);
    server.foreground_client_id = Some(1);
    server.sync_foreground_client_state();

    for client_id in [2, 3] {
        assert!(!server.handle_server_event(ServerEvent::ClientInputEvents {
            client_id,
            events: vec![crate::protocol::ClientInputEvent::FocusGained],
        }));
        assert_eq!(server.foreground_client_id, Some(1));
        assert_eq!(server.app.state.outer_terminal_focus, Some(true));
        assert_eq!(server.clients[&client_id].outer_terminal_focus, Some(false));
    }

    assert!(matches!(
        input_rx.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    ));

    assert!(server.handle_server_event(ServerEvent::ClientInputEvents {
        client_id: 3,
        events: vec![crate::protocol::ClientInputEvent::Key {
            code: crate::protocol::ClientKeyCode::Char('x'),
            modifiers: 0,
            kind: crate::protocol::ClientKeyKind::Release,

            repeat_count: 1,
            generated_text: None,
            source: crate::protocol::ClientKeySource::Synthesized,
        }],
    }));
    assert_eq!(server.foreground_client_id, Some(3));
}

#[test]
fn terminal_attach_resize_preserves_known_cell_size_when_pixels_are_omitted() {
    with_terminal_session_test_server(|server, _terminal_id, terminal_id, _pane_id| {
        let mut client = ClientConnection::app(
            (80, 24),
            crate::kitty_graphics::HostCellSize {
                width_px: 10,
                height_px: 20,
            },
            crate::terminal_theme::TerminalTheme::default(),
            Some(true),
            1,
            RenderEncoding::SemanticFrame,
            None,
        );
        client.mode = ClientConnectionMode::TerminalAttach {
            terminal_id: terminal_id.clone(),
        };
        server.clients.insert(1, client);

        assert!(server.handle_server_event(ServerEvent::ClientResize {
            client_id: 1,
            cols: 100,
            rows: 30,
            cell_width_px: 0,
            cell_height_px: 0,
            pixel_mouse: false,
        }));

        assert_eq!(
            server
                .runtime_for_terminal_id_string(&terminal_id)
                .unwrap()
                .pixel_size(),
            Some((1_000, 600))
        );
        assert_eq!(
            server.clients[&1].cell_size,
            crate::kitty_graphics::HostCellSize {
                width_px: 10,
                height_px: 20,
            }
        );
    });
}

#[tokio::test]
async fn passive_mouse_motion_forwards_without_requesting_render() {
    let mut server = test_headless_server();
    let mut input_rx = install_focused_test_runtime(&mut server, b"\x1b[?1003h\x1b[?1006h");
    server.clients.insert(1, test_app_client(Some(true), 1));
    server.foreground_client_id = Some(1);
    server.sync_foreground_client_state();
    server.resize_shared_runtime_to_effective_size();
    let baseline = FrameData {
        cells: Vec::new(),
        width: 0,
        height: 0,
        cursor: None,
        hyperlinks: Vec::new(),
        graphics: Vec::new(),
    };
    let client = server.clients.get_mut(&1).unwrap();
    let prepared = client
        .render_state
        .prepare_frame(baseline.clone())
        .expect("new semantic baseline");
    client.render_state.commit_sent_frame(prepared);
    let pane = server.app.state.view.pane_infos[0].clone();
    let column = pane.inner_rect.x + 2;
    let row = pane.inner_rect.y + 3;
    let input = format!("\x1b[<35;{};{}M", column + 1, row + 1).into_bytes();

    assert!(!server.handle_server_event(ServerEvent::ClientInput {
        client_id: 1,
        data: input,
    }));
    assert_eq!(
        input_rx.try_recv().expect("forwarded mouse motion"),
        Bytes::from_static(b"\x1b[<35;3;4M")
    );
    assert_eq!(
        server.clients[&1].render_state.last_frame(),
        Some(&baseline)
    );
}

#[test]
fn background_mouse_motion_promotes_once_then_becomes_render_neutral() {
    let mut server = test_headless_server();
    server.app.state.mode = crate::app::Mode::Terminal;
    server.clients.insert(1, test_app_client(Some(true), 1));
    server.clients.insert(2, test_app_client(Some(true), 2));
    server.foreground_client_id = Some(1);
    server.sync_foreground_client_state();
    let motion = || ServerEvent::ClientInputEvents {
        client_id: 2,
        events: vec![crate::protocol::ClientInputEvent::Mouse {
            kind: crate::protocol::ClientMouseKind::Moved,
            column: 10,
            row: 5,
            modifiers: 0,
        }],
    };

    assert!(server.handle_server_event(motion()));
    assert_eq!(server.foreground_client_id, Some(2));
    assert!(!server.handle_server_event(motion()));
}

#[test]
fn mouse_motion_in_hover_modes_requires_render() {
    let events = [crate::raw_input::RawInputEvent::Mouse(
        crossterm::event::MouseEvent {
            kind: MouseEventKind::Moved,
            column: 10,
            row: 5,
            modifiers: KeyModifiers::empty(),
        },
    )];

    assert!(events_are_render_neutral_mouse_motion(
        &events,
        crate::app::Mode::Terminal
    ));
    for mode in [
        crate::app::Mode::GlobalMenu,
        crate::app::Mode::ContextMenu,
        crate::app::Mode::Navigator,
    ] {
        assert!(!events_are_render_neutral_mouse_motion(&events, mode));
    }
}

fn install_focused_test_runtime(
    server: &mut HeadlessServer,
    terminal_bytes: &[u8],
) -> tokio::sync::mpsc::Receiver<Bytes> {
    let mut workspace = crate::workspace::Workspace::test_new("focus-reporting");
    let pane_id = workspace.tabs[0].root_pane;
    let (runtime, input_rx) =
        crate::terminal::TerminalRuntime::test_with_channel_and_scrollback_bytes(
            80,
            24,
            0,
            terminal_bytes,
            4,
        );
    workspace.insert_test_runtime(pane_id, runtime);
    server.app.state.workspaces = vec![workspace];
    server.app.state.active = Some(0);
    server.app.state.selected = 0;
    server.app.state.mode = crate::app::Mode::Terminal;
    input_rx
}

fn test_app_client(outer_terminal_focus: Option<bool>, last_activity: u64) -> ClientConnection {
    ClientConnection::app(
        (80, 24),
        crate::kitty_graphics::HostCellSize::default(),
        crate::terminal_theme::TerminalTheme::default(),
        outer_terminal_focus,
        last_activity,
        RenderEncoding::SemanticFrame,
        None,
    )
}

#[test]
fn foreground_client_focus_event_updates_app_focus_state() {
    let mut server = test_headless_server();

    server.clients.insert(
        1,
        ClientConnection::app(
            (120, 40),
            crate::kitty_graphics::HostCellSize::default(),
            crate::terminal_theme::TerminalTheme::default(),
            Some(true),
            1,
            RenderEncoding::SemanticFrame,
            None,
        ),
    );
    server.foreground_client_id = Some(1);
    server.sync_foreground_client_state();

    let changed = server.handle_server_event(ServerEvent::ClientInput {
        client_id: 1,
        data: b"\x1b[O".to_vec(),
    });

    assert!(!changed);
    assert_eq!(server.clients[&1].outer_terminal_focus, Some(false));
    assert_eq!(server.app.state.outer_terminal_focus, Some(false));
}

#[test]
fn app_client_lone_escape_closes_navigate_mode() {
    let mut server = test_headless_server();
    server.app.state.workspaces = vec![crate::workspace::Workspace::test_new("test")];
    server.app.state.active = Some(0);
    server.app.state.selected = 0;
    server.app.state.mode = crate::app::Mode::Navigate;
    server.clients.insert(
        1,
        ClientConnection::app(
            (80, 24),
            crate::kitty_graphics::HostCellSize::default(),
            crate::terminal_theme::TerminalTheme::default(),
            Some(true),
            1,
            RenderEncoding::SemanticFrame,
            None,
        ),
    );
    server.foreground_client_id = Some(1);
    server.sync_foreground_client_state();

    assert!(server.handle_server_event(ServerEvent::ClientInput {
        client_id: 1,
        data: b"\x1b".to_vec(),
    }));

    assert_eq!(server.app.state.mode, crate::app::Mode::Terminal);
}

#[test]
fn semantic_client_input_events_route_through_app_input() {
    let mut server = test_headless_server();
    server.app.state.mode = crate::app::Mode::Onboarding;
    server.clients.insert(
        1,
        ClientConnection::app(
            (80, 24),
            crate::kitty_graphics::HostCellSize::default(),
            crate::terminal_theme::TerminalTheme::default(),
            Some(true),
            1,
            RenderEncoding::SemanticFrame,
            None,
        ),
    );
    server.foreground_client_id = Some(1);
    server.sync_foreground_client_state();

    assert!(server.handle_server_event(ServerEvent::ClientInputEvents {
        client_id: 1,
        events: vec![crate::protocol::ClientInputEvent::Key {
            code: crate::protocol::ClientKeyCode::Enter,
            modifiers: 0,
            kind: crate::protocol::ClientKeyKind::Press,

            repeat_count: 1,
            generated_text: None,
            source: crate::protocol::ClientKeySource::Synthesized,
        }],
    }));

    assert_eq!(server.app.state.mode, crate::app::Mode::Settings);
    assert_eq!(
        server.app.state.settings.section,
        crate::app::state::SettingsSection::Integrations
    );
}

// TP-FCL-INPUT-01 / TP-FMR-SIDEBAR-HL-01..03: cover the real remote-client boundary that
// the App-level FMR-2 test cannot exercise. Raw host bytes must prepare
// the exact typed request, and the headless scheduled loop must consume it
// into the existing Files generation.
#[test]
fn headless_raw_mouse_locations_navigation_loads_exact_trail() {
    use crate::app::state::{
        FileManagerLocationIcon, FileManagerLocationItem, FileManagerLocationsModel, SidebarTab,
    };

    let root = std::env::temp_dir().join(format!(
        "headless-sidebar-mouse-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    let initial = root.join("initial");
    let target = root.join("target");
    fs::create_dir_all(&initial).expect("create initial directory");
    fs::create_dir_all(&target).expect("create target directory");
    fs::write(target.join("visible.txt"), b"visible").expect("write target entry");

    let mut server = test_headless_server();
    server.clients.insert(
        1,
        ClientConnection::app(
            (106, 20),
            crate::kitty_graphics::HostCellSize::default(),
            crate::terminal_theme::TerminalTheme::default(),
            Some(true),
            1,
            RenderEncoding::SemanticFrame,
            None,
        ),
    );
    server.foreground_client_id = Some(1);
    server.sync_foreground_client_state();
    server.app.state.mode = crate::app::Mode::Terminal;
    server.app.state.mouse_capture = true;
    server
        .app
        .state
        .try_open_file_manager_with(|_| Some(crate::fm::FmState::new(&initial)))
        .expect("open initial Files instance");
    let generation = server
        .app
        .state
        .stage
        .active_instance_generation()
        .expect("active Files generation");
    server.app.state.sidebar_tab = SidebarTab::Files;
    server.app.state.file_manager_locations_model = FileManagerLocationsModel::from_sources(
        vec![FileManagerLocationItem {
            label: "Home".into(),
            path: target.clone(),
            icon: FileManagerLocationIcon::Home,
            accessible: true,
            ejectable: false,
        }],
        Vec::new(),
        Vec::new(),
    );
    crate::ui::compute_view(
        &mut server.app.state,
        ratatui::layout::Rect::new(0, 0, 106, 20),
    );
    let row = server.app.state.view.file_manager_locations.rows[0].clone();
    let mouse = format!(
        "\x1b[<0;{};{}M",
        row.rect.x.saturating_add(1),
        row.rect.y.saturating_add(1)
    )
    .into_bytes();

    assert!(server.handle_server_event(ServerEvent::ClientInput {
        client_id: 1,
        data: mouse,
    }));
    assert_eq!(
        server.app.state.request_file_manager_location_navigation,
        Some(target.clone().into()),
        "raw SGR input reaches the exact model-revalidated locations rail seam"
    );

    assert!(
        server.handle_scheduled_tasks_headless(Instant::now(), false),
        "headless scheduled tasks must consume the pending location request"
    );
    server.app.wait_file_manager_io_for_test();
    assert!(
        server.handle_scheduled_tasks_headless(Instant::now(), false),
        "headless scheduled tasks must observe the prepared root"
    );
    assert!(
        server
            .app
            .state
            .request_file_manager_location_navigation
            .is_none(),
        "the typed location request is one-shot"
    );
    assert_eq!(
        server.app.state.stage.active_instance_generation(),
        Some(generation)
    );
    let file_manager = server
        .app
        .state
        .file_manager
        .as_ref()
        .expect("loaded Files state");
    assert_eq!(file_manager.cwd, target);
    assert_eq!(file_manager.trail.cols()[0].directory, target);
    assert_eq!(
        file_manager.trail_snapshots.cols()[0].directory(),
        target.as_path()
    );

    let _ = fs::remove_dir_all(root);
}

/// Build a directory holding one PNG and open Files on it, with the Trail
/// laid out so the detail panel exists.
///
/// Returns the directory so the caller can remove it; every image test
/// needs the same four steps and they are easy to get subtly wrong.
fn headless_server_showing_one_png(
    server: &mut HeadlessServer,
    label: &str,
    frame: ratatui::layout::Rect,
) -> std::path::PathBuf {
    use image::{DynamicImage, ImageFormat, Rgba, RgbaImage};

    let root = std::env::temp_dir().join(format!(
        "headless-{}-{}-{}",
        label,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    fs::create_dir_all(&root).expect("create image fixture directory");

    let rgba = RgbaImage::from_fn(160, 80, |x, y| {
        Rgba([
            u8::try_from(x % 256).expect("x channel"),
            u8::try_from(y % 256).expect("y channel"),
            0x7f,
            0xff,
        ])
    });
    let mut encoded = std::io::Cursor::new(Vec::new());
    DynamicImage::ImageRgba8(rgba)
        .write_to(&mut encoded, ImageFormat::Png)
        .expect("encode PNG fixture");
    fs::write(root.join("sample.png"), encoded.into_inner()).expect("write PNG fixture");

    // `FmState::new` reads the directory and resolves the preview on this
    // thread, so there is no background IO to wait for here. Calling
    // `wait_file_manager_io_for_test` instead blocks forever: it is an
    // unbounded condvar wait for a generation nothing ever enqueues.
    server
        .app
        .state
        .try_open_file_manager_with(|_| Some(crate::fm::FmState::new(&root)))
        .expect("open Files on the image fixture");
    crate::ui::compute_view(&mut server.app.state, frame);

    root
}

fn headless_image_preview_state(server: &HeadlessServer) -> &crate::fm::FmImagePreviewState {
    match &server
        .app
        .state
        .file_manager
        .as_ref()
        .expect("open Files state")
        .preview
    {
        crate::fm::FmPreview::File(crate::fm::FmFilePreview::Image(preview)) => &preview.state,
        other => panic!("expected an image preview, got {other:?}"),
    }
}

// The headless scheduler must drive the image preview worker.
//
// This is the root cause of "images never appear in server mode". The cell
// size is set by hand here so that this test fails for exactly one reason:
// the scheduler does not call the worker. TP-FMR-IMAGE-HL-02 covers the
// other half.
//
// TP-FMR-IMAGE-HL-01
#[test]
fn headless_scheduler_syncs_the_image_preview_worker() {
    let frame = ratatui::layout::Rect::new(0, 0, 115, 16);
    let mut server = test_headless_server();
    server.app.image_preview_cell_size = crate::kitty_graphics::HostCellSize {
        width_px: 8,
        height_px: 16,
    };
    let root = headless_server_showing_one_png(&mut server, "image-sched", frame);

    assert!(
        server.handle_scheduled_tasks_headless(Instant::now(), false),
        "starting the image decode changes what the frame shows"
    );
    assert!(
        matches!(
            headless_image_preview_state(&server),
            crate::fm::FmImagePreviewState::Loading { .. }
        ),
        "the headless scheduler must reach the image preview worker, \
         otherwise server-mode previews stay Pending forever; got {:?}",
        headless_image_preview_state(&server)
    );

    let _ = fs::remove_dir_all(root);
}

// The headless scheduler must consume queued context-menu intents.
//
// The failure this reproduces was reported from live use: right-click →
// "Send with Tailscale" in server mode closed the menu and nothing
// appeared. The menu had queued the intent correctly; the headless
// scheduler simply never called `sync_file_operation_worker`, so no queued
// context action — this one, Enlarge, delete, copy — ever ran in server
// mode. The monolithic twin of this journey passed the whole time, which
// is why the seam gets its own headless test.
//
// TP-FSEND-TS-25
#[test]
fn headless_scheduler_consumes_context_menu_intents() {
    let frame = ratatui::layout::Rect::new(0, 0, 115, 16);
    let mut server = test_headless_server();
    let root = headless_server_showing_one_png(&mut server, "tailscale-sched", frame);

    let path = root.join("sample.png");
    server.app.state.request_file_manager_context_action =
        Some(crate::app::state::FileManagerContextActionIntent {
            action: crate::app::state::FileManagerContextMenuAction::SendTailscale,
            paths: vec![path.clone()],
        });

    assert!(
        server.handle_scheduled_tasks_headless(Instant::now(), false),
        "consuming the intent changes what the frame shows"
    );
    assert!(
        server
            .app
            .state
            .request_file_manager_context_action
            .is_none(),
        "the intent must be consumed, not left queued forever"
    );
    assert_eq!(
        server.app.state.mode,
        crate::app::state::Mode::TailscaleSend,
        "the picker must open in server mode exactly as it does monolithic"
    );
    assert_eq!(
        server
            .app
            .state
            .tailscale_send
            .as_ref()
            .expect("picker state")
            .paths,
        vec![path]
    );

    let _ = fs::remove_dir_all(root);
}

// The server must publish the foreground client's cell size.
//
// Deliberately separate from TP-FMR-IMAGE-HL-01: a scheduler that runs
// against a cell size of zero derives no target and still shows nothing.
// If one of these could pass while the other fails silently, the split has
// failed.
//
// TP-FMR-IMAGE-HL-02
#[test]
fn headless_publishes_foreground_cell_size_to_the_image_preview() {
    let mut server = test_headless_server();
    server.app.state.kitty_graphics_enabled = true;
    server.clients.insert(
        1,
        ClientConnection::app(
            (115, 16),
            crate::kitty_graphics::HostCellSize {
                width_px: 9,
                height_px: 18,
            },
            crate::terminal_theme::TerminalTheme::default(),
            Some(true),
            1,
            RenderEncoding::SemanticFrame,
            None,
        ),
    );
    server.foreground_client_id = Some(1);

    server.sync_foreground_client_state();

    assert_eq!(
        server.app.image_preview_cell_size,
        crate::kitty_graphics::HostCellSize {
            width_px: 9,
            height_px: 18,
        },
        "the image preview decodes against the cell size of the client \
         actually looking at it, matching host_cell_size rather than \
         inventing a second policy"
    );
    assert_eq!(
        server.app.image_preview_cell_size, server.app.state.host_cell_size,
        "one resolved cell size, not two"
    );
}

/// Does this payload place an image?
///
/// A cleared cache still emits bytes — delete commands (`a=d`) for images
/// the terminal should drop. Asserting the payload is merely non-empty
/// therefore passes while the picture is being taken away, which is the
/// exact failure this suite exists to catch.
fn places_an_image(graphics: &[u8]) -> bool {
    graphics.windows(3).any(|window| window == b"a=p")
}

/// Does this payload take an image off the screen?
///
/// The counterpart to [`places_an_image`], and the one to reach for when
/// asking whether a picture *stays*. The encoder sends diffs: once an image
/// is placed, a frame that changes nothing about it carries no bytes at
/// all. So "is it still there" cannot be read from a single frame — but
/// "was it taken away" can, and that is the question these tests mean.
fn deletes_an_image(graphics: &[u8]) -> bool {
    graphics.windows(3).any(|window| window == b"a=d")
}

/// Drive the scheduler until the image preview holds decoded pixels.
///
/// Decoding happens on a worker thread, so a single scheduled round proves
/// nothing; every image test needs this and none of them should re-invent
/// the deadline.
fn headless_pump_image_until_ready(server: &mut HeadlessServer) {
    let wait = LoadAwareDeadline::new(10, "an image decode");
    loop {
        let _ = server.handle_scheduled_tasks_headless(Instant::now(), false);
        if matches!(
            headless_image_preview_state(server),
            crate::fm::FmImagePreviewState::Ready { .. }
        ) {
            return;
        }
        wait.check_with(format_args!(
            "state {:?}",
            headless_image_preview_state(server)
        ));
        std::thread::yield_now();
    }
}

/// Build a server whose single foreground app client has a writer and the
/// given cell size, with a workspace present so `Mode::Terminal` is real.
fn headless_graphics_server(
    cell_size: crate::kitty_graphics::HostCellSize,
) -> (HeadlessServer, std::sync::mpsc::Receiver<Vec<u8>>) {
    let mut server = test_headless_server();
    server.app.state.workspaces = vec![crate::workspace::Workspace::test_new("test")];
    server.app.state.active = Some(0);
    server.app.state.selected = 0;
    server.app.state.mode = crate::app::Mode::Terminal;
    server.app.state.kitty_graphics_enabled = true;

    let (client_tx, _client_control_rx, client_rx) = test_client_writer();
    server.clients.insert(
        1,
        ClientConnection::app(
            (115, 20),
            cell_size,
            crate::terminal_theme::TerminalTheme::default(),
            Some(true),
            1,
            RenderEncoding::SemanticFrame,
            Some(client_tx),
        ),
    );
    server.foreground_client_id = Some(1);
    server.sync_foreground_client_state();

    (server, client_rx)
}

// End-to-end: the whole chain, from the scheduler driving the decode to
// the encoder placing the result in a client frame. TP-FMR-IMAGE-HL-01 and
// -02 each prove one link; this proves they are actually connected.
//
// TP-FMR-IMAGE-HL-03
#[test]
fn server_frame_carries_fm_image_graphics_when_ready() {
    let frame_area = ratatui::layout::Rect::new(0, 0, 115, 20);
    let (mut server, client_rx) = headless_graphics_server(crate::kitty_graphics::HostCellSize {
        width_px: 8,
        height_px: 16,
    });
    let root = headless_server_showing_one_png(&mut server, "image-e2e", frame_area);

    // Decoding happens on a worker thread, so pump the scheduler until the
    // pixels land rather than assuming one round is enough.
    let wait = LoadAwareDeadline::new(10, "a server-mode image decode");
    loop {
        let _ = server.handle_scheduled_tasks_headless(Instant::now(), false);
        if matches!(
            headless_image_preview_state(&server),
            crate::fm::FmImagePreviewState::Ready { .. }
        ) {
            break;
        }
        wait.check_with(format_args!(
            "state {:?}",
            headless_image_preview_state(&server)
        ));
        std::thread::yield_now();
    }

    server.render_and_stream();
    let frame = read_server_frame(
        client_rx
            .recv_timeout(Duration::from_millis(500))
            .expect("a frame reaches the client"),
    );

    assert!(
        places_an_image(&frame.graphics),
        "a ready file manager image must reach the client as a placement, \
         not merely as some kitty graphics traffic"
    );

    let _ = fs::remove_dir_all(root);
}

// With several displays, the graphics must be encoded inside the same
// viewer window the frame was rendered in.
//
// Found live: previews vanished the moment a second display attached. The
// render arm restored the viewer before the graphics encode ran, so the
// encode read whichever view the restore installed — the session default,
// whose owned surfaces hold no file browser. One display worked only
// because a sole display shares the register slot with the session.
//
// TP-MCF-CTX-06
#[test]
fn fm_image_graphics_reach_the_display_showing_files_with_another_display_present() {
    let frame_area = ratatui::layout::Rect::new(0, 0, 115, 20);
    let cell_size = crate::kitty_graphics::HostCellSize {
        width_px: 8,
        height_px: 16,
    };
    let (mut server, client_rx_1) = headless_graphics_server(cell_size);

    // A second app display, same capabilities, its own writer.
    let (client_tx_2, _control_2, client_rx_2) = test_client_writer();
    server.clients.insert(
        2,
        ClientConnection::app(
            (115, 20),
            cell_size,
            crate::terminal_theme::TerminalTheme::default(),
            Some(true),
            2,
            RenderEncoding::SemanticFrame,
            Some(client_tx_2),
        ),
    );

    // Seed both displays' views, then open Files with the PNG inside
    // display 1's window — exactly where a person would have opened it.
    server.render_and_stream();
    let previous = server.app.state.enter_viewer(Some(1));
    let root = headless_server_showing_one_png(&mut server, "image-two-displays", frame_area);
    server.app.state.restore_viewer(previous);

    // Drive the decode to Ready, checking inside display 1's window: with
    // several displays the register holds no file manager between serves.
    let wait = LoadAwareDeadline::new(10, "an image decode with two displays");
    loop {
        let _ = server.handle_scheduled_tasks_headless(Instant::now(), false);
        let previous = server.app.state.enter_viewer(Some(1));
        let ready = matches!(
            headless_image_preview_state(&server),
            crate::fm::FmImagePreviewState::Ready { .. }
        );
        server.app.state.restore_viewer(previous);
        if ready {
            break;
        }
        wait.check();
        std::thread::yield_now();
    }

    while client_rx_1.try_recv().is_ok() {}
    while client_rx_2.try_recv().is_ok() {}
    server.render_and_stream();

    let frame_1 = read_server_frame(
        client_rx_1
            .recv_timeout(Duration::from_millis(500))
            .expect("display 1 receives a frame"),
    );
    assert!(
        places_an_image(&frame_1.graphics),
        "the display showing Files must receive its preview even while \
         another display is attached"
    );

    if let Ok(bytes) = client_rx_2.recv_timeout(Duration::from_millis(500)) {
        let frame_2 = read_server_frame(bytes);
        assert!(
            !places_an_image(&frame_2.graphics),
            "a display not showing Files must not receive the preview"
        );
    }

    let _ = fs::remove_dir_all(root);
}

// A plugin popup opened through the API belongs to the display the person
// is looking at, not to every display.
//
// The live report: clicking a preview in the file manager opened the sheet
// viewer on every attached screen. The plugin opens it by calling back into
// the API, so the request carries no display identity; the popup landed in
// the session's registers and was broadcast from there.
//
// TP-SUR-BROADCAST-05
#[tokio::test]
async fn a_plugin_popup_opened_through_the_api_belongs_to_the_focused_display() {
    let mut server = test_headless_server();
    server.app.state.workspaces = vec![crate::workspace::Workspace::test_new("test")];
    server.app.state.active = Some(0);
    server.app.state.mode = crate::app::Mode::Terminal;

    for client_id in [1u64, 2u64] {
        let (client_tx, _control, _render) = test_client_writer();
        server.clients.insert(
            client_id,
            ClientConnection::app(
                (100, 30),
                crate::kitty_graphics::HostCellSize::default(),
                crate::terminal_theme::TerminalTheme::default(),
                Some(client_id == 1),
                client_id,
                RenderEncoding::SemanticFrame,
                Some(client_tx),
            ),
        );
        // Give each display a parked bundle, as attaching does.
        let previous = server.app.state.enter_viewer(Some(client_id));
        server.app.state.restore_viewer(previous);
    }
    server.foreground_client_id = Some(1);

    // Display 1 is where the person clicked; the plugin's popup arrives
    // through the API with no display identity of its own.
    let previous = server.app.state.enter_viewer(server.foreground_client_id);
    let runtime = crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 24, b"");
    server.app.install_test_popup_runtime(runtime);
    server.app.state.restore_viewer(previous);

    let previous = server.app.state.enter_viewer(Some(1));
    let focused_sees_it = server.app.state.popup_pane.is_some();
    server.app.state.restore_viewer(previous);

    let previous = server.app.state.enter_viewer(Some(2));
    let other_sees_it = server.app.state.popup_pane.is_some();
    server.app.state.restore_viewer(previous);

    assert!(
        focused_sees_it,
        "the display the person clicked in must show the popup it opened"
    );
    assert!(
        !other_sees_it,
        "a popup opened on one display must not appear on the others"
    );
}

// The preview keeps its image while a menu is open over it.
//
// Found live, and the log named it exactly: the graphics encoder refuses to
// emit anything unless the app is in terminal mode, and it clears the
// uploaded image on the way out. Opening the right-click menu switches the
// mode, so the picture the menu is floating over disappears underneath it.
//
// The gate is right for pane graphics — a terminal app's images must not
// paint over a modal. It is wrong for the file manager preview, which herdr
// draws itself and keeps on screen while the menu is up.
//
// TP-FMR-IMAGE-HL-06
#[test]
fn fm_image_survives_a_context_menu_opening_over_it() {
    let frame_area = ratatui::layout::Rect::new(0, 0, 115, 20);
    let (mut server, client_rx) = headless_graphics_server(crate::kitty_graphics::HostCellSize {
        width_px: 8,
        height_px: 16,
    });
    let root = headless_server_showing_one_png(&mut server, "image-menu", frame_area);

    let wait = LoadAwareDeadline::new(10, "a decode");
    loop {
        let _ = server.handle_scheduled_tasks_headless(Instant::now(), false);
        if matches!(
            headless_image_preview_state(&server),
            crate::fm::FmImagePreviewState::Ready { .. }
        ) {
            break;
        }
        wait.check();
        std::thread::yield_now();
    }

    server.render_and_stream();
    let first = read_server_frame(
        client_rx
            .recv_timeout(Duration::from_millis(500))
            .expect("first frame"),
    );
    assert!(
        places_an_image(&first.graphics),
        "baseline: the image reaches the client before the menu opens"
    );
    while client_rx.try_recv().is_ok() {}

    // Right-clicking a row puts the app in menu mode. Everything else about
    // the surface is unchanged: the same preview panel is still on screen,
    // with the menu drawn above it.
    server.app.state.mode = crate::app::Mode::ContextMenu;
    let _ = server.handle_scheduled_tasks_headless(Instant::now(), false);
    server.render_and_stream();

    let with_menu = read_server_frame(
        client_rx
            .recv_timeout(Duration::from_millis(500))
            .expect("a frame while the menu is open"),
    );
    assert!(
        !deletes_an_image(&with_menu.graphics),
        "opening a menu over the preview must not take the image away"
    );

    let _ = fs::remove_dir_all(root);
}

// A full-screen overlay takes the image away.
//
// The mirror image of TP-FMR-IMAGE-HL-06, and the reason that fix cannot
// simply drop the gate. Terminal images are not cells in the frame buffer,
// so a settings page drawn over the preview overwrites the text under the
// picture without touching the picture: it would hang over the page.
// A missing image is a much smaller failure than one floating over
// unrelated content.
//
// TP-FMR-IMAGE-HL-07
#[test]
fn full_screen_overlay_takes_the_fm_image_away() {
    let frame_area = ratatui::layout::Rect::new(0, 0, 115, 20);
    let (mut server, client_rx) = headless_graphics_server(crate::kitty_graphics::HostCellSize {
        width_px: 8,
        height_px: 16,
    });
    let root = headless_server_showing_one_png(&mut server, "image-fullscreen", frame_area);
    headless_pump_image_until_ready(&mut server);

    server.render_and_stream();
    let first = read_server_frame(
        client_rx
            .recv_timeout(Duration::from_millis(500))
            .expect("first frame"),
    );
    assert!(
        places_an_image(&first.graphics),
        "baseline: the image is on screen before the overlay opens"
    );
    while client_rx.try_recv().is_ok() {}

    server.app.state.mode = crate::app::Mode::Settings;
    let _ = server.handle_scheduled_tasks_headless(Instant::now(), false);
    server.render_and_stream();

    let covered = read_server_frame(
        client_rx
            .recv_timeout(Duration::from_millis(500))
            .expect("a frame while the settings page is open"),
    );
    assert!(
        deletes_an_image(&covered.graphics),
        "an overlay that covers the whole frame must take the image with \
         it; an image is not a cell, so text drawn over it leaves the \
         picture hanging on top of the page"
    );

    let _ = fs::remove_dir_all(root);
}

// Modes that cover nothing keep the image.
//
// `Prefix` and `Navigate` are not terminal mode either, so the old gate
// silently dropped the preview for them too — the same bug wearing
// different clothes, and the reason the fix is a classification rather than
// a special case for menus.
//
// TP-FMR-IMAGE-HL-08
#[test]
fn transient_modes_that_cover_nothing_keep_the_fm_image() {
    let frame_area = ratatui::layout::Rect::new(0, 0, 115, 20);
    let (mut server, client_rx) = headless_graphics_server(crate::kitty_graphics::HostCellSize {
        width_px: 8,
        height_px: 16,
    });
    let root = headless_server_showing_one_png(&mut server, "image-transient", frame_area);
    headless_pump_image_until_ready(&mut server);
    server.render_and_stream();
    while client_rx.try_recv().is_ok() {}

    for mode in [crate::app::Mode::Prefix, crate::app::Mode::Navigate] {
        server.app.state.mode = mode;
        let _ = server.handle_scheduled_tasks_headless(Instant::now(), false);
        server.render_and_stream();
        let frame = read_server_frame(
            client_rx
                .recv_timeout(Duration::from_millis(500))
                .unwrap_or_else(|_| panic!("a frame in {mode:?}")),
        );
        assert!(
            !deletes_an_image(&frame.graphics),
            "{mode:?} covers nothing, so the preview keeps its image"
        );
        while client_rx.try_recv().is_ok() {}
    }

    let _ = fs::remove_dir_all(root);
}

// An open-but-backgrounded Files tab does not claim the placement pass.
//
// `TP-FTAB-INPUT-02` in graphics form: reading "is the file manager open"
// rather than "does it own the stage" hands the pass to a surface nobody is
// looking at, and the visible terminal loses its own images.
//
// TP-FMR-IMAGE-HL-09
#[test]
fn backgrounded_files_tab_does_not_claim_the_placement_pass() {
    let frame_area = ratatui::layout::Rect::new(0, 0, 115, 20);
    let (mut server, client_rx) = headless_graphics_server(crate::kitty_graphics::HostCellSize {
        width_px: 8,
        height_px: 16,
    });
    let root = headless_server_showing_one_png(&mut server, "image-bg", frame_area);
    headless_pump_image_until_ready(&mut server);
    server.render_and_stream();
    let first = read_server_frame(
        client_rx
            .recv_timeout(Duration::from_millis(500))
            .expect("first frame"),
    );
    assert!(places_an_image(&first.graphics), "baseline");
    while client_rx.try_recv().is_ok() {}

    // Files stays open; the terminal workspace takes the stage.
    server.app.state.show_terminal_workspace();
    assert!(
        server.app.state.file_manager.is_some(),
        "the Files tab is still open, which is exactly the trap"
    );
    let _ = server.handle_scheduled_tasks_headless(Instant::now(), false);
    server.render_and_stream();

    let backgrounded = read_server_frame(
        client_rx
            .recv_timeout(Duration::from_millis(500))
            .expect("a frame on the terminal workspace"),
    );
    assert!(
        deletes_an_image(&backgrounded.graphics),
        "a backgrounded Files tab must not leave its preview over the \
         terminal the user is actually looking at"
    );

    let _ = fs::remove_dir_all(root);
}

// An image still reaches the client after the user leaves the Files
// surface and comes back.
//
// Reported from a live run: the preview drew correctly, then opening a file
// in a new tab and returning left the panel blank for the rest of the
// session. Backgrounding the surface is an ordinary thing to do, so a
// preview that only survives until the first tab switch is barely a
// preview.
//
// TP-FMR-IMAGE-HL-05
#[test]
fn fm_image_graphics_return_after_leaving_and_reentering_the_files_surface() {
    let frame_area = ratatui::layout::Rect::new(0, 0, 115, 20);
    let (mut server, client_rx) = headless_graphics_server(crate::kitty_graphics::HostCellSize {
        width_px: 8,
        height_px: 16,
    });
    let root = headless_server_showing_one_png(&mut server, "image-return", frame_area);
    let files_instance = server
        .app
        .state
        .stage
        .app_tab_instances()
        .next()
        .expect("the open Files surface owns a strip entry");

    let pump_until_ready = |server: &mut HeadlessServer| {
        let wait = LoadAwareDeadline::new(10, "an image decode");
        loop {
            let _ = server.handle_scheduled_tasks_headless(Instant::now(), false);
            if matches!(
                headless_image_preview_state(server),
                crate::fm::FmImagePreviewState::Ready { .. }
            ) {
                return;
            }
            wait.check_with(format_args!(
                "state {:?}",
                headless_image_preview_state(server)
            ));
            std::thread::yield_now();
        }
    };

    pump_until_ready(&mut server);
    server.render_and_stream();
    let first = read_server_frame(
        client_rx
            .recv_timeout(Duration::from_millis(500))
            .expect("first frame"),
    );
    assert!(
        places_an_image(&first.graphics),
        "baseline: the image must reach the client before any tab switch"
    );

    // Leave Files for the terminal workspace, exactly as opening a file in
    // a new tab does.
    server.app.state.show_terminal_workspace();
    server.render_and_stream();
    while client_rx.try_recv().is_ok() {}

    // Come back.
    assert!(
        server.app.state.activate_stage_instance(files_instance),
        "the resident Files instance stays activatable"
    );
    crate::ui::compute_view(&mut server.app.state, frame_area);
    pump_until_ready(&mut server);
    server.render_and_stream();

    let returned = read_server_frame(
        client_rx
            .recv_timeout(Duration::from_millis(500))
            .expect("a frame after returning to Files"),
    );
    assert!(
        places_an_image(&returned.graphics),
        "the image must be placed again after returning to Files, not only \
         on the first visit"
    );

    let _ = fs::remove_dir_all(root);
}

// A client that never reported its cell size gets no graphics and no
// panic.
//
// The fail-safe matters more than it looks: kitty scales to exactly fill
// whatever cell box it is given, so guessing a cell size would stretch the
// image rather than degrade gracefully. Sending nothing is the honest
// outcome.
//
// TP-FMR-IMAGE-HL-04
#[test]
fn unknown_cell_size_client_gets_no_graphics_and_no_panic() {
    let frame_area = ratatui::layout::Rect::new(0, 0, 115, 20);
    let (mut server, client_rx) =
        headless_graphics_server(crate::kitty_graphics::HostCellSize::default());
    let root = headless_server_showing_one_png(&mut server, "image-nocell", frame_area);

    assert_eq!(
        server.app.image_preview_cell_size,
        crate::kitty_graphics::HostCellSize::default(),
        "an unknown client cell size must not be replaced by a guess"
    );

    for _ in 0..5 {
        let _ = server.handle_scheduled_tasks_headless(Instant::now(), false);
    }
    server.render_and_stream();

    let frame = read_server_frame(
        client_rx
            .recv_timeout(Duration::from_millis(500))
            .expect("a frame still reaches the client"),
    );
    assert!(
        frame.graphics.is_empty(),
        "no cell size means no placement, not a stretched one"
    );

    let _ = fs::remove_dir_all(root);
}

/// Collect the `sync_*` / `refresh_*` calls a scheduler body makes.
///
/// Reads source text rather than instrumenting the call, because the thing
/// being guarded is precisely that a call is *absent* — and an absent call
/// leaves no runtime trace to observe.
fn scheduler_calls(source: &str, signature: &str) -> std::collections::BTreeSet<String> {
    let start = source
        .find(signature)
        .unwrap_or_else(|| panic!("scheduler signature not found: {signature}"));
    let mut closed = false;
    let mut statements: Vec<&str> = Vec::new();
    for line in source[start..].lines().skip(1) {
        // Both schedulers are methods in an `impl` block, so their body
        // ends at the first four-space closing brace.
        if line == "    }" {
            closed = true;
            break;
        }
        let code = line.split("//").next().unwrap_or("").trim();
        if !code.is_empty() {
            statements.push(code);
        }
    }
    assert!(closed, "scheduler body never closed: {signature}");

    // Flatten to one stream and close the gaps around `.`, because
    // rustfmt wraps long chains: `self.app\n    .refresh_x(...)`. A
    // line-by-line scan reports those as missing when they are present.
    let body = statements.join(" ").replace(" .", ".").replace(". ", ".");

    // The monolithic loop calls `self.sync_x()`; the headless one goes
    // through `self.app.sync_x()`. Both spellings mean the same step, so
    // both must be recognised — matching only `self.` silently yields an
    // empty set for the headless side.
    let mut calls = std::collections::BTreeSet::new();
    for prefix in ["self.app.", "self."] {
        let mut rest = body.as_str();
        while let Some(at) = rest.find(prefix) {
            let after = &rest[at + prefix.len()..];
            let end = after
                .find(|c: char| !c.is_alphanumeric() && c != '_')
                .unwrap_or(after.len());
            let name = &after[..end];
            // `tick_` joined the list after a sampler landed in the
            // monolithic scheduler only. Every test stayed green, the
            // binary was correct, and a live herdr drew `--` forever,
            // because the loop the server actually runs never called it.
            // The guard for that class existed and was one prefix too
            // narrow to see it.
            if after[end..].starts_with('(')
                && (name.starts_with("sync_")
                    || name.starts_with("refresh_")
                    || name.starts_with("tick_"))
            {
                calls.insert(name.to_string());
            }
            rest = after;
        }
    }
    calls
}

// The app-level drain is unit-tested; this pins the one line in the server
// loop that calls it, so a merge cannot leave queued presses unpublished
// while every unit test stays green. TP-INP-MOUSE-01
#[test]
fn the_server_loop_publishes_queued_pointer_presses() {
    const HEADLESS_SOURCE: &str = include_str!("../../headless.rs");
    // A real newline plus the indent keeps this pattern from matching
    // its own escaped spelling inside this test.
    const RUN: &str = "\n    pub async fn run(&mut self) -> io::Result<()> {";
    let run = HEADLESS_SOURCE.find(RUN).expect("the headless run loop");
    assert_eq!(
        HEADLESS_SOURCE.rfind(RUN),
        Some(run),
        "one run loop, so the search cannot land in another function"
    );
    let body = &HEADLESS_SOURCE[run + RUN.len()..];
    let next_fn = [
        "\n    pub ",
        "\n    pub(crate) ",
        "\n    fn ",
        "\n    async fn ",
    ]
    .iter()
    .filter_map(|marker| body.find(marker))
    .min()
    .expect("a function follows the run loop");
    let drain = body
        .find("self.app.drain_pane_pointer_events();")
        .expect("the run loop drains queued pointer presses into events");
    assert!(
        drain < next_fn,
        "the drain call belongs to the run loop itself, not a later function"
    );
}

// The server's scheduler is the only one since U2b removed the in-process
// loop (upstream 2db663b2). Until then this test compared the two, and a
// gap between them was how features died quietly in server mode: "Send
// with Tailscale" queued an intent no scheduler consumed, and a sampler
// that landed in one loop only left a live herdr drawing `--` forever.
// The class outlives the second loop — a merge that drops a step compiles
// and passes every unit test — so the step sets are frozen here, as the
// two loops agreed on them when one went away. A lost step or an unlisted
// new one fails until this list changes on purpose.
//
// TP-SRV-SCHED-PARITY-01
#[test]
fn the_server_scheduler_keeps_every_scheduled_step() {
    const HEADLESS_SOURCE: &str = include_str!("../../headless.rs");
    const RUNTIME_SOURCE: &str = include_str!("../../../app/runtime.rs");
    const SCHEDULER_STEPS: [&str; 16] = [
        "refresh_agent_notification_delivery_contexts",
        "refresh_preview_bindings_if_due",
        "refresh_projects_if_due",
        "refresh_tab_branches_if_due",
        "sync_agent_attachment_delivery",
        "sync_dormant_history_removals",
        "sync_external_agents",
        "sync_file_manager_agent_handoff_send",
        "sync_file_operation_worker",
        "sync_headless_animation_timer",
        "sync_pane_dormancy_sweep",
        "sync_pending_agent_resume_deadline",
        "sync_toast_deadline",
        "tick_clock",
        "tick_resource_sample",
        "tick_selection_autoscroll",
    ];
    const PER_DISPLAY_STEPS: [&str; 9] = [
        "sync_agent_reference_picker",
        "sync_file_manager_agent_handoff",
        "sync_file_manager_io_results",
        "sync_file_manager_location_request",
        "sync_file_manager_plugin_action",
        "sync_file_manager_requests",
        "sync_file_manager_watcher_at",
        "sync_file_preview_worker",
        "sync_image_preview_worker",
    ];
    let expected = |steps: &[&str]| -> std::collections::BTreeSet<String> {
        steps.iter().map(|step| (*step).to_string()).collect()
    };

    assert_eq!(
        scheduler_calls(
            HEADLESS_SOURCE,
            "fn handle_scheduled_tasks_headless(&mut self"
        ),
        expected(&SCHEDULER_STEPS),
        "the server scheduler's steps changed: a lost step is a feature that stops in \
         server mode, and a new one must be listed here on purpose"
    );
    assert_eq!(
        scheduler_calls(
            RUNTIME_SOURCE,
            "pub(crate) fn sync_per_display_surfaces(&mut self"
        ),
        expected(&PER_DISPLAY_STEPS),
        "the per-display pass's steps changed"
    );

    // The pass only counts if the scheduler runs it, once per display.
    let at = HEADLESS_SOURCE
        .find("fn handle_scheduled_tasks_headless(&mut self")
        .expect("the server scheduler");
    let body = &HEADLESS_SOURCE[at..];
    let body = &body[..body.find("\n    }\n").expect("the scheduler body closes")];
    // rustfmt wraps the call, so compare without whitespace.
    let flat: String = body.split_whitespace().collect();
    assert!(
        flat.contains("self.app.for_each_display(|app|app.sync_per_display_surfaces(now))"),
        "the server scheduler must run the per-display pass inside for_each_display"
    );
}

/// Install a one-action file plugin into a server's app and return its root.
#[cfg(unix)]
fn headless_link_file_action_plugin(server: &mut HeadlessServer, label: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "headless-{}-{}-{}",
        label,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    fs::create_dir_all(&root).expect("create plugin root");
    fs::write(
        root.join("herdr-plugin.toml"),
        r#"
id = "example.fm-headless"
name = "FM Headless"
version = "0.1.0"
min_herdr_version = "0.6.10"

[[actions]]
id = "inspect"
title = "Inspect"
contexts = ["file"]
command = ["sh", "-c", "printf '%s' \"$HERDR_PLUGIN_ACTION_ID\""]
"#,
    )
    .expect("write plugin manifest");

    let linked = server.app.handle_api_request(crate::api::schema::Request {
        id: "link".into(),
        method: crate::api::schema::Method::PluginLink(crate::api::schema::PluginLinkParams {
            path: root.display().to_string(),
            enabled: true,
            source: None,
        }),
    });
    assert!(linked.contains("plugin_linked"), "expected link: {linked}");

    root
}

// A file-manager plugin intent chosen from the context menu is consumed and
// executed by the headless scheduled loop, exactly as by the monolithic one.
//
// Without this the right-click menu is dead over a socket: the menu builds,
// the intent is prepared, and then nothing ever consumes it. The monolithic
// loop has driven this since the feature landed (TP-C6.3-AUTHORITY); the
// server loop never did.
//
// TP-FMR-PLUGIN-HL-01
#[cfg(unix)]
#[test]
fn headless_scheduler_runs_file_manager_plugin_intent_once() {
    let mut server = test_headless_server();
    let plugin_root = headless_link_file_action_plugin(&mut server, "plugin-intent");

    let files_root = std::env::temp_dir().join(format!(
        "headless-plugin-files-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    fs::create_dir_all(&files_root).expect("create files root");
    let selected = files_root.join("selected.txt");
    fs::write(&selected, b"selected").expect("write selected file");

    let mut file_manager = crate::fm::FmState::new(&files_root);
    assert!(file_manager.replace_selection(0));
    server
        .app
        .state
        .try_open_file_manager_with(|_| Some(file_manager))
        .expect("Files activation");
    server.app.state.request_file_manager_context_action =
        Some(crate::app::state::FileManagerContextActionIntent {
            action: crate::app::state::FileManagerContextMenuAction::Plugin {
                plugin_id: "example.fm-headless".into(),
                action_id: "inspect".into(),
            },
            paths: vec![selected.clone()],
        });

    let _ = server.handle_scheduled_tasks_headless(Instant::now(), false);

    assert!(
        server
            .app
            .state
            .request_file_manager_context_action
            .is_none(),
        "the headless scheduler must consume the typed plugin intent"
    );
    assert_eq!(
        server.app.state.plugin_command_logs.len(),
        1,
        "the intent must reach the existing command runtime"
    );
    assert_eq!(
        server.app.state.plugin_command_logs[0].action_id.as_deref(),
        Some("inspect")
    );

    // A second round must not re-run it: the intent is one-shot.
    let _ = server.handle_scheduled_tasks_headless(Instant::now(), false);
    assert_eq!(
        server.app.state.plugin_command_logs.len(),
        1,
        "a consumed intent cannot run twice"
    );

    let _ = fs::remove_dir_all(files_root);
    let _ = fs::remove_dir_all(plugin_root);
}

// An intent whose file manager has since closed is consumed but not run.
//
// Closing retires the authority (TP-C6.3-LIFECYCLE). If the server loop
// consumed without revalidating, a menu choice prepared before close would
// execute after a same-directory reopen against paths that only look the
// same.
//
// TP-FMR-PLUGIN-HL-02
#[cfg(unix)]
#[test]
fn headless_scheduler_drops_plugin_intent_whose_files_surface_closed() {
    let mut server = test_headless_server();
    let plugin_root = headless_link_file_action_plugin(&mut server, "plugin-stale");

    server.app.state.request_file_manager_context_action =
        Some(crate::app::state::FileManagerContextActionIntent {
            action: crate::app::state::FileManagerContextMenuAction::Plugin {
                plugin_id: "example.fm-headless".into(),
                action_id: "inspect".into(),
            },
            paths: vec![PathBuf::from("/herdr-test-missing-fm-root/selected.txt")],
        });

    let _ = server.handle_scheduled_tasks_headless(Instant::now(), false);

    assert!(
        server
            .app
            .state
            .request_file_manager_context_action
            .is_none(),
        "a stale intent is still consumed, so it cannot linger and fire later"
    );
    assert!(
        server.app.state.plugin_command_logs.is_empty(),
        "an intent with no live Files authority must not execute"
    );

    let _ = fs::remove_dir_all(plugin_root);
}

#[test]
fn semantic_client_escape_closes_keybind_help() {
    let mut server = test_headless_server();
    server.app.state.mode = crate::app::Mode::KeybindHelp;
    server.clients.insert(
        1,
        ClientConnection::app(
            (100, 30),
            crate::kitty_graphics::HostCellSize::default(),
            crate::terminal_theme::TerminalTheme::default(),
            Some(true),
            1,
            RenderEncoding::SemanticFrame,
            None,
        ),
    );
    server.foreground_client_id = Some(1);
    server.sync_foreground_client_state();
    server.resize_shared_runtime_to_effective_size();

    assert!(server.handle_server_event(ServerEvent::ClientInputEvents {
        client_id: 1,
        events: vec![crate::protocol::ClientInputEvent::Key {
            code: crate::protocol::ClientKeyCode::Esc,
            modifiers: 0,
            kind: crate::protocol::ClientKeyKind::Press,
            repeat_count: 1,
            generated_text: None,
            source: crate::protocol::ClientKeySource::Synthesized,
        }],
    }));

    assert_eq!(server.app.state.mode, crate::app::Mode::Navigate);
}

#[test]
fn semantic_client_down_scrolls_keybind_help() {
    let mut server = test_headless_server();
    server.app.state.mode = crate::app::Mode::KeybindHelp;
    server.clients.insert(
        1,
        ClientConnection::app(
            (100, 30),
            crate::kitty_graphics::HostCellSize::default(),
            crate::terminal_theme::TerminalTheme::default(),
            Some(true),
            1,
            RenderEncoding::SemanticFrame,
            None,
        ),
    );
    server.foreground_client_id = Some(1);
    server.sync_foreground_client_state();
    server.resize_shared_runtime_to_effective_size();

    assert!(server.app.state.keybind_help_max_scroll() > 0);
    assert!(server.handle_server_event(ServerEvent::ClientInputEvents {
        client_id: 1,
        events: vec![crate::protocol::ClientInputEvent::Key {
            code: crate::protocol::ClientKeyCode::Down,
            modifiers: 0,
            kind: crate::protocol::ClientKeyKind::Press,
            repeat_count: 1,
            generated_text: None,
            source: crate::protocol::ClientKeySource::Synthesized,
        }],
    }));

    assert_eq!(server.app.state.mode, crate::app::Mode::KeybindHelp);
    assert_eq!(server.app.state.keybind_help.scroll, 1);
}

#[tokio::test]
async fn split_default_background_response_updates_theme_without_forwarding_tail() {
    let mut server = test_headless_server();
    let mut workspace = crate::workspace::Workspace::test_new("test");
    let focused = workspace.focused_pane_id().unwrap();
    let (runtime, mut rx) = crate::terminal::TerminalRuntime::test_with_channel_capacity(80, 24, 1);
    workspace.tabs[0].runtimes.insert(focused, runtime);
    server.app.state.workspaces = vec![workspace];
    server.app.state.active = Some(0);
    server.app.state.selected = 0;
    server.app.state.mode = crate::app::Mode::Terminal;
    server.clients.insert(
        1,
        ClientConnection::app(
            (80, 24),
            crate::kitty_graphics::HostCellSize::default(),
            crate::terminal_theme::TerminalTheme::default(),
            Some(true),
            1,
            RenderEncoding::SemanticFrame,
            None,
        ),
    );
    server.foreground_client_id = Some(1);
    server.sync_foreground_client_state();

    let _ = server.handle_server_event(ServerEvent::ClientInput {
        client_id: 1,
        data: b"\x1b]".to_vec(),
    });
    assert!(rx.try_recv().is_err());

    assert!(server.handle_server_event(ServerEvent::ClientInput {
        client_id: 1,
        data: b"11;#123456\x07".to_vec(),
    }));

    assert!(rx.try_recv().is_err());
    assert_eq!(
        server.clients[&1].host_terminal_theme.background,
        Some(crate::terminal_theme::RgbColor {
            r: 0x12,
            g: 0x34,
            b: 0x56,
        })
    );
    assert_eq!(
        server.app.state.host_terminal_theme.background,
        Some(crate::terminal_theme::RgbColor {
            r: 0x12,
            g: 0x34,
            b: 0x56,
        })
    );
}

#[tokio::test]
async fn render_and_stream_uses_each_client_terminal_size() {
    let mut server = test_headless_server();
    let mut workspace = crate::workspace::Workspace::test_new("test");
    let active_pane = workspace.tabs[0].root_pane;
    let background_tab = workspace.test_add_tab(Some("background"));
    let background_pane = workspace.tabs[background_tab].root_pane;
    workspace.tabs[0].runtimes.insert(
        active_pane,
        crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 24, b"active"),
    );
    workspace.tabs[background_tab].runtimes.insert(
        background_pane,
        crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 24, b"background"),
    );
    server.app.state.workspaces = vec![workspace];
    server.app.state.active = Some(0);
    server.app.state.selected = 0;
    server.app.state.mode = crate::app::Mode::Terminal;

    let (desktop_tx, _desktop_control_rx, desktop_rx) = test_client_writer();
    let (mobile_tx, _mobile_control_rx, mobile_rx) = test_client_writer();

    server.clients.insert(
        1,
        ClientConnection::app(
            (120, 40),
            crate::kitty_graphics::HostCellSize::default(),
            crate::terminal_theme::TerminalTheme::default(),
            None,
            1,
            RenderEncoding::SemanticFrame,
            Some(desktop_tx),
        ),
    );
    server.clients.insert(
        2,
        ClientConnection::app(
            (44, 20),
            crate::kitty_graphics::HostCellSize::default(),
            crate::terminal_theme::TerminalTheme::default(),
            None,
            2,
            RenderEncoding::SemanticFrame,
            Some(mobile_tx),
        ),
    );
    server.foreground_client_id = Some(1);
    server.sync_foreground_client_state();
    server.resize_shared_runtime_to_effective_size();

    server.render_and_stream();

    let desktop_frame = read_server_frame(desktop_rx.recv().expect("desktop frame"));
    let mobile_frame = read_server_frame(mobile_rx.recv().expect("mobile frame"));

    assert_eq!((desktop_frame.width, desktop_frame.height), (120, 40));
    assert_eq!((mobile_frame.width, mobile_frame.height), (44, 20));
    let mobile_text = frame_text(&mobile_frame);
    let mut mobile_rows = mobile_text.lines();
    let mobile_header = mobile_rows.by_ref().take(2).collect::<String>();
    let mobile_surface = mobile_rows.collect::<String>();
    assert!(mobile_header.contains("test"), "header: {mobile_header:?}");
    assert!(
        mobile_surface.contains("active"),
        "surface: {mobile_surface:?}"
    );
    assert!(!mobile_surface.contains("background"));

    // The shipped sidebar is 30 columns wide since the Spaces tree
    // (TP-TREE-13); this test is about per-client sizing, not the width.
    let foreground_terminal_area = Rect::new(30, 1, 90, 39);
    assert_eq!(
        server.app.state.view.layout,
        crate::app::state::ViewLayout::Desktop
    );
    assert_eq!(server.app.state.view.mobile_header_rect, Rect::default());
    assert_eq!(
        server.app.state.view.terminal_area,
        foreground_terminal_area
    );

    // Both displays adopted the same tab, so it is sized to the smaller of
    // them rather than to whichever one is in the foreground. This
    // replaces the retired contract, where the latest active client drove
    // one shared size and the other display was letterboxed or clipped
    // depending on which one had been touched last.
    //
    // TP-MCF-SIZE-02
    let (active_rows, active_cols) =
        server.app.state.workspaces[0].tabs[0].runtimes[&active_pane].current_size();
    assert!(
        active_rows <= 20 && active_cols <= 44,
        "a tab both displays watch must fit the smaller one, got {active_rows}x{active_cols}"
    );

    // Nobody is watching the background tab, so render passes leave it
    // alone; it keeps the size the size-change event path last gave it
    // (here, its spawn size). TP-MCF-SIZE-03
    let (background_rows, background_cols) = server.app.state.workspaces[0].tabs[background_tab]
        .runtimes[&background_pane]
        .current_size();
    assert!(
        background_rows > 0 && background_cols > 0,
        "an unwatched tab still gets a usable size"
    );
}

fn pointer_move(column: u16, row: u16) -> crate::raw_input::RawInputEvent {
    crate::raw_input::RawInputEvent::Mouse(crossterm::event::MouseEvent {
        kind: crossterm::event::MouseEventKind::Moved,
        column,
        row,
        modifiers: crossterm::event::KeyModifiers::empty(),
    })
}

fn wheel_up(column: u16, row: u16) -> crate::raw_input::RawInputEvent {
    crate::raw_input::RawInputEvent::Mouse(crossterm::event::MouseEvent {
        kind: crossterm::event::MouseEventKind::ScrollUp,
        column,
        row,
        modifiers: crossterm::event::KeyModifiers::empty(),
    })
}

/// TP-MOB-74: a phone attached beside a desktop still gets the phone's
/// wheel rule. The rule reads `view.layout`, and the view is one shared
/// structure that the last render leaves behind — so with a wide display
/// drawing last, a swipe from the narrow one was decided against the wide
/// display's geometry and went to the agent, which discards it. The
/// reader saw touch scrolling stop working the moment a second machine
/// attached.
#[tokio::test]
async fn a_narrow_display_keeps_its_wheel_rule_while_a_wide_one_draws() {
    let mut server = test_headless_server();
    let mut workspace = crate::workspace::Workspace::test_new("test");
    let pane_id = workspace.tabs[0].root_pane;
    let mut bytes = b"\x1b[?1000h\x1b[?1006h".to_vec();
    for line in 0..80 {
        bytes.extend_from_slice(format!("line {line:02}\r\n").as_bytes());
    }
    let (runtime, _input_rx) =
        crate::terminal::TerminalRuntime::test_with_channel_and_scrollback_bytes(
            58,
            16,
            16 * 1024,
            &bytes,
            4,
        );
    workspace.insert_test_runtime(pane_id, runtime);
    server.app.state.workspaces = vec![workspace];
    server.app.state.active = Some(0);
    server.app.state.selected = 0;
    server.app.state.mode = crate::app::Mode::Terminal;
    server.app.state.mouse_scroll_lines = 3;

    let (wide_tx, _wide_control_rx, _wide_rx) = test_client_writer();
    let (narrow_tx, _narrow_control_rx, _narrow_rx) = test_client_writer();
    server.clients.insert(
        1,
        ClientConnection::app(
            (200, 50),
            crate::kitty_graphics::HostCellSize::default(),
            crate::terminal_theme::TerminalTheme::default(),
            None,
            1,
            RenderEncoding::SemanticFrame,
            Some(wide_tx),
        ),
    );
    server.clients.insert(
        2,
        ClientConnection::app(
            (60, 20),
            crate::kitty_graphics::HostCellSize::default(),
            crate::terminal_theme::TerminalTheme::default(),
            None,
            2,
            RenderEncoding::SemanticFrame,
            Some(narrow_tx),
        ),
    );
    // The wide display is in front, so it is the geometry the last frame
    // leaves behind.
    server.foreground_client_id = Some(1);
    server.render_and_stream();
    assert_eq!(
        server.app.state.view.layout,
        crate::app::state::ViewLayout::Desktop,
        "the wide display drew last"
    );

    let before = server
        .app
        .state
        .runtime_for_pane_in_workspace(&server.app.terminal_runtimes, 0, pane_id)
        .and_then(crate::terminal::TerminalRuntime::scroll_metrics)
        .map(|metrics| metrics.offset_from_bottom)
        .expect("scroll metrics");

    server.handle_client_input_events(2, vec![wheel_up(4, 6)]);

    let after = server
        .app
        .state
        .runtime_for_pane_in_workspace(&server.app.terminal_runtimes, 0, pane_id)
        .and_then(crate::terminal::TerminalRuntime::scroll_metrics)
        .map(|metrics| metrics.offset_from_bottom)
        .expect("scroll metrics");
    assert!(
        after > before,
        "the narrow display's swipe scrolled nothing: {before} -> {after}"
    );
}

/// Correct hit geometry must not cost a layout pass per pointer motion.
/// The input loop is serial, so that cost lands directly on input latency
/// — which is what the inert-motion render gate exists to avoid.
///
/// TP-MCF-VIEW-02
#[tokio::test]
async fn a_pointer_burst_recomputes_geometry_once_and_the_foreground_never_pays() {
    let mut server = test_headless_server();
    let mut workspace = crate::workspace::Workspace::test_new("test");
    workspace.test_add_tab(Some("second"));
    server.app.state.workspaces = vec![workspace];
    server.app.state.active = Some(0);
    server.app.state.selected = 0;
    server.app.state.mode = crate::app::Mode::Terminal;

    let (wide_tx, _wide_control_rx, _wide_rx) = test_client_writer();
    let (narrow_tx, _narrow_control_rx, _narrow_rx) = test_client_writer();
    server.clients.insert(
        1,
        ClientConnection::app(
            (200, 50),
            crate::kitty_graphics::HostCellSize::default(),
            crate::terminal_theme::TerminalTheme::default(),
            None,
            1,
            RenderEncoding::SemanticFrame,
            Some(wide_tx),
        ),
    );
    server.clients.insert(
        2,
        ClientConnection::app(
            (60, 20),
            crate::kitty_graphics::HostCellSize::default(),
            crate::terminal_theme::TerminalTheme::default(),
            None,
            2,
            RenderEncoding::SemanticFrame,
            Some(narrow_tx),
        ),
    );
    server.foreground_client_id = Some(1);
    server.render_and_stream();

    // The frame leaves the geometry with the display that drew last, which
    // is the foreground one. Its own pointer motion needs no recompute.
    server.view_recomputes_for_input = 0;
    for step in 0..8 {
        server.handle_client_input_events(1, vec![pointer_move(step + 1, 1)]);
    }
    assert_eq!(
        server.view_recomputes_for_input, 0,
        "the foreground display's pointer motion must be free"
    );

    // Another display pays once for the burst, not once per motion.
    for step in 0..8 {
        server.handle_client_input_events(2, vec![pointer_move(step + 1, 1)]);
    }
    assert_eq!(
        server.view_recomputes_for_input, 1,
        "a pointer burst from one display recomputes once and then rides the ownership"
    );
}

/// Each display draws the tab it is on, at the same moment, in one frame
/// pass. This is the whole feature seen from the outside: five tabs on
/// five monitors, all live.
///
/// TP-MCF-UI-01
#[tokio::test]
async fn every_display_draws_its_own_tab_in_the_same_frame() {
    let mut server = test_headless_server();
    let mut workspace = crate::workspace::Workspace::test_new("test");
    let first_pane = workspace.tabs[0].root_pane;
    let second_tab = workspace.test_add_tab(Some("second"));
    let second_pane = workspace.tabs[second_tab].root_pane;
    workspace.tabs[0].runtimes.insert(
        first_pane,
        crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 24, b"AGENT-ALPHA"),
    );
    workspace.tabs[second_tab].runtimes.insert(
        second_pane,
        crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 24, b"AGENT-BETA"),
    );
    server.app.state.workspaces = vec![workspace];
    server.app.state.active = Some(0);
    server.app.state.selected = 0;
    server.app.state.mode = crate::app::Mode::Terminal;

    let (one_tx, _one_control_rx, one_rx) = test_client_writer();
    let (two_tx, _two_control_rx, two_rx) = test_client_writer();
    server.clients.insert(
        1,
        ClientConnection::app(
            (120, 40),
            crate::kitty_graphics::HostCellSize::default(),
            crate::terminal_theme::TerminalTheme::default(),
            None,
            1,
            RenderEncoding::SemanticFrame,
            Some(one_tx),
        ),
    );
    server.clients.insert(
        2,
        ClientConnection::app(
            (120, 40),
            crate::kitty_graphics::HostCellSize::default(),
            crate::terminal_theme::TerminalTheme::default(),
            None,
            2,
            RenderEncoding::SemanticFrame,
            Some(two_tx),
        ),
    );
    server.foreground_client_id = Some(1);

    // Both displays attach on the same tab, then the second moves.
    server.render_and_stream();
    while one_rx.try_recv().is_ok() {}
    while two_rx.try_recv().is_ok() {}

    let previous = server.app.state.enter_viewer(Some(2));
    server.app.state.workspaces[0].set_active_tab(second_tab);
    server.app.state.restore_viewer(previous);

    server.render_and_stream();

    // The second display moved, so it gets a frame. The first display did
    // not, so identical-frame suppression may send it nothing at all --
    // and that silence is itself the proof it was not dragged along.
    let two_text = frame_text(&read_server_frame(two_rx.recv().expect("second frame")));
    assert!(
        two_text.contains("AGENT-BETA"),
        "the second display draws the tab it moved to: {two_text:?}"
    );
    assert!(
        !two_text.contains("AGENT-ALPHA"),
        "the second display must not still be showing the first display's agent"
    );

    match one_rx.try_recv() {
        Ok(frame) => {
            let one_text = frame_text(&read_server_frame(frame));
            assert!(
                one_text.contains("AGENT-ALPHA"),
                "the first display keeps drawing its own agent: {one_text:?}"
            );
            assert!(
                !one_text.contains("AGENT-BETA"),
                "the first display must not be pulled onto the tab the second opened"
            );
        }
        Err(_) => {
            // No frame means nothing about that display changed. Confirm
            // that from state as well, so an unrelated send failure cannot
            // masquerade as a pass.
            let previous = server.app.state.enter_viewer(Some(1));
            let tab = server.app.state.workspaces[0].active_tab_index();
            server.app.state.restore_viewer(previous);
            assert_eq!(tab, 0, "the untouched display is still on its own tab");
        }
    }
}

// TP-MCF-SIZE-01
#[tokio::test]
async fn each_display_sizes_the_tab_it_alone_is_watching() {
    let mut server = test_headless_server();
    let mut workspace = crate::workspace::Workspace::test_new("test");
    let first_pane = workspace.tabs[0].root_pane;
    let second_tab = workspace.test_add_tab(Some("second"));
    let second_pane = workspace.tabs[second_tab].root_pane;
    workspace.tabs[0].runtimes.insert(
        first_pane,
        crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 24, b"first"),
    );
    workspace.tabs[second_tab].runtimes.insert(
        second_pane,
        crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 24, b"second"),
    );
    server.app.state.workspaces = vec![workspace];
    server.app.state.active = Some(0);
    server.app.state.selected = 0;
    server.app.state.mode = crate::app::Mode::Terminal;

    let (wide_tx, _wide_control_rx, _wide_rx) = test_client_writer();
    let (narrow_tx, _narrow_control_rx, _narrow_rx) = test_client_writer();
    server.clients.insert(
        1,
        ClientConnection::app(
            (200, 50),
            crate::kitty_graphics::HostCellSize::default(),
            crate::terminal_theme::TerminalTheme::default(),
            None,
            1,
            RenderEncoding::SemanticFrame,
            Some(wide_tx),
        ),
    );
    server.clients.insert(
        2,
        ClientConnection::app(
            (60, 20),
            crate::kitty_graphics::HostCellSize::default(),
            crate::terminal_theme::TerminalTheme::default(),
            None,
            2,
            RenderEncoding::SemanticFrame,
            Some(narrow_tx),
        ),
    );
    server.foreground_client_id = Some(1);

    // Both displays attach and adopt the tab the session is on. Doing this
    // before the switch matters: a display takes its own tab the first
    // time it is seen, so moving one of them beforehand would hand the
    // other the same tab through the default.
    server.render_and_stream();

    // Now put the narrow display on its own tab. Each tab is watched by
    // exactly one display, which is the case that must cost nothing.
    let previous = server.app.state.enter_viewer(Some(2));
    server.app.state.workspaces[0].set_active_tab(second_tab);
    server.app.state.restore_viewer(previous);

    server.render_and_stream();

    let (wide_rows, wide_cols) =
        server.app.state.workspaces[0].tabs[0].runtimes[&first_pane].current_size();
    let (narrow_rows, narrow_cols) =
        server.app.state.workspaces[0].tabs[second_tab].runtimes[&second_pane].current_size();

    assert!(
        wide_cols > 60,
        "the tab only the wide display watches keeps the wide width, got {wide_cols}"
    );
    assert!(
        wide_rows > 20,
        "the tab only the wide display watches keeps the wide height, got {wide_rows}"
    );
    assert!(
        narrow_cols <= 60 && narrow_rows <= 20,
        "the tab only the narrow display watches fits the narrow display, got {narrow_rows}x{narrow_cols}"
    );
}

// TP-PANE-RETIRED-02
#[tokio::test]
async fn a_handoff_pane_whose_process_is_gone_counts_as_retired() {
    // A pane imported through a live handoff carries no reaping record --
    // `from_handoff_fd` keeps the child's pid but leaves
    // `child_wait_completed` empty, because the task that would have set it
    // died with the previous server. Every pane in a session that has been
    // updated in place looks like this, so the record alone would report
    // every one of them as still running forever.
    let live = crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 24, b"live");
    assert!(
        !live.child_exited(),
        "a pane with no pid and no record must read as live -- the safe answer"
    );

    let departed = crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 24, b"departed");
    // Above every possible pid on any supported platform, so the process
    // cannot exist and the lookup is the only thing that can answer.
    departed.test_set_child_pid(u32::from(u16::MAX) * 2 + 1);
    assert!(
        departed.child_exited(),
        "with no reaping record, a pid the OS does not know must read as gone"
    );
}

// TP-PANE-RETIRED-01
#[tokio::test]
async fn a_background_pane_whose_child_exited_is_not_resized() {
    let mut server = test_headless_server();
    let mut workspace = crate::workspace::Workspace::test_new("test");
    let active_pane = workspace.tabs[0].root_pane;
    let background_tab = workspace.test_add_tab(Some("background"));
    let background_pane = workspace.tabs[background_tab].root_pane;
    workspace.tabs[0].runtimes.insert(
        active_pane,
        crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 24, b"live"),
    );
    // Identical to its neighbour in every way except that its child has
    // already been reaped.
    let mut retired = crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 24, b"retired");
    retired.test_mark_child_exited();
    workspace.tabs[background_tab]
        .runtimes
        .insert(background_pane, retired);
    server.app.state.workspaces = vec![workspace];
    server.app.state.active = Some(0);
    server.app.state.selected = 0;
    server.app.state.mode = crate::app::Mode::Terminal;

    server.clients.insert(
        1,
        ClientConnection::app(
            (120, 40),
            crate::kitty_graphics::HostCellSize::default(),
            crate::terminal_theme::TerminalTheme::default(),
            None,
            1,
            RenderEncoding::SemanticFrame,
            None,
        ),
    );
    server.foreground_client_id = Some(1);
    server.sync_foreground_client_state();
    server.resize_shared_runtime_to_effective_size();

    let before = server.app.state.workspaces[0].tabs[background_tab].runtimes[&background_pane]
        .applied_resizes_for_test();

    // A second size-change event, to a size nothing has been sized to yet.
    if let Some(client) = server.clients.get_mut(&1) {
        client.terminal_size = (100, 30);
    }
    server.sync_foreground_client_state();
    server.resize_shared_runtime_to_effective_size();

    let after = server.app.state.workspaces[0].tabs[background_tab].runtimes[&background_pane]
        .applied_resizes_for_test();
    let (active_rows, active_cols) =
        server.app.state.workspaces[0].tabs[0].runtimes[&active_pane].current_size();

    assert_eq!(
        after,
        before,
        "a pane whose child is gone must not be resized while it is out of \
         sight: no process is left to receive the size, and the reflow rewraps \
         its whole scrollback for nobody ({} applied)",
        after - before
    );
    assert_eq!(
        (active_rows, active_cols),
        (
            server.app.state.view.terminal_area.height,
            server.app.state.view.terminal_area.width.saturating_sub(1)
        ),
        "the tab being looked at still follows the session size"
    );
}

// TP-MCF-SIZE-05
#[tokio::test]
async fn same_tab_index_in_different_workspaces_sizes_independently() {
    let mut server = test_headless_server();
    let mut left = crate::workspace::Workspace::test_new("left");
    let left_pane = left.tabs[0].root_pane;
    left.tabs[0].runtimes.insert(
        left_pane,
        crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 24, b"left"),
    );
    let mut right = crate::workspace::Workspace::test_new("right");
    let right_pane = right.tabs[0].root_pane;
    right.tabs[0].runtimes.insert(
        right_pane,
        crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 24, b"right"),
    );
    server.app.state.workspaces = vec![left, right];
    server.app.state.active = Some(0);
    server.app.state.selected = 0;
    server.app.state.mode = crate::app::Mode::Terminal;

    let (wide_tx, _wide_control_rx, _wide_rx) = test_client_writer();
    let (narrow_tx, _narrow_control_rx, _narrow_rx) = test_client_writer();
    server.clients.insert(
        1,
        ClientConnection::app(
            (200, 50),
            crate::kitty_graphics::HostCellSize::default(),
            crate::terminal_theme::TerminalTheme::default(),
            None,
            1,
            RenderEncoding::SemanticFrame,
            Some(wide_tx),
        ),
    );
    server.clients.insert(
        2,
        ClientConnection::app(
            (60, 20),
            crate::kitty_graphics::HostCellSize::default(),
            crate::terminal_theme::TerminalTheme::default(),
            None,
            2,
            RenderEncoding::SemanticFrame,
            Some(narrow_tx),
        ),
    );
    server.foreground_client_id = Some(1);

    // Both displays attach on the session's workspace, then the narrow one
    // moves to the other. Each workspace has exactly one tab, so both
    // displays are now watching a tab whose index is 0 -- in different
    // workspaces.
    server.render_and_stream();
    server.app.state.enter_viewer(Some(2));
    server.app.state.active = Some(1);
    server.app.state.restore_viewer(None);

    server.render_and_stream();

    let (_, left_cols) = server.app.state.workspaces[0].tabs[0].runtimes[&left_pane].current_size();
    let (_, right_cols) =
        server.app.state.workspaces[1].tabs[0].runtimes[&right_pane].current_size();

    assert!(
        left_cols > 60,
        "the wide display's workspace keeps the wide width; a tab index is not \
         an identity across workspaces, so the narrow display's tab 0 must not \
         drag it down, got {left_cols}"
    );
    assert!(
        right_cols <= 60,
        "the narrow display's workspace fits the narrow display, got {right_cols}"
    );
}

// TP-MCF-SIZE-04
#[tokio::test]
async fn moving_input_between_displays_costs_no_background_resize() {
    let mut server = test_headless_server();
    let mut workspace = crate::workspace::Workspace::test_new("test");
    let first_pane = workspace.tabs[0].root_pane;
    let background_tab = workspace.test_add_tab(Some("background"));
    let background_pane = workspace.tabs[background_tab].root_pane;
    for (tab, pane) in [(0, first_pane), (background_tab, background_pane)] {
        workspace.tabs[tab].runtimes.insert(
            pane,
            crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 24, b"x"),
        );
    }
    server.app.state.workspaces = vec![workspace];
    server.app.state.active = Some(0);
    server.app.state.selected = 0;
    server.app.state.mode = crate::app::Mode::Terminal;

    let (wide_tx, _wide_control_rx, _wide_rx) = test_client_writer();
    let (narrow_tx, _narrow_control_rx, _narrow_rx) = test_client_writer();
    server.clients.insert(
        1,
        ClientConnection::app(
            (200, 50),
            crate::kitty_graphics::HostCellSize::default(),
            crate::terminal_theme::TerminalTheme::default(),
            None,
            1,
            RenderEncoding::SemanticFrame,
            Some(wide_tx),
        ),
    );
    server.clients.insert(
        2,
        ClientConnection::app(
            (60, 20),
            crate::kitty_graphics::HostCellSize::default(),
            crate::terminal_theme::TerminalTheme::default(),
            None,
            2,
            RenderEncoding::SemanticFrame,
            Some(narrow_tx),
        ),
    );
    server.foreground_client_id = Some(1);
    server.sync_foreground_client_state();
    // A real size-change event settles every tab, background ones included.
    server.resize_shared_runtime_to_effective_size();
    server.render_and_stream();

    // Typing in the other window makes it foreground. That is a change of
    // input focus, not a change of session geometry: the tab being typed
    // in follows the new display, but a tab nobody is watching has no
    // reason to be rewritten -- and rewriting it reflows its scrollback.
    let before = server.app.state.workspaces[0].tabs[background_tab].runtimes[&background_pane]
        .applied_resizes_for_test();
    for client_id in [2, 1, 2] {
        if server.promote_client_to_foreground(client_id) {
            server.resize_shared_runtime_to_effective_size_before_input();
        }
    }
    let after = server.app.state.workspaces[0].tabs[background_tab].runtimes[&background_pane]
        .applied_resizes_for_test();

    assert_eq!(
        after,
        before,
        "moving input focus between displays must not resize an unwatched tab; \
         it was rewritten to each display's geometry in turn, reflowing its whole \
         scrollback every time ({} resizes over three focus changes)",
        after - before
    );
}

// TP-MCF-SIZE-03
#[tokio::test]
async fn a_steady_frame_costs_no_background_resize() {
    let mut server = test_headless_server();
    let mut workspace = crate::workspace::Workspace::test_new("test");
    let first_pane = workspace.tabs[0].root_pane;
    let second_tab = workspace.test_add_tab(Some("second"));
    let second_pane = workspace.tabs[second_tab].root_pane;
    let background_tab = workspace.test_add_tab(Some("background"));
    let background_pane = workspace.tabs[background_tab].root_pane;
    for (tab, pane) in [
        (0, first_pane),
        (second_tab, second_pane),
        (background_tab, background_pane),
    ] {
        workspace.tabs[tab].runtimes.insert(
            pane,
            crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 24, b"x"),
        );
    }
    server.app.state.workspaces = vec![workspace];
    server.app.state.active = Some(0);
    server.app.state.selected = 0;
    server.app.state.mode = crate::app::Mode::Terminal;

    // Two displays of different sizes. The third tab is watched by neither,
    // so it is swept as a background tab by both render passes.
    let (wide_tx, _wide_control_rx, _wide_rx) = test_client_writer();
    let (narrow_tx, _narrow_control_rx, _narrow_rx) = test_client_writer();
    server.clients.insert(
        1,
        ClientConnection::app(
            (200, 50),
            crate::kitty_graphics::HostCellSize::default(),
            crate::terminal_theme::TerminalTheme::default(),
            None,
            1,
            RenderEncoding::SemanticFrame,
            Some(wide_tx),
        ),
    );
    server.clients.insert(
        2,
        ClientConnection::app(
            (60, 20),
            crate::kitty_graphics::HostCellSize::default(),
            crate::terminal_theme::TerminalTheme::default(),
            None,
            2,
            RenderEncoding::SemanticFrame,
            Some(narrow_tx),
        ),
    );
    server.foreground_client_id = Some(1);

    // Let both displays attach and adopt a tab, then put the narrow one on
    // its own tab so each display watches a different tab and the third is
    // background for both.
    server.render_and_stream();
    let previous = server.app.state.enter_viewer(Some(2));
    server.app.state.workspaces[0].set_active_tab(second_tab);
    server.app.state.restore_viewer(previous);
    server.render_and_stream();

    // Everything has settled: no input, no output, no size change. From here
    // a frame must cost nothing on a tab nobody is watching.
    let before = server.app.state.workspaces[0].tabs[background_tab].runtimes[&background_pane]
        .applied_resizes_for_test();
    server.render_and_stream();
    server.render_and_stream();
    let after = server.app.state.workspaces[0].tabs[background_tab].runtimes[&background_pane]
        .applied_resizes_for_test();

    assert_eq!(
        after,
        before,
        "a background tab must not be resized by a steady frame; each display was \
         rewriting it to its own geometry, and every one of those reflows the whole \
         scrollback (applied {} resizes over two idle frames)",
        after - before
    );
}

// TP-MCF-SIZE-03 — the event-path residual: handing the foreground to a
// display of another shape is not a size-change event for tabs nobody
// watches. Only the input path pays it, so only the input path is pinned.
#[tokio::test]
async fn foreground_handoff_between_displays_does_not_resweep_background_tabs() {
    let mut server = test_headless_server();
    let mut workspace = crate::workspace::Workspace::test_new("test");
    let watched_pane = workspace.tabs[0].root_pane;
    let background_tab = workspace.test_add_tab(Some("background"));
    let background_pane = workspace.tabs[background_tab].root_pane;
    for (tab, pane) in [(0, watched_pane), (background_tab, background_pane)] {
        workspace.tabs[tab].runtimes.insert(
            pane,
            crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 24, b"x"),
        );
    }
    server.app.state.workspaces = vec![workspace];
    server.app.state.active = Some(0);
    server.app.state.selected = 0;
    server.app.state.mode = crate::app::Mode::Terminal;

    let (wide_tx, _wide_control_rx, _wide_rx) = test_client_writer();
    let (narrow_tx, _narrow_control_rx, _narrow_rx) = test_client_writer();
    server.clients.insert(
        1,
        ClientConnection::app(
            (200, 50),
            crate::kitty_graphics::HostCellSize::default(),
            crate::terminal_theme::TerminalTheme::default(),
            None,
            1,
            RenderEncoding::SemanticFrame,
            Some(wide_tx),
        ),
    );
    server.clients.insert(
        2,
        ClientConnection::app(
            (60, 20),
            crate::kitty_graphics::HostCellSize::default(),
            crate::terminal_theme::TerminalTheme::default(),
            None,
            2,
            RenderEncoding::SemanticFrame,
            Some(narrow_tx),
        ),
    );
    server.foreground_client_id = Some(1);
    server.sync_foreground_client_state();
    server.resize_shared_runtime_to_effective_size();
    server.render_and_stream();

    // Typing lands on the narrow display, then back on the wide one. The
    // foreground hand-off resizes the shared runtime for input, but the
    // background tab is watched by neither display and must not move.
    let before = server.app.state.workspaces[0].tabs[background_tab].runtimes[&background_pane]
        .applied_resizes_for_test();
    server.promote_client_to_foreground(2);
    server.resize_shared_runtime_to_effective_size_before_input();
    server.promote_client_to_foreground(1);
    server.resize_shared_runtime_to_effective_size_before_input();
    let after = server.app.state.workspaces[0].tabs[background_tab].runtimes[&background_pane]
        .applied_resizes_for_test();

    assert_eq!(
        after,
        before,
        "a foreground hand-off between displays of different shapes must \
         not resweep background tabs (applied {} resizes across two \
         hand-offs); each one reflows that pane's whole scrollback",
        after - before
    );
}

// Negotiation is per tab, not per tab *index*: a session with two
// workspaces has two tabs at index zero, and merging them would size a
// watched tab to a display that is looking at a different workspace.
#[tokio::test]
async fn tabs_in_different_workspaces_do_not_share_a_size_negotiation() {
    let mut server = test_headless_server();
    let mut left = crate::workspace::Workspace::test_new("left");
    let left_pane = left.tabs[0].root_pane;
    left.tabs[0].runtimes.insert(
        left_pane,
        crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 24, b"left"),
    );
    let mut right = crate::workspace::Workspace::test_new("right");
    let right_pane = right.tabs[0].root_pane;
    right.tabs[0].runtimes.insert(
        right_pane,
        crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 24, b"right"),
    );
    server.app.state.workspaces = vec![left, right];
    server.app.state.active = Some(0);
    server.app.state.selected = 0;
    server.app.state.mode = crate::app::Mode::Terminal;

    let (wide_tx, _wide_control_rx, _wide_rx) = test_client_writer();
    let (narrow_tx, _narrow_control_rx, _narrow_rx) = test_client_writer();
    server.clients.insert(
        1,
        ClientConnection::app(
            (200, 50),
            crate::kitty_graphics::HostCellSize::default(),
            crate::terminal_theme::TerminalTheme::default(),
            None,
            1,
            RenderEncoding::SemanticFrame,
            Some(wide_tx),
        ),
    );
    server.clients.insert(
        2,
        ClientConnection::app(
            (60, 20),
            crate::kitty_graphics::HostCellSize::default(),
            crate::terminal_theme::TerminalTheme::default(),
            None,
            2,
            RenderEncoding::SemanticFrame,
            Some(narrow_tx),
        ),
    );
    server.foreground_client_id = Some(1);

    // Both displays attach on the left workspace, then the narrow one
    // moves to the right workspace. Each workspace's only tab sits at
    // index zero, watched by exactly one display.
    server.render_and_stream();
    let previous = server.app.state.enter_viewer(Some(2));
    server.app.state.active = Some(1);
    server.app.state.restore_viewer(previous);
    server.render_and_stream();

    let (left_rows, left_cols) =
        server.app.state.workspaces[0].tabs[0].runtimes[&left_pane].current_size();
    let (right_rows, right_cols) =
        server.app.state.workspaces[1].tabs[0].runtimes[&right_pane].current_size();

    assert!(
        left_cols > 60,
        "the tab only the wide display watches keeps the wide width even \
         though another workspace has a tab at the same index, got {left_cols}"
    );
    assert!(
        left_rows > 20,
        "the tab only the wide display watches keeps the wide height, got {left_rows}"
    );
    assert!(
        right_cols <= 60 && right_rows <= 20,
        "the tab only the narrow display watches fits the narrow display, \
         got {right_rows}x{right_cols}"
    );
}

// A frame nobody sees is not drawn: with no client attached the view is
// computed once to keep pane geometry alive for the API and the first
// attach, and PTY output after that stops producing thrown-away frames.
// TP-MCF-SIZE-06
#[tokio::test]
async fn frames_with_no_attached_client_are_computed_once_not_per_tick() {
    let mut server = test_headless_server();
    let mut workspace = crate::workspace::Workspace::test_new("test");
    let pane = workspace.tabs[0].root_pane;
    workspace.tabs[0].runtimes.insert(
        pane,
        crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 24, b"x"),
    );
    server.app.state.workspaces = vec![workspace];
    server.app.state.active = Some(0);
    server.app.state.selected = 0;
    server.app.state.mode = crate::app::Mode::Terminal;
    assert!(server.clients.is_empty());

    server.render_and_stream();
    server.render_and_stream();
    server.render_and_stream();

    assert_eq!(
        server.watcherless_virtual_frames, 1,
        "with no client attached, geometry is established once and later \
         ticks must not compute and discard full frames"
    );
    assert!(
        !server.app.state.view.pane_infos.is_empty(),
        "the one computed frame must still establish pane geometry"
    );
}

#[tokio::test]
async fn resize_shared_runtime_resizes_background_tabs() {
    let mut server = test_headless_server();
    let mut workspace = crate::workspace::Workspace::test_new("test");
    let background_tab = workspace.test_add_tab(Some("background"));
    let active_pane = workspace.tabs[0].root_pane;
    let background_pane = workspace.tabs[background_tab].root_pane;
    workspace.tabs[0].runtimes.insert(
        active_pane,
        crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 24, b""),
    );
    workspace.tabs[background_tab].runtimes.insert(
        background_pane,
        crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 24, b""),
    );
    server.app.state.workspaces = vec![workspace];
    server.app.state.active = Some(0);
    server.app.state.selected = 0;
    server.app.state.mode = crate::app::Mode::Terminal;

    server.clients.insert(
        1,
        ClientConnection::app(
            (120, 40),
            crate::kitty_graphics::HostCellSize::default(),
            crate::terminal_theme::TerminalTheme::default(),
            None,
            1,
            RenderEncoding::SemanticFrame,
            None,
        ),
    );
    server.foreground_client_id = Some(1);
    server.sync_foreground_client_state();
    server.resize_shared_runtime_to_effective_size();

    let terminal_area = server.app.state.view.terminal_area;
    let expected = (terminal_area.height, terminal_area.width.saturating_sub(1));
    assert_eq!(
        server
            .app
            .state
            .runtime_for_pane(&server.app.terminal_runtimes, active_pane)
            .unwrap()
            .current_size(),
        expected
    );
    assert_eq!(
        server
            .app
            .state
            .runtime_for_pane(&server.app.terminal_runtimes, background_pane)
            .unwrap()
            .current_size(),
        expected
    );
}

#[test]
fn terminal_attach_disconnect_restores_app_pane_size() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    let _runtime_guard = rt.enter();
    let mut server = test_headless_server();
    let workspace = crate::workspace::Workspace::test_new("test");
    let pane_id = workspace.tabs[0].root_pane;
    let terminal_id = workspace.terminal_id(pane_id).expect("terminal id").clone();
    let terminal_id_string = terminal_id.to_string();
    server.app.state.workspaces = vec![workspace];
    server.app.state.ensure_test_terminals();
    server.app.state.active = Some(0);
    server.app.state.selected = 0;
    server.app.state.mode = crate::app::Mode::Terminal;
    server.app.terminal_runtimes.insert(
        terminal_id.clone(),
        crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 24, b""),
    );
    server.clients.insert(
        1,
        ClientConnection::app(
            (120, 40),
            crate::kitty_graphics::HostCellSize::default(),
            crate::terminal_theme::TerminalTheme::default(),
            None,
            1,
            RenderEncoding::SemanticFrame,
            None,
        ),
    );
    server.foreground_client_id = Some(1);
    server.sync_foreground_client_state();
    server.resize_shared_runtime_to_effective_size();
    let expected_app_size = server
        .app
        .terminal_runtimes
        .get(&terminal_id)
        .expect("runtime")
        .current_size();
    assert_ne!(expected_app_size, (24, 80));

    let (writer, _control_rx, _render_rx) = test_client_writer();
    assert!(!server.handle_server_event(ServerEvent::ClientConnected {
        client_id: 2,
        cols: 80,
        rows: 24,
        cell_width_px: 0,
        cell_height_px: 0,
        render_encoding: RenderEncoding::TerminalAnsi,
        keybindings: None,
        direct_attach_requested: true,
        direct_graphics: false,
        media_listen: false,
        pixel_mouse: false,
        writer,
        capabilities: Vec::new(),
    }));
    assert!(
        server.handle_server_event(ServerEvent::ClientAttachTerminal {
            client_id: 2,
            terminal_id: terminal_id_string.clone(),
            takeover: false,
        })
    );
    assert_eq!(server.foreground_client_id, Some(1));
    assert!(server
        .app
        .state
        .direct_attach_resize_locks
        .contains(&terminal_id));
    assert_eq!(
        server
            .app
            .terminal_runtimes
            .get(&terminal_id)
            .expect("runtime")
            .current_size(),
        (24, 80)
    );

    assert!(server.handle_server_event(ServerEvent::ClientDisconnected { client_id: 2 }));

    assert!(!server
        .app
        .state
        .direct_attach_resize_locks
        .contains(&terminal_id));
    assert_eq!(
        server
            .app
            .terminal_runtimes
            .get(&terminal_id)
            .expect("runtime")
            .current_size(),
        expected_app_size
    );
    drop(server);
    drop(_runtime_guard);
    rt.shutdown_timeout(Duration::from_millis(100));
}

#[test]
fn render_and_stream_sends_terminal_frame_for_terminal_ansi_client() {
    let mut server = test_headless_server();
    let (client_tx, _client_control_rx, client_rx) = test_client_writer();

    server.clients.insert(
        1,
        ClientConnection::app(
            (80, 24),
            crate::kitty_graphics::HostCellSize::default(),
            crate::terminal_theme::TerminalTheme::default(),
            None,
            1,
            RenderEncoding::TerminalAnsi,
            Some(client_tx),
        ),
    );
    server.foreground_client_id = Some(1);

    server.render_and_stream();

    match read_server_message(
        client_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("terminal frame"),
    ) {
        ServerMessage::Terminal(frame) => {
            assert_eq!(frame.seq, 1);
            assert_eq!((frame.width, frame.height), (80, 24));
            assert!(frame.full);
            assert!(!frame.bytes.is_empty());
        }
        other => panic!("expected terminal frame, got {other:?}"),
    }
    assert_eq!(
        server
            .clients
            .get(&1)
            .unwrap()
            .render_state
            .terminal_seq()
            .unwrap(),
        1
    );
}

#[test]
fn render_and_stream_sends_large_terminal_frame_for_terminal_ansi_client() {
    let mut server = test_headless_server();
    server.app.state.workspaces = vec![crate::workspace::Workspace::test_new("test")];
    server.app.state.ensure_test_terminals();
    server.app.state.active = Some(0);
    server.app.state.selected = 0;
    server.app.state.mode = crate::app::Mode::Terminal;
    let (client_tx, _client_control_rx, client_rx) = test_client_writer();

    server.clients.insert(
        1,
        ClientConnection::app(
            (278, 85),
            crate::kitty_graphics::HostCellSize::default(),
            crate::terminal_theme::TerminalTheme::default(),
            None,
            1,
            RenderEncoding::TerminalAnsi,
            Some(client_tx),
        ),
    );
    server.foreground_client_id = Some(1);
    server.sync_foreground_client_state();

    server.render_and_stream();
    match read_server_message(
        client_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("initial terminal frame"),
    ) {
        ServerMessage::Terminal(frame) => {
            assert_eq!(frame.seq, 1);
            assert_eq!((frame.width, frame.height), (278, 85));
            assert!(frame.full);
        }
        other => panic!("expected terminal frame, got {other:?}"),
    }

    assert!(server.handle_server_event(ServerEvent::ClientResize {
        client_id: 1,
        cols: 710,
        rows: 202,
        cell_width_px: 0,
        cell_height_px: 0,
        pixel_mouse: false,
    }));
    server.render_and_stream();

    match read_server_message(
        client_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("large terminal frame"),
    ) {
        ServerMessage::Terminal(frame) => {
            assert_eq!(frame.seq, 2);
            assert_eq!((frame.width, frame.height), (710, 202));
            assert!(frame.full);
            assert!(!frame.bytes.is_empty());
        }
        other => panic!("expected terminal frame, got {other:?}"),
    }

    server.app.state.mode = crate::app::Mode::Navigate;
    server.render_and_stream();
    match read_server_message(
        client_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("follow-up terminal frame"),
    ) {
        ServerMessage::Terminal(frame) => assert_eq!(frame.seq, 3),
        other => panic!("expected terminal frame, got {other:?}"),
    }
}

#[test]
fn terminal_ansi_input_does_not_reset_blit_baseline() {
    let mut server = test_headless_server();
    let (client_tx, _client_control_rx, client_rx) = test_client_writer();

    server.clients.insert(
        1,
        ClientConnection::app(
            (80, 24),
            crate::kitty_graphics::HostCellSize::default(),
            crate::terminal_theme::TerminalTheme::default(),
            None,
            1,
            RenderEncoding::TerminalAnsi,
            Some(client_tx),
        ),
    );
    server.foreground_client_id = Some(1);

    server.render_and_stream();
    let _ = client_rx
        .recv_timeout(Duration::from_millis(100))
        .expect("initial terminal frame");
    assert_eq!(
        server
            .clients
            .get(&1)
            .unwrap()
            .render_state
            .terminal_seq()
            .unwrap(),
        1
    );

    assert!(!server.handle_server_event(ServerEvent::ClientInput {
        client_id: 1,
        data: Vec::new(),
    }));
    server.render_and_stream();

    assert_eq!(
        server
            .clients
            .get(&1)
            .unwrap()
            .render_state
            .terminal_seq()
            .unwrap(),
        1
    );
    assert!(client_rx.recv_timeout(Duration::from_millis(50)).is_err());
}

#[test]
fn outer_focus_gained_repaints_terminal_ansi_without_clearing() {
    let mut server = test_headless_server();
    let (client_tx, _client_control_rx, client_rx) = test_client_writer();

    server.clients.insert(
        1,
        ClientConnection::app(
            (80, 24),
            crate::kitty_graphics::HostCellSize::default(),
            crate::terminal_theme::TerminalTheme::default(),
            None,
            1,
            RenderEncoding::TerminalAnsi,
            Some(client_tx),
        ),
    );
    server.foreground_client_id = Some(1);

    server.render_and_stream();
    let _ = client_rx
        .recv_timeout(Duration::from_millis(100))
        .expect("initial terminal frame");

    assert!(server.handle_server_event(ServerEvent::ClientInput {
        client_id: 1,
        data: b"\x1b[I".to_vec(),
    }));
    server.render_and_stream();

    match read_server_message(client_rx.recv_timeout(Duration::from_millis(100)).unwrap()) {
        ServerMessage::Terminal(frame) => {
            assert_eq!(frame.seq, 2);
            assert!(frame.full);
            assert!(!frame.bytes.windows(4).any(|bytes| bytes == b"\x1b[2J"));
        }
        other => panic!("expected terminal frame, got {other:?}"),
    }
}

#[tokio::test]
async fn outer_focus_gained_client_render_pending_survives_semantic_render_queue_full() {
    let (mut server, client_rx, pane_id) = retained_test_server(b"aaaa");

    server.render_and_stream();
    let _ = client_rx
        .recv_timeout(Duration::from_millis(100))
        .expect("initial semantic frame");

    let queued = HeadlessServer::frame_server_message(&ServerMessage::ReloadSoundConfig)
        .expect("serialize dummy message");
    server.clients[&1]
        .writer
        .as_ref()
        .unwrap()
        .test_fill_render(queued);

    assert!(server.handle_server_event(ServerEvent::ClientInput {
        client_id: 1,
        data: b"\x1b[I".to_vec(),
    }));
    assert_eq!(
        server.clients.get(&1).unwrap().deferred_render(),
        DeferredRender::Full
    );

    server.render_and_stream();

    assert_eq!(
        server.clients.get(&1).unwrap().deferred_render(),
        DeferredRender::Full
    );
    assert!(matches!(
        read_server_message(client_rx.recv_timeout(Duration::from_millis(100)).unwrap()),
        ServerMessage::ReloadSoundConfig
    ));

    let runtime = server
        .app
        .state
        .runtime_for_pane_in_workspace(&server.app.terminal_runtimes, 0, pane_id)
        .expect("runtime");
    runtime.test_process_pty_bytes(b"\rZ");

    assert!(!server.render_retained_pty_update_and_stream());
    assert!(client_rx.recv_timeout(Duration::from_millis(50)).is_err());

    assert!(server.handle_server_event(ServerEvent::ClientWriterDrained { client_id: 1 }));
    server.render_and_stream();

    assert_eq!(
        server.clients.get(&1).unwrap().deferred_render(),
        DeferredRender::None
    );
    assert!(matches!(
        read_server_message(client_rx.recv_timeout(Duration::from_millis(100)).unwrap()),
        ServerMessage::Frame(_)
    ));
}

#[test]
fn outer_focus_gained_does_not_force_terminal_ansi_full_redraw_when_disabled() {
    let mut server = test_headless_server();
    server.app.state.redraw_on_focus_gained = false;
    let (client_tx, _client_control_rx, client_rx) = test_client_writer();

    server.clients.insert(
        1,
        ClientConnection::app(
            (80, 24),
            crate::kitty_graphics::HostCellSize::default(),
            crate::terminal_theme::TerminalTheme::default(),
            None,
            1,
            RenderEncoding::TerminalAnsi,
            Some(client_tx),
        ),
    );
    server.foreground_client_id = Some(1);

    server.render_and_stream();
    let _ = client_rx
        .recv_timeout(Duration::from_millis(100))
        .expect("initial terminal frame");

    server.handle_server_event(ServerEvent::ClientInput {
        client_id: 1,
        data: b"\x1b[I".to_vec(),
    });
    server.render_and_stream();

    assert!(client_rx.recv_timeout(Duration::from_millis(50)).is_err());
    assert_eq!(server.clients[&1].outer_terminal_focus, Some(true));
    assert_eq!(server.app.state.outer_terminal_focus, Some(true));
    assert_eq!(
        server
            .clients
            .get(&1)
            .unwrap()
            .render_state
            .terminal_seq()
            .unwrap(),
        1
    );
}

#[test]
fn outer_focus_gained_does_not_mark_semantic_render_pending_when_disabled() {
    let mut server = test_headless_server();
    server.app.state.redraw_on_focus_gained = false;
    let (client_tx, _client_control_rx, _client_rx) = test_client_writer();

    server.clients.insert(
        1,
        ClientConnection::app(
            (80, 24),
            crate::kitty_graphics::HostCellSize::default(),
            crate::terminal_theme::TerminalTheme::default(),
            None,
            1,
            RenderEncoding::SemanticFrame,
            Some(client_tx),
        ),
    );
    server.foreground_client_id = Some(1);

    assert!(server.handle_server_event(ServerEvent::ClientInput {
        client_id: 1,
        data: b"\x1b[I".to_vec(),
    }));

    assert_eq!(
        server.clients.get(&1).unwrap().deferred_render(),
        DeferredRender::None
    );
    assert!(!server.app.full_redraw_pending);
    assert_eq!(server.clients[&1].outer_terminal_focus, Some(true));
    assert_eq!(server.app.state.outer_terminal_focus, Some(true));
}

#[test]
fn full_render_queue_does_not_advance_terminal_ansi_baseline() {
    let mut server = test_headless_server();
    let (client_tx, _client_control_rx, client_rx) = test_client_writer();
    let queued = HeadlessServer::frame_server_message(&ServerMessage::ReloadSoundConfig)
        .expect("serialize dummy message");
    client_tx.test_fill_render(queued);

    server.clients.insert(
        1,
        ClientConnection::app(
            (80, 24),
            crate::kitty_graphics::HostCellSize::default(),
            crate::terminal_theme::TerminalTheme::default(),
            None,
            1,
            RenderEncoding::TerminalAnsi,
            Some(client_tx),
        ),
    );
    server.foreground_client_id = Some(1);

    server.render_and_stream();

    assert_eq!(
        server
            .clients
            .get(&1)
            .unwrap()
            .render_state
            .terminal_seq()
            .unwrap(),
        0
    );
    assert!(matches!(
        read_server_message(client_rx.recv_timeout(Duration::from_millis(100)).unwrap()),
        ServerMessage::ReloadSoundConfig
    ));
    assert!(client_rx.recv_timeout(Duration::from_millis(50)).is_err());
}

#[test]
fn writer_drained_retries_pending_terminal_ansi_render() {
    let mut server = test_headless_server();
    let (client_tx, _client_control_rx, client_rx) = test_client_writer();
    let queued = HeadlessServer::frame_server_message(&ServerMessage::ReloadSoundConfig)
        .expect("serialize dummy message");
    client_tx.test_fill_render(queued);

    server.clients.insert(
        1,
        ClientConnection::app(
            (80, 24),
            crate::kitty_graphics::HostCellSize::default(),
            crate::terminal_theme::TerminalTheme::default(),
            None,
            1,
            RenderEncoding::TerminalAnsi,
            Some(client_tx),
        ),
    );
    server.foreground_client_id = Some(1);

    server.render_and_stream();
    assert_eq!(
        server.clients.get(&1).unwrap().deferred_render(),
        DeferredRender::Full
    );
    assert!(matches!(
        read_server_message(client_rx.recv_timeout(Duration::from_millis(100)).unwrap()),
        ServerMessage::ReloadSoundConfig
    ));

    assert!(server.handle_server_event(ServerEvent::ClientWriterDrained { client_id: 1 }));
    server.render_and_stream();

    match read_server_message(client_rx.recv_timeout(Duration::from_millis(100)).unwrap()) {
        ServerMessage::Terminal(frame) => assert_eq!(frame.seq, 1),
        other => panic!("expected terminal frame, got {other:?}"),
    }
    assert_eq!(
        server
            .clients
            .get(&1)
            .unwrap()
            .render_state
            .terminal_seq()
            .unwrap(),
        1
    );
    assert_eq!(
        server.clients.get(&1).unwrap().deferred_render(),
        DeferredRender::None
    );
}

#[test]
fn render_and_stream_skips_identical_frame_sends() {
    let mut server = test_headless_server();
    server.app.state.workspaces = vec![crate::workspace::Workspace::test_new("test")];
    server.app.state.active = Some(0);
    server.app.state.selected = 0;
    server.app.state.mode = crate::app::Mode::Terminal;

    let (client_tx, _client_control_rx, client_rx) = test_client_writer();

    server.clients.insert(
        1,
        ClientConnection::app(
            (80, 24),
            crate::kitty_graphics::HostCellSize::default(),
            crate::terminal_theme::TerminalTheme::default(),
            None,
            1,
            RenderEncoding::SemanticFrame,
            Some(client_tx),
        ),
    );
    server.foreground_client_id = Some(1);
    server.sync_foreground_client_state();
    server.resize_shared_runtime_to_effective_size();

    server.render_and_stream();
    let first = client_rx.recv_timeout(Duration::from_millis(100));
    assert!(first.is_ok(), "expected first frame to be sent");

    server.render_and_stream();
    assert!(
        client_rx.recv_timeout(Duration::from_millis(50)).is_err(),
        "identical frame should not be sent twice"
    );
}

#[test]
fn identical_miller_frame_sends_no_payload() {
    let mut server = test_headless_server();
    open_virtual_miller_files(&mut server);
    let (client_tx, _client_control_rx, client_rx) = test_client_writer();
    server.clients.insert(
        1,
        ClientConnection::app(
            (120, 40),
            crate::kitty_graphics::HostCellSize::default(),
            crate::terminal_theme::TerminalTheme::default(),
            None,
            1,
            RenderEncoding::SemanticFrame,
            Some(client_tx),
        ),
    );
    server.foreground_client_id = Some(1);
    server.sync_foreground_client_state();
    server.resize_shared_runtime_to_effective_size();

    server.render_and_stream();
    let first = read_server_frame(
        client_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("first Miller frame"),
    );
    assert!(
        frame_text(&first).contains("alpha.txt"),
        "first transport frame must contain the active Miller surface"
    );

    server.render_and_stream();
    assert!(
        client_rx.recv_timeout(Duration::from_millis(50)).is_err(),
        "a logically identical Miller frame must produce zero payload"
    );
    assert!(!server.clients.get(&1).expect("client").render_pending);
}

#[test]
fn miller_render_queue_remains_single_slot() {
    let mut server = test_headless_server();
    open_virtual_miller_files(&mut server);
    let (client_tx, _client_control_rx, client_rx) = test_client_writer();
    server.clients.insert(
        1,
        ClientConnection::app(
            (120, 40),
            crate::kitty_graphics::HostCellSize::default(),
            crate::terminal_theme::TerminalTheme::default(),
            None,
            1,
            RenderEncoding::SemanticFrame,
            Some(client_tx),
        ),
    );
    server.foreground_client_id = Some(1);
    server.sync_foreground_client_state();
    server.resize_shared_runtime_to_effective_size();

    server.render_and_stream();
    server
        .app
        .state
        .file_manager
        .as_mut()
        .expect("open Files")
        .move_down();
    server.render_and_stream();

    assert!(
        server.clients.get(&1).expect("client").render_pending,
        "backpressure must retain one retry intent instead of enqueueing another frame"
    );
    let first = read_server_frame(
        client_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("single queued frame"),
    );
    assert!(frame_text(&first).contains("alpha.txt"));
    assert!(
        client_rx.recv_timeout(Duration::from_millis(20)).is_err(),
        "the render channel must contain at most one pending payload"
    );

    assert!(server.handle_server_event(ServerEvent::ClientWriterDrained { client_id: 1 }));
    server.render_and_stream();
    let latest = read_server_frame(
        client_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("retry frame after drain"),
    );
    assert_ne!(
        first, latest,
        "the retry must carry the latest Miller state"
    );
    assert!(!server.clients.get(&1).expect("client").render_pending);
    assert!(
        client_rx.recv_timeout(Duration::from_millis(20)).is_err(),
        "drain retry must still leave no second queued payload"
    );
}

#[tokio::test]
async fn one_miller_row_change_does_not_force_unrelated_runtime_mutation() {
    let (mut server, client_rx, pane_id) = retained_test_server(b"terminal sentinel");
    open_virtual_miller_files(&mut server);
    server.render_and_stream();
    let _ = client_rx
        .recv_timeout(Duration::from_millis(100))
        .expect("initial Files frame");
    let runtime = server
        .app
        .state
        .runtime_for_pane_in_workspace(&server.app.terminal_runtimes, 0, pane_id)
        .expect("test-owned terminal runtime");
    let before_size = runtime.current_size();
    let before_history = runtime.snapshot_history();

    server
        .app
        .state
        .file_manager
        .as_mut()
        .expect("open Files")
        .move_down();
    server.render_and_stream();
    let changed = read_server_frame(
        client_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("changed Miller frame"),
    );
    assert!(frame_text(&changed).contains("bravo.txt"));

    let runtime = server
        .app
        .state
        .runtime_for_pane_in_workspace(&server.app.terminal_runtimes, 0, pane_id)
        .expect("same test-owned terminal runtime");
    assert_eq!(runtime.current_size(), before_size);
    assert_eq!(runtime.snapshot_history(), before_history);
    shutdown_test_runtimes(&mut server);
}

#[test]
fn visible_source_wakes_pending_hidden_work() {
    let (server, background_pane) = hidden_pty_visibility_test_server(&[(120, 40)]);
    let visible_pane = server.app.state.workspaces[0].tabs[0].root_pane;
    server.sync_immediate_pty_sources();

    assert!(server.app.render_dirty.request_pty(background_pane));
    assert!(!server.has_pending_presentation_work(false, false));
    assert!(server.app.render_dirty.request_pty(visible_pane));
    assert!(server.has_pending_presentation_work(false, false));
}

#[test]
fn inactive_tab_pty_source_is_hidden_until_tab_focus() {
    let (server, background_pane) = hidden_pty_visibility_test_server(&[]);
    let sources = HashSet::from([background_pane]);
    assert!(!server.pty_sources_visible_to_any_render_target(&sources));

    let (mut server, background_pane) = hidden_pty_visibility_test_server(&[(120, 40), (44, 20)]);
    let sources = HashSet::from([background_pane]);
    assert!(!server.pty_sources_visible_to_any_render_target(&sources));

    server.app.state.workspaces[0].switch_tab(1);
    assert!(server.pty_sources_visible_to_any_render_target(&sources));
}

#[tokio::test]
async fn hidden_pty_output_appears_after_switching_to_its_tab() {
    let mut server = test_headless_server();
    let mut workspace = crate::workspace::Workspace::test_new("test");
    let background_tab = workspace.test_add_tab(Some("background"));
    let background_pane = workspace.tabs[background_tab].root_pane;
    workspace.insert_test_runtime(
        background_pane,
        crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 24, b"before"),
    );
    server.app.state.workspaces = vec![workspace];
    server.app.state.active = Some(0);
    server.app.state.selected = 0;
    server.app.state.mode = crate::app::Mode::Terminal;

    let (client_tx, _client_control_rx, client_rx) = test_client_writer();
    server.clients.insert(
        1,
        ClientConnection::app(
            (80, 24),
            crate::kitty_graphics::HostCellSize::default(),
            crate::terminal_theme::TerminalTheme::default(),
            None,
            1,
            RenderEncoding::SemanticFrame,
            Some(client_tx),
        ),
    );
    server.foreground_client_id = Some(1);
    server.sync_foreground_client_state();
    server.resize_shared_runtime_to_effective_size();
    server.render_and_stream();
    let _initial_frame = client_rx
        .recv_timeout(Duration::from_millis(100))
        .expect("initial frame");

    let runtime = server
        .app
        .state
        .runtime_for_pane_in_workspace(&server.app.terminal_runtimes, 0, background_pane)
        .expect("background runtime");
    runtime.test_process_pty_bytes(b"\rhidden-update");
    assert!(server.app.render_dirty.request_pty(background_pane));
    let request = server.app.render_dirty.take();
    let pty = if server.pty_sources_visible_to_any_render_target(&request.pty_sources) {
        PtyRenderState::Visible
    } else {
        PtyRenderState::Hidden
    };
    assert_eq!(
        retained_render_plan(RetainedRenderInput {
            needs_full_render: false,
            needs_graphics_render: false,
            pty,
        }),
        RetainedRenderPlan::HiddenPty
    );
    assert!(client_rx.recv_timeout(Duration::from_millis(50)).is_err());

    // Rebased for the fork's per-display tabs (TP-MCF-TAB-01): a bare
    // switch_tab outside a viewer window only moves the default tab,
    // while client 1 stays on the tab its ledger recorded at the first
    // render. Standing in the client's viewer is what the real
    // tab-switch path does, and it moves the ledger with it.
    let viewer = server.app.state.enter_viewer(Some(1));
    server.app.state.workspaces[0].switch_tab(background_tab);
    server.app.state.restore_viewer(viewer);
    server.render_and_stream();
    let visible_frame = read_server_frame(
        client_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("frame after tab switch"),
    );
    assert!(frame_text(&visible_frame).contains("hidden-update"));
}

#[test]
fn direct_terminal_observer_keeps_hidden_pty_source_renderable_with_app_client() {
    let (mut server, background_pane) = hidden_pty_visibility_test_server(&[(120, 40)]);
    assert!(!server.pty_sources_visible_to_any_render_target(&HashSet::from([background_pane])));

    let terminal_id = server.app.state.workspaces[0]
        .terminal_id(background_pane)
        .expect("background terminal id")
        .to_string();
    let (client_tx, _client_control_rx, _client_rx) = test_client_writer();
    server.clients.insert(
        2,
        ClientConnection::new_with_mode(
            ClientConnectionMode::TerminalObserve { terminal_id },
            (80, 24),
            crate::kitty_graphics::HostCellSize::default(),
            2,
            RenderEncoding::SemanticFrame,
            Some(client_tx),
        ),
    );

    assert!(server.pty_sources_visible_to_any_render_target(&HashSet::from([background_pane])));

    let hidden_pane = server.app.state.workspaces[0].tabs[0].root_pane;
    server.sync_immediate_pty_sources();
    assert!(server.app.render_dirty.request_pty(background_pane));
    assert!(server.has_pending_presentation_work(false, false));
    assert!(server.app.render_dirty.request_pty(hidden_pane));
}

#[tokio::test]
async fn retained_pty_update_streams_dirty_row_from_last_frame() {
    let (mut server, client_rx, pane_id) = retained_test_server(b"aaaa");
    server.render_and_stream();
    let first = read_server_frame(
        client_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("initial frame"),
    );
    assert!(first.cells.iter().any(|cell| cell.symbol == "a"));

    let runtime = server
        .app
        .state
        .runtime_for_pane_in_workspace(&server.app.terminal_runtimes, 0, pane_id)
        .expect("runtime");
    runtime.test_process_pty_bytes(b"\rZ");

    assert!(server.render_retained_pty_update_and_stream());
    let patched = read_server_frame(
        client_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("retained frame"),
    );
    assert!(patched.cells.iter().any(|cell| cell.symbol == "Z"));
    assert_eq!((patched.width, patched.height), (80, 24));
}

#[tokio::test]
async fn retained_pty_update_declines_while_popup_is_visible() {
    let (mut server, client_rx, _) = retained_test_server(b"tiled");
    let popup_runtime =
        crate::terminal::TerminalRuntime::test_with_screen_bytes(40, 12, b"popup-aaaa");
    let (_, terminal_id) = server.app.install_test_popup_runtime(popup_runtime);

    server.render_and_stream();
    let initial = read_server_frame(
        client_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("initial popup frame"),
    );
    assert!(frame_text(&initial).contains("popup-aaaa"));
    server
        .app
        .terminal_runtimes
        .get(&terminal_id)
        .unwrap()
        .test_process_pty_bytes(b"\rZ");

    assert!(!server.render_retained_pty_update_and_stream());
    server.render_and_stream();
    let updated = read_server_frame(
        client_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("full popup fallback frame"),
    );
    assert!(frame_text(&updated).contains("Zopup-aaaa"));
}

#[tokio::test]
async fn popup_forces_host_mouse_capture_for_headless_client() {
    let mut server = test_headless_server();
    let (client_tx, client_control_rx, _client_rx) = test_client_writer();
    server.clients.insert(
        1,
        ClientConnection::app(
            (80, 24),
            crate::kitty_graphics::HostCellSize::default(),
            crate::terminal_theme::TerminalTheme::default(),
            None,
            1,
            RenderEncoding::SemanticFrame,
            Some(client_tx),
        ),
    );
    server.app.state.mouse_capture = false;
    let popup_runtime = crate::terminal::TerminalRuntime::test_with_screen_bytes(40, 12, b"popup");
    server.app.install_test_popup_runtime(popup_runtime);

    server.stream_host_mouse_capture_mode();

    assert!(matches!(
        read_server_message(
            client_control_rx
                .recv_timeout(Duration::from_millis(100))
                .expect("mouse capture message")
        ),
        ServerMessage::MouseCapture { enabled: true, .. }
    ));
}

#[tokio::test]
async fn command_mode_updates_headless_client_keyboard_flags() {
    let mut server = test_headless_server();
    let (client_tx, client_control_rx, _client_rx) = test_client_writer();
    server.clients.insert(
        1,
        ClientConnection::app(
            (80, 24),
            crate::kitty_graphics::HostCellSize::default(),
            crate::terminal_theme::TerminalTheme::default(),
            None,
            1,
            RenderEncoding::SemanticFrame,
            Some(client_tx),
        ),
    );

    server.app.state.mode = crate::app::Mode::Prefix;
    server.stream_host_keyboard_enhancement_flags();
    assert!(matches!(
        read_server_message(
            client_control_rx
                .recv_timeout(Duration::from_millis(100))
                .expect("command-mode keyboard enhancement message")
        ),
        ServerMessage::KittyKeyboardReportAll { enabled: true }
    ));

    server.app.state.mode = crate::app::Mode::Terminal;
    server.stream_host_keyboard_enhancement_flags();
    assert!(matches!(
        read_server_message(
            client_control_rx
                .recv_timeout(Duration::from_millis(100))
                .expect("IME-compatible keyboard enhancement message")
        ),
        ServerMessage::KittyKeyboardReportAll { enabled: false }
    ));
}

#[tokio::test]
async fn focused_report_all_pane_updates_headless_client_keyboard_flags() {
    let mut server = test_headless_server();
    let (client_tx, client_control_rx, _client_rx) = test_client_writer();
    server.clients.insert(
        1,
        ClientConnection::app(
            (80, 24),
            crate::kitty_graphics::HostCellSize::default(),
            crate::terminal_theme::TerminalTheme::default(),
            None,
            1,
            RenderEncoding::SemanticFrame,
            Some(client_tx),
        ),
    );
    let popup_runtime =
        crate::terminal::TerminalRuntime::test_with_screen_bytes(40, 12, b"\x1b[>15u");
    server.app.install_test_popup_runtime(popup_runtime);

    server.stream_host_keyboard_enhancement_flags();

    assert!(matches!(
        read_server_message(
            client_control_rx
                .recv_timeout(Duration::from_millis(100))
                .expect("keyboard enhancement message")
        ),
        ServerMessage::KittyKeyboardReportAll { enabled: true }
    ));

    assert!(server.app.close_popup_pane());
    server.app.state.mode = crate::app::Mode::Terminal;
    server.stream_host_keyboard_enhancement_flags();
    assert!(matches!(
        read_server_message(
            client_control_rx
                .recv_timeout(Duration::from_millis(100))
                .expect("IME-compatible keyboard enhancement message")
        ),
        ServerMessage::KittyKeyboardReportAll { enabled: false }
    ));
}

#[tokio::test]
async fn virtual_render_uses_popup_cursor() {
    let (mut server, _client_rx, _) = retained_test_server(b"\x1b[2;2H");
    let popup_runtime =
        crate::terminal::TerminalRuntime::test_with_screen_bytes(40, 12, b"\x1b[4;5H");
    let (_, terminal_id) = server.app.install_test_popup_runtime(popup_runtime);

    let (_, cursor) = crate::server::render_stream::render_virtual_with_runtime_registry(
        &mut server.app.state,
        &server.app.terminal_runtimes,
        ratatui::layout::Rect::new(0, 0, 80, 24),
        true,
        crate::kitty_graphics::HostCellSize::default(),
    );
    let (_, inner) =
        crate::ui::popup_pane_rects(&server.app.state, server.app.state.view.terminal_area)
            .unwrap();
    let expected = server
        .app
        .terminal_runtimes
        .get(&terminal_id)
        .unwrap()
        .cursor_state(inner, true)
        .unwrap();

    assert_eq!(
        cursor,
        Some(crate::protocol::CursorState {
            x: expected.x,
            y: expected.y,
            visible: expected.visible,
            shape: expected.shape,
        })
    );
}

#[tokio::test]
async fn virtual_render_does_not_resize_directly_attached_popup() {
    let (mut server, _client_rx, _) = retained_test_server(b"tiled");
    let popup_runtime = crate::terminal::TerminalRuntime::test_with_screen_bytes(50, 13, b"");
    let (_, terminal_id) = server.app.install_test_popup_runtime(popup_runtime);
    server
        .app
        .state
        .direct_attach_resize_locks
        .insert(terminal_id.clone());

    let _ = crate::server::render_stream::render_virtual_with_runtime_registry(
        &mut server.app.state,
        &server.app.terminal_runtimes,
        ratatui::layout::Rect::new(0, 0, 80, 24),
        true,
        crate::kitty_graphics::HostCellSize::default(),
    );

    assert_eq!(
        server
            .app
            .terminal_runtimes
            .get(&terminal_id)
            .unwrap()
            .current_size(),
        (13, 50)
    );
}

#[tokio::test]
async fn retained_pty_update_declines_while_toast_is_visible() {
    let (mut server, client_rx, pane_id) = retained_test_server(b"aaaa");
    server.app.state.toast = Some(crate::app::state::ToastNotification {
        kind: crate::app::state::ToastKind::NeedsAttention,
        title: "pi needs attention".to_owned(),
        context: "background · 2".to_owned(),
        position: None,
        target: None,
    });
    server.render_and_stream();
    let initial = read_server_frame(
        client_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("initial frame"),
    );
    assert!(
        frame_text(&initial).contains("pi needs attention"),
        "expected initial full frame to include toast text"
    );

    let toast_row = server.app.state.view.toast_hit_area.y;
    let inner_rect = server.app.state.view.pane_infos[0].inner_rect;
    let pane_row = toast_row
        .checked_sub(inner_rect.y)
        .expect("toast should overlap the pane")
        + 1;
    assert!(pane_row <= inner_rect.height);
    let runtime = server
        .app
        .state
        .runtime_for_pane_in_workspace(&server.app.terminal_runtimes, 0, pane_id)
        .expect("runtime");
    runtime.test_process_pty_bytes(format!("\x1b[{pane_row};1Hzzzz").as_bytes());

    assert!(!server.render_retained_pty_update_and_stream());
    assert!(
        client_rx.recv_timeout(Duration::from_millis(50)).is_err(),
        "retained path should not stream a frame that can overwrite toast cells"
    );
}

#[tokio::test]
async fn retained_pty_update_declines_while_copy_feedback_is_visible() {
    let (mut server, client_rx, pane_id) = retained_test_server(b"aaaa");
    server.app.state.copy_feedback = Some(crate::app::state::CopyFeedback {
        message: "copied to clipboard".to_owned(),
    });
    server.render_and_stream();
    let initial = read_server_frame(
        client_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("initial frame"),
    );
    let initial_text = frame_text(&initial);
    assert!(
        initial_text.contains("copied to clipboard"),
        "expected initial full frame to include copy feedback"
    );

    let feedback_row = initial_text
        .lines()
        .position(|line| line.contains("copied to clipboard"))
        .expect("copy feedback row") as u16;
    let inner_rect = server.app.state.view.pane_infos[0].inner_rect;
    let pane_row = feedback_row
        .checked_sub(inner_rect.y)
        .expect("copy feedback should overlap the pane")
        + 1;
    assert!(pane_row <= inner_rect.height);
    let runtime = server
        .app
        .state
        .runtime_for_pane_in_workspace(&server.app.terminal_runtimes, 0, pane_id)
        .expect("runtime");
    runtime.test_process_pty_bytes(format!("\x1b[{pane_row};1Hzzzz").as_bytes());

    assert!(!server.render_retained_pty_update_and_stream());
    assert!(
        client_rx.recv_timeout(Duration::from_millis(50)).is_err(),
        "retained path should not stream a frame that can overwrite copy feedback cells"
    );
}

#[tokio::test]
async fn retained_pty_update_matches_full_render_frame() {
    let initial = b"\x1b[6 qleft \xe4\xb8\xad";
    let update = b"\r\x1b[44mZ\x1b[0m";
    let (mut retained_server, retained_rx, retained_pane_id) = retained_test_server(initial);
    let (mut full_server, full_rx, full_pane_id) = retained_test_server(initial);

    retained_server.render_and_stream();
    let _ = retained_rx
        .recv_timeout(Duration::from_millis(100))
        .expect("initial retained baseline");
    full_server.render_and_stream();
    let _ = full_rx
        .recv_timeout(Duration::from_millis(100))
        .expect("initial full baseline");

    retained_server
        .app
        .state
        .runtime_for_pane_in_workspace(&retained_server.app.terminal_runtimes, 0, retained_pane_id)
        .expect("retained runtime")
        .test_process_pty_bytes(update);
    full_server
        .app
        .state
        .runtime_for_pane_in_workspace(&full_server.app.terminal_runtimes, 0, full_pane_id)
        .expect("full runtime")
        .test_process_pty_bytes(update);

    assert!(retained_server.render_retained_pty_update_and_stream());
    full_server.render_and_stream();

    let retained_frame = read_server_frame(
        retained_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("retained frame"),
    );
    let full_frame = read_server_frame(
        full_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("full frame"),
    );
    assert_frame_data_eq(&retained_frame, &full_frame);
}

#[tokio::test]
async fn retained_pty_update_streams_cursor_only_change() {
    let initial = b"abcd";
    let update = b"\x1b[D";
    let (mut retained_server, retained_rx, retained_pane_id) = retained_test_server(initial);
    let (mut full_server, full_rx, full_pane_id) = retained_test_server(initial);

    retained_server.render_and_stream();
    let _ = retained_rx
        .recv_timeout(Duration::from_millis(100))
        .expect("initial retained baseline");
    full_server.render_and_stream();
    let _ = full_rx
        .recv_timeout(Duration::from_millis(100))
        .expect("initial full baseline");

    retained_server
        .app
        .state
        .runtime_for_pane_in_workspace(&retained_server.app.terminal_runtimes, 0, retained_pane_id)
        .expect("retained runtime")
        .test_process_pty_bytes(update);
    full_server
        .app
        .state
        .runtime_for_pane_in_workspace(&full_server.app.terminal_runtimes, 0, full_pane_id)
        .expect("full runtime")
        .test_process_pty_bytes(update);

    assert!(retained_server.render_retained_pty_update_and_stream());
    full_server.render_and_stream();

    let retained_frame = read_server_frame(
        retained_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("retained cursor frame"),
    );
    let full_frame = read_server_frame(
        full_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("full cursor frame"),
    );
    assert_frame_data_eq(&retained_frame, &full_frame);
}

#[tokio::test]
async fn retained_pty_update_declines_unsafe_mode_without_consuming_dirty_rows() {
    let (mut server, client_rx, pane_id) = retained_test_server(b"aaaa");
    server.render_and_stream();
    let _ = client_rx
        .recv_timeout(Duration::from_millis(100))
        .expect("initial frame");

    let runtime = server
        .app
        .state
        .runtime_for_pane_in_workspace(&server.app.terminal_runtimes, 0, pane_id)
        .expect("runtime");
    runtime.test_process_pty_bytes(b"\rZ");

    server.app.state.mode = crate::app::Mode::Navigate;
    assert!(!server.render_retained_pty_update_and_stream());
    assert!(client_rx.recv_timeout(Duration::from_millis(50)).is_err());

    server.app.state.mode = crate::app::Mode::Terminal;
    assert!(server.render_retained_pty_update_and_stream());
    let patched = read_server_frame(
        client_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("retained frame after safe mode"),
    );
    assert!(patched.cells.iter().any(|cell| cell.symbol == "Z"));
}

#[tokio::test]
async fn headless_full_render_clears_full_redraw_pending_for_future_retained_updates() {
    let (mut server, client_rx, pane_id) = retained_test_server(b"aaaa");
    server.app.full_redraw_pending = true;

    server.render_and_stream();
    let _ = client_rx
        .recv_timeout(Duration::from_millis(100))
        .expect("full redraw frame");
    assert!(!server.app.full_redraw_pending);

    let runtime = server
        .app
        .state
        .runtime_for_pane_in_workspace(&server.app.terminal_runtimes, 0, pane_id)
        .expect("runtime");
    runtime.test_process_pty_bytes(b"\rZ");

    assert!(server.render_retained_pty_update_and_stream());
}

#[tokio::test]
async fn retained_pty_update_declines_when_patch_would_stale_hyperlinks() {
    let (mut server, client_rx, pane_id) = retained_test_server(b"link");
    server.render_and_stream();
    let _ = client_rx
        .recv_timeout(Duration::from_millis(100))
        .expect("initial frame");
    let inner_rect = server.app.state.view.pane_infos[0].inner_rect;
    let client = server.clients.get_mut(&1).unwrap();
    let mut frame = client.render_state.last_frame().unwrap().clone();
    frame.hyperlinks = vec!["https://example.com".to_owned()];
    let hyperlink_idx =
        usize::from(inner_rect.y) * usize::from(frame.width) + usize::from(inner_rect.x);
    frame.cells[hyperlink_idx].hyperlink = Some(0);
    let prepared = client
        .render_state
        .prepare_frame(frame)
        .expect("hyperlink frame differs");
    client.render_state.commit_sent_frame(prepared);

    let runtime = server
        .app
        .state
        .runtime_for_pane_in_workspace(&server.app.terminal_runtimes, 0, pane_id)
        .expect("runtime");
    runtime.test_process_pty_bytes(b"\rplain");

    assert!(!server.render_retained_pty_update_and_stream());
    assert!(client_rx.recv_timeout(Duration::from_millis(50)).is_err());

    server.render_and_stream();
    let full = read_server_frame(
        client_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("full frame after hyperlink overwrite"),
    );
    assert!(
        full.cells.iter().all(|cell| cell.hyperlink.is_none()),
        "full render should clear overwritten hyperlink cells"
    );
}

#[tokio::test]
async fn retained_pty_update_allows_dirty_row_that_creates_plain_url() {
    let (mut server, client_rx, pane_id) = retained_test_server(b"plain");
    server.render_and_stream();
    let _ = client_rx
        .recv_timeout(Duration::from_millis(100))
        .expect("initial frame");

    let runtime = server
        .app
        .state
        .runtime_for_pane_in_workspace(&server.app.terminal_runtimes, 0, pane_id)
        .expect("runtime");
    runtime.test_process_pty_bytes(b"\rhttps://example.com/new");

    assert!(server.render_retained_pty_update_and_stream());
    let patched = read_server_frame(
        client_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("retained frame after plain URL"),
    );
    assert!(
        patched.hyperlinks.is_empty(),
        "retained render should not synthesize plain URL hyperlink metadata"
    );
}

#[tokio::test]
async fn retained_pty_update_allows_kitty_enabled_empty_graphics_cache() {
    let (mut server, client_rx, pane_id) = retained_test_server(b"aaaa");
    server.app.state.kitty_graphics_enabled = true;
    server.clients.get_mut(&1).unwrap().cell_size = crate::kitty_graphics::HostCellSize {
        width_px: 10,
        height_px: 20,
    };

    server.render_and_stream();
    let _ = client_rx
        .recv_timeout(Duration::from_millis(100))
        .expect("initial frame");

    let runtime = server
        .app
        .state
        .runtime_for_pane_in_workspace(&server.app.terminal_runtimes, 0, pane_id)
        .expect("runtime");
    runtime.test_process_pty_bytes(b"\rZ");

    assert!(server.render_retained_pty_update_and_stream());
    let retained = read_server_frame(
        client_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("retained frame with kitty enabled"),
    );
    assert!(retained.cells.iter().any(|cell| cell.symbol == "Z"));
}

#[tokio::test]
async fn full_render_queue_full_defers_only_that_client() {
    let (mut server, client_rx, pane_id) = retained_test_server(b"aaaa");
    let queued = HeadlessServer::frame_server_message(&ServerMessage::ReloadSoundConfig)
        .expect("serialize dummy message");
    server.clients[&1]
        .writer
        .as_ref()
        .unwrap()
        .test_fill_render(queued);
    server.app.full_redraw_pending = true;

    server.render_and_stream();

    // Recovery is per connection (U2c-2, upstream #3487): the full writer queue defers this client, and its
    // `ClientWriterDrained` brings it back; the global flag no longer holds every other client on the full path.
    assert!(!server.app.full_redraw_pending);
    assert_eq!(
        server.clients.get(&1).unwrap().deferred_render(),
        DeferredRender::Full
    );
    assert!(matches!(
        read_server_message(client_rx.recv_timeout(Duration::from_millis(100)).unwrap()),
        ServerMessage::ReloadSoundConfig
    ));

    let runtime = server
        .app
        .state
        .runtime_for_pane_in_workspace(&server.app.terminal_runtimes, 0, pane_id)
        .expect("runtime");
    runtime.test_process_pty_bytes(b"\rZ");

    assert!(!server.render_retained_pty_update_and_stream());
    assert!(client_rx.recv_timeout(Duration::from_millis(50)).is_err());
}

#[test]
fn client_config_reload_request_refreshes_attached_clients() {
    let mut server = test_headless_server();
    let (client_tx, client_control_rx, _client_rx) = test_client_writer();

    server.clients.insert(
        1,
        ClientConnection::app(
            (80, 24),
            crate::kitty_graphics::HostCellSize::default(),
            crate::terminal_theme::TerminalTheme::default(),
            None,
            1,
            RenderEncoding::SemanticFrame,
            Some(client_tx),
        ),
    );
    server.app.state.request_client_config_reload = true;

    server.drain_client_config_reload_request();

    match read_server_message(
        client_control_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("client config reload message"),
    ) {
        ServerMessage::ReloadSoundConfig => {}
        other => panic!("expected ReloadSoundConfig, got {other:?}"),
    }
    assert!(!server.app.state.request_client_config_reload);
}

#[test]
fn terminal_bell_targets_foreground_client_only() {
    let mut server = test_headless_server();
    let (background_tx, background_control_rx, _background_rx) = test_client_writer();
    let (foreground_tx, foreground_control_rx, _foreground_rx) = test_client_writer();

    server.clients.insert(
        1,
        ClientConnection::app(
            (120, 40),
            crate::kitty_graphics::HostCellSize::default(),
            crate::terminal_theme::TerminalTheme::default(),
            None,
            1,
            RenderEncoding::SemanticFrame,
            Some(background_tx),
        ),
    );
    server.clients.insert(
        2,
        ClientConnection::app(
            (80, 24),
            crate::kitty_graphics::HostCellSize::default(),
            crate::terminal_theme::TerminalTheme::default(),
            None,
            2,
            RenderEncoding::SemanticFrame,
            Some(foreground_tx),
        ),
    );
    server.foreground_client_id = Some(2);

    let changed = server.handle_internal_event_with_forwarding(AppEvent::TerminalBell {
        pane_id: crate::layout::PaneId::from_raw(1),
        count: 3,
    });

    assert!(!changed);
    match read_server_message(
        foreground_control_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("foreground terminal bell message"),
    ) {
        ServerMessage::TerminalBell { count } => assert_eq!(count, 3),
        other => panic!("expected terminal bell message, got {other:?}"),
    }
    assert!(
        background_control_rx
            .recv_timeout(Duration::from_millis(50))
            .is_err(),
        "background client should not receive terminal bells"
    );

    server.foreground_client_id = None;
    server.handle_internal_event_with_forwarding(AppEvent::TerminalBell {
        pane_id: crate::layout::PaneId::from_raw(1),
        count: 1,
    });
    assert!(
        foreground_control_rx
            .recv_timeout(Duration::from_millis(50))
            .is_err(),
        "bells without a foreground client must not be retained"
    );
}

#[test]
fn clipboard_write_targets_foreground_client_only() {
    let mut server = test_headless_server();
    let (background_tx, background_control_rx, _background_rx) = test_client_writer();
    let (foreground_tx, foreground_control_rx, _foreground_rx) = test_client_writer();

    server.clients.insert(
        1,
        ClientConnection::new(
            (120, 40),
            crate::kitty_graphics::HostCellSize::default(),
            1,
            RenderEncoding::SemanticFrame,
            Some(background_tx),
        ),
    );
    server.clients.insert(
        2,
        ClientConnection::new(
            (80, 24),
            crate::kitty_graphics::HostCellSize::default(),
            2,
            RenderEncoding::SemanticFrame,
            Some(foreground_tx),
        ),
    );
    server.foreground_client_id = Some(2);
    server.sync_foreground_client_state();

    let changed = server.handle_internal_event_with_forwarding(AppEvent::ClipboardWrite {
        content: b"test".to_vec(),
    });

    assert!(!changed);
    match read_server_message(
        foreground_control_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("foreground clipboard message"),
    ) {
        ServerMessage::Clipboard { data } => assert_eq!(data, "dGVzdA=="),
        other => panic!("expected clipboard message, got {other:?}"),
    }
    assert!(
        background_control_rx
            .recv_timeout(Duration::from_millis(50))
            .is_err(),
        "background client should not receive clipboard writes"
    );
}

#[test]
fn clipboard_write_without_foreground_client_does_not_change_visual_state() {
    let mut server = test_headless_server();
    server.foreground_client_id = None;

    let changed = server.handle_internal_event_with_forwarding(AppEvent::ClipboardWrite {
        content: b"test".to_vec(),
    });

    assert!(!changed);
}

#[test]
fn clipboard_write_failed_foreground_send_removes_client_without_visual_change() {
    let mut server = test_headless_server();
    let (foreground_tx, foreground_control_rx, _foreground_rx) = test_client_writer();
    drop(foreground_control_rx);
    foreground_tx.test_close();

    server.clients.insert(
        1,
        ClientConnection::new(
            (80, 24),
            crate::kitty_graphics::HostCellSize::default(),
            1,
            RenderEncoding::SemanticFrame,
            Some(foreground_tx),
        ),
    );
    server.foreground_client_id = Some(1);

    let changed = server.handle_internal_event_with_forwarding(AppEvent::ClipboardWrite {
        content: b"test".to_vec(),
    });

    assert!(!changed);
    assert!(
        !server.clients.contains_key(&1),
        "failed targeted send should remove the broken foreground client"
    );
}

#[test]
fn clipboard_write_without_foreground_client_does_not_show_feedback() {
    let mut server = test_headless_server();
    server.foreground_client_id = None;

    let changed = server.handle_internal_event_with_forwarding(AppEvent::ClipboardWrite {
        content: b"test".to_vec(),
    });

    assert!(!changed);
    assert!(
        server.app.state.copy_feedback.is_none(),
        "clipboard feedback should only show when a foreground client can receive the write"
    );
}

#[test]
fn clipboard_write_failed_foreground_send_does_not_show_feedback() {
    let mut server = test_headless_server();
    let (foreground_tx, foreground_control_rx, _foreground_rx) = test_client_writer();
    drop(foreground_control_rx);
    foreground_tx.test_close();

    server.clients.insert(
        1,
        ClientConnection::app(
            (80, 24),
            crate::kitty_graphics::HostCellSize::default(),
            crate::terminal_theme::TerminalTheme::default(),
            None,
            1,
            RenderEncoding::SemanticFrame,
            Some(foreground_tx),
        ),
    );
    server.foreground_client_id = Some(1);

    let changed = server.handle_internal_event_with_forwarding(AppEvent::ClipboardWrite {
        content: b"test".to_vec(),
    });

    assert!(!changed);
    assert!(
        server.app.state.copy_feedback.is_none(),
        "clipboard feedback should only show after the foreground client receives the write"
    );
    assert!(
        !server.clients.contains_key(&1),
        "failed targeted send should remove the broken foreground client"
    );
}

#[test]
fn prefix_input_source_targets_foreground_client_only() {
    let mut server = test_headless_server();
    let (background_tx, background_control_rx, _background_rx) = test_client_writer();
    let (foreground_tx, foreground_control_rx, _foreground_rx) = test_client_writer();

    server.clients.insert(
        1,
        ClientConnection::app(
            (120, 40),
            crate::kitty_graphics::HostCellSize::default(),
            crate::terminal_theme::TerminalTheme::default(),
            None,
            1,
            RenderEncoding::SemanticFrame,
            Some(background_tx),
        ),
    );
    server.clients.insert(
        2,
        ClientConnection::app(
            (80, 24),
            crate::kitty_graphics::HostCellSize::default(),
            crate::terminal_theme::TerminalTheme::default(),
            None,
            2,
            RenderEncoding::SemanticFrame,
            Some(foreground_tx),
        ),
    );
    server.foreground_client_id = Some(2);
    server.sync_foreground_client_state();
    // Drain any setup messages (e.g. mouse-capture sync) before exercising the event.
    while foreground_control_rx
        .recv_timeout(Duration::from_millis(20))
        .is_ok()
    {}

    let changed =
        server.handle_internal_event_with_forwarding(AppEvent::PrefixInputSource { active: true });

    assert!(changed);
    match read_server_message(
        foreground_control_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("foreground prefix input-source message"),
    ) {
        ServerMessage::PrefixInputSource { active } => assert!(active),
        other => panic!("expected prefix input-source message, got {other:?}"),
    }
    assert!(
        background_control_rx
            .recv_timeout(Duration::from_millis(50))
            .is_err(),
        "background client should not receive prefix input-source changes"
    );
}

#[test]
fn semantic_notifications_broadcast_only_to_client_shells() {
    let mut server = test_headless_server();
    let (shell_one_tx, shell_one_control, _shell_one_frames) = test_client_writer();
    let (shell_two_tx, shell_two_control, _shell_two_frames) = test_client_writer();
    let (terminal_tx, terminal_control, _terminal_frames) = test_client_writer();
    for (client_id, writer) in [(1, shell_one_tx), (2, shell_two_tx)] {
        server.clients.insert(
            client_id,
            ClientConnection::new_with_mode(
                ClientConnectionMode::ClientShell,
                (80, 24),
                crate::kitty_graphics::HostCellSize::default(),
                client_id,
                RenderEncoding::SemanticFrame,
                Some(writer),
            ),
        );
    }
    server.clients.insert(
        3,
        ClientConnection::new_with_mode(
            ClientConnectionMode::TerminalPending,
            (80, 24),
            crate::kitty_graphics::HostCellSize::default(),
            3,
            RenderEncoding::TerminalAnsi,
            Some(terminal_tx),
        ),
    );
    let event = protocol::SemanticNotification {
        kind: protocol::SemanticNotificationKind::Custom,
        title: "hello".into(),
        body: None,
        sound: None,
        agent: None,
        workspace_id: None,
        tab_id: None,
        pane_id: None,
        position: None,
    };
    assert!(server.send_to_client_shells(ServerMessage::SemanticNotification(event.clone())));
    for receiver in [shell_one_control, shell_two_control] {
        assert_eq!(
            read_server_message(
                receiver
                    .recv_timeout(Duration::from_millis(100))
                    .expect("semantic notification")
            ),
            ServerMessage::SemanticNotification(event.clone())
        );
    }
    assert!(terminal_control
        .recv_timeout(Duration::from_millis(50))
        .is_err());
}

#[test]
fn notification_show_uses_client_shell_policy_independent_of_server_delivery() {
    let mut server = test_headless_server();
    server.app.state.toast_config.delivery = config::ToastDelivery::Off;
    let (shell_tx, shell_control, _shell_frames) = test_client_writer();
    server.clients.insert(
        1,
        ClientConnection::new_with_mode(
            ClientConnectionMode::ClientShell,
            (80, 24),
            crate::kitty_graphics::HostCellSize::default(),
            1,
            RenderEncoding::SemanticFrame,
            Some(shell_tx),
        ),
    );
    let response = server.handle_notification_show_api(
        "notify-shell".into(),
        api::schema::NotificationShowParams {
            title: "plugin title".into(),
            body: Some("plugin body".into()),
            position: Some(crate::config::ToastHerdrPosition::TopLeft),
            sound: api::schema::NotificationShowSound::Done,
        },
    );
    let response: api::schema::SuccessResponse = serde_json::from_str(&response).unwrap();
    assert!(matches!(
        response.result,
        api::schema::ResponseResult::NotificationShow { shown: true, .. }
    ));
    assert_eq!(
        read_server_message(
            shell_control
                .recv_timeout(Duration::from_millis(100))
                .expect("semantic plugin notification")
        ),
        ServerMessage::SemanticNotification(protocol::SemanticNotification {
            kind: protocol::SemanticNotificationKind::Custom,
            title: "plugin title".into(),
            body: Some("plugin body".into()),
            sound: Some(protocol::SemanticNotificationSound::Done),
            agent: None,
            workspace_id: None,
            tab_id: None,
            pane_id: None,
            position: Some(crate::config::ToastHerdrPosition::TopLeft),
        })
    );
}

#[test]
fn notification_show_preserves_foreground_app_with_background_shell() {
    let mut server = test_headless_server();
    server.app.state.toast_config.delivery = config::ToastDelivery::System;
    let (shell_tx, shell_control, _shell_frames) = test_client_writer();
    let (app_tx, app_control, _app_frames) = test_client_writer();
    server.clients.insert(
        1,
        ClientConnection::new_with_mode(
            ClientConnectionMode::ClientShell,
            (80, 24),
            crate::kitty_graphics::HostCellSize::default(),
            1,
            RenderEncoding::SemanticFrame,
            Some(shell_tx),
        ),
    );
    server.clients.insert(
        2,
        ClientConnection::app(
            (80, 24),
            crate::kitty_graphics::HostCellSize::default(),
            crate::terminal_theme::TerminalTheme::default(),
            None,
            2,
            RenderEncoding::SemanticFrame,
            Some(app_tx),
        ),
    );
    server.foreground_client_id = Some(2);
    let response = server.handle_notification_show_api(
        "mixed-notify".into(),
        api::schema::NotificationShowParams {
            title: "mixed title".into(),
            body: Some("mixed body".into()),
            position: None,
            sound: api::schema::NotificationShowSound::None,
        },
    );
    let response: api::schema::SuccessResponse = serde_json::from_str(&response).unwrap();
    assert!(matches!(
        response.result,
        api::schema::ResponseResult::NotificationShow { shown: true, .. }
    ));
    assert!(matches!(
        read_server_message(
            shell_control
                .recv_timeout(Duration::from_millis(100))
                .expect("shell semantic notification")
        ),
        ServerMessage::SemanticNotification(_)
    ));
    assert_eq!(
        read_server_message(
            app_control
                .recv_timeout(Duration::from_millis(100))
                .expect("legacy app notification")
        ),
        ServerMessage::Notify {
            kind: protocol::NotifyKind::SystemToast,
            message: "mixed title".into(),
            body: Some("mixed body".into()),
        }
    );
}

#[test]
fn client_local_notifications_target_foreground_client_only() {
    let mut server = test_headless_server();
    let (background_tx, background_control_rx, _background_rx) = test_client_writer();
    let (foreground_tx, foreground_control_rx, _foreground_rx) = test_client_writer();

    server.clients.insert(
        1,
        ClientConnection::app(
            (120, 40),
            crate::kitty_graphics::HostCellSize::default(),
            crate::terminal_theme::TerminalTheme::default(),
            None,
            1,
            RenderEncoding::SemanticFrame,
            Some(background_tx),
        ),
    );
    server.clients.insert(
        2,
        ClientConnection::app(
            (80, 24),
            crate::kitty_graphics::HostCellSize::default(),
            crate::terminal_theme::TerminalTheme::default(),
            None,
            2,
            RenderEncoding::SemanticFrame,
            Some(foreground_tx),
        ),
    );
    server.foreground_client_id = Some(2);
    server.sync_foreground_client_state();

    assert!(server.send_to_foreground_client(ServerMessage::Notify {
        kind: protocol::NotifyKind::Toast,
        message: "pi finished".to_string(),
        body: Some("workspace 1".to_string()),
    }));

    match read_server_message(
        foreground_control_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("foreground toast message"),
    ) {
        ServerMessage::Notify {
            kind,
            message,
            body,
        } => {
            assert_eq!(kind, protocol::NotifyKind::Toast);
            assert_eq!(message, "pi finished");
            assert_eq!(body.as_deref(), Some("workspace 1"));
        }
        other => panic!("expected toast notify, got {other:?}"),
    }
    assert!(
        background_control_rx
            .recv_timeout(Duration::from_millis(50))
            .is_err(),
        "background client should not receive client-local notifications"
    );
}

#[test]
fn oversized_paste_rejection_notifies_only_the_sending_client() {
    let mut server = test_headless_server();
    let (sender_writer, sender_control_rx, _sender_render_rx) = test_client_writer();
    let (foreground_writer, foreground_control_rx, _foreground_render_rx) = test_client_writer();

    server.clients.insert(
        1,
        ClientConnection::new(
            (120, 40),
            crate::kitty_graphics::HostCellSize::default(),
            1,
            RenderEncoding::SemanticFrame,
            Some(sender_writer),
        ),
    );
    server.clients.insert(
        2,
        ClientConnection::new(
            (80, 24),
            crate::kitty_graphics::HostCellSize::default(),
            2,
            RenderEncoding::SemanticFrame,
            Some(foreground_writer),
        ),
    );
    server.foreground_client_id = Some(2);
    server.sync_foreground_client_state();

    assert!(
        !server.handle_server_event(ServerEvent::ClientPasteRejected {
            client_id: 1,
            size: 5_000_012,
            max: 1_048_576,
        })
    );

    match read_server_message(
        sender_control_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("sending client rejection notification"),
    ) {
        ServerMessage::ClientShellError { message } => assert_eq!(
            message,
            "Paste rejected: Input message is 5000012 bytes; Herdr's limit is 1048576 bytes"
        ),
        other => panic!("expected client shell paste error, got {other:?}"),
    }
    let (shell_writer, shell_control_rx, _shell_render_rx) = test_client_writer();
    server.clients.insert(
        3,
        ClientConnection::new_with_mode(
            ClientConnectionMode::ClientShell,
            (100, 30),
            crate::kitty_graphics::HostCellSize::default(),
            3,
            RenderEncoding::SemanticFrame,
            Some(shell_writer),
        ),
    );
    assert!(
        !server.handle_server_event(ServerEvent::ClientPasteRejected {
            client_id: 3,
            size: 7_000_000,
            max: 1_048_576,
        })
    );
    match read_server_message(
        shell_control_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("client shell rejection error"),
    ) {
        ServerMessage::ClientShellError { message } => assert_eq!(
            message,
            "Paste rejected: Input message is 7000000 bytes; Herdr's limit is 1048576 bytes"
        ),
        other => panic!("expected client shell paste error, got {other:?}"),
    }
    assert!(
        foreground_control_rx
            .recv_timeout(Duration::from_millis(50))
            .is_err(),
        "foreground client must not receive another client's rejection"
    );
    assert_eq!(server.foreground_client_id, Some(2));
    assert_eq!(server.clients.len(), 3);
    assert!(server.app.state.toast.is_none());
}

#[test]
fn update_notification_reaches_client_shell_independent_of_delivery() {
    let mut server = test_headless_server();
    let (client_tx, client_control_rx, _client_rx) = test_client_writer();

    server.clients.insert(
        1,
        ClientConnection::new(
            (80, 24),
            crate::kitty_graphics::HostCellSize::default(),
            1,
            RenderEncoding::SemanticFrame,
            Some(client_tx),
        ),
    );
    server.foreground_client_id = Some(1);
    server.app.state.toast_config.delivery = crate::config::ToastDelivery::Herdr;

    let changed = server.handle_internal_event_with_forwarding(AppEvent::UpdateReady {
        version: "9.9.9".to_string(),
        install_command: "herdr update".into(),
    });

    assert!(changed);
    assert!(matches!(
        read_server_message(
            client_control_rx
                .recv_timeout(Duration::from_millis(100))
                .expect("semantic update notification")
        ),
        ServerMessage::SemanticNotification(protocol::SemanticNotification {
            kind: protocol::SemanticNotificationKind::UpdateInstalled,
            ..
        })
    ));
}

#[test]
fn update_notification_is_semantic_for_system_delivery() {
    let mut server = test_headless_server();
    let (client_tx, client_control_rx, _client_rx) = test_client_writer();

    server.clients.insert(
        1,
        ClientConnection::new(
            (80, 24),
            crate::kitty_graphics::HostCellSize::default(),
            1,
            RenderEncoding::SemanticFrame,
            Some(client_tx),
        ),
    );
    server.foreground_client_id = Some(1);
    server.app.state.toast_config.delivery = crate::config::ToastDelivery::System;

    let changed = server.handle_internal_event_with_forwarding(AppEvent::UpdateReady {
        version: "9.9.9".to_string(),
        install_command: "herdr update".into(),
    });

    assert!(changed);
    match read_server_message(
        client_control_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("semantic update notification"),
    ) {
        ServerMessage::SemanticNotification(notification) => {
            assert_eq!(
                notification.kind,
                protocol::SemanticNotificationKind::UpdateInstalled
            );
            assert_eq!(notification.title, "Herdr v9.9.9 available");
            assert_eq!(
                notification.body.as_deref(),
                Some("detach, run `herdr update`, then run Herdr again to reconnect")
            );
        }
        other => panic!("expected semantic update notification, got {other:?}"),
    }
}

#[test]
fn notification_show_api_forwards_one_semantic_client_notification() {
    let mut server = test_headless_server();
    let (client_tx, client_control_rx, _client_rx) = test_client_writer();

    server.clients.insert(
        1,
        ClientConnection::new(
            (80, 24),
            crate::kitty_graphics::HostCellSize::default(),
            1,
            RenderEncoding::SemanticFrame,
            Some(client_tx),
        ),
    );
    server.foreground_client_id = Some(1);
    server.app.state.toast_config.delivery = crate::config::ToastDelivery::System;

    let (respond_to, response_rx) = std::sync::mpsc::channel();
    let changed = server.handle_api_request_with_shutdown_check(api::ApiRequestMessage {
        request: api::schema::Request {
            id: "notify".into(),
            method: api::schema::Method::NotificationShow(api::schema::NotificationShowParams {
                title: "build failed".into(),
                body: Some("api workspace".into()),
                position: Some(crate::config::ToastHerdrPosition::TopLeft),
                sound: api::schema::NotificationShowSound::Request,
            }),
        },
        respond_to,
        response_write_complete: None,
        stream_active: None,
    });

    assert!(changed);
    let response = response_rx
        .recv_timeout(Duration::from_millis(100))
        .unwrap();
    let parsed: api::schema::SuccessResponse = serde_json::from_str(&response).unwrap();
    assert_eq!(
        parsed.result,
        api::schema::ResponseResult::NotificationShow {
            shown: true,
            reason: api::schema::NotificationShowReason::Shown,
        }
    );
    match read_server_message(
        client_control_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("semantic api notification"),
    ) {
        ServerMessage::SemanticNotification(notification) => {
            assert_eq!(notification.title, "build failed");
            assert_eq!(notification.body.as_deref(), Some("api workspace"));
            assert_eq!(
                notification.sound,
                Some(protocol::SemanticNotificationSound::Request)
            );
        }
        other => panic!("expected semantic api notification, got {other:?}"),
    }
}

#[test]
fn herdr_toast_delivery_keeps_toast_in_frame_without_client_notify() {
    let mut server = test_headless_server();
    let (client_tx, client_control_rx, _client_rx) = test_client_writer();

    server.clients.insert(
        1,
        ClientConnection::app(
            (80, 24),
            crate::kitty_graphics::HostCellSize::default(),
            crate::terminal_theme::TerminalTheme::default(),
            None,
            1,
            RenderEncoding::SemanticFrame,
            Some(client_tx),
        ),
    );
    server.foreground_client_id = Some(1);
    server.app.state.toast_config.delivery = crate::config::ToastDelivery::Herdr;

    let changed = server.handle_internal_event_with_forwarding(AppEvent::UpdateReady {
        version: "9.9.9".to_string(),
        install_command: "herdr update".into(),
    });

    assert!(changed);
    assert!(server.app.state.toast.is_some());
    assert!(
        client_control_rx
            .recv_timeout(Duration::from_millis(50))
            .is_err(),
        "herdr delivery should render in-frame instead of forwarding a client-local notification"
    );
}

#[test]
fn system_toast_delivery_forwards_system_notify_kind() {
    let mut server = test_headless_server();
    let (client_tx, client_control_rx, _client_rx) = test_client_writer();

    server.clients.insert(
        1,
        ClientConnection::app(
            (80, 24),
            crate::kitty_graphics::HostCellSize::default(),
            crate::terminal_theme::TerminalTheme::default(),
            None,
            1,
            RenderEncoding::SemanticFrame,
            Some(client_tx),
        ),
    );
    server.foreground_client_id = Some(1);
    server.app.state.toast_config.delivery = crate::config::ToastDelivery::System;

    let changed = server.handle_internal_event_with_forwarding(AppEvent::UpdateReady {
        version: "9.9.9".to_string(),
        install_command: "herdr update".into(),
    });

    assert!(changed);
    match read_server_message(
        client_control_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("system toast message"),
    ) {
        ServerMessage::Notify {
            kind,
            message,
            body,
        } => {
            assert_eq!(kind, protocol::NotifyKind::SystemToast);
            assert_eq!(message, "v9.9.9 available");
            assert_eq!(
                body.as_deref(),
                Some("detach, run `herdr update`, then run Herdr again to reconnect")
            );
        }
        other => panic!("expected system toast notify, got {other:?}"),
    }
}

#[test]
fn notification_show_api_forwards_system_notification_to_foreground_client() {
    let mut server = test_headless_server();
    let (client_tx, client_control_rx, _client_rx) = test_client_writer();

    server.clients.insert(
        1,
        ClientConnection::app(
            (80, 24),
            crate::kitty_graphics::HostCellSize::default(),
            crate::terminal_theme::TerminalTheme::default(),
            None,
            1,
            RenderEncoding::SemanticFrame,
            Some(client_tx),
        ),
    );
    server.foreground_client_id = Some(1);
    server.app.state.toast_config.delivery = crate::config::ToastDelivery::System;

    let (respond_to, response_rx) = std::sync::mpsc::channel();
    let changed = server.handle_api_request_with_shutdown_check(api::ApiRequestMessage {
        request: api::schema::Request {
            id: "notify".into(),
            method: api::schema::Method::NotificationShow(api::schema::NotificationShowParams {
                title: "build failed".into(),
                body: Some("api workspace".into()),
                position: Some(crate::config::ToastHerdrPosition::TopLeft),
                sound: api::schema::NotificationShowSound::Request,
            }),
        },
        respond_to,
        response_write_complete: None,
        stream_active: None,
    });

    assert!(changed);
    let response = response_rx
        .recv_timeout(Duration::from_millis(100))
        .unwrap();
    let parsed: api::schema::SuccessResponse = serde_json::from_str(&response).unwrap();
    assert_eq!(
        parsed.result,
        api::schema::ResponseResult::NotificationShow {
            shown: true,
            reason: api::schema::NotificationShowReason::Shown,
        }
    );
    let first = read_server_message(
        client_control_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("api notification message"),
    );
    let second = read_server_message(
        client_control_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("api sound message"),
    );

    match first {
        ServerMessage::Notify {
            kind,
            message,
            body,
        } => {
            assert_eq!(kind, protocol::NotifyKind::SystemToast);
            assert_eq!(message, "build failed");
            assert_eq!(body.as_deref(), Some("api workspace"));
        }
        other => panic!("expected api notification, got {other:?}"),
    }
    match second {
        ServerMessage::Notify {
            kind,
            message,
            body,
        } => {
            assert_eq!(kind, protocol::NotifyKind::Sound);
            assert_eq!(message, "agent attention");
            assert!(body.is_none());
        }
        other => panic!("expected api sound, got {other:?}"),
    }
}

#[test]
fn notification_show_api_preserves_colon_in_forwarded_title() {
    let mut server = test_headless_server();
    let (client_tx, client_control_rx, _client_rx) = test_client_writer();

    server.clients.insert(
        1,
        ClientConnection::new(
            (80, 24),
            crate::kitty_graphics::HostCellSize::default(),
            1,
            RenderEncoding::SemanticFrame,
            Some(client_tx),
        ),
    );
    server.foreground_client_id = Some(1);
    server.app.state.toast_config.delivery = crate::config::ToastDelivery::System;

    let (respond_to, response_rx) = std::sync::mpsc::channel();
    let changed = server.handle_api_request_with_shutdown_check(api::ApiRequestMessage {
        request: api::schema::Request {
            id: "notify".into(),
            method: api::schema::Method::NotificationShow(api::schema::NotificationShowParams {
                title: "build: failed".into(),
                body: Some("api workspace".into()),
                position: None,
                sound: api::schema::NotificationShowSound::None,
            }),
        },
        respond_to,
        response_write_complete: None,
        stream_active: None,
    });

    assert!(changed);
    let response = response_rx
        .recv_timeout(Duration::from_millis(100))
        .unwrap();
    let parsed: api::schema::SuccessResponse = serde_json::from_str(&response).unwrap();
    assert_eq!(
        parsed.result,
        api::schema::ResponseResult::NotificationShow {
            shown: true,
            reason: api::schema::NotificationShowReason::Shown,
        }
    );
    match read_server_message(
        client_control_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("semantic api notification"),
    ) {
        ServerMessage::SemanticNotification(notification) => {
            assert_eq!(notification.title, "build: failed");
            assert_eq!(notification.body.as_deref(), Some("api workspace"));
        }
        other => panic!("expected semantic api notification, got {other:?}"),
    }
}

#[test]
fn notification_show_api_validates_empty_title_before_disabled_delivery() {
    let mut server = test_headless_server();
    server.app.state.toast_config.delivery = crate::config::ToastDelivery::Off;

    let (respond_to, response_rx) = std::sync::mpsc::channel();
    let changed = server.handle_api_request_with_shutdown_check(api::ApiRequestMessage {
        request: api::schema::Request {
            id: "notify".into(),
            method: api::schema::Method::NotificationShow(api::schema::NotificationShowParams {
                title: "\n\t".into(),
                body: None,
                position: None,
                sound: api::schema::NotificationShowSound::None,
            }),
        },
        respond_to,
        response_write_complete: None,
        stream_active: None,
    });

    assert!(changed);
    let response = response_rx
        .recv_timeout(Duration::from_millis(100))
        .unwrap();
    let parsed: api::schema::ErrorResponse = serde_json::from_str(&response).unwrap();
    assert_eq!(parsed.error.code, "invalid_params");
    assert_eq!(parsed.error.message, "notification title is empty");
}

#[test]
fn notification_show_api_reports_no_foreground_client() {
    let mut server = test_headless_server();
    server.foreground_client_id = None;
    server.app.state.toast_config.delivery = crate::config::ToastDelivery::System;

    let (respond_to, response_rx) = std::sync::mpsc::channel();
    let changed = server.handle_api_request_with_shutdown_check(api::ApiRequestMessage {
        request: api::schema::Request {
            id: "notify".into(),
            method: api::schema::Method::NotificationShow(api::schema::NotificationShowParams {
                title: "build failed".into(),
                body: None,
                position: None,
                sound: api::schema::NotificationShowSound::Request,
            }),
        },
        respond_to,
        response_write_complete: None,
        stream_active: None,
    });

    assert!(changed);
    let response = response_rx
        .recv_timeout(Duration::from_millis(100))
        .unwrap();
    let parsed: api::schema::SuccessResponse = serde_json::from_str(&response).unwrap();
    assert_eq!(
        parsed.result,
        api::schema::ResponseResult::NotificationShow {
            shown: false,
            reason: api::schema::NotificationShowReason::NoForegroundClient,
        }
    );
}

#[test]
fn notification_show_api_includes_sound_in_semantic_event() {
    let mut server = test_headless_server();
    let (client_tx, client_control_rx, _client_rx) = test_client_writer();

    server.clients.insert(
        1,
        ClientConnection::new(
            (80, 24),
            crate::kitty_graphics::HostCellSize::default(),
            1,
            RenderEncoding::SemanticFrame,
            Some(client_tx),
        ),
    );
    server.foreground_client_id = Some(1);
    server.app.state.toast_config.delivery = crate::config::ToastDelivery::Herdr;

    let (respond_to, response_rx) = std::sync::mpsc::channel();
    assert!(
        server.handle_api_request_with_shutdown_check(api::ApiRequestMessage {
            request: api::schema::Request {
                id: "notify".into(),
                method: api::schema::Method::NotificationShow(
                    api::schema::NotificationShowParams {
                        title: "build failed".into(),
                        body: None,
                        position: None,
                        sound: api::schema::NotificationShowSound::Done,
                    },
                ),
            },
            respond_to,
            response_write_complete: None,
            stream_active: None,
        })
    );

    let response = response_rx
        .recv_timeout(Duration::from_millis(100))
        .unwrap();
    let parsed: api::schema::SuccessResponse = serde_json::from_str(&response).unwrap();
    assert_eq!(
        parsed.result,
        api::schema::ResponseResult::NotificationShow {
            shown: true,
            reason: api::schema::NotificationShowReason::Shown,
        }
    );
    match read_server_message(
        client_control_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("semantic api notification"),
    ) {
        ServerMessage::SemanticNotification(notification) => {
            assert_eq!(notification.title, "build failed");
            assert_eq!(
                notification.sound,
                Some(protocol::SemanticNotificationSound::Done)
            );
        }
        other => panic!("expected semantic api notification, got {other:?}"),
    }
}

#[test]
fn notification_show_api_herdr_toast_expires_headless() {
    let mut server = test_headless_server();
    server.app.state.toast_config.delivery = crate::config::ToastDelivery::Herdr;

    let (respond_to, response_rx) = std::sync::mpsc::channel();
    assert!(
        server.handle_api_request_with_shutdown_check(api::ApiRequestMessage {
            request: api::schema::Request {
                id: "notify".into(),
                method: api::schema::Method::NotificationShow(
                    api::schema::NotificationShowParams {
                        title: "build failed".into(),
                        body: None,
                        position: None,
                        sound: api::schema::NotificationShowSound::None,
                    },
                ),
            },
            respond_to,
            response_write_complete: None,
            stream_active: None,
        })
    );

    let response = response_rx
        .recv_timeout(Duration::from_millis(100))
        .unwrap();
    let parsed: api::schema::SuccessResponse = serde_json::from_str(&response).unwrap();
    assert_eq!(
        parsed.result,
        api::schema::ResponseResult::NotificationShow {
            shown: true,
            reason: api::schema::NotificationShowReason::Shown,
        }
    );
    let deadline = server.app.toast_deadline.expect("api toast deadline");
    assert!(server.handle_scheduled_tasks_headless(deadline, false));
    assert!(server.app.state.toast.is_none());
    assert!(server.app.toast_deadline.is_none());
}

#[test]
fn notification_show_api_forwards_sound_for_herdr_delivery() {
    let mut server = test_headless_server();
    let (client_tx, client_control_rx, _client_rx) = test_client_writer();

    server.clients.insert(
        1,
        ClientConnection::app(
            (80, 24),
            crate::kitty_graphics::HostCellSize::default(),
            crate::terminal_theme::TerminalTheme::default(),
            None,
            1,
            RenderEncoding::SemanticFrame,
            Some(client_tx),
        ),
    );
    server.foreground_client_id = Some(1);
    server.app.state.toast_config.delivery = crate::config::ToastDelivery::Herdr;

    let (respond_to, response_rx) = std::sync::mpsc::channel();
    assert!(
        server.handle_api_request_with_shutdown_check(api::ApiRequestMessage {
            request: api::schema::Request {
                id: "notify".into(),
                method: api::schema::Method::NotificationShow(
                    api::schema::NotificationShowParams {
                        title: "build failed".into(),
                        body: None,
                        position: None,
                        sound: api::schema::NotificationShowSound::Done,
                    },
                ),
            },
            respond_to,
            response_write_complete: None,
            stream_active: None,
        })
    );

    let response = response_rx
        .recv_timeout(Duration::from_millis(100))
        .unwrap();
    let parsed: api::schema::SuccessResponse = serde_json::from_str(&response).unwrap();
    assert_eq!(
        parsed.result,
        api::schema::ResponseResult::NotificationShow {
            shown: true,
            reason: api::schema::NotificationShowReason::Shown,
        }
    );
    match read_server_message(
        client_control_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("api sound message"),
    ) {
        ServerMessage::Notify {
            kind,
            message,
            body,
        } => {
            assert_eq!(kind, protocol::NotifyKind::Sound);
            assert_eq!(message, "agent done");
            assert!(body.is_none());
        }
        other => panic!("expected api sound, got {other:?}"),
    }
}

#[test]
fn startup_idle_does_not_forward_completion() {
    let mut server = test_headless_server();
    let workspace = crate::workspace::Workspace::test_new("active");
    let pane_id = workspace.tabs[0].root_pane;
    server.app.state.workspaces = vec![workspace];
    server.app.state.ensure_test_terminals();
    server.app.state.active = Some(0);
    server.app.state.toast_config.delivery = crate::config::ToastDelivery::System;
    server.app.state.toast_config.delay_seconds = 0;
    server.app.state.sound.enabled = true;

    assert!(
        server.handle_internal_event_with_forwarding(AppEvent::AgentProcessDetected {
            pane_id,
            agent: crate::detect::Agent::Pi,
            observed_at: Instant::now(),
        })
    );

    let (client_tx, client_control_rx, _client_rx) = test_client_writer();
    server.clients.insert(
        1,
        ClientConnection::app(
            (80, 24),
            crate::kitty_graphics::HostCellSize::default(),
            crate::terminal_theme::TerminalTheme::default(),
            Some(false),
            1,
            RenderEncoding::SemanticFrame,
            Some(client_tx),
        ),
    );
    server.foreground_client_id = Some(1);
    server.sync_foreground_client_state();
    while client_control_rx
        .recv_timeout(Duration::from_millis(20))
        .is_ok()
    {}

    assert!(
        server.handle_internal_event_with_forwarding(AppEvent::StateChanged {
            pane_id,
            agent: Some(crate::detect::Agent::Pi),
            state: crate::detect::AgentState::Idle,
            visible_blocker: false,
            visible_working: false,
            process_exited: false,
            observed_at: Instant::now(),
        })
    );
    assert!(
        client_control_rx
            .recv_timeout(Duration::from_millis(50))
            .is_err(),
        "startup readiness should not forward a completion notification"
    );
}

#[test]
fn delayed_agent_notification_forwards_after_deadline() {
    let mut server = test_headless_server();
    let background = crate::workspace::Workspace::test_new("background");
    let pane_id = background.tabs[0].root_pane;
    let foreground = crate::workspace::Workspace::test_new("foreground");
    server.app.state.workspaces = vec![background, foreground];
    server.app.state.ensure_test_terminals();
    server.app.state.active = Some(1);
    server.app.state.selected = 1;
    server.app.state.mode = crate::app::Mode::Terminal;
    server.app.state.toast_config.delivery = crate::config::ToastDelivery::System;
    server.app.state.toast_config.delay_seconds = 1;

    let (client_tx, client_control_rx, _client_rx) = test_client_writer();
    server.clients.insert(
        1,
        ClientConnection::app(
            (80, 24),
            crate::kitty_graphics::HostCellSize::default(),
            crate::terminal_theme::TerminalTheme::default(),
            None,
            1,
            RenderEncoding::SemanticFrame,
            Some(client_tx),
        ),
    );
    server.foreground_client_id = Some(1);
    server.sync_foreground_client_state();

    let changed = server.handle_internal_event_with_forwarding(AppEvent::StateChanged {
        pane_id,
        agent: Some(crate::detect::Agent::Pi),
        state: crate::detect::AgentState::Blocked,
        visible_blocker: false,
        visible_working: false,
        process_exited: false,
        observed_at: Instant::now(),
    });

    assert!(changed);
    assert!(server.app.state.toast.is_none());
    assert!(
        client_control_rx
            .recv_timeout(Duration::from_millis(50))
            .is_err(),
        "delayed transition should not notify immediately"
    );

    let deadline = server
        .app
        .state
        .next_pending_agent_notification_deadline()
        .expect("pending notification deadline");
    assert!(server.handle_scheduled_tasks_headless(deadline, false));

    let first = read_server_message(
        client_control_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("delayed sound message"),
    );
    let second = read_server_message(
        client_control_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("delayed toast message"),
    );

    assert!(matches!(
        first,
        ServerMessage::Notify {
            kind: protocol::NotifyKind::Sound,
            ..
        }
    ));
    match second {
        ServerMessage::Notify {
            kind,
            message,
            body,
        } => {
            assert_eq!(kind, protocol::NotifyKind::SystemToast);
            assert_eq!(message, "pi needs attention");
            assert_eq!(body.as_deref(), Some("background · 1"));
        }
        other => panic!("expected delayed system toast, got {other:?}"),
    }
    assert!(server.app.state.pending_agent_notifications.is_empty());
}

#[test]
fn delayed_active_tab_unfocused_agent_notification_forwards_after_deadline() {
    let mut server = test_headless_server();
    let workspace = crate::workspace::Workspace::test_new("active");
    let pane_id = workspace.tabs[0].root_pane;
    server.app.state.workspaces = vec![workspace];
    server.app.state.ensure_test_terminals();
    server.app.state.active = Some(0);
    server.app.state.selected = 0;
    server.app.state.mode = crate::app::Mode::Terminal;
    server.app.state.toast_config.delivery = crate::config::ToastDelivery::System;
    server.app.state.toast_config.delay_seconds = 1;

    let (client_tx, client_control_rx, _client_rx) = test_client_writer();
    server.clients.insert(
        1,
        ClientConnection::app(
            (80, 24),
            crate::kitty_graphics::HostCellSize::default(),
            crate::terminal_theme::TerminalTheme::default(),
            Some(false),
            1,
            RenderEncoding::SemanticFrame,
            Some(client_tx),
        ),
    );
    server.foreground_client_id = Some(1);
    server.sync_foreground_client_state();

    assert!(
        server.handle_internal_event_with_forwarding(AppEvent::StateChanged {
            pane_id,
            agent: Some(crate::detect::Agent::Pi),
            state: crate::detect::AgentState::Blocked,
            visible_blocker: false,
            visible_working: false,
            process_exited: false,
            observed_at: Instant::now(),
        })
    );
    assert!(server.app.state.toast.is_none());
    assert!(
        client_control_rx
            .recv_timeout(Duration::from_millis(50))
            .is_err(),
        "delayed transition should not notify immediately"
    );

    let deadline = server
        .app
        .state
        .next_pending_agent_notification_deadline()
        .expect("pending notification deadline");
    assert!(server.handle_scheduled_tasks_headless(deadline, false));

    let first = read_server_message(
        client_control_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("delayed sound message"),
    );
    let second = read_server_message(
        client_control_rx
            .recv_timeout(Duration::from_millis(100))
            .expect("delayed toast message"),
    );

    assert!(matches!(
        first,
        ServerMessage::Notify {
            kind: protocol::NotifyKind::Sound,
            ..
        }
    ));
    match second {
        ServerMessage::Notify {
            kind,
            message,
            body,
        } => {
            assert_eq!(kind, protocol::NotifyKind::SystemToast);
            assert_eq!(message, "pi needs attention");
            assert_eq!(body.as_deref(), Some("active · 1"));
        }
        other => panic!("expected delayed system toast, got {other:?}"),
    }
}

#[test]
fn stale_api_agent_report_does_not_forward_done_sound() {
    let mut server = test_headless_server();
    let background = crate::workspace::Workspace::test_new("background");
    let pane_id = background.tabs[0].root_pane;
    let public_pane_id = format!("{}:p1", background.id);
    let foreground = crate::workspace::Workspace::test_new("foreground");
    server.app.state.workspaces = vec![background, foreground];
    server.app.state.ensure_test_terminals();
    let terminal_id = server.app.state.workspaces[0]
        .pane_state(pane_id)
        .unwrap()
        .attached_terminal_id
        .clone();
    server
        .app
        .state
        .terminals
        .get_mut(&terminal_id)
        .unwrap()
        .set_detected_state(
            Some(crate::detect::Agent::Pi),
            crate::detect::AgentState::Idle,
        );
    server
        .app
        .state
        .terminals
        .get_mut(&terminal_id)
        .unwrap()
        .set_persisted_agent_session(crate::agent_resume::PersistedAgentSession {
            source: "herdr:pi".into(),
            agent: "pi".into(),
            session_ref: crate::agent_resume::AgentSessionRef::path(
                std::env::current_dir()
                    .unwrap()
                    .join("headless-pi-session.jsonl")
                    .display()
                    .to_string(),
            )
            .unwrap(),
        });
    server
        .app
        .state
        .terminals
        .get_mut(&terminal_id)
        .unwrap()
        .set_hook_authority(
            "herdr:pi".into(),
            "pi".into(),
            crate::detect::AgentState::Working,
            None,
            Some(20),
        );
    server.app.state.active = Some(1);
    server.app.state.selected = 1;
    server.app.state.mode = crate::app::Mode::Terminal;

    let (client_tx, client_control_rx, _client_rx) = test_client_writer();
    server.clients.insert(
        1,
        ClientConnection::app(
            (80, 24),
            crate::kitty_graphics::HostCellSize::default(),
            crate::terminal_theme::TerminalTheme::default(),
            None,
            1,
            RenderEncoding::SemanticFrame,
            Some(client_tx),
        ),
    );
    server.foreground_client_id = Some(1);
    server.sync_foreground_client_state();

    let (respond_to, response_rx) = std::sync::mpsc::channel();
    let changed = server.handle_api_request_with_shutdown_check(api::ApiRequestMessage {
        request: api::schema::Request {
            id: "stale".into(),
            method: api::schema::Method::PaneReportAgent(api::schema::PaneReportAgentParams {
                pane_id: public_pane_id,
                source: "herdr:pi".into(),
                agent: "pi".into(),
                state: api::schema::PaneAgentState::Idle,
                message: None,
                seq: Some(19),
                agent_session_id: None,
                agent_session_path: None,
            }),
        },
        respond_to,
        response_write_complete: None,
        stream_active: None,
    });

    assert!(changed);
    assert!(response_rx.recv_timeout(Duration::from_millis(100)).is_ok());
    assert_eq!(
        server.app.state.terminals.get(&terminal_id).unwrap().state,
        crate::detect::AgentState::Working
    );
    assert!(
        client_control_rx
            .recv_timeout(Duration::from_millis(50))
            .is_err(),
        "stale idle report must not forward a done sound"
    );
}

/// Verify that calls to the app's internal-event methods only occur inside
/// `handle_internal_event_with_forwarding`. This ensures the forwarding
/// bypass cannot be reintroduced.
#[test]
fn no_handle_internal_event_bypass_in_module() {
    let source = include_str!("../../headless.rs");

    // Find all lines containing handle_internal_event
    let mut bypass_lines: Vec<String> = Vec::new();
    let mut inside_forwarding_method = false;
    let mut forwarding_method_brace_depth = 0u32;

    for (i, line) in source.lines().enumerate() {
        let line_num = i + 1;

        // Track when we're inside handle_internal_event_with_forwarding
        if line.contains("fn handle_internal_event_with_forwarding") {
            inside_forwarding_method = true;
            forwarding_method_brace_depth = 0;
        }

        if inside_forwarding_method {
            // Count braces to track when we exit the method
            for ch in line.chars() {
                match ch {
                    '{' => forwarding_method_brace_depth += 1,
                    '}' => {
                        forwarding_method_brace_depth =
                            forwarding_method_brace_depth.saturating_sub(1);
                        if forwarding_method_brace_depth == 0 {
                            inside_forwarding_method = false;
                        }
                    }
                    _ => {}
                }
            }
        } else if (line.contains("self.app.handle_internal_event(")
            || line.contains("self.app.handle_internal_event_with_render_impact(")
            || line.contains("self.app.handle_internal_event_with_prefix_sync("))
            && !line.trim().starts_with("///")
            && !line.contains("contains(")
        {
            // Internal-event call outside the forwarding method.
            bypass_lines.push(format!("line {}: {}", line_num, line.trim()));
        }
    }

    assert!(
        bypass_lines.is_empty(),
        "Found direct calls to self.app.handle_internal_event outside \
             handle_internal_event_with_forwarding (bypass risk):\n  {}",
        bypass_lines.join("\n  ")
    );
}

/// Two displays of very different sizes, each on its own tab. The narrow
/// one sends a pointer event; the geometry it is resolved against must be
/// the narrow display's, not whatever the last render left behind.
///
/// TP-MCF-VIEW-01
#[tokio::test]
async fn a_pointer_event_is_resolved_against_its_own_display() {
    let mut server = test_headless_server();
    let mut workspace = crate::workspace::Workspace::test_new("test");
    let second_tab = workspace.test_add_tab(Some("second"));
    server.app.state.workspaces = vec![workspace];
    server.app.state.active = Some(0);
    server.app.state.selected = 0;
    server.app.state.mode = crate::app::Mode::Terminal;

    let (wide_tx, _wide_control_rx, _wide_rx) = test_client_writer();
    let (narrow_tx, _narrow_control_rx, _narrow_rx) = test_client_writer();
    server.clients.insert(
        1,
        ClientConnection::app(
            (200, 50),
            crate::kitty_graphics::HostCellSize::default(),
            crate::terminal_theme::TerminalTheme::default(),
            None,
            1,
            RenderEncoding::SemanticFrame,
            Some(wide_tx),
        ),
    );
    server.clients.insert(
        2,
        ClientConnection::app(
            (60, 20),
            crate::kitty_graphics::HostCellSize::default(),
            crate::terminal_theme::TerminalTheme::default(),
            None,
            2,
            RenderEncoding::SemanticFrame,
            Some(narrow_tx),
        ),
    );
    server.foreground_client_id = Some(1);

    server.render_and_stream();
    let previous = server.app.state.enter_viewer(Some(2));
    server.app.state.workspaces[0].set_active_tab(second_tab);
    server.app.state.restore_viewer(previous);
    server.render_and_stream();

    // After a frame the shared view belongs to whichever client drew last,
    // which is the foreground one. Prove that first, so the assertion
    // below is measuring the fix and not an accident of ordering.
    let after_render_width = server.app.state.view.terminal_area.width;
    assert!(
        after_render_width > 60,
        "the frame leaves the wide display's geometry behind, got {after_render_width}"
    );

    server.handle_client_input_events(
        2,
        vec![crate::raw_input::RawInputEvent::Mouse(
            crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::Moved,
                column: 1,
                row: 1,
                modifiers: crossterm::event::KeyModifiers::empty(),
            },
        )],
    );

    let resolved_width = server.app.state.view.terminal_area.width;
    assert!(
        resolved_width <= 60,
        "the narrow display's pointer must be resolved against its own layout, got {resolved_width}"
    );
}
