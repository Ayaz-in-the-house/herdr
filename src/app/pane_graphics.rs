use std::collections::{HashMap, VecDeque};
use std::hash::{Hash, Hasher};

use crate::api::schema::{PaneGraphicsFormat, PaneGraphicsPlacementParams};
use crate::layout::PaneId;

pub(crate) type Key = (PaneId, String);

pub(crate) const DIRECT_DELIVERY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
pub(crate) const DIRECT_RESPONSE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);
pub(crate) const DIRECT_OUTER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(9);

/// How many pictures one pane may hold for the video arm.
///
/// The loop's video tick was measured at ~7 Hz under load (2026-09-01, isolated
/// server, a 320x180 source at 15 fps): 360 ticks over roughly fifty seconds.
/// A 30 fps source therefore leaves about 4.3 pictures per tick, so eight is
/// two ticks of headroom.
pub(crate) const VIDEO_QUEUE_FRAMES: usize = 8;

/// ...and how many bytes, whichever binds first.
///
/// Eight 1080p pictures are 66 MB *per pane*, which breaks the resource
/// doctrine at exactly the resolution that matters most. When the byte budget
/// binds, the queue degrades toward the single slot it replaces — and that is
/// the right answer rather than a compromise, because 4K at 30 fps genuinely
/// cannot be buffered on the way to an encoder.
pub(crate) const VIDEO_QUEUE_BYTES: usize = 16 * 1024 * 1024;

/// One picture the producer pushed, waiting for the encoder.
///
/// It carries its own size because a resize is a new stream: the tick that
/// drains this must start the encoder for the size of *this* picture, not for
/// whatever the slot happens to hold by the time it looks.
#[derive(Debug)]
pub(crate) struct QueuedFrame {
    pub(crate) image_width: u32,
    pub(crate) image_height: u32,
    pub(crate) rgba: Vec<u8>,
    /// When the picture was recorded — the producer's moment, which becomes
    /// the chunk's `pts_us`. Taken here and never at the tick: pictures
    /// taken together in one tick are still apart in time. TP-VIDEO-PTS-03
    pub(crate) pts_us: u64,
}

/// What one pane has waiting, and what it last kept.
///
/// `last_fingerprint` outlives a drain on purpose: it is what makes an
/// unchanged picture cost nothing, and clearing it when the queue empties
/// would re-offer the same picture on every tick that follows.
#[derive(Debug, Default)]
struct VideoQueue {
    frames: VecDeque<QueuedFrame>,
    bytes: usize,
    last_fingerprint: Option<u64>,
}

#[derive(Debug)]
pub(crate) enum Backing {
    Inline(Vec<u8>),
    Direct(crate::pane_graphics_files::Lease),
    Resident { len: usize, client_id: u64 },
}

#[derive(Debug)]
pub(crate) struct Layer {
    pub(crate) format: PaneGraphicsFormat,
    pub(crate) image_width: u32,
    pub(crate) image_height: u32,
    pub(crate) backing: Backing,
    pub(crate) data_fingerprint: u64,
    pub(crate) render: PaneGraphicsPlacementParams,
    pub(crate) z_index: i32,
}

#[derive(Debug)]
pub(crate) struct DirectGate {
    pub(crate) transfer_id: u64,
    pub(crate) image_id: u32,
    pub(crate) client_id: u64,
    pub(crate) deadline: std::time::Instant,
    pub(crate) written: bool,
    pub(crate) success_response: String,
    pub(crate) respond_to: std::sync::mpsc::Sender<String>,
}

#[derive(Debug)]
pub(crate) struct Slot {
    pub(crate) host_image_id: u32,
    /// The picture the terminal draws.
    ///
    /// ⚠ Two arms read a pane's pictures and only one of them reads this
    /// field. The terminal draws whatever is here *now*; the video arm needs
    /// every picture that was here, in order, and reads
    /// [`Runtime::take_video_frames`] instead. A write that sets this field
    /// without recording the frame reaches the screen and never reaches the
    /// encoder — which is the whole of E-118, measured at nine percent of
    /// frames arriving. Install a picture through
    /// [`Runtime::set_slot_layer`] or [`Runtime::insert_slot`]; both record.
    pub(crate) layer: Option<Layer>,
    pub(crate) stream_owner: Option<String>,
    pub(crate) stream_active: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    pub(crate) direct_gate: Option<DirectGate>,
}

