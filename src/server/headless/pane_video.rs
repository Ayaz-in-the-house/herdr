//! Offering one pane's pictures to whoever can decode H.264.
//!
//! The tap is `pane_graphics`: the layer a pane's program already pushes is the
//! same picture the kitty path spends 169-188 Mbit/s carrying, so the video arm
//! reads it rather than capturing a second copy. Two answers to the same
//! question would eventually disagree, and the one nobody is watching would be
//! the one that drifts.
//!
//! ## Only `Backing::Inline`, and the reason is not "remote"
//!
//! A layer arrives one of three ways. `Inline` holds the bytes; `Direct` holds
//! a file lease the client reads for itself; `Resident` holds nothing
//! server-side because the client already has the picture.
//!
//! Encoding keys on `Inline` rather than on the connection being remote, and
//! the distinction matters more than it first looks. `Inline` is not a synonym
//! for remote: it is what the layer becomes whenever the cheap path is
//! unavailable, and `direct_graphics_available` requires **exactly one** full
//! app client, in the foreground, that negotiated direct graphics. So a second
//! local client — a second window, a second display — drops a purely local
//! session onto the inline path too, where every frame is deflated, base64'd
//! and pushed down a pty exactly as it would be over ssh.
//!
//! So the rule is not "encode when remote". It is **encode when the expensive
//! path is in use**, which is the same question the backing already answers.
//! `Direct` and `Resident` mean the client reads the picture without the wire
//! paying for it, and encoding those would spend a GPU session to save nothing
//! while adding a lossy round trip and the encoder's own pipeline delay.
//!
//! ## Attaching is an event
//!
//! An open is sent once per viewer, not once per stream. Every tick takes
//! the difference between the viewers that can decode and the viewers each
//! stream has already been announced to, and tells the rest — on the control
//! lane, before that tick's chunks. The first viewer and a late one are the
//! same case: an audience that does not yet contain them. A viewer that is
//! only ever told at `VideoEvent::Opened` is deaf if it arrives a moment
//! later, and that was measured before it was fixed: 0 B for the late
//! viewer against 152,379 B on a clean server. TP-VIDEO-LATE-01
//!
//! TP-VIDEO-TAP-01.

use std::collections::BTreeSet;

use super::*;
use crate::app::pane_graphics::Backing;
use crate::protocol::{capability, codec, MediaCloseReason, MediaParams};
use crate::server::client_transport::WriteLane;
use crate::server::headless::pane_video_capture::VideoEvent;

/// The frame rate the encoder is configured for.
///
/// Nominal, and `MediaParams::Video` says so: the source pushes frames when it
/// has them, not on a clock. Real timing rides `MediaChunk::pts_us`, which is
/// the only clock A/V sync rests on. This number tells the encoder how to size
/// its rate control and its key-frame interval, nothing more.
const NOMINAL_FPS: u32 = 30;

/// What the video chain has lost so far, stage by stage.
///
/// One value rather than seven fields so the report can only ever say all of
/// them or none: read separately, each looks like "the video stutters" and any
/// of a dozen causes; read together they name the stage.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct VideoDropReport {
    /// The producer showed pictures faster than the tick drained them.
    pub(super) before_encoder: u64,
    /// The tick handed pictures over faster than the encoder took them.
    pub(super) at_encoder: u64,
    /// The encoder produced output faster than the loop collected it.
    pub(super) after_encoder: u64,
    /// A viewer's media lane was full, so the chunk never left the server.
    pub(super) at_client: u64,
    /// A picture came back with no stamp waiting for it. TP-VIDEO-PTS-01
    pub(super) unstamped: u64,
    /// A picture was handed over one picture late: its PES had no length.
    pub(super) late_transport: u64,
    /// Bytes the reader could not read as pictures: cut short, or resynced.
    pub(super) broken_transport: u64,
}

/// One open stream: what a viewer has to be told to route its chunks, and
/// which viewers have been told.
///
/// The parameters are kept so a viewer arriving after the open is told the
/// same thing the first one was; the audience is kept so nobody is told
/// twice — a second open for a pane a client already mirrors reads as
/// "stream restarted" and re-initialises its decoder. TP-VIDEO-LATE-01
pub(super) struct VideoStream {
    stream_id: u32,
    width: u32,
    height: u32,
    fps: u32,
    /// Clients that have received this stream's `MediaOpen`. Chunks and the
    /// close go to exactly these.
    announced: BTreeSet<u64>,
    /// The next chunk's number. Per stream, from zero: `seq` means "a gap is
    /// a loss", and a counter shared across panes made every other pane's
    /// picture look like this pane's loss. TP-VIDEO-SEQ-01
    next_seq: u64,
}

