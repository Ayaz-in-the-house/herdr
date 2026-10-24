// The page that plays panes: geometry from the server, one stream for every
// pane, audio as the clock, pictures following it.
//
// Everything decidable without a browser lives next door (framing, demux,
// playout, sync) and is unit-tested; this file is the glue that hands those
// modules the browser's clocks and decoders, and is proven on the product
// (PRD test point SYNC-02: bip + flash, |A−V| < 40 ms) rather than in node.

import { PaneDemux } from "./demux.mjs";
import { AudioTimeline, VideoPacer, ArrivalClock, HiddenGate, avOffsetsUs, avErrorsUs, medianUs } from "./sync.mjs";
import { FRAME_US } from "./playout.mjs";
import { picturePoint, pointerBody, wheelAction } from "./pointer.mjs";

const stage = document.getElementById("stage");
const hud = document.getElementById("hud");
const startButton = document.getElementById("start");

const panes = new Map(); // pane id → { el, canvas, ctx2d, video, audio, lastDriftUs }
const demux = new PaneDemux();
// Video while nobody looks: hidden → no picture decoded, audio goes on;
// shown again → resume at the next key (TP-PLAYER-IDLE-01).
const gate = new HiddenGate();
document.addEventListener("visibilitychange", () => gate.hidden(document.hidden));
let audioCtx = null;
let generation = -1;
let cell = null;
let scale = 1;
const dropped = { audioBeforeStart: 0 };
// Pointers the page sent back through the bridge (TP-PLAYER-INPUT-01).
const sent = { pointers: 0, refused: 0 };
let streamState = "bağlanıyor";

// ── Measure mode (?measure): the lip-sync instrument of PRD SYNC-02 ────────
// A bip is the first loud sample of a scheduled buffer after silence; a
// flash is a drawn picture whose centre is white after a dark one. Each is
// stamped twice: when it reaches the output (audio-clock µs) and when the
// server recorded it (its `pts_us`). The number that judges the player is
// the output spacing minus the source spacing — the producer's own skew is
// not the player's to answer for. Off by default: reading pixels back costs
// a copy per picture.
const measure = new URLSearchParams(location.search).has("measure");
const probe = { bips: [], flashes: [], quietAudio: true, darkPicture: true };
const BIP_LEVEL = 0.2;
const FLASH_LEVEL = 200;
const PAIR_WINDOW_US = 500_000;

// ── Geometry: the server says where every pane is, in this client's pixels ──
function fit(width, height) {
  // One scale for the whole stage: what must survive is the layout, not the
  // absolute size.
  const margin = 28;
  scale = Math.min((innerWidth - margin) / width, (innerHeight - margin) / height, 1);
  stage.style.width = `${Math.round(width * scale)}px`;
  stage.style.height = `${Math.round(height * scale)}px`;
}

function entry(paneId) {
  let e = panes.get(paneId);
  if (!e) {
    const el = document.createElement("div");
    el.className = "pane";
    const tag = document.createElement("span");
    tag.className = "tag";
    tag.textContent = paneId;
    const canvas = document.createElement("canvas");
    // The picture is not watch-only: a press on it goes back to the pane
    // through the bridge, in the picture's own pixels. TP-PLAYER-INPUT-01
    canvas.addEventListener("pointerdown", (ev) => sendPointer(paneId, "down", ev));
    canvas.addEventListener("pointerup", (ev) => sendPointer(paneId, "up", ev));
    canvas.addEventListener("contextmenu", (ev) => ev.preventDefault());
    canvas.addEventListener("wheel", (ev) => {
      const kind = wheelAction(ev.deltaY);
      if (!kind) return;
      ev.preventDefault();
      sendPointer(paneId, kind, ev);
    }, { passive: false });
    el.append(tag, canvas);
    stage.append(el);
    // Measure mode reads pixels back after every picture; a CPU-backed canvas makes that a copy, not a GPU stall.
    e = { el, canvas, ctx2d: canvas.getContext("2d", { willReadFrequently: measure }), video: null, audio: null, lastDriftUs: null };
    panes.set(paneId, e);
  }
  return e;
}