impl Slot {
    #[cfg(test)]
    pub(crate) fn test(host_image_id: u32, layer: Option<Layer>) -> Self {
        Self {
            host_image_id,
            layer,
            stream_owner: None,
            stream_active: None,
            direct_gate: None,
        }
    }

    pub(crate) fn stream_is_active(&self) -> bool {
        self.stream_active
            .as_ref()
            .is_some_and(|active| active.load(std::sync::atomic::Ordering::Acquire))
    }

    pub(crate) fn direct_client(&self) -> Option<u64> {
        self.direct_gate
            .as_ref()
            .map(|gate| gate.client_id)
            .or_else(|| self.layer.as_ref().and_then(Layer::resident_client))
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        if let Some(active) = &self.stream_active {
            active.store(false, std::sync::atomic::Ordering::Release);
        }
    }
}

#[derive(Debug)]
pub(crate) struct Runtime {
    pub(crate) slots: HashMap<Key, Slot>,
    revision: u64,
    next_host_image_id: u32,
    /// Whether anything is watching a pane's pictures as video.
    ///
    /// The gate the resource doctrine requires: with nobody watching, a push
    /// costs one boolean read and the queues stay empty. `pane_audio` states
    /// the same rule as credit — a frame is encoded exactly when somebody can
    /// take it.
    video_wanted: bool,
    video_queues: HashMap<Key, VideoQueue>,
    video_dropped: u64,
    /// The server's video arm, woken when a picture is recorded so it is fed
    /// on the picture rather than on the timer. TP-VIDEO-PTS-05
    video_wake: Option<std::sync::Arc<tokio::sync::Notify>>,
}

impl Default for Runtime {
    fn default() -> Self {
        Self {
            slots: HashMap::new(),
            revision: 0,
            next_host_image_id: 1 << 31,
            video_wanted: false,
            video_queues: HashMap::new(),
            video_dropped: 0,
            video_wake: None,
        }
    }
}

impl Runtime {
    pub(crate) fn revision(&self) -> u64 {
        self.revision
    }

    pub(crate) fn mark_changed(&mut self) {
        self.revision = self.revision.wrapping_add(1);
    }

    #[cfg(test)]
    pub(crate) fn clear(&mut self) {
        self.slots.clear();
        self.mark_changed();
    }

    pub(crate) fn reserve_image_id(&mut self, key: &Key) -> Option<u32> {
        if let Some(id) = self.slots.get(key).map(|slot| slot.host_image_id) {
            return Some(id);
        }
        for _ in 0..=self.slots.len() {
            let id = self.next_host_image_id | (1 << 31);
            self.next_host_image_id = id.wrapping_add(1) | (1 << 31);
            if self.slots.values().all(|slot| slot.host_image_id != id) {
                return Some(id);
            }
        }
        None
    }

    pub(crate) fn layer_count(&self, pane_id: PaneId) -> usize {
        self.slots.keys().filter(|(id, _)| *id == pane_id).count()
    }

    pub(crate) fn can_add_slot(&self, key: &Key) -> bool {
        self.slots.contains_key(key)
            || self.slots.len() < crate::api::schema::panes::PANE_GRAPHICS_MAX_LAYERS_TOTAL
    }

    pub(crate) fn can_store_inline(&self, key: &Key, len: usize) -> bool {
        let replaced = self
            .slots
            .get(key)
            .and_then(|slot| slot.layer.as_ref())
            .and_then(Layer::inline_data)
            .map_or(0, <[u8]>::len);
        self.inline_bytes()
            .saturating_sub(replaced)
            .saturating_add(len)
            <= crate::api::schema::panes::PANE_GRAPHICS_MAX_INLINE_BYTES_TOTAL
    }

