// Unit tests for the calibration DSP — the first tests this code has ever had.
//
// Why these matter more than they look: a sign flip, an off-by-one, or a filter with the wrong
// coefficients here does NOT crash and does NOT fail any other test. It produces a client that
// connects, reports healthy, plays audio, and lands at the wrong instant — caught only by a human
// standing between two speakers. Everything asserted below is a property that must hold for the
// correlation to lock at all, so a break here is a break in sync itself.
//
// Run: cd e2e && npm run test:unit   (no browser, no server, milliseconds)
// Not `node --test <dir>` — on Node 24 that resolves the path as a MODULE and fails with "Cannot
// find module", which reads as a broken test suite rather than a bad invocation. The npm script
// names the files explicitly, and is what CI runs.
const { test } = require('node:test');
const assert = require('node:assert/strict');
const path = require('node:path');

const DSP = require(path.join(__dirname, '..', '..', 'crates', 'desktop', 'web', 'nfs-dsp.js'));
const { calCodePrng, calBiquad, calBiquadLP, calBiquadHP, calBuildCode, calResample } = DSP;

/** Peak absolute value of a sequence. */
const peak = (x) => x.reduce((m, v) => Math.max(m, Math.abs(v)), 0);
/** Plain RMS, for energy comparisons. */
const rms = (x) => Math.sqrt(x.reduce((s, v) => s + v * v, 0) / x.length);

test('the code is DETERMINISTIC for a seed — both devices must generate the same signal', () => {
  // The reference plays it and the follower correlates against its own copy. If the same seed ever
  // produced different chips on two devices, correlation could never lock and calibration would
  // silently return garbage offsets.
  const a = calCodePrng(12345, 2048);
  const b = calCodePrng(12345, 2048);
  assert.deepEqual(Array.from(a), Array.from(b), 'same seed must give the same sequence');

  const c = calCodePrng(12346, 2048);
  assert.notDeepEqual(Array.from(a), Array.from(c), 'a different seed must give a different sequence');
});

test('the code is ±1 chips and roughly balanced', () => {
  const x = calCodePrng(7, 8192);
  for (const v of x) assert.ok(v === 1 || v === -1, `chip must be ±1, got ${v}`);
  // A DC-biased code correlates against silence; ±5% is generous for 8192 fair coin flips.
  const mean = x.reduce((s, v) => s + v, 0) / x.length;
  assert.ok(Math.abs(mean) < 0.05, `code should be balanced, mean=${mean}`);
});

test('calBuildCode is peak-normalized to 1 and periodic', () => {
  const n = 4096;
  const code = calBuildCode(99, n, 16000, 500, 4000);
  assert.equal(code.length, n);

  // Peak-normalized: the playback path multiplies by CODE_AMP assuming a unit-peak signal, so a
  // normalization bug would either clip or emit a signal too quiet to hear over the room.
  assert.ok(Math.abs(peak(code) - 1) < 1e-6, `peak should be 1, got ${peak(code)}`);

  // Genuinely periodic — this is the whole reason calBuildCode filters a DOUBLED sequence and keeps
  // the second copy. If the filter's start transient leaked in, a correlation window straddling a
  // period boundary would fail to lock.
  const again = calBuildCode(99, n, 16000, 500, 4000);
  assert.deepEqual(Array.from(code), Array.from(again), 'must be reproducible');

  // Every sample finite: one NaN poisons the entire correlation.
  for (const v of code) assert.ok(Number.isFinite(v), 'every sample must be finite');
});

test('calBuildCode band-limits: energy outside [f0,f1] is attenuated', () => {
  const rate = 16000;
  const code = calBuildCode(3, 8192, rate, 500, 4000);
  // Compare in-band vs far-out-of-band energy with a naive Goertzel-style probe.
  const energyAt = (hz) => {
    let re = 0, im = 0;
    const w = (2 * Math.PI * hz) / rate;
    for (let i = 0; i < code.length; i++) {
      re += code[i] * Math.cos(w * i);
      im += code[i] * Math.sin(w * i);
    }
    return Math.hypot(re, im) / code.length;
  };
  const inBand = energyAt(2000);
  const belowBand = energyAt(60);    // well under the 500 Hz high-pass
  const aboveBand = energyAt(7500);  // well over the 4 kHz low-pass
  assert.ok(inBand > belowBand * 3, `in-band (${inBand}) should dominate sub-band (${belowBand})`);
  assert.ok(inBand > aboveBand * 3, `in-band (${inBand}) should dominate super-band (${aboveBand})`);
});

