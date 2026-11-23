// @ts-check
// Video frame targets are smoothed onto a steady cadence (app.js "Video cadence"): a few ms of wobble
// per frame — capture stamps, clock-sync updates — must not reach the screen, where on a 60 Hz panel
// it made ~30% of a 30 fps picture's frames land a refresh early or late (measured on a Google TV).
const { test, expect } = require('@playwright/test');

test('jittered frame targets come out evenly spaced', async ({ page }) => {
  await page.goto('/');
  const r = await page.evaluate(() => {
    resetVideoCadence();
    const period = 1000 / 30;
    let seed = 7;
    const rnd = () => ((seed = (seed * 16807) % 2147483647) / 2147483647) * 2 - 1; // deterministic ±1
    const out = [];
    for (let i = 0; i < 300; i++) {
      const pts = i * period + rnd() * 4; // the source's capture stamps wobble ±4 ms
      const target = 5000 + pts + rnd() * 3; // and the clock mapping another ±3 ms
      out.push(smoothVideoTarget(target, pts));
    }
    const gaps = [];
    for (let i = 101; i < out.length; i++) gaps.push(out[i] - out[i - 1]); // after it has settled
    return { min: Math.min(...gaps), max: Math.max(...gaps) };
  });
  expect(r.min).toBeGreaterThan(1000 / 30 - 1.5);
  expect(r.max).toBeLessThan(1000 / 30 + 1.5);
});

test('a real jump in the timeline is followed at once, not smoothed over', async ({ page }) => {
  await page.goto('/');
  const r = await page.evaluate(() => {
    resetVideoCadence();
    const period = 1000 / 30;
    for (let i = 0; i < 60; i++) smoothVideoTarget(1000 + i * period, i * period);
    // The source skips half a second (a stall, a trim change): the next frame must not be held back.
    const after = smoothVideoTarget(1000 + 60 * period + 500, 60 * period + 500);
    return after - (1000 + 60 * period + 500);
  });
  expect(Math.abs(r)).toBeLessThan(0.001);
});

test('a skipped source frame leaves a gap of two frames, not a squeezed one', async ({ page }) => {
  await page.goto('/');
  const r = await page.evaluate(() => {
    resetVideoCadence();
    const period = 1000 / 30;
    let last = 0;
    for (let i = 0; i < 60; i++) last = smoothVideoTarget(1000 + i * period, i * period);
    const next = smoothVideoTarget(1000 + 61 * period, 61 * period); // frame 60 never came
    return (next - last) / period;
  });
  expect(r).toBeCloseTo(2, 1);
});
