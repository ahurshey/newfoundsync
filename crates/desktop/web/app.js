"use strict";
// Newfoundsync web client.
//
// Connects a WebSocket to the Rust server, NTP-syncs to the server's monotonic
// clock, and plays the buffered audio (Opus -> WebCodecs -> Web Audio) and video
// (H.264 -> WebCodecs -> canvas) at each frame's PTS deadline on the synced clock.
// Every client uses the same buffer, so they all play in lock-step.
//
// Mobile notes (iOS Safari + Android Chrome), hard-won:
//  - Audio must be unlocked SYNCHRONOUSLY inside the tap gesture (silent 1-sample
//    buffer), and the AudioContext re-resumed on interruptions (calls/Siri/bg).
//  - Don't pin the AudioContext sample rate; iOS may run at 44.1k and resamples.
//  - iOS VideoDecoder needs AVCC (length-prefixed NALs) + an avcC `description`;
//    the server sends Annex-B, so we parse SPS/PPS from the first keyframe, build
//    the description, and convert every frame to AVCC. (Chrome accepts this too.)
//  - Decoded VideoFrames are huge (full surfaces) — cap the queue by buffer TIME,
//    not a fixed big count, or a backgrounded tab OOMs the phone.
//  - rAF stops when backgrounded: on hide we stop enqueueing; on show we flush,
//    re-wait for a keyframe, and re-anchor the clock.
//  - Screen Wake Lock keeps the phone awake; it's auto-released on background and
//    must be re-acquired on visibilitychange.

const MSG_AUDIO = 0x01;
const MSG_VIDEO = 0x02;
const MSG_CLOCK_REQ = 0x10;
const MSG_CLOCK_RSP = 0x11;
const MSG_SET_VOLUME = 0x20; // server→client: set this device's remote volume (f32 LE)
const MSG_HELLO = 0x21; // client→server: identify with a stable id + friendly name
const MSG_SET_TRIM = 0x23; // server→client: set this device's sync offset, ms (i32 LE)
const MSG_CLIENT_SYNC = 0x24; // client→server: report this device's ACTUAL effective sync, ms (i32 LE)
const MSG_CALIB_CTRL = 0x22; // server↔client: calibration orchestration (ROLE / STATUS sub-types)

const els = {
  dot: document.getElementById("dot"),
  srv: document.getElementById("srv"),
  start: document.getElementById("start"),
  hint: document.getElementById("hint"),
  warn: document.getElementById("warn"),
  fsbtn: document.getElementById("fsbtn"),
  zoomout: document.getElementById("zoomout"),
  zoomin: document.getElementById("zoomin"),
  themebtn: document.getElementById("themebtn"),
  controls: document.getElementById("controls"),
  mute: document.getElementById("mute"),
  stop: document.getElementById("stop"),
  calib: document.getElementById("calib"),
  calibroles: document.getElementById("calibroles"),
  calibref: document.getElementById("calibref"),
  caliblisten: document.getElementById("caliblisten"),
  calibcancel: document.getElementById("calibcancel"),
  calibstatus: document.getElementById("calibstatus"),
  vol: document.getElementById("vol"),
  trim: document.getElementById("trim"),
  trimval: document.getElementById("trimval"),
  trimdown: document.getElementById("trimdown"),
  trimup: document.getElementById("trimup"),
  stage: document.getElementById("stage"),
  vlogo: document.getElementById("vlogo"),
  vlogoimg: document.getElementById("vlogoimg"),
  viz: document.getElementById("viz"),
  viztoggle: document.getElementById("viztoggle"),
  hero: document.getElementById("hero"),
  biglogo: document.getElementById("biglogo"),
  buffering: document.getElementById("buffering"),
  bufbar: document.getElementById("bufbar"),
  bufbarfill: document.getElementById("bufbarfill"),
  buftext: document.getElementById("buftext"),
  canvas: document.getElementById("video"),
  state: document.getElementById("state"),
  sync: document.getElementById("sync"),
  buf: document.getElementById("buf"),
  ai: document.getElementById("ai"),
  vi: document.getElementById("vi"),
  namemodal: document.getElementById("namemodal"),
  nameinput: document.getElementById("nameinput"),
  namesave: document.getElementById("namesave"),
  nameskip: document.getElementById("nameskip"),
};
const ctx2d = els.canvas.getContext("2d", { alpha: false });

const isIOS =
  /iP(hone|od|ad)/.test(navigator.platform) ||
  (/Mac/.test(navigator.userAgent) && "ontouchend" in document); // iPadOS masquerades as Mac

function setState(s, cls) {
  els.state.textContent = s;
  els.dot.className = cls || "";
}
function showWarn(html) {
  els.warn.innerHTML = html;
  els.warn.style.display = "block";
}

// ---- session state (one AudioContext for the whole session) -----------------
let started = false;
let stopping = false;
let everPlayed = false; // reached playback once → don't re-show the buffering bar on re-anchors
let wired = false; // one-time listeners attached
let ac = null;
let gain = null;
let analyser = null; // taps the output for the audio visualizer
let wakeLock = null;
let volume = 1; // 0..1, persisted (the device's own slider)
let muted = false;
let remoteVol = 1; // 0..1, server-controlled (SET_VOLUME); multiplies the local volume
let trimMs = 0; // per-device sync trim (ms), persisted: + = play later, - = earlier
let remoteTrimMs = 0; // server-controlled sync offset (SET_TRIM, ms); ADDS to the local trim
let chosenName = null; // in-memory device name; survives a failed localStorage write this session

// The playout offset that actually drives scheduling = this device's own trim plus the
// server-pushed offset. Used everywhere we translate a server PTS / shared-clock tick into
// a local playout time (including calibration), so a server nudge can't silently misalign.
function effTrimMs() {
  return trimMs + remoteTrimMs;
}

// Tell the server our ACTUAL current sync offset (own trim + the server's remote offset), so the
// server GUI shows each device's real value instead of the commanded 0. Coalesced: a slider drag
// or a burst of calibration setTrim() calls schedules one send that carries the latest value.
let _syncReportTimer = 0;
function reportSync() {
  if (_syncReportTimer) return; // a send is already queued; it reads effTrimMs() when it fires
  _syncReportTimer = setTimeout(() => {
    _syncReportTimer = 0;
    if (!ws || ws.readyState !== WebSocket.OPEN) return;
    const buf = new ArrayBuffer(5);
    const dv = new DataView(buf);
    dv.setUint8(0, MSG_CLIENT_SYNC);
    dv.setInt32(1, Math.round(effTrimMs()), true); // i32 LE ms
    try { ws.send(buf); } catch (e) {}
  }, 150);
}

// Acoustic auto-calibration state (see the "Acoustic auto-calibration" section below).
// Two roles: "ref" plays a dial-up sync tone on the shared-clock beat; "listen" hears
// that tone via the mic and nudges THIS device's trim to match. `active` gates mic
// capture; everything else is set up only while a role is running.
const calib = {
  role: null, // "ref" | "listen" | null
  running: false, // a listener measurement session is in progress
  active: false, // a single mic-capture window is open
  mic: null, // { stream, source, node, sink }
  worker: null, // DSP worker (FFT / matched filter)
  micChunks: null, // [Float32Array] raw mic blocks @ ac.sampleRate
  micT0: null, // ac-time (s) of the first captured mic sample
  micLen: 0,
  micNextFrame: null, // expected frame index of the next batch (gaps are silence-padded)
  workletReady: false, // mic-tap AudioWorklet module added once
  pendingResolve: null, // resolver for an in-flight worker measurement (for clean abort)
  refTimer: null, // reference-role scheduler interval
  scheduled: null, // Set of shared-clock tick-ns already scheduled (ref role)
  toneBuf: null, // the dial-up tone AudioBuffer (ref role)
  refSources: null, // [AudioBufferSourceNode] live scheduled tones (so we can stop them)
  runSeq: 0, // monotonic listener-session counter (stale-finally guard)
  // ---- Server-orchestrated "Calibrate all" (Phase B) ----
  orchestrated: false, // this session was started by the server, not a local button
  refSeed: null, // reference code seed (shared by ref + followers); null ⇒ manual default
  selfSeed: null, // this follower's distinct self-test seed (CDMA, currently informational)
  slot: 0, // this follower's TDMA self-test slot (stagger so self-tests don't collide)
};
// Coded path is active either by the local flag OR a server-orchestrated session.
function calibCoded() { return CALCFG.CODED || calib.orchestrated; }
// Per-slot TDMA stagger for followers' self-tests. ONE self-test is measureSelfLoop's 5 reps ×
// ~1.67 s ≈ 8.4 s, so the slot must exceed that for the (loud, identical) chirps to truly
// serialize and not overlap in the shared room. (A distinct coded self-test per follower would
// allow a shorter slot, but the self-test deliberately reuses the proven chirp path.)
const CALIB_SELF_SLOT_MS = 9500;

// Reconstruct the slider's filled portion (custom WebKit track removes accent-color).
function paintSlider(el) {
  const min = +el.min,
    max = +el.max;
  const pct = max > min ? ((+el.value - min) / (max - min)) * 100 : 0;
  el.style.setProperty("--fillpct", pct.toFixed(1) + "%");
}

function loadVolume() {
  try {
    const v = parseFloat(localStorage.getItem("nfs_vol"));
    if (!Number.isNaN(v)) volume = Math.min(1, Math.max(0, v));
  } catch (e) {}
  els.vol.value = String(volume);
  paintSlider(els.vol);
}
function applyGain() {
  // Effective gain = the device's own volume × the server-controlled remote volume.
  if (gain) gain.gain.value = muted ? 0 : volume * remoteVol;
  const off = muted || volume === 0;
  els.mute.textContent = off ? "🔇 Muted" : "🔊 Sound on";
  els.mute.classList.toggle("muted", off);
}
els.vol.addEventListener("input", () => {
  const v = parseFloat(els.vol.value);
  if (!Number.isFinite(v)) return; // never let a NaN reach gain.gain.value / localStorage
  volume = v;
  muted = false;
  paintSlider(els.vol);
  try {
    localStorage.setItem("nfs_vol", String(volume));
  } catch (e) {}
  applyGain();
});
els.mute.addEventListener("click", () => {
  muted = !muted;
  applyGain();
});
els.stop.addEventListener("click", stop);

function loadTrim() {
  try {
    const t = parseFloat(localStorage.getItem("nfs_trim"));
    if (!Number.isNaN(t)) trimMs = t;
  } catch (e) {}
  els.trim.value = String(trimMs);
  els.trimval.textContent = (trimMs >= 0 ? "+" : "") + trimMs.toFixed(1) + " ms";
  paintSlider(els.trim);
}
function setTrim(ms) {
  if (!Number.isFinite(ms)) return; // a NaN would poison serverPtsToPerfMs → all scheduling stops (unrecoverable without reload)
  trimMs = Math.max(-2000, Math.min(3000, Math.round(ms * 10) / 10)); // 0.1 ms resolution (calibration is sub-ms)
  els.trim.value = String(trimMs);
  els.trimval.textContent = (trimMs >= 0 ? "+" : "") + trimMs.toFixed(1) + " ms";
  paintSlider(els.trim);
  try {
    localStorage.setItem("nfs_trim", String(trimMs));
  } catch (e) {}
  aPlayhead = null; // re-anchor audio so the change takes effect immediately
  flushVideo(); // re-time the video queue to the new offset
  reportSync(); // let the server GUI reflect this device's real sync
}
els.trim.addEventListener("input", () => setTrim(parseFloat(els.trim.value)));
els.trimdown.addEventListener("click", () => setTrim(trimMs - 10));
els.trimup.addEventListener("click", () => setTrim(trimMs + 10));

// ---- Light/dark theme toggle (persisted; default dark). The <head> pre-applies the saved
// theme before paint to avoid a flash; here we keep the button icon + theme-color meta in sync.
function applyTheme(light) {
  document.documentElement.classList.toggle("light", light);
  if (els.themebtn) {
    els.themebtn.setAttribute("aria-checked", light ? "true" : "false");
    const knob = els.themebtn.querySelector(".themeknob");
    if (knob) knob.textContent = light ? "☀" : "🌙";
  }
  const meta = document.querySelector('meta[name="theme-color"]');
  if (meta) meta.setAttribute("content", light ? "#f4f6fa" : "#0b0f15");
  try { localStorage.setItem("nfs_theme", light ? "light" : "dark"); } catch (e) {}
}
applyTheme(document.documentElement.classList.contains("light")); // sync icon/meta with the pre-applied theme
if (els.themebtn) {
  els.themebtn.addEventListener("click", () => applyTheme(!document.documentElement.classList.contains("light")));
}

loadVolume();
loadTrim();

// ---- page scaling (header − / + buttons, like browser zoom) -----------------
let pageZoom = 1; // 1 = 100%; persisted
function applyZoom() { try { document.documentElement.style.zoom = String(pageZoom); } catch (e) {} }
function setZoom(z) {
  pageZoom = Math.min(2, Math.max(0.5, Math.round(z * 20) / 20)); // 5% grid, clamp 50%–200%
  try { localStorage.setItem("nfs_zoom", String(pageZoom)); } catch (e) {}
  applyZoom();
}
try { const z = parseFloat(localStorage.getItem("nfs_zoom")); if (!Number.isNaN(z)) pageZoom = Math.min(2, Math.max(0.5, z)); } catch (e) {}
applyZoom();
if (els.zoomout) els.zoomout.addEventListener("click", () => setZoom(pageZoom - 0.1));
if (els.zoomin) els.zoomin.addEventListener("click", () => setZoom(pageZoom + 0.1));

