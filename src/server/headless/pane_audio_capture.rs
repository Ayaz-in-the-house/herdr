//! Starting and stopping the capture of one pane's sound.
//!
//! The two sources this owns — the recorder and the graph watcher — both block
//! on purpose: the alternative is polling, and polling bills an idle desktop
//! for a listener who is not there. But the server loop cannot block for even
//! one frame, so each blocking read lives on a thread of its own and reaches
//! the loop through a channel the loop drains without waiting.
//!
//! Three decisions here are worth their reasons.
//!
//! The frame channel is **bounded**. A recorder produces 7680 bytes every
//! twenty milliseconds — 384 KB a second for one pane — so an unbounded
//! channel turns a stalled loop into growing memory. When the channel is full
//! the frame is **dropped** and counted, never waited on: a blocking send would
//! back up into the recorder's pipe, and what comes back after that is not
//! silence but sound with accumulated delay, which is worse. This is not a new
//! policy — `Runtime::offer` already drops late chunks at the source.
//!
//! A reader thread is never joined from the loop. Joining is waiting, and this
//! is the one place that cannot wait; finished threads are reaped when they are
//! noticed instead.
//!
//! Nothing here starts before a listener exists. That is the resource doctrine
//! at its plainest, and its violation is silent: a watcher nobody asked for
//! looks exactly like a watcher somebody did.
//!
//! TP-MEDIA-SUPERVISE-01.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::Arc;
use std::sync::Mutex;
use std::thread::JoinHandle;
use std::time::Instant;

use crate::app::pane_audio_source::{Debouncer, GRAPH_QUIET};
use crate::platform::{AudioSourceError, FrameSource, GraphSignals};

/// How many frames may wait for the server loop before one is dropped.
///
/// Eight frames is 160 ms. Long enough to ride out a loop that is busy drawing,
/// short enough that a listener never hears the queue instead of the sound.
/// Frames the queue holds while the loop is away: half a second, the ceiling
/// of the client's jitter buffer, so a loop away for as long as the client
/// could absorb loses nothing at the source. 25 × 7680 bytes is 192 KB a
/// pane — bounded still. Eight frames (160 ms) was the size before, and a
/// loop that renders and encodes for a remote client was away longer than
/// that; the queue then dropped the *fresh* frames and kept the stale ones,
/// and every frame it kept was past the lateness budget by the time the loop
/// returned: a listener that heard nothing while the recorder counted drops
/// (K18, E-162). TP-MEDIA-FLOW-06
const QUEUE_MS: usize = 500;
pub(crate) const FRAME_QUEUE: usize = QUEUE_MS / crate::media::FRAME_MS as usize;

/// How a capture is started, behind a boxed function so a test can hand the
/// supervisor a source it wrote. The shipped one is `platform::capture_stream`;
/// asserting against the real recorder would be asserting about the machine.
type CaptureFn = Box<dyn Fn(u32) -> Option<Result<Box<dyn FrameSource>, AudioSourceError>> + Send>;

/// The same seam for the graph watcher.
type WatcherFn = Box<dyn Fn() -> Option<Result<Box<dyn GraphSignals>, AudioSourceError>> + Send>;

/// What the server loop should do about a pane, in the order it must do it.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CaptureEvent {
    /// The pane's first whole frame. The stream is opened *now*, with this
    /// frame in hand, because the server counts a chunk's timestamp from the
    /// open: opening earlier and waiting for sound makes every frame late.
    Opened { pane_id: String, frame: Vec<u8> },
    /// A further frame for a pane whose stream is already open. `offset` is
    /// how many frames the recorder produced since the one that opened the
    /// stream, dropped ones included: the frame's moment, counted by the
    /// recorder rather than by whoever managed to receive it. TP-MEDIA-FLOW-01
    Frame {
        pane_id: String,
        frame: Vec<u8>,
        offset: u64,
    },
    /// The pane's source ended, on its own or because it was stopped.
    Ended { pane_id: String },
}

/// The frames one recorder has produced that the loop has not taken yet.
///
/// A **sliding window**, not a queue: when it is full the OLDEST frame is
/// pushed out and the new one is kept. The direction matters more than the
/// depth. The window holds `QUEUE_MS` of sound and the encoder refuses a chunk
/// once it is older than the playout delay, so a window that kept its oldest
/// would hand the loop nothing but frames already past their budget — every
/// one of them dropped, for as long as the recorder stays ahead. That was
/// measured: a recorder at a full 384 kB/s, a climbing drop count, and two
/// listeners receiving nothing at all (K21). The video queue drops its oldest
/// for the same reason and says so; this is the audio side of that rule.
///
/// A frame is never waited on in either direction: the recorder pushes and
/// returns, the loop takes everything and returns. `ended` replaces what a
/// channel's disconnect used to say.
#[derive(Default)]
struct FrameWindow {
    frames: VecDeque<(u64, Vec<u8>)>,
    ended: bool,
}

/// The window plus the lock that guards it. Short critical sections on both
/// sides: a push, or a drain that swaps the whole deque out.
type SharedWindow = Arc<Mutex<FrameWindow>>;