function sendPointer(paneId, kind, ev) {
  const e = panes.get(paneId);
  if (!e) return;
  const picture = e.video?.params?.width
    ? { width: e.video.params.width, height: e.video.params.height }
    : { width: e.canvas.width, height: e.canvas.height };
  const point = picturePoint(e.canvas.getBoundingClientRect(), ev.clientX, ev.clientY, picture);
  if (!point) return;
  const button = ev.button > 0 ? ev.button : 0;
  fetch("/pointer", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(pointerBody(paneId, kind, button, point)),
  })
    .then((res) => { if (res.ok) sent.pointers += 1; else sent.refused += 1; })
    .catch(() => { sent.refused += 1; });
}

function applyGeometry(g) {
  generation = g.generation;
  cell = { w: g.cell_width_px, h: g.cell_height_px };
  let maxX = 0;
  let maxY = 0;
  for (const p of g.panes) {
    maxX = Math.max(maxX, p.x + p.width);
    maxY = Math.max(maxY, p.y + p.height);
  }
  fit(maxX, maxY);
  const seen = new Set();
  for (const p of g.panes) {
    seen.add(p.pane_id);
    const e = entry(p.pane_id);
    // Outer box places the pane, inner box is the drawing area: a player that
    // paints the outer box paints over the border the terminal drew.
    Object.assign(e.el.style, {
      left: `${p.x * scale}px`, top: `${p.y * scale}px`,
      width: `${p.width * scale}px`, height: `${p.height * scale}px`,
    });
    e.el.classList.toggle("focus", !!p.focused);
    Object.assign(e.canvas.style, {
      left: `${(p.inner_x - p.x) * scale}px`, top: `${(p.inner_y - p.y) * scale}px`,
      width: `${p.inner_width * scale}px`, height: `${p.inner_height * scale}px`,
    });
  }
  // Panes the server stopped naming are not on the active tab: absence is
  // the visibility signal (wire.rs), so they leave the stage.
  for (const [id, e] of panes) {
    if (!seen.has(id)) e.el.hidden = true;
    else e.el.hidden = false;
  }
  redrawHud();
}

// ── Audio: the clock ────────────────────────────────────────────────────────
function openAudio(paneId, params) {
  const e = entry(paneId);
  if (e.audio) closeAudio(e);
  const timeline = new AudioTimeline({ targetDelayUs: params.target_latency_us, frameUs: FRAME_US });
  const lane = { params, timeline, decoder: null, gain: null, decodeErrors: 0, received: 0 };
  const decoder = new AudioDecoder({
    output: (data) => onAudioData(e, lane, data),
    error: (err) => { lane.decodeErrors += 1; console.warn("audio decoder", paneId, err); },
  });
  decoder.configure({ codec: "opus", sampleRate: params.sample_rate_hz, numberOfChannels: params.channels });
  lane.decoder = decoder;
  e.audio = lane;
}

function closeAudio(e) {
  try { e.audio?.decoder?.close(); } catch (_) { /* already closed */ }
  e.audio = null;
}

function onAudioData(e, lane, data) {
  if (!audioCtx) { data.close(); return; }
  if (!lane.gain) {
    lane.gain = audioCtx.createGain();
    lane.gain.connect(audioCtx.destination);
  }
  const frames = data.numberOfFrames;
  const channels = data.numberOfChannels;
  const buffer = audioCtx.createBuffer(channels, frames, data.sampleRate);
  if (data.format === "f32-planar") {
    for (let ch = 0; ch < channels; ch += 1) {
      const plane = new Float32Array(frames);
      data.copyTo(plane, { planeIndex: ch });
      buffer.copyToChannel(plane, ch);
    }
  } else {
    // Ask the browser to convert; every shipping WebCodecs does.
    for (let ch = 0; ch < channels; ch += 1) {
      const plane = new Float32Array(frames);
      data.copyTo(plane, { planeIndex: ch, format: "f32-planar" });
      buffer.copyToChannel(plane, ch);
    }
  }
  const slot = lane.timeline.schedule(data.timestamp, audioCtx.currentTime);
  data.close();
  if (!slot) return;
  if (measure) {
    const samples = buffer.getChannelData(0);
    let onset = -1;
    for (let i = 0; i < samples.length; i += 1) {
      if (Math.abs(samples[i]) > BIP_LEVEL) { onset = i; break; }
    }
    if (onset >= 0 && probe.quietAudio) {
      const inBufferUs = (onset / buffer.sampleRate) * 1e6;
      probe.bips.push({ outUs: Math.round(slot.at * 1e6 + inBufferUs), ptsUs: Math.round(data.timestamp + inBufferUs) });
    }
    probe.quietAudio = onset < 0;
  }
  const source = audioCtx.createBufferSource();
  source.buffer = buffer;
  source.connect(lane.gain);
  source.start(slot.at);
}

