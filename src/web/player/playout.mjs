// The client side of the media clock: hold chunks until their moment, then
// hand them over in order.
//
// A port of `src/media/playout.rs` and `src/media/clock.rs`, constant for
// constant, so the browser player and the TUI client answer "is this chunk
// due" the same way (TP-PLAYER-PLAYOUT-01/02). The rule is one sentence: a
// chunk is played at `pts_us + target`, never on arrival. Everything else
// here exists to make that rule survive what a network does — chunks out of
// order, chunks that never arrive, chunks that arrive after their moment.

export const FRAME_MS = 20;
export const FRAME_US = FRAME_MS * 1000;

/** Smallest playout delay the client will hold. */
export const MIN_PLAYOUT_DELAY_US = 10_000;
/** Largest: the phase's 800 ms budget minus what Opus and the network need. */
export const MAX_PLAYOUT_DELAY_US = 500_000;

const JITTER_MULTIPLIER = 3.0;
// The target has to move slowly: audio played at a shifting delay is audio
// played at a shifting *rate*, and a rate change is heard where a delay is not.
const DELAY_SMOOTHING = 0.99;
const OFFSET_SAMPLES = 9;
const STALE_AFTER_US = 60_000_000;
// A ceiling on memory rather than a policy: four seconds at 20 ms a frame.
const MAX_HELD = 200;

/** RFC 3550 interarrival jitter, one sixteenth per step. */
export class JitterEstimator {
  #jitterUs = 0;
  #last = null;

  /** Feeds one chunk's presentation time and its arrival, both on the server's clock. */
  observe(ptsUs, arrivalUs) {
    if (this.#last === null) {
      this.#last = [ptsUs, Math.max(arrivalUs, 0)];
      return;
    }
    const [lastPts, lastArrival] = this.#last;
    // A reordered chunk's transit difference describes the reordering, not
    // the jitter — RFC 3550 makes the same restriction.
    if (ptsUs <= lastPts) return;
    const sent = ptsUs - lastPts;
    const received = arrivalUs - lastArrival;
    const transit = Math.abs(received - sent);
    this.#jitterUs += (transit - this.#jitterUs) / 16;
    this.#last = [ptsUs, Math.max(arrivalUs, 0)];
  }

  jitterUs() {
    return this.#jitterUs;
  }
}

/** Turns measured jitter into the delay a chunk waits before playing. */
export class PlayoutDelay {
  #targetUs;

  constructor(suggestedUs) {
    this.#targetUs = Math.min(Math.max(suggestedUs, MIN_PLAYOUT_DELAY_US), MAX_PLAYOUT_DELAY_US);
  }

  update(jitterUs) {
    const raw = jitterUs * JITTER_MULTIPLIER;
    const clamped = Math.min(Math.max(raw, MIN_PLAYOUT_DELAY_US), MAX_PLAYOUT_DELAY_US);
    this.#targetUs = this.#targetUs * DELAY_SMOOTHING + clamped * (1.0 - DELAY_SMOOTHING);
  }

