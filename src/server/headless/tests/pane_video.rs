//! TP-VIDEO-TAP-01 and TP-VIDEO-TAP-02 for the pane video tap.
//!
//! Test points were written before the code, and
//! `.local/prd/2026-09-01-pane-video-h264-PRD.md` carries the reason each one
//! exists — the part a test name cannot hold.

use super::*;
use crate::app::pane_graphics::{Layer, Slot};
use crate::protocol::{capability, codec, CapabilityEntry, CapabilitySet};
use crate::server::client_transport::WriteLane;
use crate::server::headless::mpegts::test_stream::{access_unit, pes, program};
use crate::server::headless::pane_video_capture::VideoEvent;

/// An encoder that swallows pictures and never speaks.
///
/// Enough for every test here: what is under test is *when* an encoder exists
/// and what it is fed, never what ffmpeg does with it. The real spawn would
/// make each of these assertions about the machine instead.
fn silent_spawn() -> crate::server::headless::pane_video_capture::EncoderProcess {
    struct Sink;
    impl std::io::Write for Sink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    struct Silent;
    impl std::io::Read for Silent {
        fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
            // Parks the reader thread instead of reporting end-of-stream, which
            // would end the encoder and make every test about ffmpeg's absence.
            std::thread::sleep(std::time::Duration::from_secs(3600));
            Ok(0)
        }
    }
    crate::server::headless::pane_video_capture::EncoderProcess {
        child: None,
        stdin: Box::new(Sink),
        stdout: Box::new(Silent),
    }
}

fn video_test_server() -> (HeadlessServer, crate::layout::PaneId, String) {
    let mut server = test_headless_server();
    server
        .pane_video
        .set_spawn_for_test(Box::new(|_w, _h, _f| Ok(silent_spawn())));
    let workspace = crate::workspace::Workspace::test_new("video");
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

fn connect_viewer(
    server: &mut HeadlessServer,
    client_id: u64,
    wants_video: bool,
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
    client.capabilities = CapabilitySet::from_entries(if wants_video {
        vec![CapabilityEntry::with_values(
            capability::VIDEO_DECODE,
            [codec::H264],
        )]
    } else {
        Vec::new()
    });
    server.clients.insert(client_id, client);
    drain
}

/// Put one inline RGBA layer on a pane, through the runtime `pane.graphics`
/// uses.
///
/// `insert_slot`, never `slots.insert`: a slot written straight into the map
/// is drawn and never recorded, so a fixture that did that would test a path
/// production does not take and would agree the feed works while it does not.
fn set_inline_layer_at(
    server: &mut HeadlessServer,
    pane_id: crate::layout::PaneId,
    fill: u8,
    pts_us: u64,
) {
    let layer = Layer::inline(
        crate::api::schema::PaneGraphicsFormat::Rgba,
        4,
        4,
        vec![fill; 64],
        Default::default(),
        0,
    );
    server.app.pane_graphics.insert_slot_at(
        (pane_id, "primary".to_owned()),
        Slot::test(1, Some(layer)),
        pts_us,
    );
}

fn set_inline_layer(
    server: &mut HeadlessServer,
    pane_id: crate::layout::PaneId,
    width: u32,
    height: u32,
    fill: u8,
) {
    let data = vec![fill; (width * height * 4) as usize];
    let layer = Layer::inline(
        crate::api::schema::PaneGraphicsFormat::Rgba,
        width,
        height,
        data,
        Default::default(),
        0,
    );
    server
        .app
        .pane_graphics
        .insert_slot((pane_id, "primary".to_owned()), Slot::test(1, Some(layer)));
}

/// An encoder that keeps every picture written to it, so a test can count
/// what actually arrived rather than what a counter says arrived.
fn recording_spawn(
    log: std::sync::Arc<std::sync::Mutex<Vec<Vec<u8>>>>,
) -> crate::server::headless::pane_video_capture::EncoderProcess {
    struct Recorder(std::sync::Arc<std::sync::Mutex<Vec<Vec<u8>>>>);
    impl std::io::Write for Recorder {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("record").push(buf.to_vec());
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    struct Silent;
    impl std::io::Read for Silent {
        fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
            std::thread::sleep(std::time::Duration::from_secs(3600));
            Ok(0)
        }
    }
    crate::server::headless::pane_video_capture::EncoderProcess {
        child: None,
        stdin: Box::new(Recorder(log)),
        stdout: Box::new(Silent),
    }
}

/// Pictures the encoder was handed, in arrival order, identified by the fill
/// byte the fixture painted them with.
fn recorded_fills(
    log: &std::sync::Arc<std::sync::Mutex<Vec<Vec<u8>>>>,
    frame_bytes: usize,
    expected: usize,
) -> Vec<u8> {
    // The write goes off to a thread, so the test waits for it rather than
    // assuming the scheduler was prompt. It waits for the number of pictures
    // the caller expects, not merely for "something whole": an earlier
    // version returned as soon as the byte count was a multiple of one
    // frame, which is true after the FIRST picture, so the four-picture test
    // passed only when the scheduler had happened to write all four before
    // the first poll. It failed on the landing gate under load, having passed
    // the same gate minutes earlier on the same code.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let snapshot = || -> Vec<u8> {
        log.lock()
            .expect("record")
            .iter()
            .flat_map(|chunk| chunk.iter().copied())
            .collect()
    };
    let mut bytes = snapshot();
    while bytes.len() < expected * frame_bytes && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(5));
        bytes = snapshot();
    }
    // Then a settle window, so a picture that should NOT have arrived still
    // can: an "offered once" assertion that returned at the first frame
    // would never see the second one it exists to forbid.
    std::thread::sleep(std::time::Duration::from_millis(150));
    bytes = snapshot();
    bytes.chunks(frame_bytes).map(|frame| frame[0]).collect()
}

