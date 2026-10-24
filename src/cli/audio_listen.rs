//! `herdr pane audio-listen --pane <id>` — one pane's audio, on stdout.
//!
//! ⛔ **This is a CLIENT, not a tap.** Reading the server's `Outbound` queue
//! directly would be shorter and would work, and it would lose capability
//! negotiation, credit and reconnect at the same time — silently, which is this
//! feature's signature failure. If a future change makes the tap look tempting
//! because it is shorter, that shortness is the cost being hidden, not saved.
//!
//! Backpressure needs no mechanism of its own. A slow consumer fills the pipe,
//! the write to stdout blocks, this stops reading, credit stops advancing, and
//! the server drops at the source under the policy it already has. Adding a
//! second mechanism would mean two that have to agree.
//!
//! Two contracts the consumer depends on:
//!
//! 1. **stdout carries frame bytes and nothing else.** Every human-readable
//!    line goes to stderr. The bridge reads stdout as a binary stream, so one
//!    stray line lands mid-frame and every frame after it is garbage.
//! 2. **A `Close` frame is written before this process exits**, whenever it can
//!    still write one. The consumer tells *ended* from *crashed* by whether a
//!    Close arrived, and its respawn decision hangs on that — so a Close-less
//!    EOF must mean this process died, never that a stream finished.
//!
//! There is no reconnect loop here on purpose. The process ends when the stream
//! ends; respawning is the caller's job, exactly as it already is for
//! `herdr terminal session observe`. One lifetime, one meaning.

use std::io::{self, Write};

use tracing::info;

use crate::client::{client_socket_path, connect_media_listen_stream};
use crate::media::web_frame::{
    close_payload, open_payload, WebCloseReason, WebFrame, WebFrameKind,
};
use crate::protocol::{
    self, codec, ClientMessage, MediaCloseReason, MediaParams, ServerMessage, MAX_FRAME_SIZE,
};

/// How many chunks this listener declares it can still take.
///
/// One second at 20 ms frames. It is a window, not a budget: it is re-declared
/// after every chunk that reaches stdout, and a write only returns once the
/// consumer has taken the bytes. So credit advances exactly as fast as the
/// consumer drains, and when it stalls the server drops at the source. The
/// number only has to be large enough that a healthy consumer never waits on it
/// and small enough that a stalled one is noticed within a second.
const CREDIT_WINDOW: u16 = 50;

// Checked by the compiler rather than by a test, because both facts are known
// at compile time and a guarantee the build enforces cannot be skipped:
//
//   * zero is the protocol's refusal, so a window that reached zero would
//     decline the very stream it exists to carry — and the symptom would be
//     silence with every layer reporting success
//   * the window is one second at the frame size the codec actually uses, so
//     changing FRAME_MS without revisiting this stops compiling instead of
//     quietly making the window mean something else
const _: () = {
    assert!(CREDIT_WINDOW > 0);
    assert!(CREDIT_WINDOW as u32 == 1000 / crate::media::FRAME_MS);
};

/// What the caller asked for.
pub(crate) struct AudioListenArgs {
    pub(crate) pane_id: String,
}

/// Everything that can end this **process**.
///
/// Deliberately short: a stream ending is not on this list. Streams open and
/// close as a pane's audio comes and goes, and each of those is a `Close` frame
/// on stdout rather than an exit. What ends the process is one of its two ends
/// going away.
enum Ending {
    /// The server went away, or refused to go on. A `Close` goes out first if a
    /// stream was still open, so the consumer is never left guessing.
    ServerGone(String),
    /// The consumer closed the pipe. Normal — it is the one who decided.
    ConsumerLeft,
}