  targetUs() {
    return Math.round(this.#targetUs);
  }

  /** Early chunks wait. Whether a late one is *too* late is a different question. */
  isDue(ptsUs, nowServerUs) {
    return nowServerUs >= ptsUs + Math.trunc(this.#targetUs);
  }

  /** Whether playing this chunk would move the stream backwards. */
  isExpired(ptsUs, nowServerUs, frameUs) {
    return nowServerUs > ptsUs + Math.trunc(this.#targetUs) + frameUs;
  }
}

/** Holds chunks until their presentation time. */
export class PlayoutBuffer {
  #delay;
  #jitter = new JitterEstimator();
  /** Sorted by presentation time, which is also the order they play in. */
  #held = [];
  #nextSeq = null;
  #droppedLate = 0;
  #droppedFull = 0;
  #lost = 0;

  constructor(targetLatencyUs) {
    this.#delay = new PlayoutDelay(targetLatencyUs);
  }

  /** Offers a chunk that just arrived; `false` means it was refused. */
  push(chunk, nowServerUs) {
    this.#jitter.observe(chunk.ptsUs, nowServerUs);
    this.#delay.update(this.#jitter.jitterUs());
    if (this.#delay.isExpired(chunk.ptsUs, nowServerUs, FRAME_US)) {
      // Refused at the door rather than filtered at playout: keeping it would
      // let it into the memory bound for something that can never be played.
      this.#droppedLate += 1;
      return false;
    }
    if (this.#held.length >= MAX_HELD) {
      this.#droppedFull += 1;
      return false;
    }
    this.#insert(chunk);
    return true;
  }

  /** Whatever is due at `nowServerUs`: `play`, `waiting` or `lost`. */
  take(nowServerUs) {
    // Anything whose moment passed while it sat here is discarded first.
    // Playing it would move the stream backwards; holding it would block
    // everything behind it.
    for (;;) {
      if (this.#held.length === 0) return { type: "waiting" };
      const first = this.#held[0];
      if (this.#delay.isExpired(first.ptsUs, nowServerUs, FRAME_US)) {
        this.#held.shift();
        this.#droppedLate += 1;
        continue;
      }
      if (!this.#delay.isDue(first.ptsUs, nowServerUs)) return { type: "waiting" };
      break;
    }
    const chunk = this.#held.shift();
    if (this.#nextSeq !== null) {
      if (chunk.seq > this.#nextSeq) {
        const missing = chunk.seq - this.#nextSeq;
        this.#lost += missing;
        this.#nextSeq = chunk.seq;
        // The chunk stays held so the caller gets it on the next call, after
        // it has concealed the gap.
        this.#insert(chunk);
        return { type: "lost", missing };
      }
      if (chunk.seq < this.#nextSeq) {
        // A duplicate or a straggler from before a gap was reported.
        return { type: "waiting" };
      }
    }
    this.#nextSeq = chunk.seq + 1;
    return { type: "play", chunk };
  }

  #insert(chunk) {
    let low = 0;
    let high = this.#held.length;
    while (low < high) {
      const mid = (low + high) >> 1;
      if (this.#held[mid].ptsUs < chunk.ptsUs) low = mid + 1;
      else high = mid;
    }
    if (low < this.#held.length && this.#held[low].ptsUs === chunk.ptsUs) {
      // Same moment: the later arrival replaces, as a map keyed by pts would.
      this.#held[low] = chunk;
    } else {
      this.#held.splice(low, 0, chunk);
    }
  }

  targetDelayUs() {
    return this.#delay.targetUs();
  }

  jitterUs() {
    return this.#jitter.jitterUs();
  }

  held() {
    return this.#held.length;
  }

  droppedLate() {
    return this.#droppedLate;
  }

  droppedFull() {
    return this.#droppedFull;
  }

  lost() {
    return this.#lost;
  }
}

/** NTP four-stamp offset between this clock and the server's. */
export class ClockSync {
  #samples = [];
  #lastProbeUs = null;
  #offsetUs = 0;

  observe(t1ClientSend, t2ServerRecv, t3ServerSend, t4ClientRecv) {
    if (this.#lastProbeUs !== null && t4ClientRecv - this.#lastProbeUs > STALE_AFTER_US) {
      // A minute without a probe is a clock that may have moved: the old
      // samples would outvote the first honest one.
      this.#samples = [];
    }
    this.#lastProbeUs = t4ClientRecv;
    const outbound = t2ServerRecv - t1ClientSend;
    const inbound = t4ClientRecv - t3ServerSend;
    const offset = Math.trunc((outbound - inbound) / 2);
    if (this.#samples.length === OFFSET_SAMPLES) this.#samples.shift();
    this.#samples.push(offset);
    const sorted = [...this.#samples].sort((a, b) => a - b);
    this.#offsetUs = sorted[sorted.length >> 1];
  }

  offsetUs() {
    return this.#offsetUs;
  }

  isSynced() {
    return this.#samples.length > 0;
  }

  /** A client instant on the server's clock. */
  toServerUs(clientUs) {
    return clientUs + this.#offsetUs;
  }
}