fn opens(
    drain: &crate::server::client_transport::TestQueueDrain,
) -> Vec<(WriteLane, ServerMessage)> {
    drain
        .drain()
        .into_iter()
        .map(|(lane, bytes)| {
            let mut cursor = std::io::Cursor::new(bytes.as_slice());
            let message = protocol::read_message(&mut cursor, crate::protocol::MAX_FRAME_SIZE)
                .expect("framed server message");
            (lane, message)
        })
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

/// TP-VIDEO-TAP-01 · nothing runs while nobody can decode.
///
/// An encoder holds a GPU session and a core. Starting one for an empty
/// desk is the resource doctrine's plainest violation and its most silent:
/// a running encoder nobody watches looks exactly like one somebody does.
#[test]
fn no_encoder_starts_while_no_client_can_decode_video() {
    let (mut server, pane_id, _public) = video_test_server();
    let _drain = connect_viewer(&mut server, 1, false);
    set_inline_layer(&mut server, pane_id, 8, 8, 0x11);

    server.tick_pane_video();

    assert_eq!(
        server.pane_video.encoding_count(),
        0,
        "a client that cannot decode h264 must not cause encoding"
    );
}

/// TP-VIDEO-TAP-01 · a viewer plus a picture is an encoder.
#[test]
fn a_video_capable_client_starts_the_pane_encoder() {
    let (mut server, pane_id, public) = video_test_server();
    let _drain = connect_viewer(&mut server, 1, true);
    set_inline_layer(&mut server, pane_id, 8, 8, 0x22);

    server.tick_pane_video();

    assert!(
        server.pane_video.is_encoding(&public),
        "the pane with a picture and a viewer is being encoded"
    );
}

/// TP-VIDEO-TAP-02 · the same picture is not encoded twice.
///
/// `data_fingerprint` is what makes this cheap: an unchanged layer is
/// recognised without comparing or copying its bytes.
#[test]
fn an_unchanged_picture_is_offered_once() {
    let (mut server, pane_id, _public) = video_test_server();
    let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let held = log.clone();
    server
        .pane_video
        .set_spawn_for_test(Box::new(move |_w, _h, _f| {
            Ok(recording_spawn(held.clone()))
        }));
    let _drain = connect_viewer(&mut server, 1, true);

    // Written three times with the same bytes, across three ticks: the
    // repeats are what is under test, and the ticks make sure a drained queue
    // does not forget what it last kept.
    server.tick_pane_video();
    for _ in 0..3 {
        set_inline_layer(&mut server, pane_id, 8, 8, 0x33);
        server.tick_pane_video();
    }

    assert_eq!(
        recorded_fills(&log, 8 * 8 * 4, 1),
        vec![0x33],
        "three writes of one picture reach the encoder once, not three times"
    );
}

/// TP-VIDEO-TAP-01 · the last viewer leaving stops everything.
#[test]
fn the_last_viewer_leaving_stops_the_encoders() {
    let (mut server, pane_id, public) = video_test_server();
    let _drain = connect_viewer(&mut server, 1, true);
    set_inline_layer(&mut server, pane_id, 8, 8, 0x44);
    server.tick_pane_video();
    assert!(server.pane_video.is_encoding(&public));

    server.clients.remove(&1);
    server.tick_pane_video();

    assert_eq!(
        server.pane_video.encoding_count(),
        0,
        "no viewer, no encoder — and nothing waits for a timeout to notice"
    );
    assert!(server.pane_video_streams.is_empty());
    assert_eq!(
        server.app.pane_graphics.video_queued_frames(),
        0,
        "and nothing is still being kept for a viewer that left"
    );
}

/// TP-VIDEO-TAP-01 · a layer with no inline bytes is skipped, deliberately.
///
/// The gate is the backing, not the distance. `Direct` and `Resident` mean the
/// client reads the picture without the wire paying for it, so encoding them
/// would add a lossy round trip and the encoder's pipeline delay to save
/// nothing. `Inline` means the expensive path is in use — which happens over
/// ssh, and equally on a purely local session the moment a second app client
/// makes direct graphics unavailable.
///
/// This test exists so the skip stays a decision rather than becoming a bug
/// somebody fixes.
#[test]
fn a_layer_without_inline_bytes_is_not_encoded_because_the_wire_is_not_paying() {
    let (mut server, pane_id, public) = video_test_server();
    let _drain = connect_viewer(&mut server, 1, true);
    // A slot with no inline layer stands for the direct case here: the
    // encoder is driven by `Backing::Inline` and nothing else.
    server
        .app
        .pane_graphics
        .slots
        .insert((pane_id, "primary".to_owned()), Slot::test(1, None));

    server.tick_pane_video();

    assert!(
        !server.pane_video.is_encoding(&public),
        "only inline layers feed the encoder"
    );
}

/// TP-VIDEO-TAP-01 · ⭐⭐ THE REACHABILITY TEST — the one the dead-code allow promised.
///
/// `pane_video_capture` shipped behind `#![allow(dead_code)]` with a note
/// calling that allow a lie until this test existed, because twice on
/// 2026-09-01 a feature landed correct, tested, mutated and green while
/// production never reached it (E28, E35). So this drives the tick the server
/// loop actually calls, and reads the wire.
#[tokio::test]
async fn a_video_listener_reaches_the_encoder_through_the_production_path() {
    let (mut server, pane_id, public) = video_test_server();
    // An encoder that actually emits, so there is something to find on the wire.
    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    let held = std::sync::Mutex::new(Some(rx));
    server
        .pane_video
        .set_spawn_for_test(Box::new(move |_w, _h, _f| {
            struct Sink;
            impl std::io::Write for Sink {
                fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                    Ok(buf.len())
                }
                fn flush(&mut self) -> std::io::Result<()> {
                    Ok(())
                }
            }
            struct Scripted {
                rx: std::sync::mpsc::Receiver<Vec<u8>>,
                left: Vec<u8>,
            }
            impl std::io::Read for Scripted {
                fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                    if self.left.is_empty() {
                        match self.rx.recv() {
                            Ok(next) if next.is_empty() => return Ok(0),
                            Ok(next) => self.left = next,
                            Err(_) => return Ok(0),
                        }
                    }
                    let n = self.left.len().min(buf.len());
                    buf[..n].copy_from_slice(&self.left[..n]);
                    self.left.drain(..n);
                    Ok(n)
                }
            }
            let rx = held
                .lock()
                .expect("spawn seam")
                .take()
                .ok_or("spawned twice")?;
            Ok(
                crate::server::headless::pane_video_capture::EncoderProcess {
                    child: None,
                    stdin: Box::new(Sink),
                    stdout: Box::new(Scripted {
                        rx,
                        left: Vec::new(),
                    }),
                },
            )
        }));
    let drain = connect_viewer(&mut server, 1, true);
    set_inline_layer(&mut server, pane_id, 8, 8, 0x55);

    server.tick_pane_video();
    tx.send(program(0x100, &[access_unit(true, b"fake-annex-b")]))
        .expect("script");

    // The encoder reads on a thread; the tick that publishes is a later one,
    // exactly as it is in the loop.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut seen = Vec::new();
    while seen.is_empty() && std::time::Instant::now() < deadline {
        server.tick_pane_video();
        seen = opens(&drain);
        if seen.is_empty() {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    let open = seen
        .iter()
        .find(|(_, message)| matches!(message, ServerMessage::MediaOpen { .. }));
    match open {
        Some((
            lane,
            ServerMessage::MediaOpen {
                pane_id,
                codec,
                params,
                ..
            },
        )) => {
            assert_eq!(pane_id, &public, "the stream names the pane it came from");
            assert_eq!(codec, crate::protocol::codec::H264);
            assert_eq!(
                params,
                &crate::protocol::MediaParams::Video {
                    width: 8,
                    height: 8,
                    fps: 30
                }
            );
            assert_eq!(
                *lane,
                WriteLane::Control,
                "an open is not droppable; a viewer that missed it can route nothing"
            );
        }
        _ => panic!(
            "no MediaOpen reached the wire — the encoder is unreachable from the \
             production tick, which is the exact failure this test exists for: {seen:?}"
        ),
    }

    assert!(
        seen.iter().any(
            |(lane, message)| matches!(message, ServerMessage::MediaChunk { .. })
                && *lane == WriteLane::Media
        ),
        "and the encoded bytes follow on the droppable lane: {seen:?}"
    );
}

/// TP-VIDEO-TAP-01 · a drop is counted, so the policy can say it fired.
///
/// Dropping is correct — a blocking send returns video with accumulated delay
/// rather than a gap — but a policy that never reports is indistinguishable
/// from a bug, and "the picture stutters under load" has a dozen other
/// candidate causes. Starting from zero is the claim under test: the counter
/// exists and is readable, so a later report has something to read.
#[test]
fn dropped_chunks_are_counted_from_zero_so_the_drop_policy_can_report() {
    let (server, _pane, _public) = video_test_server();
    assert_eq!(
        server.pane_video.dropped_chunks(),
        0,
        "a supervisor that has encoded nothing has dropped nothing"
    );
}

/// TP-VIDEO-FEED-01 · ⭐⭐⭐ every picture that was shown reaches the encoder, in order.
///
/// The defect this whole branch exists for. `slot.layer` is one slot, not a
/// queue: a producer writing at 15 fps overwrites it between ticks, the tick
/// reads whatever survived, and the rest are gone. Measured on 2026-09-01 at
/// `ticks=360 seen=360 offered=66 deduped=294` — roughly nine percent of
/// frames arriving — while twenty-six tests, eight mutations and a green gate
/// all agreed the feature was correct.
///
/// They agreed because every one of them fed a *single* picture, and a single
/// picture cannot tell "read the last one" from "read them all". So this test
/// writes four distinct pictures between two ticks, and would pass on the old
/// code only if it read one.
#[test]
fn every_picture_written_between_ticks_reaches_the_encoder_in_order() {
    let (mut server, pane_id, _public) = video_test_server();
    let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let held = log.clone();
    server
        .pane_video
        .set_spawn_for_test(Box::new(move |_w, _h, _f| {
            Ok(recording_spawn(held.clone()))
        }));
    let _drain = connect_viewer(&mut server, 1, true);
    // One tick to notice the viewer: the producer is told to keep pictures
    // before it has any, exactly as it is in the loop.
    server.tick_pane_video();

    for fill in [0x11_u8, 0x22, 0x33, 0x44] {
        set_inline_layer(&mut server, pane_id, 4, 4, fill);
    }
    server.tick_pane_video();

    assert_eq!(
        recorded_fills(&log, 4 * 4 * 4, 4),
        vec![0x11, 0x22, 0x33, 0x44],
        "four pictures were shown and four reached the encoder, in the order they were shown"
    );
}

/// TP-VIDEO-FEED-02 · nothing is kept while nobody is watching.
///
/// The resource doctrine's central claim applied to this queue: a producer
/// pushing pictures into a session with no video client must cost one boolean
/// read and no memory. A queue that filled unconditionally would be a leak on
/// every still session — the exact failure `pane_audio` avoids by refusing to
/// encode when no client has credit.
#[test]
fn no_video_client_means_no_picture_is_kept() {
    let (mut server, pane_id, _public) = video_test_server();
    // A client that cannot decode video is still a client: the gate is the
    // capability, not the connection.
    let _drain = connect_viewer(&mut server, 1, false);
    server.tick_pane_video();

    for fill in [0x11_u8, 0x22, 0x33, 0x44, 0x55] {
        set_inline_layer(&mut server, pane_id, 64, 64, fill);
    }

    assert_eq!(
        server.app.pane_graphics.video_queued_frames(),
        0,
        "no viewer, no queue — a still session pays nothing for video it is not sending"
    );
    assert_eq!(server.app.pane_graphics.video_queued_bytes(), 0);
    assert_eq!(
        server.pane_video.encoding_count(),
        0,
        "and no encoder was started to drain it"
    );
}

/// TP-VIDEO-FEED-02 · a viewer leaving drops what was being kept for it.
///
/// The queue must not outlive its reason. Pictures held past the last viewer
/// are stale by the time anyone reconnects, so keeping them warm buys a worse
/// first frame at the price of memory nobody asked for.
#[test]
fn the_last_viewer_leaving_empties_the_queue() {
    let (mut server, pane_id, _public) = video_test_server();
    let _drain = connect_viewer(&mut server, 1, true);
    server.tick_pane_video();
    for fill in [0x11_u8, 0x22] {
        set_inline_layer(&mut server, pane_id, 32, 32, fill);
    }
    assert!(server.app.pane_graphics.video_queued_frames() > 0);

    server.clients.remove(&1);
    server.tick_pane_video();

    assert_eq!(server.app.pane_graphics.video_queued_frames(), 0);
    assert_eq!(server.app.pane_graphics.video_queued_bytes(), 0);
}

/// TP-VIDEO-FEED-06 · ⭐⭐ REACHABILITY — a real API write reaches the encoder.
///
/// The recording site is new wiring, and new wiring is how four defects landed
/// on 2026-09-01 (E28, E35, E36, E40): code correct, tested, mutated, green,
/// and never reached from production. Every other test here installs a layer
/// through the same fixture; this one goes through `pane.graphics.set`, the
/// request an actual producer sends, and asserts the picture arrived.
#[test]
fn a_picture_set_through_the_api_reaches_the_encoder() {
    let (mut server, pane_id, _public) = video_test_server();
    server.app.state.kitty_graphics_enabled = true;
    let public_pane_id = server
        .app
        .public_pane_id(0, pane_id)
        .expect("public pane id");
    let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let held = log.clone();
    server
        .pane_video
        .set_spawn_for_test(Box::new(move |_w, _h, _f| {
            Ok(recording_spawn(held.clone()))
        }));
    let _drain = connect_viewer(&mut server, 1, true);
    server.tick_pane_video();

    for fill in [0x66_u8, 0x77] {
        let (respond_to, _response_rx) = std::sync::mpsc::channel();
        server.handle_api_request_with_render_impact(api::ApiRequestMessage {
            request: api::schema::Request {
                id: format!("set-{fill}"),
                method: api::schema::Method::PaneGraphicsSet(api::schema::PaneGraphicsSetParams {
                    pane_id: public_pane_id.clone(),
                    layer_id: None,
                    z_index: 0,
                    owner: String::new(),
                    format: api::schema::PaneGraphicsFormat::Rgba,
                    image_width: 2,
                    image_height: 2,
                    data: Some(vec![fill; 2 * 2 * 4]),
                    data_base64: String::new(),
                    placement: api::schema::PaneGraphicsPlacementParams::default(),
                }),
            },
            respond_to,
            response_write_complete: None,
            stream_active: None,
        });
    }
    server.tick_pane_video();

    assert_eq!(
        recorded_fills(&log, 2 * 2 * 4, 2),
        vec![0x66, 0x77],
        "a picture set through the production API reaches the encoder — if this is empty the \
         recording site is unreachable, which is the failure this test exists for"
    );
}

/// TP-VIDEO-FEED-01 · a pane that stopped drawing does not start an encoder
/// with what it left behind.
///
/// The queue is drained on every tick whether or not the pane still has a
/// picture. Skipping the drain for a dead pane would leave its last frames for
/// a later tick to find, and `start` would open a stream for a pane whose
/// stream had just been closed — a spurious open/close pair on the wire and a
/// GPU session for a picture that no longer exists.
#[test]
fn frames_left_by_a_pane_that_stopped_drawing_do_not_start_an_encoder() {
    let (mut server, pane_id, public) = video_test_server();
    let _drain = connect_viewer(&mut server, 1, true);
    server.tick_pane_video();

    set_inline_layer(&mut server, pane_id, 4, 4, 0x88);
    // The picture goes away in the same window the frame was queued in.
    server
        .app
        .pane_graphics
        .slots
        .insert((pane_id, "primary".to_owned()), Slot::test(1, None));

    server.tick_pane_video();

    assert!(
        !server.pane_video.is_encoding(&public),
        "a pane with no picture starts no encoder, whatever it left in the queue"
    );
    assert_eq!(
        server.app.pane_graphics.video_queued_frames(),
        0,
        "and the leftovers are gone rather than waiting for a later tick"
    );
}

/// TP-VIDEO-WRITE-02 · ⭐ a chunk a viewer's lane refuses is counted, not lost
/// in silence.
///
/// The fourth drop stage and the last one to get a counter. The media lane
/// refuses rather than grows, deliberately — an unbounded lane turns a slow
/// link into a delay that never recovers — and its own documentation puts the
/// answer at the call site, "where it still knows the chunk's deadline". The
/// video producer answered with `let _ =`, which drops the chunk and the fact
/// together, and a slow viewer would then look exactly like a slow encoder,
/// a slow producer, or a busy loop.
#[test]
fn a_chunk_a_viewers_lane_refuses_is_counted() {
    let (mut server, pane_id, _public) = video_test_server();
    // An encoder that actually emits, so there is a chunk to refuse.
    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    let held = std::sync::Mutex::new(Some(rx));
    server
        .pane_video
        .set_spawn_for_test(Box::new(move |_w, _h, _f| {
            struct Sink;
            impl std::io::Write for Sink {
                fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                    Ok(buf.len())
                }
                fn flush(&mut self) -> std::io::Result<()> {
                    Ok(())
                }
            }
            struct Scripted {
                rx: std::sync::mpsc::Receiver<Vec<u8>>,
                left: Vec<u8>,
            }
            impl std::io::Read for Scripted {
                fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                    if self.left.is_empty() {
                        match self.rx.recv() {
                            Ok(next) if next.is_empty() => return Ok(0),
                            Ok(next) => self.left = next,
                            Err(_) => return Ok(0),
                        }
                    }
                    let n = self.left.len().min(buf.len());
                    buf[..n].copy_from_slice(&self.left[..n]);
                    self.left.drain(..n);
                    Ok(n)
                }
            }
            let rx = held
                .lock()
                .expect("spawn seam")
                .take()
                .ok_or("spawned twice")?;
            Ok(
                crate::server::headless::pane_video_capture::EncoderProcess {
                    child: None,
                    stdin: Box::new(Sink),
                    stdout: Box::new(Scripted {
                        rx,
                        left: Vec::new(),
                    }),
                },
            )
        }));
    let _drain = connect_viewer(&mut server, 1, true);
    set_inline_layer(&mut server, pane_id, 4, 4, 0x99);
    server.tick_pane_video();
    tx.send(program(0x100, &[access_unit(true, b"chunk")]))
        .expect("script");

    // The lane is filled to its bound by hand: nothing drains it here, which is
    // exactly the shape of a viewer whose link cannot keep up.
    {
        let writer = server.clients[&1].writer.as_ref().expect("writer");
        while writer.media.try_send(vec![0_u8; 4]).is_ok() {}
    }
    assert_eq!(server.pane_video_lane_dropped, 0, "nothing refused yet");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while server.pane_video_lane_dropped == 0 && std::time::Instant::now() < deadline {
        server.tick_pane_video();
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    assert!(
        server.pane_video_lane_dropped > 0,
        "a full media lane refused the chunk and the server said nothing — which is the \
         difference between a policy and a bug"
    );
}