struct PaneCapture {
    /// Each frame with the recorder's count of it, so a frame the window had
    /// to push out still counts towards the offset of the ones after it.
    frames: SharedWindow,
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
    /// False until a whole frame has been handed to the loop. The pre-roll rule
    /// lives in this one bit.
    opened: bool,
    /// The recorder's count of the frame that opened the stream: offsets are
    /// measured from it.
    base: u64,
}

struct WatcherHandle {
    signals: Receiver<()>,
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

pub(crate) struct CaptureSupervisor {
    capture: CaptureFn,
    watcher_factory: WatcherFn,
    watcher: Option<WatcherHandle>,
    panes: BTreeMap<String, PaneCapture>,
    debouncer: Debouncer,
    dropped: Arc<AtomicU64>,
    /// Panes stopped since the last drain whose stream had opened: the loop
    /// is told they ended, because nothing else will tell it. TP-MEDIA-FLOW-03
    stopped: Vec<String>,
    /// Whoever wants to know a frame was queued — the server loop — told at
    /// once rather than found on the next timer. `None` where the drain is
    /// driven by hand. TP-MEDIA-FLOW-02
    wake: Option<Arc<tokio::sync::Notify>>,
    /// The last state that was reported, so a repeat is not reported again.
    last_report: Option<CaptureReport>,
    /// Set when a watcher ended as soon as it started, so the next tick does
    /// not spawn another one every quarter second on a machine where it cannot
    /// work at all.
    watch_unavailable: bool,
}

/// The shape of the capture, as a log line would tell it.
///
/// `dropping` is a flag rather than the count on purpose: the count climbs on
/// every dropped frame, and keying the report on it would turn a busy loop into
/// a busy log. The exact number still travels in the message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CaptureReport {
    pub(crate) watching: bool,
    pub(crate) captured: usize,
    pub(crate) dropping: bool,
    /// Frames the stream refused as late, as a flag for the same reason as
    /// `dropping`. This is the shape of a capture nobody can hear: the
    /// recorder runs, the queue never overflows, and every frame is thrown
    /// away at the stream's clock (E-166). TP-MEDIA-FLOW-14
    pub(crate) dropping_late: bool,
    pub(crate) streams: usize,
    pub(crate) listeners: usize,
}

impl std::fmt::Debug for CaptureSupervisor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CaptureSupervisor")
            .field("panes", &self.panes.len())
            .field("watching", &self.watcher.is_some())
            .field("dropped", &self.dropped.load(Ordering::Relaxed))
            .finish()
    }
}

impl CaptureSupervisor {
    /// The shipped supervisor, wired to this platform.
    pub(crate) fn new() -> Self {
        Self::with_sources(
            Box::new(crate::platform::capture_stream),
            Box::new(crate::platform::start_graph_watcher),
        )
    }

    /// The seam a test uses to drive sources it controls.
    pub(crate) fn with_sources(capture: CaptureFn, watcher_factory: WatcherFn) -> Self {
        Self {
            capture,
            watcher_factory,
            watcher: None,
            panes: BTreeMap::new(),
            debouncer: Debouncer::new(GRAPH_QUIET),
            dropped: Arc::new(AtomicU64::new(0)),
            stopped: Vec::new(),
            wake: None,
            last_report: None,
            watch_unavailable: false,
        }
    }

    /// Signals `wake` whenever a recorder queues a frame, so the loop can
    /// deliver on the frame instead of on its next timer. TP-MEDIA-FLOW-02
    pub(crate) fn wake_on(mut self, wake: Arc<tokio::sync::Notify>) -> Self {
        self.wake = Some(wake);
        self
    }

    /// Whether this supervisor signals `handle` — the same allocation, not a
    /// look-alike. A test's question; production only ever wires it.
    #[cfg(test)]
    pub(crate) fn wakes(&self, handle: &Arc<tokio::sync::Notify>) -> bool {
        self.wake
            .as_ref()
            .is_some_and(|wake| Arc::ptr_eq(wake, handle))
    }