// ── Video: follows the clock ────────────────────────────────────────────────
function openVideo(paneId, params) {
  const e = entry(paneId);
  if (e.video) closeVideo(e);
  e.canvas.width = params.width;
  e.canvas.height = params.height;
  const frameUs = Math.max(1, Math.round(1e6 / (params.fps || 30)));
  const lane = {
    params,
    frameUs,
    pacer: new VideoPacer({ frameUs, discard: (p) => p.frame.close() }),
    arrival: new ArrivalClock({ targetDelayUs: params.target_latency_us }),
    decoder: null,
    decodeErrors: 0,
    decoded: 0,
    received: 0,
  };
  const decoder = new VideoDecoder({
    output: (frame) => {
      lane.decoded += 1;
      lane.arrival.anchor(frame.timestamp, performance.now() * 1000);
      lane.pacer.offer({ ptsUs: frame.timestamp, frame });
    },
    error: (err) => { lane.decodeErrors += 1; console.warn("video decoder", paneId, err); },
  });
  // The profile/level ride in the SPS; Annex-B carries no description.
  decoder.configure({ codec: "avc1.4d0015", optimizeForLatency: true });
  lane.decoder = decoder;
  e.video = lane;
}

function closeVideo(e) {
  try { e.video?.decoder?.close(); } catch (_) { /* already closed */ }
  e.video = null;
}

/** The server moment this pane should be showing right now. */
function paneClockUs(e) {
  if (e.audio && audioCtx && e.audio.timeline.anchored()) {
    return e.audio.timeline.audioNowUs(audioCtx.currentTime);
  }
  return e.video ? e.video.arrival.nowUs(performance.now() * 1000) : null;
}

function present() {
  for (const e of panes.values()) {
    const lane = e.video;
    if (!lane) continue;
    const clockNow = paneClockUs(e);
    if (clockNow === null) continue;
    const picture = lane.pacer.next(clockNow);
    if (!picture) continue;
    e.ctx2d.drawImage(picture.frame, 0, 0, e.canvas.width, e.canvas.height);
    e.lastDriftUs = clockNow - picture.ptsUs;
    picture.frame.close();
    if (measure && audioCtx) {
      const cx = e.canvas.width >> 1;
      const cy = e.canvas.height >> 1;
      const px = e.ctx2d.getImageData(cx - 4, cy - 4, 8, 8).data;
      let sum = 0;
      for (let i = 0; i < px.length; i += 4) sum += px[i] + px[i + 1] + px[i + 2];
      const bright = sum / (px.length / 4) / 3 > FLASH_LEVEL;
      if (bright && probe.darkPicture) probe.flashes.push({ outUs: Math.round(audioCtx.currentTime * 1e6), ptsUs: picture.ptsUs });
      probe.darkPicture = !bright;
    }
  }
  requestAnimationFrame(present);
}
requestAnimationFrame(present);

// ── The one stream ──────────────────────────────────────────────────────────
function onEvent(ev) {
  switch (ev.type) {
    case "open":
      if (ev.lane === "audio") openAudio(ev.pane, ev.params);
      else openVideo(ev.pane, ev.params);
      break;
    case "chunk": {
      const e = entry(ev.pane);
      if (ev.lane === "audio") {
        if (!audioCtx) { dropped.audioBeforeStart += 1; break; }
        if (!e.audio) break;
        e.audio.received += 1;
        e.audio.decoder.decode(new EncodedAudioChunk({ type: "key", timestamp: ev.ptsUs, data: ev.data }));
      } else if (e.video) {
        e.video.received += 1;
        if (gate.admit(ev.lane, ev.key) === "drop") break;
        e.video.decoder.decode(new EncodedVideoChunk({ type: ev.key ? "key" : "delta", timestamp: ev.ptsUs, data: ev.data }));
      }
      break;
    }
    case "close": {
      const e = entry(ev.pane);
      if (ev.lane === "audio") closeAudio(e);
      else closeVideo(e);
      break;
    }
    default:
      break; // loss and timesync only feed the HUD's counters
  }
}

