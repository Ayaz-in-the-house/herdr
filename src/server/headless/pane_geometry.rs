//! Where every visible pane is, in the client's own pixels.
//!
//! The server already computes all of this for its own drawing: `TileLayout::
//! panes()` returns an outer rect, an inner rect and a focus flag per pane, and
//! it has done so since long before anything wanted to read them. None of it
//! has ever left the server, and that — not a codec, not a transport — is the
//! single reason a client cannot place drawing of its own over a pane.
//!
//! Sent on change and never on a tick. A still screen must cost nothing.
//!
//! Per client rather than broadcast-identical, because the rectangles are in
//! *that client's* pixels: two clients attached at different cell sizes are
//! looking at the same layout through different rulers.

use super::*;
use crate::protocol::{capability, PaneRect};

impl HeadlessServer {
    /// Clients that asked for geometry, can be written to, and reported a cell
    /// size to convert with.
    ///
    /// A client with no cell size is excluded rather than sent zeroes: zero
    /// pixels is not a smaller rectangle, it is an invisible one, and a
    /// consumer drawing into it fails without any error to notice. TP-GEOM-PUB-01
    fn pane_geometry_clients(&self) -> Vec<u64> {
        let mut wanted: Vec<u64> = self
            .clients
            .iter()
            .filter(|(_, client)| {
                client.writer.is_some()
                    && client.capabilities.has(capability::PANE_GEOMETRY)
                    && client.cell_size.width_px > 0
                    && client.cell_size.height_px > 0
            })
            .map(|(id, _)| *id)
            .collect();
        wanted.sort_unstable();
        wanted
    }

    /// The active tab's panes, converted to one client's pixels.
    ///
    /// Read from `view.pane_infos` rather than recomputed from the layout tree.
    /// That vector is what the server actually drew this frame; recomputing it
    /// here would create a second answer to the same question, and the two
    /// would eventually disagree about a frame nobody was watching.
    ///
    /// Panes of background tabs never appear in it, and that absence *is* the
    /// visibility signal. A `visible: bool` would be a second fact to keep
    /// true, and this repository has already paid once for a bulk guard that
    /// went stale because a new field was added beside it and not to it.
    fn pane_rects_for_cell(&self, cell_w: u32, cell_h: u32) -> Vec<PaneRect> {
        let Some(ws_idx) = self.app.state.active else {
            return Vec::new();
        };
        self.app
            .state
            .view
            .pane_infos
            .iter()
            .filter_map(|info| {
                let pane_id = self.app.public_pane_id(ws_idx, info.id)?;
                Some(PaneRect {
                    pane_id,
                    x: u32::from(info.rect.x) * cell_w,
                    y: u32::from(info.rect.y) * cell_h,
                    width: u32::from(info.rect.width) * cell_w,
                    height: u32::from(info.rect.height) * cell_h,
                    inner_x: u32::from(info.inner_rect.x) * cell_w,
                    inner_y: u32::from(info.inner_rect.y) * cell_h,
                    inner_width: u32::from(info.inner_rect.width) * cell_w,
                    inner_height: u32::from(info.inner_rect.height) * cell_h,
                    focused: info.is_focused,
                })
            })
            .collect()
    }

    /// Broadcast the active tab's pane rectangles to every client that asked.
    ///
    /// On the control lane, not the media one. Media chunks are droppable on
    /// purpose — a lost audio packet is concealed and forgotten. A lost
    /// rectangle is not: it leaves a player drawing in the wrong place until
    /// the layout happens to change again, which may be never. TP-GEOM-PUB-01
    pub(super) fn fan_out_pane_geometry(&mut self) {
        let targets = self.pane_geometry_clients();
        if targets.is_empty() {
            // The whole feature costs one map lookup while nobody wants it.
            self.last_pane_geometry.clear();
            return;
        }
        self.last_pane_geometry
            .retain(|client_id, _| self.clients.contains_key(client_id));

        // Compute first, compare second, send third. Emitting on the render
        // path rather than from a list of layout events is deliberate: an
        // event list has to be complete, and the one thing this repository
        // has proved about event lists is that one of them is always missing.
        // A comparison cannot be missing an event. TP-GEOM-QUIET-01
        let mut pending: Vec<(u64, u32, u32, Vec<PaneRect>)> = Vec::new();
        for client_id in targets {
            let Some((cell_w, cell_h)) = self
                .clients
                .get(&client_id)
                .map(|client| (client.cell_size.width_px, client.cell_size.height_px))
            else {
                continue;
            };
            let panes = self.pane_rects_for_cell(cell_w, cell_h);
            if self.last_pane_geometry.get(&client_id) == Some(&(cell_w, cell_h, panes.clone())) {
                continue;
            }
            pending.push((client_id, cell_w, cell_h, panes));
        }
        if pending.is_empty() {
            return;
        }

        self.pane_geometry_generation = self.pane_geometry_generation.saturating_add(1);
        let generation = self.pane_geometry_generation;
        for (client_id, cell_width_px, cell_height_px, panes) in pending {
            let message = ServerMessage::PaneGeometry {
                generation,
                cell_width_px,
                cell_height_px,
                panes: panes.clone(),
            };
            let Ok(framed) = Self::frame_server_message(&message) else {
                continue;
            };
            let sent = self
                .clients
                .get(&client_id)
                .and_then(|client| client.writer.as_ref())
                .is_some_and(|writer| writer.control.send(framed).is_ok());
            if sent {
                self.last_pane_geometry
                    .insert(client_id, (cell_width_px, cell_height_px, panes));
            }
        }
    }
}
