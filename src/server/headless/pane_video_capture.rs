//! Turning one pane's pictures into H.264, while somebody is watching.
//!
//! The encoder is a child `ffmpeg` process rather than a linked library, and
//! that is a decision about *this slice*, not about the shipped product. The
//! slice exists to settle one measured question — whether the 84x gap between
//! what herdr sends and what the same picture costs as H.264 is real end to
//! end — and a pipe answers it with no new dependency, no build change, and no
//! LGPL/GPL link-time question. Whichever way that eventually settles, only
//! this module should have to change.
//!
//! Three policies are inherited from `pane_audio_capture` rather than
//! reinvented, because each was paid for once already:
//!
//! Nothing starts before a listener exists. An encoder nobody asked for looks
//! exactly like an encoder somebody did, except that it burns a GPU session and
//! a core while the desk is empty.
//!
//! The chunk channel is **bounded and drops**. A blocking send would back up
//! into ffmpeg's pipe, and what comes back after that is not a gap but video
//! with accumulated delay — worse than a dropped frame, and invisible until a
//! user says the picture is behind.
//!
//! A reader thread is never joined from the server loop. Joining is waiting.
//!
//! ## And the same in the other direction
//!
//! Pictures go *in* on a thread too, behind a bounded channel that drops. The
//! encoder's input is a pipe with a 64 KB kernel buffer and one 320x180 RGBA
//! picture is 230 KB, so a write from the server loop blocks until the encoder
//! reads — and the server loop is the whole product: a loop waiting on ffmpeg
//! is every pane frozen, not just the video.
//!
//! This was invisible until 2026-09-01 because only nine frames in a hundred
//! ever reached the encoder (E-118): the pipe was always empty, so the write
//! always returned. Fixing the feed removed the mask, which is why both live
//! in one change. TP-VIDEO-WRITE-01.
//!
//! TP-VIDEO-SUPERVISE-01.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::process::{Child, ChildStdin, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TryRecvError, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use super::mpegts::TransportDemux;

/// How many encoded chunks may wait for the server loop before one is dropped.
///
/// Sixteen at the chunk size below is roughly a second of a well-behaved
/// stream: long enough to ride out a loop busy drawing, short enough that a
/// viewer never watches the queue instead of the video.
const CHUNK_QUEUE: usize = 16;

/// How much is read from the encoder at a time.
///
/// A read size and nothing more. Pictures are cut by the transport, not by
/// the read: the encoder writes MPEG-TS with each picture's length in its
/// PES, and `mpegts::TransportDemux` hands over one whole access unit the
/// moment its last byte arrives, whatever the pipe returned. That is what
/// lets a timestamp mean one picture. TP-VIDEO-AU-01
const CHUNK_BYTES: usize = 32 * 1024;

/// How many pictures may wait for the encoder before one is dropped.
///
/// Small on purpose. Anything waiting here is latency a viewer sees, and the
/// producer already holds its own bounded queue upstream: two buffers deep
/// enough to hide a stall would add their delays together. Four is enough to
/// ride out a scheduling hiccup and not enough to watch.
const FRAME_QUEUE: usize = 4;

/// The encoder to ask ffmpeg for.
///
/// Measured on this machine rather than assumed: `h264_nvenc` encodes,
/// `h264_vaapi` fails on both render nodes ("no usable encoding entrypoint"),
/// and `av1_nvenc` has no capable device. An ffmpeg build listing an encoder
/// says what it was compiled with, never what the hardware will do.
const ENCODER: &str = "h264_nvenc";

/// What the server loop should do about a pane's video, in order.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum VideoEvent {
    /// The pane's first encoded chunk. The stream opens *now*, with this chunk
    /// in hand, for the reason the audio side records: a timestamp counts from
    /// the open, so opening earlier and waiting makes every chunk late.
    Opened {
        pane_id: String,
        width: u32,
        height: u32,
        fps: u32,
        /// One whole picture.
        chunk: Vec<u8>,
        /// When that picture was shown — stamped at hand-off, carried through
        /// the encoder picture for picture. TP-VIDEO-PTS-01
        pts_us: u64,
    },
    /// A further picture for a pane whose stream is already open.
    Chunk {
        pane_id: String,
        chunk: Vec<u8>,
        pts_us: u64,
    },
    /// The encoder ended — finished, refused to start, or was told to stop.
    ///
    /// `detail` is empty for a normal end. A consumer tells *ended* from
    /// *crashed* by that difference and its respawn decision hangs on it, so an
    /// encoder that never started reports here rather than staying silent.
    Ended { pane_id: String, detail: String },
}

/// How an encoder is started, behind a boxed function so a test can hand the
/// supervisor a process it wrote. The shipped one spawns ffmpeg; asserting
/// against the real one would be asserting about the machine.
type SpawnFn = Box<dyn Fn(u32, u32, u32) -> Result<EncoderProcess, String> + Send>;

/// A running encoder: somewhere to write pictures, somewhere to read H.264.
pub(crate) struct EncoderProcess {
    pub(crate) child: Option<Child>,
    pub(crate) stdin: Box<dyn Write + Send>,
    pub(crate) stdout: Box<dyn Read + Send>,
}

struct PaneEncoder {
    /// Where pictures are handed off. Dropping it is how the writer thread is
    /// told to finish, which closes the encoder's input in turn.
    frames: Option<SyncSender<(u64, Vec<u8>)>>,
    /// Pictures back from the encoder, each with the stamp of the picture
    /// that went in.
    chunks: Receiver<(u64, Vec<u8>)>,
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
    child: Option<Child>,
    width: u32,
    height: u32,
    fps: u32,
    opened: bool,
    /// Set when writing a picture failed. The encoder is finished at that
    /// point; the loop learns on its next drain rather than mid-write.
    ///
    /// Shared, because the write now happens on a thread: a failure that could
    /// not travel back would leave a consumer unable to tell *ended* from
    /// *crashed*, and its respawn decision hangs on exactly that.
    /// TP-VIDEO-WRITE-03
    broken: Arc<Mutex<Option<String>>>,
    /// This encoder's share of the supervisor's frame-drop counter.
    dropped_frames: Arc<AtomicU64>,
}

