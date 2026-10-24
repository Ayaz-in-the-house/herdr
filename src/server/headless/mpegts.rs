//! The transport the encoder's pictures arrive in.
//!
//! ffmpeg's output is read from a pipe, and a pipe hands over bytes, not
//! pictures. Annex-B alone cannot say where a picture ends — a decoder finds the
//! boundary at the *next* picture's start code, which for a live stream means
//! every picture is known to be complete one frame late. Measured 2026-09-02
//! at 10 fps: a picture's end was known 101 ms after it was handed to the
//! encoder with Annex-B and with fragmented MP4 (whose muxer holds a sample
//! until it knows the sample's duration), and **1.3 ms** with MPEG-TS whose
//! PES length is written (`-omit_video_pes_length 0`). The length is the whole
//! reason for the container; nothing else in it is used.
//!
//! This is the reader for that: MPEG-TS packets in, whole Annex-B access units
//! out, each one handed over the moment its last byte arrives. Written from
//! ISO 13818-1 §2.4.3 and measured against `scripts/pane_video_ts_fixture.py`,
//! an independent demuxer, over bytes ffmpeg's own muxer produced — three
//! implementations that have to agree, so a disagreement names the wrong one.
//!
//! Pure: bytes in, bytes out, no clock and no process.

use std::collections::VecDeque;

/// Every transport packet is this long.
pub(crate) const PACKET_BYTES: usize = 188;
/// The byte every packet starts with.
pub(crate) const SYNC_BYTE: u8 = 0x47;
/// The program association table's PID.
const PAT_PID: u16 = 0;
/// `stream_type` for H.264 video in a program map table.
const H264_STREAM_TYPE: u8 = 0x1B;

/// A video PES whose last byte has not arrived.
#[derive(Debug)]
struct Pending {
    body: Vec<u8>,
    /// How long the body will be, when the PES said. `None` is a length of
    /// zero on the wire — allowed for video, and the case this reader exists
    /// to avoid — which only the next picture's start can end.
    expected: Option<usize>,
}

/// Reads MPEG-TS bytes and yields whole pictures.
#[derive(Debug, Default)]
pub(crate) struct TransportDemux {
    buf: Vec<u8>,
    pmt_pid: Option<u16>,
    video_pid: Option<u16>,
    pending: Option<Pending>,
    pictures: VecDeque<Vec<u8>>,
    unbounded: u64,
    truncated: u64,
    resyncs: u64,
}