    fn inline_bytes(&self) -> usize {
        self.slots
            .values()
            .filter_map(|slot| slot.layer.as_ref())
            .filter_map(Layer::inline_data)
            .map(<[u8]>::len)
            .sum()
    }

    pub(crate) fn active_for_pane(&self, pane_id: PaneId) -> bool {
        self.slots
            .iter()
            .any(|((id, _), slot)| *id == pane_id && slot.layer.is_some())
    }

    pub(crate) fn attach_stream_active(
        &mut self,
        key: &Key,
        owner: &str,
        active: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) {
        if let Some(slot) = self
            .slots
            .get_mut(key)
            .filter(|slot| slot.stream_owner.as_deref() == Some(owner))
        {
            slot.stream_active = Some(active);
        }
    }

    pub(crate) fn retain_live_panes(&mut self, state: &crate::app::state::AppState) -> bool {
        // Swept beside the slots, and only here: a queue keyed on a pane that
        // no longer exists would hold its last pictures until the process
        // ended, and its `last_fingerprint` would outlive the pane it
        // described.
        self.video_queues.retain(|(pane_id, _), _| {
            state
                .workspaces
                .iter()
                .any(|workspace| workspace.pane_state(*pane_id).is_some())
        });
        if self.slots.is_empty() {
            return false;
        }
        let before = self.slots.len();
        self.slots.retain(|(pane_id, _), _| {
            state
                .workspaces
                .iter()
                .any(|workspace| workspace.pane_state(*pane_id).is_some())
        });
        if self.slots.len() == before {
            return false;
        }
        self.mark_changed();
        true
    }

    /// Install a picture on a slot that already exists, recording it for the
    /// video arm on the way in.
    ///
    /// One of the two write points for `Slot::layer`, and the reason both
    /// exist: the terminal reads the slot and the video arm reads the queue,
    /// so a write that only does the first is invisible to video. Returns
    /// false when the slot is gone, which the callers already had to handle.
    ///
    /// It installs a picture and does nothing else. In particular it leaves
    /// `direct_gate` alone: one caller retires the gate after installing and
    /// another still needs it, and folding that into here removed the gate
    /// from under the second — which retired the whole slot, several frames
    /// away from anything that mentioned graphics.
    pub(crate) fn set_slot_layer(&mut self, key: &Key, layer: Layer) -> bool {
        self.set_slot_layer_at(key, layer, crate::media::now_us())
    }

    /// `set_slot_layer` with the picture's moment given, which is what the
    /// production path passes as `now` and a test passes as a number.
    pub(crate) fn set_slot_layer_at(&mut self, key: &Key, layer: Layer, pts_us: u64) -> bool {
        if !self.slots.contains_key(key) {
            return false;
        }
        // Recorded before it is installed, while the layer is still owned
        // here: recording afterwards would have to borrow it back out of the
        // slot that was just mutably borrowed to write it.
        self.record_video_frame(key, &layer, pts_us);
        let slot = self.slots.get_mut(key).expect("checked just above");
        slot.layer = Some(layer);
        true
    }

    /// Install a whole slot, recording its picture if it has one.
    ///
    /// The other write point. A slot arriving with a layer already in it is
    /// how the one-shot `pane.graphics.set` path works, and it is a picture
    /// like any other.
    pub(crate) fn insert_slot(&mut self, key: Key, slot: Slot) {
        self.insert_slot_at(key, slot, crate::media::now_us());
    }

    /// `insert_slot` with the picture's moment given.
    pub(crate) fn insert_slot_at(&mut self, key: Key, slot: Slot, pts_us: u64) {
        if let Some(layer) = slot.layer.as_ref() {
            self.record_video_frame(&key, layer, pts_us);
        }
        self.slots.insert(key, slot);
    }

    /// Hands the runtime the server's video wake. TP-VIDEO-PTS-05
    pub(crate) fn set_video_wake(&mut self, wake: std::sync::Arc<tokio::sync::Notify>) {
        self.video_wake = Some(wake);
    }

