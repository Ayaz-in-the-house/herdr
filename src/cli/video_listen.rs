//! `herdr pane video-listen --pane <id>` — one pane's H.264, on stdout.
//!
//! ⛔ **A CLIENT, not a tap** — the same rule `audio_listen` states, for the
//! same reason: reading the server's queue directly would be shorter and would
//! lose capability negotiation, which is the only thing that can tell this
//! build "the server cannot encode" from "the stream has not started yet".
//!
//! Two contracts the consumer depends on:
//!
//! 1. **stdout carries Annex-B H.264 and nothing else** — or, with `--framed`,
//!    the web frames `audio_listen` writes and nothing else. Every human-readable
//!    line goes to stderr. A single stray byte on stdout lands inside a NAL and
//!    every picture after it is garbage — which a decoder reports as corrupt
//!    video rather than as a stray byte, so the cause is invisible.
//! 2. **stdout is a decoder's input, not a file.** The whole point is that
//!    `herdr pane video-listen --pane X | mpv -` needs no client-side drawing
//!    code: the decoder already exists on every machine that would watch this.
//!
//! There is no reconnect loop, exactly as in `audio_listen`: the process ends
//! when the stream ends, and respawning is the caller's job. One lifetime, one
//! meaning.

use std::io::{self, Write};

use crate::client::{client_socket_path, connect_video_listen_stream};
use crate::media::h264::access_unit_has_idr;
use crate::media::web_frame::{
    close_payload, video_open_payload, WebCloseReason, WebFrame, WebFrameKind, WEB_FRAME_FLAG_KEY,
};
use crate::protocol::{self, codec, MediaCloseReason, MediaParams, ServerMessage, MAX_FRAME_SIZE};

/// What the caller asked for.
pub(crate) struct VideoListenArgs {
    pub(crate) pane_id: String,
    /// Print a byte/rate summary to stderr as it goes.
    ///
    /// The slice exists to settle a number, and a consumer pipe swallows the
    /// only place that number could be read. So it is offered here, on the
    /// stream that is not the video.
    pub(crate) meter: bool,
    /// Write web frames rather than bare picture bytes.
    ///
    /// The format `audio_listen` already writes: a header carrying the
    /// server's `seq` and `pts_us`, an open naming the codec and the picture
    /// size, a close saying why. Bare Annex-B is what a decoder pipe wants; a
    /// player also wants to know *when*, and a bare byte stream has nowhere
    /// to carry it. TP-WIRE-K6-01
    pub(crate) framed: bool,
}

pub(crate) fn run(args: VideoListenArgs) -> io::Result<i32> {
    let socket_path = client_socket_path();
    let mut stream = match connect_video_listen_stream(&socket_path) {
        Ok(stream) => stream,
        Err(message) => {
            eprintln!("herdr: {message}");
            return Ok(1);
        }
    };

    eprintln!(
        "herdr: connected, waiting for {}'s video — the encoder starts when the \
         pane has an inline graphics layer",
        args.pane_id
    );

    let mut out = io::stdout().lock();
    let mut forwarder = Forwarder::new(args.pane_id.clone(), args.framed);
    let started = std::time::Instant::now();
    let mut last_report = started;

    loop {
        let message = match protocol::read_message(&mut stream, MAX_FRAME_SIZE) {
            Ok(message) => message,
            Err(err) => {
                report(&args, &forwarder, started, true);
                eprintln!("herdr: server disconnected: {err}");
                return Ok(1);
            }
        };
        match forwarder.forward(&message, &mut out)? {
            Step::Continue => {}
            Step::Ended(code) => {
                report(&args, &forwarder, started, true);
                return Ok(code);
            }
        }
        if args.meter && last_report.elapsed() >= std::time::Duration::from_secs(2) {
            last_report = std::time::Instant::now();
            report(&args, &forwarder, started, false);
        }
    }
}

/// What one message did to the process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Step {
    /// Keep reading.
    Continue,
    /// The stream is over, or the consumer is gone; exit with this code.
    Ended(i32),
}

/// Turns the server's messages into the bytes one consumer wants.
///
/// Kept apart from the socket so the byte contract can be held without a
/// server: what reaches stdout for each message is the whole of what a
/// consumer depends on, and it is decidable from the message alone.
pub(crate) struct Forwarder {
    pane_id: String,
    framed: bool,
    /// The one stream this process mirrors, once the server opens it.
    mirrored: Option<u32>,
    scratch: Vec<u8>,
    bytes: u64,
    chunks: u64,
}

impl Forwarder {
    pub(crate) fn new(pane_id: String, framed: bool) -> Self {
        Self {
            pane_id,
            framed,
            mirrored: None,
            scratch: Vec::with_capacity(4096),
            bytes: 0,
            chunks: 0,
        }
    }

