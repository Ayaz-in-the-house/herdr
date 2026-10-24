// One connection, every pane: the bridge's record stream taken apart.
//
// The bridge writes `HV` | u8 tag length | listener tag | u32 BE length |
// bytes, where the tag names the listener (`w1:p4#a` for the pane's
// `audio-listen`, `w1:p4#v` for its `video-listen --framed`) and the bytes
// are that listener's web frames; an empty record (no tag, no bytes) goes
// out once a second so a closed connection is noticed. A record boundary is
// not a frame boundary and a read boundary is neither, so there are two
// buffers here — one for records, one per *listener* for frames — and the
// events are the only thing a caller sees (TP-PLAYER-DEMUX-01..05).
//
// Per listener, never per pane: each listener is its own herdr client, so
// it numbers its streams from one (a pane's audio and video listeners both
// say "stream 1") and its bytes are its own byte stream (a picture split
// over two of its reads may have the other listener's record between).

import { FrameDecoder, Kind, FLAG_KEY } from "./framing.mjs";

const RECORD_MAGIC_H = 0x48;
const RECORD_MAGIC_V = 0x56;
const RECORD_FIXED = 3 + 4; // magic, id length, u32 length (the id sits between)

/** What the bridge writes when it has nothing to say: an empty id, zero bytes. */
export const HEARTBEAT = new Uint8Array([RECORD_MAGIC_H, RECORD_MAGIC_V, 0, 0, 0, 0, 0]);

const utf8 = new TextEncoder();
const text = new TextDecoder();

/** The pane a listener tag belongs to: `w1:p4#v` → `w1:p4`. */
export function paneOf(tag) {
  const hash = tag.indexOf("#");
  return hash < 0 ? tag : tag.slice(0, hash);
}

/** One record, as the bridge would write it. */
export function encodeRecord(tag, bytes) {
  const id = utf8.encode(tag);
  if (id.byteLength > 255) throw new Error(`listener tag ${tag} is longer than a record can name`);
  const out = new Uint8Array(RECORD_FIXED + id.byteLength + bytes.byteLength);
  out[0] = RECORD_MAGIC_H;
  out[1] = RECORD_MAGIC_V;
  out[2] = id.byteLength;
  out.set(id, 3);
  new DataView(out.buffer).setUint32(3 + id.byteLength, bytes.byteLength, false);
  out.set(bytes, RECORD_FIXED + id.byteLength);
  return out;
}

/** Incremental record reader: `push(bytes)` → `[{ tag, bytes }]`. */
export class RecordDecoder {
  #buffer = new Uint8Array(0);

