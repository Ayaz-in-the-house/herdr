// The TUI client's playout rule, in JavaScript, held to the same scenarios
// as `src/media/playout.rs` and `src/media/clock.rs`.
import test from "node:test";
import assert from "node:assert/strict";

import {
  PlayoutBuffer,
  JitterEstimator,
  PlayoutDelay,
  ClockSync,
  FRAME_US,
  MIN_PLAYOUT_DELAY_US,
  MAX_PLAYOUT_DELAY_US,
} from "../playout.mjs";

const chunk = (seq, ptsUs) => ({ seq, ptsUs, data: new Uint8Array([seq & 0xff]) });
const buffer = () => new PlayoutBuffer(100_000);
const play = (seq, ptsUs) => ({ type: "play", chunk: chunk(seq, ptsUs) });
const WAITING = { type: "waiting" };

// TP-PLAYER-PLAYOUT-01
test("an early chunk is held until its moment", () => {
  const buf = buffer();
  assert.equal(buf.push(chunk(0, 1_000_000), 1_000_000), true);
  const dueAt = 1_000_000 + buf.targetDelayUs();
  assert.deepEqual(buf.take(dueAt - 50_000), WAITING);
  assert.deepEqual(buf.take(dueAt - 1), WAITING);
  assert.deepEqual(buf.take(dueAt), play(0, 1_000_000));
});

// TP-PLAYER-PLAYOUT-01
test("a chunk that arrives after its moment is refused at the door", () => {
  const buf = buffer();
  assert.equal(buf.push(chunk(0, 1_000_000), 1_500_000), false);
  assert.equal(buf.held(), 0);
  assert.equal(buf.droppedLate(), 1);
  assert.deepEqual(buf.take(1_500_000), WAITING);
});

// TP-PLAYER-PLAYOUT-01
test("chunks that arrive out of order play in order", () => {
  const buf = buffer();
  assert.equal(buf.push(chunk(1, 1_020_000), 1_020_000), true);
  assert.equal(buf.push(chunk(0, 1_000_000), 1_021_000), true);
  assert.deepEqual(buf.take(1_100_000), play(0, 1_000_000));
  assert.deepEqual(buf.take(1_120_000), play(1, 1_020_000));
});

// TP-PLAYER-PLAYOUT-01
test("a missing chunk is reported before the one after it plays", () => {
  const buf = buffer();
  assert.equal(buf.push(chunk(0, 1_000_000), 1_000_000), true);
  assert.deepEqual(buf.take(1_100_000), play(0, 1_000_000));
  assert.equal(buf.push(chunk(2, 1_040_000), 1_040_000), true);
  assert.deepEqual(buf.take(1_140_000), { type: "lost", missing: 1 });
  assert.equal(buf.lost(), 1);
  assert.deepEqual(buf.take(1_140_000), play(2, 1_040_000));
});

// TP-PLAYER-PLAYOUT-01
test("a duplicate chunk is not played twice", () => {
  const buf = buffer();
  assert.equal(buf.push(chunk(0, 1_000_000), 1_000_000), true);
  assert.deepEqual(buf.take(1_100_000), play(0, 1_000_000));
  assert.equal(buf.push(chunk(0, 1_000_000), 1_001_000), true);
  assert.deepEqual(buf.take(1_100_000), WAITING);
});

// TP-PLAYER-PLAYOUT-01
test("a chunk that expires while waiting is discarded rather than blocking", () => {
  const buf = buffer();
  assert.equal(buf.push(chunk(0, 1_000_000), 1_000_000), true);
  assert.equal(buf.push(chunk(1, 1_020_000), 1_000_500), true);
  assert.deepEqual(buf.take(1_200_000), WAITING);
  assert.equal(buf.held(), 0);
  assert.equal(buf.droppedLate(), 2);
});

// TP-PLAYER-PLAYOUT-01
test("a steady stream keeps the target delay from growing", () => {
  const buf = buffer();
  const start = buf.targetDelayUs();
  for (let seq = 0; seq < 500; seq += 1) {
    const pts = 1_000_000 + seq * 20_000;
    buf.push(chunk(seq, pts), pts + 30_000);
    buf.take(pts + 200_000);
  }
  assert.ok(buf.targetDelayUs() <= start, `grew from ${start} to ${buf.targetDelayUs()}`);
});

// TP-PLAYER-PLAYOUT-02
test("perfectly regular arrivals produce no jitter", () => {
  const jitter = new JitterEstimator();
  for (let i = 0; i < 100; i += 1) jitter.observe(1_000_000 + i * 20_000, 1_050_000 + i * 20_000);
  assert.equal(jitter.jitterUs(), 0);
});

