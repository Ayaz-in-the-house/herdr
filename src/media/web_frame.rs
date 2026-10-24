//! The frame format the web client already speaks.
//!
//! Not ours to design: `herdr-web/src/media-framing.ts` defines it, its decoder
//! is already written and has ten tests, and its own comment says the header is
//! laid out "to sit flat against the fork's `MediaChunk { stream_id, seq,
//! pts_us, data }`". Emitting that format means the consumer needs no change.
//! Inventing a second one would mean two decoders that have to agree, which is
//! a class of bug this project has already paid for.
//!
//! ```text
//!   offset  size  field
//!        0     1  magic   'M' (0x4D)
//!        1     1  version 1
//!        2     1  kind    0=chunk 1=open 2=close 3=timesync
//!        3     1  flags
//!        4     4  stream_id   u32 LE
//!        8     8  seq         u64 LE
//!       16     8  pts_us      u64 LE
//!       24     4  payload_len u32 LE
//!       28     n  payload
//! ```
//!
//! Pure on purpose: no socket, no process, no clock. Everything that can be
//! wrong about the bytes is decidable without any of them.

/// `'M'`. The decoder throws on anything else rather than guessing.
pub const WEB_FRAME_MAGIC: u8 = 0x4D;

/// Format version. The web decoder validates this byte too, so it is
/// load-bearing and not decoration: a bumped version on one side and not the
/// other stops the stream instead of silently misreading it.
pub const WEB_FRAME_VERSION: u8 = 1;

/// Fixed header width. Fixed rather than varint by the web side's stated
/// reasoning: a few bytes per frame are cheaper than a decoder that has to
/// agree with a Rust one about edge cases.
pub const WEB_FRAME_HEADER_BYTES: usize = 28;

/// A ceiling that is a refusal, not an allocation.
///
/// Mirrors `DEFAULT_MAX_PAYLOAD` in `media-framing.ts`. The two must not drift:
/// if this side emits what that side refuses, the stream dies on a frame that
/// was never malformed.
pub const WEB_FRAME_MAX_PAYLOAD: usize = 8 * 1024 * 1024;

/// Flag bit on a video chunk: the picture holds an IDR slice, so a decoder
/// may start here.
///
/// Derived once, by the listener, from the bytes it already has
/// (`media::h264`), so a player joining mid-stream need not parse what was
/// parsed for it. Bit zero, and pinned: the player switches on it.
pub const WEB_FRAME_FLAG_KEY: u8 = 0b0000_0001;

/// What a frame carries. The web side switches on this byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum WebFrameKind {
    Chunk = 0,
    Open = 1,
    Close = 2,
    TimeSync = 3,
}

/// One frame, borrowing its payload.
///
/// Borrowed rather than owned because a stream emits fifty of these a second
/// per pane, and the payload always already exists somewhere else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebFrame<'a> {
    pub kind: WebFrameKind,
    pub flags: u8,
    pub stream_id: u32,
    pub seq: u64,
    pub pts_us: u64,
    pub payload: &'a [u8],
}

/// Anything the framing refuses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebFrameError {
    /// Larger than either side will hold. Carries what was asked so the caller
    /// can say which frame, not just that one was refused.
    TooLarge { got: usize },
}

impl std::fmt::Display for WebFrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLarge { got } => write!(
                f,
                "media frame payload {got} exceeds {WEB_FRAME_MAX_PAYLOAD}"
            ),
        }
    }
}

impl std::error::Error for WebFrameError {}

