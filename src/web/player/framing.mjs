// The web frame, as the player reads and writes it.
//
// Not ours to design: `src/media/web_frame.rs` produces it, herdr-web's
// `media-framing.ts` consumes it, and `scripts/media_web_frame_fixture.py`
// referees between them from the documented table. This module is a third
// implementation of the same 28 bytes, and the fixture — not this file — is
// what it is held to (TP-PLAYER-FRAMING-01).
//
//   offset  size  field
//        0     1  magic   'M' (0x4D)
//        1     1  version 1
//        2     1  kind    0=chunk 1=open 2=close 3=timesync
//        3     1  flags
//        4     4  stream_id   u32 LE
//        8     8  seq         u64 LE
//       16     8  pts_us      u64 LE
//       24     4  payload_len u32 LE
//       28     n  payload
//
// Pure on purpose: no socket, no clock, no DOM. Everything that can be wrong
// about the bytes is decidable without any of them.

export const MAGIC = 0x4d;
export const VERSION = 1;
export const HEADER_BYTES = 28;

/** A ceiling that is a refusal, not an allocation. Mirrors the Rust and TS caps. */
export const MAX_PAYLOAD = 8 * 1024 * 1024;

/** Bit zero of `flags` on a video chunk: the picture holds an IDR slice. */
export const FLAG_KEY = 0b0000_0001;

export const Kind = Object.freeze({ Chunk: 0, Open: 1, Close: 2, TimeSync: 3 });

// `seq` and `pts_us` are u64 on the wire and plain numbers here: a microsecond
// clock takes 285 years to leave the safe-integer range, and WebCodecs
// timestamps are numbers too. A value past that range is refused, not
// rounded — rounding a presentation time is how two pictures share a moment.
function u64(view, offset) {
  const value = view.getBigUint64(offset, true);
  if (value > BigInt(Number.MAX_SAFE_INTEGER)) {
    throw new Error(`web frame u64 ${value} exceeds the safe integer range`);
  }
  return Number(value);
}

/** One frame, header and payload in a single allocation. */
export function encodeFrame({ kind, flags = 0, streamId, seq, ptsUs, payload }) {
  if (payload.byteLength > MAX_PAYLOAD) {
    throw new Error(`web frame payload ${payload.byteLength} is too large (cap ${MAX_PAYLOAD})`);
  }
  const out = new Uint8Array(HEADER_BYTES + payload.byteLength);
  const view = new DataView(out.buffer);
  view.setUint8(0, MAGIC);
  view.setUint8(1, VERSION);
  view.setUint8(2, kind);
  view.setUint8(3, flags & 0xff);
  view.setUint32(4, streamId >>> 0, true);
  view.setBigUint64(8, BigInt(seq), true);
  view.setBigUint64(16, BigInt(ptsUs), true);
  view.setUint32(24, payload.byteLength, true);
  out.set(payload, HEADER_BYTES);
  return out;
}

/**
 * Incremental decoder.
 *
 * A read boundary is not a frame boundary: a header can land in two reads and
 * two frames in one. So this buffers, and a decoder that assumed whole
 * messages would decode a truncated header as a valid frame full of zeros.
 */
export class FrameDecoder {
  #buffer = new Uint8Array(0);
  #maxPayload;

  constructor({ maxPayload = MAX_PAYLOAD } = {}) {
    this.#maxPayload = maxPayload;
  }

  /** Feed bytes; returns every complete frame they finished, in order. */
  push(bytes) {
    if (this.#buffer.byteLength === 0) {
      this.#buffer = bytes.slice();
    } else {
      const joined = new Uint8Array(this.#buffer.byteLength + bytes.byteLength);
      joined.set(this.#buffer, 0);
      joined.set(bytes, this.#buffer.byteLength);
      this.#buffer = joined;
    }
    const frames = [];
    let offset = 0;
    const buffer = this.#buffer;
    while (buffer.byteLength - offset >= HEADER_BYTES) {
      const view = new DataView(buffer.buffer, buffer.byteOffset + offset, HEADER_BYTES);
      const magic = view.getUint8(0);
      if (magic !== MAGIC) {
        throw new Error(`web frame magic ${magic} is not 'M' (${MAGIC})`);
      }
      const version = view.getUint8(1);
      if (version !== VERSION) {
        throw new Error(`web frame version ${version} is not ${VERSION}`);
      }
      const length = view.getUint32(24, true);
      if (length > this.#maxPayload) {
        // Refused on the header alone: the bytes it announces never have to
        // arrive for the answer to be no.
        throw new Error(`web frame payload ${length} is too large (cap ${this.#maxPayload})`);
      }
      if (buffer.byteLength - offset - HEADER_BYTES < length) break;
      const start = offset + HEADER_BYTES;
      frames.push({
        kind: view.getUint8(2),
        flags: view.getUint8(3),
        streamId: view.getUint32(4, true),
        seq: u64(view, 8),
        ptsUs: u64(view, 16),
        // Copied, not a view: the caller keeps this after the buffer moves on.
        payload: buffer.slice(start, start + length),
      });
      offset = start + length;
    }
    this.#buffer = offset === 0 ? buffer : buffer.slice(offset);
    return frames;
  }
}