impl TransportDemux {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Feeds bytes as they came off the pipe, in any sized pieces.
    pub(crate) fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
        loop {
            if self.buf.len() < PACKET_BYTES {
                return;
            }
            if self.buf[0] != SYNC_BYTE && !self.resync() {
                return;
            }
            let packet: [u8; PACKET_BYTES] = self.buf[..PACKET_BYTES]
                .try_into()
                .expect("exactly one packet");
            self.buf.drain(..PACKET_BYTES);
            self.read_packet(&packet);
        }
    }

    /// The next complete picture, as Annex-B bytes, if one has arrived.
    pub(crate) fn next_picture(&mut self) -> Option<Vec<u8>> {
        self.pictures.pop_front()
    }

    /// Pictures whose PES length was not written, so their end was only
    /// known when the next picture began. Counted because that is one frame
    /// of delay each, and an encoder flag change would show up here first.
    pub(crate) fn unbounded_pictures(&self) -> u64 {
        self.unbounded
    }

    /// Pictures that ended before the length they promised — dropped rather
    /// than handed to a decoder as a whole picture.
    pub(crate) fn truncated_pictures(&self) -> u64 {
        self.truncated
    }

    /// Times the reader lost the packet boundary and searched for it again.
    pub(crate) fn resyncs(&self) -> u64 {
        self.resyncs
    }

    /// Finds the next byte that starts a packet: a sync byte with another
    /// sync byte one packet later. One sync byte alone is any byte that
    /// happens to be 0x47. Returns whether the buffer now starts on a packet;
    /// `false` means more bytes are needed, and what cannot be a packet start
    /// has been dropped.
    fn resync(&mut self) -> bool {
        self.resyncs += 1;
        let len = self.buf.len();
        for i in 0..len {
            if self.buf[i] != SYNC_BYTE {
                continue;
            }
            if i + PACKET_BYTES >= len {
                // A candidate whose partner is not visible yet.
                self.buf.drain(..i);
                return false;
            }
            if self.buf[i + PACKET_BYTES] == SYNC_BYTE {
                self.buf.drain(..i);
                return true;
            }
        }
        self.buf.clear();
        false
    }

    fn read_packet(&mut self, packet: &[u8; PACKET_BYTES]) {
        let start = packet[1] & 0x40 != 0;
        let pid = u16::from(packet[1] & 0x1F) << 8 | u16::from(packet[2]);
        let adaptation = (packet[3] >> 4) & 0x3;
        let mut at = 4;
        if adaptation & 0x2 != 0 {
            at += 1 + usize::from(packet[4]);
        }
        if adaptation & 0x1 == 0 || at > PACKET_BYTES {
            return;
        }
        let payload = &packet[at..];
        if pid == PAT_PID {
            if start {
                self.read_pat(payload);
            }
        } else if Some(pid) == self.pmt_pid {
            if start {
                self.read_pmt(payload);
            }
        } else if Some(pid) == self.video_pid {
            self.read_video(start, payload);
        }
    }

    /// A section as it sits in a packet that starts one: after the pointer
    /// field, up to and excluding the CRC.
    fn section(payload: &[u8]) -> Option<&[u8]> {
        let pointer = usize::from(*payload.first()?);
        let section = payload.get(1 + pointer..)?;
        if section.len() < 12 {
            return None;
        }
        let length = usize::from(section[1] & 0x0F) << 8 | usize::from(section[2]);
        let end = (3 + length).saturating_sub(4).min(section.len());
        Some(&section[..end])
    }

    fn read_pat(&mut self, payload: &[u8]) {
        let Some(section) = Self::section(payload) else {
            return;
        };
        for program in section.get(8..).unwrap_or(&[]).chunks_exact(4) {
            let number = u16::from(program[0]) << 8 | u16::from(program[1]);
            if number != 0 {
                self.pmt_pid = Some(u16::from(program[2] & 0x1F) << 8 | u16::from(program[3]));
                return;
            }
        }
    }

    fn read_pmt(&mut self, payload: &[u8]) {
        let Some(section) = Self::section(payload) else {
            return;
        };
        let info = usize::from(section[10] & 0x0F) << 8 | usize::from(section[11]);
        let mut i = 12 + info;
        while i + 5 <= section.len() {
            let stream_type = section[i];
            let pid = u16::from(section[i + 1] & 0x1F) << 8 | u16::from(section[i + 2]);
            let es_info = usize::from(section[i + 3] & 0x0F) << 8 | usize::from(section[i + 4]);
            if stream_type == H264_STREAM_TYPE && self.video_pid.is_none() {
                self.video_pid = Some(pid);
            }
            i += 5 + es_info;
        }
    }

    fn read_video(&mut self, start: bool, payload: &[u8]) {
        if start {
            if let Some(pending) = self.pending.take() {
                // A new picture starting is the only end an unbounded PES
                // has; for one that promised a length it is a picture cut
                // short.
                match pending.expected {
                    None => {
                        self.unbounded += 1;
                        self.pictures.push_back(pending.body);
                    }
                    Some(_) => self.truncated += 1,
                }
            }
            if payload.len() < 9 || payload[..3] != [0, 0, 1] {
                return;
            }
            if !(0xE0..=0xEF).contains(&payload[3]) {
                return;
            }
            let length = usize::from(payload[4]) << 8 | usize::from(payload[5]);
            let header = usize::from(payload[8]);
            let Some(body) = payload.get(9 + header..) else {
                return;
            };
            // The length counts from the byte after itself: two flag bytes,
            // the header length byte, the header, then the body.
            let expected = (length != 0).then(|| length.saturating_sub(3 + header));
            self.pending = Some(Pending {
                body: body.to_vec(),
                expected,
            });
        } else if let Some(pending) = self.pending.as_mut() {
            pending.body.extend_from_slice(payload);
        }
        let complete = matches!(
            self.pending.as_ref(),
            Some(Pending { body, expected: Some(expected) }) if body.len() >= *expected
        );
        if complete {
            let Some(mut pending) = self.pending.take() else {
                return;
            };
            if let Some(expected) = pending.expected {
                pending.body.truncate(expected);
            }
            self.pictures.push_back(pending.body);
        }
    }
}

