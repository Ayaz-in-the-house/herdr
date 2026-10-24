// The player's frame codec, measured against the referee fixture.
//
// Three implementations of one 28-byte layout (Python referee, Rust producer,
// this decoder). None of them may move alone: the fixture bytes are the
// contract, and every assertion here is against those bytes, never against
// what this module itself produced.
import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";

import {
  FrameDecoder,
  encodeFrame,
  HEADER_BYTES,
  MAGIC,
  VERSION,
  MAX_PAYLOAD,
  FLAG_KEY,
  Kind,
} from "../framing.mjs";

const fixture = new Uint8Array(
  readFileSync(new URL("../../../../tests/fixtures/media_web_frames.bin", import.meta.url)),
);
const manifest = JSON.parse(
  readFileSync(new URL("../../../../tests/fixtures/media_web_frames.json", import.meta.url), "utf8"),
);

const hex = (bytes) => Buffer.from(bytes).toString("hex");

// TP-PLAYER-FRAMING-01
test("the golden fixture decodes to the referee manifest and re-encodes byte for byte", () => {
  assert.equal(fixture.length, manifest.total_bytes);
  assert.equal(HEADER_BYTES, manifest.header_bytes);
  assert.equal(MAGIC, manifest.magic);
  assert.equal(VERSION, manifest.version);

  const frames = new FrameDecoder().push(fixture);
  assert.equal(frames.length, manifest.frames.length);
  manifest.frames.forEach((want, index) => {
    const got = frames[index];
    assert.equal(got.kind, want.kind, `${want.name} kind`);
    assert.equal(got.flags, want.flags, `${want.name} flags`);
    assert.equal(got.streamId, want.stream_id, `${want.name} stream_id`);
    assert.equal(got.seq, want.seq, `${want.name} seq`);
    assert.equal(got.ptsUs, want.pts_us, `${want.name} pts_us`);
    assert.equal(got.payload.length, want.payload_len, `${want.name} payload_len`);
    assert.equal(hex(got.payload), want.payload_hex, `${want.name} payload`);

    // The encoder is held to the same bytes, not to its own decoder.
    const encoded = encodeFrame(got);
    const slice = fixture.subarray(want.offset, want.offset + HEADER_BYTES + want.payload_len);
    assert.equal(hex(encoded), hex(slice), `${want.name} re-encoded`);
  });
  // The kinds the manifest names are the kinds this side names.
  assert.equal(Kind.Chunk, 0);
  assert.equal(Kind.Open, 1);
  assert.equal(Kind.Close, 2);
  assert.equal(Kind.TimeSync, 3);
});

// TP-PLAYER-FRAMING-01
test("a frame split at any byte waits for the rest and decodes the same", () => {
  const whole = new FrameDecoder().push(fixture).map((f) => [f.kind, f.seq, hex(f.payload)]);
  for (let cut = 1; cut < fixture.length; cut += 1) {
    const decoder = new FrameDecoder();
    const first = decoder.push(fixture.subarray(0, cut));
    const second = decoder.push(fixture.subarray(cut));
    const got = [...first, ...second].map((f) => [f.kind, f.seq, hex(f.payload)]);
    assert.deepEqual(got, whole, `cut at ${cut}`);
  }
  // Fed one byte at a time — the cruellest read pattern — nothing is lost.
  const decoder = new FrameDecoder();
  const frames = [];
  for (const byte of fixture) frames.push(...decoder.push(new Uint8Array([byte])));
  assert.deepEqual(
    frames.map((f) => [f.kind, f.seq, hex(f.payload)]),
    whole,
  );
});

// TP-PLAYER-FRAMING-01
test("a foreign magic a future version and an over-cap payload are refused", () => {
  const frame = { kind: Kind.Chunk, flags: 0, streamId: 1, seq: 0, ptsUs: 0, payload: new Uint8Array([1, 2, 3]) };
  const foreign = encodeFrame(frame);
  foreign[0] = 0x4e;
  assert.throws(() => new FrameDecoder().push(foreign), /magic/);

  const future = encodeFrame(frame);
  future[1] = VERSION + 1;
  assert.throws(() => new FrameDecoder().push(future), /version/);

  const header = encodeFrame({ ...frame, payload: new Uint8Array(0) });
  new DataView(header.buffer, header.byteOffset).setUint32(24, MAX_PAYLOAD + 1, true);
  // Refused on the header alone: the bytes it announces never have to arrive.
  assert.throws(() => new FrameDecoder().push(header), /too large|cap/);
});

// TP-PLAYER-FRAMING-01
test("the key flag is bit zero of flags", () => {
  assert.equal(FLAG_KEY, 1);
  const key = encodeFrame({ kind: Kind.Chunk, flags: FLAG_KEY, streamId: 9, seq: 4, ptsUs: 1_000_000, payload: new Uint8Array([0]) });
  const plain = encodeFrame({ kind: Kind.Chunk, flags: 0, streamId: 9, seq: 5, ptsUs: 1_040_000, payload: new Uint8Array([0]) });
  const [k, p] = new FrameDecoder().push(new Uint8Array([...key, ...plain]));
  assert.equal(k.flags & FLAG_KEY, FLAG_KEY);
  assert.equal(p.flags & FLAG_KEY, 0);
  assert.equal(key[3], 0b0000_0001);
});