    /// Begins capturing one pane. Starting a pane that is already captured does
    /// nothing, which is what makes it safe to call from a plan recomputed on
    /// every event.
    pub(crate) fn start(
        &mut self,
        pane_id: &str,
        object_serial: u32,
    ) -> Result<(), AudioSourceError> {
        if self.panes.contains_key(pane_id) {
            return Ok(());
        }
        let mut source = match (self.capture)(object_serial) {
            Some(source) => source?,
            None => {
                return Err(AudioSourceError::Unavailable(
                    "this platform cannot capture audio".to_owned(),
                ))
            }
        };
        let frames: SharedWindow = Arc::new(Mutex::new(FrameWindow::default()));
        let producer = Arc::clone(&frames);
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let dropped = Arc::clone(&self.dropped);
        let wake = self.wake.clone();
        let join = std::thread::Builder::new()
            .name(format!("pane-audio-{pane_id}"))
            .spawn(move || {
                // Counted before the queue decides: a dropped frame still
                // took its twenty milliseconds of the recorder's time.
                let mut produced: u64 = 0;
                while !thread_stop.load(Ordering::Acquire) {
                    match source.next_frame() {
                        Ok(Some(frame)) => {
                            {
                                let mut window = match producer.lock() {
                                    Ok(window) => window,
                                    // A poisoned lock means the loop panicked
                                    // while holding it; the recorder has
                                    // nobody to hand frames to.
                                    Err(_) => break,
                                };
                                window.frames.push_back((produced, frame));
                                // The loop is behind. The OLDEST goes, never
                                // the newest: a listener wants the freshest
                                // sound it can get, and keeping a stale
                                // backlog makes every delivered frame late
                                // for as long as the recorder stays ahead.
                                // TP-MEDIA-FLOW-09
                                while window.frames.len() > FRAME_QUEUE {
                                    window.frames.pop_front();
                                    dropped.fetch_add(1, Ordering::Relaxed);
                                }
                            }
                            produced += 1;
                            // Always signalled, because a frame is always
                            // accepted. The old queue went quiet once it
                            // filled, which left the loop on its timer and
                            // made the backlog permanent.
                            if let Some(wake) = &wake {
                                wake.notify_one();
                            }
                        }
                        // The recorder ended. Saying so is what a dropped
                        // sender used to say.
                        Ok(None) => break,
                        Err(_) => break,
                    }
                }
                if let Ok(mut window) = producer.lock() {
                    window.ended = true;
                }
                // The thread owns the source, so the thread closes it. Leaving
                // that to a drop somewhere else is how a recorder outlives the
                // pane that wanted it.
                let _ = source.close();
            })
            .map_err(|err| AudioSourceError::Unavailable(format!("capture thread: {err}")))?;
        self.panes.insert(
            pane_id.to_owned(),
            PaneCapture {
                frames,
                stop,
                join: Some(join),
                opened: false,
                base: 0,
            },
        );
        Ok(())
    }

    /// Ends one pane's capture. The thread notices within a frame and closes
    /// the source itself; nothing here waits for it.
    pub(crate) fn stop(&mut self, pane_id: &str) {
        if let Some(pane) = self.panes.remove(pane_id) {
            // The pane leaves the map with its window, so the drain can
            // never see this pane end on its own; if a stream was opened for
            // it, the loop is told here. A pane that never opened has no
            // stream to end. The flag is what ends the recorder thread.
            if pane.opened {
                self.stopped.push(pane_id.to_owned());
            }
            pane.stop.store(true, Ordering::Release);
            drop(pane.frames);
            drop(pane.join);
        }
    }

    /// Ends everything, including the watcher. What the last listener leaving
    /// costs is nothing at all.
    pub(crate) fn stop_all(&mut self) {
        let panes: Vec<String> = self.panes.keys().cloned().collect();
        for pane_id in panes {
            self.stop(&pane_id);
        }
        self.unwatch();
    }

    /// Starts the graph watcher if it is not already running.
    pub(crate) fn watch(&mut self) -> Result<(), AudioSourceError> {
        if self.watcher.is_some() {
            return Ok(());
        }
        if self.watch_unavailable {
            return Err(AudioSourceError::Unavailable(
                "the graph watcher ended as soon as it started".to_owned(),
            ));
        }
        let mut watcher = match (self.watcher_factory)() {
            Some(watcher) => watcher?,
            None => {
                return Err(AudioSourceError::Unavailable(
                    "this platform has no graph watcher".to_owned(),
                ))
            }
        };
        let (tx, signals) = std::sync::mpsc::channel::<()>();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let join = std::thread::Builder::new()
            .name("pane-audio-graph".to_owned())
            .spawn(move || {
                while !thread_stop.load(Ordering::Acquire) {
                    match watcher.next_signal() {
                        Ok(true) => {
                            if tx.send(()).is_err() {
                                break;
                            }
                        }
                        Ok(false) | Err(_) => break,
                    }
                }
                let _ = watcher.close();
            })
            .map_err(|err| AudioSourceError::Unavailable(format!("watch thread: {err}")))?;
        self.watcher = Some(WatcherHandle {
            signals,
            stop,
            join: Some(join),
        });
        Ok(())
    }

    /// Stops the graph watcher.
    pub(crate) fn unwatch(&mut self) {
        self.watch_unavailable = false;
        if let Some(watcher) = self.watcher.take() {
            watcher.stop.store(true, Ordering::Release);
            drop(watcher.signals);
            drop(watcher.join);
        }
    }

    pub(crate) fn is_watching(&self) -> bool {
        self.watcher.is_some()
    }

