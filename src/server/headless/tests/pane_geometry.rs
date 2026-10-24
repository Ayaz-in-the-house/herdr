//! The pane-geometry behaviour registry entries, exercised.
//!
//! Written before the producer, and the expected results were written before
//! either — `.local/prd/2026-09-01-pane-geometry-broadcast-PRD.md` §4 carries
//! the reason each of these exists, which is the part a test name cannot hold.

use super::*;
use crate::protocol::{capability, CapabilityEntry, CapabilitySet, PaneRect};

fn geometry_test_server() -> (HeadlessServer, crate::layout::PaneId, String) {
    let mut server = test_headless_server();
    let workspace = crate::workspace::Workspace::test_new("geometry");
    let pane_id = workspace.focused_pane_id().expect("focused pane");
    server.app.state.workspaces = vec![workspace];
    server.app.state.active = Some(0);
    server.app.state.selected = 0;
    server.app.state.ensure_test_terminals();
    server.app.state.view.terminal_area = ratatui::layout::Rect::new(0, 0, 80, 24);
    let public = server
        .app
        .public_pane_id(0, pane_id)
        .expect("public pane id");
    (server, pane_id, public)
}

fn connect_geometry(
    server: &mut HeadlessServer,
    client_id: u64,
    wants_geometry: bool,
    cell: (u32, u32),
) -> crate::server::client_transport::TestQueueDrain {
    let (writer, drain) = ClientWriter::test_channel_through_queue();
    let mut client = ClientConnection::app(
        (80, 24),
        crate::kitty_graphics::HostCellSize {
            width_px: cell.0,
            height_px: cell.1,
        },
        crate::terminal_theme::TerminalTheme::default(),
        None,
        client_id,
        RenderEncoding::SemanticFrame,
        Some(writer),
    );
    client.capabilities = CapabilitySet::from_entries(if wants_geometry {
        vec![CapabilityEntry::flag(capability::PANE_GEOMETRY)]
    } else {
        Vec::new()
    });
    server.clients.insert(client_id, client);
    drain
}

/// Every `PaneGeometry` that reached this client, paired with the lane it
/// travelled on. The lane is returned rather than dropped because TP-GEOM-PUB-01 turns
/// on it: geometry that can be dropped is geometry that leaves a player drawing
/// in the wrong place for as long as the pane stays put.
/// Drive the real render path once, exactly as the loop does.
///
/// Tests used to call `fan_out_pane_geometry` on its own, back when it
/// recomputed the layout for itself. It now reads `view.pane_infos` — the
/// vector the server actually drew — so a test that never renders is asking
/// about a screen that was never drawn. The claim each test makes is
/// unchanged; only the way it reaches the code is. (G22)
fn render(server: &mut HeadlessServer) {
    server.render_and_stream();
}

fn geometries(
    drain: &crate::server::client_transport::TestQueueDrain,
) -> Vec<(crate::server::client_transport::WriteLane, ServerMessage)> {
    drain
        .drain()
        .into_iter()
        .map(|(lane, bytes)| {
            let mut cursor = std::io::Cursor::new(bytes.as_slice());
            let message = protocol::read_message(&mut cursor, crate::protocol::MAX_FRAME_SIZE)
                .expect("framed server message");
            (lane, message)
        })
        .filter(|(_, message)| matches!(message, ServerMessage::PaneGeometry { .. }))
        .collect()
}

fn rects(message: &ServerMessage) -> &[PaneRect] {
    match message {
        ServerMessage::PaneGeometry { panes, .. } => panes,
        other => panic!("expected PaneGeometry, got {other:?}"),
    }
}

/// TP-GEOM-PUB-01 · the happy path. A capable client that sees a layout change receives
/// exactly one geometry describing every pane of the active tab.
#[tokio::test]
async fn pane_geometry_is_broadcast_to_a_capable_client() {
    let (mut server, _pane, public) = geometry_test_server();
    let drain = connect_geometry(&mut server, 1, true, (10, 20));
    server.foreground_client_id = Some(1);
    server.sync_foreground_client_state();

    render(&mut server);

    let sent = geometries(&drain);
    assert_eq!(
        sent.len(),
        1,
        "one geometry per change, not zero and not two"
    );
    let panes = rects(&sent[0].1);
    assert_eq!(panes.len(), 1, "the test workspace has one pane");
    assert_eq!(
        panes[0].pane_id, public,
        "TP-GEOM-PUB-01: the id space is MediaOpen's"
    );
    // TP-GEOM-PUB-01 · the control lane, not the media one. Media is droppable by
    // design; a dropped rectangle leaves a player painting in the wrong place
    // until the layout happens to change again, which may be never.
    assert_eq!(
        sent[0].0,
        crate::server::client_transport::WriteLane::Control,
        "geometry is not droppable"
    );
}