// ── TP-VIDEO-LATE-01 · attaching is an event — the third time ───────────────

/// An encoder whose output the test writes.
///
/// Each `send` becomes one read on the supervisor's reader thread: the first
/// is the stream's open, the rest are chunks, an empty one is EOF. The same
/// shape as the reachability test above, lifted out because the late-arrival
/// tests need a stream that is *really* open on the wire before a second
/// viewer arrives — the silent encoder never opens one.
fn scripted_encoder(server: &mut HeadlessServer) -> std::sync::mpsc::Sender<Vec<u8>> {
    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    let held = std::sync::Mutex::new(Some(rx));
    server
        .pane_video
        .set_spawn_for_test(Box::new(move |_w, _h, _f| {
            struct Sink;
            impl std::io::Write for Sink {
                fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                    Ok(buf.len())
                }
                fn flush(&mut self) -> std::io::Result<()> {
                    Ok(())
                }
            }
            struct Scripted {
                rx: std::sync::mpsc::Receiver<Vec<u8>>,
                left: Vec<u8>,
            }
            impl std::io::Read for Scripted {
                fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                    if self.left.is_empty() {
                        match self.rx.recv() {
                            Ok(next) if next.is_empty() => return Ok(0),
                            Ok(next) => self.left = next,
                            Err(_) => return Ok(0),
                        }
                    }
                    let n = self.left.len().min(buf.len());
                    buf[..n].copy_from_slice(&self.left[..n]);
                    self.left.drain(..n);
                    Ok(n)
                }
            }
            let rx = held
                .lock()
                .expect("spawn seam")
                .take()
                .ok_or("spawned twice")?;
            Ok(
                crate::server::headless::pane_video_capture::EncoderProcess {
                    child: None,
                    stdin: Box::new(Sink),
                    stdout: Box::new(Scripted {
                        rx,
                        left: Vec::new(),
                    }),
                },
            )
        }));
    tx
}

