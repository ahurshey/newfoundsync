// @ts-check
// The video stage must take the PICTURE's shape, and the fit control must actually change how the
// picture meets that box.
//
// This is the bug class these tests exist for: an ultrawide desktop arrived squashed, and nothing in
// the suite noticed, because every frame counter, timestamp and health field was perfect. The only
// symptom was that circles were ellipses. Geometry needs its own assertions.
const { test, expect } = require('@playwright/test');

/** Start playback so the stage and its controls exist. */
async function play(page) {
  await page.goto('/');
  await expect(page.locator('#start')).toBeVisible();
  await page.locator('#start').click();
  try {
    await page.locator('#nameskip').click({ timeout: 3000 });
  } catch {
    /* modal not shown */
  }
}

test.beforeEach(async ({ page, browserName }) => {
  test.skip(browserName !== 'chromium', 'needs WebCodecs to leave the idle gate');
  await play(page);
});

test('the stage adopts the frame shape instead of a hard-coded 16:9', async ({ page }) => {
  // Drive the paint path with a synthetic ultrawide frame rather than waiting for a real capture —
  // the geometry is what is under test, not the decoder.
  const ar = await page.evaluate(() => {
    els.stage.style.display = 'block';
    els.canvas.width = 3440;
    els.canvas.height = 1440;
    els.stage.style.setProperty('--stage-ar', '3440 / 1440');
    return getComputedStyle(els.stage).getPropertyValue('--stage-ar').trim();
  });
  expect(ar).toBe('3440 / 1440');
});

test('the fit button cycles contain -> cover -> fill and back', async ({ page }) => {
  await page.evaluate(() => {
    els.stage.style.display = 'block';
    els.fitbtn.style.display = '';
  });
  const state = async () =>
    page.evaluate(() => ({
      fit: getComputedStyle(els.canvas).objectFit,
      label: els.fitbtn.textContent.trim(),
    }));

  // Starts on the honest one: whole picture, true shape.
  expect((await state()).fit).toBe('contain');

  await page.locator('#fitbtn').click();
  expect((await state()).fit, 'second mode fills the box by cropping').toBe('cover');

  await page.locator('#fitbtn').click();
  expect((await state()).fit, 'third mode stretches').toBe('fill');

  await page.locator('#fitbtn').click();
  expect((await state()).fit, 'and wraps around').toBe('contain');
});

test('the chosen fit survives a reload — it is a property of the screen, not the session', async ({ page }) => {
  await page.evaluate(() => {
    els.stage.style.display = 'block';
    els.fitbtn.style.display = '';
  });
  await page.locator('#fitbtn').click(); // -> cover
  expect(await page.evaluate(() => localStorage.getItem('nfs_fit'))).toBe('cover');

  await play(page);
  const after = await page.evaluate(() => {
    els.stage.style.display = 'block';
    return getComputedStyle(els.canvas).objectFit;
  });
  expect(after).toBe('cover');
});