impl PaneEncoder {
    fn broken_reason(&self) -> Option<String> {
        self.broken
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .clone()
    }

    fn set_broken(&self, detail: String) {
        let mut slot = self.broken.lock().unwrap_or_else(|err| err.into_inner());
        if slot.is_none() {
            *slot = Some(detail);
        }
    }
}

pub(crate) struct VideoSupervisor {
    spawn: SpawnFn,
    panes: BTreeMap<String, PaneEncoder>,
    dropped: Arc<AtomicU64>,
    dropped_frames: Arc<AtomicU64>,
    /// Signalled by a reader thread when it queues a picture, so the loop
    /// delivers it on the picture rather than on its next timer.
    /// TP-VIDEO-PTS-02
    wake: Option<Arc<tokio::sync::Notify>>,
    /// Pictures that came back with no stamp waiting for them.
    unstamped: Arc<AtomicU64>,
    /// Pictures whose PES carried no length, so they were handed over one
    /// picture late — the delay the transport was chosen to avoid.
    late: Arc<AtomicU64>,
    /// Pictures cut short and packet boundaries lost, summed: bytes the
    /// reader could not read as pictures.
    broken: Arc<AtomicU64>,
}

impl std::fmt::Debug for VideoSupervisor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VideoSupervisor")
            .field("panes", &self.panes.len())
            .field("dropped", &self.dropped.load(Ordering::Relaxed))
            .field(
                "dropped_frames",
                &self.dropped_frames.load(Ordering::Relaxed),
            )
            .finish()
    }
}

impl VideoSupervisor {
    pub(crate) fn new() -> Self {
        Self::with_spawn(Box::new(spawn_ffmpeg))
    }