async function openStream() {
  const res = await fetch("/stream");
  const reader = res.body.getReader();
  streamState = "akıyor";
  for (;;) {
    const { value, done } = await reader.read();
    if (done) { streamState = "bitti"; break; }
    for (const ev of demux.push(value)) onEvent(ev);
  }
}
openStream().catch((err) => { streamState = `akış: ${err}`; redrawHud(); });

const geometry = new EventSource("/geometry");
geometry.onmessage = (m) => { try { applyGeometry(JSON.parse(m.data)); } catch (err) { streamState = `geo: ${err}`; } };
geometry.onerror = () => { streamState = "geometri akışı koptu"; redrawHud(); };

startButton.addEventListener("click", async () => {
  audioCtx = new AudioContext({ sampleRate: 48000, latencyHint: "interactive" });
  await audioCtx.resume();
  startButton.hidden = true;
});

// ── HUD ─────────────────────────────────────────────────────────────────────
function redrawHud() {
  const rows = [];
  for (const [id, e] of panes) {
    const v = e.video;
    const a = e.audio;
    const drift = e.lastDriftUs === null ? "" : `  A−V ${(e.lastDriftUs / 1000).toFixed(0)} ms`;
    rows.push(
      `${id}` +
        (v ? `  video ${v.received} al / ${v.decoded} çöz / ${v.pacer.counters.shown} göst / ${v.pacer.counters.late} geç / ${v.pacer.counters.dropped} at / ${v.pacer.counters.skipped} atla${v.decodeErrors ? ` / ${v.decodeErrors} HATA` : ""}` : "  video —") +
        (a ? `  ses ${a.received} al / ${a.timeline.counters.scheduled} plan / ${a.timeline.counters.late} geç / ${a.timeline.counters.gaps} boşluk${a.decodeErrors ? ` / ${a.decodeErrors} HATA` : ""}` : "  ses —") +
        drift,
    );
  }
  const c = demux.counters;
  hud.textContent =
    `${streamState} · gen ${generation} · hücre ${cell ? `${cell.w}x${cell.h}` : "?"} · ölçek ${scale.toFixed(2)}\n` +
    `demux: kayıp ${c.lost} · anahtar-öncesi ${c.skippedUntilKey} · açılmamış ${c.unknownStream} · gizli-at ${gate.counters.hiddenDropped} · dönüş-atla ${gate.counters.resumedSkipped}` +
    ` · işaretçi ${sent.pointers}${sent.refused ? ` / ${sent.refused} ret` : ""}` +
    (audioCtx ? ` · ses saati ${audioCtx.currentTime.toFixed(1)} s` : ` · ses başlatılmadı (${dropped.audioBeforeStart} chunk düştü)`) +
    (rows.length ? `\n${rows.join("\n")}` : "\n(pane yok)") +
    (measure ? `\nölçüm: ${probeLine()}` : "");
}
function avReport() {
  const flashes = probe.flashes.slice(-10);
  const errorsUs = avErrorsUs(flashes, probe.bips, PAIR_WINDOW_US);
  const rawUs = avOffsetsUs(flashes.map((f) => f.outUs), probe.bips.map((b) => b.outUs), PAIR_WINDOW_US);
  return {
    bips: probe.bips.length,
    flashes: probe.flashes.length,
    paired: errorsUs.length,
    errorsUs,
    medianAbsUs: medianUs(errorsUs.map(Math.abs)),
    medianSignedUs: medianUs(errorsUs),
    rawMedianSignedUs: medianUs(rawUs),
  };
}

function probeLine() {
  const r = avReport();
  return `bip ${r.bips} · flaş ${r.flashes} · eşleşen ${r.paired}` +
    (r.medianAbsUs === null ? "" : ` · |A−V hata| medyan ${(r.medianAbsUs / 1000).toFixed(1)} ms (işaretli ${(r.medianSignedUs / 1000).toFixed(1)} ms; + = resim geç · ham çıkış farkı ${(r.rawMedianSignedUs / 1000).toFixed(1)} ms)`);
}
window.herdrProbe = probe;
window.herdrAvReport = avReport;
// The browser may sit at the far end of an ssh tunnel; the report goes back
// over the same wire so a shell beside the server can read `/report`.
if (measure) {
  setInterval(() => {
    const report = avReport();
    report.hud = hud.textContent;
    report.at = new Date().toISOString();
    fetch("/report", { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(report) }).catch(() => {});
  }, 5000);
}
setInterval(redrawHud, 500);
