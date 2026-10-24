//! The server side of one audio stream: sequencing, timestamps, credit, and
//! the decision to drop.
//!
//! The decision that shapes this module is *where* a chunk is dropped. A queue
//! that grows under load delivers every chunk eventually, each one later than
//! the last, and the delay never comes back down — the link recovers and the
//! backlog does not. Dropping at the source is the opposite: the stream thins
//! out while the link is bad and is instantly current again when it is not.
//!
//! That only works here, because only here is the chunk's deadline still known.
//! By the time bytes are in a writer queue they are anonymous.

use super::opus::AudioEncoder;
use super::{MediaError, CHANNELS, FRAME_MS, FRAME_SAMPLES};

/// Playout delay the server suggests when it opens a stream: 100 ms, inside
/// the jitter buffer's 10..500 ms range and small enough to feel live.
pub const TARGET_LATENCY_US: u32 = 100_000;

/// How far past its presentation time a chunk may still be worth sending.
///
/// The playout delay, exactly. The client holds every chunk for that long
/// before playing it, so a chunk late by less still lands in its own slot;
/// one later than that would land behind a slot the client has already
/// played past, and sending it spends bandwidth to make the stream briefly
/// worse. The two numbers are one number on purpose. The budget used to be
/// a single frame, and a loop that also renders and encodes for a remote
/// client is routinely more than 20 ms behind: every frame it delivered was
/// thrown away here, and a listener beside a playing client heard nothing
/// while the recorder counted drops (K18, E-162). TP-MEDIA-FLOW-05
pub const MAX_LATENESS_US: u64 = TARGET_LATENCY_US as u64;

/// What happened to one frame offered to the stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChunkFate {
    /// Encoded and ready to queue.
    Send {
        seq: u64,
        pts_us: u64,
        data: Vec<u8>,
    },
    /// The chunk's moment had already passed when it was offered.
    DroppedLate { seq: u64, pts_us: u64 },
    /// The client has no room for it.
    ///
    /// Distinct from `DroppedLate` because it means something different about
    /// the far end: late is the network, no credit is the client's own
    /// playback falling behind, and treating them alike hides which one is
    /// happening.
    DroppedNoCredit { seq: u64, pts_us: u64 },
}

/// One outbound audio stream.
pub struct AudioStream {
    stream_id: u32,
    pane_id: String,
    encoder: AudioEncoder,
    /// Next sequence number. Monotonic and never reused, including across
    /// drops: a gap is how the client learns a chunk is missing, so a stream
    /// that renumbers to stay dense makes loss invisible.
    next_seq: u64,
    /// Presentation time of the next frame, in the server's clock.
    next_pts_us: u64,
    /// When the stream opened, in the server's clock: what a recorder offset
    /// is counted from.
    start_pts_us: u64,
    /// Chunks the client has room for.
    credit: u16,
    packet: Vec<u8>,
}

impl AudioStream {
    /// Opens a stream whose first frame presents at `start_pts_us`.
    pub fn new(
        stream_id: u32,
        pane_id: impl Into<String>,
        bitrate_bps: i32,
        start_pts_us: u64,
    ) -> Result<Self, MediaError> {
        Ok(Self {
            stream_id,
            pane_id: pane_id.into(),
            encoder: AudioEncoder::new(bitrate_bps)?,
            next_seq: 0,
            next_pts_us: start_pts_us,
            start_pts_us,
            credit: 0,
            packet: vec![0u8; 4000],
        })
    }

    pub fn stream_id(&self) -> u32 {
        self.stream_id
    }

    pub fn pane_id(&self) -> &str {
        &self.pane_id
    }

    pub fn credit(&self) -> u16 {
        self.credit
    }

    /// Records the room the client says it has.
    ///
    /// Replaces rather than adds. The client reports a level, not a delta, so
    /// a lost or duplicated credit message costs one stale reading instead of
    /// permanently shifting the server's idea of the far end's buffer.
    pub fn set_credit(&mut self, chunks: u16) {
        self.credit = chunks;
    }

    /// Offers one frame of interleaved samples.
    ///
    /// `now_us` is the server's clock. Time advances by one frame whatever the
    /// outcome: a dropped chunk still consumed its slot, and a stream whose
    /// timestamps pause during a drop plays the rest of the audio late by the
    /// length of the outage.
    /// Offers a frame at the recorder's `offset` from the open: its moment is
    /// the open plus `offset` frames whatever was lost on the way, and its
    /// `seq` is that offset, so the loss shows as a gap. The next `offer`
    /// continues after it.
    ///
    /// Counting offered frames instead — what `offer` does, and what a
    /// producer that never loses a frame can rely on — leaves the clock one
    /// frame behind for every frame lost between the recorder and this call,
    /// and once it is a frame behind every later frame is late and dropped
    /// before it is encoded. TP-MEDIA-FLOW-01
    pub fn offer_at(
        &mut self,
        pcm: &[f32],
        offset: u64,
        now_us: u64,
    ) -> Result<ChunkFate, MediaError> {
        self.next_seq = offset;
        self.next_pts_us = self.start_pts_us + offset * FRAME_MS as u64 * 1000;
        self.offer(pcm, now_us)
    }