pub(crate) fn run(args: AudioListenArgs) -> io::Result<i32> {
    let socket_path = client_socket_path();
    let mut stream = match connect_media_listen_stream(&socket_path, &args.pane_id) {
        Ok(stream) => stream,
        Err(message) => {
            eprintln!("herdr: {message}");
            return Ok(1);
        }
    };

    let mut out = io::stdout().lock();
    let mut scratch: Vec<u8> = Vec::with_capacity(4096);
    // The one stream this process mirrors, once the server opens it.
    let mut mirrored: Option<u32> = None;

    let ending = loop {
        let message = match protocol::read_message(&mut stream, MAX_FRAME_SIZE) {
            Ok(message) => message,
            Err(err) => {
                // The server went away. If a stream was open the consumer is
                // owed a Close saying so — silence would read as this process
                // having died, and the respawn policy hangs on that difference.
                break Ending::ServerGone(format!("server disconnected: {err}"));
            }
        };

        match message {
            ServerMessage::MediaOpen {
                stream_id,
                pane_id,
                codec: offered,
                params,
                target_latency_us,
            } => {
                // Every capable client is offered every pane's stream, so a
                // stream for another pane is refused rather than discarded:
                // discarding it would leave the server encoding and sending
                // audio nobody reads. Zero credit is the protocol's one
                // refusal, and it is what the TUI already answers with.
                if pane_id != args.pane_id || offered != codec::OPUS {
                    decline(&mut stream, stream_id);
                    continue;
                }
                // Video streams now exist and reach every media listener. This
                // one has an audio frame format and an audio consumer on the
                // other end of stdout, so it declines rather than dropping:
                // dropping would leave the server encoding pictures nobody
                // reads. The codec check above already refuses anything but
                // Opus; this arm is what stops a future audio codec carrying
                // non-audio params from being read as audio anyway.
                let MediaParams::Audio {
                    sample_rate_hz,
                    channels,
                } = params
                else {
                    decline(&mut stream, stream_id);
                    continue;
                };
                let payload = open_payload(
                    &pane_id,
                    &offered,
                    sample_rate_hz,
                    channels,
                    target_latency_us,
                );
                if !emit(
                    &mut out,
                    &mut scratch,
                    WebFrameKind::Open,
                    stream_id,
                    0,
                    0,
                    &payload,
                )? {
                    break Ending::ConsumerLeft;
                }
                mirrored = Some(stream_id);
                // Credit starts at zero for every client the moment a stream
                // opens, so without this first report nothing is ever sent and
                // every layer looks healthy while the consumer hears silence.
                //
                // One round trip is unavoidable here and is not a defect: a
                // chunk offered between the server's open and this grant has
                // nowhere to go, so the server drops it and consumes its
                // sequence number. Measured live — a stream begins at a
                // non-zero `seq` when that happens, which is exactly how the
                // consumer learns twenty milliseconds went missing. Seeding the
                // room on subscribe instead would send chunks to a client that
                // never said it could take them, which is the failure the
                // zero-credit refusal exists to prevent.
                grant(&mut stream, stream_id);
            }

            ServerMessage::MediaChunk {
                stream_id,
                // Passed through, never renumbered. The server's numbering is
                // per stream, starts at zero, and — this is the part that makes
                // it useful — a dropped chunk still consumes its number
                // (`media::stream`, `a_dropped_chunk_still_consumes_its_slot_in_time_and_sequence`).
                // So a gap here is not a hint that something was lost, it is
                // exactly how much: two missing numbers are 40 ms to conceal.
                //
                // Renumbering into a contiguous sequence would hand the consumer
                // a stream that looks perfect and sounds wrong, and nothing
                // downstream could tell.
                seq,
                pts_us,
                data,
            } => {
                if mirrored != Some(stream_id) {
                    continue;
                }
                if !emit(
                    &mut out,
                    &mut scratch,
                    WebFrameKind::Chunk,
                    stream_id,
                    seq,
                    pts_us,
                    &data,
                )? {
                    break Ending::ConsumerLeft;
                }
                // Re-declared only now, after the consumer took the bytes.
                // That is the whole backpressure chain.
                grant(&mut stream, stream_id);
            }

            ServerMessage::MediaClose {
                stream_id,
                reason,
                detail,
            } => {
                if mirrored != Some(stream_id) {
                    continue;
                }
                // Forwarded, and then this keeps listening. A stream ending is
                // not this process ending: a pane's audio stops every time a
                // video is paused, and the capture opens a fresh stream when it
                // resumes. Exiting here would mean a process launch per pause,
                // and the caller would have to tell "the pane is quiet again"
                // from "something broke" with no way to.
                //
                // The lifetime being mirrored is the **pane's**, not one
                // stream's — which is also what the verb says.
                let payload = close_payload(
                    match reason {
                        MediaCloseReason::Ended => WebCloseReason::Ended,
                        MediaCloseReason::Failed => WebCloseReason::Failed,
                    },
                    &detail,
                );
                if !emit(
                    &mut out,
                    &mut scratch,
                    WebFrameKind::Close,
                    stream_id,
                    0,
                    0,
                    &payload,
                )? {
                    break Ending::ConsumerLeft;
                }
                mirrored = None;
            }

            ServerMessage::ServerShutdown { reason } => {
                break Ending::ServerGone(
                    reason.unwrap_or_else(|| "server is shutting down".to_owned()),
                );
            }

            // Everything else on this socket belongs to a client that draws.
            // A listener is not one, and reading past what it needs is how a
            // narrow tool grows into a wide one.
            _ => {}
        }
    };

    finish(&mut out, &mut scratch, mirrored, ending)
}