impl WebFrame<'_> {
    /// Appends this frame to `out`.
    ///
    /// Appends rather than returning a `Vec` so a caller can keep one buffer
    /// for the life of a stream. Nothing is written when the frame is refused:
    /// a partially written frame in a shared buffer is worse than no frame,
    /// because the next one would be read as its continuation.
    pub fn encode_into(&self, out: &mut Vec<u8>) -> Result<(), WebFrameError> {
        let payload_len = self.payload.len();
        // Refused before the reserve below, not after: a corrupt length must
        // not be able to make this side allocate for a frame it will never
        // write. Both ends refuse at the same number, so a frame accepted here
        // is a frame the web decoder will take.
        if payload_len > WEB_FRAME_MAX_PAYLOAD {
            return Err(WebFrameError::TooLarge { got: payload_len });
        }
        // Total, not lossy: the ceiling above is 8 MiB, far inside u32, so
        // there is no truncation for this cast to hide.
        let declared = payload_len as u32;

        out.reserve(WEB_FRAME_HEADER_BYTES + payload_len);
        out.push(WEB_FRAME_MAGIC);
        out.push(WEB_FRAME_VERSION);
        out.push(self.kind as u8);
        out.push(self.flags);
        out.extend_from_slice(&self.stream_id.to_le_bytes());
        out.extend_from_slice(&self.seq.to_le_bytes());
        out.extend_from_slice(&self.pts_us.to_le_bytes());
        out.extend_from_slice(&declared.to_le_bytes());
        out.extend_from_slice(self.payload);
        Ok(())
    }
}

/// The body of an `Open` frame.
///
/// JSON with named fields, and only because this travels **once per stream**:
/// the hot path is `Chunk`, which carries the encoded packet and nothing else.
/// Spending a wrapper there would repeat fifty times a second per pane; here it
/// costs nothing and buys a body a later version can add a field to without
/// breaking a decoder that has never heard of it.
///
/// The names mirror `ServerMessage::MediaOpen` one for one, so there is no
/// translation table for anyone to maintain or get wrong.
///
/// Key order is not part of the contract. The consumer parses this, so the
/// agreement is about fields; pinning bytes here would turn a serialiser's
/// ordering choice into a failure that means nothing.
pub fn open_payload(
    pane_id: &str,
    codec: &str,
    sample_rate_hz: u32,
    channels: u8,
    target_latency_us: u32,
) -> Vec<u8> {
    #[derive(serde::Serialize)]
    struct Open<'a> {
        pane_id: &'a str,
        codec: &'a str,
        sample_rate_hz: u32,
        channels: u8,
        target_latency_us: u32,
    }
    // An object, not an array or a bare value: a decoder that ignores unknown
    // keys needs keys to ignore, and that is what makes a later field additive.
    serde_json::to_vec(&Open {
        pane_id,
        codec,
        sample_rate_hz,
        channels,
        target_latency_us,
    })
    .unwrap_or_else(|_| b"{}".to_vec())
}

/// The body of an `Open` frame for a video stream.
///
/// The names mirror `MediaParams::Video` one for one, as `open_payload`'s
/// mirror the audio ones, and for the same reason: a player configures its
/// decoder from these, and a translation table is one more thing to get
/// wrong. `fps` is nominal, exactly as the wire says it is; timing rides
/// each chunk's `pts_us`.
pub fn video_open_payload(
    pane_id: &str,
    codec: &str,
    width: u32,
    height: u32,
    fps: u32,
    target_latency_us: u32,
) -> Vec<u8> {
    #[derive(serde::Serialize)]
    struct Open<'a> {
        pane_id: &'a str,
        codec: &'a str,
        width: u32,
        height: u32,
        fps: u32,
        target_latency_us: u32,
    }
    serde_json::to_vec(&Open {
        pane_id,
        codec,
        width,
        height,
        fps,
        target_latency_us,
    })
    .unwrap_or_else(|_| b"{}".to_vec())
}

/// The body of a `Close` frame.
///
/// `reason` is a closed set and `detail` is free text, which is the same split
/// `MediaCloseReason` makes on the wire and for the same reason: a receiver
/// that meets an unknown discriminant cannot read the message at all, so the
/// meaning lives in the closed set and the specifics live in the string.
///
/// The consumer reads `reason` to tell a stream that ended from one that
/// failed, and that decision is what its respawn policy hangs on.
pub fn close_payload(reason: WebCloseReason, detail: &str) -> Vec<u8> {
    #[derive(serde::Serialize)]
    struct Close<'a> {
        reason: &'a str,
        detail: &'a str,
    }
    serde_json::to_vec(&Close {
        reason: reason.as_str(),
        detail,
    })
    .unwrap_or_else(|_| b"{}".to_vec())
}