    pub fn offer(&mut self, pcm: &[f32], now_us: u64) -> Result<ChunkFate, MediaError> {
        let expected = FRAME_SAMPLES * CHANNELS as usize;
        if pcm.len() != expected {
            return Err(MediaError::FrameSize {
                expected,
                got: pcm.len(),
            });
        }

        let seq = self.next_seq;
        let pts_us = self.next_pts_us;
        self.next_seq += 1;
        self.next_pts_us += FRAME_MS as u64 * 1000;

        if now_us > pts_us + MAX_LATENESS_US {
            return Ok(ChunkFate::DroppedLate { seq, pts_us });
        }
        if self.credit == 0 {
            return Ok(ChunkFate::DroppedNoCredit { seq, pts_us });
        }

        let written = self.encoder.encode(pcm, &mut self.packet)?;
        self.credit -= 1;
        Ok(ChunkFate::Send {
            seq,
            pts_us,
            data: self.packet[..written].to_vec(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::DEFAULT_BITRATE_BPS;

    fn frame() -> Vec<f32> {
        (0..FRAME_SAMPLES * CHANNELS as usize)
            .map(|i| ((i as f32) * 0.001).sin() * 0.4)
            .collect()
    }

    fn stream() -> AudioStream {
        AudioStream::new(1, "w1:p1", DEFAULT_BITRATE_BPS, 1_000_000).expect("stream")
    }

    // TP-MEDIA-SOURCE-01
    #[test]
    fn a_chunk_past_its_moment_is_dropped_before_it_is_encoded() {
        // Dropped at the source, which is the only place the deadline is still
        // known. A queue that holds it instead delivers it eventually — and
        // every chunk after it, each one later than the last, with the delay
        // never coming back down once the link recovers.
        let mut audio = stream();
        audio.set_credit(10);

        let fate = audio
            .offer(&frame(), 1_000_000 + MAX_LATENESS_US + 1)
            .expect("offer");
        assert_eq!(
            fate,
            ChunkFate::DroppedLate {
                seq: 0,
                pts_us: 1_000_000
            }
        );
        assert_eq!(
            audio.credit(),
            10,
            "a dropped chunk must not spend the client's room"
        );
    }

    // TP-MEDIA-SOURCE-01
    /// TP-MEDIA-FLOW-01 · a frame offered at the recorder's offset presents
    /// at its own moment, whatever was offered before it.
    #[test]
    fn a_frame_offered_at_the_recorders_offset_presents_at_its_own_moment() {
        let mut audio = stream();
        audio.set_credit(3);
        match audio.offer_at(&frame(), 0, 1_000_000).expect("offer") {
            ChunkFate::Send { seq, pts_us, .. } => {
                assert_eq!((seq, pts_us), (0, 1_000_000), "the opening frame");
            }
            other => panic!("the opening frame is sent: {other:?}"),
        }
        // Nine frames were lost on the way. The tenth is 5 ms old: fresh.
        match audio.offer_at(&frame(), 10, 1_205_000).expect("offer") {
            ChunkFate::Send { seq, pts_us, .. } => {
                assert_eq!(seq, 10, "seq is the offset, so the loss is a gap");
                assert_eq!(pts_us, 1_200_000, "its moment is open + 10 frames");
            }
            other => panic!("a fresh frame after a loss is sent: {other:?}"),
        }
        // A plain offer continues after it.
        match audio.offer(&frame(), 1_225_000).expect("offer") {
            ChunkFate::Send { seq, pts_us, .. } => {
                assert_eq!((seq, pts_us), (11, 1_220_000));
            }
            other => panic!("the next frame follows: {other:?}"),
        }
    }

    #[test]
    fn a_dropped_chunk_still_consumes_its_slot_in_time_and_sequence() {
        // The subtle half. If time paused during a drop, everything after the
        // outage would present late by the length of the outage — the audio
        // would be permanently behind rather than briefly thin. And if the
        // sequence numbers closed up, the client would see an unbroken stream
        // and never learn anything was lost.
        let mut audio = stream();
        audio.set_credit(10);

        let dropped = audio
            .offer(&frame(), 1_000_000 + MAX_LATENESS_US + 1)
            .expect("offer");
        assert!(matches!(dropped, ChunkFate::DroppedLate { seq: 0, .. }));

        let sent = audio.offer(&frame(), 1_000_000).expect("offer");
        match sent {
            ChunkFate::Send { seq, pts_us, .. } => {
                assert_eq!(seq, 1, "the sequence must gap where a chunk was lost");
                assert_eq!(pts_us, 1_020_000, "time advances through a drop");
            }
            other => panic!("expected Send, got {other:?}"),
        }
    }

    // TP-MEDIA-SOURCE-01
    #[test]
    fn a_client_with_no_room_is_not_sent_to() {
        // Without this the server's memory grows instead of the client's
        // queue, and the first symptom of a slow client is the server rather
        // than the playback.
        let mut audio = stream();
        assert_eq!(audio.credit(), 0);

        let fate = audio.offer(&frame(), 1_000_000).expect("offer");
        assert!(matches!(fate, ChunkFate::DroppedNoCredit { seq: 0, .. }));
    }

    // TP-MEDIA-SOURCE-01
    #[test]
    fn credit_is_spent_one_chunk_at_a_time_and_replaced_not_accumulated() {
        // Replaced, because the client reports a level rather than a delta. A
        // lost or duplicated credit message then costs one stale reading, where
        // an accumulating counter would shift the server's idea of the far end
        // permanently and in a direction nothing corrects.
        let mut audio = stream();
        audio.set_credit(2);

        assert!(matches!(
            audio.offer(&frame(), 1_000_000),
            Ok(ChunkFate::Send { seq: 0, .. })
        ));
        assert_eq!(audio.credit(), 1);
        assert!(matches!(
            audio.offer(&frame(), 1_020_000),
            Ok(ChunkFate::Send { seq: 1, .. })
        ));
        assert_eq!(audio.credit(), 0);
        assert!(matches!(
            audio.offer(&frame(), 1_040_000),
            Ok(ChunkFate::DroppedNoCredit { seq: 2, .. })
        ));

        audio.set_credit(5);
        assert_eq!(audio.credit(), 5, "credit is a level, not a running total");
    }

    // TP-MEDIA-SOURCE-01
    #[test]
    fn lateness_up_to_the_playout_delay_is_still_worth_sending() {
        // The boundary is a decision, not an accident: a chunk exactly the
        // playout delay late still lands inside the client's jitter buffer,
        // and dropping it would spend a gap to save nothing.
        let mut audio = stream();
        audio.set_credit(10);
        let fate = audio
            .offer(&frame(), 1_000_000 + MAX_LATENESS_US)
            .expect("offer");
        assert!(matches!(fate, ChunkFate::Send { seq: 0, .. }));
    }

    /// TP-MEDIA-FLOW-05 · a backlog drained at once is not all late: every
    /// frame still inside the client's playout delay is sent, and only the
    /// frames the client has already played past are dropped.
    #[test]
    fn a_backlog_drained_at_once_keeps_every_frame_inside_the_playout_delay() {
        // The loop was away for eight frames — rendering for a remote
        // client, say — and drains them in one go with one reading of the
        // clock. The client holds each chunk for the playout delay before
        // playing it, so a chunk late by less than that still plays in its
        // own slot. A lateness budget of one frame instead throws seven of
        // the eight away, and a loop that is away that long every turn
        // starves the listener: that was heard as silence beside a playing
        // client while the recorder counted drops (K18, E-162).
        let frame_us = FRAME_MS as u64 * 1000;
        let playout_us = u64::from(crate::app::pane_audio::TARGET_LATENCY_US);
        let mut audio = stream();
        audio.set_credit(10);
        let now_us = 1_000_000 + 8 * frame_us;
        let mut sent = Vec::new();
        let mut late = Vec::new();
        for offset in 0..8 {
            match audio.offer_at(&frame(), offset, now_us).expect("offer") {
                ChunkFate::Send { seq, .. } => sent.push(seq),
                ChunkFate::DroppedLate { seq, .. } => late.push(seq),
                other => panic!("credit was granted: {other:?}"),
            }
        }
        assert_eq!(
            late,
            vec![0, 1, 2],
            "only the frames the client has already played past are dropped"
        );
        assert_eq!(
            sent,
            vec![3, 4, 5, 6, 7],
            "every frame still inside the playout delay is sent"
        );
        assert_eq!(
            MAX_LATENESS_US, playout_us,
            "the lateness budget is the playout delay: the two are one number"
        );
    }

    // TP-MEDIA-SOURCE-01
    #[test]
    fn a_wrong_sized_frame_is_refused_without_consuming_a_slot() {
        // A caller mistake must not move the stream's clock. If it did, every
        // rejected frame would shift the audio by 20 ms and the drift would
        // have no visible cause.
        let mut audio = stream();
        audio.set_credit(10);
        assert_eq!(
            audio.offer(&[0.0f32; 8], 1_000_000),
            Err(MediaError::FrameSize {
                expected: FRAME_SAMPLES * CHANNELS as usize,
                got: 8
            })
        );

        let fate = audio.offer(&frame(), 1_000_000).expect("offer");
        match fate {
            ChunkFate::Send { seq, pts_us, .. } => {
                assert_eq!(seq, 0);
                assert_eq!(pts_us, 1_000_000);
            }
            other => panic!("expected Send, got {other:?}"),
        }
    }
}
