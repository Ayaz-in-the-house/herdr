use super::*;
use crate::app::pane_audio::Outbound;
use crate::protocol::{capability, codec, CapabilityEntry, CapabilitySet};
use crate::server::client_transport::WriteLane;

fn audio_test_server() -> (HeadlessServer, crate::layout::PaneId, String) {
    let mut server = test_headless_server();
    let workspace = crate::workspace::Workspace::test_new("audio");
    let pane_id = workspace.focused_pane_id().expect("focused pane");
    server.app.state.workspaces = vec![workspace];
    server.app.state.active = Some(0);
    server.app.state.selected = 0;
    server.app.state.ensure_test_terminals();
    let public = server
        .app
        .public_pane_id(0, pane_id)
        .expect("public pane id");
    (server, pane_id, public)
}

fn connect(
    server: &mut HeadlessServer,
    client_id: u64,
    sink: Option<&str>,
) -> crate::server::client_transport::TestQueueDrain {
    let (writer, drain) = ClientWriter::test_channel_through_queue();
    let mut client = ClientConnection::app(
        (80, 24),
        crate::kitty_graphics::HostCellSize::default(),
        crate::terminal_theme::TerminalTheme::default(),
        None,
        client_id,
        RenderEncoding::SemanticFrame,
        Some(writer),
    );
    client.capabilities = CapabilitySet::from_entries(match sink {
        Some(codec) => vec![
            CapabilityEntry::flag(capability::MEDIA_STREAMS),
            CapabilityEntry::with_values(capability::AUDIO_SINK, [codec]),
        ],
        None => Vec::new(),
    });
    server.clients.insert(client_id, client);
    drain
}

fn decode(bytes: &[u8]) -> ServerMessage {
    let mut cursor = std::io::Cursor::new(bytes);
    protocol::read_message(&mut cursor, crate::protocol::MAX_FRAME_SIZE)
        .expect("framed server message")
}

fn open_stream(server: &mut HeadlessServer, public: &str, owner: &str) {
    let response = server.app.handle_api_request(api::schema::Request {
        id: "open".into(),
        method: api::schema::Method::PaneAudioStreamOpen(api::schema::PaneAudioStreamParams {
            pane_id: public.to_owned(),
            sample_rate_hz: 48_000,
            channels: 2,
            format: "f32le".into(),
            owner: owner.to_owned(),
        }),
    });
    assert!(
        serde_json::from_str::<api::schema::SuccessResponse>(&response).is_ok(),
        "{response}"
    );
    server.flush_pane_audio_outbound();
}

fn feed_frame(server: &mut HeadlessServer, public: &str, owner: &str) {
    let response = server.app.handle_api_request(api::schema::Request {
        id: "chunk".into(),
        method: api::schema::Method::PaneAudioStreamChunk(api::schema::PaneAudioChunkParams {
            pane_id: public.to_owned(),
            owner: owner.to_owned(),
            pcm: 0.05_f32
                .to_le_bytes()
                .repeat(api::schema::PANE_AUDIO_FRAME_BYTES / 4),
        }),
    });
    assert!(
        serde_json::from_str::<api::schema::SuccessResponse>(&response).is_ok(),
        "{response}"
    );
    server.flush_pane_audio_outbound();
}

// TP-MEDIA-DISPATCH-01
#[test]
fn an_audio_request_off_the_graphics_path_still_reaches_the_fan_out() {
    // The main loop routes an API request through `handle_api_request_with_shutdown_check`
    // whenever no graphics runtime is active — which, for an audio-only session, is
    // always. That path used to skip the audio fan-out entirely: the stream opened,
    // the API answered ok, and no client was ever offered it. This drives the real
    // request path, not the app handler the other tests call directly.
    let (mut server, _pane, public) = audio_test_server();
    let opus = connect(&mut server, 1, Some(codec::OPUS));
    let (tx, _rx) = std::sync::mpsc::channel();
    server.handle_api_request_with_shutdown_check(api::ApiRequestMessage {
        request: api::schema::Request {
            id: "open".into(),
            method: api::schema::Method::PaneAudioStreamOpen(api::schema::PaneAudioStreamParams {
                pane_id: public.clone(),
                sample_rate_hz: 48_000,
                channels: 2,
                format: "f32le".into(),
                owner: "owner".into(),
            }),
        },
        respond_to: tx,
        response_write_complete: None,
        stream_active: None,
    });
    let items = opus.drain();
    assert_eq!(items.len(), 1, "the opus client is offered the stream");
    assert_eq!(
        items[0].0,
        WriteLane::Control,
        "open rides the reliable lane"
    );
    assert!(
        matches!(
            decode(&items[0].1),
            ServerMessage::MediaOpen { stream_id: 1, .. }
        ),
        "the offer is a MediaOpen for the first stream"
    );
}