    /// Picture bytes forwarded so far.
    pub(crate) fn bytes(&self) -> u64 {
        self.bytes
    }

    /// Pictures forwarded so far.
    pub(crate) fn chunks(&self) -> u64 {
        self.chunks
    }

    pub(crate) fn forward(
        &mut self,
        message: &ServerMessage,
        out: &mut impl Write,
    ) -> io::Result<Step> {
        match message {
            ServerMessage::MediaOpen {
                stream_id,
                pane_id,
                codec: offered,
                params,
                target_latency_us,
            } => {
                // Every capable client is offered every pane's stream. One for
                // another pane, or in a codec this process does not forward, is
                // ignored rather than mirrored: mirroring it would splice two
                // unrelated pictures into one byte stream, and the decoder
                // would render the result rather than reporting it.
                if pane_id != &self.pane_id || offered != codec::H264 {
                    return Ok(Step::Continue);
                }
                let MediaParams::Video { width, height, fps } = params else {
                    return Ok(Step::Continue);
                };
                if self.mirrored.is_some() {
                    // A second stream for the same pane means the first one is
                    // over — a resize opens a new stream rather than changing
                    // an old one. Following the new one keeps the picture; the
                    // decoder sees new parameter sets and re-inits.
                    eprintln!("herdr: stream restarted at {width}x{height}");
                }
                self.mirrored = Some(*stream_id);
                eprintln!(
                    "herdr: {pane_id} {width}x{height} @{fps} h264 — pipe this into a decoder, \
                     for example: herdr pane video-listen --pane {pane_id} | mpv -"
                );
                if !self.framed {
                    return Ok(Step::Continue);
                }
                let payload =
                    video_open_payload(pane_id, offered, *width, *height, *fps, *target_latency_us);
                self.write_frame(out, WebFrameKind::Open, *stream_id, 0, 0, 0, &payload)
            }

            ServerMessage::MediaChunk {
                stream_id,
                // Passed through, never renumbered, for the reason
                // `audio_listen` records: a gap is exactly how much was lost.
                seq,
                pts_us,
                data,
            } => {
                if self.mirrored != Some(*stream_id) {
                    return Ok(Step::Continue);
                }
                self.bytes += data.len() as u64;
                self.chunks += 1;
                if !self.framed {
                    return write_out(out, data);
                }
                // One picture per chunk, so the flag is the picture's. A
                // player joining mid-stream waits for the first one set.
                let flags = if access_unit_has_idr(data) {
                    WEB_FRAME_FLAG_KEY
                } else {
                    0
                };
                self.write_frame(
                    out,
                    WebFrameKind::Chunk,
                    *stream_id,
                    flags,
                    *seq,
                    *pts_us,
                    data,
                )
            }

            ServerMessage::MediaClose {
                stream_id,
                reason,
                detail,
            } => {
                if self.mirrored != Some(*stream_id) {
                    return Ok(Step::Continue);
                }
                self.mirrored = None;
                let (web_reason, code) = match reason {
                    MediaCloseReason::Ended => {
                        eprintln!("herdr: stream ended");
                        (WebCloseReason::Ended, 0)
                    }
                    MediaCloseReason::Failed => {
                        eprintln!("herdr: stream failed: {detail}");
                        (WebCloseReason::Failed, 1)
                    }
                };
                if self.framed {
                    let payload = close_payload(web_reason, detail);
                    if let Step::Ended(gone) =
                        self.write_frame(out, WebFrameKind::Close, *stream_id, 0, 0, 0, &payload)?
                    {
                        return Ok(Step::Ended(gone));
                    }
                }
                Ok(Step::Ended(code))
            }

            _ => Ok(Step::Continue),
        }
    }

    /// Encodes one frame and writes it.
    ///
    /// A frame the encoder refuses costs that frame and not the stream:
    /// nothing is written, so the bytes already out still describe whole
    /// frames.
    #[allow(clippy::too_many_arguments)]
    fn write_frame(
        &mut self,
        out: &mut impl Write,
        kind: WebFrameKind,
        stream_id: u32,
        flags: u8,
        seq: u64,
        pts_us: u64,
        payload: &[u8],
    ) -> io::Result<Step> {
        self.scratch.clear();
        if let Err(err) = (WebFrame {
            kind,
            flags,
            stream_id,
            seq,
            pts_us,
            payload,
        })
        .encode_into(&mut self.scratch)
        {
            eprintln!("herdr: dropped a video frame: {err}");
            return Ok(Step::Continue);
        }
        write_out(out, &self.scratch)
    }
}