  push(bytes) {
    const joined = new Uint8Array(this.#buffer.byteLength + bytes.byteLength);
    joined.set(this.#buffer, 0);
    joined.set(bytes, this.#buffer.byteLength);
    const records = [];
    let offset = 0;
    while (joined.byteLength - offset >= 3) {
      if (joined[offset] !== RECORD_MAGIC_H || joined[offset + 1] !== RECORD_MAGIC_V) {
        // Resynchronising by scanning for the next 'HV' would turn a payload
        // that happens to contain those bytes into a frame: stop instead.
        throw new Error(`bridge record is not aligned at offset ${offset}: expected 'HV'`);
      }
      const idLength = joined[offset + 2];
      const lengthAt = offset + 3 + idLength;
      if (joined.byteLength < lengthAt + 4) break;
      const length = new DataView(joined.buffer, joined.byteOffset + lengthAt, 4).getUint32(0, false);
      const start = lengthAt + 4;
      if (joined.byteLength < start + length) break;
      records.push({
        tag: text.decode(joined.subarray(offset + 3, offset + 3 + idLength)),
        bytes: joined.slice(start, start + length),
      });
      offset = start + length;
    }
    this.#buffer = offset === 0 ? joined : joined.slice(offset);
    return records;
  }
}

/** Which lane an open frame's parameters describe. */
function laneOf(params) {
  if (params.codec === "opus" || "sample_rate_hz" in params) return "audio";
  if ("width" in params || params.codec === "h264") return "video";
  throw new Error(`open payload names no lane: ${JSON.stringify(params)}`);
}

function json(payload) {
  return JSON.parse(text.decode(payload));
}

/**
 * Routes the bridge stream into per-pane, per-lane events:
 *
 *   { type: "open",     pane, source, lane, streamId, ptsUs, params }
 *   { type: "chunk",    pane, source, lane, streamId, seq, ptsUs, key, data }
 *   { type: "loss",     pane, source, lane, streamId, missing }
 *   { type: "close",    pane, source, lane, streamId, reason, detail }
 *   { type: "timesync", pane, source, streamId, seq, ptsUs, payload }
 *
 * `source` is the listener tag the bytes came from; `pane` is its pane.
 */
export class PaneDemux {
  #records = new RecordDecoder();
  #decoders = new Map(); // listener tag → FrameDecoder
  /** `${tag}\u0000${streamId}` → { pane, source, lane, params, nextSeq, keySeen } */
  streams = new Map();
  counters = { heartbeats: 0, unknownStream: 0, skippedUntilKey: 0, lost: 0 };

  /** The route a listener's stream id names, if it is open. */
  route(tag, streamId) {
    return this.streams.get(`${tag}\u0000${streamId}`);
  }

  push(bytes) {
    const events = [];
    for (const record of this.#records.push(bytes)) {
      if (record.tag === "" && record.bytes.byteLength === 0) {
        this.counters.heartbeats += 1;
        continue;
      }
      if (record.bytes.byteLength === 0) continue;
      let decoder = this.#decoders.get(record.tag);
      if (!decoder) {
        decoder = new FrameDecoder();
        this.#decoders.set(record.tag, decoder);
      }
      for (const frame of decoder.push(record.bytes)) this.#route(record.tag, frame, events);
    }
    return events;
  }

  #route(source, frame, events) {
    const { streamId } = frame;
    const pane = paneOf(source);
    const key = `${source}\u0000${streamId}`;
    switch (frame.kind) {
      case Kind.Open: {
        const params = json(frame.payload);
        const lane = laneOf(params);
        // Audio needs no key; the gate is a video fact, so it is born open
        // for every other lane.
        this.streams.set(key, { pane, source, lane, params, nextSeq: null, keySeen: lane !== "video" });
        events.push({ type: "open", pane, source, lane, streamId, ptsUs: frame.ptsUs, params });
        return;
      }
      case Kind.Chunk: {
        const stream = this.streams.get(key);
        if (!stream) {
          // A chunk with no open is a chunk with no decoder configured for
          // it; routing it would only move the failure into the decoder.
          this.counters.unknownStream += 1;
          return;
        }
        const isKey = (frame.flags & FLAG_KEY) === FLAG_KEY;
        if (!stream.keySeen) {
          if (!isKey) {
            // Skipped pictures are not losses: the network delivered them,
            // the player chose not to decode them.
            this.counters.skippedUntilKey += 1;
            return;
          }
          stream.keySeen = true;
        }
        if (stream.nextSeq !== null && frame.seq > stream.nextSeq) {
          const missing = frame.seq - stream.nextSeq;
          this.counters.lost += missing;
          // Reported before the chunk after the gap, as its own event, so the
          // consumer conceals first and plays second.
          events.push({ type: "loss", pane, source, lane: stream.lane, streamId, missing });
        }
        if (stream.nextSeq === null || frame.seq >= stream.nextSeq) {
          stream.nextSeq = frame.seq + 1;
        }
        events.push({
          type: "chunk",
          pane,
          source,
          lane: stream.lane,
          streamId,
          seq: frame.seq,
          ptsUs: frame.ptsUs,
          key: isKey,
          data: frame.payload,
        });
        return;
      }
      case Kind.Close: {
        const stream = this.streams.get(key);
        const { reason = "failed", detail = "" } = frame.payload.byteLength ? json(frame.payload) : {};
        if (!stream) {
          this.counters.unknownStream += 1;
          return;
        }
        this.streams.delete(key);
        events.push({ type: "close", pane, source, lane: stream.lane, streamId, reason, detail });
        return;
      }
      case Kind.TimeSync: {
        events.push({ type: "timesync", pane, source, streamId, seq: frame.seq, ptsUs: frame.ptsUs, payload: frame.payload });
        return;
      }
      default:
        throw new Error(`web frame kind ${frame.kind} has no name on this side`);
    }
  }
}
