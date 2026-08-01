// @ts-check
// The ⟲ reset must clear BOTH halves of a device's alignment. Clearing the slider while leaving
// `aligned` set would claim an output-latency compensation the trim no longer contains — audio would
// sit one speaker latency off and video would follow it (see videoPresentMs).
const { test, expect } = require('@playwright/test');

test('reset clears the trim AND the aligned flag', async ({ page, browserName }) => {
  // The control lives in the playback panel, which is behind the idle gate — so this drives the app
  // the way a listener does rather than poking at a hidden button.
  test.skip(browserName !== 'chromium', 'needs WebCodecs AudioDecoder to leave the idle gate');
  await page.goto('/');
  await expect(page.locator('#start')).toBeVisible();
  await page.locator('#start').click();
  try { await page.locator('#nameskip').click({ timeout: 3000 }); } catch { /* not shown */ }
  await expect(page.locator('#trimreset')).toBeVisible();
  const before = await page.evaluate(() => {
    setTrim(250);
    markAligned(true);
    return { trimMs, aligned, corr: videoPresentMs(0) - serverPtsToPerfMs(0) };
  });
  expect(before.trimMs).toBe(250);
  expect(before.aligned).toBe(true);

  await page.locator('#trimreset').click();

  const after = await page.evaluate(() => ({
    trimMs, aligned, corr: videoPresentMs(0) - serverPtsToPerfMs(0),
    slider: document.getElementById('trim').value,
  }));
  expect(after.trimMs, 'trim back to zero').toBe(0);
  expect(after.aligned, 'and no longer claiming an alignment').toBe(false);
  expect(after.slider, 'the slider itself moved too').toBe('0');
  // With aligned cleared, video must stop adding the output-latency correction.
  expect(after.corr).toBe(0);
});

test('a server-pushed reset takes the same path as the button', async ({ page }) => {
  await page.goto('/');
  await expect(page.locator('#start')).toBeVisible();
  const after = await page.evaluate(() => {
    setTrim(-180);
    markAligned(true);
    // Exactly what the GUI's per-client / "Reset all" buttons send: tag 0x25, no payload.
    onMessage({ data: new Uint8Array([0x25]).buffer });
    return { trimMs, aligned };
  });
  expect(after.trimMs).toBe(0);
  expect(after.aligned).toBe(false);
});
