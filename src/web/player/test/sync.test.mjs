// Audio is the clock; the picture follows it. Pure logic, no AudioContext:
// the timeline is handed "now" in seconds and answers in seconds.
import test from "node:test";
import assert from "node:assert/strict";

import { AudioTimeline, VideoPacer, ArrivalClock } from "../sync.mjs";
import { MAX_PLAYOUT_DELAY_US } from "../playout.mjs";

const near = (got, want, eps = 1e-6) => assert.ok(Math.abs(got - want) <= eps, `${got} is not within ${eps} of ${want}`);

// TP-PLAYER-AV-01
test("the first audio chunk anchors the timeline one target delay ahead", () => {
  const timeline = new AudioTimeline({ targetDelayUs: 100_000, frameUs: 20_000 });
  near(timeline.schedule(1_000_000, 5.0).at, 5.1);
  // The second chunk is placed by its distance from the anchor, not by when it arrived.
  near(timeline.schedule(1_020_000, 5.013).at, 5.12);
  assert.equal(timeline.audioNowUs(5.1), 1_000_000);
  assert.equal(timeline.audioNowUs(5.15), 1_050_000);
  assert.equal(timeline.counters.scheduled, 2);
});

// TP-PLAYER-AV-01
test("a chunk whose moment has passed is dropped and counted", () => {
  const timeline = new AudioTimeline({ targetDelayUs: 100_000, frameUs: 20_000 });
  timeline.schedule(1_000_000, 5.0); // anchor: pts 1.000 s sounds at 5.1
  // pts 1.040 s should sound at 5.14; it is offered at 5.2, after it would have ended.
  assert.equal(timeline.schedule(1_040_000, 5.2), null);
  assert.equal(timeline.counters.late, 1);
  // pts 1.220 s sounds at 5.32: still ahead of 5.2, so it is scheduled.
  near(timeline.schedule(1_220_000, 5.2).at, 5.32);
  assert.equal(timeline.counters.scheduled, 2);
});

// TP-PLAYER-AV-01
test("a gap is left silent rather than closed", () => {
  const timeline = new AudioTimeline({ targetDelayUs: 100_000, frameUs: 20_000 });
  timeline.schedule(1_000_000, 5.0);
  // Five frames never arrive; the next one keeps its own moment.
  near(timeline.schedule(1_120_000, 5.02).at, 5.22);
  assert.equal(timeline.counters.gaps, 1);
  near(timeline.schedule(1_140_000, 5.03).at, 5.24);
  assert.equal(timeline.counters.gaps, 1);
});

// TP-PLAYER-AV-01
test("before the anchor there is no audio clock", () => {
  const timeline = new AudioTimeline({ targetDelayUs: 100_000, frameUs: 20_000 });
  assert.equal(timeline.audioNowUs(5.0), null);
  assert.equal(timeline.anchored(), false);
  timeline.schedule(1_000_000, 5.0);
  assert.equal(timeline.anchored(), true);
});

// TP-PLAYER-AV-02
test("an early picture waits an on time picture shows and a late one is dropped", () => {
  const pacer = new VideoPacer({ frameUs: 41_667 });
  assert.equal(pacer.decide(1_000_000, 900_000), "wait");
  assert.equal(pacer.decide(1_000_000, 1_000_000), "show");
  assert.equal(pacer.decide(1_000_000, 1_030_000), "show");
  assert.equal(pacer.counters.late, 1, "shown, but behind the clock");
  assert.equal(pacer.decide(1_000_000, 1_041_668), "drop");
  assert.deepEqual(pacer.counters, { shown: 2, late: 1, dropped: 1, skipped: 0 });
});

