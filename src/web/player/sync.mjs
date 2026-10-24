// Audio is the clock; the picture follows it.
//
// The standard shape (ffplay, mpv, Chromium): the output device consumes PCM
// at a fixed rate, so the moment a sample sounds is the one clock nobody can
// argue with. Server `pts_us` is anchored to that clock once, by the first
// scheduled chunk, one target delay ahead of now; every later chunk keeps its
// distance from the anchor. Video asks the timeline which server moment is
// sounding and shows the picture for it (TP-PLAYER-AV-01..03).
//
// Pure: "now" is an argument, never read from a clock, so every rule here is
// decidable in a test with no AudioContext in sight.

import { PlayoutDelay, JitterEstimator, MAX_PLAYOUT_DELAY_US } from "./playout.mjs";

/** Smoothed lead past which the anchor is moved (PRD CLOCK-01: 20 ms). */
const REALIGN_US = 20_000;
/** How much of the old lead survives each chunk — one late chunk moves nothing. */
const LEAD_SMOOTHING = 0.9;
/** Late chunks in a row before the timeline admits the anchor itself is late. */
const LATE_RUN_REALIGN = 3;

/**
 * Server `pts_us` ↔ `AudioContext` seconds, anchored by the first chunk.
 *
 * The keeper (TP-PLAYER-CLOCK-01/02): every chunk measures its lead — how far
 * its moment sits ahead of now, against the target — and the smoothed lead is
 * what moves the anchor, never one chunk. The target itself adapts to jitter
 * with the TUI client's rule (`PlayoutDelay`), floored at the server's
 * suggestion so a calm link keeps the buffer it was given rather than
 * trading it for glitches.
 */
export class AudioTimeline {
  #suggestedUs;
  #delay;
  #jitter = new JitterEstimator();
  #frameUs;
  #anchorPtsUs = null;
  #anchorCtxSec = null;
  #lastPtsUs = null;
  #leadUs = 0;
  #lateRun = 0;
  counters = { scheduled: 0, late: 0, gaps: 0, realigned: 0 };

  constructor({ targetDelayUs, frameUs }) {
    this.#suggestedUs = Math.min(targetDelayUs, MAX_PLAYOUT_DELAY_US);
    this.#delay = new PlayoutDelay(targetDelayUs);
    this.#frameUs = frameUs;
  }

  anchored() {
    return this.#anchorPtsUs !== null;
  }