#[cfg(test)]
pub(crate) mod test_stream {
    //! A tiny MPEG-TS writer for tests, written from the same standard as the
    //! reader and deliberately not sharing a line with it.

    use super::{PACKET_BYTES, SYNC_BYTE};

    /// One transport packet. A short payload is padded with an adaptation
    /// field, which is how a muxer ends a PES that does not fill a packet.
    pub(crate) fn packet(pid: u16, start: bool, payload: &[u8]) -> Vec<u8> {
        assert!(
            payload.len() <= PACKET_BYTES - 4,
            "payload does not fit a packet"
        );
        let mut out = Vec::with_capacity(PACKET_BYTES);
        out.push(SYNC_BYTE);
        out.push(((start as u8) << 6) | ((pid >> 8) as u8 & 0x1F));
        out.push(pid as u8);
        let room = PACKET_BYTES - 4;
        if payload.len() == room {
            out.push(0x10); // payload only
        } else {
            out.push(0x30); // adaptation field, then payload
            let field_len = room - payload.len() - 1;
            out.push(field_len as u8);
            if field_len > 0 {
                out.push(0x00);
                out.extend(std::iter::repeat_n(0xFF, field_len - 1));
            }
        }
        out.extend_from_slice(payload);
        assert_eq!(out.len(), PACKET_BYTES);
        out
    }

    /// A program association table naming one program at `pmt_pid`.
    pub(crate) fn pat(pmt_pid: u16) -> Vec<u8> {
        let mut section = vec![0x00, 0xB0, 0x0D, 0x00, 0x01, 0xC1, 0x00, 0x00];
        section.extend([0x00, 0x01, 0xE0 | (pmt_pid >> 8) as u8, pmt_pid as u8]);
        section.extend([0xDE, 0xAD, 0xBE, 0xEF]); // the reader does not check the CRC
        let mut payload = vec![0x00]; // pointer field
        payload.extend(section);
        packet(0, true, &payload)
    }

    /// A program map table listing `streams` as `(stream_type, pid)`, in order.
    pub(crate) fn pmt(pmt_pid: u16, streams: &[(u8, u16)]) -> Vec<u8> {
        let length = 9 + 5 * streams.len() + 4;
        let mut section = vec![0x02, 0xB0 | (length >> 8) as u8, length as u8];
        section.extend([0x00, 0x01, 0xC1, 0x00, 0x00]);
        let pcr_pid = streams.first().map(|(_, pid)| *pid).unwrap_or(0x1FFF);
        section.extend([0xE0 | (pcr_pid >> 8) as u8, pcr_pid as u8, 0xF0, 0x00]);
        for (stream_type, pid) in streams {
            section.extend([
                *stream_type,
                0xE0 | (pid >> 8) as u8,
                *pid as u8,
                0xF0,
                0x00,
            ]);
        }
        section.extend([0xDE, 0xAD, 0xBE, 0xEF]);
        let mut payload = vec![0x00];
        payload.extend(section);
        packet(pmt_pid, true, &payload)
    }

    /// One PES packet carrying `body`, split over as many transport packets as
    /// it needs. With `write_length` the PES says how long it is; without it,
    /// the length field is zero and the end is only implied by the next start.
    pub(crate) fn pes(pid: u16, body: &[u8], write_length: bool) -> Vec<u8> {
        let header_data_length = 5u8; // a PTS
        let mut pes = vec![0x00, 0x00, 0x01, 0xE0];
        let length = if write_length {
            3 + header_data_length as usize + body.len()
        } else {
            0
        };
        assert!(length <= 0xFFFF, "a written length fits sixteen bits");
        pes.extend([
            (length >> 8) as u8,
            length as u8,
            0x80,
            0x80,
            header_data_length,
        ]);
        pes.extend([0x21, 0x00, 0x01, 0x00, 0x01]); // PTS 0
        pes.extend_from_slice(body);
        let mut out = Vec::new();
        let mut first = true;
        for piece in pes.chunks(PACKET_BYTES - 4) {
            out.extend(packet(pid, first, piece));
            first = false;
        }
        out
    }

