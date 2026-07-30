// @ts-check
// A/V sync invariant: video must be presented when the matching sound is AUDIBLE.
//
// This is the bug class these tests exist for. `serverPtsToPerfMs` is the shared content→wall-clock
// map, and the audio path deliberately deviates from it: for an un-aligned device it submits samples
// `outLat` EARLY so the sound emerges on the map. Video, which has no speaker delay, must not copy
// that correction — and must not silently inherit it either. Once a device is ALIGNED (calibrated or
// manually trimmed) the compensation instead lives in `trimMs`, which IS inside the shared map, so
// video reading the raw map lands a full speaker output latency early — a visible lip-sync error
// (tens of ms on a laptop, 150 ms+ over Bluetooth) that no trim can remove, because trim moves audio
// and video together.
//
// Nothing else catches this: the client connects, reports healthy, plays audio and shows video. It
// only fails in front of a human watching someone's lips.
const { test, expect } = require('@playwright/test');

/**
 * Evaluate the two timing maps under a forced (aligned, outLat) state and return their difference.
 * Restores the state afterwards so one case can't leak into the next.
 */
async function delta(page, /** @type {boolean} */ isAligned, /** @type {number} */ outLat) {
  return page.evaluate(
    ([al, ol]) => {
      // Top-level `let` in a classic script lives in the global lexical environment, which this
      // function's scope chain includes — so these are the very bindings app.js schedules against.
      const prevA = aligned,
        prevO = outLatMs;
      aligned = al;
      outLatMs = ol;
      const pts = 1e9; // 1 s of server-mono ns; any value works, only the DIFFERENCE is asserted
      const d = videoPresentMs(pts) - serverPtsToPerfMs(pts);
      aligned = prevA;
      outLatMs = prevO;
      return d;
    },
    [isAligned, outLat],
  );
}

test.beforeEach(async ({ page }) => {
  await page.goto('/');
  await expect(page.locator('#start')).toBeVisible(); // app.js has run
});

test('un-aligned: video tracks the shared map (the audio path applies outLat itself)', async ({ page }) => {
  // onAudioData pulls the audio anchor back by outLat so sound EMERGES on the map. Video is already
  // at that instant, so any correction here would double-count and push the picture late.
  expect(await delta(page, false, 40)).toBe(0);
  expect(await delta(page, false, 250)).toBe(0);
});

test('aligned: video waits out the speaker latency that trim folded into the map', async ({ page }) => {
  // effTrim now carries the output-latency compensation, so the map is the SUBMIT time and the sound
  // lands outLat after it. Video has to wait exactly that long or it leads the lips.
  expect(await delta(page, true, 40)).toBe(40);
  expect(await delta(page, true, 250)).toBe(250);
});

test('the correction is the ONLY difference between the audio and video maps', async ({ page }) => {
  // Guards against a future edit re-routing video through serverPtsToPerfMs (the original bug) or
  // giving it an offset of its own: with no output latency to compensate, the two maps must coincide
  // in BOTH alignment states.
  expect(await delta(page, true, 0)).toBe(0);
  expect(await delta(page, false, 0)).toBe(0);
});

test('trim moves audio and video TOGETHER, so it can never be an A/V control', async ({ page }) => {
  // The reason this bug needed a code fix rather than a slider: trim is inside the shared map, so it
  // shifts the whole stream. If this ever stops holding, "just trim it" becomes a plausible-looking
  // fix that silently desyncs multi-room alignment instead.
  const shift = await page.evaluate(() => {
    const prev = trimMs;
    const pts = 1e9;
    trimMs = 0;
    const a0 = serverPtsToPerfMs(pts),
      v0 = videoPresentMs(pts);
    trimMs = 100;
    const a1 = serverPtsToPerfMs(pts),
      v1 = videoPresentMs(pts);
    trimMs = prev;
    return { audio: a1 - a0, video: v1 - v0 };
  });
  expect(shift.audio).toBeCloseTo(100, 6);
  expect(shift.video).toBeCloseTo(100, 6);
});