    /// Whether the graph has moved and then settled, which is the only moment
    /// worth re-reading it. Never blocks.
    pub(crate) fn graph_settled(&mut self, now: Instant) -> bool {
        let mut ended = false;
        if let Some(watcher) = self.watcher.as_ref() {
            loop {
                match watcher.signals.try_recv() {
                    Ok(()) => self.debouncer.signal(now),
                    Err(TryRecvError::Empty) => break,
                    // The watcher ended on its own. Almost always this means it
                    // could not reach the sound server at all — it starts,
                    // fails to connect, and exits, which from here looks
                    // exactly like a watcher that is working and quiet.
                    Err(TryRecvError::Disconnected) => {
                        ended = true;
                        break;
                    }
                }
            }
        }
        if ended {
            self.unwatch();
            self.watch_unavailable = true;
            tracing::info!(
                "pane audio graph watcher ended immediately: the sound server could not be reached"
            );
        }
        self.debouncer.take_due(now)
    }

    /// Takes everything the reader threads have produced. Never blocks, and
    /// never waits on a thread: the server loop is the one place that cannot.
    pub(crate) fn drain(&mut self) -> Vec<CaptureEvent> {
        let mut events = Vec::new();
        let mut ended = Vec::new();
        for (pane_id, pane) in self.panes.iter_mut() {
            // The whole window is taken under one short lock and read
            // outside it: the recorder is never made to wait on the loop's
            // own work.
            let (taken, done) = match pane.frames.lock() {
                Ok(mut window) => (std::mem::take(&mut window.frames), window.ended),
                // The recorder panicked holding the lock; treat it as ended
                // rather than leaving a pane captured by nobody.
                Err(_) => (VecDeque::new(), true),
            };
            for (index, frame) in taken {
                if pane.opened {
                    events.push(CaptureEvent::Frame {
                        pane_id: pane_id.clone(),
                        frame,
                        offset: index.saturating_sub(pane.base),
                    });
                } else {
                    pane.opened = true;
                    pane.base = index;
                    events.push(CaptureEvent::Opened {
                        pane_id: pane_id.clone(),
                        frame,
                    });
                }
            }
            if done {
                ended.push(pane_id.clone());
            }
        }
        for pane_id in ended {
            self.panes.remove(&pane_id);
            events.push(CaptureEvent::Ended { pane_id });
        }
        for pane_id in self.stopped.drain(..) {
            events.push(CaptureEvent::Ended { pane_id });
        }
        events
    }