/// Writes the closing frame, if there is still somebody to write it to.
fn finish(
    out: &mut impl Write,
    scratch: &mut Vec<u8>,
    mirrored: Option<u32>,
    ending: Ending,
) -> io::Result<i32> {
    let detail = match &ending {
        Ending::ServerGone(detail) => detail.clone(),
        // The pipe is gone, so no Close can be written and none is owed: the
        // consumer is the one who ended it and already knows.
        Ending::ConsumerLeft => {
            info!("audio listener stopped: the consumer closed the pipe");
            return Ok(0);
        }
    };

    if let Some(stream_id) = mirrored {
        // A stream was still live when the server went, so the consumer is owed
        // a Close saying why. Control frames carry no sequence of their own —
        // `seq` belongs to the chunk stream, and a number here would sit in a
        // field that means something else.
        let payload = close_payload(WebCloseReason::Failed, &detail);
        let _ = emit(out, scratch, WebFrameKind::Close, stream_id, 0, 0, &payload);
    }
    eprintln!("herdr: audio listener stopped: {detail}");
    Ok(1)
}

/// Encodes one frame and writes it. `false` means the consumer closed the pipe.
///
/// A closed pipe is an ordinary end, not an exceptional one — panicking here
/// would kill a session because somebody pressed `q` in a pager.
fn emit(
    out: &mut impl Write,
    scratch: &mut Vec<u8>,
    kind: WebFrameKind,
    stream_id: u32,
    seq: u64,
    pts_us: u64,
    payload: &[u8],
) -> io::Result<bool> {
    scratch.clear();
    if let Err(err) = (WebFrame {
        kind,
        flags: 0,
        stream_id,
        seq,
        pts_us,
        payload,
    })
    .encode_into(scratch)
    {
        // Refused before anything was written, so the stream is still intact
        // and dropping this one frame is the smallest possible loss.
        eprintln!("herdr: dropped an audio frame: {err}");
        return Ok(true);
    }
    match out.write_all(scratch).and_then(|()| out.flush()) {
        Ok(()) => Ok(true),
        Err(err) if err.kind() == io::ErrorKind::BrokenPipe => Ok(false),
        Err(err) => Err(err),
    }
}

/// Tells the server this listener can still take a window of chunks.
fn grant(stream: &mut crate::ipc::LocalStream, stream_id: u32) {
    let _ = protocol::write_message(
        stream,
        &ClientMessage::MediaCredit {
            stream_id,
            chunks: CREDIT_WINDOW,
        },
    );
}