    pub(crate) fn with_spawn(spawn: SpawnFn) -> Self {
        Self {
            spawn,
            panes: BTreeMap::new(),
            dropped: Arc::new(AtomicU64::new(0)),
            dropped_frames: Arc::new(AtomicU64::new(0)),
            wake: None,
            unstamped: Arc::new(AtomicU64::new(0)),
            late: Arc::new(AtomicU64::new(0)),
            broken: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Wires the supervisor to the loop's wake, so a queued picture is
    /// delivered on the picture rather than on the timer. TP-VIDEO-PTS-02
    pub(crate) fn wake_on(mut self, wake: Arc<tokio::sync::Notify>) -> Self {
        self.wake = Some(wake);
        self
    }

    /// Whether this supervisor wakes exactly `handle`, and not merely some
    /// look-alike. A test's question; production only ever wires it.
    #[cfg(test)]
    pub(crate) fn wakes(&self, handle: &Arc<tokio::sync::Notify>) -> bool {
        self.wake
            .as_ref()
            .is_some_and(|wake| Arc::ptr_eq(wake, handle))
    }

    /// Swap the encoder factory. Tests only, and the reason is the module's
    /// own rule: asserting against real ffmpeg would be asserting about the
    /// machine, and the machine is not what any of these tests are about.
    #[cfg(test)]
    pub(crate) fn set_spawn_for_test(&mut self, spawn: SpawnFn) {
        self.stop_all();
        self.spawn = spawn;
    }

    /// Test-only: production drives the encoders through `tick_pane_video`
    /// and reads their state from the events they produce, never by asking.
    /// Marked rather than left public so the compiler keeps saying so.
    #[cfg(test)]
    pub(crate) fn is_encoding(&self, pane_id: &str) -> bool {
        self.panes.contains_key(pane_id)
    }

    pub(crate) fn encoding_count(&self) -> usize {
        self.panes.len()
    }

    /// How many encoded chunks were dropped because the loop was not draining.
    ///
    /// Reported rather than merely counted: dropping is the correct policy —
    /// a blocking send returns video with accumulated delay instead of a gap —
    /// but a policy that never says it fired is indistinguishable from a bug,
    /// and the symptom (a picture that stutters under load) has a dozen other
    /// candidate causes.
    pub(crate) fn dropped_chunks(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Pictures that came back from an encoder with no stamp waiting for
    /// them, and were stamped on arrival instead.
    ///
    /// Zero while one picture in is one picture out, which is what the
    /// encoder is asked for (`-fps_mode passthrough`, no B-frames). A count
    /// climbing here means that pairing broke, and every stamp after it is
    /// one picture off. TP-VIDEO-PTS-01
    pub(crate) fn unstamped_pictures(&self) -> u64 {
        self.unstamped.load(Ordering::Relaxed)
    }

    /// Pictures handed over one picture late because their PES carried no
    /// length. Zero while the encoder is asked for lengths; a count here is
    /// the one-frame delay of E-146 coming back. TP-VIDEO-AU-01
    pub(crate) fn late_pictures(&self) -> u64 {
        self.late.load(Ordering::Relaxed)
    }

    /// Pictures cut short plus packet boundaries lost. Zero on a healthy pipe.
    pub(crate) fn broken_transport(&self) -> u64 {
        self.broken.load(Ordering::Relaxed)
    }

    /// Start encoding one pane. Idempotent for an unchanged size.
    ///
    /// A changed size restarts rather than reconfigures: a decoder in the
    /// middle of a picture group cannot be told its picture just changed shape,
    /// which is why `MediaParams::Video` carries the size at all.
    pub(crate) fn start(&mut self, pane_id: &str, width: u32, height: u32, fps: u32) {
        if let Some(existing) = self.panes.get(pane_id) {
            if existing.width == width && existing.height == height {
                return;
            }
            self.stop(pane_id);
        }
        if width == 0 || height == 0 {
            return;
        }
        let process = match (self.spawn)(width, height, fps) {
            Ok(process) => process,
            Err(err) => {
                // Reported, not panicked: a machine without ffmpeg is a machine
                // that cannot do video, and that is a fact the consumer needs
                // rather than a reason to take the server down.
                // TP-VIDEO-SUPERVISE-01
                tracing::warn!(pane_id, error = %err, "video encoder did not start");
                self.panes.insert(
                    pane_id.to_owned(),
                    PaneEncoder::stillborn(width, height, fps, err),
                );
                return;
            }
        };
        let EncoderProcess {
            child,
            mut stdin,
            mut stdout,
        } = process;
        let (tx, rx) = sync_channel::<(u64, Vec<u8>)>(CHUNK_QUEUE);
        // One stamp per picture written, read back one per picture returned.
        // Unbounded: the writer never waits on the reader, and a picture the
        // encoder holds is a stamp that waits here for it.
        let (stamps_tx, stamps_rx) = std::sync::mpsc::channel::<u64>();
        let stop = Arc::new(AtomicBool::new(false));
        let broken = Arc::new(Mutex::new(None::<String>));

        // Pictures go in on their own thread. The server loop hands one over
        // and returns; whether ffmpeg is ready is ffmpeg's business.
        // TP-VIDEO-WRITE-01
        let (frame_tx, frame_rx) = sync_channel::<(u64, Vec<u8>)>(FRAME_QUEUE);
        let broken_writer = broken.clone();
        std::thread::Builder::new()
            .name(format!("herdr-video-in-{pane_id}"))
            .spawn(move || {
                while let Ok((pts_us, frame)) = frame_rx.recv() {
                    // The stamp goes to the reader before the picture goes
                    // to the encoder: a picture cannot come back before it
                    // went in, so the reader always finds its stamp waiting.
                    // TP-VIDEO-PTS-01
                    let _ = stamps_tx.send(pts_us);
                    if let Err(err) = stdin.write_all(&frame) {
                        let mut slot = broken_writer
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        if slot.is_none() {
                            *slot = Some(format!("encoder input closed: {err}"));
                        }
                        break;
                    }
                }
                // Dropping the writer here is what closes the encoder's input,
                // which is how ffmpeg is asked to wrap up. `finish` drops the
                // sender to get here.
                drop(stdin);
            })
            .ok();
        let stop_thread = stop.clone();
        let dropped = self.dropped.clone();
        let unstamped = self.unstamped.clone();
        let late_total = self.late.clone();
        let broken_total = self.broken.clone();
        let wake = self.wake.clone();
        let join = std::thread::Builder::new()
            .name(format!("herdr-video-{pane_id}"))
            .spawn(move || {
                let mut buf = vec![0_u8; CHUNK_BYTES];
                let mut demux = TransportDemux::new();
                let (mut seen_late, mut seen_broken) = (0_u64, 0_u64);
                while !stop_thread.load(Ordering::Relaxed) {
                    let n = match stdout.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => n,
                    };
                    demux.push(&buf[..n]);
                    // Each reader adds what its own transport lost since it
                    // last looked, so several encoders share the two totals.
                    let now_late = demux.unbounded_pictures();
                    let now_broken = demux.truncated_pictures() + demux.resyncs();
                    late_total.fetch_add(now_late - seen_late, Ordering::Relaxed);
                    broken_total.fetch_add(now_broken - seen_broken, Ordering::Relaxed);
                    seen_late = now_late;
                    seen_broken = now_broken;
                    while let Some(picture) = demux.next_picture() {
                        // The oldest stamp is this picture's: the encoder
                        // returns pictures in the order it was handed them.
                        let pts_us = match stamps_rx.try_recv() {
                            Ok(pts_us) => pts_us,
                            Err(_) => {
                                unstamped.fetch_add(1, Ordering::Relaxed);
                                crate::media::now_us()
                            }
                        };
                        match tx.try_send((pts_us, picture)) {
                            Ok(()) => {
                                // Woken per picture, so delivery follows the
                                // picture rather than the loop's timer.
                                // TP-VIDEO-PTS-02
                                if let Some(wake) = wake.as_ref() {
                                    wake.notify_one();
                                }
                            }
                            Err(TrySendError::Full(_)) => {
                                dropped.fetch_add(1, Ordering::Relaxed);
                            }
                            Err(TrySendError::Disconnected(_)) => return,
                        }
                    }
                }
            })
            .ok();
        self.panes.insert(
            pane_id.to_owned(),
            PaneEncoder {
                frames: Some(frame_tx),
                chunks: rx,
                stop,
                join,
                child,
                width,
                height,
                fps,
                opened: false,
                broken,
                dropped_frames: self.dropped_frames.clone(),
            },
        );
    }

    /// Hand one picture to a pane's encoder.
    ///
    /// A pane with no encoder is not an error: every frame passes through here
    /// and almost none of them are being watched.
    ///
    /// Test-only: production always knows the picture's moment — the
    /// graphics runtime recorded it — and offers with `offer_at`.
    #[cfg(test)]
    pub(crate) fn offer(&mut self, pane_id: &str, rgba: &[u8]) {
        self.offer_at(pane_id, rgba, crate::media::now_us());
    }

    /// Hands a picture over with the moment it was shown, which is the moment
    /// its chunk will carry.
    ///
    /// The stamp is taken here, at hand-off, and never at drain: pictures
    /// drained together and stamped with one `now` are pictures a player
    /// cannot place in time, which is what the audio side paid for once
    /// (E-143). TP-VIDEO-PTS-01
    pub(crate) fn offer_at(&mut self, pane_id: &str, rgba: &[u8], pts_us: u64) {
        let Some(encoder) = self.panes.get_mut(pane_id) else {
            return;
        };
        if encoder.broken_reason().is_some() {
            return;
        }
        let expected = encoder.width as usize * encoder.height as usize * 4;
        if rgba.len() != expected {
            // A picture of the wrong size would desynchronise the raw stream
            // for good: ffmpeg reads a fixed number of bytes per frame and has
            // no way to notice it was handed the wrong count.
            //
            // Checked here, on the calling side, and never behind the channel:
            // a wrong picture that got queued would desynchronise on a thread,
            // where the size that caused it is no longer in anyone's hands.
            // TP-VIDEO-WRITE-04
            encoder.set_broken(format!(
                "frame is {} bytes, encoder was started for {expected}",
                rgba.len()
            ));
            return;
        }
        let Some(frames) = encoder.frames.as_ref() else {
            return;
        };
        match frames.try_send((pts_us, rgba.to_vec())) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                // Dropped rather than waited on. A picture the encoder never
                // sees is one frame a decoder conceals; a server loop that
                // waited would freeze every pane instead. TP-VIDEO-WRITE-02
                encoder.dropped_frames.fetch_add(1, Ordering::Relaxed);
            }
            Err(TrySendError::Disconnected(_)) => {
                encoder.set_broken("encoder input closed".to_owned());
            }
        }
    }