// ---- PWA: register the service worker (installable + offline app shell) -----
// Network-first (see sw.js), so this never serves stale code while the server is reachable.
if ("serviceWorker" in navigator) {
  window.addEventListener("load", () => {
    navigator.serviceWorker.register("/sw.js").catch((e) => console.warn("SW register failed", e));
  });
}

// ---- audio visualizer (the logo, shown for audio-only sources) --------------
// A circular spectrum drawn around the logo from the AnalyserNode, plus a bass-driven
// "breathe" on the logo. Toggleable (button under the logo), default on, persisted.
const VIZ = { raf: 0, enabled: true, ctx: null, freq: null };

function applyVizUI() {
  if (els.viztoggle) els.viztoggle.textContent = "Visualizer: " + (VIZ.enabled ? "On" : "Off");
  if (els.vlogo) els.vlogo.classList.toggle("viz-off", !VIZ.enabled);
}
function vizStop() {
  if (VIZ.raf) { cancelAnimationFrame(VIZ.raf); VIZ.raf = 0; }
  if (VIZ.ctx && els.viz) VIZ.ctx.clearRect(0, 0, els.viz.width, els.viz.height);
  if (els.vlogoimg) els.vlogoimg.style.transform = "";
}
function vizStart() {
  if (!VIZ.enabled || !analyser || VIZ.raf) return;
  if (!els.vlogo || els.vlogo.style.display === "none") return; // only while the logo is shown
  if (!VIZ.ctx && els.viz) VIZ.ctx = els.viz.getContext("2d");
  if (!VIZ.ctx) return;
  if (!VIZ.freq) VIZ.freq = new Uint8Array(analyser.frequencyBinCount);
  VIZ.raf = requestAnimationFrame(vizDraw);
}
function vizDraw() {
  VIZ.raf = 0;
  if (!VIZ.enabled || !analyser || !els.vlogo || els.vlogo.style.display === "none") { vizStop(); return; }
  analyser.getByteFrequencyData(VIZ.freq);
  const c = els.viz, x = VIZ.ctx, W = c.width, H = c.height, cx = W / 2, cy = H / 2;
  x.clearRect(0, 0, W, H);
  const bars = 56, inner = W * 0.37, maxLen = W * 0.105; // tight ring hugging the logo
  const usable = Math.floor(VIZ.freq.length * 0.62); // skip the near-empty top bins
  x.lineCap = "round";
  x.lineWidth = 3.4;
  for (let i = 0; i < bars; i++) {
    const bin = 2 + Math.floor((i / bars) * usable);
    const mag = VIZ.freq[bin] / 255;
    const len = maxLen * Math.pow(mag, 1.45);
    const ang = (i / bars) * Math.PI * 2 - Math.PI / 2;
    const ca = Math.cos(ang), sa = Math.sin(ang);
    x.strokeStyle = "rgba(108,182,255," + (0.3 + 0.55 * mag).toFixed(3) + ")";
    x.beginPath();
    x.moveTo(cx + ca * inner, cy + sa * inner);
    x.lineTo(cx + ca * (inner + len), cy + sa * (inner + len));
    x.stroke();
  }
  // The logo no longer scales with audio — its size is identical whether the visualizer
  // is on or off (only the ring reacts). The ring around it is the visualizer.
  VIZ.raf = requestAnimationFrame(vizDraw);
}
if (els.viztoggle) {
  els.viztoggle.addEventListener("click", () => {
    VIZ.enabled = !VIZ.enabled;
    try { localStorage.setItem("nfs_viz", VIZ.enabled ? "1" : "0"); } catch (e) {}
    applyVizUI();
    if (VIZ.enabled) vizStart(); else vizStop();
  });
}
try { const v = localStorage.getItem("nfs_viz"); VIZ.enabled = v === null ? true : v === "1"; } catch (e) {}
applyVizUI();

// ---- per-connection state (reset on every (re)connect) ----------------------
let ws = null;
let cfg = null;
let bufferMs = 3000;
let offsetNs = null; // median(serverNs - clientPerfNs)
let offsets = [];
let pending = []; // clock-req send times (FIFO)
let audioDecoder = null;
let videoDecoder = null;
let gotParams = false; // have we configured the video decoder from SPS/PPS yet?
// Decoder accel: let the browser choose (hardware when good, software otherwise). Desktop
// Chrome often lacks usable hardware H.264 in WebCodecs, so we fall back to software on
// repeated errors instead of looping on a dead hardware path. (Async failures arrive via
// the error callback, not the synchronous configure() — see onDecErr.)
let videoAccel = "no-preference";
let aPlayhead = null; // AudioContext time of the next audio frame (gapless scheduler)
let firstPlayoutAc = null; // ac time the first buffered audio is scheduled to sound
let evq = []; // ENCODED video queue [{key, tsUs, data}] — cheap, holds the whole buffer
let vq = []; // DECODED render queue [{frame, targetPerf}] — small, kept just ahead
let maxEvq = 400; // encoded queue cap (recomputed from fps + bufferMs)
let needDecodeKey = false; // after an encoded-queue overflow, resync decode at a keyframe
let aFrames = 0;
let vFrames = 0;
let vDims = "";
let f32pool = null; // reused per-channel scratch for audio copyTo

// Decode video just-in-time: keep only ~this far of DECODED frames ahead of
// playout (decoded frames are huge); the deep buffer lives as cheap encoded chunks.
const DECODE_AHEAD_MS = 500;
const MAX_DECODED = 36; // hard ceiling on decoded frames held at once
let reconnectAttempts = 0;
let reconnectTimer = null;
let keepaliveTimer = null;
let statsTimer = null;

// ---- capability gate (existence only; precise check happens after config) ---
if (!window.isSecureContext) {
  // Plain http:// to a LAN IP is an insecure context, so the browser hides the
  // WebCodecs decoder. The server serves https for exactly this reason.
  showWarn(
    "⚠ This page isn't secure, so the browser blocks audio/video decoding. Open the " +
      "<b>https://</b> address shown in the server window and accept the one-time certificate warning."
  );
  els.start.disabled = true;
} else if (!window.AudioDecoder || !window.VideoDecoder || !window.AudioContext) {
  showWarn(
    "⚠ This browser lacks <b>WebCodecs</b>. Use Chrome/Edge, or update to <b>iOS&nbsp;17+</b> (Safari)."
  );
  els.start.disabled = true;
}

// =============================================================================
// H.264 Annex-B -> AVCC helpers (so iOS Safari's VideoDecoder is happy)
// =============================================================================
function nalType(nal) {
  return nal[0] & 0x1f;
}

// Split an Annex-B byte stream into NAL units (handles 3- and 4-byte start codes).
function splitNalsAnnexB(u8) {
  const nals = [];
  const n = u8.length;
  let start = -1;
  for (let p = 0; p + 2 < n; p++) {
    if (u8[p] === 0 && u8[p + 1] === 0 && u8[p + 2] === 1) {
      if (start >= 0) {
        let end = p;
        if (end > start && u8[end - 1] === 0) end--; // trim the 4-byte code's extra 00
        if (end > start) nals.push(u8.subarray(start, end));
      }
      start = p + 3;
      p = start - 1;
    }
  }
  if (start >= 0 && start < n) nals.push(u8.subarray(start, n));
  return nals;
}

// Re-emit NALs as length-prefixed (4-byte BE) AVCC.
function annexBToAvcc(u8) {
  const nals = splitNalsAnnexB(u8);
  if (!nals.length) return null;
  let total = 0;
  for (const x of nals) total += 4 + x.length;
  const out = new Uint8Array(total);
  let o = 0;
  for (const x of nals) {
    out[o] = (x.length >>> 24) & 255;
    out[o + 1] = (x.length >>> 16) & 255;
    out[o + 2] = (x.length >>> 8) & 255;
    out[o + 3] = x.length & 255;
    out.set(x, o + 4);
    o += 4 + x.length;
  }
  return out;
}

// Build an avcC (AVCDecoderConfigurationRecord) from one SPS + one PPS NAL.
function buildAvcC(sps, pps) {
  const len = 6 + 2 + sps.length + 1 + 2 + pps.length;
  const b = new Uint8Array(len);
  let o = 0;
  b[o++] = 1; // configurationVersion
  b[o++] = sps[1]; // AVCProfileIndication
  b[o++] = sps[2]; // profile_compatibility
  b[o++] = sps[3]; // AVCLevelIndication
  b[o++] = 0xff; // reserved(6) + lengthSizeMinusOne(2)=3
  b[o++] = 0xe1; // reserved(3) + numOfSequenceParameterSets(5)=1
  b[o++] = (sps.length >> 8) & 255;
  b[o++] = sps.length & 255;
  b.set(sps, o);
  o += sps.length;
  b[o++] = 1; // numOfPictureParameterSets
  b[o++] = (pps.length >> 8) & 255;
  b[o++] = pps.length & 255;
  b.set(pps, o);
  return b;
}

function codecFromSps(sps) {
  const h = (x) => x.toString(16).padStart(2, "0");
  return "avc1." + h(sps[1]) + h(sps[2]) + h(sps[3]);
}

// =============================================================================
// Start (user gesture)
// =============================================================================
els.start.addEventListener("click", onStart);

function onStart() {
  if (started) return;
  started = true;
  stopping = false;
  everPlayed = false; // a fresh, user-initiated connect → the buffering bar may show again
  videoAccel = "no-preference"; // re-evaluate decoder accel each fresh start (don't stay stuck on software)
  vDecErrStreak = 0;
  vGoodRun = 0;

  // CRITICAL (iOS): the AudioContext must be created + unlocked synchronously inside
  // the gesture. Reuse it across stop/restart (browsers cap how many you can make).
  try {
    if (!ac) {
      const Ctx = window.AudioContext || window.webkitAudioContext;
      // Pin to 48 kHz to MATCH the Opus decode rate: if the context ran at the device's
      // native rate (often 44.1 kHz), Web Audio would resample every ~20 ms frame
      // independently → audible boundary seams. A 48 kHz graph plays our buffers verbatim.
      // Some devices reject a forced rate → fall back to an unpinned context (then behaves
      // exactly as before, no regression).
      try {
        ac = new Ctx({ latencyHint: "playback", sampleRate: 48000 });
      } catch (e) {
        ac = new Ctx({ latencyHint: "playback" });
      }
      gain = ac.createGain();
      gain.connect(ac.destination);
      analyser = ac.createAnalyser(); // tap for the visualizer (analyses; no onward connection needed)
      analyser.fftSize = 512;
      analyser.smoothingTimeConstant = 0.82;
      gain.connect(analyser);
      applyGain(); // honor the saved volume / mute
      const s = ac.createBufferSource(); // 1-sample silent blip unlocks the session
      s.buffer = ac.createBuffer(1, 1, ac.sampleRate);
      s.connect(ac.destination);
      s.start(0);
      ac.onstatechange = () => {
        if (ac && ac.state !== "running" && !stopping) ac.resume().catch(() => {});
      };
    }
    ac.resume().catch(() => {}); // first start AND restart-after-stop
  } catch (e) {
    started = false;
    showWarn("⚠ Audio init failed: " + e.message);
    return;
  }

  requestWakeLock(); // also inside the gesture

  els.warn.style.display = "none";
  if (isIOS) {
    showWarn(
      "iPhone tip: if you hear nothing, flip the <b>Ring/Silent</b> switch to Ring and turn the volume up — the silent switch mutes web audio."
    );
  }

  els.start.style.display = "none";
  els.hint.style.display = "none";
  els.hero.style.display = "none";
  // Show the (boxless) logo right away — branding while we connect/buffer, and it stays for
  // audio-only sources. The 16:9 stage box only appears once real video frames arrive.
  els.stage.style.display = "none";
  els.vlogo.style.display = "flex";
  vizStart();
  els.controls.style.display = "flex";
  showBuffering(true, 0, "Connecting…"); // show the loading bar immediately
  setState("connecting", "warn");

  if (!wired) {
    wired = true;
    // Fullscreen toggle (real Fullscreen API where supported, else iOS immersive CSS).
    els.fsbtn.addEventListener("click", toggleFullscreen);
    els.stage.addEventListener("dblclick", toggleFullscreen);
    document.addEventListener("visibilitychange", onVisibility);
  }

  connect();
}

// Stop playback + disconnect; return to the start screen so the user can resume.
function stop() {
  stopping = true;
  started = false;
  clearTimeout(reconnectTimer);
  calibAbort("Stopped."); // tear the mic down before we suspend the AudioContext
  teardownConnection(); // close ws, clear timers/decoders/queues (no reconnect)
  if (ac) ac.suspend().catch(() => {}); // silence already-scheduled audio at once
  if (wakeLock) {
    try {
      wakeLock.release();
    } catch (e) {}
    wakeLock = null;
  }
  showBuffering(null);
  els.controls.style.display = "none";
  els.stage.style.display = "none";
  els.vlogo.style.display = "none";
  vizStop();
  els.warn.style.display = "none";
  els.hero.style.display = "";
  els.hint.style.display = "";
  els.start.style.display = "";
  els.srv.textContent = "";
  setState("stopped", "");
}

// =============================================================================
// WebSocket connect / reconnect
// =============================================================================