    /// Whether this runtime wakes exactly `handle`. A test's question.
    #[cfg(test)]
    pub(crate) fn wakes(&self, handle: &std::sync::Arc<tokio::sync::Notify>) -> bool {
        self.video_wake
            .as_ref()
            .is_some_and(|wake| std::sync::Arc::ptr_eq(wake, handle))
    }

    /// Turn video recording on or off. Off empties every queue.
    ///
    /// Called from the video tick with whether any client can decode. Turning
    /// it off drops what is held rather than keeping it warm: the pictures
    /// waiting are already stale by the time anyone connects again, and a
    /// queue that survives its last viewer is memory nobody asked for.
    ///
    /// ⭐ Turning it **on** is an event, and seeds the queue from whatever is
    /// already on screen. Without that, a viewer attaching to a pane that is
    /// showing something waits for the producer to draw again — and a still
    /// picture is exactly the case where it never will, so the stream would
    /// simply never open. The same shape as the geometry publisher, which
    /// learned this the same day: change detection alone cannot see the
    /// change that is *attaching*. TP-VIDEO-FEED-07
    pub(crate) fn set_video_wanted(&mut self, wanted: bool) {
        if self.video_wanted == wanted {
            return;
        }
        self.video_wanted = wanted;
        if !wanted {
            self.video_queues.clear();
            return;
        }
        // Collected before it is pushed, because pushing borrows the queues
        // while reading borrows the slots. One copy per pane, once, at the
        // moment a viewer arrives.
        let seeds: Vec<(Key, u32, u32, u64, Vec<u8>)> = self
            .slots
            .iter()
            .filter_map(|(key, slot)| {
                let layer = slot.layer.as_ref()?;
                let rgba = layer.inline_data()?;
                Some((
                    key.clone(),
                    layer.image_width,
                    layer.image_height,
                    layer.data_fingerprint,
                    rgba.to_vec(),
                ))
            })
            .collect();
        let now_us = crate::media::now_us();
        for (key, image_width, image_height, fingerprint, rgba) in seeds {
            let queue = self.video_queues.entry(key).or_default();
            queue.last_fingerprint = Some(fingerprint);
            queue.bytes += rgba.len();
            queue.frames.push_back(QueuedFrame {
                image_width,
                image_height,
                rgba,
                pts_us: now_us,
            });
        }
    }

    /// Remember one picture for the video arm.
    ///
    /// Costs a boolean read while nobody is watching. Only `Backing::Inline`
    /// is kept, for the reason the video tap records: `Direct` and `Resident`
    /// mean the client reads the picture without the wire paying for it.
    fn record_video_frame(&mut self, key: &Key, layer: &Layer, pts_us: u64) {
        if !self.video_wanted {
            return;
        }
        let Some(rgba) = layer.inline_data() else {
            return;
        };
        let queue = self.video_queues.entry(key.clone()).or_default();
        // The same picture twice is the same bytes twice. Recognised here
        // rather than at the tick so the copy below never happens for it.
        if queue.last_fingerprint == Some(layer.data_fingerprint) {
            return;
        }
        queue.last_fingerprint = Some(layer.data_fingerprint);
        queue.bytes += rgba.len();
        queue.frames.push_back(QueuedFrame {
            image_width: layer.image_width,
            image_height: layer.image_height,
            rgba: rgba.to_vec(),
            pts_us,
        });
        let mut dropped = 0_u64;
        // The oldest goes first. A live decoder wants the newest picture it
        // can get; dropping the newest instead would make the delay permanent,
        // which is the failure the audio side measured and refused. One frame
        // always survives, so a single oversized picture is still encoded
        // rather than dropped forever.
        while queue.frames.len() > VIDEO_QUEUE_FRAMES
            || (queue.bytes > VIDEO_QUEUE_BYTES && queue.frames.len() > 1)
        {
            let Some(gone) = queue.frames.pop_front() else {
                break;
            };
            queue.bytes = queue.bytes.saturating_sub(gone.rgba.len());
            dropped += 1;
        }
        self.video_dropped += dropped;
        // Woken per picture, so the arm feeds the encoder now rather than on
        // its timer. Only while wanted: an idle session draws without
        // waking anyone. TP-VIDEO-PTS-05
        if let Some(wake) = self.video_wake.as_ref() {
            wake.notify_one();
        }
    }