/// TP-GEOM-PUB-01 · a client that never announced `PANE_GEOMETRY` pays nothing. The
/// resource doctrine's claim is that an idle looker costs zero; a broadcast
/// that ignores the handshake would make that false without anyone noticing.
#[tokio::test]
async fn pane_geometry_is_not_sent_to_a_client_that_did_not_ask() {
    let (mut server, _pane, _public) = geometry_test_server();
    let drain = connect_geometry(&mut server, 1, false, (10, 20));
    server.foreground_client_id = Some(1);
    server.sync_foreground_client_state();

    render(&mut server);

    assert!(
        geometries(&drain).is_empty(),
        "a client without the capability receives no geometry at all"
    );
}

/// TP-GEOM-PUB-01 · the cell size travels with the rectangles, and the rectangles are
/// already in pixels. A consumer must never have to multiply.
#[tokio::test]
async fn pane_geometry_carries_the_cell_size_it_was_computed_with() {
    let (mut server, _pane, _public) = geometry_test_server();
    let drain = connect_geometry(&mut server, 1, true, (7, 15));
    server.foreground_client_id = Some(1);
    server.sync_foreground_client_state();

    render(&mut server);

    let sent = geometries(&drain);
    let ServerMessage::PaneGeometry {
        cell_width_px,
        cell_height_px,
        ..
    } = &sent[0].1
    else {
        panic!("expected PaneGeometry");
    };
    assert_eq!((*cell_width_px, *cell_height_px), (7, 15));
}

/// TP-GEOM-CLIP-01 · ⭐ the published inner box comes from `inner_rect`, not from `rect`.
///
/// The first version of this test asserted `inner_x >= x` and `inner_width <=
/// width` against the default fixture, and a mutation that published `rect` in
/// the inner fields **survived it**: one pane draws no border, so `inner_rect`
/// already equals `rect` and both assertions held on an equality that proved
/// nothing (E30 — a fixture that makes the feature unobservable).
///
/// So the fixture is adversarial now: a pane inset by one cell on every side,
/// injected directly. Reading `rect` where `inner_rect` belongs moves four
/// numbers, and each of them is checked by value rather than by inequality.
///
/// This is the clip rectangle, which is why it earns its own fixture: a player
/// handed the outer box paints over the border the terminal drew around it.
#[test]
fn published_inner_box_is_the_clip_rect_not_the_outer_box() {
    let (mut server, pane, public) = geometry_test_server();
    let drain = connect_geometry(&mut server, 1, true, (10, 20));

    // Outer 0,0 40x20 cells · inner 1,1 38x18 — a one-cell border all round.
    server.app.state.view.pane_infos = vec![crate::layout::PaneInfo {
        id: pane,
        rect: ratatui::layout::Rect::new(0, 0, 40, 20),
        inner_rect: ratatui::layout::Rect::new(1, 1, 38, 18),
        scrollbar_rect: None,
        borders: ratatui::widgets::Borders::ALL,
        is_focused: true,
    }];

    server.fan_out_pane_geometry();

    let sent = geometries(&drain);
    assert_eq!(sent.len(), 1);
    let rect = rects(&sent[0].1)
        .iter()
        .find(|candidate| candidate.pane_id == public)
        .expect("the injected pane is published");

    assert_eq!((rect.x, rect.y), (0, 0), "outer origin, in pixels");
    assert_eq!(
        (rect.width, rect.height),
        (400, 400),
        "outer size, in pixels"
    );
    assert_eq!(
        (rect.inner_x, rect.inner_y),
        (10, 20),
        "inner origin is inset one cell — publishing rect.x here would give 0"
    );
    assert_eq!(
        (rect.inner_width, rect.inner_height),
        (380, 360),
        "inner size drops both borders — publishing rect.width would give 400"
    );
    assert!(
        rect.inner_x > rect.x && rect.inner_width < rect.width,
        "strict, so publishing the outer box twice cannot pass"
    );
}

/// TP-GEOM-PUB-01 · a client that reported no cell size (kitty graphics off) is sent
/// nothing rather than a set of zero-sized boxes. Zero pixels is not a smaller
/// rectangle; it is an invisible one, and drawing into it fails silently.
#[tokio::test]
async fn pane_geometry_is_withheld_when_the_client_has_no_cell_size() {
    let (mut server, _pane, _public) = geometry_test_server();
    let drain = connect_geometry(&mut server, 1, true, (0, 0));
    server.foreground_client_id = Some(1);
    server.sync_foreground_client_state();

    render(&mut server);

    assert!(
        geometries(&drain).is_empty(),
        "no cell size means no pixels, and no pixels means no message"
    );
}