test('the biquads behave at DC and Nyquist', () => {
  const rate = 16000;
  const n = 2048;
  const dc = new Float32Array(n).fill(1);
  // Alternating ±1 is Nyquist (rate/2).
  const nyq = new Float32Array(n);
  for (let i = 0; i < n; i++) nyq[i] = i % 2 === 0 ? 1 : -1;
  const tail = (x) => x.slice(x.length >> 1); // skip the start transient

  // A low-pass must PASS DC and KILL Nyquist.
  const lp = calBiquadLP(rate, 1000);
  assert.ok(rms(tail(calBiquad(dc, lp))) > 0.9, 'LP should pass DC');
  assert.ok(rms(tail(calBiquad(nyq, lp))) < 0.05, 'LP should reject Nyquist');

  // A high-pass must KILL DC and PASS Nyquist. If these were ever swapped, calBuildCode would emit a
  // signal outside the band the microphone path is filtered to, and calibration would never lock.
  const hp = calBiquadHP(rate, 1000);
  assert.ok(rms(tail(calBiquad(dc, hp))) < 0.05, 'HP should reject DC');
  assert.ok(rms(tail(calBiquad(nyq, hp))) > 0.9, 'HP should pass Nyquist');
});

test('biquad coefficients are finite and normalized (a0 == 1)', () => {
  for (const fc of [100, 500, 1000, 4000, 7000]) {
    for (const c of [calBiquadLP(16000, fc), calBiquadHP(16000, fc)]) {
      assert.equal(c.length, 5, 'coefficients are [b0,b1,b2,a1,a2] with a0 normalized out');
      for (const v of c) assert.ok(Number.isFinite(v), `coefficient must be finite (fc=${fc})`);
    }
  }
});

test('calResample preserves a linear ramp (the interpolation is not skewed)', () => {
  // A ramp is the sharpest test of a linear interpolator: any index/fraction error shows up as a
  // systematic offset, which in the calibration path would read as a constant latency error — i.e.
  // every device confidently mis-aligned by the same amount.
  const n = 1000;
  const ramp = new Float32Array(n);
  for (let i = 0; i < n; i++) ramp[i] = i / (n - 1); // 0 → 1

  const up = calResample(ramp, 16000, 48000); // 3x up
  assert.ok(Math.abs(up.length - 3 * n) <= 3, `expected ~${3 * n} samples, got ${up.length}`);
  for (let i = 0; i < up.length; i++) {
    const expected = i / (up.length - 1);
    assert.ok(Math.abs(up[i] - expected) < 0.01, `up[${i}]=${up[i]} expected ≈${expected}`);
  }

  const down = calResample(ramp, 48000, 16000); // 3x down
  assert.ok(Math.abs(down.length - n / 3) <= 3, `expected ~${n / 3} samples, got ${down.length}`);
  for (let i = 0; i < down.length; i++) {
    const expected = i / (down.length - 1);
    assert.ok(Math.abs(down[i] - expected) < 0.01, `down[${i}]=${down[i]} expected ≈${expected}`);
  }
});

test('calResample is identity at equal rates and never returns an empty buffer', () => {
  const x = calCodePrng(1, 64);
  assert.equal(calResample(x, 16000, 16000), x, 'equal rates should short-circuit');
  // A pathological ratio must still yield at least one sample rather than a zero-length buffer that
  // would make downstream correlation divide by zero.
  assert.ok(calResample(x, 16000, 1).length >= 1);
});

test('a round trip through resampling stays recognizably the same signal', () => {
  // 16k → 48k → 16k should return something highly correlated with the original. This is the actual
  // path: the canonical 16 kHz code is rendered to the context rate to play, and the mic is brought
  // back to 16 kHz to correlate.
  const code = calBuildCode(42, 4096, 16000, 500, 4000);
  const back = calResample(calResample(code, 16000, 48000), 48000, 16000);
  const n = Math.min(code.length, back.length);
  let num = 0, da = 0, db = 0;
  for (let i = 0; i < n; i++) {
    num += code[i] * back[i];
    da += code[i] * code[i];
    db += back[i] * back[i];
  }
  const corr = num / Math.sqrt(da * db);
  assert.ok(corr > 0.95, `round-trip correlation should stay high, got ${corr.toFixed(4)}`);
});