    /// Everything the producer pushed since the last drain, per pane, in the
    /// order it was pushed.
    ///
    /// Order matters within a pane and not between them: each pane is its own
    /// encoder and its own stream, so two panes' pictures never interleave
    /// into one decoder.
    pub(crate) fn take_video_frames(&mut self) -> Vec<(Key, QueuedFrame)> {
        let mut taken = Vec::new();
        for (key, queue) in &mut self.video_queues {
            if queue.frames.is_empty() {
                continue;
            }
            queue.bytes = 0;
            for frame in queue.frames.drain(..) {
                taken.push((key.clone(), frame));
            }
        }
        taken
    }

    /// How many pictures were dropped because the producer outran the tick.
    ///
    /// Reported rather than merely counted, for the reason the encoder's own
    /// drop counter exists: a policy that never says it fired cannot be told
    /// from a bug, and "the video stutters" has a dozen other causes.
    pub(crate) fn video_frames_dropped(&self) -> u64 {
        self.video_dropped
    }

    /// Test-only: production learns what is queued by draining it.
    #[cfg(test)]
    pub(crate) fn video_queued_frames(&self) -> usize {
        self.video_queues.values().map(|q| q.frames.len()).sum()
    }

    /// Test-only: the byte budget is a claim about memory, so a test reads it.
    #[cfg(test)]
    pub(crate) fn video_queued_bytes(&self) -> usize {
        self.video_queues.values().map(|q| q.bytes).sum()
    }
}

impl Layer {
    pub(crate) fn inline(
        format: PaneGraphicsFormat,
        image_width: u32,
        image_height: u32,
        data: Vec<u8>,
        render: PaneGraphicsPlacementParams,
        z_index: i32,
    ) -> Self {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        data.hash(&mut hasher);
        Self {
            format,
            image_width,
            image_height,
            backing: Backing::Inline(data),
            data_fingerprint: hasher.finish(),
            render,
            z_index,
        }
    }

    pub(crate) fn direct(
        image_width: u32,
        image_height: u32,
        lease: crate::pane_graphics_files::Lease,
        render: PaneGraphicsPlacementParams,
        z_index: i32,
    ) -> Self {
        let data_fingerprint = lease.fingerprint();
        Self {
            format: PaneGraphicsFormat::Rgba,
            image_width,
            image_height,
            backing: Backing::Direct(lease),
            data_fingerprint,
            render,
            z_index,
        }
    }

    pub(crate) fn data_len(&self) -> usize {
        match &self.backing {
            Backing::Inline(data) => data.len(),
            Backing::Direct(lease) => lease.len(),
            Backing::Resident { len, .. } => *len,
        }
    }

    pub(crate) fn inline_data(&self) -> Option<&[u8]> {
        match &self.backing {
            Backing::Inline(data) => Some(data),
            Backing::Direct(_) | Backing::Resident { .. } => None,
        }
    }

    pub(crate) fn direct_lease(&self) -> Option<&crate::pane_graphics_files::Lease> {
        match &self.backing {
            Backing::Direct(lease) => Some(lease),
            Backing::Inline(_) | Backing::Resident { .. } => None,
        }
    }

    pub(crate) fn mark_resident(&mut self, client_id: u64) -> bool {
        let Backing::Direct(lease) = &self.backing else {
            return false;
        };
        self.backing = Backing::Resident {
            len: lease.len(),
            client_id,
        };
        true
    }

    pub(crate) fn resident_client(&self) -> Option<u64> {
        match self.backing {
            Backing::Resident { client_id, .. } => Some(client_id),
            Backing::Inline(_) | Backing::Direct(_) => None,
        }
    }

    pub(crate) fn terminal_only(&self) -> bool {
        matches!(self.backing, Backing::Direct(_) | Backing::Resident { .. })
    }
}