  /** The delay a chunk waits: adaptive, never below what the server suggested. */
  targetDelayUs() {
    return Math.max(this.#delay.targetUs(), this.#suggestedUs);
  }

  /** The context second at which `ptsUs` sounds, once anchored. */
  ctxAt(ptsUs) {
    return this.#anchorCtxSec + (ptsUs - this.#anchorPtsUs) / 1e6;
  }

  /**
   * Where to start the chunk for `ptsUs`, given the context's current time,
   * or `null` when its moment has already passed. The first chunk anchors.
   */
  schedule(ptsUs, ctxNowSec) {
    const nowUs = Math.round(ctxNowSec * 1e6);
    if (this.#anchorPtsUs === null) {
      this.#anchorPtsUs = ptsUs;
      this.#anchorCtxSec = ctxNowSec + this.targetDelayUs() / 1e6;
    }
    // The target follows the link: jitter measured on the audio clock, the
    // TUI client's multiplier and smoothing (TP-PLAYER-CLOCK-02).
    this.#jitter.observe(ptsUs, nowUs);
    this.#delay.update(this.#jitter.jitterUs());
    let at = this.ctxAt(ptsUs);
    if (at + this.#frameUs / 1e6 <= ctxNowSec) {
      // Past its moment: dropped, and the gap it leaves is silence. A run of
      // them is the anchor itself being late — then the anchor moves.
      this.counters.late += 1;
      this.#lateRun += 1;
      if (this.#lateRun >= LATE_RUN_REALIGN) {
        this.#anchorCtxSec += ctxNowSec - at + this.targetDelayUs() / 1e6;
        this.#leadUs = 0;
        this.#lateRun = 0;
        this.counters.realigned += 1;
        at = this.ctxAt(ptsUs);
      } else {
        return null;
      }
    }
    this.#lateRun = 0;
    const leadUs = (at - ctxNowSec) * 1e6 - this.targetDelayUs();
    this.#leadUs = this.#leadUs * LEAD_SMOOTHING + leadUs * (1 - LEAD_SMOOTHING);
    if (Math.abs(this.#leadUs) > REALIGN_US) {
      // The two clocks disagree (or the target grew): move the anchor once,
      // by the measured lead, and count it (TP-PLAYER-CLOCK-01).
      this.#anchorCtxSec -= this.#leadUs / 1e6;
      this.#leadUs = 0;
      this.counters.realigned += 1;
      at = this.ctxAt(ptsUs);
    }
    if (this.#lastPtsUs !== null && ptsUs - this.#lastPtsUs > this.#frameUs * 1.5) {
      // Not closed: the chunk keeps its own moment and the silence stays
      // where the loss was, which is what the listener's ear expects.
      this.counters.gaps += 1;
    }
    this.#lastPtsUs = Math.max(this.#lastPtsUs ?? ptsUs, ptsUs);
    this.counters.scheduled += 1;
    return { at };
  }

  /** The server moment sounding at `ctxNowSec`, or `null` before the anchor. */
  audioNowUs(ctxNowSec) {
    if (this.#anchorPtsUs === null) return null;
    return Math.round(this.#anchorPtsUs + (ctxNowSec - this.#anchorCtxSec) * 1e6);
  }
}

/** Judges pictures against whatever clock it is handed. */
export class VideoPacer {
  #frameUs;
  #discard;
  #queue = [];
  counters = { shown: 0, late: 0, dropped: 0, skipped: 0 };

  /** `discard` receives every picture not shown — a decoder frame is a buffer someone must close. */
  constructor({ frameUs, discard = () => {} }) {
    this.#frameUs = frameUs;
    this.#discard = discard;
  }

  /** `wait` (early), `show` (within a frame; `late` counted if behind), `drop` (further behind). */
  decide(ptsUs, clockNowUs) {
    if (ptsUs > clockNowUs) return "wait";
    const behind = clockNowUs - ptsUs;
    if (behind > this.#frameUs) {
      this.counters.dropped += 1;
      return "drop";
    }
    if (behind > 0) this.counters.late += 1;
    this.counters.shown += 1;
    return "show";
  }

  offer(picture) {
    this.#queue.push(picture);
  }

  pending() {
    return this.#queue.length;
  }

  /**
   * The picture to show at `clockNowUs`, or `null`. When several are due the
   * newest is shown and the older ones skipped: the picture catches up
   * instead of replaying the stall.
   */
  next(clockNowUs) {
    let chosen = null;
    while (this.#queue.length > 0 && this.#queue[0].ptsUs <= clockNowUs) {
      const picture = this.#queue.shift();
      if (clockNowUs - picture.ptsUs > this.#frameUs && this.#queue.length === 0 && chosen === null) {
        // Alone and already too old: nothing newer to catch up to.
        this.counters.dropped += 1;
        this.#discard(picture);
        return null;
      }
      if (chosen !== null) {
        this.counters.skipped += 1;
        this.#discard(chosen);
      }
      chosen = picture;
    }
    if (chosen === null) return null;
    if (clockNowUs - chosen.ptsUs > 0) this.counters.late += 1;
    this.counters.shown += 1;
    return chosen;
  }
}

/** A clock for a pane with pictures and no sound: the first picture anchors it. */
export class ArrivalClock {
  #targetDelayUs;
  #anchorPtsUs = null;
  #anchorWallUs = null;

  constructor({ targetDelayUs }) {
    this.#targetDelayUs = targetDelayUs;
  }

  /** Anchors once; a clock that already runs is not moved. */
  anchor(ptsUs, wallNowUs) {
    if (this.#anchorPtsUs !== null) return;
    this.#anchorPtsUs = ptsUs;
    this.#anchorWallUs = wallNowUs;
  }

  /** The server moment to present at `wallNowUs`, or `null` before the anchor. */
  nowUs(wallNowUs) {
    if (this.#anchorPtsUs === null) return null;
    return this.#anchorPtsUs - this.#targetDelayUs + (wallNowUs - this.#anchorWallUs);
  }
}

/**
 * Lip-sync instrument (PRD SYNC-02): each flash, in server or context
 * microseconds, is paired with the nearest bip within `windowUs`; the
 * offsets are flash minus bip, positive when the picture came later than
 * the sound. Flashes with no bip near them pair with nothing.
 */
export function avOffsetsUs(flashesUs, bipsUs, windowUs) {
  const offsets = [];
  for (const flash of flashesUs) {
    let best = null;
    for (const bip of bipsUs) {
      const delta = flash - bip;
      if (Math.abs(delta) <= windowUs && (best === null || Math.abs(delta) < Math.abs(best))) best = delta;
    }
    if (best !== null) offsets.push(best);
  }
  return offsets;
}

/** The middle value, or `null` for nothing. */
export function medianUs(values) {
  if (values.length === 0) return null;
  const sorted = [...values].sort((a, b) => a - b);
  return sorted[sorted.length >> 1];
}

/**
 * Lip-sync error per flash: how far the output spacing (flash drawn minus bip
 * sounded) departs from the source spacing (flash pts minus bip pts). The
 * producer's own skew cancels out; what remains is the player's. Each flash
 * pairs with the bip nearest to it in pts within `windowUs`; unpaired flashes
 * are left out. Positive means the picture came later than it should have.
 */
export function avErrorsUs(flashes, bips, windowUs) {
  const errors = [];
  for (const flash of flashes) {
    let best = null;
    for (const bip of bips) {
      const delta = flash.ptsUs - bip.ptsUs;
      if (Math.abs(delta) <= windowUs && (best === null || Math.abs(delta) < Math.abs(flash.ptsUs - best.ptsUs))) best = bip;
    }
    if (best !== null) errors.push((flash.outUs - best.outUs) - (flash.ptsUs - best.ptsUs));
  }
  return errors;
}

/**
 * Video while nobody looks (PRD IDLE-01, TP-PLAYER-IDLE-01): a hidden page
 * decodes no pictures — dropped and counted — while audio keeps flowing; when
 * the page is visible again video resumes at the next key-flagged picture,
 * because a delta after the gap references pictures the decoder never saw.
 */
export class HiddenGate {
  #hidden = false;
  #needKey = false;
  counters = { hiddenDropped: 0, resumedSkipped: 0 };

  hidden(isHidden) {
    if (isHidden && !this.#hidden) this.#needKey = true;
    this.#hidden = isHidden;
  }

  /** `decode` or `drop` for a chunk of `lane` whose key flag is `isKey`. */
  admit(lane, isKey) {
    if (lane !== "video") return "decode";
    if (this.#hidden) {
      this.counters.hiddenDropped += 1;
      return "drop";
    }
    if (this.#needKey) {
      if (!isKey) {
        this.counters.resumedSkipped += 1;
        return "drop";
      }
      this.#needKey = false;
    }
    return "decode";
  }
}
