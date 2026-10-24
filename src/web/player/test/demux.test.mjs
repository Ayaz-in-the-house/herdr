// The bridge's single stream, taken apart into panes, lanes and chunks.
//
// Every scenario is built from the same encoders the page will use, cut at
// every byte, and judged by the events it produces — never by the bytes it
// keeps, which is how a demux hides a lost frame.
import test from "node:test";
import assert from "node:assert/strict";

import { encodeFrame, Kind, FLAG_KEY } from "../framing.mjs";
import { PaneDemux, encodeRecord, HEARTBEAT } from "../demux.mjs";

const text = (obj) => new TextEncoder().encode(JSON.stringify(obj));
const audioOpen = (pane) => text({ pane_id: pane, codec: "opus", sample_rate_hz: 48000, channels: 2, target_latency_us: 100000 });
const videoOpen = (pane) => text({ pane_id: pane, codec: "h264", width: 480, height: 270, fps: 30, target_latency_us: 100000 });
const frame = (kind, streamId, seq, ptsUs, payload, flags = 0) =>
  encodeFrame({ kind, flags, streamId, seq, ptsUs, payload });
const chunk = (streamId, seq, key = false) =>
  frame(Kind.Chunk, streamId, seq, 1_000_000 + seq * 20_000, new Uint8Array([seq]), key ? FLAG_KEY : 0);
const concat = (...parts) => {
  const out = new Uint8Array(parts.reduce((n, p) => n + p.length, 0));
  let at = 0;
  for (const p of parts) { out.set(p, at); at += p.length; }
  return out;
};
const summary = (events) => events.map((e) => [e.type, e.pane, e.lane, e.seq ?? e.missing ?? e.reason ?? null]);

/** Two panes on one connection, heartbeats between, exactly as the bridge writes. */
function twoPanes() {
  return concat(
    encodeRecord("w1:p1#a", concat(frame(Kind.Open, 7, 0, 1_000_000, audioOpen("w1:p1")), chunk(7, 1), chunk(7, 2))),
    HEARTBEAT,
    encodeRecord("w1:p2#v", concat(frame(Kind.Open, 3, 0, 1_000_000, videoOpen("w1:p2")), chunk(3, 0, true))),
    HEARTBEAT,
    encodeRecord("w1:p1#a", chunk(7, 3)),
  );
}

// TP-PLAYER-DEMUX-01 · TP-PLAYER-BRIDGE-01 · TP-PLAYER-BRIDGE-02
// The bridge ends every listener it spawned on a signal, so none outlives it as an orphan
// (TP-PLAYER-BRIDGE-03).
test("bridge records split at any byte yield the same frames per pane", () => {
  const bytes = twoPanes();
  const whole = summary(new PaneDemux().push(bytes));
  assert.deepEqual(whole, [
    ["open", "w1:p1", "audio", null],
    ["chunk", "w1:p1", "audio", 1],
    ["chunk", "w1:p1", "audio", 2],
    ["open", "w1:p2", "video", null],
    ["chunk", "w1:p2", "video", 0],
    ["chunk", "w1:p1", "audio", 3],
  ]);
  for (let cut = 1; cut < bytes.length; cut += 1) {
    const demux = new PaneDemux();
    const got = summary([...demux.push(bytes.subarray(0, cut)), ...demux.push(bytes.subarray(cut))]);
    assert.deepEqual(got, whole, `cut at ${cut}`);
  }
});

// TP-PLAYER-DEMUX-01
test("a heartbeat record is not a frame and a misaligned record is an error", () => {
  const demux = new PaneDemux();
  assert.deepEqual(demux.push(concat(HEARTBEAT, HEARTBEAT)), []);
  assert.equal(demux.counters.heartbeats, 2);
  assert.throws(() => new PaneDemux().push(new Uint8Array([0x58, 0x58, 0x00, 0, 0, 0, 0])), /align|record/);
});