    /// Frames the loop was too slow to take. A number that climbs is a report
    /// about the loop, not about the sound.
    pub(crate) fn dropped_frames(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    pub(crate) fn active_panes(&self) -> usize {
        self.panes.len()
    }

    /// Whether this state is news.
    ///
    /// The server logs at info by default, so a line that only exists at debug
    /// is a line nobody reading a live log will ever see — and the whole point
    /// of this one is to answer "why is there no sound" without a rebuild. A
    /// state repeated every quarter second would be noise, so it is reported
    /// when it changes and only then.
    pub(crate) fn report_changed(&mut self, report: CaptureReport) -> bool {
        if self.last_report == Some(report) {
            return false;
        }
        self.last_report = Some(report);
        true
    }

    /// The panes being captured right now, which is what the plan compares
    /// against. Owned rather than borrowed so the caller can keep planning
    /// while it starts and stops.
    pub(crate) fn captured_panes(&self) -> BTreeSet<String> {
        self.panes.keys().cloned().collect()
    }
}

impl Drop for CaptureSupervisor {
    fn drop(&mut self) {
        self.stop_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::time::Duration;

    /// A source that hands over a written list of frames and then ends.
    ///
    /// Nothing here needs a recorder, so these run on a build box with no sound
    /// server: a test that needed pw-record would be a test about the machine.
    struct ScriptedSource {
        frames: VecDeque<Vec<u8>>,
        closed: Arc<AtomicU64>,
    }

    impl FrameSource for ScriptedSource {
        fn next_frame(&mut self) -> Result<Option<Vec<u8>>, AudioSourceError> {
            Ok(self.frames.pop_front())
        }

        fn close(&mut self) -> Result<(), AudioSourceError> {
            self.closed.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
    }

    /// A watcher that reports a written number of changes and then ends.
    struct ScriptedWatcher {
        remaining: u64,
    }

    impl GraphSignals for ScriptedWatcher {
        fn next_signal(&mut self) -> Result<bool, AudioSourceError> {
            if self.remaining == 0 {
                return Ok(false);
            }
            self.remaining -= 1;
            Ok(true)
        }

        fn close(&mut self) -> Result<(), AudioSourceError> {
            Ok(())
        }
    }

    fn frame(fill: u8) -> Vec<u8> {
        vec![fill; crate::app::pane_audio::FRAME_BYTES]
    }

    /// Counts how many times a capture was asked for, so a test can prove that
    /// nothing was started rather than that nothing was heard.
    fn counting_capture(
        frames: usize,
        starts: Arc<AtomicU64>,
        closed: Arc<AtomicU64>,
    ) -> CaptureFn {
        Box::new(move |_serial| {
            starts.fetch_add(1, Ordering::Relaxed);
            let scripted = ScriptedSource {
                frames: (0..frames).map(|i| frame(i as u8)).collect(),
                closed: Arc::clone(&closed),
            };
            Some(Ok(Box::new(scripted) as Box<dyn FrameSource>))
        })
    }

    fn no_watcher() -> WatcherFn {
        Box::new(|| {
            Some(Err(AudioSourceError::Unavailable(
                "not in this test".to_owned(),
            )))
        })
    }

    /// Waits until a condition holds, with a ceiling rather than a guess.
    ///
    /// The assertions themselves are exact — this only gives the reader thread
    /// room to finish, and fails loudly instead of asserting on a race.
    fn until(mut done: impl FnMut() -> bool) {
        for _ in 0..2_000 {
            if done() {
                return;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        panic!("the reader thread never finished");
    }

    #[test]
    fn nothing_is_started_until_a_pane_is_captured() {
        // SUP-1. The doctrine's plainest rule, and the one whose violation is
        // invisible: a recorder nobody asked for looks like one somebody did.
        let starts = Arc::new(AtomicU64::new(0));
        let mut supervisor = CaptureSupervisor::with_sources(
            counting_capture(0, Arc::clone(&starts), Arc::new(AtomicU64::new(0))),
            no_watcher(),
        );
        for _ in 0..50 {
            assert!(supervisor.drain().is_empty());
        }
        assert_eq!(starts.load(Ordering::Relaxed), 0);
        assert_eq!(supervisor.active_panes(), 0);
        assert!(!supervisor.is_watching());
    }

    #[test]
    fn the_first_frame_opens_and_the_rest_do_not() {
        // PRE-1 and PRE-2. The open carries the first frame because the server
        // dates every chunk from the open: opening early and waiting for sound
        // makes every frame late, which was measured as played=0.
        let mut supervisor = CaptureSupervisor::with_sources(
            counting_capture(3, Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0))),
            no_watcher(),
        );
        supervisor.start("w1:pA", 1).expect("start");
        let mut opened = 0;
        let mut frames = 0;
        until(|| {
            for event in supervisor.drain() {
                match event {
                    CaptureEvent::Opened { frame, .. } => {
                        assert_eq!(frames, 0, "a frame arrived before the open");
                        assert_eq!(frame.len(), crate::app::pane_audio::FRAME_BYTES);
                        opened += 1;
                    }
                    CaptureEvent::Frame { frame, .. } => {
                        assert_eq!(opened, 1, "a frame arrived before the open");
                        assert_eq!(frame.len(), crate::app::pane_audio::FRAME_BYTES);
                        frames += 1;
                    }
                    CaptureEvent::Ended { .. } => return true,
                }
            }
            false
        });
        assert_eq!(opened, 1, "exactly one open");
        assert_eq!(frames, 2, "the remaining frames are not opens");
    }

    /// A recorder that hands over one frame (or none) and then stays alive
    /// and silent until the test lets go of it. A scripted list of frames
    /// cannot play this part: it ends on its own, and a source that ends
    /// reports its own ending.
    struct HeldSource {
        first: Option<Vec<u8>>,
        held: std::sync::mpsc::Receiver<()>,
    }

    impl FrameSource for HeldSource {
        fn next_frame(&mut self) -> Result<Option<Vec<u8>>, AudioSourceError> {
            if let Some(frame) = self.first.take() {
                return Ok(Some(frame));
            }
            let _ = self.held.recv();
            Ok(None)
        }

        fn close(&mut self) -> Result<(), AudioSourceError> {
            Ok(())
        }
    }

    fn held_capture(first: bool) -> (CaptureFn, std::sync::mpsc::Sender<()>) {
        let (leash, held) = std::sync::mpsc::channel::<()>();
        let held = std::sync::Mutex::new(Some(held));
        let capture: CaptureFn = Box::new(move |_serial| {
            held.lock().unwrap().take().map(|held| {
                Ok(Box::new(HeldSource {
                    first: first.then(|| frame(1)),
                    held,
                }) as Box<dyn FrameSource>)
            })
        });
        (capture, leash)
    }

    /// TP-MEDIA-FLOW-02 · a frame the recorder queued wakes whoever waits on
    /// the supervisor's wake handle, so the loop need not look on a timer.
    #[tokio::test]
    async fn a_queued_frame_wakes_whoever_is_waiting() {
        let wake = Arc::new(tokio::sync::Notify::new());
        let mut supervisor = CaptureSupervisor::with_sources(
            counting_capture(1, Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0))),
            no_watcher(),
        )
        .wake_on(Arc::clone(&wake));
        supervisor.start("w1:pA", 1).expect("start");
        tokio::time::timeout(Duration::from_secs(2), wake.notified())
            .await
            .expect("the queued frame wakes the waiter, not the next timer");
        assert!(
            supervisor
                .drain()
                .iter()
                .any(|event| matches!(event, CaptureEvent::Opened { .. })),
            "and the frame is there to take"
        );
    }