impl HeadlessServer {
    /// Clients that can decode H.264, may be offered media, and can be written
    /// to.
    ///
    /// Not `is_full_app_client()`, for the reason `can_receive_media` records:
    /// hearing and owning the screen are different rights, and a viewer asks
    /// for one.
    pub(super) fn video_capable_clients(&self) -> Vec<u64> {
        let mut capable: Vec<u64> = self
            .clients
            .iter()
            .filter(|(_, client)| {
                client.writer.is_some()
                    && client.can_receive_media()
                    && client
                        .capabilities
                        .values_of(capability::VIDEO_DECODE)
                        .iter()
                        .any(|value| value == codec::H264)
            })
            .map(|(id, _)| *id)
            .collect();
        capable.sort_unstable();
        capable
    }

    /// Start, feed and stop pane encoders, then fan out whatever they produced.
    ///
    /// Called from the same place the audio tick is called, and shaped the same
    /// way: nothing here runs while nobody is watching, and the first thing it
    /// does when the last viewer leaves is stop everything.
    pub(super) fn tick_pane_video(&mut self) {
        let viewers = self.video_capable_clients();
        // Told before anything else, so a producer writing a picture in the
        // next moment knows whether to keep it. Off empties the queues, which
        // is why the branch below can return without sweeping them.
        // TP-VIDEO-FEED-02
        self.app.pane_graphics.set_video_wanted(!viewers.is_empty());
        if viewers.is_empty() {
            if self.pane_video.encoding_count() > 0 {
                tracing::debug!("last video viewer left; stopping encoders");
                self.pane_video.stop_all();
                self.pane_video_streams.clear();
            }
            return;
        }

        // Which panes still have a picture at all — the question the stale
        // sweep asks, and the only one the slots still answer for video. What
        // was *shown*, in order, comes from the queue below. TP-VIDEO-FEED-01
        let mut live: Vec<String> = Vec::new();
        for ((pane_id, _layer_id), slot) in &self.app.pane_graphics.slots {
            let Some(layer) = slot.layer.as_ref() else {
                continue;
            };
            if !matches!(layer.backing, Backing::Inline(_)) {
                // Direct and Resident are the local paths. See the module note:
                // locally there is no wire cost to encode away. TP-VIDEO-TAP-01
                continue;
            }
            let Some(public) = self.app.public_pane_id(0, *pane_id) else {
                continue;
            };
            live.push(public);
        }

        // A pane whose layer went away stops encoding. Left running it would
        // hold a GPU session for a picture that no longer exists.
        let stale: Vec<String> = self
            .pane_video_streams
            .keys()
            .filter(|pane| !live.contains(pane))
            .cloned()
            .collect();
        for pane in stale {
            self.pane_video.stop(&pane);
            if let Some(stream) = self.pane_video_streams.remove(&pane) {
                self.fan_out_video_close(&stream, MediaCloseReason::Ended, String::new());
            }
        }

        // Whoever arrived since the last tick is told about every stream
        // already open — before this tick's chunks, so nothing reaches a
        // viewer it cannot route. TP-VIDEO-LATE-01
        self.announce_video_streams(&viewers);

        // Every picture the producer pushed since the last tick, in order.
        // Reading the slot instead would read only the last one, which is
        // what the slot is *for* — and is why nine frames in ten never
        // reached the encoder. E-118 · TP-VIDEO-FEED-01
        //
        // Drained whole and offered only for panes still holding a picture.
        // The queue is drained either way, because a pane that stopped
        // drawing must not leave its last frames to be found by a later tick
        // and start an encoder for a stream that just closed.
        for (key, frame) in self.app.pane_graphics.take_video_frames() {
            let Some(pane) = self.app.public_pane_id(0, key.0) else {
                continue;
            };
            if !live.contains(&pane) {
                continue;
            }
            self.pane_video
                .start(&pane, frame.image_width, frame.image_height, NOMINAL_FPS);
            // The picture's own moment, taken where it was recorded; the
            // tick carries it and never takes it. TP-VIDEO-PTS-03
            self.pane_video.offer_at(&pane, &frame.rgba, frame.pts_us);
        }

        self.deliver_pane_video();
    }

    /// What the video wake runs: the whole tick, because a wake comes from
    /// either end — a picture recorded for the encoder, or a picture back
    /// from it — and both are what the tick does. TP-VIDEO-PTS-06
    pub(super) fn on_pane_video_wake(&mut self) {
        self.tick_pane_video();
    }