/// Writes bytes to the consumer. A closed pipe is an ordinary end: the
/// consumer decided, and the exit code says so.
fn write_out(out: &mut impl Write, bytes: &[u8]) -> io::Result<Step> {
    match out.write_all(bytes).and_then(|()| out.flush()) {
        Ok(()) => Ok(Step::Continue),
        Err(err) if err.kind() == io::ErrorKind::BrokenPipe => Ok(Step::Ended(0)),
        Err(err) => Err(err),
    }
}

/// The number this whole slice exists to produce, on stderr so the pipe stays
/// clean.
fn report(
    args: &VideoListenArgs,
    forwarder: &Forwarder,
    started: std::time::Instant,
    final_: bool,
) {
    if !args.meter {
        return;
    }
    let (bytes, chunks) = (forwarder.bytes(), forwarder.chunks());
    let secs = started.elapsed().as_secs_f64().max(0.001);
    let mbit = (bytes as f64) * 8.0 / secs / 1e6;
    let label = if final_ { "total" } else { "so far" };
    eprintln!(
        "herdr: {label} {bytes} B in {chunks} chunks over {secs:.1}s = {mbit:.2} Mbit/s \
         (kitty path measured 169-188 Mbit/s for the same picture)"
    );
}

#[cfg(test)]
mod framed_tests {
    use super::*;
    use crate::media::web_frame::{
        WEB_FRAME_FLAG_KEY, WEB_FRAME_HEADER_BYTES, WEB_FRAME_MAGIC, WEB_FRAME_VERSION,
    };
    use crate::server::headless::mpegts::test_stream::access_unit;

    fn open_for(pane: &str) -> ServerMessage {
        ServerMessage::MediaOpen {
            stream_id: 9,
            pane_id: pane.to_owned(),
            codec: codec::H264.to_owned(),
            params: MediaParams::Video {
                width: 480,
                height: 270,
                fps: 30,
            },
            target_latency_us: 100_000,
        }
    }

    fn chunk(seq: u64, pts_us: u64, data: Vec<u8>) -> ServerMessage {
        ServerMessage::MediaChunk {
            stream_id: 9,
            seq,
            pts_us,
            data,
        }
    }

    fn close() -> ServerMessage {
        ServerMessage::MediaClose {
            stream_id: 9,
            reason: MediaCloseReason::Ended,
            detail: String::new(),
        }
    }

    struct Frame {
        kind: u8,
        flags: u8,
        stream_id: u32,
        seq: u64,
        pts_us: u64,
        payload: Vec<u8>,
    }

    /// Reads frames back the way the web decoder does.
    fn frames(mut bytes: &[u8]) -> Vec<Frame> {
        let mut out = Vec::new();
        while !bytes.is_empty() {
            assert!(bytes.len() >= WEB_FRAME_HEADER_BYTES, "a whole header");
            assert_eq!(bytes[0], WEB_FRAME_MAGIC);
            assert_eq!(bytes[1], WEB_FRAME_VERSION);
            let u32_at = |o: usize| u32::from_le_bytes(bytes[o..o + 4].try_into().unwrap());
            let u64_at = |o: usize| u64::from_le_bytes(bytes[o..o + 8].try_into().unwrap());
            let len = u32_at(24) as usize;
            let end = WEB_FRAME_HEADER_BYTES + len;
            out.push(Frame {
                kind: bytes[2],
                flags: bytes[3],
                stream_id: u32_at(4),
                seq: u64_at(8),
                pts_us: u64_at(16),
                payload: bytes[WEB_FRAME_HEADER_BYTES..end].to_vec(),
            });
            bytes = &bytes[end..];
        }
        out
    }

    fn forward_all(forwarder: &mut Forwarder, messages: &[ServerMessage]) -> Vec<u8> {
        let mut out = Vec::new();
        for message in messages {
            let step = forwarder
                .forward(message, &mut out)
                .expect("writes to a vec");
            assert!(matches!(step, Step::Continue), "the stream goes on");
        }
        out
    }