// A stable identity for the server's per-client list. Persists across reloads and
// reconnects (localStorage) so the server can remember this device's volume.
// Best-effort friendly device label from the user agent. Browsers don't expose the real
// hostname, but the UA usually reveals the platform (and, on Android, the model). Returns
// null if we can't tell — the caller falls back to a random room name.
function uaDeviceLabel() {
  const ua = navigator.userAgent || "";
  if (/iPhone/.test(ua)) return "iPhone";
  if (/iPad/.test(ua) || (/Macintosh/.test(ua) && "ontouchend" in document)) return "iPad";
  if (/Android/.test(ua)) {
    const m = ua.match(/;\s*([^;()]+?)\s+Build\//); // e.g. "...; Pixel 7 Build/..."
    if (m && m[1] && m[1].trim().length > 1) return m[1].trim();
    return "Android phone";
  }
  if (/Windows/.test(ua)) return "Windows PC";
  if (/Macintosh|Mac OS X/.test(ua)) return "Mac";
  if (/CrOS/.test(ua)) return "Chromebook";
  if (/Linux/.test(ua)) return "Linux PC";
  return null;
}
function randomRoomName() {
  const adj = ["Sunny", "Quiet", "Cozy", "Brisk", "Amber", "Misty", "Lively", "Calm", "Bright", "Bold"];
  const noun = ["Harbour", "Cabin", "Parlour", "Kitchen", "Loft", "Studio", "Deck", "Den", "Porch", "Hall"];
  return `${adj[Math.floor(Math.random() * adj.length)]} ${noun[Math.floor(Math.random() * noun.length)]}`;
}
// First-run default name: the detected device label, else a random room name.
function defaultDeviceName() {
  return uaDeviceLabel() || randomRoomName();
}

function clientIdentity() {
  let id = null,
    name = null;
  try {
    id = localStorage.getItem("nfs_cid");
    name = localStorage.getItem("nfs_cname");
  } catch (e) {}
  if (chosenName) name = chosenName; // in-session pick wins (covers a failed storage write)
  if (!id) {
    id = "c" + Math.random().toString(36).slice(2, 10) + Date.now().toString(36);
    try { localStorage.setItem("nfs_cid", id); } catch (e) {}
  }
  if (!name) {
    name = defaultDeviceName();
    try { localStorage.setItem("nfs_cname", name); } catch (e) {}
  }
  return { id, name };
}

// Tell the server who we are: [0x21][id_len:u8][id utf8][name utf8].
function sendHello() {
  if (!ws || ws.readyState !== WebSocket.OPEN) return;
  const { id, name } = clientIdentity();
  const enc = new TextEncoder();
  const idB = enc.encode(id).subarray(0, 255); // id_len is a single byte
  const nameB = enc.encode(name);
  const buf = new Uint8Array(2 + idB.length + nameB.length);
  buf[0] = MSG_HELLO;
  buf[1] = idB.length;
  buf.set(idB, 2);
  buf.set(nameB, 2 + idB.length);
  try { ws.send(buf); } catch (e) {}
  reportSync(); // report our current sync offset right after identifying, so the GUI shows it on connect
}

// Has the user been offered the one-time "name this device" prompt yet?
function needsNamePrompt() {
  try { return localStorage.getItem("nfs_named") !== "1"; } catch (e) { return false; }
}

// Show the naming modal, prefilled with the current (random) name. Saving stores the
// chosen name and re-announces it to the server; skipping keeps the random default.
// Either way we won't ask again on this device.
function maybePromptName() {
  if (!needsNamePrompt() || !els.namemodal) return;
  if (els.namemodal.style.display === "flex") return; // already open — a reconnect mustn't reset typing
  els.nameinput.value = clientIdentity().name;
  els.namemodal.style.display = "flex";
  setTimeout(() => { try { els.nameinput.focus(); els.nameinput.select(); } catch (e) {} }, 60);
}
function closeNameModal(save) {
  if (!els.namemodal) return;
  if (save) {
    const nm = (els.nameinput.value || "").trim().slice(0, 40);
    if (nm) {
      chosenName = nm; // remember in memory first, so a storage failure can't lose it
      try { localStorage.setItem("nfs_cname", nm); } catch (e) {}
      sendHello(); // re-announce with the chosen name (clientIdentity prefers chosenName)
    }
  }
  try { localStorage.setItem("nfs_named", "1"); } catch (e) {} // asked once; don't nag
  els.namemodal.style.display = "none";
}
if (els.namesave) els.namesave.addEventListener("click", () => closeNameModal(true));
if (els.nameskip) els.nameskip.addEventListener("click", () => closeNameModal(false));
if (els.nameinput)
  els.nameinput.addEventListener("keydown", (e) => {
    if (e.key === "Enter") { e.preventDefault(); closeNameModal(true); }
    else if (e.key === "Escape") { e.preventDefault(); closeNameModal(false); }
  });

function connect() {
  teardownConnection(); // ensure a clean slate (no leaked timers/decoders)
  const proto = location.protocol === "https:" ? "wss" : "ws";
  try {
    ws = new WebSocket(`${proto}://${location.host}/ws`);
  } catch (e) {
    scheduleReconnect();
    return;
  }
  ws.binaryType = "arraybuffer";
  ws.onopen = () => {
    reconnectAttempts = 0;
    setState("syncing clock", "warn");
    startClockSync();
    sendHello(); // identify so the server can list us + restore our volume
    maybePromptName(); // first connect on this device → offer to name it
  };
  ws.onmessage = onMessage;
  ws.onclose = () => scheduleReconnect();
  ws.onerror = () => {
    /* close will follow */
  };

  statsTimer = setInterval(updateStats, 500);
}

function scheduleReconnect() {
  if (stopping) return;
  // tear down everything for this dead connection except the AudioContext
  teardownConnection();
  reconnectAttempts++;
  setState("reconnecting…", "warn");
  // capped exponential backoff with jitter
  const base = Math.min(15000, 500 * Math.pow(2, Math.min(reconnectAttempts, 5)));
  const delay = base * (0.7 + Math.random() * 0.6);
  clearTimeout(reconnectTimer);
  reconnectTimer = setTimeout(connect, delay);
}

// Drop all per-connection resources. Keeps `ac`/`gain`/`wakeLock` alive.
function teardownConnection() {
  // A reconnect resets the clock/anchor, which invalidates an in-flight calibration.
  calibAbort("Connection dropped — calibration cancelled.");
  clearInterval(keepaliveTimer);
  keepaliveTimer = null;
  clearInterval(statsTimer);
  statsTimer = null;
  if (ws) {
    ws.onopen = ws.onmessage = ws.onclose = ws.onerror = null;
    try {
      ws.close();
    } catch (e) {}
    ws = null;
  }
  flushVideo();
  try {
    if (audioDecoder && audioDecoder.state !== "closed") audioDecoder.close();
  } catch (e) {}
  try {
    if (videoDecoder && videoDecoder.state !== "closed") videoDecoder.close();
  } catch (e) {}
  audioDecoder = null;
  videoDecoder = null;
  gotParams = false;
  aPlayhead = null;
  firstPlayoutAc = null;
  offsetNs = null;
  offsets = [];
  pending = [];
}

function flushVideo() {
  for (const item of vq) {
    try {
      item.frame.close();
    } catch (e) {}
  }
  vq = [];
  evq = [];
  needDecodeKey = false;
}

// =============================================================================
// Clock sync (NTP-style against the server's monotonic clock)
// =============================================================================
function sendClockReq() {
  if (!ws || ws.readyState !== 1) return;
  pending.push(performance.now());
  if (pending.length > 32) pending.shift();
  try {
    ws.send(new Uint8Array([MSG_CLOCK_REQ]));
  } catch (e) {}
}

function startClockSync() {
  for (let i = 0; i < 6; i++) setTimeout(sendClockReq, i * 30); // burst
  clearInterval(keepaliveTimer);
  keepaliveTimer = setInterval(sendClockReq, 2000); // keepalive + drift correction
}

function serverPtsToPerfMs(ptsNs) {
  // ptsNs is server-mono ns; offsetNs maps it to performance.now() ms, then we add
  // the shared buffer (same on every client → same wall-clock instant) plus this
  // device's effective sync trim (local + server-pushed, to align speakers with
  // different output latencies).
  return (ptsNs - offsetNs) / 1e6 + bufferMs + effTrimMs();
}

// =============================================================================
// Messages
// =============================================================================
function onMessage(ev) {
  if (typeof ev.data === "string") {
    onConfig(ev.data);
    return;
  }
  const dv = new DataView(ev.data);
  const type = dv.getUint8(0);

  if (type === MSG_CLOCK_RSP) {
    const t1 = pending.shift();
    const t4 = performance.now();
    if (t1 === undefined) return;
    const serverNs = Number(dv.getBigInt64(1, false));
    const midPerfNs = ((t1 + t4) / 2) * 1e6;
    offsets.push(serverNs - midPerfNs);
    if (offsets.length > 15) offsets.shift();
    const sorted = [...offsets].sort((a, b) => a - b);
    offsetNs = sorted[Math.floor(sorted.length / 2)];
    return; // updateStats decides "buffering …" vs "playing"
  }

  if (type === MSG_SET_VOLUME) {
    // Server-controlled (remote) volume: multiplies this device's own slider.
    const v = dv.getFloat32(1, true); // f32 LE
    if (Number.isFinite(v)) {
      remoteVol = Math.min(1, Math.max(0, v));
      applyGain();
    }
    return;
  }

  if (type === MSG_SET_TRIM) {
    // Server-controlled sync offset (ms): adds to this device's own trim.
    const ms = dv.getInt32(1, true); // i32 LE
    if (Number.isFinite(ms)) {
      const next = Math.max(-5000, Math.min(5000, ms));
      if (next !== remoteTrimMs) {
        remoteTrimMs = next;
        aPlayhead = null; // re-anchor audio so the new offset takes effect immediately
        flushVideo(); // re-time the video queue too (mirror setTrim, so A/V stay aligned)
        reportSync(); // report the new effective total back so the GUI reflects it
      }
    }
    return;
  }

  if (type === MSG_CALIB_CTRL) {
    // Server-orchestrated calibration (Phase B). Sub-type 1 = ROLE.
    if (dv.getUint8(1) === 1 && ev.data.byteLength >= 12) {
      const role = dv.getUint8(2);
      const refSeed = dv.getUint32(3, true);
      const selfSeed = dv.getUint32(7, true);
      const slot = dv.getUint8(11);
      calibOnRole(role, refSeed, selfSeed, slot);
    }
    return;
  }

  if (type === MSG_AUDIO) {
    if (!audioDecoder || offsetNs === null) return;
    if (audioDecoder.decodeQueueSize > 200) {
      // Device fell behind: shed this frame. Re-anchor so the next decoded frame snaps back
      // to its true PTS instead of butting against the playhead and drifting permanently
      // ahead of the other clients (a silent desync is worse than one brief gap).
      aPlayhead = null;
      return;
    }
    const ptsNs = dv.getBigInt64(1, false);
    try {
      audioDecoder.decode(
        new EncodedAudioChunk({ type: "key", timestamp: Number(ptsNs / 1000n), data: new Uint8Array(ev.data, 9) })
      );
    } catch (e) {}
    return;
  }

  if (type === MSG_VIDEO) {
    if (offsetNs === null) return;
    if (document.hidden) return; // don't buffer video the (paused) rAF can't drain
    const tsUs = Number(dv.getBigInt64(1, false) / 1000n); // exact micros, < 2^53
    const key = (dv.getUint8(9) & 1) !== 0;
    const data = new Uint8Array(ev.data, 10);
    onVideoChunk(tsUs, key, data);
    return;
  }
}

function onConfig(text) {
  let c;
  try {
    c = JSON.parse(text);
  } catch (e) {
    return;
  }
  cfg = c;
  els.srv.textContent = c.name ? "· " + c.name : "";
  // Honor the full server buffer (up to 10 s). It's cheap now: video is buffered as
  // ENCODED chunks and only decoded just-in-time, so a deep buffer no longer means
  // a wall of decoded surfaces.
  bufferMs = Math.min(Math.max(c.bufferMs || 3000, 200), 15000);
  const fps = c.frameRate || 30;
  // Encoded queue must hold the whole buffer's worth of frames (+ headroom).
  maxEvq = Math.ceil((fps * bufferMs) / 1000) + 90;

  if ("mediaSession" in navigator) {
    try {
      navigator.mediaSession.metadata = new MediaMetadata({
        title: c.name || "Newfoundsync",
        artist: "LAN stream",
      });
      navigator.mediaSession.playbackState = "playing";
    } catch (e) {}
  }

  setupDecoders(c).catch((e) => showWarn("⚠ Decoder setup failed: " + e.message));
}

async function setupDecoders(c) {
  // ---- audio ----
  const acfg = {
    codec: c.audioCodec || "opus",
    sampleRate: c.sampleRate || 48000,
    numberOfChannels: c.channels || 2,
  };
  if (window.AudioDecoder.isConfigSupported) {
    const r = await AudioDecoder.isConfigSupported(acfg).catch(() => null);
    if (r && !r.supported) {
      showWarn("⚠ This device can't decode the audio codec (" + acfg.codec + ").");
      return;
    }
  }
  if (audioDecoder) return; // already set up on this connection
  audioDecoder = new AudioDecoder({ output: onAudioData, error: (e) => onDecErr("audio", e) });
  audioDecoder.configure(acfg);

  // ---- video: coarse support probe only; real configure waits for the keyframe ----
  if (c.video && window.VideoDecoder.isConfigSupported) {
    const probe = { codec: c.videoCodec || "hev1.1.6.L153.B0", optimizeForLatency: true };
    const r = await VideoDecoder.isConfigSupported(probe).catch(() => null);
    if (r && !r.supported) {
      showWarn("⚠ This device can't decode the video codec (HEVC/H.265). Audio will still play.");
    } else {
      els.fsbtn.style.display = "flex";
    }
  } else if (!c.video) {
    // Audio-only source: hide the video box, show the (boxless) logo branding instead.
    els.stage.style.display = "none";
    els.vlogo.style.display = "flex";
    vizStart();
    els.fsbtn.style.display = "none";
  }
}

let vDecErrStreak = 0;
let vGoodRun = 0; // consecutive good frames since the last error (gates clearing the streak)
function onDecErr(kind, e) {
  console.error(kind + " decode error", e);
  if (kind === "video") {
    // Most video decode errors are recoverable: drop state and re-wait for a keyframe.
    gotParams = false;
    flushVideo();
    vGoodRun = 0; // a fresh error breaks any in-progress "recovered" run
    vDecErrStreak++;
    if (videoAccel !== "prefer-software" && vDecErrStreak >= 2) {
      // The chosen (likely hardware) decoder keeps failing — common on desktop Chrome
      // without usable hardware H.264. Force software decode on the next reconfigure.
      videoAccel = "prefer-software";
      vDecErrStreak = 0;
      console.warn("video: falling back to software decode");
    } else if (vDecErrStreak >= 4) {
      showWarn(
        "⚠ Video keeps failing to decode on this device — it may not support this " +
          "resolution/codec. Try a lower resolution on the server. Audio is unaffected."
      );
    }
  }
}

// =============================================================================
// Video: configure from first keyframe's SPS/PPS; buffer ENCODED chunks and
// decode them just-in-time (so a 10 s buffer stays cheap, not 10 s of surfaces).
// =============================================================================
function onVideoChunk(tsUs, key, annexb) {
  if (!gotParams) {
    if (!key) return; // wait for a keyframe (HEVC IRAP carries VPS/SPS/PPS in-band)
    // HEVC straight from Annex-B: configure WITHOUT a description, so the decoder reads
    // VPS/SPS/PPS from the in-band keyframe and we can feed raw Annex-B access units.
    const vcfg = {
      codec: (cfg && cfg.videoCodec) || "hev1.1.6.L153.B0",
      optimizeForLatency: true,
      hardwareAcceleration: videoAccel,
    };
    try {
      if (videoDecoder && videoDecoder.state !== "closed") videoDecoder.close();
      videoDecoder = new VideoDecoder({ output: onVideoFrame, error: (e) => onDecErr("video", e) });
      videoDecoder.configure(vcfg);
      gotParams = true;
    } catch (e) {
      try {
        delete vcfg.hardwareAcceleration;
        videoDecoder = new VideoDecoder({ output: onVideoFrame, error: (e2) => onDecErr("video", e2) });
        videoDecoder.configure(vcfg);
        gotParams = true;
      } catch (e2) {
        showWarn("⚠ Video decoder couldn't start — this device may not support HEVC/H.265. Audio still plays. (" + e2.message + ")");
        return;
      }
    }
  }
  // Feed Annex-B directly (decoder configured without a description = Annex-B mode).
  evq.push({ key, tsUs, data: annexb });
  // Overflow = we're receiving faster than we can decode/play. Drop the oldest and
  // force the decoder to resync at the next keyframe (don't break the delta chain).
  if (evq.length > maxEvq) {
    evq.splice(0, evq.length - maxEvq);
    needDecodeKey = true;
  }
}

// Feed the decoder in order, but only frames whose playout is near (DECODE_AHEAD),
// and only while the decoded render queue + decoder backlog have room.
function pumpVideo() {
  // WebCodecs decoder state is "unconfigured" | "configured" | "closed" (no "running").
  if (!videoDecoder || videoDecoder.state !== "configured") return;
  const now = performance.now();
  while (evq.length && vq.length < MAX_DECODED && videoDecoder.decodeQueueSize < 8) {
    const head = evq[0];
    if (serverPtsToPerfMs(head.tsUs * 1000) > now + DECODE_AHEAD_MS) break;
    evq.shift();
    if (needDecodeKey) {
      if (!head.key) continue; // skip stale deltas until a keyframe re-anchors decode
      needDecodeKey = false;
    }
    try {
      videoDecoder.decode(
        new EncodedVideoChunk({ type: head.key ? "key" : "delta", timestamp: head.tsUs, data: head.data })
      );
    } catch (e) {
      onDecErr("video", e);
      break;
    }
  }
}

function onVideoFrame(frame) {
  vFrames++;
  // Only a SUSTAINED run of good frames clears the failure streak. A single good frame
  // between errors must NOT reset it, or an intermittently-failing hardware decoder
  // (fail, ok, fail, ok…) would never reach the threshold that escalates to software.
  if (vDecErrStreak > 0 && ++vGoodRun >= 30) {
    vDecErrStreak = 0;
    vGoodRun = 0;
  }
  if (offsetNs === null || document.hidden) {
    frame.close();
    return;
  }
  vq.push({ frame, targetPerf: serverPtsToPerfMs(frame.timestamp * 1000) });
  while (vq.length > MAX_DECODED) vq.shift().frame.close();
  const dims = frame.displayWidth + "×" + frame.displayHeight;
  if (dims !== vDims) vDims = dims; // avoid per-frame string churn in stats
}

// One pass: feed the decoder just ahead of playout, then draw whatever is due.
function videoStep() {
  pumpVideo();
  const now = performance.now();
  let due = null;
  while (vq.length && vq[0].targetPerf <= now) {
    if (due) due.close();
    due = vq.shift().frame;
  }
  if (due) {
    if (els.canvas.width !== due.displayWidth) {
      els.canvas.width = due.displayWidth;
      els.canvas.height = due.displayHeight;
    }
    if (els.vlogo.style.display !== "none") { els.vlogo.style.display = "none"; vizStop(); } // real video → swap logo for the stage
    els.stage.style.display = "block";
    ctx2d.drawImage(due, 0, 0);
    due.close();
  }
}

let rafPending = false;
function drawLoop() {
  rafPending = false;
  videoStep();
  scheduleDraw();
}
function scheduleDraw() {
  if (rafPending || document.hidden) return;
  rafPending = true;
  requestAnimationFrame(drawLoop);
}
scheduleDraw();
// Backstop: if rAF ever stalls (some embedded/background webviews throttle or never
// fire it), keep video moving from a timer too. rAF stays the smooth 60 fps path.
setInterval(() => {
  if (!document.hidden && videoDecoder) videoStep();
}, 120);

// =============================================================================
// Audio: decode -> Web Audio. Gapless scheduler — frames are queued back-to-back
// at a running "playhead" so there are NO per-frame clock-jitter seams. The synced
// clock sets the initial anchor; small drift is corrected by gently nudging the playback
// rate (≤±1%, inaudible), and we only hard re-anchor on a real discontinuity (resume /
// gap / trim change). This is what makes a deep buffer play smoothly instead of garbled.
// =============================================================================
function onAudioData(ad) {
  aFrames++;
  if (!ac || ac.state !== "running" || offsetNs === null) {
    ad.close();
    return;
  }
  const dur = ad.numberOfFrames / ad.sampleRate; // seconds this frame occupies
  const targetAc = ac.currentTime + (serverPtsToPerfMs(ad.timestamp * 1000) - performance.now()) / 1000;

  // Drift correction (VLC-style): hard re-anchor ONLY on the first frame or a real
  // discontinuity (resume / gap / trim change); for normal small drift, nudge the playback
  // RATE a hair instead of jumping. A ±1% rate change is essentially inaudible and pulls the
  // playhead back onto the synced clock over a few seconds — no click.
  const A_DEADBAND = 0.008; // ±8 ms: within this, don't correct (avoid hunting)
  const A_HARD_RESET = 0.4; // beyond this it's a discontinuity, not drift → jump
  const A_MAX_ADJ = 0.01; //  cap the rate change at ±1%
  const A_DRIFT_K = 0.1; //   reach the cap at ~100 ms of drift
  if (aPlayhead === null || Math.abs(aPlayhead - targetAc) > A_HARD_RESET) {
    aPlayhead = Math.max(targetAc, ac.currentTime + 0.03);
  }
  // Never schedule in the past (would drop or pile up at "now" → clicks).
  if (aPlayhead < ac.currentTime + 0.005) {
    aPlayhead = ac.currentTime + 0.01;
  }
  // drift > 0 ⇒ we're scheduling later than the clock wants (behind) ⇒ speed up slightly;
  // drift < 0 ⇒ ahead ⇒ slow down slightly. Converges drift → 0 without a jump.
  let rate = 1.0;
  const drift = aPlayhead - targetAc;
  if (Math.abs(drift) > A_DEADBAND) {
    rate = Math.min(1 + A_MAX_ADJ, Math.max(1 - A_MAX_ADJ, 1 + A_DRIFT_K * drift));
  }

  const ch = ad.numberOfChannels;
  const frames = ad.numberOfFrames;
  if (!f32pool || f32pool.length < frames) f32pool = new Float32Array(frames);
  const buf = ac.createBuffer(ch, frames, ad.sampleRate); // carries its own rate; graph resamples
  for (let c = 0; c < ch; c++) {
    ad.copyTo(f32pool.subarray(0, frames), { planeIndex: c, format: "f32-planar" });
    buf.copyToChannel(f32pool.subarray(0, frames), c);
  }
  ad.close();
  if (firstPlayoutAc === null) firstPlayoutAc = aPlayhead; // for the "buffering" countdown
  const src = ac.createBufferSource();
  src.buffer = buf;
  if (rate !== 1.0) src.playbackRate.value = rate; // gentle catch-up/slow-down
  src.connect(gain);
  src.start(aPlayhead);
  aPlayhead += dur / rate; // advance by the ACTUAL playout time (rate changes it)
}

// =============================================================================
// Mobile lifecycle: visibility, wake lock, fullscreen
// =============================================================================
function onVisibility() {
  if (document.visibilityState === "visible") {
    if (ac && ac.state !== "running") ac.resume().catch(() => {});
    requestWakeLock();
    // After a background interval the clock and queue are stale: re-anchor.
    flushVideo();
    gotParams = false; // re-wait for the next keyframe
    aPlayhead = null; // re-anchor audio
    firstPlayoutAc = null;
    offsets = [];
    offsetNs = null;
    if (ws && ws.readyState <= 1) {
      // OPEN → re-anchor the clock; CONNECTING → its onopen will sync. Either way don't
      // open a second socket on top of a live/in-flight one.
      if (ws.readyState === 1) startClockSync();
    } else {
      // Socket dead/closing while backgrounded → reconnect now, and cancel any pending
      // backoff timer so it doesn't fire a duplicate connect() right after this one.
      clearTimeout(reconnectTimer);
      reconnectTimer = null;
      connect();
    }
    scheduleDraw();
  } else {
    flushVideo(); // stop holding decoded surfaces while hidden
  }
}

async function requestWakeLock() {
  try {
    if ("wakeLock" in navigator && document.visibilityState === "visible") {
      wakeLock = await navigator.wakeLock.request("screen");
      wakeLock.addEventListener("release", () => {
        wakeLock = null;
      });
    }
  } catch (e) {
    /* not granted / not supported — fine */
  }
}

function toggleFullscreen() {
  const fsEl = document.fullscreenElement || document.webkitFullscreenElement;
  if (fsEl || document.body.classList.contains("immersive")) {
    if (document.exitFullscreen) document.exitFullscreen().catch(() => {});
    else if (document.webkitExitFullscreen) document.webkitExitFullscreen();
    document.body.classList.remove("immersive");
    return;
  }
  const el = els.stage;
  if (el.requestFullscreen) {
    el.requestFullscreen().catch(() => document.body.classList.add("immersive"));
  } else if (el.webkitRequestFullscreen) {
    el.webkitRequestFullscreen();
  } else {
    // iPhone: Fullscreen API is unavailable on non-<video> elements — go immersive.
    document.body.classList.add("immersive");
  }
}

// =============================================================================
// Stats
// =============================================================================
// Drive the buffering loading bar. indeterminate=null → hide.
// The determinate fill is animated per-frame (requestAnimationFrame) off the deterministic
// countdown so it glides smoothly instead of stepping every stats tick.
let bufRaf = 0;
function bufAnimStop() {
  if (bufRaf) { cancelAnimationFrame(bufRaf); bufRaf = 0; }
}
function bufAnimTick() {
  bufRaf = 0;
  if (!started || ac == null || offsetNs === null || firstPlayoutAc === null) return;
  const remain = firstPlayoutAc - ac.currentTime;
  const total = Math.max(0.2, bufferMs / 1000);
  const pct = Math.max(0, Math.min(100, (1 - remain / total) * 100));
  els.bufbarfill.style.width = pct.toFixed(2) + "%";
  els.buftext.textContent = "Buffering… " + Math.max(0, remain).toFixed(1) + "s";
  if (remain > 0.02) bufRaf = requestAnimationFrame(bufAnimTick); // keep gliding until full
}
function showBuffering(indeterminate, pct, text) {
  if (indeterminate === null) {
    bufAnimStop();
    els.buffering.style.display = "none";
    return;
  }
  els.buffering.style.display = "flex";
  if (text != null) els.buftext.textContent = text;
  if (indeterminate) {
    bufAnimStop();
    els.bufbar.classList.add("indeterminate");
  } else {
    els.bufbar.classList.remove("indeterminate");
    if (!bufRaf) bufRaf = requestAnimationFrame(bufAnimTick); // rAF drives width + text
  }
}

function updateStats() {
  if (offsetNs !== null && ac) {
    if (firstPlayoutAc !== null && ac.currentTime < firstPlayoutAc - 0.05) {
      const remain = firstPlayoutAc - ac.currentTime;
      // Only show the bar on the FIRST connect; stay hidden for re-buffers after a tab
      // re-focus / reconnect (re-anchor) once we've played at least once this session.
      showBuffering(everPlayed ? null : false);
      setState("buffering " + Math.max(0, Math.ceil(remain)) + "s", "warn");
    } else {
      everPlayed = true;
      showBuffering(null);
      setState("playing", "ok");
    }
  } else if (started) {
    showBuffering(everPlayed ? null : true, 0, offsetNs === null ? "Connecting…" : "Buffering…");
  }
  els.sync.textContent = offsetNs === null ? "…" : "✔ ±" + syncJitterMs().toFixed(1) + "ms";
  const vbuf = cfg && cfg.video ? " · vid enc " + evq.length + "/dec " + vq.length : "";
  els.buf.textContent = (bufferMs / 1000).toFixed(1) + "s" + vbuf;
  els.ai.textContent = cfg ? aFrames + " frames" : "—";
  els.vi.textContent = vDims ? vDims + " · " + vFrames + " frames" : "—";
}

function syncJitterMs() {
  if (offsets.length < 2) return 0;
  const mn = Math.min(...offsets),
    mx = Math.max(...offsets);
  return (mx - mn) / 1e6 / 2;
}

// Diagnostics hook (for support/QA): window.nfsDebug() reports the live playout lead.
window.nfsDebug = function () {
  return {
    audioLeadSec: aPlayhead !== null && ac ? +(aPlayhead - ac.currentTime).toFixed(3) : null,
    bufferMs,
    trimMs,
    firstPlayoutInSec: firstPlayoutAc !== null && ac ? +(firstPlayoutAc - ac.currentTime).toFixed(3) : null,
    offsetSynced: offsetNs !== null,
    audioFrames: aFrames,
    evq: evq.length,
    vq: vq.length,
    acState: ac ? ac.state : null,
  };
};

// =============================================================================
// Acoustic auto-calibration (microphone) — 100% client-side, no server.
//
// Role-based, for speakers in the SAME ROOM. One device plays a distinctive dial-up
// tone on the shared-clock beat ("🔊 Play sync tone" / reference). Another device
// listens for that tone with its mic ("🎤 Listen & align" / follower) and nudges ITS
// OWN trim so the reference's sound lands when it plays the matching content.
//
// The shared clock is the coordinator: both devices key the tone to the same server-
// time ticks, so no server relay is needed. The follower matched-filters the known
// dial-up pattern out of the room (robust — it never has to disentangle itself from
// the music, and it never listens to its own tone). It can't measure its OWN output
// lag (it doesn't hear itself), so it subtracts what the browser reports
// (AudioContext.outputLatency + mic input latency); whatever isn't reported — notably
// Bluetooth codec delay — remains as residual, so fine-tune by ear on wireless gear.
// =============================================================================
const CALCFG = {
  TICK_S: 1.5, // the reference emits the tone every TICK_S on the shared clock
  WINDOW_TICKS: 5, // ticks observed per measurement (more hits → lower run-to-run variance)
  TONE_AMP: 0.45, // tone level (straight to output, bypasses volume/mute)
  // A clean upward sine sweep ("whoop"). A chirp has near-ideal autocorrelation (pulse
  // compression) → a sharp, unambiguous matched-filter peak, and it sounds smooth rather
  // than choppy. The reference plays it; the follower matched-filters an identical copy.
  CHIRP_MS: 420, // sweep duration
  CHIRP_F0: 700, // sweep start (Hz)
  CHIRP_F1: 4000, // sweep end (Hz) — below the 16 kHz analysis Nyquist
  // Half-width of the per-tick tone-search window, around our own playout beat. Kept
  // STRICTLY below TICK_S/2 so a neighbouring tick's tone can never alias into this
  // tick's window (which would lock us a whole TICK_S off). The catch: a true offset
  // larger than this can't be auto-detected — get within ~0.7s by ear first.
  SEARCH_HALF_S: 0.7,
  MIN_SCORE: 0.1, // matched-filter detection threshold (0..1) — lenient; tick window + median reject false hits
  MIN_DETECTIONS: 3, // need at least this many tone hits to trust a measurement (more = lower variance)
  STEP_MS: 1, // converge when the residual is within this many ms (trim grid is now 0.1 ms)
  MAX_ITERS: 4, // measure→correct cycles (no buffer-drain wait between them)
  // Partial GCC-PHAT exponent for the matched filter's TIMING peak (0 = plain matched filter,
  // 1 = full PHAT). Whitening the cross-spectrum sharpens the peak and locks the direct-path
  // arrival in reverberant rooms; partial (~0.7) keeps detection robust in lower-SNR conditions.
  PHAT_BETA: 0.7,

  // ---- Coded continuous signal (Phase A) — now the DEFAULT. The chirp above remains a
  // fallback: window.setCalibCoded(false) reverts this device (persists in localStorage).
  // A looping band-limited pseudonoise code converges faster (it's "on" continuously, so a
  // short capture sees several periods to average) and packs more data into the tone. Still
  // wants on-device tuning (level, band, period, audibility); flip back to the chirp if a room
  // gives it trouble. (Phase B "Calibrate all" forces the coded path regardless of this flag.)
  CODED: true,
  CODE_SEED: 0x9e3779b9, // PRNG seed — reference and follower MUST share it
  CODE_N: 16383, // code length @16 kHz → period ≈ 1.024 s. The follower windows around its OWN
  // measured loopback (not the idealized beat), so the detectable RESIDUAL (ref−follower latency
  // mismatch) is ±~0.46 s — robust to a high-latency speaker without needing a longer period.
  CODE_F0: 1200, // band low (Hz)
  CODE_F1: 6000, // band high (Hz)
  CODE_AMP: 0.32, // playback level (straight to output, bypasses volume/mute)
  CODE_PERIODS: 4, // periods recorded per measurement (more → more peaks to median, lower variance)
  CODE_MIN_DETECTIONS: 2, // coded peaks are continuous + reliable → accept a measurement at 2 hits
  CODE_MAX_ITERS: 2, // converges fast → far fewer measure→correct cycles than the chirp path
};
// Coded is the default; an explicit per-device choice (sticky in localStorage) overrides it.
// Absent ⇒ keep the default (true); only a stored "0" reverts this device to the chirp.
try {
  const v = localStorage.getItem("nfs_calib_coded");
  if (v !== null) CALCFG.CODED = v === "1";
} catch (e) {}
window.setCalibCoded = (on) => {
  CALCFG.CODED = !!on;
  try { localStorage.setItem("nfs_calib_coded", on ? "1" : "0"); } catch (e) {}
  return CALCFG.CODED;
};

function setCalibStatus(text) {
  if (!els.calibstatus) return;
  if (!text) {
    els.calibstatus.style.display = "none";
    els.calibstatus.textContent = "";
    return;
  }
  els.calibstatus.textContent = text;
  els.calibstatus.style.display = "";
}

function calibSleep(ms) {
  return new Promise((r) => setTimeout(r, ms));
}

// Build the chirp ("whoop") as an AudioBuffer at the context's sample rate (reference role).
function makeToneBuffer(rate, f0, f1) {
  const n = Math.round((CALCFG.CHIRP_MS / 1000) * rate);
  const b = ac.createBuffer(1, n, rate);
  fillChirp(b.getChannelData(0), rate, CALCFG.TONE_AMP, f0, f1);
  return b;
}

// Linear sine sweep f0→f1 with a raised-cosine fade at each end (click-free, smooth).
// f0<f1 is an up-sweep (reference); f0>f1 is a down-sweep (the follower's self-test).
function fillChirp(out, rate, amp, f0, f1) {
  const n = out.length;
  const T = n / rate, k = (f1 - f0) / T;
  const fade = Math.max(1, Math.round(0.08 * n));
  for (let i = 0; i < n; i++) {
    const t = i / rate;
    let w = 1;
    if (i < fade) w = 0.5 - 0.5 * Math.cos((Math.PI * i) / fade);
    else if (i >= n - fade) w = 0.5 - 0.5 * Math.cos((Math.PI * (n - 1 - i)) / fade);
    out[i] = amp * w * Math.sin(2 * Math.PI * (f0 * t + 0.5 * k * t * t));
  }
}

// ---- Coded continuous signal (Phase A) -----------------------------------------------
// A band-limited pseudonoise "code": a deterministic ±1 PRNG sequence band-passed to a
// pleasant mid-band, taken over exactly one period. It's broadband and noise-like → a sharp,
// REAL autocorrelation peak (so it reuses the existing real matched-filter + PHAT, unlike a
// BPSK code whose peak would rotate with the unknown channel carrier phase). Played looped
// and clock-aligned by the reference; the follower matched-filters one identical period.
//
// These four helpers are the SINGLE source of truth: they run on the main thread (to build the
// reference's AudioBuffer) AND are injected verbatim into the DSP worker (to build the template),
// via `.toString()` below — so the played signal and the template are guaranteed identical.
function calCodePrng(seed, n) {
  // mulberry32 → ±1 chips. Deterministic: same seed ⇒ same sequence on both devices.
  let a = seed >>> 0;
  const out = new Float32Array(n);
  for (let i = 0; i < n; i++) {
    a = (a + 0x6d2b79f5) | 0;
    let t = Math.imul(a ^ (a >>> 15), 1 | a);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    const u = ((t ^ (t >>> 14)) >>> 0) / 4294967296;
    out[i] = u < 0.5 ? -1 : 1;
  }
  return out;
}
function calBiquad(x, c) {
  // Direct-form-I biquad; c = [b0,b1,b2,a1,a2] (a0 normalized to 1).
  const b0 = c[0], b1 = c[1], b2 = c[2], a1 = c[3], a2 = c[4];
  const y = new Float32Array(x.length);
  let x1 = 0, x2 = 0, y1 = 0, y2 = 0;
  for (let i = 0; i < x.length; i++) {
    const xi = x[i];
    const yi = b0 * xi + b1 * x1 + b2 * x2 - a1 * y1 - a2 * y2;
    x2 = x1; x1 = xi; y2 = y1; y1 = yi;
    y[i] = yi;
  }
  return y;
}
function calBiquadLP(rate, fc) {
  const w = (2 * Math.PI * fc) / rate, cs = Math.cos(w), sn = Math.sin(w);
  const alpha = sn / (2 * Math.SQRT1_2), a0 = 1 + alpha;
  return [(1 - cs) / 2 / a0, (1 - cs) / a0, (1 - cs) / 2 / a0, (-2 * cs) / a0, (1 - alpha) / a0];
}
function calBiquadHP(rate, fc) {
  const w = (2 * Math.PI * fc) / rate, cs = Math.cos(w), sn = Math.sin(w);
  const alpha = sn / (2 * Math.SQRT1_2), a0 = 1 + alpha;
  return [(1 + cs) / 2 / a0, -(1 + cs) / a0, (1 + cs) / 2 / a0, (-2 * cs) / a0, (1 - alpha) / a0];
}
// One steady-state period of the band-limited code at `rate`, peak-normalized to 1. Filtering
// the doubled sequence and taking the SECOND copy removes the filter's start transient, so the
// result is genuinely periodic (correlating a window that straddles a period boundary still locks).
function calBuildCode(seed, n, rate, f0, f1) {
  const raw = calCodePrng(seed, n);
  const dbl = new Float32Array(2 * n);
  dbl.set(raw, 0);
  dbl.set(raw, n);
  let y = calBiquad(dbl, calBiquadHP(rate, f0));
  y = calBiquad(y, calBiquadLP(rate, f1));
  const out = new Float32Array(n);
  let peak = 1e-9;
  for (let i = 0; i < n; i++) {
    const v = y[n + i];
    out[i] = v;
    const a = v < 0 ? -v : v;
    if (a > peak) peak = a;
  }
  const g = 1 / peak;
  for (let i = 0; i < n; i++) out[i] *= g;
  return out;
}
// General linear resample (up or down) — used to render the 16 kHz canonical code to the
// reference's AudioContext rate for playback. (The worker keeps the 16 kHz canonical as-is.)
function calResample(x, inRate, outRate) {
  if (inRate === outRate) return x;
  const ratio = inRate / outRate;
  const n = Math.max(1, Math.round(x.length / ratio));
  const out = new Float32Array(n);
  for (let i = 0; i < n; i++) {
    const pos = i * ratio, i0 = pos | 0, frac = pos - i0;
    const a = x[i0] || 0, b = (i0 + 1 < x.length ? x[i0 + 1] : x[i0]) || 0;
    out[i] = a + (b - a) * frac;
  }
  return out;
}
// Build the reference's playback AudioBuffer: the 16 kHz canonical code, resampled to the
// context rate and scaled. (The follower correlates the mic — resampled back to 16 kHz —
// against the same 16 kHz canonical, so both sides share one definition of the signal.)
function makeCodeBuffer(rate) {
  const seed = calib.refSeed || CALCFG.CODE_SEED; // orchestrated uses the server-assigned seed
  const canon = calBuildCode(seed, CALCFG.CODE_N, 16000, CALCFG.CODE_F0, CALCFG.CODE_F1);
  const up = calResample(canon, 16000, rate);
  const b = ac.createBuffer(1, up.length, rate);
  const ch = b.getChannelData(0);
  for (let i = 0; i < up.length; i++) ch[i] = CALCFG.CODE_AMP * up[i];
  return b;
}

// Mic capture worklet (runs on the audio render thread). Batches ~4096 samples and
// posts them with the ac-frame index of the batch's first sample, so the main thread
// can place mic samples on the same ac clock as the reference.
const MIC_WORKLET_SRC = `
class MicTap extends AudioWorkletProcessor {
  constructor() { super(); this.buf = new Float32Array(4096); this.n = 0; this.start = -1; }
  flush() {
    if (this.n === 0) return;
    const out = this.buf.slice(0, this.n);
    this.port.postMessage({ frame: this.start, samples: out }, [out.buffer]);
    this.n = 0; this.start = -1;
  }
  process(inputs) {
    const inp = inputs[0];
    const x = inp && inp[0];
    if (x && x.length) {
      // A non-contiguous quantum (dropped/glitched render block) closes the current batch
      // so each posted batch is internally gapless; the main thread sees the frame jump.
      if (this.n > 0 && currentFrame !== this.start + this.n) this.flush();
      if (this.start < 0) this.start = currentFrame;
      this.buf.set(x, this.n); this.n += x.length;
      if (this.n >= this.buf.length - 128) this.flush();
    }
    return true;
  }
}
registerProcessor("mic-tap", MicTap);
`;

// DSP worker: resample the mic to 16 kHz and matched-filter the known dial-up template
// across it, returning every strong tone hit (ac-time + score). Off the main thread so
// the gapless audio scheduler never stalls.
// Inject the code-generation helpers (defined once, above) into the worker verbatim, so the
// follower's template is byte-identical to what the reference plays. Single source of truth.
const CALCODE_SRC = [calCodePrng, calBiquad, calBiquadLP, calBiquadHP, calBuildCode]
  .map((f) => f.toString())
  .join("\n");
const DSP_WORKER_SRC = CALCODE_SRC + "\n" + `
const RS = 16000;
self.onmessage = (e) => {
  try { self.postMessage(analyze(e.data)); }
  catch (err) { self.postMessage({ error: String((err && err.message) || err) }); }
};

function nextPow2(n) { let p = 1; while (p < n) p <<= 1; return p; }

function resample(x, inRate, outRate) {
  if (inRate === outRate) return x;
  const ratio = inRate / outRate;
  const n = Math.floor(x.length / ratio);
  const out = new Float32Array(n);
  for (let i = 0; i < n; i++) {
    const pos = i * ratio, i0 = pos | 0, frac = pos - i0;
    const a = x[i0] || 0, b = x[i0 + 1] || 0;
    out[i] = a + (b - a) * frac;
  }
  return out;
}

// Rebuild the same chirp the reference plays (amplitude is irrelevant — match is
// energy-normalized; the fade/sweep shape must match makeToneBuffer/fillChirp exactly).
function makeChirp(c, rate) {
  const n = Math.round((c.ms / 1000) * rate);
  const out = new Float32Array(n);
  const T = n / rate, k = (c.f1 - c.f0) / T;
  const fade = Math.max(1, Math.round(0.08 * n));
  for (let i = 0; i < n; i++) {
    const t = i / rate;
    let w = 1;
    if (i < fade) w = 0.5 - 0.5 * Math.cos((Math.PI * i) / fade);
    else if (i >= n - fade) w = 0.5 - 0.5 * Math.cos((Math.PI * (n - 1 - i)) / fade);
    out[i] = w * Math.sin(2 * Math.PI * (c.f0 * t + 0.5 * k * t * t));
  }
  return out;
}

// In-place iterative radix-2 FFT (re/im Float64Array, length power of two).
function fft(re, im, inverse) {
  const n = re.length;
  for (let i = 1, j = 0; i < n; i++) {
    let bit = n >> 1;
    for (; j & bit; bit >>= 1) j ^= bit;
    j ^= bit;
    if (i < j) { const tr = re[i]; re[i] = re[j]; re[j] = tr; const ti = im[i]; im[i] = im[j]; im[j] = ti; }
  }
  for (let len = 2; len <= n; len <<= 1) {
    const ang = ((inverse ? 2 : -2) * Math.PI) / len;
    const wr = Math.cos(ang), wi = Math.sin(ang);
    for (let i = 0; i < n; i += len) {
      let cr = 1, ci = 0;
      const half = len >> 1;
      for (let m = 0; m < half; m++) {
        const a = i + m, b = a + half;
        const xr = re[b] * cr - im[b] * ci;
        const xi = re[b] * ci + im[b] * cr;
        re[b] = re[a] - xr; im[b] = im[a] - xi;
        re[a] += xr; im[a] += xi;
        const ncr = cr * wr - ci * wi; ci = cr * wi + ci * wr; cr = ncr;
      }
    }
  }
  if (inverse) for (let i = 0; i < n; i++) { re[i] /= n; im[i] /= n; }
}

// FFT-based normalized cross-correlation of the template across the mic, then pick peaks.
function analyze(p) {
  const mic = resample(p.mic, p.micRate, RS);
  // Template: one period of the coded signal (Phase A) or the chirp (default). Both at RS.
  const tmpl = p.code
    ? calBuildCode(p.code.seed, p.code.n, RS, p.code.f0, p.code.f1)
    : makeChirp(p.chirp, RS);
  const L = tmpl.length, M = mic.length;
  if (M < L + 1) return { peaks: [] };
  const N = nextPow2(M + L);
  const sr = new Float64Array(N), si = new Float64Array(N);
  const tr = new Float64Array(N), ti = new Float64Array(N);
  for (let i = 0; i < M; i++) sr[i] = mic[i];
  for (let j = 0; j < L; j++) tr[j] = tmpl[j];
  fft(sr, si, false); fft(tr, ti, false);
  // X = S * conj(T) (kept in sr,si for the normalized correlation). Also build a partial-PHAT
  // weighted copy X / (|X|^beta + eps) in tr,ti: whitening the cross-spectrum sharpens the peak
  // and pulls it onto the direct-path arrival in reverberant rooms. beta=0 → plain matched filter.
  const beta = p.phatBeta || 0;
  for (let i = 0; i < N; i++) {
    const xr = sr[i] * tr[i] + si[i] * ti[i];
    const xi = si[i] * tr[i] - sr[i] * ti[i];
    sr[i] = xr; si[i] = xi;
    if (beta > 0) {
      const mag = Math.pow(Math.sqrt(xr * xr + xi * xi) + 1e-12, beta);
      tr[i] = xr / mag; ti[i] = xi / mag;
    } else {
      tr[i] = xr; ti[i] = xi;
    }
  }
  fft(sr, si, true); // → normalized-correlation domain (detection + score)
  fft(tr, ti, true); // → PHAT-weighted correlation domain (fine, reverberation-robust timing)
  let te = 0; for (let j = 0; j < L; j++) te += tmpl[j] * tmpl[j];
  const pe = new Float64Array(M + 1);
  for (let i = 0; i < M; i++) pe[i + 1] = pe[i] + mic[i] * mic[i];
  const lim = M - L;
  const corr = new Float32Array(lim + 1);
  let top = 0; // best correlation anywhere (for diagnostics, even below threshold)
  for (let k = 0; k <= lim; k++) {
    const se = pe[k + L] - pe[k];
    const c = sr[k] / Math.sqrt(te * se + 1e-12); // normalized 0..~1
    corr[k] = c;
    if (c > top) top = c;
  }
  // Sub-sample lag for a detected peak: snap to the PHAT correlation's local max within ±2
  // samples of the (normalized) peak, then parabolic-interpolate between samples → fractional lag.
  const phatLag = (bk) => {
    let bp = bk;
    const lo = Math.max(1, bk - 2), hi = Math.min(lim - 1, bk + 2);
    for (let j = lo; j <= hi; j++) if (tr[j] > tr[bp]) bp = j;
    if (bp < 1 || bp > lim - 1) return bp;
    const ym = tr[bp - 1], y0 = tr[bp], yp = tr[bp + 1];
    const denom = ym - 2 * y0 + yp;
    let d = denom !== 0 ? (0.5 * (ym - yp)) / denom : 0;
    if (d > 0.5) d = 0.5; else if (d < -0.5) d = -0.5;
    return bp + d;
  };
  const minScore = p.minScore, minSep = Math.max(1, Math.round(p.minSepSec * RS));
  const peaks = [];
  let k = 0;
  while (k <= lim) {
    if (corr[k] >= minScore) {
      const end = Math.min(lim, k + minSep);
      let bk = k, bv = corr[k];
      for (let j = k; j <= end; j++) { if (corr[j] > bv) { bv = corr[j]; bk = j; } }
      peaks.push({ time: p.micT0 + phatLag(bk) / RS, score: bv }); // sub-sample ac-time of the tone
      k = bk + minSep;
    } else k++;
  }
  const rms = Math.sqrt(pe[M] / Math.max(1, M)); // overall mic level (0 = silence)
  return { peaks, top, rms };
}
`;

function onMicBlock(e) {
  if (!calib.active || !calib.micChunks) return;
  const d = e.data;
  if (calib.micT0 === null) {
    calib.micT0 = d.frame / ac.sampleRate;
    calib.micNextFrame = d.frame;
  }
  // Keep the flat buffer frame-accurate so sample index ↔ ac-frame holds: if a batch
  // starts later than the previous one ended (the render thread dropped/idled a quantum),
  // pad the gap with silence. Never discard — silence in the buffer doesn't hurt the
  // matched filter, and the tone keeps its true position. (Cap pad at 5 s for sanity.)
  let gap = d.frame - calib.micNextFrame;
  if (gap > 0) {
    if (gap > 5 * ac.sampleRate) gap = 5 * ac.sampleRate;
    calib.micChunks.push(new Float32Array(gap));
    calib.micLen += gap;
  }
  calib.micChunks.push(d.samples);
  calib.micLen += d.samples.length;
  calib.micNextFrame = d.frame + d.samples.length;
}

async function setupMic(stream) {
  if (!calib.workletReady) {
    const url = URL.createObjectURL(new Blob([MIC_WORKLET_SRC], { type: "application/javascript" }));
    try {
      await ac.audioWorklet.addModule(url);
    } finally {
      URL.revokeObjectURL(url);
    }
    calib.workletReady = true;
  }
  const source = ac.createMediaStreamSource(stream);
  const node = new AudioWorkletNode(ac, "mic-tap", {
    numberOfInputs: 1,
    numberOfOutputs: 1,
    channelCount: 1,
    channelCountMode: "explicit",
    outputChannelCount: [1],
  });
  const sink = ac.createGain();
  sink.gain.value = 0; // silent: keeps the graph pulling the worklet without feedback
  source.connect(node);
  node.connect(sink);
  sink.connect(ac.destination);
  node.port.onmessage = onMicBlock;
  const mic = { stream, source, node, sink };
  calib.mic = mic;
  if (!calib.worker) {
    const wurl = URL.createObjectURL(new Blob([DSP_WORKER_SRC], { type: "application/javascript" }));
    calib.worker = new Worker(wurl);
    URL.revokeObjectURL(wurl);
  }
  return mic; // hand back the exact nodes so a superseded caller can tear down ITS own tap
}

function teardownMic() {
  const m = calib.mic;
  if (m) {
    try { m.node.port.onmessage = null; } catch (e) {}
    try { m.source.disconnect(); } catch (e) {}
    try { m.node.disconnect(); } catch (e) {}
    try { m.sink.disconnect(); } catch (e) {}
    try { m.stream.getTracks().forEach((t) => t.stop()); } catch (e) {}
    calib.mic = null;
  }
}

function endCalibSession() {
  calib.active = false;
  // Silence any self-test chirps scheduled into the future (mirrors stopRef for the ref role).
  if (calib.selfSources) {
    for (const s of calib.selfSources) { try { s.onended = null; s.stop(); } catch (e) {} }
    calib.selfSources = null;
  }
  // Resolve any in-flight measurement so its awaiter unwinds before we kill the worker.
  if (calib.pendingResolve) {
    try { calib.pendingResolve(null); } catch (e) {}
    calib.pendingResolve = null;
  }
  teardownMic();
  if (calib.worker) {
    try { calib.worker.terminate(); } catch (e) {}
    calib.worker = null;
  }
  calib.micChunks = null;
}

// Estimate this device's own output (+ input) latency from what the browser reports, so
// "hear the other only" doesn't over-correct by our own playout lag. Unreported latency
// (e.g. Bluetooth codec delay) is NOT captured here and remains as residual.
function estOutLatMs() {
  let s = 0;
  if (ac && typeof ac.outputLatency === "number" && ac.outputLatency > 0 && ac.outputLatency < 0.6) s = ac.outputLatency;
  else if (ac && typeof ac.baseLatency === "number" && ac.baseLatency > 0 && ac.baseLatency < 0.6) s = ac.baseLatency;
  let mic = 0;
  try {
    const st = calib.mic && calib.mic.stream.getAudioTracks()[0].getSettings();
    if (st && st.latency > 0 && st.latency < 0.6) mic = st.latency;
  } catch (e) {}
  return (s + mic) * 1000;
}

// Cancel any running role (also called by stop()/teardownConnection()).
function calibAbort(msg) {
  const was = calib.running || !!calib.mic || calib.role === "ref";
  stopRef();
  endCalibSession();
  calib.role = null;
  calib.running = false;
  calib.orchestrated = false; // clear any server-orchestration state
  calib.refSeed = null;
  calib.selfSeed = null;
  calib.slot = 0;
  applyGain(); // un-mute our music (a listen/ref session muted it)
  if (els.caliblisten) { els.caliblisten.textContent = "🎤 Listen & align"; els.caliblisten.disabled = false; }
  if (els.calibref) els.calibref.disabled = false;
  // Collapse the role panel on every abort/cancel/error path (the startListen finally only covers
  // a normal run-completion). Without this, the mic-denied / mic-setup-fail early returns leave the
  // panel open, so the next top-level Calibrate tap just toggles it closed and emits no tone.
  // calibOnRole re-reveals the panel immediately after it calls calibAbort, so orchestration is fine.
  if (els.calibroles) els.calibroles.style.display = "none";
  if (was && msg) setCalibStatus(msg);
}

// ---- Reference role: emit the dial-up tone on the shared-clock beat -----------------
function refTick() {
  if (calib.role !== "ref" || !ac || offsetNs === null) return;
  // Coded path: schedule code periods back-to-back (continuous); chirp path: one tone per TICK_S.
  const tickNs = (calib.codePeriodS || CALCFG.TICK_S) * 1e9;
  const nowServerNs = performance.now() * 1e6 + offsetNs;
  const lookaheadNs = 2.0e9;
  for (let t = Math.ceil(nowServerNs / tickNs) * tickNs; t <= nowServerNs + lookaheadNs; t += tickNs) {
    if (calib.scheduled.has(t)) continue;
    const perfMs = (t - offsetNs) / 1e6 + effTrimMs(); // emit as if it were content for tick t
    const acT = ac.currentTime + (perfMs - performance.now()) / 1000;
    if (acT <= ac.currentTime + 0.03) continue; // too late to schedule cleanly — retry next pass
    calib.scheduled.add(t); // only mark scheduled once we actually start a source
    const src = ac.createBufferSource();
    src.buffer = calib.toneBuf;
    src.connect(ac.destination); // straight to output: detectable regardless of volume/mute
    src.start(acT);
    calib.refSources.push(src);
    src.onended = () => {
      const i = calib.refSources ? calib.refSources.indexOf(src) : -1;
      if (i >= 0) calib.refSources.splice(i, 1);
    };
  }
  const cutoff = nowServerNs - tickNs * 4; // bound the dedup set
  for (const v of calib.scheduled) if (v < cutoff) calib.scheduled.delete(v);
}

function startRef() {
  if (!ac || ac.state !== "running" || offsetNs === null) {
    showWarn("⚠ Start playback first (so the clock syncs), then play the sync tone.");
    return;
  }
  calib.role = "ref";
  calib.scheduled = new Set();
  calib.refSources = [];
  if (calibCoded()) {
    if (!calib.orchestrated) calib.refSeed = CALCFG.CODE_SEED; // manual ref uses the default seed
    calib.toneBuf = makeCodeBuffer(ac.sampleRate); // looping band-limited pseudonoise code
    calib.codePeriodS = CALCFG.CODE_N / 16000;
  } else {
    calib.toneBuf = makeToneBuffer(ac.sampleRate, CALCFG.CHIRP_F0, CALCFG.CHIRP_F1); // up-sweep chirp
    calib.codePeriodS = null;
  }
  if (gain) { try { gain.gain.cancelScheduledValues(ac.currentTime); } catch (e) {} gain.gain.value = 0; } // mute music — only the tone should sound
  refTick();
  calib.refTimer = setInterval(refTick, 400);
  els.calibref.textContent = "⏹ Stop tone";
  els.caliblisten.disabled = true;
  setCalibStatus(
    (calibCoded()
      ? "Playing a continuous sync code (music muted here). "
      : "Playing the sync tone every " + CALCFG.TICK_S + "s (music muted here). ") +
      (calib.orchestrated
        ? "Other devices are aligning to this one."
        : "On another device in the room, tap 🎤 Listen & align.")
  );
}

function stopRef() {
  if (calib.refTimer) { clearInterval(calib.refTimer); calib.refTimer = null; }
  if (calib.refSources) { // silence tones already scheduled into the future
    for (const s of calib.refSources) { try { s.onended = null; s.stop(); } catch (e) {} }
    calib.refSources = null;
  }
  const wasRef = calib.role === "ref";
  if (calib.role === "ref") calib.role = null;
  calib.scheduled = null;
  calib.toneBuf = null;
  calib.codePeriodS = null;
  if (wasRef) applyGain(); // restore music to the saved volume/mute
  if (els.calibref) els.calibref.textContent = "🔊 Play sync tone";
  if (els.caliblisten) els.caliblisten.disabled = false;
}

// ---- Follower role: hear the reference's tone and align our own trim ----------------
// Send the captured mic buffer to the DSP worker and resolve its matched-filter result.
function calibWorkerAnalyze(payload, transfer) {
  return new Promise((resolve) => {
    const w = calib.worker;
    if (!w) { resolve(null); return; }
    const done = (v) => { w.removeEventListener("message", onMsg); calib.pendingResolve = null; resolve(v); };
    const onMsg = (ev) => done(ev.data);
    calib.pendingResolve = done; // so an abort can unblock this await
    w.addEventListener("message", onMsg);
    w.postMessage(payload, transfer);
  });
}

// Median of a SORTED numeric array. For an even count, average the two central elements —
// picking the upper one (arr[n>>1]) biases every correction toward over-shoot at the low
// detection counts that are the common case here.
function calibMedian(sorted) {
  const n = sorted.length;
  if (!n) return null;
  const m = n >> 1;
  return n & 1 ? sorted[m] : (sorted[m - 1] + sorted[m]) / 2;
}

// Measure THIS device's own speaker→mic loopback (output latency + air + mic-in) with a
// few DOWN-sweeps — distinct from the reference's up-sweep so they never confuse. This is
// the systematic latency the "listen-only" method can't otherwise see; subtracting the
// measured value (instead of the browser's estimate) is what gets alignment to a few ms.
// Returns seconds, or null if it can't get a consistent reading (→ fall back to estimate).
async function measureSelfLoop(run) {
  const selfBuf = makeToneBuffer(ac.sampleRate, CALCFG.CHIRP_F1, CALCFG.CHIRP_F0); // down-sweep
  const lead = 0.25, MAXLAG = 0.8; // tolerate big phone/Bluetooth output latency
  const capMs = Math.round((lead + MAXLAG + CALCFG.CHIRP_MS / 1000 + 0.2) * 1000);
  const MIN_SELF_SCORE = 0.3; // must CLEARLY hear our own (loud, clean) echo to trust it
  const loops = [], scores = [];
  calib.selfSources = [];
  for (let i = 0; i < 5 && calib.running && calib.runSeq === run; i++) {
    calib.micChunks = []; calib.micT0 = null; calib.micLen = 0; calib.micNextFrame = null;
    calib.active = true;
    const Temit = ac.currentTime + lead;
    const src = ac.createBufferSource();
    src.buffer = selfBuf;
    src.connect(ac.destination); // straight to output (bypasses the muted music gain)
    src.start(Temit);
    // Track it so an abort/cancel during the capture sleep can silence a chirp that's
    // already scheduled into the future (otherwise it "whoops" after the user hit Cancel).
    calib.selfSources.push(src);
    src.onended = () => { const i = calib.selfSources ? calib.selfSources.indexOf(src) : -1; if (i >= 0) calib.selfSources.splice(i, 1); };
    await calibSleep(capMs);
    // If a newer session took over during the sleep, bail WITHOUT clearing calib.active —
    // that flag now belongs to the new session's mic capture and must not be stomped.
    if (!calib.running || calib.runSeq !== run) return null;
    calib.active = false;
    if (calib.micT0 === null || calib.micLen === 0) continue;
    const mic = new Float32Array(calib.micLen);
    let o = 0;
    for (const c of calib.micChunks) { mic.set(c, o); o += c.length; }
    const res = await calibWorkerAnalyze(
      { mic, micRate: ac.sampleRate, micT0: calib.micT0, chirp: { ms: CALCFG.CHIRP_MS, f0: CALCFG.CHIRP_F1, f1: CALCFG.CHIRP_F0 }, minScore: 0.05, minSepSec: 0.05, phatBeta: CALCFG.PHAT_BETA },
      [mic.buffer]
    );
    if (!res || !res.peaks) continue;
    let best = null;
    for (const pk of res.peaks) {
      const lag = pk.time - Temit; // emit→arrival
      if (lag >= -0.02 && lag <= MAXLAG) { if (!best || pk.score > best.score) best = pk; }
    }
    // Only trust a STRONG match — a faint/garbage peak (e.g. the reference's up-sweep
    // bleeding through) would poison the correction. If unsure, we fall back to the estimate.
    if (best && best.score >= MIN_SELF_SCORE) { loops.push(best.time - Temit); scores.push(best.score); }
  }
  const ok = loops.length;
  loops.sort((a, b) => a - b);
  const med = calibMedian(loops);
  // Consistency gate: with ≥4 reps, drop the single worst on each end so one outlier (a stray
  // reflection / glitchy rep) doesn't falsely fail an otherwise-tight self-test.
  const spread = ok >= 4 ? loops[ok - 2] - loops[1] : ok ? loops[ok - 1] - loops[0] : 0;
  try { console.log("[calib] selfLoop reps=" + ok + " medMs=" + (med != null ? Math.round(med * 1000) : "—") + " spreadMs=" + Math.round(spread * 1000) + " maxScore=" + (scores.length ? Math.max.apply(null, scores).toFixed(2) : "—")); } catch (e) {}
  if (ok < 2) return null; // not enough clean reads → fall back to browser estimate
  if (spread > 0.03) return null; // inconsistent → don't trust
  if (med < 0.003 || med > MAXLAG) return null; // implausible
  return med;
}

// One measurement: record a few ticks, find the reference tone near each of our own
// playout beats, return the median arrival gap (ms). → {medGapMs,n} | {n} | {error} | null.
async function runListenMeasurement(run, selfLoopSec) {
  calib.micChunks = [];
  calib.micT0 = null;
  calib.micLen = 0;
  calib.micNextFrame = null;
  const AC0 = ac.currentTime, P0 = performance.now(); // sample the perf↔ac mapping once
  const coded = calibCoded();
  // Period of the repeating event we align to: one code period (coded) or one tick (chirp).
  const periodS = coded ? CALCFG.CODE_N / 16000 : CALCFG.TICK_S;
  // Coded path's event is the (instantaneous) code-phase-0; the chirp spans CHIRP_MS.
  const toneDur = coded ? 0 : CALCFG.CHIRP_MS / 1000;
  // Search half-width around each expected beat, kept < period/2 so a neighbouring period
  // can't alias into this one.
  const half = coded ? periodS * 0.45 : CALCFG.SEARCH_HALF_S;
  calib.active = true;
  // (Our own music is muted for the whole listen session in startListen, so the mic hears
  // the reference cleanly.) Coded: record CODE_PERIODS periods (the code is continuous, so a
  // short capture sees several). Chirp: size so ≥ WINDOW_TICKS tones land fully inside.
  const captureMs = coded
    ? Math.round((CALCFG.CODE_PERIODS * periodS + 2 * half + 0.6) * 1000)
    : Math.round((CALCFG.WINDOW_TICKS * CALCFG.TICK_S + 2 * CALCFG.SEARCH_HALF_S + toneDur + 0.6) * 1000);
  await calibSleep(captureMs);
  // Superseded mid-capture → bail without clearing the new session's calib.active flag.
  if (!calib.running || calib.runSeq !== run) return null;
  calib.active = false;
  const micLenSec = calib.micLen / ac.sampleRate;
  if (calib.micT0 === null || calib.micLen === 0) return { noMic: true, micLenSec: 0 };
  const mic = new Float32Array(calib.micLen);
  let o = 0;
  for (const c of calib.micChunks) { mic.set(c, o); o += c.length; }
  const recStart = calib.micT0; // ac-time of first recorded sample
  const recEnd = calib.micT0 + micLenSec; // …and last
  const tmplParams = coded
    ? { code: { seed: calib.refSeed || CALCFG.CODE_SEED, n: CALCFG.CODE_N, f0: CALCFG.CODE_F0, f1: CALCFG.CODE_F1 } }
    : { chirp: { ms: CALCFG.CHIRP_MS, f0: CALCFG.CHIRP_F0, f1: CALCFG.CHIRP_F1 } };
  const res = await calibWorkerAnalyze(
    { mic, micRate: ac.sampleRate, micT0: calib.micT0, ...tmplParams, minScore: coded ? 0.04 : CALCFG.MIN_SCORE, minSepSec: periodS * 0.5, phatBeta: CALCFG.PHAT_BETA },
    [mic.buffer]
  );
  if (!res) return null;
  if (res.error) return { error: res.error };
  const peaks = res.peaks || [];
  const top = res.top || 0; // best matched-filter score anywhere (diagnostics)
  const rms = res.rms || 0; // overall mic level (0 = silence)
  const tickNs = periodS * 1e9;
  const startServerNs = P0 * 1e6 + offsetNs;
  const endServerNs = startServerNs + captureMs * 1e6;
  const gaps = [];
  for (let t = Math.ceil(startServerNs / tickNs) * tickNs; t <= endServerNs; t += tickNs) {
    const SL = AC0 + (t - startServerNs) / 1e9 + effTrimMs() / 1000; // our playout beat for tick t (ac time)
    // Coded path: center the search on our OWN measured loopback so the window tracks the small
    // residual (ref−follower latency mismatch), not the raw arrival latency — otherwise a
    // high-latency reference speaker (Bluetooth/HDMI) could fall outside ±half and alias to the
    // next period. Chirp path keeps centering on the beat (its ±0.7 s window already covers it).
    const center = coded ? SL + selfLoopSec : SL;
    // Only count beats whose whole symmetric window + tone length lies inside the recording —
    // a tone must never be "missing" merely because it fell past the captured audio.
    if (center - half < recStart || center + half + toneDur > recEnd) continue;
    let best = null;
    for (const pk of peaks) {
      const g = pk.time - center;
      if (g >= -half && g <= half) {
        if (best === null || pk.score > best.score) best = pk; // strongest match = direct path
      }
    }
    if (best) gaps.push(best.time - SL); // raw gap (arrival − beat); corr subtracts selfLoop later
  }
  // Diagnostics — inspect window.nfsCalib or the console if a measurement comes up empty.
  const diag = { micLenSec: +micLenSec.toFixed(2), micLevel: +rms.toFixed(4), peaks: peaks.length, topScore: +top.toFixed(3), detections: gaps.length, gapsMs: gaps.map((g) => Math.round(g * 1000)) };
  try { window.nfsCalib = diag; console.log("[calib]", JSON.stringify(diag)); } catch (e) {}
  const minDet = coded ? CALCFG.CODE_MIN_DETECTIONS : CALCFG.MIN_DETECTIONS;
  if (gaps.length < minDet) return { n: gaps.length, top, micLenSec, rms };
  gaps.sort((a, b) => a - b);
  return { medGapMs: calibMedian(gaps) * 1000, n: gaps.length, top, micLenSec, rms };
}

async function startListen() {
  if (calib.role === "listen") { calibAbort("Aligning cancelled."); return; } // toggle = cancel
  if (!ac || ac.state !== "running" || offsetNs === null) {
    showWarn("⚠ Start playback first (so the clock syncs), then align.");
    return;
  }
  if (!navigator.mediaDevices || !navigator.mediaDevices.getUserMedia) {
    showWarn("⚠ This browser can't use the microphone. Sync by ear with the slider.");
    return;
  }
  calib.role = "listen";
  calib.running = true;
  const myRun = ++calib.runSeq; // identity for the stale-finally / supersede guard
  const owns = () => calib.running && calib.runSeq === myRun; // still this session's turn?
  els.caliblisten.textContent = "✕ Cancel";
  els.calibref.disabled = true;
  els.warn.style.display = "none";
  setCalibStatus("Requesting microphone…");

  let stream;
  try {
    stream = await navigator.mediaDevices.getUserMedia({
      audio: { echoCancellation: false, noiseSuppression: false, autoGainControl: false, channelCount: 1 },
      video: false,
    });
  } catch (e) {
    showWarn("⚠ Aligning needs microphone access. Allow the prompt, or sync by ear with the slider.");
    calibAbort(""); setCalibStatus(""); return;
  }
  if (!owns()) { stream.getTracks().forEach((t) => t.stop()); return; } // cancelled/superseded during prompt

  const tr = stream.getAudioTracks()[0];
  const st = tr && tr.getSettings ? tr.getSettings() : {};
  const aecForced = st.echoCancellation === true; // iOS Safari often ignores the constraint

  let myMic;
  try {
    myMic = await setupMic(stream);
  } catch (e) {
    try { stream.getTracks().forEach((t) => t.stop()); } catch (e2) {}
    showWarn("⚠ Couldn't start the microphone. Use the slider to sync by ear.");
    calibAbort(""); setCalibStatus(""); return;
  }
  if (!owns()) {
    // Superseded/cancelled while parked inside setupMic's first-run worklet compile: we installed
    // our mic tap AFTER the new session's teardown ran. Disconnect only OUR nodes (not the global
    // calib.mic, which the new run may now own) so our tap can't leak blocks into its capture.
    if (myMic) { try { myMic.node.port.onmessage = null; myMic.source.disconnect(); myMic.node.disconnect(); myMic.sink.disconnect(); } catch (e) {} }
    try { stream.getTracks().forEach((t) => t.stop()); } catch (e) {}
    if (calib.mic === myMic) calib.mic = null;
    return;
  }

  // Mute our own music for the whole session — our speaker sits next to our mic and would
  // otherwise drown out the reference's tone. Restored in finally (via applyGain).
  if (gain) { try { gain.gain.cancelScheduledValues(ac.currentTime); } catch (e) {} gain.gain.value = 0; }

  // TDMA: stagger each follower's self-test by a FULL self-test length (slot × ~8.4 s) so the
  // loud, identical chirps serialize and never overlap in the shared room. Slot 0 starts at once;
  // the reference's code keeps playing throughout (the chirp self-test is separable from it by
  // shape). The listen phase that follows is collision-free, so it stays simultaneous.
  if (calib.orchestrated && calib.slot > 0) {
    setCalibStatus("Waiting for my self-test slot…");
    calibSendStatus("queued");
    await calibSleep(calib.slot * CALIB_SELF_SLOT_MS);
    if (!owns()) return; // cancelled / superseded during the wait
  }

  // Measure our OWN speaker→mic loopback first — this is the systematic latency the
  // listen-only method can't otherwise see (the ~100 ms residual). Subtracting the
  // measured value (vs the browser's estimate) is what gets us to a few ms.
  setCalibStatus("Measuring this device's own latency…");
  if (calib.orchestrated) calibSendStatus("self-test…");
  const selfLoopSec = await measureSelfLoop(myRun);
  const selfLoopMs = selfLoopSec != null ? selfLoopSec * 1000 : estOutLatMs();
  try { window.nfsSelfLoopMs = Math.round(selfLoopMs); window.nfsSelfMeasured = selfLoopSec != null; } catch (e) {}
  // On-screen so it's readable without a dev console (phones/Steam Deck).
  const selfStr = selfLoopSec != null ? "own lat " + Math.round(selfLoopMs) + "ms ✓" : "own lat ~" + Math.round(selfLoopMs) + "ms (est, self-test failed)";

  const startTrim = trimMs;
  // Measure from a 0 baseline so our OWN prior trim can't push the offset past a tick
  // boundary (the tone train is periodic, so a >½-tick offset would alias to the wrong
  // tick). Restored on any non-success exit. The remaining alias risk is only a large
  // trim on the REFERENCE device — keep the anchor near 0.
  if (!owns()) return; // cancelled/superseded during the self-test — leave trim & session to the owner
  setTrim(0);
  let bestTrim = 0, bestAbs = Infinity, everHeard = false, committed = false;
  const maxIters = calibCoded() ? CALCFG.CODE_MAX_ITERS : CALCFG.MAX_ITERS; // coded converges faster
  if (calib.orchestrated) calibSendStatus("listening…");
  try {
    for (let iter = 0; iter < maxIters && owns(); iter++) {
      setCalibStatus("Listening for the sync tone… " + selfStr + " (" + (iter + 1) + "/" + maxIters + ")");
      const trimAt = trimMs;
      const r = await runListenMeasurement(myRun, selfLoopMs / 1000); // window centers on our loopback (coded)
      if (!owns()) break; // cancelled, or superseded by a newer session
      if (r && r.error) throw new Error(r.error);
      if (r && r.noMic) {
        throw { soft: "No microphone input on this device — check the mic and that this browser tab has mic access." };
      }
      if (!r || r.medGapMs == null) {
        if (everHeard) break; // had a lock, lost it — stop with the best so far
        const heardSec = r && r.micLenSec ? r.micLenSec.toFixed(1) : "0";
        const matchPct = r && r.top != null ? Math.round(r.top * 100) : 0;
        const micPct = r && r.rms != null ? (r.rms * 100).toFixed(1) : "?";
        const selfNote = selfLoopSec != null ? "self-test OK" : "self-test FAILED";
        let why;
        if (r && r.rms != null && r.rms < 0.002) {
          why = "this device's mic is silent (mic level " + micPct + "%) — check Chrome's mic permission and that the right input device is selected.";
        } else if (selfLoopSec == null) {
          why = "this device can't hear its own speaker (mic level " + micPct + "%, " + selfNote + ") — make sure it's playing out a speaker (not headphones) and the mic works.";
        } else {
          why = "the mic works (level " + micPct + "%, self-test OK) but the reference tone never arrived. Make sure the OTHER device is on the latest version (hard-refresh it — it should play a rising “whoop”, not a dial-up sound), is turned up, and is in the same room.";
        }
        throw {
          soft: aecForced
            ? "This mic forces echo-cancellation, which blocks aligning. Use the slider to sync by ear."
            : "Couldn't pick out the sync tone (heard " + heardSec + "s, best match " + matchPct + "%): " + why,
        };
      }
      everHeard = true;
      const corr = r.medGapMs - selfLoopMs; // ms to add to our trim (subtract our own loopback)
      // Track best by the MEASURED residual at the trim we actually applied (trimAt), never
      // by the un-measured prediction — so we never commit a worse, unvalidated trim.
      if (Math.abs(corr) < bestAbs) { bestAbs = Math.abs(corr); bestTrim = trimAt; }
      if (Math.abs(corr) <= CALCFG.STEP_MS) break; // trimAt is aligned → done
      if (iter === maxIters - 1) break; // last pass: don't apply what we can't verify
      setTrim(trimAt + corr); // probe the predicted trim; the next iteration measures it
      setCalibStatus("Adjusting " + (corr >= 0 ? "+" : "") + Math.round(corr) + " ms…");
    }
    if (owns() && everHeard) {
      if (trimMs !== bestTrim) setTrim(bestTrim);
      committed = true;
      const moved = Math.round(bestTrim - startTrim);
      const latNote = selfLoopSec != null ? " (own latency " + Math.round(selfLoopMs) + " ms)" : " (own latency estimated — accuracy limited)";
      setCalibStatus("✔ Aligned to the reference — nudged this device " + (moved >= 0 ? "+" : "") + moved + " ms." + latNote);
      if (calib.orchestrated) calibSendStatus("aligned " + (moved >= 0 ? "+" : "") + moved + " ms");
    } else if (owns()) {
      showWarn("⚠ Couldn't complete a measurement. Try again.");
      setCalibStatus("");
      if (calib.orchestrated) calibSendStatus("no lock");
    }
  } catch (e) {
    if (calib.runSeq === myRun) {
      if (e && e.soft) showWarn("⚠ " + e.soft);
      else showWarn("⚠ Align error: " + (e && e.message ? e.message : e));
      setCalibStatus("");
      if (calib.orchestrated) calibSendStatus("failed");
    }
  } finally {
    if (calib.runSeq === myRun) { // a newer session hasn't taken over
      if (!committed) setTrim(startTrim); // restore the user's prior trim on failure/cancel
      applyGain(); // unmute our music (restore saved volume/mute)
      endCalibSession();
      calib.role = null;
      calib.running = false;
      calib.orchestrated = false; // an orchestrated follower session is now finished
      els.caliblisten.textContent = "🎤 Listen & align";
      els.caliblisten.disabled = false;
      els.calibref.disabled = false;
      // Collapse the role panel now the run is over: the top-level "Calibrate" button is a
      // toggle, so if we leave the panel open the NEXT tap just closes it and starts nothing
      // ("no tone on the 2nd calibrate"). Hiding it here makes the next tap re-open + re-run.
      // (#calibstatus is a separate sibling, so the "✔ Aligned" result stays visible.)
      if (els.calibroles) els.calibroles.style.display = "none";
    }
  }
}

// ---- Server-orchestrated calibration (Phase B) --------------------------------------
// Report short progress text back to the server (shown per-client in the server GUI).
function calibSendStatus(text) {
  if (!ws || ws.readyState !== WebSocket.OPEN) return;
  const b = new TextEncoder().encode(String(text).slice(0, 64));
  const buf = new Uint8Array(2 + b.length);
  buf[0] = MSG_CALIB_CTRL;
  buf[1] = 2; // STATUS
  buf.set(b, 2);
  try { ws.send(buf); } catch (e) {}
}

// Server assigned this device a calibration role. role: 0 = stop/idle, 1 = reference, 2 = follower.
function calibOnRole(role, refSeed, selfSeed, slot) {
  calibAbort(""); // cancel whatever this device was doing (also clears the orchestrated flags)
  if (role === 0) { setCalibStatus(""); calibSendStatus("idle"); return; }
  if (!ac || ac.state !== "running" || offsetNs === null) { calibSendStatus("not playing"); return; }
  calib.orchestrated = true;
  calib.refSeed = refSeed >>> 0;
  calib.selfSeed = selfSeed >>> 0;
  calib.slot = slot | 0;
  if (els.calibroles) els.calibroles.style.display = ""; // reveal the calibrate panel so it's visible
  if (role === 1) {
    startRef(); // reference: loop the shared code continuously
  } else if (role === 2) {
    startListen(); // follower: align to the reference (staggered self-test by slot)
  }
}

// ---- UI wiring: Calibrate reveals the two role buttons ------------------------------
els.calib.addEventListener("click", () => {
  const showing = els.calibroles.style.display !== "none";
  if (showing) {
    if (calib.role) calibAbort(""); // tapping Calibrate again closes/cancels
    els.calibroles.style.display = "none";
    setCalibStatus("");
  } else {
    els.calibroles.style.display = "";
    setCalibStatus("Same-room sync: on one device tap 🔊 Play sync tone, on another tap 🎤 Listen & align.");
  }
});
els.calibref.addEventListener("click", () => {
  if (calib.role === "ref") stopRef();
  else { calib.orchestrated = false; calib.refSeed = null; startRef(); } // manual ref uses the default seed
});
els.caliblisten.addEventListener("click", () => {
  if (calib.role !== "listen") { calib.orchestrated = false; calib.refSeed = null; calib.selfSeed = null; calib.slot = 0; }
  startListen();
});
els.calibcancel.addEventListener("click", () => { calibAbort(""); els.calibroles.style.display = "none"; setCalibStatus(""); });