    /// How many pictures were dropped because the encoder was not keeping up.
    ///
    /// Separate from `dropped_chunks`, which counts the loop failing to drain
    /// output: these are opposite bottlenecks, and one number would hide
    /// whichever is smaller.
    pub(crate) fn dropped_frames(&self) -> u64 {
        self.dropped_frames.load(Ordering::Relaxed)
    }

    /// Stop every encoder. What the last viewer leaving triggers.
    pub(crate) fn stop_all(&mut self) {
        let ids: Vec<String> = self.panes.keys().cloned().collect();
        for pane_id in ids {
            self.stop(&pane_id);
        }
    }

    /// Stop one pane's encoder and forget it.
    pub(crate) fn stop(&mut self, pane_id: &str) {
        if let Some(mut encoder) = self.panes.remove(pane_id) {
            encoder.finish();
        }
    }

    /// Everything the loop should act on, in order. Never waits.
    pub(crate) fn drain(&mut self) -> Vec<VideoEvent> {
        let mut events = Vec::new();
        let mut finished: Vec<(String, String)> = Vec::new();
        for (pane_id, encoder) in &mut self.panes {
            loop {
                match encoder.chunks.try_recv() {
                    Ok((pts_us, chunk)) => {
                        if encoder.opened {
                            events.push(VideoEvent::Chunk {
                                pane_id: pane_id.clone(),
                                chunk,
                                pts_us,
                            });
                        } else {
                            encoder.opened = true;
                            events.push(VideoEvent::Opened {
                                pane_id: pane_id.clone(),
                                width: encoder.width,
                                height: encoder.height,
                                fps: encoder.fps,
                                chunk,
                                pts_us,
                            });
                        }
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        finished
                            .push((pane_id.clone(), encoder.broken_reason().unwrap_or_default()));
                        break;
                    }
                }
            }
            if let Some(detail) = encoder.broken_reason() {
                if !finished.iter().any(|(id, _)| id == pane_id) {
                    finished.push((pane_id.clone(), detail));
                }
            }
        }
        for (pane_id, detail) in finished {
            self.stop(&pane_id);
            events.push(VideoEvent::Ended { pane_id, detail });
        }
        events
    }
}

impl PaneEncoder {
    /// An encoder that never started. It reports once and then ends, so a
    /// consumer learns *why* rather than watching nothing arrive.
    fn stillborn(width: u32, height: u32, fps: u32, detail: String) -> Self {
        let (_tx, rx) = sync_channel::<(u64, Vec<u8>)>(1);
        Self {
            frames: None,
            chunks: rx,
            stop: Arc::new(AtomicBool::new(true)),
            join: None,
            child: None,
            width,
            height,
            fps,
            opened: false,
            broken: Arc::new(Mutex::new(Some(detail))),
            dropped_frames: Arc::new(AtomicU64::new(0)),
        }
    }

    fn finish(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Closing the input is how ffmpeg is asked to wrap up; ending it any
        // harder first would throw away whatever it had already encoded. The
        // input now lives on the writer thread, so dropping the sender is what
        // closes it: the thread's `recv` ends, and the writer goes with it.
        self.frames = None;
        // The child is reaped off the loop. Wrapping up takes ffmpeg about two
        // seconds — its last group of pictures — and a stop that waited for
        // it here held the whole server for that long at every viewer's
        // departure: the audio recorder counted a hundred lost frames each
        // time, the bursts of a hundred in `dropped` on the live server (K19,
        // E-163). The wait still happens, on a thread of its own, because a
        // child nobody waits for is a zombie per viewer. TP-VIDEO-SUPERVISE-02
        if let Some(mut child) = self.child.take() {
            let reaped = std::thread::Builder::new()
                .name("herdr-video-reap".to_owned())
                .spawn(move || {
                    let _ = child.wait();
                });
            if reaped.is_err() {
                // No thread to be had: the zombie is the lesser cost, and it
                // is bounded — one per encoder that ever stopped.
                tracing::warn!("video encoder could not be reaped off the loop");
            }
        }
        // Not joined: joining is waiting, and the read blocks until the pipe
        // closes. The thread ends on its own once the child is gone.
        self.join = None;
    }
}

impl Drop for VideoSupervisor {
    fn drop(&mut self) {
        let ids: Vec<String> = self.panes.keys().cloned().collect();
        for pane_id in ids {
            self.stop(&pane_id);
        }
    }
}