/// TP-GEOM-PUB-01 · the generation advances once per *change*, not once per render.
///
/// Two renders with a changed cell size are two different screens, so two
/// geometries travel and the number between them goes up by exactly one. The
/// quiet guarantee is what makes this test say "change" where it used to say
/// "broadcast": a render that alters nothing no longer produces one.
#[tokio::test]
async fn pane_geometry_generation_advances_once_per_change() {
    let (mut server, _pane, _public) = geometry_test_server();
    let drain = connect_geometry(&mut server, 1, true, (10, 20));
    server.foreground_client_id = Some(1);
    server.sync_foreground_client_state();

    render(&mut server);
    if let Some(client) = server.clients.get_mut(&1) {
        client.cell_size = crate::kitty_graphics::HostCellSize {
            width_px: 7,
            height_px: 15,
        };
    }
    render(&mut server);

    let sent = geometries(&drain);
    assert_eq!(sent.len(), 2, "a changed cell size is a changed screen");
    let generation = |message: &ServerMessage| match message {
        ServerMessage::PaneGeometry { generation, .. } => *generation,
        other => panic!("expected PaneGeometry, got {other:?}"),
    };
    assert_eq!(
        generation(&sent[1].1),
        generation(&sent[0].1) + 1,
        "a gap in this number is a consumer's only way to know it missed one"
    );
}

/// TP-GEOM-REACH-01 · ⭐⭐ THE REACHABILITY TEST.
///
/// Everything above proves the code behaves. None of it proves anything calls
/// it. On 2026-09-01 a feature shipped, installed, ran, and moved nothing on
/// the wire: nineteen tests, four mutations and a green gate all agreed it was
/// correct, and not one had asked whether production reached it (E28/G21).
/// This test asks, by driving the render path and looking on the wire.
///
/// Its name deliberately carries `pane_geometry` so that the obvious filter —
/// `-E 'test(pane_geometry)'` — cannot run the suite without running this.
#[tokio::test]
async fn a_render_reaches_pane_geometry_through_the_production_path() {
    let (mut server, _pane, public) = geometry_test_server();
    let drain = connect_geometry(&mut server, 1, true, (10, 20));
    server.foreground_client_id = Some(1);
    server.sync_foreground_client_state();

    render(&mut server);

    let sent = geometries(&drain);
    assert!(
        !sent.is_empty(),
        "a real render must publish geometry; if this fails the feature is \
         unreachable in production however green the rest of this file is"
    );
    assert!(
        rects(&sent[0].1).iter().any(|pane| pane.pane_id == public),
        "the published set names the pane that is actually on screen"
    );
}

/// TP-GEOM-QUIET-01 · ⛔ THE QUIET GUARANTEE.
///
/// A screen that did not change must cost nothing. That is the resource
/// doctrine's whole claim, and a broadcast bolted to the render path would
/// break it at sixty messages a second while every other test here stayed
/// green.
#[tokio::test]
async fn repeated_renders_change_nothing_and_pane_geometry_stays_silent() {
    let (mut server, _pane, _public) = geometry_test_server();
    let drain = connect_geometry(&mut server, 1, true, (10, 20));
    server.foreground_client_id = Some(1);
    server.sync_foreground_client_state();

    render(&mut server);
    assert_eq!(
        geometries(&drain).len(),
        1,
        "the first render publishes the initial state"
    );

    for _ in 0..20 {
        render(&mut server);
    }

    assert_eq!(
        geometries(&drain).len(),
        0,
        "twenty renders with an unchanged layout must publish nothing at all"
    );
}

/// TP-GEOM-PUB-01 · a client that asks is told where the panes are *now*.
///
/// Publication is derived from renders, and a session nobody is touching has
/// no renders due — so a consumer that attached to a still screen received
/// nothing at all until something moved, which on a still screen is never.
/// Measured on the product, not deduced: the listener connected, the handshake
/// succeeded, and stdout stayed empty.
#[tokio::test]
async fn a_pane_geometry_client_is_answered_on_connect_not_on_the_next_change() {
    let (mut server, _pane, public) = geometry_test_server();
    server.foreground_client_id = Some(1);
    server.sync_foreground_client_state();
    render(&mut server);

    // A second client asks for geometry after the screen has settled, and
    // nothing moves afterwards.
    let drain = connect_geometry(&mut server, 2, true, (10, 20));
    server.fan_out_pane_geometry();

    let sent = geometries(&drain);
    assert_eq!(
        sent.len(),
        1,
        "the newcomer is told the current layout without waiting for a change"
    );
    assert!(
        rects(&sent[0].1).iter().any(|pane| pane.pane_id == public),
        "and the answer names the pane that is on screen"
    );
}