    /// TP-WIRE-K6-01 · ⭐ framed output is the web frame the audio listener
    /// already speaks: a video open, chunks with their numbers and moments,
    /// a close — one format, one demux.
    #[test]
    fn framed_output_is_web_frames_with_a_video_open() {
        let key = access_unit(true, b"idr");
        let delta = access_unit(false, b"p");
        let mut forwarder = Forwarder::new("w1:p3".to_owned(), true);
        let mut out = forward_all(
            &mut forwarder,
            &[
                open_for("w1:p3"),
                chunk(0, 1_000, key.clone()),
                chunk(1, 34_333, delta.clone()),
            ],
        );
        let step = forwarder
            .forward(&close(), &mut out)
            .expect("writes to a vec");
        assert_eq!(
            step,
            Step::Ended(0),
            "a close is the end, and an ended stream exits 0"
        );
        let got = frames(&out);
        assert_eq!(got.len(), 4, "open, two chunks, close");

        let open = &got[0];
        assert_eq!(
            (open.kind, open.stream_id, open.seq, open.pts_us),
            (1, 9, 0, 0)
        );
        let body: serde_json::Value = serde_json::from_slice(&open.payload).expect("json");
        assert_eq!(body["pane_id"], "w1:p3");
        assert_eq!(body["codec"], "h264");
        assert_eq!(body["width"], 480);
        assert_eq!(body["height"], 270);
        assert_eq!(body["fps"], 30);
        assert_eq!(body["target_latency_us"], 100_000);

        assert_eq!(
            (got[1].kind, got[1].stream_id, got[1].seq, got[1].pts_us),
            (0, 9, 0, 1_000)
        );
        assert_ne!(
            got[1].flags & WEB_FRAME_FLAG_KEY,
            0,
            "an IDR picture is flagged key"
        );
        assert_eq!(got[1].payload, key, "the picture travels untouched");

        assert_eq!((got[2].kind, got[2].seq, got[2].pts_us), (0, 1, 34_333));
        assert_eq!(got[2].flags & WEB_FRAME_FLAG_KEY, 0, "a P picture is not");
        assert_eq!(got[2].payload, delta);

        assert_eq!(got[3].kind, 2);
        let body: serde_json::Value = serde_json::from_slice(&got[3].payload).expect("json");
        assert_eq!(body["reason"], "ended");
    }

    /// TP-WIRE-K6-01 · a stream for another pane, or in another codec, writes
    /// nothing — and neither does a chunk for a stream that was never opened.
    #[test]
    fn a_stream_that_is_not_this_panes_writes_nothing() {
        let mut forwarder = Forwarder::new("w1:p3".to_owned(), true);
        let out = forward_all(
            &mut forwarder,
            &[
                open_for("w1:p9"),
                chunk(0, 1, access_unit(true, b"x")),
                ServerMessage::MediaOpen {
                    stream_id: 9,
                    pane_id: "w1:p3".to_owned(),
                    codec: codec::OPUS.to_owned(),
                    params: MediaParams::Audio {
                        sample_rate_hz: 48_000,
                        channels: 2,
                    },
                    target_latency_us: 100_000,
                },
                chunk(1, 2, access_unit(true, b"y")),
            ],
        );
        assert!(
            out.is_empty(),
            "nothing of another pane's or another codec's: {out:?}"
        );
    }

    /// TP-WIRE-K6-01 · without `--framed` the bytes are the picture and
    /// nothing else: the decoder-pipe contract the module states, kept.
    #[test]
    fn raw_output_is_the_picture_bytes_and_nothing_else() {
        let key = access_unit(true, b"idr");
        let delta = access_unit(false, b"p");
        let mut forwarder = Forwarder::new("w1:p3".to_owned(), false);
        let out = forward_all(
            &mut forwarder,
            &[
                open_for("w1:p3"),
                chunk(0, 1_000, key.clone()),
                chunk(1, 2_000, delta.clone()),
            ],
        );
        let mut expected = key;
        expected.extend(delta);
        assert_eq!(
            out, expected,
            "raw is the concatenated pictures, no header of any kind"
        );
    }

    /// TP-WIRE-K6-01 · a close is the end of the mirror for that stream: a
    /// chunk that arrives after it is not written, and a stream that ended
    /// normally ends the process with success.
    #[test]
    fn a_close_ends_the_mirror_and_says_how() {
        let mut forwarder = Forwarder::new("w1:p3".to_owned(), true);
        let mut out = Vec::new();
        forwarder.forward(&open_for("w1:p3"), &mut out).unwrap();
        let step = forwarder.forward(&close(), &mut out).unwrap();
        assert!(matches!(step, Step::Ended(0)), "an ended stream exits 0");
        let before = out.len();
        forwarder
            .forward(&chunk(5, 5, access_unit(false, b"late")), &mut out)
            .unwrap();
        assert_eq!(out.len(), before, "a chunk after the close is not written");
        let failed = ServerMessage::MediaClose {
            stream_id: 9,
            reason: MediaCloseReason::Failed,
            detail: "encoder died".to_owned(),
        };
        let mut forwarder = Forwarder::new("w1:p3".to_owned(), true);
        forwarder.forward(&open_for("w1:p3"), &mut out).unwrap();
        let step = forwarder.forward(&failed, &mut out).unwrap();
        assert!(matches!(step, Step::Ended(1)), "a failed stream exits 1");
    }
}