/// Tick the production video tick until `done` is satisfied by what `drain`
/// has yielded so far, or five seconds pass. Everything drained is returned
/// in wire order: `opens` is destructive, so an assertion made after a poll
/// would otherwise never see what the poll consumed.
fn pump(
    server: &mut HeadlessServer,
    drain: &crate::server::client_transport::TestQueueDrain,
    done: impl Fn(&[(WriteLane, ServerMessage)]) -> bool,
) -> Vec<(WriteLane, ServerMessage)> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut seen = Vec::new();
    loop {
        server.tick_pane_video();
        seen.extend(opens(drain));
        if done(&seen) || std::time::Instant::now() >= deadline {
            return seen;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

fn is_open(message: &(WriteLane, ServerMessage)) -> bool {
    matches!(message.1, ServerMessage::MediaOpen { .. })
}

fn is_chunk(message: &(WriteLane, ServerMessage)) -> bool {
    matches!(message.1, ServerMessage::MediaChunk { .. })
}

fn opened_stream(seen: &[(WriteLane, ServerMessage)]) -> Option<u32> {
    seen.iter().find_map(|(_, message)| match message {
        ServerMessage::MediaOpen { stream_id, .. } => Some(*stream_id),
        _ => None,
    })
}

/// A viewer, a picture, and a stream that is open on the wire: the state
/// every late-arrival test starts from. Returns the first viewer's drain,
/// the encoder's input and the stream id the first viewer was told.
fn open_stream_with_first_viewer(
    server: &mut HeadlessServer,
    pane_id: crate::layout::PaneId,
) -> (
    crate::server::client_transport::TestQueueDrain,
    std::sync::mpsc::Sender<Vec<u8>>,
    u32,
) {
    let tx = scripted_encoder(server);
    let first = connect_viewer(server, 1, true);
    set_inline_layer(server, pane_id, 8, 8, 0x55);
    server.tick_pane_video();
    tx.send(program(0x100, &[access_unit(true, b"first")]))
        .expect("script");
    let seen = pump(server, &first, |seen| seen.iter().any(is_open));
    let stream_id = opened_stream(&seen).expect("the first viewer is told the stream opened");
    (first, tx, stream_id)
}

/// TP-VIDEO-LATE-01 · ⭐⭐ a viewer that arrives while the stream is open is told about it.
///
/// Measured before this test existed: with one viewer already watching, a
/// second `video-listen` received 0 B; on a clean server the same command
/// received 152,379 B. The open was sent once, at `VideoEvent::Opened`, so a
/// late viewer got chunks for a stream it had never been offered and
/// discarded every one of them. Attaching is an event — the third time that
/// lesson was paid for on 2026-09-01 (E36, TP-VIDEO-FEED-07, E47).
#[test]
fn a_viewer_that_arrives_while_a_stream_is_open_is_told_about_it() {
    let (mut server, pane_id, public) = video_test_server();
    let (_first, tx, stream_id) = open_stream_with_first_viewer(&mut server, pane_id);

    let late = connect_viewer(&mut server, 2, true);
    server.tick_pane_video();
    tx.send(pes(0x100, &access_unit(false, b"second"), true))
        .expect("script");
    let seen = pump(&mut server, &late, |seen| seen.iter().any(is_chunk));

    match seen.first() {
        Some((
            lane,
            ServerMessage::MediaOpen {
                stream_id: told,
                pane_id: pane,
                codec,
                params,
                ..
            },
        )) => {
            assert_eq!(
                *told, stream_id,
                "the same stream the first viewer is watching, not a new one"
            );
            assert_eq!(pane, &public);
            assert_eq!(codec, codec::H264);
            assert_eq!(
                params,
                &crate::protocol::MediaParams::Video {
                    width: 8,
                    height: 8,
                    fps: 30
                }
            );
            assert_eq!(
                *lane,
                WriteLane::Control,
                "an open a late viewer misses leaves it routing nothing"
            );
        }
        other => panic!(
            "a late viewer's first message must be the open of the stream already running, \
             got {other:?}; everything it received: {seen:?}"
        ),
    }
    assert!(
        seen.iter()
            .any(|message| is_chunk(message) && message.0 == WriteLane::Media),
        "and the chunks follow on the media lane: {seen:?}"
    );
}

/// TP-VIDEO-LATE-01 · the first viewer is not told again when a second arrives.
///
/// A second `MediaOpen` for a pane a client is already mirroring is read as
/// "stream restarted" (`video_listen.rs`): the consumer re-initialises its
/// decoder for a stream that never stopped. Telling the newcomer must not
/// mean telling everyone.
#[test]
fn the_first_viewer_is_not_told_twice_when_a_second_arrives() {
    let (mut server, pane_id, _public) = video_test_server();
    let (first, tx, _stream_id) = open_stream_with_first_viewer(&mut server, pane_id);

    let late = connect_viewer(&mut server, 2, true);
    server.tick_pane_video();
    tx.send(pes(0x100, &access_unit(false, b"second"), true))
        .expect("script");
    let _ = pump(&mut server, &late, |seen| seen.iter().any(is_chunk));
    // Two more ticks with nothing new: an open that was going to be repeated
    // would have been repeated by now — announcing is synchronous in the tick.
    server.tick_pane_video();
    server.tick_pane_video();

    let repeated = opens(&first)
        .iter()
        .filter(|message| is_open(message))
        .count();
    assert_eq!(
        repeated, 0,
        "the first viewer was told once, before the second arrived, and never again"
    );
}

/// TP-VIDEO-LATE-01 · a viewer that leaves and returns is told again.
///
/// Identity is the client id, and the record of who was told must not
/// outlive the client: a set that kept departed ids would grow with every
/// viewer that ever left, and an id seen again would be taken for a viewer
/// that already knows. The departure travels the production event rather
/// than a direct call, so the forgetting is tested where it is wired.
#[tokio::test]
async fn a_viewer_that_leaves_and_returns_is_told_again() {
    let (mut server, pane_id, _public) = video_test_server();
    let (first, tx, stream_id) = open_stream_with_first_viewer(&mut server, pane_id);
    // A second viewer keeps the stream alive while the first is away;
    // otherwise the last viewer leaving stops everything and the return
    // would be a fresh open rather than a late one.
    let _keeper = connect_viewer(&mut server, 2, true);
    server.tick_pane_video();
    drop(first);

    server.handle_server_event(ServerEvent::ClientDisconnected { client_id: 1 });
    assert!(!server.clients.contains_key(&1), "the viewer is gone");
    server.tick_pane_video();

    let returned = connect_viewer(&mut server, 1, true);
    server.tick_pane_video();
    tx.send(pes(0x100, &access_unit(false, b"after"), true))
        .expect("script");
    let seen = pump(&mut server, &returned, |seen| seen.iter().any(is_chunk));

    assert_eq!(
        opened_stream(&seen),
        Some(stream_id),
        "the returning viewer is told about the stream that kept running: {seen:?}"
    );
    assert!(
        seen.iter().any(is_chunk),
        "and receives its chunks: {seen:?}"
    );
}

/// TP-VIDEO-LATE-01 · a viewer that arrives after the stream ended is told nothing.
///
/// A closed stream announces itself to nobody: an open with no encoder
/// behind it is a ghost the viewer would wait on forever, and the close it
/// would need was sent before it arrived.
#[test]
fn a_viewer_that_arrives_after_a_stream_ended_is_told_nothing() {
    let (mut server, pane_id, _public) = video_test_server();
    let (first, _tx, stream_id) = open_stream_with_first_viewer(&mut server, pane_id);

    // The pane stops showing a picture: the stale sweep closes its stream.
    server
        .app
        .pane_graphics
        .slots
        .remove(&(pane_id, "primary".to_owned()));
    server.tick_pane_video();
    let closed = opens(&first).into_iter().any(|(_, message)| {
        matches!(message, ServerMessage::MediaClose { stream_id: closed, .. } if closed == stream_id)
    });
    assert!(
        closed,
        "the first viewer saw the stream close — the precondition this test rests on"
    );

    let late = connect_viewer(&mut server, 2, true);
    server.tick_pane_video();
    server.tick_pane_video();
    let seen = opens(&late);
    assert!(
        seen.is_empty(),
        "nothing is open, so nothing is announced: {seen:?}"
    );
}

/// TP-VIDEO-LATE-01 · ⭐ REACHABILITY — the late viewer arrives through the
/// production connect path, not through `clients.insert`.
///
/// Every earlier gap on 2026-09-01 (E28, E35, E47) was a feature correct at
/// the function and unreachable from the product. So this one registers the
/// second viewer exactly as the accept thread does — `ServerEvent::ClientConnected`
/// carrying the capabilities the handshake accepted — and reads the wire.
#[tokio::test]
async fn a_late_viewer_is_told_through_the_production_connect_path() {
    let (mut server, pane_id, public) = video_test_server();
    let (_first, tx, stream_id) = open_stream_with_first_viewer(&mut server, pane_id);

    let (writer, late) = ClientWriter::test_channel_through_queue();
    server.handle_server_event(ServerEvent::ClientConnected {
        client_id: 2,
        cols: 80,
        rows: 24,
        cell_width_px: 8,
        cell_height_px: 16,
        render_encoding: RenderEncoding::SemanticFrame,
        keybindings: None,
        direct_attach_requested: false,
        direct_graphics: false,
        media_listen: true,
        pixel_mouse: false,
        writer,
        capabilities: vec![CapabilityEntry::with_values(
            capability::VIDEO_DECODE,
            [codec::H264],
        )],
    });
    assert!(
        server.clients.contains_key(&2),
        "the connect event registered the viewer"
    );

    server.tick_pane_video();
    tx.send(pes(0x100, &access_unit(false, b"second"), true))
        .expect("script");
    let seen = pump(&mut server, &late, |seen| seen.iter().any(is_chunk));

    assert_eq!(
        opened_stream(&seen),
        Some(stream_id),
        "the newcomer that arrived through the real connect path is told: {seen:?}"
    );
    assert!(
        seen.iter().any(|(_, message)| {
            matches!(message, ServerMessage::MediaOpen { pane_id, .. } if pane_id == &public)
        }),
        "and told which pane: {seen:?}"
    );
}

/// TP-VIDEO-SEQ-01 · two panes number their chunks independently.
///
/// `seq` is "monotonic per stream; a gap is a loss". One counter shared by
/// every pane made a viewer of pane A see a gap each time pane B sent, and
/// a player conceals what it takes for a loss — so it would conceal the
/// other pane's pictures.
#[test]
fn two_panes_number_their_chunks_independently() {
    let (mut server, _pane, _public) = video_test_server();
    let drain = connect_viewer(&mut server, 1, true);
    let picture = |fill: u8| access_unit(fill == 0, &[fill; 8]);
    let opened = |pane: &str, pts_us: u64| VideoEvent::Opened {
        pane_id: pane.to_owned(),
        width: 4,
        height: 4,
        fps: 30,
        chunk: picture(0),
        pts_us,
    };
    let chunk = |pane: &str, fill: u8, pts_us: u64| VideoEvent::Chunk {
        pane_id: pane.to_owned(),
        chunk: picture(fill),
        pts_us,
    };
    server.deliver_video_event(opened("w1:pA", 1));
    server.deliver_video_event(chunk("w1:pA", 1, 2));
    server.deliver_video_event(opened("w1:pB", 3));
    server.deliver_video_event(chunk("w1:pB", 1, 4));
    server.deliver_video_event(chunk("w1:pA", 2, 5));
    server.deliver_video_event(chunk("w1:pB", 2, 6));
    server.deliver_video_event(chunk("w1:pA", 3, 7));

    let seen = opens(&drain);
    let stream_of = |pane: &str| {
        seen.iter()
            .find_map(|(_, message)| match message {
                ServerMessage::MediaOpen {
                    stream_id, pane_id, ..
                } if pane_id == pane => Some(*stream_id),
                _ => None,
            })
            .expect("the pane's stream was opened")
    };
    let seqs = |stream: u32| -> Vec<u64> {
        seen.iter()
            .filter_map(|(_, message)| match message {
                ServerMessage::MediaChunk { stream_id, seq, .. } if *stream_id == stream => {
                    Some(*seq)
                }
                _ => None,
            })
            .collect()
    };
    let (a, b) = (stream_of("w1:pA"), stream_of("w1:pB"));
    assert_ne!(a, b);
    assert_eq!(seqs(a), vec![0, 1, 2, 3], "pane A counts its own pictures");
    assert_eq!(seqs(b), vec![0, 1, 2], "and pane B counts its own");
}

/// TP-VIDEO-PTS-01 · pictures that were shown at two moments reach the wire
/// with those two moments, however they were drained.
#[test]
fn a_chunk_on_the_wire_carries_its_pictures_own_moment() {
    let (mut server, _pane, _public) = video_test_server();
    let drain = connect_viewer(&mut server, 1, true);
    server.deliver_video_event(VideoEvent::Opened {
        pane_id: "w1:pA".to_owned(),
        width: 4,
        height: 4,
        fps: 30,
        chunk: access_unit(true, b"k"),
        pts_us: 5_000,
    });
    server.deliver_video_event(VideoEvent::Chunk {
        pane_id: "w1:pA".to_owned(),
        chunk: access_unit(false, b"p"),
        pts_us: 105_000,
    });
    let stamps: Vec<u64> = opens(&drain)
        .iter()
        .filter_map(|(_, message)| match message {
            ServerMessage::MediaChunk { pts_us, .. } => Some(*pts_us),
            _ => None,
        })
        .collect();
    assert_eq!(
        stamps,
        vec![5_000, 105_000],
        "the wire says when each picture was shown, not when the loop looked"
    );
}

/// TP-VIDEO-PTS-02 · the server's video supervisor is wired to the server's
/// wake, so a picture is delivered on the picture rather than on the timer.
#[test]
fn the_servers_video_supervisor_wakes_the_servers_loop() {
    let (server, _pane, _public) = video_test_server();
    assert!(
        server.pane_video.wakes(&server.pane_video_wake),
        "a supervisor wired to some other handle wakes nobody"
    );
}

/// TP-VIDEO-PTS-04 · pictures queued between ticks reach the wire with their
/// own moments, however many one tick feeds.
#[test]
fn pictures_queued_between_ticks_reach_the_wire_with_their_own_moments() {
    let (mut server, pane_id, _public) = video_test_server();
    let tx = scripted_encoder(&mut server);
    let drain = connect_viewer(&mut server, 1, true);
    server.tick_pane_video();
    set_inline_layer_at(&mut server, pane_id, 0x11, 5_000);
    set_inline_layer_at(&mut server, pane_id, 0x22, 35_000);
    server.tick_pane_video();
    tx.send(program(
        0x100,
        &[access_unit(true, b"a"), access_unit(false, b"b")],
    ))
    .expect("script");
    let seen = pump(&mut server, &drain, |seen| {
        seen.iter().filter(|message| is_chunk(message)).count() >= 2
    });
    let stamps: Vec<u64> = seen
        .iter()
        .filter_map(|(_, message)| match message {
            ServerMessage::MediaChunk { pts_us, .. } => Some(*pts_us),
            _ => None,
        })
        .collect();
    assert_eq!(
        stamps,
        vec![5_000, 35_000],
        "each picture's own moment: {seen:?}"
    );
}

/// TP-VIDEO-PTS-05 · the server's graphics runtime wakes the server's video arm.
#[test]
fn the_servers_graphics_runtime_wakes_the_servers_video_arm() {
    let (server, _pane, _public) = video_test_server();
    assert!(server.app.pane_graphics.wakes(&server.pane_video_wake));
}

/// TP-VIDEO-PTS-06 · the wake feeds the encoder: a picture recorded after
/// the tick reaches the encoder on the wake, without waiting for the timer.
#[test]
fn the_video_wake_feeds_the_encoder() {
    let (mut server, pane_id, _public) = video_test_server();
    let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Vec<u8>>::new()));
    let seen_log = std::sync::Arc::clone(&log);
    server
        .pane_video
        .set_spawn_for_test(Box::new(move |_w, _h, _f| {
            Ok(recording_spawn(std::sync::Arc::clone(&seen_log)))
        }));
    let _drain = connect_viewer(&mut server, 1, true);
    server.tick_pane_video();
    set_inline_layer(&mut server, pane_id, 4, 4, 0x33);
    server.on_pane_video_wake();
    assert_eq!(
        recorded_fills(&log, 64, 1),
        vec![0x33],
        "fed on the wake, not on a later tick"
    );
}