// TP-PLAYER-DEMUX-02
test("an open routes a stream to its pane and lane and a close retires it", () => {
  const demux = new PaneDemux();
  const opened = demux.push(concat(
    encodeRecord("w1:p1#a", frame(Kind.Open, 7, 0, 1_000_000, audioOpen("w1:p1"))),
    encodeRecord("w1:p1#v", frame(Kind.Open, 3, 0, 1_000_000, videoOpen("w1:p1"))),
  ));
  assert.deepEqual(opened.map((e) => [e.type, e.streamId, e.lane, e.params.codec]), [
    ["open", 7, "audio", "opus"],
    ["open", 3, "video", "h264"],
  ]);
  assert.equal(opened[1].params.width, 480);
  assert.equal(demux.route("w1:p1#a", 7).pane, "w1:p1");
  assert.equal(demux.route("w1:p1#v", 3).lane, "video");

  const closed = demux.push(encodeRecord("w1:p1#a", frame(Kind.Close, 7, 4, 1_080_000, text({ reason: "ended", detail: "" }))));
  assert.deepEqual(closed.map((e) => [e.type, e.streamId, e.lane, e.reason]), [["close", 7, "audio", "ended"]]);
  assert.equal(demux.route("w1:p1#a", 7), undefined, "a closed stream keeps no route");
  // Video is untouched by audio's close.
  assert.equal(demux.route("w1:p1#v", 3).lane, "video");
});

// TP-PLAYER-DEMUX-02
test("a chunk for a stream that never opened is counted and dropped", () => {
  const demux = new PaneDemux();
  const events = demux.push(encodeRecord("w1:p1#v", concat(chunk(9, 0, true), chunk(9, 1))));
  assert.deepEqual(events, []);
  assert.equal(demux.counters.unknownStream, 2);
  // Opening afterwards routes what follows, but never what came before.
  const later = demux.push(encodeRecord("w1:p1#v", concat(frame(Kind.Open, 9, 0, 1_000_000, videoOpen("w1:p1")), chunk(9, 2, true))));
  assert.deepEqual(summary(later), [["open", "w1:p1", "video", null], ["chunk", "w1:p1", "video", 2]]);
});

// TP-PLAYER-DEMUX-03
test("video before the first key is skipped and counted and audio is never gated", () => {
  const demux = new PaneDemux();
  const events = demux.push(concat(
    encodeRecord("w1:p2#v", concat(
      frame(Kind.Open, 3, 0, 1_000_000, videoOpen("w1:p2")),
      chunk(3, 0), chunk(3, 1), chunk(3, 2, true), chunk(3, 3),
    )),
    encodeRecord("w1:p2#a", concat(frame(Kind.Open, 7, 0, 1_000_000, audioOpen("w1:p2")), chunk(7, 0), chunk(7, 1))),
  ));
  const chunks = events.filter((e) => e.type === "chunk").map((e) => [e.lane, e.seq, e.key]);
  assert.deepEqual(chunks, [["video", 2, true], ["video", 3, false], ["audio", 0, false], ["audio", 1, false]]);
  assert.equal(demux.counters.skippedUntilKey, 2);
  // The gate opens once; a later non-key picture is not skipped again.
  const more = demux.push(encodeRecord("w1:p2#v", chunk(3, 4)));
  assert.deepEqual(summary(more), [["chunk", "w1:p2", "video", 4]]);
  assert.equal(demux.counters.skippedUntilKey, 2);
});