/// What ffmpeg is asked for, as a list so a test can read it.
///
/// Every flag here was measured on 2026-09-02 and is load-bearing:
///
/// - `-f mpegts -omit_video_pes_length 0`: a transport whose pictures say
///   how long they are, so the reader knows a picture is whole on its own
///   last byte — 1.3 ms after hand-off, against 101 ms for Annex-B or
///   fragmented MP4, both of which reveal a picture's end only at the next
///   picture's start. TP-VIDEO-AU-01
/// - `-delay 0`: nvenc's default output delay holds pictures back.
/// - `-bf 0`, `-fps_mode passthrough`: no reordering and one picture out per
///   picture in, so the stamp taken at hand-off can be paired with the
///   picture that comes back. TP-VIDEO-PTS-01
/// - `-muxdelay 0 -muxpreload 0 -flush_packets 1`: nothing waits in the muxer.
///
/// TP-VIDEO-ENC-01
pub(crate) fn encoder_args(width: u32, height: u32, fps: u32) -> Vec<String> {
    [
        "-hide_banner",
        "-loglevel",
        "error",
        "-f",
        "rawvideo",
        "-pix_fmt",
        "rgba",
        "-s",
        &format!("{width}x{height}"),
        "-r",
        &fps.to_string(),
        "-i",
        "-",
        "-c:v",
        ENCODER,
        // Latency over size: no B-frames, low-latency tuning, no output
        // delay, and a key frame every two seconds so a consumer that joins
        // late does not wait minutes for a picture.
        "-preset",
        "p1",
        "-tune",
        "ll",
        "-bf",
        "0",
        "-delay",
        "0",
        "-g",
        &(fps * 2).max(1).to_string(),
        "-fps_mode",
        "passthrough",
        "-f",
        "mpegts",
        "-omit_video_pes_length",
        "0",
        "-muxdelay",
        "0",
        "-muxpreload",
        "0",
        "-flush_packets",
        "1",
        "-",
    ]
    .iter()
    .map(|arg| (*arg).to_owned())
    .collect()
}