#[cfg(test)]
mod tests {
    /// TP-VIDEO-PTS-03 · a recorded picture carries the moment it was
    /// recorded, so pictures taken together in one tick are still apart in
    /// time. Stamping at the tick gave 86 of 923 pictures the same moment
    /// (E-149, K12).
    #[test]
    fn two_pictures_recorded_apart_keep_their_own_moments() {
        let mut runtime = Runtime::default();
        runtime.set_video_wanted(true);
        let key = (PaneId::from_raw(1), "primary".into());
        runtime.slots.insert(key.clone(), Slot::test(1 << 31, None));
        let layer = |fill: u8| {
            Layer::inline(
                PaneGraphicsFormat::Rgba,
                2,
                2,
                vec![fill; 16],
                PaneGraphicsPlacementParams::default(),
                0,
            )
        };
        assert!(runtime.set_slot_layer_at(&key, layer(1), 1_000));
        assert!(runtime.set_slot_layer_at(&key, layer(2), 31_000));
        let stamps: Vec<u64> = runtime
            .take_video_frames()
            .into_iter()
            .map(|(_, frame)| frame.pts_us)
            .collect();
        assert_eq!(stamps, vec![1_000, 31_000], "taken together, stamped apart");
    }

    /// TP-VIDEO-PTS-05 · a recorded picture wakes the video arm.
    #[tokio::test]
    async fn a_recorded_picture_wakes_the_video_arm() {
        let wake = std::sync::Arc::new(tokio::sync::Notify::new());
        let mut runtime = Runtime::default();
        runtime.set_video_wake(std::sync::Arc::clone(&wake));
        runtime.set_video_wanted(true);
        let key = (PaneId::from_raw(1), "primary".into());
        queue_one(&mut runtime, &key, 2, 7);
        tokio::time::timeout(std::time::Duration::from_secs(5), wake.notified())
            .await
            .expect("a recorded picture wakes the arm");
        assert!(runtime.wakes(&wake));
    }