/// Refuses a stream with zero credit — the protocol's one refusal.
fn decline(stream: &mut crate::ipc::LocalStream, stream_id: u32) {
    let _ = protocol::write_message(
        stream,
        &ClientMessage::MediaCredit {
            stream_id,
            chunks: 0,
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::web_frame::{WEB_FRAME_HEADER_BYTES, WEB_FRAME_MAGIC, WEB_FRAME_VERSION};

    /// A writer that refuses everything the way a closed pipe does.
    struct ClosedPipe;

    impl Write for ClosedPipe {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed"))
        }
        fn flush(&mut self) -> io::Result<()> {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed"))
        }
    }

    /// A writer that fails in a way that is *not* a closed pipe.
    struct BrokenDisk;

    impl Write for BrokenDisk {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::StorageFull, "no room"))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn header_of(bytes: &[u8]) -> (u8, u32, u64, u64, usize) {
        assert!(bytes.len() >= WEB_FRAME_HEADER_BYTES, "not a whole frame");
        assert_eq!(bytes[0], WEB_FRAME_MAGIC);
        assert_eq!(bytes[1], WEB_FRAME_VERSION);
        let u32_at = |o: usize| u32::from_le_bytes(bytes[o..o + 4].try_into().unwrap());
        let u64_at = |o: usize| u64::from_le_bytes(bytes[o..o + 8].try_into().unwrap());
        (
            bytes[2],
            u32_at(4),
            u64_at(8),
            u64_at(16),
            u32_at(24) as usize,
        )
    }

    fn body_of(bytes: &[u8]) -> serde_json::Value {
        serde_json::from_slice(&bytes[WEB_FRAME_HEADER_BYTES..]).expect("control body is json")
    }

    /// EG-9 — a closed pipe is an ordinary end, not a panic.
    ///
    /// The consumer pressing `q` in a pager must not take a session with it.
    #[test]
    fn a_closed_pipe_ends_the_stream_without_a_panic() {
        let mut scratch = Vec::new();
        let more = emit(
            &mut ClosedPipe,
            &mut scratch,
            WebFrameKind::Chunk,
            1,
            0,
            0,
            b"abc",
        )
        .expect("a closed pipe is not an error to report");
        assert!(!more, "a closed pipe means stop, not continue");
    }

    /// And a failure that is *not* a closed pipe is still an error.
    ///
    /// Treating every write failure as a polite end would swallow a real one:
    /// the process would exit 0 having written nothing, and the caller would
    /// read that as a stream that finished.
    #[test]
    fn a_write_failure_that_is_not_a_closed_pipe_is_reported() {
        let mut scratch = Vec::new();
        let err = emit(
            &mut BrokenDisk,
            &mut scratch,
            WebFrameKind::Chunk,
            1,
            0,
            0,
            b"abc",
        )
        .expect_err("a full disk is not an ordinary end");
        assert_eq!(err.kind(), io::ErrorKind::StorageFull);
    }

    /// One refused frame is one lost frame, not a lost stream.
    ///
    /// The encoder writes nothing when it refuses, so the bytes already on
    /// stdout still describe whole frames and the consumer stays in sync.
    #[test]
    fn a_frame_the_encoder_refuses_costs_one_frame_and_not_the_stream() {
        let oversize = vec![0_u8; crate::media::web_frame::WEB_FRAME_MAX_PAYLOAD + 1];
        let mut out: Vec<u8> = Vec::new();
        let mut scratch = Vec::new();

        let more = emit(
            &mut out,
            &mut scratch,
            WebFrameKind::Chunk,
            1,
            0,
            0,
            &oversize,
        )
        .expect("a refusal is not an io error");

        assert!(more, "the stream continues");
        assert!(out.is_empty(), "nothing half-written reached the consumer");
    }

    /// EG-15 — a stream cut short by the server's departure says so.
    ///
    /// The consumer is mid-stream when the server goes. Without this frame it
    /// would see bytes simply stop, which is what a dead process looks like —
    /// and its respawn decision hangs on telling those apart.
    #[test]
    fn a_stream_still_open_when_the_server_goes_gets_a_close_that_says_why() {
        let mut out: Vec<u8> = Vec::new();
        let mut scratch = Vec::new();

        let code = finish(
            &mut out,
            &mut scratch,
            Some(9),
            Ending::ServerGone("server disconnected: eof".to_owned()),
        )
        .expect("finishing writes");

        assert_eq!(code, 1, "the server going is not an ordinary end");
        let (kind, stream_id, seq, pts, len) = header_of(&out);
        assert_eq!(kind, WebFrameKind::Close as u8);
        assert_eq!(stream_id, 9);
        assert_eq!(seq, 0, "control frames carry no chunk sequence");
        assert_eq!(pts, 0);
        assert_eq!(len, out.len() - WEB_FRAME_HEADER_BYTES);
        let body = body_of(&out);
        assert_eq!(body["reason"], "failed");
        assert_eq!(body["detail"], "server disconnected: eof");
    }

    /// A consumer that left is owed nothing.
    ///
    /// It closed the pipe, so there is nowhere to write and it already knows.
    /// Exiting non-zero here would make an ordinary `| head` look like a fault.
    #[test]
    fn a_consumer_that_closed_the_pipe_is_not_owed_a_close() {
        let mut out: Vec<u8> = Vec::new();
        let mut scratch = Vec::new();

        let code = finish(&mut out, &mut scratch, Some(1), Ending::ConsumerLeft)
            .expect("finishing writes");

        assert_eq!(code, 0);
        assert!(out.is_empty(), "nothing is written to a pipe that is gone");
    }

    /// And a stream that never opened closes nothing.
    ///
    /// A Close naming a stream the consumer was never told about would be a
    /// frame it has to decide what to do with, for no reason.
    #[test]
    fn a_stream_that_never_opened_writes_no_close() {
        let mut out: Vec<u8> = Vec::new();
        let mut scratch = Vec::new();

        let code = finish(
            &mut out,
            &mut scratch,
            None,
            Ending::ServerGone("gone".to_owned()),
        )
        .expect("finishing writes");

        assert_eq!(code, 1, "the server still went away");
        assert!(
            out.is_empty(),
            "a Close for a stream the consumer never heard of is a frame it \
             has to decide what to do with, for no reason"
        );
    }
}