/// The shipped encoder: raw RGBA in, pictures out as MPEG-TS.
fn spawn_ffmpeg(width: u32, height: u32, fps: u32) -> Result<EncoderProcess, String> {
    let mut child = std::process::Command::new("ffmpeg")
        .args(encoder_args(width, height, fps))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|err| format!("ffmpeg did not start: {err}"))?;
    let stdin: ChildStdin = child.stdin.take().ok_or("ffmpeg has no stdin")?;
    let stdout = child.stdout.take().ok_or("ffmpeg has no stdout")?;
    Ok(EncoderProcess {
        child: Some(child),
        stdin: Box::new(stdin),
        stdout: Box::new(stdout),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::headless::mpegts::test_stream::{access_unit, pes, program};
    use std::sync::mpsc::{channel, Sender};

    /// A fake encoder: whatever is written is ignored, and a scripted stream of
    /// bytes comes back. Asserting against real ffmpeg would be asserting about
    /// the machine, which is what the spawn seam exists to avoid.
    struct Scripted {
        rx: Receiver<Vec<u8>>,
        left: Vec<u8>,
    }

    impl Read for Scripted {
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

    struct Sink;
    impl Write for Sink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn scripted() -> (VideoSupervisor, Sender<Vec<u8>>) {
        scripted_with(Sink)
    }

    /// A scripted encoder whose input goes to `sink`, for the tests that
    /// need to know a picture was written before they script its answer.
    fn scripted_with(sink: impl Write + Send + 'static) -> (VideoSupervisor, Sender<Vec<u8>>) {
        let (tx, rx) = channel::<Vec<u8>>();
        let shared = std::sync::Mutex::new(Some((rx, sink)));
        let supervisor = VideoSupervisor::with_spawn(Box::new(move |_w, _h, _f| {
            let (rx, sink) = shared
                .lock()
                .expect("spawn seam")
                .take()
                .ok_or("spawned twice")?;
            Ok(EncoderProcess {
                child: None,
                stdin: Box::new(sink),
                stdout: Box::new(Scripted {
                    rx,
                    left: Vec::new(),
                }),
            })
        }));
        (supervisor, tx)
    }

    /// TP-VIDEO-SUPERVISE-02 · stopping an encoder returns before the encoder
    /// exits, and the encoder is still reaped.
    #[cfg(target_os = "linux")]
    #[test]
    fn stopping_an_encoder_does_not_wait_for_it_in_the_loop() {
        // The "encoder" is a process that takes two seconds to exit once its
        // input closes — about what ffmpeg takes to flush its last group of
        // pictures. A stop that waits for it holds the server loop for those
        // seconds, and the recorder counted a hundred lost audio frames at
        // every viewer's departure (K19, E-163). The stop must return at
        // once; the child must still be reaped, because a zombie per viewer
        // is a leak by another name.
        let pid = Arc::new(AtomicU64::new(0));
        let seen = Arc::clone(&pid);
        let mut supervisor = VideoSupervisor::with_spawn(Box::new(move |_w, _h, _f| {
            let mut child = std::process::Command::new("sh")
                .args(["-c", "cat >/dev/null; sleep 2"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .map_err(|err| err.to_string())?;
            seen.store(u64::from(child.id()), Ordering::Relaxed);
            let stdin = child.stdin.take().ok_or("no stdin")?;
            let stdout = child.stdout.take().ok_or("no stdout")?;
            Ok(EncoderProcess {
                child: Some(child),
                stdin: Box::new(stdin),
                stdout: Box::new(stdout),
            })
        }));
        supervisor.start("w1:pA", 16, 16, 10);
        let stopping = std::time::Instant::now();
        supervisor.stop("w1:pA");
        let took = stopping.elapsed();
        assert!(
            took < std::time::Duration::from_millis(500),
            "stop held the loop for {took:?} while the encoder wrapped up"
        );
        let proc_dir = format!("/proc/{}", pid.load(Ordering::Relaxed));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(6);
        while std::path::Path::new(&proc_dir).exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "the encoder was never reaped: {proc_dir} still exists"
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    /// A sink that counts the pictures written to it.
    struct Counting(Arc<AtomicU64>);
    impl Write for Counting {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.fetch_add(1, Ordering::Relaxed);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn picture_of(event: &VideoEvent) -> (Vec<u8>, u64) {
        match event {
            VideoEvent::Opened { chunk, pts_us, .. } | VideoEvent::Chunk { chunk, pts_us, .. } => {
                (chunk.clone(), *pts_us)
            }
            other => panic!("not a picture: {other:?}"),
        }
    }

    /// TP-VIDEO-AU-01 · one picture is one chunk, however the encoder's pipe
    /// cut the bytes.
    ///
    /// Before this, a chunk was whatever one `read` returned — 32 KiB or
    /// less — so a picture could arrive in two chunks and two pictures in
    /// one, and no timestamp could mean one picture.
    #[test]
    fn a_picture_is_one_chunk_however_the_pipe_cut_it() {
        let (mut supervisor, tx) = scripted();
        supervisor.start("w1:p1", 4, 4, 30);
        let pictures = vec![
            access_unit(true, &[1; 700]),
            access_unit(false, &[2; 40]),
            access_unit(false, &[3; 300]),
        ];
        for piece in program(0x100, &pictures).chunks(97) {
            tx.send(piece.to_vec()).expect("script");
        }
        let events = wait_for(&mut supervisor, 3);
        let got: Vec<Vec<u8>> = events.iter().map(|event| picture_of(event).0).collect();
        assert_eq!(
            got, pictures,
            "three pictures, whole, in order, and nothing else"
        );
    }

    /// TP-VIDEO-PTS-01 · ⭐⭐ a chunk carries the stamp of its own picture,
    /// not the moment the loop drained it.
    ///
    /// The audio side paid for this once (E-143): pictures drained together
    /// stamped with one `now` are pictures the player cannot tell apart in
    /// time, and A/V sync rests on nothing else. The recorder stamps at
    /// hand-off; the loop only carries the stamp.
    #[test]
    fn a_chunk_carries_the_stamp_of_its_own_picture_and_not_the_drain_time() {
        let written = Arc::new(AtomicU64::new(0));
        let (mut supervisor, tx) = scripted_with(Counting(Arc::clone(&written)));
        supervisor.start("w1:p1", 4, 4, 30);
        supervisor.offer_at("w1:p1", &[0; 64], 1_000);
        supervisor.offer_at("w1:p1", &[1; 64], 2_000);
        supervisor.offer_at("w1:p1", &[2; 64], 3_000);
        // The pictures cross to the encoder on a thread; the answer is only
        // scripted once all three have been written, as it would be in life.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while written.load(Ordering::Relaxed) < 3 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(
            written.load(Ordering::Relaxed),
            3,
            "three pictures reached the encoder"
        );
        tx.send(program(
            0x100,
            &[
                access_unit(true, b"a"),
                access_unit(false, b"b"),
                access_unit(false, b"c"),
            ],
        ))
        .expect("script");

        let events = wait_for(&mut supervisor, 3);
        let stamps: Vec<u64> = events.iter().map(|event| picture_of(event).1).collect();
        assert_eq!(
            stamps,
            vec![1_000, 2_000, 3_000],
            "three pictures drained at once still carry three different moments"
        );
    }

    /// TP-VIDEO-PTS-02 · a queued picture wakes whoever is waiting.
    ///
    /// The loop's idle timer is 250 ms; a picture that waits for it is late
    /// by up to that, and at 10 fps that is two and a half pictures.
    #[tokio::test]
    async fn a_queued_picture_wakes_whoever_is_waiting() {
        let wake = Arc::new(tokio::sync::Notify::new());
        let (supervisor, tx) = scripted();
        let mut supervisor = supervisor.wake_on(Arc::clone(&wake));
        supervisor.start("w1:p1", 4, 4, 30);
        tx.send(program(0x100, &[access_unit(true, b"x")]))
            .expect("script");
        tokio::time::timeout(std::time::Duration::from_secs(5), wake.notified())
            .await
            .expect("the loop is woken within a moment of the picture");
        assert_eq!(
            wait_for(&mut supervisor, 1).len(),
            1,
            "and the picture is there to take"
        );
    }

    /// TP-VIDEO-ENC-01 · the encoder is asked for a transport whose pictures
    /// carry their length, and for no output delay.
    ///
    /// A configuration pin, and a deliberate one: every flag here was
    /// measured (2026-09-02) and a merge that dropped one would not fail a
    /// test elsewhere — the video would simply arrive a frame late and
    /// nobody could say why.
    #[test]
    fn the_encoder_is_asked_for_lengths_and_no_delay() {
        let args = encoder_args(640, 360, 30);
        let has = |flag: &str, value: &str| {
            args.windows(2)
                .any(|pair| pair[0] == flag && pair[1] == value)
        };
        assert!(has("-f", "mpegts"), "{args:?}");
        assert!(
            has("-omit_video_pes_length", "0"),
            "the length is the whole reason for the container: {args:?}"
        );
        assert!(
            has("-delay", "0"),
            "nvenc's default holds pictures back: {args:?}"
        );
        assert!(
            has("-bf", "0"),
            "no reordering, so pictures leave in the order they came: {args:?}"
        );
        assert!(
            has("-fps_mode", "passthrough"),
            "one picture in is one picture out, or the stamps drift: {args:?}"
        );
        assert!(has("-s", "640x360") && has("-r", "30"), "{args:?}");
        assert_eq!(
            args.last().map(String::as_str),
            Some("-"),
            "to stdout: {args:?}"
        );
    }

    fn wait_for(supervisor: &mut VideoSupervisor, want: usize) -> Vec<VideoEvent> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut events = Vec::new();
        while events.len() < want && std::time::Instant::now() < deadline {
            events.extend(supervisor.drain());
            if events.len() < want {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
        events
    }

    /// TP-VIDEO-SUPERVISE-01 · nothing runs before somebody asks.
    #[test]
    fn no_encoder_exists_until_a_pane_is_started() {
        let (mut supervisor, _tx) = scripted();
        assert_eq!(supervisor.encoding_count(), 0);
        supervisor.offer("w1:p1", &[0; 16]);
        assert_eq!(
            supervisor.encoding_count(),
            0,
            "a frame for a pane nobody watches starts nothing"
        );
    }

    /// TP-VIDEO-SUPERVISE-01 · the first chunk opens the stream, the rest do not.
    #[test]
    fn the_first_chunk_opens_and_the_rest_are_chunks() {
        let (mut supervisor, tx) = scripted();
        supervisor.start("w1:p1", 4, 4, 30);
        tx.send(program(0x100, &[access_unit(true, b"first")]))
            .expect("script");
        let events = wait_for(&mut supervisor, 1);
        assert!(
            matches!(
                events.first(),
                Some(VideoEvent::Opened {
                    width: 4,
                    height: 4,
                    fps: 30,
                    ..
                })
            ),
            "got {events:?}"
        );
        tx.send(pes(0x100, &access_unit(false, b"second"), true))
            .expect("script");
        let events = wait_for(&mut supervisor, 1);
        assert!(
            matches!(events.first(), Some(VideoEvent::Chunk { .. })),
            "got {events:?}"
        );
    }

    /// TP-VIDEO-SUPERVISE-01 · a machine without an encoder reports, and does not panic.
    ///
    /// Silence here would be indistinguishable from a stream that simply has
    /// nothing to say yet, and the consumer's respawn decision hangs on telling
    /// those apart.
    #[test]
    fn an_encoder_that_cannot_start_ends_with_a_reason() {
        let mut supervisor =
            VideoSupervisor::with_spawn(Box::new(|_w, _h, _f| Err("ffmpeg not found".to_owned())));
        supervisor.start("w1:p1", 4, 4, 30);
        let events = supervisor.drain();
        match events.first() {
            Some(VideoEvent::Ended { pane_id, detail }) => {
                assert_eq!(pane_id, "w1:p1");
                assert!(detail.contains("ffmpeg"), "the reason travels: {detail}");
            }
            other => panic!("expected Ended with a reason, got {other:?}"),
        }
        assert_eq!(supervisor.encoding_count(), 0, "and it is not left behind");
    }

    /// A picture of the wrong size ends the encoder instead of desynchronising
    /// the raw stream, which ffmpeg cannot detect and never recovers from.
    #[test]
    fn a_frame_of_the_wrong_size_ends_the_encoder_rather_than_corrupting_it() {
        let (mut supervisor, _tx) = scripted();
        supervisor.start("w1:p1", 4, 4, 30);
        supervisor.offer("w1:p1", &[0; 8]); // 4x4 RGBA is 64 bytes
        let events = supervisor.drain();
        assert!(
            matches!(events.first(), Some(VideoEvent::Ended { detail, .. }) if detail.contains("64")),
            "got {events:?}"
        );
    }

    /// A writer that never accepts a byte, so the pipe is always full.
    struct Wedged;
    impl Write for Wedged {
        fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
            // Parks rather than erroring: an encoder that refuses input is a
            // slow encoder, not a dead one, and the two must behave
            // differently. Mirrors the reader-side `Silent` fixture.
            std::thread::sleep(std::time::Duration::from_secs(3600));
            Ok(0)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// A writer that fails on the first picture.
    struct Refusing;
    impl Write for Refusing {
        fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "encoder went away",
            ))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn supervisor_writing_into<W: Write + Send + 'static>(make: fn() -> W) -> VideoSupervisor {
        VideoSupervisor::with_spawn(Box::new(move |_w, _h, _f| {
            struct Silent;
            impl Read for Silent {
                fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
                    std::thread::sleep(std::time::Duration::from_secs(3600));
                    Ok(0)
                }
            }
            Ok(EncoderProcess {
                child: None,
                stdin: Box::new(make()),
                stdout: Box::new(Silent),
            })
        }))
    }

    /// TP-VIDEO-WRITE-01 · ⭐ the server loop never waits on the encoder.
    ///
    /// `offer` is called from the loop that draws every pane, so a blocking
    /// write there does not degrade video — it freezes the whole product. The
    /// module already forbids this in the read direction ("joining is
    /// waiting"); until 2026-09-01 the write direction had no such guard, and
    /// it went unnoticed only because nine frames in a hundred ever reached
    /// the pipe, so the pipe was always empty.
    ///
    /// Shaped to fail rather than hang: the offer runs on a thread the test
    /// can outlive, so a regression reports instead of wedging the suite.
    #[test]
    fn offering_a_picture_never_blocks_the_caller() {
        let mut supervisor = supervisor_writing_into(|| Wedged);
        supervisor.start("w1:p1", 4, 4, 30);

        let done = Arc::new(AtomicBool::new(false));
        let flag = done.clone();
        let handle = std::thread::spawn(move || {
            // More pictures than the queue holds: past the bound, a blocking
            // implementation has nowhere to put them and stops here.
            for _ in 0..(FRAME_QUEUE + 8) {
                supervisor.offer("w1:p1", &[0; 64]);
            }
            flag.store(true, Ordering::Relaxed);
            supervisor
        });

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !done.load(Ordering::Relaxed) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(
            done.load(Ordering::Relaxed),
            "offer is still waiting on an encoder that will not read — in the server loop that \
             is every pane frozen, not one stalled video"
        );
        // Leaked deliberately: the writer thread is parked inside `Wedged`, and
        // joining it is the wait this test exists to forbid.
        std::mem::forget(handle.join().expect("offer thread"));
    }

    /// TP-VIDEO-WRITE-02 · a full encoder input drops, and says it dropped.
    ///
    /// Dropping is the right policy — waiting returns video with accumulated
    /// delay instead of a gap — but a policy that never reports cannot be told
    /// from a bug, and "the video stutters" has a dozen other causes.
    #[test]
    fn a_picture_the_encoder_cannot_take_is_dropped_and_counted() {
        let mut supervisor = supervisor_writing_into(|| Wedged);
        supervisor.start("w1:p1", 4, 4, 30);
        assert_eq!(supervisor.dropped_frames(), 0);

        for _ in 0..(FRAME_QUEUE + 8) {
            supervisor.offer("w1:p1", &[0; 64]);
        }

        assert!(
            supervisor.dropped_frames() > 0,
            "twelve pictures went into a queue of {FRAME_QUEUE} that nothing drains, and the \
             counter says none were dropped"
        );
        std::mem::forget(supervisor);
    }

    /// TP-VIDEO-WRITE-03 · a write that fails on a thread still reports why.
    ///
    /// The regression risk of moving the write off the loop: the failure now
    /// happens somewhere the caller is not, and a consumer that cannot tell
    /// *ended* from *crashed* has no basis for its respawn decision.
    #[test]
    fn a_write_that_fails_on_the_thread_still_ends_the_stream_with_its_reason() {
        let mut supervisor = supervisor_writing_into(|| Refusing);
        supervisor.start("w1:p1", 4, 4, 30);
        supervisor.offer("w1:p1", &[0; 64]);

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut events = Vec::new();
        while events.is_empty() && std::time::Instant::now() < deadline {
            events = supervisor.drain();
            if events.is_empty() {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }

        match events.first() {
            Some(VideoEvent::Ended { pane_id, detail }) => {
                assert_eq!(pane_id, "w1:p1");
                assert!(
                    detail.contains("encoder went away"),
                    "the reason travelled back from the writer thread: {detail}"
                );
            }
            other => panic!("expected Ended carrying the write failure, got {other:?}"),
        }
    }

    /// TP-VIDEO-WRITE-04 · a wrong-sized picture is refused before the queue.
    ///
    /// The size check has to stay on the calling side. Behind the channel it
    /// would desynchronise the raw stream on a thread, where the count that
    /// caused it is gone — and a raw stream, once desynchronised, never
    /// recovers, because the encoder reads a fixed byte count per picture and
    /// cannot detect the mistake.
    #[test]
    fn a_wrong_sized_picture_is_refused_before_it_can_be_queued() {
        let seen = Arc::new(AtomicU64::new(0));
        let counted = seen.clone();
        let mut supervisor = VideoSupervisor::with_spawn(Box::new(move |_w, _h, _f| {
            struct Counting(Arc<AtomicU64>);
            impl Write for Counting {
                fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                    self.0.fetch_add(1, Ordering::Relaxed);
                    Ok(buf.len())
                }
                fn flush(&mut self) -> std::io::Result<()> {
                    Ok(())
                }
            }
            struct Silent;
            impl Read for Silent {
                fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
                    std::thread::sleep(std::time::Duration::from_secs(3600));
                    Ok(0)
                }
            }
            Ok(EncoderProcess {
                child: None,
                stdin: Box::new(Counting(counted.clone())),
                stdout: Box::new(Silent),
            })
        }));
        supervisor.start("w1:p1", 4, 4, 30);

        supervisor.offer("w1:p1", &[0; 8]); // 4x4 RGBA is 64 bytes

        let events = supervisor.drain();
        assert!(
            matches!(events.first(), Some(VideoEvent::Ended { detail, .. }) if detail.contains("64")),
            "got {events:?}"
        );
        // Given a moment in case the writer thread was going to take it anyway.
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert_eq!(
            seen.load(Ordering::Relaxed),
            0,
            "and nothing of the wrong size ever reached the encoder's input"
        );
    }

    /// Starting the same size twice changes nothing; a new size restarts.
    #[test]
    fn a_resize_restarts_the_encoder_and_an_unchanged_size_does_not() {
        let (tx1, rx1) = channel::<Vec<u8>>();
        let (_tx2, rx2) = channel::<Vec<u8>>();
        let queue = std::sync::Mutex::new(vec![rx2, rx1]);
        let spawned = Arc::new(AtomicU64::new(0));
        let counter = spawned.clone();
        let mut supervisor = VideoSupervisor::with_spawn(Box::new(move |_w, _h, _f| {
            counter.fetch_add(1, Ordering::Relaxed);
            let rx = queue
                .lock()
                .expect("seam")
                .pop()
                .ok_or("spawned too often")?;
            Ok(EncoderProcess {
                child: None,
                stdin: Box::new(Sink),
                stdout: Box::new(Scripted {
                    rx,
                    left: Vec::new(),
                }),
            })
        }));
        supervisor.start("w1:p1", 4, 4, 30);
        supervisor.start("w1:p1", 4, 4, 30);
        assert_eq!(
            spawned.load(Ordering::Relaxed),
            1,
            "same size, same encoder"
        );
        supervisor.start("w1:p1", 8, 8, 30);
        assert_eq!(
            spawned.load(Ordering::Relaxed),
            2,
            "a new size is a new stream"
        );
        drop(tx1);
    }
}