// TP-PLAYER-PLAYOUT-02
test("a late arrival raises the estimate and a calm link lowers it again", () => {
  const jitter = new JitterEstimator();
  jitter.observe(1_000_000, 1_050_000);
  jitter.observe(1_020_000, 1_070_000);
  assert.equal(jitter.jitterUs(), 0);
  jitter.observe(1_040_000, 1_130_000); // 40 ms late
  const raised = jitter.jitterUs();
  assert.ok(raised > 0 && raised <= 40_000 / 16 + 1, `one sixteenth of the transit difference: ${raised}`);
  for (let i = 3; i < 200; i += 1) jitter.observe(1_000_000 + i * 20_000, 1_090_000 + i * 20_000);
  assert.ok(jitter.jitterUs() < raised / 100, `calm link left jitter at ${jitter.jitterUs()}`);
});

// TP-PLAYER-PLAYOUT-02
test("an out of order chunk is not treated as jitter", () => {
  const jitter = new JitterEstimator();
  jitter.observe(1_000_000, 1_050_000);
  jitter.observe(1_040_000, 1_090_000);
  jitter.observe(1_020_000, 1_091_000); // older pts arriving late: reordering, not jitter
  assert.equal(jitter.jitterUs(), 0);
});

// TP-PLAYER-PLAYOUT-02
test("the playout target stays inside its bounds and moves slowly", () => {
  assert.equal(new PlayoutDelay(1_000).targetUs(), MIN_PLAYOUT_DELAY_US);
  assert.equal(new PlayoutDelay(5_000_000).targetUs(), MAX_PLAYOUT_DELAY_US);
  const delay = new PlayoutDelay(100_000);
  delay.update(1_000_000); // absurd jitter
  assert.ok(delay.targetUs() <= MAX_PLAYOUT_DELAY_US);
  // One update moves the target one percent of the way, not all of it.
  const fresh = new PlayoutDelay(100_000);
  fresh.update(100_000); // asks for 300 ms
  assert.equal(fresh.targetUs(), 102_000);
  assert.equal(FRAME_US, 20_000);
});

// TP-PLAYER-PLAYOUT-02
test("due and expired are different questions", () => {
  const delay = new PlayoutDelay(100_000);
  assert.equal(delay.isDue(1_000_000, 1_099_999), false);
  assert.equal(delay.isDue(1_000_000, 1_100_000), true);
  assert.equal(delay.isExpired(1_000_000, 1_100_000, FRAME_US), false);
  assert.equal(delay.isExpired(1_000_000, 1_120_000, FRAME_US), false);
  assert.equal(delay.isExpired(1_000_000, 1_120_001, FRAME_US), true);
});

/** One probe with a symmetric path: server clock = client clock + offset. */
function probe(sync, clientSend, offset, oneWay) {
  const serverRecv = clientSend + oneWay + offset;
  const serverSend = serverRecv + 100;
  const clientRecv = serverSend - offset + oneWay;
  sync.observe(clientSend, serverRecv, serverSend, clientRecv);
}

// TP-PLAYER-PLAYOUT-02
test("a symmetric probe recovers the offset exactly", () => {
  const sync = new ClockSync();
  assert.equal(sync.isSynced(), false);
  probe(sync, 1_000_000, 5_000_000, 3_000);
  assert.equal(sync.isSynced(), true);
  assert.equal(sync.offsetUs(), 5_000_000);
  assert.equal(sync.toServerUs(2_000_000), 7_000_000);
});

// TP-PLAYER-PLAYOUT-02
test("one delayed probe does not move the offset", () => {
  const sync = new ClockSync();
  for (let i = 0; i < 8; i += 1) probe(sync, 1_000_000 + i * 1_000_000, 5_000_000, 3_000);
  // A probe whose reply took 200 ms longer than its request skews this sample
  // by 100 ms; the median of nine ignores one outlier.
  sync.observe(9_000_000, 9_003_000 + 5_000_000, 9_003_100 + 5_000_000, 9_003_100 + 3_000 + 200_000);
  assert.equal(sync.offsetUs(), 5_000_000);
});

// TP-PLAYER-PLAYOUT-02
test("a gap longer than a minute throws the old samples away", () => {
  const sync = new ClockSync();
  for (let i = 0; i < 9; i += 1) probe(sync, 1_000_000 + i * 1_000_000, 5_000_000, 3_000);
  assert.equal(sync.offsetUs(), 5_000_000);
  // The server rebooted; its clock now runs 7 s ahead. The first probe after
  // a minute of silence must not be outvoted by nine stale samples.
  probe(sync, 9_000_000 + 61_000_000, 7_000_000, 3_000);
  assert.equal(sync.offsetUs(), 7_000_000);
});