// TP-MEDIA-CAP-07
#[tokio::test]
async fn a_stream_is_offered_only_to_clients_that_negotiated_an_opus_sink() {
    let (mut server, _pane, public) = audio_test_server();
    let opus = connect(&mut server, 1, Some(codec::OPUS));
    let silent = connect(&mut server, 2, None);
    let other = connect(&mut server, 3, Some(codec::PCM_S16LE));

    open_stream(&mut server, &public, "owner");

    let opus_items = opus.drain();
    assert_eq!(opus_items.len(), 1);
    assert_eq!(
        opus_items[0].0,
        WriteLane::Control,
        "open rides the reliable lane"
    );
    match decode(&opus_items[0].1) {
        ServerMessage::MediaOpen {
            stream_id,
            codec: name,
            target_latency_us,
            ..
        } => {
            assert_eq!(stream_id, 1);
            assert_eq!(name, codec::OPUS);
            assert_eq!(target_latency_us, crate::app::pane_audio::TARGET_LATENCY_US);
        }
        other => panic!("expected MediaOpen, got {other:?}"),
    }
    assert!(
        silent.drain().is_empty(),
        "a client without a sink hears nothing"
    );
    assert!(
        other.drain().is_empty(),
        "a client with another codec hears nothing"
    );
    let session = server
        .app
        .pane_audio
        .session_for_stream(1)
        .expect("session");
    assert_eq!(session.subscribers(), vec![1]);
    assert_eq!(session.counters().declined_no_sink, 2);
}

// TP-MEDIA-CREDIT-02
#[tokio::test]
async fn credit_reported_by_a_client_gates_what_it_is_sent() {
    let (mut server, _pane, public) = audio_test_server();
    let starved = connect(&mut server, 1, Some(codec::OPUS));
    let fed = connect(&mut server, 2, Some(codec::OPUS));
    open_stream(&mut server, &public, "owner");
    starved.drain();
    fed.drain();

    // Nobody has reported room: the frame is dropped at the source.
    feed_frame(&mut server, &public, "owner");
    assert!(starved.drain().is_empty());
    assert!(fed.drain().is_empty());
    let counters = server
        .app
        .pane_audio
        .session_for_stream(1)
        .unwrap()
        .counters();
    assert_eq!(counters.dropped_no_credit, 1);

    assert!(!server.handle_server_event(ServerEvent::ClientMediaCredit {
        client_id: 2,
        stream_id: 1,
        chunks: 2,
    }));
    assert!(!server.handle_server_event(ServerEvent::ClientMediaCredit {
        client_id: 1,
        stream_id: 1,
        chunks: 0,
    }));
    for _ in 0..3 {
        feed_frame(&mut server, &public, "owner");
    }
    assert!(starved.drain().is_empty(), "zero credit means zero chunks");
    let fed_items = fed.drain();
    assert_eq!(
        fed_items.len(),
        2,
        "two chunks of room, then the source drops"
    );
    assert!(fed_items.iter().all(|(lane, _)| *lane == WriteLane::Media));
    assert!(matches!(
        decode(&fed_items[0].1),
        ServerMessage::MediaChunk { seq: 1, .. }
    ));
    let counters = server
        .app
        .pane_audio
        .session_for_stream(1)
        .unwrap()
        .counters();
    assert_eq!(counters.sent, 2);
    assert_eq!(counters.dropped_no_credit, 2);

    // A departed client is forgotten; its stale room can never send to it.
    assert!(server.handle_server_event(ServerEvent::ClientDisconnected { client_id: 2 }));
    assert_eq!(
        server
            .app
            .pane_audio
            .session_for_stream(1)
            .unwrap()
            .subscribers(),
        vec![1],
        "only the departed client is forgotten"
    );
}

// TP-MEDIA-LANE-02
#[tokio::test]
async fn a_full_media_lane_drops_the_chunk_and_control_still_leaves_first() {
    let (mut server, _pane, public) = audio_test_server();
    let drain = connect(&mut server, 1, Some(codec::OPUS));
    open_stream(&mut server, &public, "owner");
    assert!(!server.handle_server_event(ServerEvent::ClientMediaCredit {
        client_id: 1,
        stream_id: 1,
        chunks: u16::MAX,
    }));

    // 64 fit the lane; the 65th is refused and counted, not queued.
    for _ in 0..65 {
        feed_frame(&mut server, &public, "owner");
    }
    let counters = server
        .app
        .pane_audio
        .session_for_stream(1)
        .unwrap()
        .counters();
    assert_eq!(counters.sent, 64);
    assert_eq!(counters.dropped_full, 1);

    let items = drain.drain();
    assert_eq!(
        items[0].0,
        WriteLane::Control,
        "the open left before any chunk"
    );
    assert_eq!(items.len(), 65);
    assert!(items[1..].iter().all(|(lane, _)| *lane == WriteLane::Media));

    let _ = Outbound::Open {
        stream_id: 0,
        pane_id: String::new(),
    };
}

// TP-MEDIA-FLOW-08
#[tokio::test]
async fn a_full_lane_costs_only_its_own_client_the_chunk() {
    // Two listeners with room; one of them never reads. Its lane fills at 64
    // and the 65th chunk is refused there — and only there. The listener that
    // keeps up hears all 65, the stream's clock does not wait for the slow
    // one, and the refusal spends no credit. One client's full lane holding
    // up another's sound is the head-of-line failure this pins against.
    let (mut server, _pane, public) = audio_test_server();
    let stuck = connect(&mut server, 1, Some(codec::OPUS));
    let fluent = connect(&mut server, 2, Some(codec::OPUS));
    open_stream(&mut server, &public, "owner");
    for client_id in [1, 2] {
        assert!(!server.handle_server_event(ServerEvent::ClientMediaCredit {
            client_id,
            stream_id: 1,
            chunks: u16::MAX,
        }));
    }
    let mut heard = 0;
    for _ in 0..65 {
        feed_frame(&mut server, &public, "owner");
        heard += media_messages(&fluent)
            .iter()
            .filter(|(_, message)| matches!(message, ServerMessage::MediaChunk { .. }))
            .count();
    }
    assert_eq!(heard, 65, "the listener that keeps up hears every chunk");
    let counters = server
        .app
        .pane_audio
        .session_for_stream(1)
        .unwrap()
        .counters();
    assert_eq!(
        counters.dropped_full, 1,
        "the refusal is counted once, for the stuck lane"
    );
    assert_eq!(counters.sent, 64 + 65, "and nobody else lost a chunk to it");
    let items = stuck.drain();
    assert_eq!(
        items.len(),
        65,
        "the stuck lane holds its open and 64 chunks"
    );
}