// TP-PLAYER-AV-02
test("when several pictures are due the newest shows and the rest are skipped", () => {
  // A skipped or dropped picture is a decoder buffer someone must close:
  // the pacer hands every one it discards back, in order.
  const discarded = [];
  const pacer = new VideoPacer({ frameUs: 41_667, discard: (p) => discarded.push(p.seq) });
  const picture = (seq, ptsUs) => ({ seq, ptsUs });
  pacer.offer(picture(0, 1_000_000));
  pacer.offer(picture(1, 1_040_000));
  pacer.offer(picture(2, 1_080_000));
  assert.equal(pacer.pending(), 3);
  // A stall ends: three pictures are due at once. Showing all three replays
  // the stall; showing the newest catches up.
  assert.deepEqual(pacer.next(1_085_000), picture(2, 1_080_000));
  assert.equal(pacer.pending(), 0);
  assert.equal(pacer.counters.skipped, 2);
  assert.equal(pacer.counters.shown, 1);
  assert.equal(pacer.next(1_086_000), null);
  pacer.offer(picture(3, 1_120_000));
  assert.equal(pacer.next(1_090_000), null, "early: it waits");
  assert.equal(pacer.pending(), 1);
  assert.deepEqual(pacer.next(1_120_000), picture(3, 1_120_000));
  // A picture that is already too old when it is offered is dropped at the door.
  pacer.offer(picture(4, 1_160_000));
  assert.equal(pacer.next(1_260_000), null);
  assert.equal(pacer.counters.dropped, 1);
  assert.equal(pacer.pending(), 0);
  assert.deepEqual(discarded, [0, 1, 4], "every picture not shown was handed back");
});

// TP-PLAYER-AV-03
test("without audio the picture clock is anchored on the first picture and runs at wall pace", () => {
  const clock = new ArrivalClock({ targetDelayUs: 100_000 });
  assert.equal(clock.nowUs(50_000_000), null);
  clock.anchor(1_000_000, 50_000_000);
  assert.equal(clock.nowUs(50_000_000), 900_000);
  assert.equal(clock.nowUs(50_100_000), 1_000_000);
  assert.equal(clock.nowUs(50_141_667), 1_041_667);
  // Anchoring again does not move a clock that already runs.
  clock.anchor(9_000_000, 50_200_000);
  assert.equal(clock.nowUs(50_200_000), 1_100_000);
});

import { avOffsetsUs, medianUs } from "../sync.mjs";

// TP-PLAYER-AV-04
test("each flash is paired with its nearest bip and the median offset is reported", () => {
  // Bips every second; flashes drawn 12 ms after, except one 30 ms before
  // and one flash with no bip near it (which pairs with nothing).
  const bips = [1_000_000, 2_000_000, 3_000_000, 4_000_000];
  const flashes = [1_012_000, 1_970_000, 3_012_000, 4_012_000, 9_000_000];
  assert.deepEqual(avOffsetsUs(flashes, bips, 500_000), [12_000, -30_000, 12_000, 12_000]);
  assert.equal(medianUs([12_000, -30_000, 12_000, 12_000]), 12_000);
  assert.equal(medianUs([]), null);
  // The median of the magnitudes is what the acceptance criterion reads.
  assert.equal(medianUs(avOffsetsUs(flashes, bips, 500_000).map(Math.abs)), 12_000);
});

import { avErrorsUs } from "../sync.mjs";

// TP-PLAYER-AV-04
test("the lip-sync error is the output spacing minus the source spacing", () => {
  // The producer's own skew is not the player's fault: what the player owes
  // is to reproduce the server's pts relationship at the output. A bip stamped
  // pts 1.000 s that sounds at 5.100 s and a flash stamped pts 1.010 s drawn
  // at 5.120 s are 10 ms late, not 20.
  const bips = [{ outUs: 5_100_000, ptsUs: 1_000_000 }, { outUs: 6_100_000, ptsUs: 2_000_000 }];
  const flashes = [{ outUs: 5_120_000, ptsUs: 1_010_000 }, { outUs: 6_095_000, ptsUs: 2_010_000 }, { outUs: 9_000_000, ptsUs: 8_000_000 }];
  assert.deepEqual(avErrorsUs(flashes, bips, 500_000), [10_000, -15_000]);
  assert.equal(medianUs(avErrorsUs(flashes, bips, 500_000).map(Math.abs)), 15_000);
});

import { HiddenGate } from "../sync.mjs";

// TP-PLAYER-IDLE-01
test("a hidden page drops video and counts it while audio keeps flowing", () => {
  const gate = new HiddenGate();
  assert.equal(gate.admit("video", true), "decode");
  assert.equal(gate.admit("video", false), "decode");
  gate.hidden(true);
  assert.equal(gate.admit("video", true), "drop");
  assert.equal(gate.admit("video", false), "drop");
  assert.equal(gate.admit("audio", false), "decode", "sound goes on in the background");
  assert.deepEqual(gate.counters, { hiddenDropped: 2, resumedSkipped: 0 });
});