    /// TP-VIDEO-PTS-05 · nobody watching, nobody woken: the resource
    /// doctrine's rule, a push costs one boolean read while idle.
    #[tokio::test]
    async fn an_unwatched_picture_wakes_nobody() {
        let wake = std::sync::Arc::new(tokio::sync::Notify::new());
        let mut runtime = Runtime::default();
        runtime.set_video_wake(std::sync::Arc::clone(&wake));
        let key = (PaneId::from_raw(1), "primary".into());
        queue_one(&mut runtime, &key, 2, 7);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), wake.notified())
                .await
                .is_err(),
            "video is not wanted, so the arm sleeps"
        );
    }
    use super::*;

    #[test]
    fn stream_liveness_attaches_only_to_the_exact_owned_layer() {
        let mut runtime = Runtime::default();
        let first = (PaneId::from_raw(1), "first".into());
        let second = (PaneId::from_raw(2), "second".into());
        for (key, id) in [(&first, 1), (&second, 2)] {
            runtime.slots.insert(
                key.clone(),
                Slot {
                    host_image_id: (1 << 31) | id,
                    layer: None,
                    stream_owner: Some("shared-owner".into()),
                    stream_active: None,
                    direct_gate: None,
                },
            );
        }
        let active = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));

        runtime.attach_stream_active(&second, "shared-owner", active.clone());

        assert!(runtime.slots[&first].stream_active.is_none());
        assert!(std::sync::Arc::ptr_eq(
            runtime.slots[&second].stream_active.as_ref().unwrap(),
            &active
        ));
    }

    /// A picture of `size` filled with `fill`, in a slot that already exists.
    fn queue_one(runtime: &mut Runtime, key: &Key, size: u32, fill: u8) {
        if !runtime.slots.contains_key(key) {
            runtime.slots.insert(key.clone(), Slot::test(1 << 31, None));
        }
        runtime.set_slot_layer(
            key,
            Layer::inline(
                PaneGraphicsFormat::Rgba,
                size,
                size,
                vec![fill; (size * size * 4) as usize],
                PaneGraphicsPlacementParams::default(),
                0,
            ),
        );
    }

    /// TP-VIDEO-FEED-03 · the queue is bounded, and the oldest goes first.
    ///
    /// Unbounded, a loop that stopped draining would take the process with it.
    /// The *oldest* is dropped because a live decoder wants the newest picture
    /// it can get: dropping the newest would keep the delay rather than the
    /// gap, which is the failure the audio side measured and refused.
    #[test]
    fn the_frame_queue_keeps_the_newest_and_drops_the_oldest() {
        let mut runtime = Runtime::default();
        runtime.set_video_wanted(true);
        let key = (PaneId::from_raw(1), "primary".into());

        let overflow = 3_u8;
        for step in 0..(VIDEO_QUEUE_FRAMES as u8 + overflow) {
            queue_one(&mut runtime, &key, 2, step);
        }

        let taken = runtime.take_video_frames();
        assert_eq!(
            taken.len(),
            VIDEO_QUEUE_FRAMES,
            "the queue holds its bound and no more"
        );
        assert_eq!(
            taken.first().map(|(_, frame)| frame.rgba[0]),
            Some(overflow),
            "the three oldest were dropped, so the front is the fourth picture pushed"
        );
        assert_eq!(
            taken.last().map(|(_, frame)| frame.rgba[0]),
            Some(VIDEO_QUEUE_FRAMES as u8 + overflow - 1),
            "and the newest survived, which is the one a decoder wants"
        );
        assert_eq!(
            runtime.video_frames_dropped(),
            u64::from(overflow),
            "the drop is counted, so a policy that fired can say so"
        );
    }

    /// TP-VIDEO-FEED-04 · a byte budget binds before the frame count on large
    /// pictures.
    ///
    /// Eight 1080p frames are 66 MB per pane. Bounding only by count would
    /// break the resource doctrine at exactly the resolution where video
    /// matters most, and would do it silently. One frame always survives, so
    /// a single oversized picture is still encoded rather than dropped for
    /// good.
    #[test]
    fn a_large_picture_is_bounded_by_bytes_rather_than_by_count() {
        let mut runtime = Runtime::default();
        runtime.set_video_wanted(true);
        let key = (PaneId::from_raw(1), "primary".into());

        // 1024x1024 RGBA is 4 MiB, so five would be 20 MiB against a 16 MiB
        // budget: the byte bound has to bite before the count bound does.
        for step in 0..5_u8 {
            queue_one(&mut runtime, &key, 1024, step);
        }

        assert!(
            runtime.video_queued_bytes() <= VIDEO_QUEUE_BYTES,
            "held {} bytes against a {VIDEO_QUEUE_BYTES} budget",
            runtime.video_queued_bytes()
        );
        let taken = runtime.take_video_frames();
        assert!(
            taken.len() < VIDEO_QUEUE_FRAMES,
            "bytes bound before the frame count did, holding {} pictures",
            taken.len()
        );
        assert!(
            !taken.is_empty(),
            "and at least one picture survives, or a large source is never encoded at all"
        );
        assert_eq!(
            taken.last().map(|(_, frame)| frame.rgba[0]),
            Some(4),
            "what survives is the newest"
        );
    }

    /// TP-VIDEO-FEED-05 · a drained queue still remembers what it last kept.
    ///
    /// The fingerprint has to outlive the drain. Clearing it with the frames
    /// would re-offer the same picture on every tick that followed, which is
    /// the cost `data_fingerprint` exists to avoid.
    #[test]
    fn an_unchanged_picture_is_not_kept_twice_across_a_drain() {
        let mut runtime = Runtime::default();
        runtime.set_video_wanted(true);
        let key = (PaneId::from_raw(1), "primary".into());

        queue_one(&mut runtime, &key, 2, 0x9);
        assert_eq!(runtime.take_video_frames().len(), 1);
        queue_one(&mut runtime, &key, 2, 0x9);

        assert!(
            runtime.take_video_frames().is_empty(),
            "the same picture written again after a drain is still the same picture"
        );
    }

    /// TP-VIDEO-FEED-07 · a viewer arriving is itself an event.
    ///
    /// Change detection cannot see the change that is *attaching*. A pane
    /// showing a still picture writes nothing more, so a queue that only fills
    /// on writes would never hand that pane's picture to an encoder and the
    /// stream would never open — the viewer would wait forever on a screen
    /// that is, from its point of view, working perfectly.
    #[test]
    fn turning_video_on_seeds_the_queue_from_what_is_already_on_screen() {
        let mut runtime = Runtime::default();
        let key = (PaneId::from_raw(1), "primary".into());
        // Drawn while nobody was watching, and never drawn again.
        queue_one(&mut runtime, &key, 4, 0x5a);
        assert_eq!(runtime.video_queued_frames(), 0, "kept nothing, correctly");

        runtime.set_video_wanted(true);

        let taken = runtime.take_video_frames();
        assert_eq!(
            taken.len(),
            1,
            "the picture already on screen is handed over when a viewer arrives"
        );
        assert_eq!(taken[0].1.rgba[0], 0x5a);
        assert_eq!(taken[0].1.image_width, 4);

        // And it is remembered, so the next tick does not offer it again.
        runtime.set_slot_layer(
            &key,
            Layer::inline(
                PaneGraphicsFormat::Rgba,
                4,
                4,
                vec![0x5a; 4 * 4 * 4],
                PaneGraphicsPlacementParams::default(),
                0,
            ),
        );
        assert!(
            runtime.take_video_frames().is_empty(),
            "the seed counts as having been kept, or a still screen re-encodes forever"
        );
    }

    /// TP-VIDEO-FEED-02 · a picture written while nobody watches costs nothing.
    ///
    /// The counts after the gate opens belong to TP-VIDEO-FEED-07 and are
    /// asserted here only so this test cannot pass by the queue being broken
    /// in both directions: one seed for what was already on screen, then one
    /// more for the next distinct picture.
    #[test]
    fn nothing_is_kept_until_something_wants_video() {
        let mut runtime = Runtime::default();
        let key = (PaneId::from_raw(1), "primary".into());

        queue_one(&mut runtime, &key, 64, 1);
        assert_eq!(runtime.video_queued_frames(), 0, "nothing is kept");
        assert_eq!(runtime.video_queued_bytes(), 0, "and nothing is held");

        runtime.set_video_wanted(true);
        assert_eq!(
            runtime.video_queued_frames(),
            1,
            "opening the gate seeds what is already on screen (TP-VIDEO-FEED-07)"
        );
        queue_one(&mut runtime, &key, 64, 2);
        assert_eq!(
            runtime.video_queued_frames(),
            2,
            "and keeps recording from there"
        );

        runtime.set_video_wanted(false);
        assert_eq!(
            runtime.video_queued_frames(),
            0,
            "and stops, dropping what it was holding for a viewer that left"
        );
        assert_eq!(runtime.video_queued_bytes(), 0);
    }

    #[test]
    fn runtime_bounds_total_slots_and_inline_bytes() {
        let mut runtime = Runtime::default();
        for index in 0..crate::api::schema::panes::PANE_GRAPHICS_MAX_LAYERS_TOTAL {
            let key = (PaneId::from_raw(1), format!("layer-{index}"));
            runtime
                .slots
                .insert(key, Slot::test((1 << 31) | index as u32, None));
        }
        assert!(!runtime.can_add_slot(&(PaneId::from_raw(2), "extra".into())));

        let mut runtime = Runtime::default();
        let key = (PaneId::from_raw(1), "primary".into());
        runtime.slots.insert(
            key.clone(),
            Slot::test(
                1 << 31,
                Some(Layer::inline(
                    PaneGraphicsFormat::Rgba,
                    1,
                    1,
                    vec![0; 4],
                    PaneGraphicsPlacementParams::default(),
                    0,
                )),
            ),
        );
        assert!(runtime.can_store_inline(
            &key,
            crate::api::schema::panes::PANE_GRAPHICS_MAX_INLINE_BYTES_TOTAL,
        ));
        assert!(!runtime.can_store_inline(
            &(PaneId::from_raw(2), "primary".into()),
            crate::api::schema::panes::PANE_GRAPHICS_MAX_INLINE_BYTES_TOTAL,
        ));
    }
}