    /// TP-MEDIA-FLOW-03 · a pane stopped after its stream opened is reported
    /// ended, once; one stopped before it opened is not.
    #[test]
    fn a_stopped_pane_is_reported_ended_once() {
        let (capture, _leash) = held_capture(true);
        let mut supervisor = CaptureSupervisor::with_sources(capture, no_watcher());
        supervisor.start("w1:pA", 1).expect("start");
        let mut opened = false;
        until(|| {
            opened |= supervisor
                .drain()
                .iter()
                .any(|event| matches!(event, CaptureEvent::Opened { .. }));
            opened
        });
        supervisor.stop("w1:pA");
        let ended: Vec<_> = supervisor
            .drain()
            .into_iter()
            .filter(|event| matches!(event, CaptureEvent::Ended { .. }))
            .collect();
        assert_eq!(
            ended,
            vec![CaptureEvent::Ended {
                pane_id: "w1:pA".to_owned()
            }],
            "the loop is told once that the stopped pane ended"
        );
        assert!(
            supervisor.drain().is_empty(),
            "and not again on the next drain"
        );

        // Stopped before a frame ever opened it: nothing to end.
        let (capture, _leash) = held_capture(false);
        let mut supervisor = CaptureSupervisor::with_sources(capture, no_watcher());
        supervisor.start("w1:pB", 2).expect("start");
        supervisor.stop("w1:pB");
        assert!(supervisor.drain().is_empty(), "no stream, no ending");
    }

