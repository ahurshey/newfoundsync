// @ts-check
// Playout self-correction: the scheduler must heal discontinuities on its own, and moving playout in
// time (trim, server push, tab refocus) must not throw away the buffered lead.
//
// Feeds real AudioData frames straight into onAudioData with server timestamps derived from the live
// clock offset, so the test controls exactly which frames "arrive" — a dropped run of frames is just a
// gap in the timestamps, which is what a server-side queue drop looks like to the client.
const { test, expect } = require('@playwright/test');

test.beforeEach(async ({ page, browserName }) => {
  // Builds WebCodecs AudioData by hand; CI runs Chromium only.
  test.skip(browserName === 'webkit', 'needs WebCodecs AudioData');
  await page.goto('/');
  await page.locator('#start').click();
  // Clock anchored + audio context running = onAudioData will schedule what we feed it.
  await page.waitForFunction(() => offsetNs !== null && ac && ac.state === 'running', null, { timeout: 15_000 });
  await page.evaluate(() => {
    const FRAME_NS = 20e6;
    // Server-mono ns that maps to "now" on this client; frame i is i*20 ms after it.
    const t0 = performance.now() * 1e6 + offsetNs;
    window.__feed = (from, to) => {
      for (let i = from; i < to; i++) {
        const ptsNs = t0 + i * FRAME_NS;
        onAudioData(
          new AudioData({
            format: 'f32-planar',
            sampleRate: 48000,
            numberOfFrames: 960,
            numberOfChannels: 2,
            timestamp: Math.round(ptsNs / 1000),
            data: new Float32Array(960 * 2),
          }),
        );
      }
      return t0 + to * FRAME_NS; // pts of the next frame
    };
    // Playout error (s) for a frame with this pts if it were scheduled now: + = early, − = late.
    window.__err = (nextPtsNs) =>
      ac.currentTime + (serverPtsToPerfMs(nextPtsNs) - performance.now()) / 1000 - (aligned ? 0 : outLatMs / 1000) - aPlayhead;
  });
});

test('a run of dropped frames is stepped out within a few frames, not slewed for seconds', async ({ page }) => {
  const r = await page.evaluate(() => {
    const steps0 = aSteps;
    __feed(0, 40);
    // Frames 40–44 never arrive (100 ms hole). The next frames land 100 ms early against the playhead.
    const next = __feed(45, 60);
    return { steps: aSteps - steps0, errMs: __err(next) * 1000, scheduled: liveSources.size };
  });
  expect(r.steps).toBe(1);
  expect(Math.abs(r.errMs)).toBeLessThan(5); // the rate servo alone would still be ~95 ms off here
  expect(r.scheduled).toBe(55); // a forward step only opens silence; nothing is dropped
});

test('a trim change re-times the buffered lead instead of cancelling it', async ({ page }) => {
  const r = await page.evaluate(() => {
    __feed(0, 60);
    const before = { n: liveSources.size, head: aPlayhead };
    setTrim(trimMs + 50);
    const later = { n: liveSources.size, head: aPlayhead };
    setTrim(trimMs - 50);
    return { before, later, back: { n: liveSources.size, head: aPlayhead } };
  });
  expect(r.later.n).toBe(r.before.n); // reanchorAudio() used to empty this → ~bufferMs of silence
  expect(r.later.head - r.before.head).toBeCloseTo(0.05, 6);
  expect(r.back.n).toBe(r.before.n);
  expect(r.back.head - r.before.head).toBeCloseTo(0, 6);
});

test('coming back to a still-playing tab keeps the buffered lead', async ({ page }) => {
  await page.evaluate(() => __feed(0, 60));
  const n = await page.evaluate(() => {
    let state = 'hidden';
    Object.defineProperty(document, 'visibilityState', { configurable: true, get: () => state });
    Object.defineProperty(document, 'hidden', { configurable: true, get: () => state === 'hidden' });
    document.dispatchEvent(new Event('visibilitychange'));
    state = 'visible';
    document.dispatchEvent(new Event('visibilitychange'));
    return { n: liveSources.size, anchored: aPlayhead !== null, synced: offsetNs !== null };
  });
  expect(n.n).toBe(60);
  expect(n.anchored).toBe(true);
  expect(n.synced).toBe(true);
});

test('frames that arrive late are stepped back, and the lead moves earlier with them', async ({ page }) => {
  const r = await page.evaluate(() => {
    const steps0 = aSteps;
    __feed(0, 40);
    // Frames 35–39 arrive AGAIN (a re-sent/backlogged run): everything after lands 100 ms late.
    const next = __feed(35, 60);
    return { steps: aSteps - steps0, errMs: __err(next) * 1000, scheduled: liveSources.size };
  });
  expect(r.steps).toBe(1);
  expect(Math.abs(r.errMs)).toBeLessThan(5);
  expect(r.scheduled).toBe(65); // a deep lead absorbs a 100 ms pull-in without skipping anything
});

test('pulling playout earlier than the lead allows skips content and never overlaps it', async ({ page }) => {
  const r = await page.evaluate(() => {
    __feed(0, 60);
    const n0 = liveSources.size;
    const first = Math.min(...[...liveSources].map((s) => s._at));
    shiftAudio(-(first - ac.currentTime) - 0.1); // the first ~100 ms of the lead would now be in the past
    const srcs = [...liveSources].sort((a, b) => a._at - b._at);
    let overlap = false;
    for (let i = 1; i < srcs.length; i++) if (srcs[i]._at < srcs[i - 1]._at + srcs[i - 1]._len - 1e-6) overlap = true;
    return { n0, n: srcs.length, earliest: srcs[0]._at - ac.currentTime, overlap };
  });
  expect(r.n).toBeLessThan(r.n0);
  expect(r.n).toBeGreaterThan(0);
  expect(r.earliest).toBeGreaterThanOrEqual(0.019);
  expect(r.overlap).toBe(false);
});