    /// Fans out whatever the encoders produced, and says what was lost.
    ///
    /// Called from the tick and from the loop's wake: a reader thread that
    /// queued a picture wakes the loop, and the loop comes here rather than
    /// waiting for its timer. TP-VIDEO-PTS-02
    pub(super) fn deliver_pane_video(&mut self) {
        for event in self.pane_video.drain() {
            self.deliver_video_event(event);
        }
        self.report_video_drops();
    }

    /// Report what the video chain is losing, and where.
    ///
    /// Three counters, one line, because they are three *different*
    /// bottlenecks on one chain and the useful reading is which of them is
    /// climbing:
    ///
    /// - `before_encoder` — the producer showed pictures faster than the tick
    ///   drained them, so the oldest were dropped from the pane's queue.
    /// - `at_encoder` — the tick handed pictures over faster than the encoder
    ///   took them.
    /// - `after_encoder` — the encoder produced output faster than the loop
    ///   collected it.
    /// - `at_client` — a viewer's media lane was full, so the chunk never left
    ///   the server. This is the stage a slow link shows up in, and the one
    ///   the other three cannot distinguish themselves from.
    ///
    /// Read separately, each one looks like "the video stutters" and any of a
    /// dozen causes; read together they name the stage. Said once per change
    /// rather than once per drop: the counts climb on every drop, and keying
    /// the line on the count itself would turn a busy loop into a busy log —
    /// the reason `CaptureReport` carries a flag rather than a number on the
    /// audio side. TP-VIDEO-TAP-01 · TP-VIDEO-WRITE-02
    fn report_video_drops(&mut self) {
        let now = VideoDropReport {
            before_encoder: self.app.pane_graphics.video_frames_dropped(),
            at_encoder: self.pane_video.dropped_frames(),
            after_encoder: self.pane_video.dropped_chunks(),
            at_client: self.pane_video_lane_dropped,
            unstamped: self.pane_video.unstamped_pictures(),
            late_transport: self.pane_video.late_pictures(),
            broken_transport: self.pane_video.broken_transport(),
        };
        if now == self.pane_video_drops_reported {
            return;
        }
        self.pane_video_drops_reported = now;
        tracing::debug!(
            before_encoder = now.before_encoder,
            at_encoder = now.at_encoder,
            after_encoder = now.after_encoder,
            at_client = now.at_client,
            unstamped = now.unstamped,
            late_transport = now.late_transport,
            broken_transport = now.broken_transport,
            "video is dropping; the stage with the climbing count is the one that is behind"
        );
    }

    pub(super) fn deliver_video_event(&mut self, event: VideoEvent) {
        match event {
            VideoEvent::Opened {
                pane_id,
                width,
                height,
                fps,
                chunk,
                pts_us,
            } => {
                let stream_id = self.next_video_stream_id;
                self.next_video_stream_id = self.next_video_stream_id.wrapping_add(1).max(1);
                self.pane_video_streams.insert(
                    pane_id.clone(),
                    VideoStream {
                        stream_id,
                        width,
                        height,
                        fps,
                        announced: BTreeSet::new(),
                        next_seq: 0,
                    },
                );
                // The first viewers are the late ones' special case: the
                // audience is empty, so everyone capable is told.
                let viewers = self.video_capable_clients();
                self.announce_video_streams(&viewers);
                self.fan_out_video_chunk(&pane_id, chunk, pts_us);
            }
            VideoEvent::Chunk {
                pane_id,
                chunk,
                pts_us,
            } => {
                self.fan_out_video_chunk(&pane_id, chunk, pts_us);
            }
            VideoEvent::Ended { pane_id, detail } => {
                let Some(stream) = self.pane_video_streams.remove(&pane_id) else {
                    return;
                };
                let reason = if detail.is_empty() {
                    MediaCloseReason::Ended
                } else {
                    MediaCloseReason::Failed
                };
                self.fan_out_video_close(&stream, reason, detail);
            }
        }
    }