// TP-PLAYER-IDLE-01
test("a page shown again resumes video at the next key", () => {
  const gate = new HiddenGate();
  gate.hidden(true);
  gate.admit("video", true);
  gate.hidden(false);
  assert.equal(gate.admit("video", false), "drop", "a delta after the gap references a picture the decoder never saw");
  assert.equal(gate.admit("video", false), "drop");
  assert.equal(gate.admit("video", true), "decode");
  assert.equal(gate.admit("video", false), "decode");
  assert.deepEqual(gate.counters, { hiddenDropped: 1, resumedSkipped: 2 });
  // Hidden and shown again without a key in between: still waiting for one.
  gate.hidden(true);
  gate.hidden(false);
  assert.equal(gate.admit("video", false), "drop");
});

// TP-PLAYER-CLOCK-01
test("a server clock that runs ahead is re-anchored once the lead passes 20 ms", () => {
  // Server pts advance 20 ms per chunk; the audio clock only 19.9 ms — the
  // server runs 0.5 % fast, so every chunk lands a little further ahead of
  // its slot and the buffer would grow forever.
  const timeline = new AudioTimeline({ targetDelayUs: 100_000, frameUs: 20_000 });
  let realignedAt = null;
  for (let i = 0; i < 2000; i += 1) {
    const ptsUs = 1_000_000 + i * 20_000;
    const ctxNow = 5.0 + i * 0.0199;
    const slot = timeline.schedule(ptsUs, ctxNow);
    assert.ok(slot, `chunk ${i} was dropped`);
    if (timeline.counters.realigned > 0 && realignedAt === null) realignedAt = i;
    // After a realignment the lead is back near the target.
    if (realignedAt !== null && i === realignedAt + 1) {
      const leadUs = (slot.at - ctxNow) * 1e6 - timeline.targetDelayUs();
      assert.ok(Math.abs(leadUs) < 20_000, `lead after realignment ${leadUs}`);
    }
  }
  assert.ok(timeline.counters.realigned >= 1, "never realigned");
  assert.ok(realignedAt > 50 && realignedAt < 600, `realigned at chunk ${realignedAt}`);
});

// TP-PLAYER-CLOCK-01
test("regular arrivals never re-anchor and one late chunk moves nothing", () => {
  const timeline = new AudioTimeline({ targetDelayUs: 100_000, frameUs: 20_000 });
  for (let i = 0; i < 500; i += 1) assert.ok(timeline.schedule(1_000_000 + i * 20_000, 5.0 + i * 0.02));
  assert.equal(timeline.counters.realigned, 0);
  // One chunk 300 ms late: dropped, and the anchor stays where it was.
  const before = timeline.ctxAt(11_000_000);
  assert.equal(timeline.schedule(11_000_000, 5.0 + 500 * 0.02 + 0.3), null);
  assert.equal(timeline.counters.late, 1);
  assert.equal(timeline.counters.realigned, 0);
  assert.equal(timeline.ctxAt(11_000_000), before);
});

// TP-PLAYER-CLOCK-02
test("a jittery link raises the target and the anchor follows it", () => {
  const timeline = new AudioTimeline({ targetDelayUs: 100_000, frameUs: 20_000 });
  const start = timeline.targetDelayUs();
  // Arrivals wobble ±60 ms around their cadence.
  let lastSlot = null;
  let ctxNow = 5.0;
  for (let i = 0; i < 1500; i += 1) {
    const ptsUs = 1_000_000 + i * 20_000;
    ctxNow = 5.0 + i * 0.02 + (i % 2 === 0 ? 0.06 : -0.06) * (i > 0 ? 1 : 0);
    lastSlot = timeline.schedule(ptsUs, ctxNow) ?? lastSlot;
  }
  const target = timeline.targetDelayUs();
  assert.ok(target > start, `target did not grow: ${target}`);
  assert.ok(target <= MAX_PLAYOUT_DELAY_US);
  assert.ok(timeline.counters.realigned >= 1, "the grown target was never applied");
});