// ---------------------------------------------------------------------------
// Listener admission — LS-1, LS-2, LS-4, LS-5, LS-6
//
// The capture gate used to ask for `is_full_app_client()`, which meant the only
// way to be offered audio was to also take foreground, resize the shared
// runtime and be rendered for. These pin the separation.
// ---------------------------------------------------------------------------

fn connect_media_listener(
    server: &mut HeadlessServer,
    client_id: u64,
    sink: Option<&str>,
) -> crate::server::client_transport::TestQueueDrain {
    let (writer, drain) = ClientWriter::test_channel_through_queue();
    assert!(server.handle_server_event(ServerEvent::ClientConnected {
        client_id,
        cols: 80,
        rows: 24,
        cell_width_px: 0,
        cell_height_px: 0,
        render_encoding: RenderEncoding::SemanticFrame,
        keybindings: None,
        direct_attach_requested: false,
        direct_graphics: false,
        media_listen: true,
        pixel_mouse: false,
        writer,
        capabilities: match sink {
            Some(codec) => vec![
                CapabilityEntry::flag(capability::MEDIA_STREAMS),
                CapabilityEntry::with_values(capability::AUDIO_SINK, [codec]),
            ],
            None => Vec::new(),
        },
    }));
    drain
}

fn connect_app_client(server: &mut HeadlessServer, client_id: u64) {
    let (writer, _drain) = ClientWriter::test_channel_through_queue();
    assert!(server.handle_server_event(ServerEvent::ClientConnected {
        client_id,
        cols: 120,
        rows: 40,
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
}

/// LS-1 — a media listener does not take foreground.
///
/// This is the user-visible half. Their terminal would silently stop being the
/// connection that owns the screen, and nothing anywhere would say why.
#[test]
fn a_media_listener_does_not_take_foreground_from_the_user() {
    let (mut server, _pane, _public) = audio_test_server();
    connect_app_client(&mut server, 1);
    assert_eq!(server.foreground_client_id, Some(1), "the app client leads");

    connect_media_listener(&mut server, 2, Some(codec::OPUS));

    assert_eq!(
        server.foreground_client_id,
        Some(1),
        "the listener took the screen the user was looking at"
    );
}

/// LS-2 — and it does not resize the session to its own window.
///
/// The quieter half, and the worse one: every pane reflows to whatever size the
/// listening process happened to report. A listener has no window to speak for.
#[test]
fn a_media_listener_does_not_resize_the_session() {
    let (mut server, _pane, _public) = audio_test_server();
    connect_app_client(&mut server, 1);
    let before = server.effective_size;

    connect_media_listener(&mut server, 2, Some(codec::OPUS));

    assert_eq!(
        server.effective_size, before,
        "the listener reflowed the user's panes to its own size"
    );
}

/// LS-2b — with no app client at all, it still claims nothing.
///
/// The empty case is where an accidental `foreground_client_id = Some(..)`
/// hides: there is nobody to take it from, so nothing looks wrong until
/// somebody real connects.
#[test]
fn a_media_listener_alone_is_still_not_the_foreground_client() {
    let (mut server, _pane, _public) = audio_test_server();
    connect_media_listener(&mut server, 1, Some(codec::OPUS));
    assert_eq!(server.foreground_client_id, None);
}

/// LS-2c — and it does not eat the handoff repaint nudge.
///
/// This is the part of the early return that is a correctness difference
/// rather than a cost. After a live update the server owes the next client to
/// attach one repaint; the flag that remembers this is consumed by whoever
/// attaches first. A listener consuming it would leave the real client with a
/// screen nobody redrew, and nothing would report why.
///
/// Recorded honestly: LS-2 above pins `effective_size`, but a mutation showed
/// it passes because the listener never becomes foreground — the resize path
/// early-returns on that — not because of the early return here. This is the
/// test that fails when the early return goes.
#[test]
fn a_media_listener_does_not_eat_the_handoff_repaint_nudge() {
    let (mut server, _pane, _public) = audio_test_server();
    server.pending_handoff_repaint_nudge = true;

    connect_media_listener(&mut server, 1, Some(codec::OPUS));

    assert!(
        server.pending_handoff_repaint_nudge,
        "the listener consumed the repaint the next real client is owed"
    );
}

/// LS-2d — an ordinary app client still takes it.
///
/// The other half: a guard that stopped everyone from consuming the nudge
/// would pass the test above and quietly break the thing the nudge exists for.
#[test]
fn an_app_client_still_takes_the_handoff_repaint_nudge() {
    let (mut server, _pane, _public) = audio_test_server();
    server.pending_handoff_repaint_nudge = true;

    connect_app_client(&mut server, 1);

    assert!(
        !server.pending_handoff_repaint_nudge,
        "the app client should have consumed the repaint it is owed"
    );
}

/// LS-4 — a media listener alone opens the capture gate.
///
/// The whole point. Without this the capture never starts for a headless
/// listener, and a remote ear is impossible however correct the egress is.
#[test]
fn a_media_listener_alone_counts_as_somebody_who_can_hear() {
    let (mut server, _pane, _public) = audio_test_server();
    connect_media_listener(&mut server, 1, Some(codec::OPUS));

    let (capable, declined) = server.audio_capable_clients();
    assert_eq!(capable, vec![1], "the listener is not counted");
    assert_eq!(declined, 0);
}

/// LS-5 — the capability decides, not the role.
///
/// Otherwise the new role becomes a second way to be sent audio without ever
/// saying it can play it — the failure the capability handshake exists to stop.
#[test]
fn a_media_listener_without_a_sink_is_declined_like_anyone_else() {
    let (mut server, _pane, _public) = audio_test_server();
    connect_media_listener(&mut server, 1, None);

    let (capable, declined) = server.audio_capable_clients();
    assert!(
        capable.is_empty(),
        "a listener with no sink was offered audio"
    );
    assert_eq!(declined, 1);
}

/// LS-6 — when the last listener goes, the capture goes with it.
///
/// Symmetry with the existing gate. An idle supervisor holds a watcher process
/// and a thread, and a cost nobody asked for is invisible until it is measured.
#[test]
fn the_last_media_listener_leaving_closes_the_gate() {
    let (mut server, _pane, _public) = audio_test_server();
    connect_media_listener(&mut server, 1, Some(codec::OPUS));
    assert_eq!(server.audio_capable_clients().0, vec![1]);

    server.remove_client_and_resize_if_needed(1);

    assert!(
        server.audio_capable_clients().0.is_empty(),
        "the gate stayed open after the only listener left"
    );
}

// ── TP-MEDIA-LATE-01 · attaching is an event, on the audio side too ─────────

/// Drive the production audio tick without touching this machine's audio
/// graph: a supervisor whose sources say "nothing here". `tick_pane_audio`
/// is what the server loop calls; the announce under test lives in it, so
/// the tests go through it rather than through a function of their own.
fn hermetic_audio_capture(server: &mut HeadlessServer) {
    server.pane_audio_capture = Some(
        crate::server::headless::pane_audio_capture::CaptureSupervisor::with_sources(
            Box::new(|_rate| None),
            Box::new(|| None),
        ),
    );
}

/// Every media message this client received, with its lane.
fn media_messages(
    drain: &crate::server::client_transport::TestQueueDrain,
) -> Vec<(WriteLane, ServerMessage)> {
    drain
        .drain()
        .into_iter()
        .map(|(lane, bytes)| (lane, decode(&bytes)))
        .filter(|(_, message)| {
            matches!(
                message,
                ServerMessage::MediaOpen { .. }
                    | ServerMessage::MediaChunk { .. }
                    | ServerMessage::MediaClose { .. }
            )
        })
        .collect()
}

fn audio_open_of(seen: &[(WriteLane, ServerMessage)]) -> Option<(WriteLane, u32, String)> {
    seen.iter().find_map(|(lane, message)| match message {
        ServerMessage::MediaOpen {
            stream_id,
            pane_id,
            codec,
            ..
        } if codec == codec::OPUS => Some((*lane, *stream_id, pane_id.clone())),
        _ => None,
    })
}

fn close_stream(server: &mut HeadlessServer, public: &str, owner: &str) {
    let response = server.app.handle_api_request(api::schema::Request {
        id: "close".into(),
        method: api::schema::Method::PaneAudioStreamClose(
            api::schema::PaneAudioStreamCloseParams {
                pane_id: public.to_owned(),
                owner: owner.to_owned(),
                failed: false,
                detail: String::new(),
            },
        ),
    });
    assert!(
        serde_json::from_str::<api::schema::SuccessResponse>(&response).is_ok(),
        "{response}"
    );
    server.flush_pane_audio_outbound();
}

/// TP-MEDIA-LATE-01 · ⭐⭐ a listener that arrives while a stream is open is told about it.
///
/// The video side's K1, on the audio side: `fan_out_media_open` subscribed
/// whoever could hear at `Outbound::Open` and nobody afterwards. A second
/// listener was counted as somebody who can hear — the capture kept running
/// for it — and was never subscribed, so `clients_with_room` never named it
/// and it never received a chunk.
#[test]
fn a_listener_that_arrives_while_an_audio_stream_is_open_is_told_about_it() {
    let (mut server, _pane, public) = audio_test_server();
    hermetic_audio_capture(&mut server);
    let first = connect(&mut server, 1, Some(codec::OPUS));
    open_stream(&mut server, &public, "tb");
    let (_, stream_id, _) =
        audio_open_of(&media_messages(&first)).expect("the first listener is told the open");

    let late = connect(&mut server, 2, Some(codec::OPUS));
    server.tick_pane_audio();

    let seen = media_messages(&late);
    match audio_open_of(&seen) {
        Some((lane, told, pane)) => {
            assert_eq!(told, stream_id, "the stream already running, not a new one");
            assert_eq!(pane, public);
            assert_eq!(
                lane,
                WriteLane::Control,
                "an open a listener misses leaves it deaf"
            );
        }
        None => panic!("the late listener was never told the stream is open: {seen:?}"),
    }

    // Told, it reports room — and then it is sent sound like anybody else.
    server.handle_server_event(ServerEvent::ClientMediaCredit {
        client_id: 2,
        stream_id,
        chunks: 2,
    });
    feed_frame(&mut server, &public, "tb");
    let seen = media_messages(&late);
    assert!(
        seen.iter().any(|(lane, message)| {
            matches!(message, ServerMessage::MediaChunk { .. }) && *lane == WriteLane::Media
        }),
        "and its chunks follow on the media lane: {seen:?}"
    );
}

/// TP-MEDIA-LATE-01 · the first listener is not told again when a second arrives.
#[test]
fn the_first_listener_is_not_told_twice_when_a_second_arrives() {
    let (mut server, _pane, public) = audio_test_server();
    hermetic_audio_capture(&mut server);
    let first = connect(&mut server, 1, Some(codec::OPUS));
    open_stream(&mut server, &public, "tb");
    assert!(audio_open_of(&media_messages(&first)).is_some());

    let _late = connect(&mut server, 2, Some(codec::OPUS));
    server.tick_pane_audio();
    server.tick_pane_audio();

    let repeated = media_messages(&first)
        .iter()
        .filter(|(_, message)| matches!(message, ServerMessage::MediaOpen { .. }))
        .count();
    assert_eq!(
        repeated, 0,
        "told once, before the second arrived, and never again"
    );
}

/// TP-MEDIA-LATE-01 · a listener that leaves and returns is told again.
///
/// `forget_client` already drops a departed client's room; the announce must
/// read the same record, so the same id arriving again is a new listener.
#[tokio::test]
async fn a_listener_that_leaves_and_returns_is_told_again() {
    let (mut server, _pane, public) = audio_test_server();
    hermetic_audio_capture(&mut server);
    let first = connect(&mut server, 1, Some(codec::OPUS));
    let _keeper = connect(&mut server, 2, Some(codec::OPUS));
    open_stream(&mut server, &public, "tb");
    let (_, stream_id, _) = audio_open_of(&media_messages(&first)).expect("told");
    drop(first);

    server.handle_server_event(ServerEvent::ClientDisconnected { client_id: 1 });
    assert!(!server.clients.contains_key(&1));
    server.tick_pane_audio();

    let returned = connect(&mut server, 1, Some(codec::OPUS));
    server.tick_pane_audio();

    let seen = media_messages(&returned);
    assert_eq!(
        audio_open_of(&seen).map(|(_, id, _)| id),
        Some(stream_id),
        "the returning listener is told about the stream that kept running: {seen:?}"
    );
}

/// TP-MEDIA-LATE-01 · a listener that arrives after the stream closed is told nothing.
#[test]
fn a_listener_that_arrives_after_an_audio_stream_closed_is_told_nothing() {
    let (mut server, _pane, public) = audio_test_server();
    hermetic_audio_capture(&mut server);
    let first = connect(&mut server, 1, Some(codec::OPUS));
    open_stream(&mut server, &public, "tb");
    assert!(audio_open_of(&media_messages(&first)).is_some());
    close_stream(&mut server, &public, "tb");
    assert!(
        media_messages(&first)
            .iter()
            .any(|(_, message)| matches!(message, ServerMessage::MediaClose { .. })),
        "the first listener saw the close — the precondition this test rests on"
    );

    let late = connect(&mut server, 2, Some(codec::OPUS));
    server.tick_pane_audio();
    server.tick_pane_audio();
    let seen = media_messages(&late);
    assert!(
        seen.is_empty(),
        "nothing is open, so nothing is announced: {seen:?}"
    );
}

/// TP-MEDIA-LATE-01 · ⭐ REACHABILITY — the late listener arrives through the
/// production connect event, and is told by the production tick.
#[tokio::test]
async fn a_late_listener_is_told_through_the_production_connect_path() {
    let (mut server, _pane, public) = audio_test_server();
    hermetic_audio_capture(&mut server);
    let first = connect(&mut server, 1, Some(codec::OPUS));
    open_stream(&mut server, &public, "tb");
    let (_, stream_id, _) = audio_open_of(&media_messages(&first)).expect("told");

    let late = connect_media_listener(&mut server, 2, Some(codec::OPUS));
    server.tick_pane_audio();

    let seen = media_messages(&late);
    assert_eq!(
        audio_open_of(&seen).map(|(_, id, pane)| (id, pane)),
        Some((stream_id, public.clone())),
        "the newcomer that arrived through the real connect path is told: {seen:?}"
    );
}

/// TP-MEDIA-FLOW-01 · TP-MEDIA-FLOW-03 · TP-MEDIA-FLOW-04 — the flow of a
/// captured stream once more than one listener is on it, and what the stream
/// does when its recorder stops.
///
/// The recorder here is fed by hand, frame by frame, so the test decides when
/// a frame exists and when the loop looks; the graph watcher never speaks and
/// never ends. Nothing in these tests reaches a sound server.
mod flow {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::mpsc::{channel, Receiver, Sender};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use crate::app::pane_audio::FRAME_BYTES;
    use crate::platform::{AudioSourceError, FrameSource, GraphSignals};
    use crate::server::client_transport::TestQueueDrain;
    use crate::server::headless::pane_audio::CAPTURE_OWNER;
    use crate::server::headless::pane_audio_capture::{CaptureSupervisor, FRAME_QUEUE};

    /// A recorder that produces a frame when the test sends one and ends when
    /// the test drops the sender.
    struct FedSource {
        frames: Receiver<Vec<u8>>,
    }

    impl FrameSource for FedSource {
        fn next_frame(&mut self) -> Result<Option<Vec<u8>>, AudioSourceError> {
            Ok(self.frames.recv().ok())
        }

        fn close(&mut self) -> Result<(), AudioSourceError> {
            Ok(())
        }
    }

    /// A graph watcher that stays quiet until the test lets go of it.
    struct QuietWatcher {
        held: Receiver<()>,
    }

    impl GraphSignals for QuietWatcher {
        fn next_signal(&mut self) -> Result<bool, AudioSourceError> {
            let _ = self.held.recv();
            Ok(false)
        }

        fn close(&mut self) -> Result<(), AudioSourceError> {
            Ok(())
        }
    }

    /// What the test holds to drive the capture: one frame sender per source
    /// it may start (in order), and the watcher's leash.
    struct FedCapture {
        sources: Vec<Sender<Vec<u8>>>,
        _leash: Sender<()>,
    }

    /// Installs a supervisor whose recorders are the test's own, `sources` of
    /// them, handed out in the order the server starts captures.
    fn fed_capture(server: &mut HeadlessServer, sources: usize) -> FedCapture {
        let mut senders = Vec::new();
        let mut receivers = VecDeque::new();
        for _ in 0..sources {
            let (tx, rx) = channel::<Vec<u8>>();
            senders.push(tx);
            receivers.push_back(rx);
        }
        let (leash, held) = channel::<()>();
        let receivers = Mutex::new(receivers);
        let held = Mutex::new(Some(held));
        server.pane_audio_capture = Some(CaptureSupervisor::with_sources(
            Box::new(move |_serial| {
                receivers
                    .lock()
                    .unwrap()
                    .pop_front()
                    .map(|frames| Ok(Box::new(FedSource { frames }) as Box<dyn FrameSource>))
            }),
            Box::new(move || {
                held.lock()
                    .unwrap()
                    .take()
                    .map(|held| Ok(Box::new(QuietWatcher { held }) as Box<dyn GraphSignals>))
            }),
        ));
        FedCapture {
            sources: senders,
            _leash: leash,
        }
    }

    fn frame() -> Vec<u8> {
        0.05_f32.to_le_bytes().repeat(FRAME_BYTES / 4)
    }

    fn start_capture(server: &mut HeadlessServer, public: &str, serial: u32) {
        server
            .pane_audio_capture
            .as_mut()
            .expect("a capture is installed")
            .start(public, serial)
            .expect("the fed recorder starts");
    }

    /// Ticks the audio arm and collects what `drain` received until `done`
    /// says so, or two seconds pass. The frames cross a thread, so a single
    /// tick proves nothing either way; the count decides (E49).
    fn pump_until(
        server: &mut HeadlessServer,
        drain: &TestQueueDrain,
        seen: &mut Vec<(WriteLane, ServerMessage)>,
        mut done: impl FnMut(&[(WriteLane, ServerMessage)]) -> bool,
    ) -> bool {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            server.tick_pane_audio();
            seen.extend(media_messages(drain));
            if done(seen) {
                return true;
            }
            if Instant::now() > deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn chunks(seen: &[(WriteLane, ServerMessage)]) -> Vec<(u64, u64)> {
        seen.iter()
            .filter_map(|(_, message)| match message {
                ServerMessage::MediaChunk { seq, pts_us, .. } => Some((*seq, *pts_us)),
                _ => None,
            })
            .collect()
    }

    fn closes(seen: &[(WriteLane, ServerMessage)]) -> Vec<(WriteLane, u32)> {
        seen.iter()
            .filter_map(|(lane, message)| match message {
                ServerMessage::MediaClose { stream_id, .. } => Some((*lane, *stream_id)),
                _ => None,
            })
            .collect()
    }

    /// Feeds the first frame and waits for the stream it opens to be offered.
    fn open_by_capture(
        server: &mut HeadlessServer,
        capture: &FedCapture,
        source: usize,
        listener: &TestQueueDrain,
        seen: &mut Vec<(WriteLane, ServerMessage)>,
    ) -> u32 {
        capture.sources[source].send(frame()).unwrap();
        assert!(
            pump_until(server, listener, seen, |seen| audio_open_of(seen).is_some()),
            "the first captured frame opens a stream and the listener is told: {seen:?}"
        );
        audio_open_of(seen).map(|(_, id, _)| id).unwrap()
    }

    /// TP-MEDIA-FLOW-02 · the supervisor the server ships is wired to the
    /// server's own wake, the one its loop selects on.
    #[test]
    fn the_servers_capture_wakes_the_servers_loop() {
        let (server, _pane, _public) = audio_test_server();
        let capture = server.new_pane_audio_capture();
        assert!(
            capture.wakes(&server.pane_audio_wake),
            "a supervisor wired to some other handle wakes nobody"
        );
    }

    /// TP-MEDIA-FLOW-02 · a captured frame signals the server's wake, so
    /// delivery follows the frame rather than the timer.
    #[tokio::test]
    async fn a_captured_frame_signals_the_server_within_a_frame() {
        let (mut server, _pane, public) = audio_test_server();
        let capture = fed_capture(&mut server, 1);
        server.pane_audio_capture = server
            .pane_audio_capture
            .take()
            .map(|capture| capture.wake_on(Arc::clone(&server.pane_audio_wake)));
        let _listener = connect(&mut server, 1, Some(codec::OPUS));
        start_capture(&mut server, &public, 7);
        capture.sources[0].send(frame()).unwrap();
        let wake = Arc::clone(&server.pane_audio_wake);
        // Nothing but the recorder can signal this handle here, so the bound
        // only has to outlast a loaded build box; without the wiring it never
        // fires at all.
        tokio::time::timeout(Duration::from_secs(2), wake.notified())
            .await
            .expect("the frame woke the server; nothing else could have");
    }

    /// TP-MEDIA-FLOW-01 · a frame the loop never received does not move the
    /// stream's clock.
    ///
    /// The recorder produces a hundred frames while the loop is away. The
    /// window keeps its worth of the NEWEST and pushes the rest out, counted
    /// (TP-MEDIA-FLOW-09). What the loop then takes must carry the
    /// recorder's own numbering, not a count of what the loop happened to
    /// receive: the first chunk after the overflow is the tail's own frame,
    /// so the hole shows as a `seq` gap the client can see, and the fresh
    /// frame after it lands at 101. Under a clock that counted offers
    /// instead, every one of these would present in the past and be dropped.
    #[tokio::test]
    async fn a_frame_the_loop_missed_does_not_move_the_streams_clock() {
        let (mut server, _pane, public) = audio_test_server();
        let capture = fed_capture(&mut server, 1);
        let listener = connect(&mut server, 1, Some(codec::OPUS));
        start_capture(&mut server, &public, 7);
        let mut seen = Vec::new();
        let stream_id = open_by_capture(&mut server, &capture, 0, &listener, &mut seen);
        server.handle_server_event(ServerEvent::ClientMediaCredit {
            client_id: 1,
            stream_id,
            chunks: 50,
        });

        // The loop is away: a hundred frames, the queue's worth kept, the
        // rest dropped at the source.
        let lost = 100 - FRAME_QUEUE as u64;
        for _ in 0..100 {
            capture.sources[0].send(frame()).unwrap();
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let dropped = server.pane_audio_capture.as_ref().unwrap().dropped_frames();
            if dropped == lost {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "the recorder should have dropped {lost} frames at the queue, dropped {dropped}"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
        server.tick_pane_audio();
        seen.extend(media_messages(&listener));
        let after_overflow = chunks(&seen);
        assert!(
            !after_overflow.is_empty(),
            "the window's survivors are the freshest frames and are sent, not dropped as late"
        );
        assert_eq!(
            after_overflow[0].0,
            lost + 1,
            "the first chunk after the overflow carries the recorder's own number, so the hole is visible: {after_overflow:?}"
        );

        // The next frame is fresh. Its moment is its own, not the counter's.
        // Waited for by growth, not by emptiness: the survivors above are
        // already in `seen`, so "any chunk" would be true before this frame
        // ever arrived.
        let before = after_overflow.len();
        capture.sources[0].send(frame()).unwrap();
        assert!(
            pump_until(&mut server, &listener, &mut seen, |seen| chunks(seen).len()
                > before),
            "a fresh frame after an overflow is sent; the clock moved with the recorder: {seen:?}"
        );
        let sent = chunks(&seen);
        assert_eq!(
            sent.last().expect("a chunk").0,
            101,
            "the last chunk's seq is the recorder's offset (the opening frame was 0): {sent:?}"
        );
    }

    /// TP-MEDIA-FLOW-03 · stopping a captured pane ends its stream, and the
    /// next capture of that pane opens a new one the listeners are offered.
    #[tokio::test]
    async fn stopping_a_captured_pane_closes_its_stream_and_the_next_capture_opens_a_new_one() {
        let (mut server, _pane, public) = audio_test_server();
        let capture = fed_capture(&mut server, 2);
        let listener = connect(&mut server, 1, Some(codec::OPUS));
        start_capture(&mut server, &public, 7);
        let mut seen = Vec::new();
        let first = open_by_capture(&mut server, &capture, 0, &listener, &mut seen);

        // The graph says the source is gone: the plan stops the pane.
        server.pane_audio_capture.as_mut().unwrap().stop(&public);
        assert!(
            pump_until(&mut server, &listener, &mut seen, |seen| !closes(seen)
                .is_empty()),
            "a stopped pane's stream closes and the listener is told: {seen:?}"
        );
        assert_eq!(
            closes(&seen),
            vec![(WriteLane::Control, first)],
            "the close names the stream that ended, on the control lane"
        );
        assert!(
            server.app.pane_audio.session_for_stream(first).is_none(),
            "the session is gone with its stream"
        );

        // The pane makes sound again: a new stream, offered to the same listener.
        seen.clear();
        start_capture(&mut server, &public, 8);
        let second = open_by_capture(&mut server, &capture, 1, &listener, &mut seen);
        assert_ne!(second, first, "a new stream, not the stopped one reopened");
    }

    /// TP-MEDIA-FLOW-04 · a listener that joins does not interrupt the one
    /// already hearing. Fed through the API path, so nothing here waits on a
    /// thread: every count is exact.
    #[test]
    fn a_listener_that_joins_does_not_interrupt_the_one_already_hearing() {
        let (mut server, _pane, public) = audio_test_server();
        hermetic_audio_capture(&mut server);
        let first = connect(&mut server, 1, Some(codec::OPUS));
        open_stream(&mut server, &public, "tb");
        let (_, stream_id, _) = audio_open_of(&media_messages(&first)).expect("told");
        server.handle_server_event(ServerEvent::ClientMediaCredit {
            client_id: 1,
            stream_id,
            chunks: 50,
        });
        feed_frame(&mut server, &public, "tb");
        feed_frame(&mut server, &public, "tb");
        assert_eq!(
            chunks(&media_messages(&first)).len(),
            2,
            "hearing before anyone joins"
        );

        let joiner = connect(&mut server, 2, Some(codec::OPUS));
        server.tick_pane_audio();
        let (_, told, _) = audio_open_of(&media_messages(&joiner)).expect("the joiner is told");
        assert_eq!(told, stream_id);
        server.handle_server_event(ServerEvent::ClientMediaCredit {
            client_id: 2,
            stream_id,
            chunks: 50,
        });
        feed_frame(&mut server, &public, "tb");
        feed_frame(&mut server, &public, "tb");
        assert_eq!(
            chunks(&media_messages(&first)).len(),
            2,
            "the first listener keeps hearing after the joiner reports room"
        );
        assert_eq!(
            chunks(&media_messages(&joiner)).len(),
            2,
            "and the joiner hears the same frames"
        );
    }

    /// TP-MEDIA-FLOW-12 · the last listener leaving closes the stream the
    /// capture opened, and the next capture is a new stream.
    ///
    /// Measured in a clean room (E-166): on one server process the first
    /// capture played and every later one was silent — the recorder at a
    /// full 384 kB/s, the queue never overflowing, the listener handed
    /// nothing but the open. Tearing the supervisor down queued an `Ended`
    /// for the pane and then dropped the supervisor with it, so the runtime
    /// kept a session that said it was live. The next capture met it as a
    /// conflict, its frames were offered to a clock anchored at the old
    /// open, and every one was late. A live server that had run for days
    /// was in this state for every listener after its first.
    #[tokio::test]
    async fn the_last_listener_leaving_closes_the_captures_stream() {
        let (mut server, _pane, public) = audio_test_server();
        let capture = fed_capture(&mut server, 1);
        let listener = connect(&mut server, 1, Some(codec::OPUS));
        start_capture(&mut server, &public, 7);
        let mut seen = Vec::new();
        let first = open_by_capture(&mut server, &capture, 0, &listener, &mut seen);
        assert!(
            server.app.pane_audio.owner_is_active(CAPTURE_OWNER),
            "the capture holds the pane's stream while it records"
        );

        // The only listener leaves; the next tick tears the capture down.
        server.remove_client_and_resize_if_needed(1);
        server.tick_pane_audio();
        assert!(
            server.pane_audio_capture.is_none(),
            "with nobody to hear it, the supervisor is gone"
        );
        assert!(
            !server.app.pane_audio.owner_is_active(CAPTURE_OWNER),
            "what the server opened, the server closed"
        );

        // A new listener and a new capture: a new stream opened now, not the
        // old one announced again.
        let capture = fed_capture(&mut server, 1);
        let joiner = connect(&mut server, 2, Some(codec::OPUS));
        start_capture(&mut server, &public, 8);
        let mut seen = Vec::new();
        let second = open_by_capture(&mut server, &capture, 0, &joiner, &mut seen);
        assert_ne!(second, first, "a new capture is a new stream: {seen:?}");
        server.handle_server_event(ServerEvent::ClientMediaCredit {
            client_id: 2,
            stream_id: second,
            chunks: 50,
        });
        capture.sources[0].send(frame()).unwrap();
        assert!(
            pump_until(&mut server, &joiner, &mut seen, |seen| !chunks(seen)
                .is_empty()),
            "the joiner hears the new capture: {seen:?}"
        );
    }

    /// TP-MEDIA-FLOW-13 · a capture that meets its own stale stream on the
    /// pane supersedes it rather than falling silent behind it.
    ///
    /// The teardown above is the one known way to leave such a session
    /// behind; this pins the behaviour against any other. A same-owner
    /// re-open is a restart, never a conflict: the old stream is closed for
    /// its subscribers and the new one opens at this moment.
    #[tokio::test]
    async fn a_capture_supersedes_its_own_stale_stream() {
        let (mut server, pane, public) = audio_test_server();
        // The leak, staged: a capture-owned session nobody closed, anchored
        // long ago.
        let stale = server
            .app
            .pane_audio
            .open(pane, &public, CAPTURE_OWNER, 1_000)
            .expect("the stale stream opens");
        server.flush_pane_audio_outbound();
        let capture = fed_capture(&mut server, 1);
        let listener = connect(&mut server, 1, Some(codec::OPUS));
        start_capture(&mut server, &public, 7);
        let mut seen = Vec::new();
        capture.sources[0].send(frame()).unwrap();
        let fresh_open = |seen: &[(WriteLane, ServerMessage)]| {
            seen.iter().find_map(|(_, message)| match message {
                ServerMessage::MediaOpen { stream_id, .. } if *stream_id != stale => {
                    Some(*stream_id)
                }
                _ => None,
            })
        };
        assert!(
            pump_until(&mut server, &listener, &mut seen, |seen| fresh_open(seen)
                .is_some()),
            "the capture opens a fresh stream over its stale one: {seen:?}"
        );
        let fresh = fresh_open(&seen).expect("a fresh stream");
        assert!(
            closes(&seen).iter().any(|(_, id)| *id == stale),
            "the stale stream is closed for whoever was offered it: {seen:?}"
        );
        server.handle_server_event(ServerEvent::ClientMediaCredit {
            client_id: 1,
            stream_id: fresh,
            chunks: 50,
        });
        capture.sources[0].send(frame()).unwrap();
        assert!(
            pump_until(&mut server, &listener, &mut seen, |seen| !chunks(seen)
                .is_empty()),
            "the listener hears the fresh stream, not silence behind the stale one: {seen:?}"
        );
    }
}