/// Why a stream ended, in the words the payload carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebCloseReason {
    /// The source finished normally.
    Ended,
    /// Something went wrong. `detail` says what.
    Failed,
}

impl WebCloseReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ended => "ended",
            Self::Failed => "failed",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// EG-18 · the video open body names what a player needs to configure a
    /// decoder, as an object, with the names `MediaParams::Video` uses.
    #[test]
    fn the_video_open_payload_carries_the_decoders_parameters() {
        let body: serde_json::Value =
            serde_json::from_slice(&video_open_payload("w1:p3", "h264", 480, 270, 30, 100_000))
                .expect("json");
        assert!(body.is_object());
        assert_eq!(body["pane_id"], "w1:p3");
        assert_eq!(body["codec"], "h264");
        assert_eq!(body["width"], 480);
        assert_eq!(body["height"], 270);
        assert_eq!(body["fps"], 30);
        assert_eq!(body["target_latency_us"], 100_000);
    }

    /// EG-18 · the key flag is bit zero of the flags byte.
    ///
    /// Pinned because the player switches on it to know where it may start
    /// decoding; a moved bit would make every picture look like a P picture
    /// and the player would wait for a key that never comes.
    #[test]
    fn the_key_flag_is_bit_zero() {
        assert_eq!(WEB_FRAME_FLAG_KEY, 0b0000_0001);
    }

    /// Reads one frame back the way `media-framing.ts` does.
    ///
    /// Written from the TypeScript decoder's offsets, not from the encoder
    /// above, so that a test passing here means the two agree about layout
    /// rather than that one function is self-consistent. It is deliberately
    /// dumb: no buffering, no error recovery. The real cross-language proof is
    /// EG-7, the golden fixture; this only catches Rust-side layout mistakes.
    struct DecodedFrame {
        magic: u8,
        version: u8,
        kind: u8,
        flags: u8,
        stream_id: u32,
        seq: u64,
        pts_us: u64,
        payload_len: u32,
        payload: Vec<u8>,
    }

    fn decode_like_the_web(bytes: &[u8]) -> DecodedFrame {
        assert!(
            bytes.len() >= WEB_FRAME_HEADER_BYTES,
            "a frame shorter than the header is not a frame: {} bytes",
            bytes.len()
        );
        let u32_at = |offset: usize| {
            u32::from_le_bytes(bytes[offset..offset + 4].try_into().expect("four bytes"))
        };
        let u64_at = |offset: usize| {
            u64::from_le_bytes(bytes[offset..offset + 8].try_into().expect("eight bytes"))
        };
        let payload_len = u32_at(24);
        DecodedFrame {
            magic: bytes[0],
            version: bytes[1],
            kind: bytes[2],
            flags: bytes[3],
            stream_id: u32_at(4),
            seq: u64_at(8),
            pts_us: u64_at(16),
            payload_len,
            payload: bytes[WEB_FRAME_HEADER_BYTES..].to_vec(),
        }
    }

    fn sample_chunk<'a>(payload: &'a [u8]) -> WebFrame<'a> {
        WebFrame {
            kind: WebFrameKind::Chunk,
            flags: 0,
            stream_id: 7,
            seq: 42,
            pts_us: 1_234_567,
            payload,
        }
    }

    /// EG-1 — the header is exactly what the web decoder pins.
    ///
    /// A mismatch here is not an error on the wire, it is silent garbage: the
    /// decoder reads whatever happens to sit at the offset it expects.
    #[test]
    fn the_header_is_twenty_eight_little_endian_bytes_starting_with_m() {
        let payload = [1_u8, 2, 3, 4];
        let mut out = Vec::new();
        sample_chunk(&payload)
            .encode_into(&mut out)
            .expect("a four-byte payload is not too large");

        assert_eq!(
            out.len(),
            WEB_FRAME_HEADER_BYTES + payload.len(),
            "a frame is its header plus its payload and nothing else"
        );
        let decoded = decode_like_the_web(&out);
        assert_eq!(decoded.magic, WEB_FRAME_MAGIC, "magic must be 'M'");
        assert_eq!(decoded.version, WEB_FRAME_VERSION, "version must be 1");
        assert_eq!(
            &out[..2],
            &[0x4D, 0x01],
            "the first two bytes are literal, not derived"
        );
    }

    /// EG-2 — every field survives the layout unchanged.
    ///
    /// `pts_us` is what the player schedules against; a truncation here drifts
    /// the clock forever, and does it silently.
    #[test]
    fn every_field_survives_the_layout_field_for_field() {
        let payload = b"opus-ish".to_vec();
        let frame = WebFrame {
            kind: WebFrameKind::Chunk,
            flags: 0xA5,
            stream_id: 0xDEAD_BEEF,
            seq: u64::MAX - 3,
            pts_us: 0x0102_0304_0506_0708,
            payload: &payload,
        };
        let mut out = Vec::new();
        frame.encode_into(&mut out).expect("small payload");

        let decoded = decode_like_the_web(&out);
        assert_eq!(decoded.flags, 0xA5, "flags byte");
        assert_eq!(decoded.stream_id, 0xDEAD_BEEF, "stream_id u32 LE");
        assert_eq!(decoded.seq, u64::MAX - 3, "seq u64 LE");
        assert_eq!(decoded.pts_us, 0x0102_0304_0506_0708, "pts_us u64 LE");
        assert_eq!(decoded.payload, payload, "payload bytes");
    }

    /// EG-3 — the kind byte means what the web side thinks it means.
    ///
    /// The web decoder switches on this byte. A wrong value plays an open as
    /// audio, which is a burst of noise rather than an error.
    #[test]
    fn the_kind_byte_matches_the_web_sides_numbering() {
        for (kind, expected) in [
            (WebFrameKind::Chunk, 0_u8),
            (WebFrameKind::Open, 1),
            (WebFrameKind::Close, 2),
            (WebFrameKind::TimeSync, 3),
        ] {
            let mut out = Vec::new();
            WebFrame {
                kind,
                flags: 0,
                stream_id: 1,
                seq: 0,
                pts_us: 0,
                payload: &[],
            }
            .encode_into(&mut out)
            .expect("empty payload");
            assert_eq!(
                decode_like_the_web(&out).kind,
                expected,
                "{kind:?} must encode as {expected}"
            );
        }
    }

    /// EG-4 — the payload is the encoded body, never padded to a frame size.
    ///
    /// The whole reason for the work: a chunk carries the Opus packet the
    /// server already produced. Padding it back out to a raw frame would cost
    /// 32x the bandwidth to arrive at the same sound.
    #[test]
    fn a_chunk_carries_exactly_the_bytes_it_was_given() {
        // What one 20 ms Opus packet at 64 kbps actually weighs, against the
        // 7680 bytes the same 20 ms costs as raw f32 stereo.
        let opus_sized = vec![0xAB_u8; 160];
        let mut out = Vec::new();
        sample_chunk(&opus_sized)
            .encode_into(&mut out)
            .expect("small payload");

        let decoded = decode_like_the_web(&out);
        assert_eq!(decoded.payload_len, 160, "declared length is the real one");
        assert_eq!(decoded.payload.len(), 160, "and no padding follows it");
        assert!(
            out.len() < 7680,
            "a whole frame must stay far under the raw cost of the same audio"
        );
    }

    /// EG-5 — an oversize payload is refused before anything is written.
    ///
    /// Both ends must agree on the ceiling or one of them allocates on a
    /// corrupt length. Refusing without touching the buffer also keeps a
    /// shared buffer usable after a refusal.
    #[test]
    fn an_oversize_payload_is_refused_and_the_buffer_is_untouched() {
        let too_big = vec![0_u8; WEB_FRAME_MAX_PAYLOAD + 1];
        let mut out = b"previous frame".to_vec();
        let before = out.clone();

        let err = sample_chunk(&too_big)
            .encode_into(&mut out)
            .expect_err("a payload over the ceiling must be refused");

        assert_eq!(err, WebFrameError::TooLarge { got: too_big.len() });
        assert_eq!(out, before, "a refused frame writes nothing at all");
    }

    /// EG-5b — the ceiling itself is allowed, not refused.
    ///
    /// An off-by-one here would refuse a frame the other side accepts, which
    /// looks like a network fault and is not one.
    #[test]
    fn a_payload_exactly_at_the_ceiling_is_accepted() {
        let at_limit = vec![0_u8; WEB_FRAME_MAX_PAYLOAD];
        let mut out = Vec::new();
        sample_chunk(&at_limit)
            .encode_into(&mut out)
            .expect("the ceiling is inclusive");
        assert_eq!(
            decode_like_the_web(&out).payload_len as usize,
            WEB_FRAME_MAX_PAYLOAD
        );
    }

    /// EG-6 — the declared length is the payload's length.
    ///
    /// A decoder that trusts the length reads into the next frame when it
    /// lies, and every frame after that is garbage.
    #[test]
    fn the_declared_length_always_equals_the_bytes_that_follow() {
        for size in [0_usize, 1, 27, 28, 29, 160, 4096] {
            let payload = vec![0x5A_u8; size];
            let mut out = Vec::new();
            sample_chunk(&payload)
                .encode_into(&mut out)
                .expect("small payload");
            let decoded = decode_like_the_web(&out);
            assert_eq!(
                decoded.payload_len as usize, size,
                "declared length for a {size}-byte payload"
            );
            assert_eq!(
                out.len() - WEB_FRAME_HEADER_BYTES,
                size,
                "bytes actually written after the header for a {size}-byte payload"
            );
        }
    }

    /// EG-13 — encoding into a non-empty buffer appends.
    ///
    /// The buffer is borrowed so a stream can keep one for its whole life. A
    /// `clear()` hidden in here would silently drop the frame written just
    /// before, and the loss would look like a network drop.
    #[test]
    fn encoding_appends_and_leaves_earlier_frames_intact() {
        let first = [0xAA_u8; 3];
        let second = [0xBB_u8; 5];
        let mut out = Vec::new();

        sample_chunk(&first).encode_into(&mut out).expect("small");
        let after_first = out.len();
        sample_chunk(&second).encode_into(&mut out).expect("small");

        assert_eq!(
            after_first,
            WEB_FRAME_HEADER_BYTES + first.len(),
            "first frame's own size"
        );
        assert_eq!(
            out.len(),
            (WEB_FRAME_HEADER_BYTES + first.len()) + (WEB_FRAME_HEADER_BYTES + second.len()),
            "two whole frames, back to back"
        );
        assert_eq!(
            decode_like_the_web(&out[..after_first]).payload,
            first,
            "the first frame is still readable after the second was appended"
        );
        assert_eq!(
            &out[after_first..after_first + 2],
            &[WEB_FRAME_MAGIC, WEB_FRAME_VERSION],
            "the second frame starts where the first ended"
        );
    }

    // ---- EG-7 · the golden fixture -------------------------------------
    //
    // The one test that fails when the two implementations drift apart.
    //
    // The bytes are written by `scripts/media_web_frame_fixture.py` from the
    // documented layout — by *neither* implementation — so a disagreement here
    // names which of the three is wrong instead of proving that two agree with
    // each other. `herdr-web` asserts the same bytes with its own decoder, and
    // neither side can move alone.

    /// Pinned in test code and deliberately **not** stored beside the data: a
    /// hash that travels with a hand-edited copy agrees with it. One constant
    /// per language, and they must be changed in the same breath.
    const FIXTURE_SHA256: &str = "7ffa28f50a06313ef1186541f86a13e6f416de5f0c7d355a9daf1e246683dfa7";
    const FIXTURE_BYTES: &[u8] = include_bytes!("../../tests/fixtures/media_web_frames.bin");
    const FIXTURE_MANIFEST: &str = include_str!("../../tests/fixtures/media_web_frames.json");

    fn kind_from_u8(value: u64) -> WebFrameKind {
        match value {
            0 => WebFrameKind::Chunk,
            1 => WebFrameKind::Open,
            2 => WebFrameKind::Close,
            3 => WebFrameKind::TimeSync,
            other => panic!("the manifest names kind {other}, which this side has no name for"),
        }
    }

    fn hex_to_bytes(hex: &str) -> Vec<u8> {
        assert!(hex.len().is_multiple_of(2), "hex must be whole bytes");
        (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).expect("hex digits"))
            .collect()
    }

    /// EG-7a — the fixture is the one both languages agreed on.
    #[test]
    fn the_golden_fixture_still_hashes_to_what_both_sides_pinned() {
        use sha2::{Digest, Sha256};
        let digest = crate::checksum::to_lower_hex(&Sha256::digest(FIXTURE_BYTES));
        assert_eq!(
            digest, FIXTURE_SHA256,
            "the fixture changed; herdr-web pins the same hash and has to change with it"
        );
    }

    /// EG-7b — the encoder reproduces every golden frame byte for byte.
    ///
    /// Fields and payloads come from the manifest, not from this side, so the
    /// assertion is "given these inputs, produce these bytes" — not "this
    /// encoder agrees with itself".
    #[test]
    fn the_encoder_reproduces_every_golden_frame_byte_for_byte() {
        let manifest: serde_json::Value =
            serde_json::from_str(FIXTURE_MANIFEST).expect("the manifest is json");
        let frames = manifest["frames"]
            .as_array()
            .expect("the manifest lists frames");
        assert_eq!(frames.len(), 4, "one open, two chunks, one close");

        let mut produced = Vec::new();
        for frame in frames {
            let name = frame["name"].as_str().expect("every frame is named");
            let payload = hex_to_bytes(frame["payload_hex"].as_str().expect("payload_hex"));
            let offset = frame["offset"].as_u64().expect("offset") as usize;

            assert_eq!(
                offset,
                produced.len(),
                "{name} does not start where the frames before it ended"
            );
            assert_eq!(
                frame["payload_len"].as_u64().expect("payload_len") as usize,
                payload.len(),
                "{name}: the manifest's declared length is not its payload's length"
            );

            WebFrame {
                kind: kind_from_u8(frame["kind"].as_u64().expect("kind")),
                flags: frame["flags"].as_u64().expect("flags") as u8,
                stream_id: frame["stream_id"].as_u64().expect("stream_id") as u32,
                seq: frame["seq"].as_u64().expect("seq"),
                pts_us: frame["pts_us"].as_u64().expect("pts_us"),
                payload: &payload,
            }
            .encode_into(&mut produced)
            .unwrap_or_else(|err| panic!("{name} was refused: {err}"));

            let end = produced.len();
            assert_eq!(
                &produced[offset..end],
                &FIXTURE_BYTES[offset..end],
                "{name} does not match the golden bytes"
            );
        }

        assert_eq!(
            produced, FIXTURE_BYTES,
            "the whole stream must be byte-identical, not only frame by frame"
        );
    }

    /// EG-17 — the open payload this side builds says what the fixture says.
    ///
    /// Compared **field by field, not byte for byte**: the consumer parses this
    /// body, so the agreement is about fields. A byte comparison would make a
    /// serialiser's key order a failure, which is a red that means nothing and
    /// trains people to ignore it.
    #[test]
    fn the_open_payload_carries_the_fields_the_fixture_agreed_on() {
        let manifest: serde_json::Value =
            serde_json::from_str(FIXTURE_MANIFEST).expect("the manifest is json");
        let golden_open = manifest["frames"]
            .as_array()
            .and_then(|frames| frames.iter().find(|f| f["kind"] == 1))
            .expect("the fixture has an open frame");
        let golden: serde_json::Value =
            serde_json::from_slice(&hex_to_bytes(golden_open["payload_hex"].as_str().unwrap()))
                .expect("the open payload is json");

        let built: serde_json::Value = serde_json::from_slice(&open_payload(
            golden["pane_id"].as_str().expect("pane_id"),
            golden["codec"].as_str().expect("codec"),
            golden["sample_rate_hz"].as_u64().expect("sample_rate_hz") as u32,
            golden["channels"].as_u64().expect("channels") as u8,
            golden["target_latency_us"]
                .as_u64()
                .expect("target_latency_us") as u32,
        ))
        .expect("what we built is json");

        assert_eq!(built, golden, "field for field, in any order");
    }

    /// EG-17b — and it is an object, which is what makes a later field additive.
    ///
    /// The forward-compatibility claim rests on the consumer being able to
    /// ignore keys it does not know. An array or a bare value has no keys to
    /// ignore, so the claim would be empty. `herdr-web` pins the other half:
    /// that its decoder actually ignores an unknown key.
    #[test]
    fn the_control_payloads_are_json_objects_so_a_later_field_is_additive() {
        for body in [
            open_payload("p", "opus", 48_000, 2, 100_000),
            close_payload(WebCloseReason::Ended, ""),
            close_payload(WebCloseReason::Failed, "server disconnected"),
        ] {
            let parsed: serde_json::Value =
                serde_json::from_slice(&body).expect("control payloads are json");
            assert!(parsed.is_object(), "not an object: {parsed}");
        }
    }

    /// EG-17c — the close reason is the closed set, spelled the agreed way.
    ///
    /// The consumer switches on this word to tell a stream that ended from one
    /// that failed, and its respawn policy hangs on the answer. A typo here is
    /// not a decode error on the other side — it is a stream that never
    /// restarts, or one that restarts forever.
    #[test]
    fn the_close_reason_is_the_word_the_consumer_switches_on() {
        let ended: serde_json::Value =
            serde_json::from_slice(&close_payload(WebCloseReason::Ended, "")).unwrap();
        assert_eq!(ended["reason"], "ended");
        assert_eq!(ended["detail"], "");

        let failed: serde_json::Value =
            serde_json::from_slice(&close_payload(WebCloseReason::Failed, "boom")).unwrap();
        assert_eq!(failed["reason"], "failed");
        assert_eq!(failed["detail"], "boom");
    }

    /// EG-7c — the manifest and the bytes describe the same thing.
    ///
    /// Guards the fixture itself: a hand-edited `.bin` with an unedited
    /// `.json` would otherwise pass every test above by supplying its own
    /// expectations. `scripts/media_web_frame_fixture.py --check` says the
    /// same thing outside the test binary, for CI.
    #[test]
    fn the_manifest_describes_the_bytes_that_are_actually_committed() {
        let manifest: serde_json::Value =
            serde_json::from_str(FIXTURE_MANIFEST).expect("the manifest is json");
        assert_eq!(
            manifest["total_bytes"].as_u64().expect("total_bytes") as usize,
            FIXTURE_BYTES.len(),
            "the manifest and the fixture disagree about how long the fixture is"
        );
        assert_eq!(
            manifest["header_bytes"].as_u64().expect("header_bytes") as usize,
            WEB_FRAME_HEADER_BYTES,
            "the manifest's header width is not this side's"
        );
        assert_eq!(
            manifest["magic"].as_u64().expect("magic") as u8,
            WEB_FRAME_MAGIC
        );
        assert_eq!(
            manifest["version"].as_u64().expect("version") as u8,
            WEB_FRAME_VERSION
        );
    }
}