    /// The smallest access unit that reads as a picture: delimiter and one
    /// slice, IDR or not, followed by `fill` so sizes can differ.
    pub(crate) fn access_unit(key: bool, fill: &[u8]) -> Vec<u8> {
        let mut au = vec![0x00, 0x00, 0x00, 0x01, 0x09, 0xF0];
        au.extend([0x00, 0x00, 0x00, 0x01, if key { 0x65 } else { 0x41 }, 0x88]);
        au.extend_from_slice(fill);
        au
    }

    /// PAT, PMT with one H.264 stream on `video_pid`, then the given pictures,
    /// each as a length-written PES.
    pub(crate) fn program(video_pid: u16, pictures: &[Vec<u8>]) -> Vec<u8> {
        let mut out = pat(0x1000);
        out.extend(pmt(0x1000, &[(super::H264_STREAM_TYPE, video_pid)]));
        for picture in pictures {
            out.extend(pes(video_pid, picture, true));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::test_stream::{access_unit, packet, pat, pes, pmt, program};
    use super::*;
    use crate::media::h264::access_unit_has_idr;

    /// The stream ffmpeg's muxer wrote, and the referee's description of it.
    const GOLDEN: &[u8] = include_bytes!("../../../tests/fixtures/pane_video_frames.ts");
    const MANIFEST: &str = include_str!("../../../tests/fixtures/pane_video_frames.json");

    fn manifest() -> serde_json::Value {
        serde_json::from_str(MANIFEST).expect("the manifest is json")
    }

    fn drain(demux: &mut TransportDemux) -> Vec<Vec<u8>> {
        std::iter::from_fn(|| demux.next_picture()).collect()
    }

    fn all_at_once(bytes: &[u8]) -> Vec<Vec<u8>> {
        let mut demux = TransportDemux::new();
        demux.push(bytes);
        drain(&mut demux)
    }

    /// TP-VIDEO-AU-01 · ⭐⭐ the golden stream yields every picture the referee
    /// describes, at the size the referee measured, with the key pictures
    /// where the referee found them.
    ///
    /// Three implementations meet here: ffmpeg wrote the bytes, the Python
    /// referee described them from the standard, and this reader must agree
    /// with both. A reader that agreed only with itself could split on the
    /// wrong field and pass its own tests forever.
    #[test]
    fn the_golden_stream_yields_every_picture_the_referee_describes() {
        let described = manifest();
        let expected = described["pictures"].as_array().expect("pictures");
        assert_eq!(expected.len(), 5, "the fixture is five pictures");
        assert_eq!(
            described["total_bytes"].as_u64().expect("total_bytes") as usize,
            GOLDEN.len(),
            "the manifest describes the bytes that are committed"
        );

        let pictures = all_at_once(GOLDEN);
        assert_eq!(pictures.len(), expected.len(), "one picture per PES");
        for (picture, want) in pictures.iter().zip(expected) {
            let index = want["index"].as_u64().unwrap();
            assert_eq!(
                picture.len(),
                want["size"].as_u64().unwrap() as usize,
                "picture {index}: size"
            );
            assert_eq!(
                want["starts_with_aud"].as_bool().unwrap(),
                picture.starts_with(&[0, 0, 0, 1, 0x09]),
                "picture {index}: the muxer's access unit delimiter is where the picture starts"
            );
            assert_eq!(
                access_unit_has_idr(picture),
                want["key"].as_bool().unwrap(),
                "picture {index}: key"
            );
        }
        let mut demux = TransportDemux::new();
        demux.push(GOLDEN);
        drain(&mut demux);
        assert_eq!(demux.unbounded_pictures(), 0, "every length was written");
        assert_eq!(demux.resyncs(), 0, "a clean stream is never resynced");
    }

    /// TP-VIDEO-AU-01 · a picture arrives whole however the pipe cut it.
    ///
    /// A pipe returns whatever is there: one byte, a packet and a half, three
    /// pictures at once. The reader is the only thing between that and a
    /// decoder handed half a picture.
    #[test]
    fn a_picture_arrives_whole_however_the_bytes_were_cut() {
        let whole = all_at_once(GOLDEN);
        for cut in [1usize, 7, 100, 188, 189, 1000, 4096] {
            let mut demux = TransportDemux::new();
            let mut got = Vec::new();
            for piece in GOLDEN.chunks(cut) {
                demux.push(piece);
                got.extend(drain(&mut demux));
            }
            assert_eq!(got, whole, "cut every {cut} bytes");
        }
    }

    /// TP-VIDEO-AU-01 · ⭐ a picture's end is known from its length, not from
    /// the next picture — the property the container was chosen for.
    ///
    /// The unbounded form is still read, one picture late, and counted, so a
    /// muxer flag that quietly went missing shows up as a number rather than
    /// as "the video feels laggy".
    #[test]
    fn a_pictures_end_is_known_from_its_length_and_not_from_the_next_picture() {
        let first = access_unit(true, &[1; 300]);
        let second = access_unit(false, &[2; 40]);

        let mut demux = TransportDemux::new();
        demux.push(&pat(0x1000));
        demux.push(&pmt(0x1000, &[(H264_STREAM_TYPE, 0x100)]));
        demux.push(&pes(0x100, &first, true));
        assert_eq!(
            demux.next_picture().as_deref(),
            Some(first.as_slice()),
            "with the length written, the picture is out before anything follows it"
        );
        assert_eq!(demux.unbounded_pictures(), 0);

        let mut demux = TransportDemux::new();
        demux.push(&pat(0x1000));
        demux.push(&pmt(0x1000, &[(H264_STREAM_TYPE, 0x100)]));
        demux.push(&pes(0x100, &first, false));
        assert_eq!(
            demux.next_picture(),
            None,
            "with no length, the reader cannot know the picture is over"
        );
        demux.push(&pes(0x100, &second, false));
        assert_eq!(
            demux.next_picture().as_deref(),
            Some(first.as_slice()),
            "the next picture's start is what ends it"
        );
        assert_eq!(demux.next_picture(), None);
        assert_eq!(demux.unbounded_pictures(), 1, "and the lateness is counted");
    }

    /// TP-VIDEO-AU-01 · the video stream is found by its type, not by being
    /// first in the table.
    #[test]
    fn the_video_stream_is_found_by_its_type_and_not_by_its_position() {
        let picture = access_unit(true, &[7; 50]);
        let mut demux = TransportDemux::new();
        demux.push(&pat(0x1000));
        demux.push(&pmt(0x1000, &[(0x0F, 0x101), (H264_STREAM_TYPE, 0x102)]));
        demux.push(&pes(0x101, b"aac-ish bytes that are not a picture", true));
        demux.push(&pes(0x102, &picture, true));
        assert_eq!(drain(&mut demux), vec![picture]);
    }

    /// TP-VIDEO-AU-01 · bytes before the first sync byte are skipped, and the
    /// stream still reads.
    #[test]
    fn garbage_before_the_first_sync_byte_is_skipped() {
        let mut bytes = b"junk\x47junk".to_vec();
        bytes.extend_from_slice(GOLDEN);
        let mut demux = TransportDemux::new();
        demux.push(&bytes);
        assert_eq!(drain(&mut demux), all_at_once(GOLDEN));
        assert!(demux.resyncs() >= 1, "and the search is counted");
    }

    /// TP-VIDEO-AU-01 · nothing is a picture before the program is known.
    ///
    /// The negative twin: a reader that took any PES on any PID would pass
    /// every test above and also deliver the audio track as video.
    #[test]
    fn nothing_is_a_picture_before_the_program_is_known() {
        let mut demux = TransportDemux::new();
        demux.push(&pes(0x100, &access_unit(true, &[3; 20]), true));
        demux.push(&packet(0x1FFF, false, &[0xFF; 184]));
        assert_eq!(drain(&mut demux), Vec::<Vec<u8>>::new());
    }

    /// The test writer's own shape check: the packets it makes are packets.
    #[test]
    fn the_test_writer_makes_whole_packets() {
        let bytes = program(
            0x100,
            &[access_unit(true, &[0; 500]), access_unit(false, &[])],
        );
        assert_eq!(bytes.len() % PACKET_BYTES, 0);
        for pkt in bytes.chunks(PACKET_BYTES) {
            assert_eq!(pkt[0], SYNC_BYTE);
        }
    }
}