// TP-PLAYER-DEMUX-04
test("a seq gap is reported once per stream with the count missed", () => {
  const demux = new PaneDemux();
  const v = (...parts) => encodeRecord("w1:p2#v", concat(...parts));
  const a = (...parts) => encodeRecord("w1:p2#a", concat(...parts));
  const events = demux.push(concat(
    v(frame(Kind.Open, 3, 0, 1_000_000, videoOpen("w1:p2"))),
    a(frame(Kind.Open, 7, 0, 1_000_000, audioOpen("w1:p2"))),
    v(chunk(3, 0, true), chunk(3, 1)), a(chunk(7, 0), chunk(7, 1)),
    v(chunk(3, 4)), // 2 and 3 never arrive
    a(chunk(7, 2)), // audio is intact
    v(chunk(3, 5)),
  ));
  assert.deepEqual(summary(events).filter((e) => e[0] !== "open"), [
    ["chunk", "w1:p2", "video", 0],
    ["chunk", "w1:p2", "video", 1],
    ["chunk", "w1:p2", "audio", 0],
    ["chunk", "w1:p2", "audio", 1],
    ["loss", "w1:p2", "video", 2],
    ["chunk", "w1:p2", "video", 4],
    ["chunk", "w1:p2", "audio", 2],
    ["chunk", "w1:p2", "video", 5],
  ]);
  assert.equal(demux.counters.lost, 2);
  // Skipped-until-key pictures are not losses: the counter names what the
  // network lost, not what the player chose not to decode.
  const late = new PaneDemux();
  late.push(encodeRecord("w1:p3#v", concat(frame(Kind.Open, 5, 0, 1_000_000, videoOpen("w1:p3")), chunk(5, 0), chunk(5, 1), chunk(5, 2, true))));
  assert.equal(late.counters.lost, 0);
  assert.equal(late.counters.skippedUntilKey, 2);
});

// TP-PLAYER-DEMUX-05
test("two listeners of one pane may reuse a stream id and neither shadows the other", () => {
  // Measured 2026-09-03 (E-155): each listener is its own client and numbers
  // its streams from one, so a pane's audio and video listeners both said
  // "stream 1" — and the second open shadowed the first, whose 1257 chunks
  // were then dropped as unknown.
  const demux = new PaneDemux();
  const events = demux.push(concat(
    encodeRecord("w1:p4#a", concat(frame(Kind.Open, 1, 0, 1_000_000, audioOpen("w1:p4")), chunk(1, 0), chunk(1, 1))),
    encodeRecord("w1:p4#v", concat(frame(Kind.Open, 1, 0, 1_000_000, videoOpen("w1:p4")), chunk(1, 0, true))),
    encodeRecord("w1:p4#a", chunk(1, 2)),
    encodeRecord("w1:p4#v", chunk(1, 1)),
  ));
  assert.deepEqual(summary(events), [
    ["open", "w1:p4", "audio", null], ["chunk", "w1:p4", "audio", 0], ["chunk", "w1:p4", "audio", 1],
    ["open", "w1:p4", "video", null], ["chunk", "w1:p4", "video", 0],
    ["chunk", "w1:p4", "audio", 2],
    ["chunk", "w1:p4", "video", 1],
  ]);
  assert.equal(demux.counters.unknownStream, 0);
  assert.equal(demux.counters.lost, 0);
});

// TP-PLAYER-DEMUX-05
test("a picture split across records with another listener's record between stays whole", () => {
  // A listener's read boundary is not a frame boundary: a 100 KB picture
  // leaves the pipe in two reads, and the audio listener's record can land
  // between them. One frame decoder per listener, never per pane.
  const demux = new PaneDemux();
  const big = frame(Kind.Chunk, 1, 1, 1_040_000, new Uint8Array(100_000).fill(7), FLAG_KEY);
  const cut = 65_536;
  const events = demux.push(concat(
    encodeRecord("w1:p4#v", frame(Kind.Open, 1, 0, 1_000_000, videoOpen("w1:p4"))),
    encodeRecord("w1:p4#a", frame(Kind.Open, 1, 0, 1_000_000, audioOpen("w1:p4"))),
    encodeRecord("w1:p4#v", big.subarray(0, cut)),
    encodeRecord("w1:p4#a", chunk(1, 0)),
    encodeRecord("w1:p4#v", big.subarray(cut)),
  ));
  const chunks = events.filter((e) => e.type === "chunk").map((e) => [e.lane, e.seq, e.data.length]);
  assert.deepEqual(chunks, [["audio", 0, 1], ["video", 1, 100_000]]);
  assert.equal(events.find((e) => e.lane === "video" && e.type === "chunk").data[99_999], 7);
});