    /// Tell every viewer about every open stream it has not been told about.
    ///
    /// One path for the first viewer and a late one: the open that follows
    /// `VideoEvent::Opened` is this same difference, taken while the
    /// audience is still empty. A viewer is told once; the audience is what
    /// chunks and the close are addressed to from then on. TP-VIDEO-LATE-01
    fn announce_video_streams(&mut self, viewers: &[u64]) {
        let mut pending: Vec<(String, ServerMessage, Vec<u64>)> = Vec::new();
        for (pane_id, stream) in &self.pane_video_streams {
            let newcomers: Vec<u64> = viewers
                .iter()
                .copied()
                .filter(|client_id| !stream.announced.contains(client_id))
                .collect();
            if newcomers.is_empty() {
                continue;
            }
            let message = ServerMessage::MediaOpen {
                stream_id: stream.stream_id,
                pane_id: pane_id.clone(),
                codec: codec::H264.to_owned(),
                params: MediaParams::Video {
                    width: stream.width,
                    height: stream.height,
                    fps: stream.fps,
                },
                target_latency_us: crate::app::pane_audio::TARGET_LATENCY_US,
            };
            pending.push((pane_id.clone(), message, newcomers));
        }
        for (pane_id, message, newcomers) in pending {
            let told = self.send_video_message(&newcomers, &message, WriteLane::Control);
            if let Some(stream) = self.pane_video_streams.get_mut(&pane_id) {
                stream.announced.extend(told);
            }
        }
    }

    /// A client that is gone is forgotten by every stream, so the record of
    /// who was told cannot outlive the client it describes: the same id
    /// arriving again is a new viewer, and is told again. TP-VIDEO-LATE-01
    pub(super) fn forget_video_viewer(&mut self, client_id: u64) {
        for stream in self.pane_video_streams.values_mut() {
            stream.announced.remove(&client_id);
        }
    }

    /// Chunks ride the media lane, where a full queue is a drop.
    ///
    /// The opposite of the geometry decision, and for the opposite reason: a
    /// lost rectangle leaves a player permanently in the wrong place, while a
    /// lost picture is one frame the decoder conceals. Making video reliable
    /// would let a slow link turn into accumulated delay, which is the failure
    /// the audio side already measured and refused.
    ///
    /// `pts_us` is the picture's own moment, stamped where the picture was
    /// handed to the encoder; it is carried here, never taken here — a stamp
    /// taken at fan-out is the loop's moment, not the picture's.
    /// TP-VIDEO-PTS-01
    fn fan_out_video_chunk(&mut self, pane_id: &str, data: Vec<u8>, pts_us: u64) {
        let Some(stream) = self.pane_video_streams.get_mut(pane_id) else {
            return;
        };
        let stream_id = stream.stream_id;
        let audience: Vec<u64> = stream.announced.iter().copied().collect();
        let seq = stream.next_seq;
        stream.next_seq = stream.next_seq.wrapping_add(1);
        let message = ServerMessage::MediaChunk {
            stream_id,
            seq,
            pts_us,
            data,
        };
        self.send_video_message(&audience, &message, WriteLane::Media);
    }

    fn fan_out_video_close(
        &mut self,
        stream: &VideoStream,
        reason: MediaCloseReason,
        detail: String,
    ) {
        let audience: Vec<u64> = stream.announced.iter().copied().collect();
        let message = ServerMessage::MediaClose {
            stream_id: stream.stream_id,
            reason,
            detail,
        };
        self.send_video_message(&audience, &message, WriteLane::Control);
    }

    /// Frame `message` once and write it to each of `recipients` that still
    /// has a writer, on `lane`. Returns the recipients it reached.
    fn send_video_message(
        &mut self,
        recipients: &[u64],
        message: &ServerMessage,
        lane: WriteLane,
    ) -> Vec<u64> {
        let Ok(framed) = Self::frame_server_message(message) else {
            return Vec::new();
        };
        // Counted locally and added after the loop: the writers are borrowed
        // out of `self.clients` for its duration.
        let mut refused = 0_u64;
        let mut reached = Vec::with_capacity(recipients.len());
        for &client_id in recipients {
            let Some(writer) = self
                .clients
                .get(&client_id)
                .and_then(|client| client.writer.as_ref())
            else {
                continue;
            };
            let sent = match lane {
                WriteLane::Media => {
                    // Refusing is the lane's design — an unbounded media lane
                    // turns a slow link into a delay that never recovers. But
                    // the lane's own contract puts the answer here, "where it
                    // still knows the chunk's deadline", and answering with
                    // `let _ =` drops the chunk and the fact together.
                    // TP-VIDEO-WRITE-02
                    let sent = writer.media.try_send(framed.clone()).is_ok();
                    if !sent {
                        refused += 1;
                    }
                    sent
                }
                _ => writer.control.send(framed.clone()).is_ok(),
            };
            if sent {
                reached.push(client_id);
            }
        }
        self.pane_video_lane_dropped += refused;
        reached
    }
}
