// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Alex Hurshman and the Newfoundsync contributors.

// Newfoundsync calibration DSP — the pure signal-processing core, extracted from app.js so it can be
// unit tested WITHOUT a browser.
//
// Why this file exists: this is the hardest and most consequential code in the project, and it had no
// test seam at all. A sign flip or an off-by-one here produces a client that connects, reports healthy,
// plays audio — and lands at the wrong instant. Every Rust test and every browser test stays green; the
// only thing that catches it is a human standing between two speakers. These functions are pure
// (arguments in, Float32Array out, no DOM, no globals beyond Math), so they can be asserted directly.
//
// It loads in three places and MUST behave identically in all of them:
//   1. the page, as a plain <script> before app.js (exposed as globals, so existing call sites work);
//   2. the DSP worker, which app.js builds by `.toString()`-ing these very function objects — that is
//      what guarantees the signal the reference PLAYS and the template the follower CORRELATES are
//      byte-identical, so do not add module-scope dependencies to any function below or the worker
//      copy will reference something that isn't there;
//   3. node --test (see e2e/unit/), via module.exports.
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

// ---- exports ------------------------------------------------------------------------------------
// Dual-target on purpose (see the header): CommonJS for `node --test`, and plain globals for the page
// so app.js keeps calling `calBuildCode(...)` unchanged and `.toString()` worker injection still works.
(function (root) {
  const api = { calCodePrng, calBiquad, calBiquadLP, calBiquadHP, calBuildCode, calResample };
  if (typeof module !== "undefined" && module.exports) module.exports = api;
  if (root) for (const k in api) root[k] = api[k];
})(typeof globalThis !== "undefined" ? globalThis : typeof self !== "undefined" ? self : null);