    #[test]
    fn a_frame_is_a_whole_protocol_frame() {
        // SUP-6. The runtime takes samples, not bytes, and refuses any body of
        // the wrong length — so the wrong length here is silence, not an error
        // anybody sees.
        let mut supervisor = CaptureSupervisor::with_sources(
            counting_capture(1, Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0))),
            no_watcher(),
        );
        supervisor.start("w1:pA", 1).expect("start");
        let mut checked = false;
        until(|| {
            for event in supervisor.drain() {
                if let CaptureEvent::Opened { frame, .. } = event {
                    let pcm = crate::app::pane_audio::pcm_from_f32le(&frame)
                        .expect("the runtime accepts a captured frame");
                    assert_eq!(pcm.len(), crate::media::FRAME_SAMPLES * 2);
                    checked = true;
                }
            }
            checked
        });
    }

    #[test]
    fn a_source_that_ends_ends_its_pane_and_leaves_the_others() {
        // SUP-3. One pane's failure is not the others'.
        let mut supervisor = CaptureSupervisor::with_sources(
            Box::new(|serial| {
                let frames = if serial == 1 { 0 } else { 4 };
                Some(Ok(Box::new(ScriptedSource {
                    frames: (0..frames).map(|i| frame(i as u8)).collect(),
                    closed: Arc::new(AtomicU64::new(0)),
                }) as Box<dyn FrameSource>))
            }),
            no_watcher(),
        );
        supervisor.start("w1:silent", 1).expect("start");
        supervisor.start("w1:loud", 2).expect("start");
        let mut ended = Vec::new();
        let mut loud_frames = 0;
        until(|| {
            for event in supervisor.drain() {
                match event {
                    CaptureEvent::Ended { pane_id } => ended.push(pane_id),
                    CaptureEvent::Opened { pane_id, .. } | CaptureEvent::Frame { pane_id, .. } => {
                        assert_eq!(pane_id, "w1:loud");
                        loud_frames += 1;
                    }
                }
            }
            ended.len() == 2
        });
        assert_eq!(loud_frames, 4, "the live pane delivered everything");
        assert!(ended.contains(&"w1:silent".to_owned()));
        assert_eq!(supervisor.active_panes(), 0);
    }

    #[test]
    fn a_pane_can_be_captured_again_after_it_stops() {
        // SUP-2. A one-shot mistake in this family has been measured before.
        let starts = Arc::new(AtomicU64::new(0));
        let mut supervisor = CaptureSupervisor::with_sources(
            counting_capture(2, Arc::clone(&starts), Arc::new(AtomicU64::new(0))),
            no_watcher(),
        );
        supervisor.start("w1:pA", 1).expect("first start");
        supervisor.stop("w1:pA");
        assert_eq!(supervisor.active_panes(), 0);
        supervisor.start("w1:pA", 1).expect("second start");
        let mut delivered = 0;
        until(|| {
            for event in supervisor.drain() {
                if matches!(
                    event,
                    CaptureEvent::Opened { .. } | CaptureEvent::Frame { .. }
                ) {
                    delivered += 1;
                }
            }
            delivered == 2
        });
        assert_eq!(starts.load(Ordering::Relaxed), 2, "both starts happened");
    }

    #[test]
    fn starting_a_captured_pane_again_starts_nothing() {
        let starts = Arc::new(AtomicU64::new(0));
        let mut supervisor = CaptureSupervisor::with_sources(
            counting_capture(4, Arc::clone(&starts), Arc::new(AtomicU64::new(0))),
            no_watcher(),
        );
        supervisor.start("w1:pA", 1).expect("start");
        supervisor.start("w1:pA", 1).expect("start again");
        assert_eq!(starts.load(Ordering::Relaxed), 1);
        assert_eq!(supervisor.active_panes(), 1);
    }

    #[test]
    fn a_full_queue_drops_frames_instead_of_waiting() {
        // SUP-5. Exact rather than timed: every frame the source produced was
        // either delivered or dropped, whatever the scheduler did in between. A
        // blocking send would instead have hung this test, which is the point.
        let produced = 200;
        let mut supervisor = CaptureSupervisor::with_sources(
            counting_capture(
                produced,
                Arc::new(AtomicU64::new(0)),
                Arc::new(AtomicU64::new(0)),
            ),
            no_watcher(),
        );
        supervisor.start("w1:pA", 1).expect("start");
        let mut delivered = 0_u64;
        let mut ended = false;
        until(|| {
            for event in supervisor.drain() {
                match event {
                    CaptureEvent::Ended { .. } => ended = true,
                    _ => delivered += 1,
                }
            }
            ended
        });
        let dropped = supervisor.dropped_frames();
        assert!(dropped > 0, "a queue of {FRAME_QUEUE} could not hold 200");
        assert_eq!(
            delivered + dropped,
            produced as u64,
            "every frame was either delivered or dropped, none waited on"
        );
    }

    /// TP-MEDIA-FLOW-06 · the queue holds half a second: a loop away for as
    /// long as the client's jitter buffer could absorb loses nothing at the
    /// source.
    #[test]
    fn a_loop_away_for_half_a_second_loses_no_frame_at_the_source() {
        // Half a second of frames are offered to the queue before the loop
        // takes any. The recorder is scripted to end after them, and its
        // close is the signal that every frame has been offered — only then
        // does the loop drain, so the count owes nothing to the scheduler.
        // Eight frames (160 ms) used to be the ceiling, and a loop that
        // renders and encodes for a remote client is away longer than that
        // every turn; the queue then kept the stale frames and dropped the
        // fresh ones, and the listener heard nothing (K18, E-162).
        let produced = 500 / crate::media::FRAME_MS as usize;
        let closed = Arc::new(AtomicU64::new(0));
        let mut supervisor = CaptureSupervisor::with_sources(
            counting_capture(produced, Arc::new(AtomicU64::new(0)), Arc::clone(&closed)),
            no_watcher(),
        );
        supervisor.start("w1:pA", 1).expect("start");
        until(|| closed.load(Ordering::Relaxed) == 1);
        let delivered = supervisor
            .drain()
            .iter()
            .filter(|event| !matches!(event, CaptureEvent::Ended { .. }))
            .count();
        assert_eq!(
            supervisor.dropped_frames(),
            0,
            "half a second of frames fits the queue: none is dropped at the source"
        );
        assert_eq!(
            delivered, produced,
            "and every one of them reaches the loop in one drain"
        );
    }

    /// TP-MEDIA-FLOW-09 · an overflowing queue keeps the NEWEST frames.
    #[test]
    fn an_overflowing_queue_keeps_the_newest_frames_not_the_oldest() {
        // The recorder outruns the loop by a factor of two. WHICH frames the
        // loop finds decides whether the listener hears anything at all: the
        // queue holds QUEUE_MS of sound and the encoder refuses a chunk older
        // than the playout delay, so a queue that keeps its oldest hands the
        // loop nothing but frames already past their budget — every one of
        // them dropped, for as long as the producer stays ahead. Measured
        // live: `pw-record` at a full 384 kB/s, `dropped` climbing, two
        // listeners at zero chunks (K21). The video queue drops its oldest
        // for this exact reason; this is the audio side of the same rule.
        //
        // The frames are numbered in their own bytes, because the open
        // re-bases the offsets and so offsets alone cannot tell a stale
        // prefix from a fresh tail.
        let produced = FRAME_QUEUE * 2;
        let closed = Arc::new(AtomicU64::new(0));
        let mut supervisor = CaptureSupervisor::with_sources(
            counting_capture(produced, Arc::new(AtomicU64::new(0)), Arc::clone(&closed)),
            no_watcher(),
        );
        supervisor.start("w1:pA", 1).expect("start");
        // The source's close is how we know every frame was offered to the
        // queue with no drain in between.
        until(|| closed.load(Ordering::Relaxed) == 1);
        let marks: Vec<u8> = supervisor
            .drain()
            .into_iter()
            .filter_map(|event| match event {
                CaptureEvent::Opened { frame, .. } | CaptureEvent::Frame { frame, .. } => {
                    Some(frame[0])
                }
                CaptureEvent::Ended { .. } => None,
            })
            .collect();
        assert_eq!(
            marks.len(),
            FRAME_QUEUE,
            "the window holds its capacity and no more"
        );
        assert_eq!(
            supervisor.dropped_frames(),
            (produced - FRAME_QUEUE) as u64,
            "every frame the window pushed out is counted"
        );
        let expected: Vec<u8> = (FRAME_QUEUE as u8..produced as u8).collect();
        assert_eq!(
            marks, expected,
            "the survivors are the newest frames; a stale prefix here is the bug"
        );
    }

    #[test]
    fn stopping_closes_the_source_the_thread_owns() {
        // The orphan rule, one layer up: the thread that owns the recorder is
        // the thread that closes it.
        let closed = Arc::new(AtomicU64::new(0));
        let mut supervisor = CaptureSupervisor::with_sources(
            counting_capture(1, Arc::new(AtomicU64::new(0)), Arc::clone(&closed)),
            no_watcher(),
        );
        supervisor.start("w1:pA", 1).expect("start");
        until(|| closed.load(Ordering::Relaxed) == 1);
    }

    #[test]
    fn the_graph_settles_once_after_a_burst() {
        // WCH-2 at the supervisor's level: many signals, one look.
        let base = Instant::now();
        let mut supervisor = CaptureSupervisor::with_sources(
            counting_capture(0, Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0))),
            Box::new(|| {
                Some(Ok(
                    Box::new(ScriptedWatcher { remaining: 12 }) as Box<dyn GraphSignals>
                ))
            }),
        );
        supervisor.watch().expect("watch");
        assert!(supervisor.is_watching());
        until(|| {
            supervisor.graph_settled(base);
            supervisor.graph_settled(base + GRAPH_QUIET + Duration::from_millis(1))
        });
        // The burst is spent: quiet does not keep asking.
        assert!(!supervisor.graph_settled(base + Duration::from_secs(10)));
        supervisor.unwatch();
        assert!(!supervisor.is_watching());
    }

    #[test]
    fn a_watcher_that_ends_at_once_is_not_counted_as_watching() {
        // WCH-7. Measured live: without XDG_RUNTIME_DIR the watcher starts,
        // prints "can't connect", and exits — which from in here looks exactly
        // like a watcher that is working and quiet. Reporting "watching" for
        // something that stopped is how a dead feature keeps looking healthy,
        // and retrying it every tick would spawn a process a second for
        // nothing.
        let base = Instant::now();
        let mut supervisor = CaptureSupervisor::with_sources(
            counting_capture(0, Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0))),
            Box::new(|| {
                Some(Ok(
                    Box::new(ScriptedWatcher { remaining: 0 }) as Box<dyn GraphSignals>
                ))
            }),
        );
        supervisor.watch().expect("the watcher starts");
        until(|| {
            supervisor.graph_settled(base);
            !supervisor.is_watching()
        });
        // And it is not started again on the next tick.
        let err = supervisor
            .watch()
            .expect_err("a dead watcher is not retried");
        assert!(matches!(err, AudioSourceError::Unavailable(_)), "{err}");
        // A listener leaving clears it, so a later one gets a fresh try.
        supervisor.stop_all();
        assert!(supervisor.watch().is_ok(), "a new listener tries again");
    }

    #[test]
    fn a_platform_without_capture_is_an_error_not_a_panic() {
        let mut supervisor = CaptureSupervisor::with_sources(Box::new(|_| None), Box::new(|| None));
        let err = supervisor.start("w1:pA", 1).expect_err("no capture here");
        assert!(matches!(err, AudioSourceError::Unavailable(_)), "{err}");
        let err = supervisor.watch().expect_err("no watcher here");
        assert!(matches!(err, AudioSourceError::Unavailable(_)), "{err}");
        assert_eq!(supervisor.active_panes(), 0);
    }

    #[test]
    fn a_state_is_reported_when_it_changes_and_not_when_it_repeats() {
        // The server logs at info; a line only emitted at debug is a line the
        // person reading a live log never sees. So this one is emitted on
        // change — and a repeat has to be silent, or it is noise instead.
        let mut supervisor = CaptureSupervisor::with_sources(
            counting_capture(0, Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0))),
            no_watcher(),
        );
        let quiet = CaptureReport {
            watching: true,
            captured: 0,
            dropping: false,
            dropping_late: false,
            streams: 3,
            listeners: 1,
        };
        assert!(supervisor.report_changed(quiet), "the first state is news");
        assert!(!supervisor.report_changed(quiet), "a repeat is not");
        let capturing = CaptureReport {
            captured: 1,
            ..quiet
        };
        assert!(
            supervisor.report_changed(capturing),
            "a change is news again"
        );
        assert!(!supervisor.report_changed(capturing));
    }

    /// TP-MEDIA-FLOW-14 · a capture whose frames are being dropped as late is
    /// a different state from one whose frames are heard, and the line says
    /// so — a recorder at full speed, an empty queue and a silent listener is
    /// exactly this flag, and it was invisible for days (E-166).
    #[test]
    fn dropping_late_is_a_state_worth_reporting() {
        let mut supervisor = CaptureSupervisor::with_sources(
            counting_capture(0, Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0))),
            no_watcher(),
        );
        let heard = CaptureReport {
            watching: true,
            captured: 1,
            dropping: false,
            dropping_late: false,
            streams: 1,
            listeners: 1,
        };
        assert!(supervisor.report_changed(heard));
        let silent = CaptureReport {
            dropping_late: true,
            ..heard
        };
        assert!(
            supervisor.report_changed(silent),
            "frames thrown away at the clock is news"
        );
        assert!(!supervisor.report_changed(silent));
    }
}
