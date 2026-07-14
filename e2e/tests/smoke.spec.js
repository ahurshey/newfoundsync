// @ts-check
// Full-loop smoke: load the real client, press Start, and confirm it leaves the idle gate and
// connects to the server (exercises the WebSocket connect + config + clock-sync + decoder-setup path
// end to end). Audio won't audibly play in a headless browser with no output device, but the state
// machine still progresses — so we assert on observable UI state, not sound.
const { test, expect } = require('@playwright/test');

test('serves the client and reports its version', async ({ page, baseURL }) => {
  const res = await page.request.get(`${baseURL}/version`);
  expect(res.ok()).toBeTruthy();
  const tag = (await res.text()).trim();
  expect(tag, 'server /version build tag').toMatch(/^[0-9a-f]{8,}$/);
});

test('loads the idle client', async ({ page }) => {
  await page.goto('/');
  await expect(page.locator('#start')).toBeVisible();
  await expect(page.locator('#state')).toHaveText('idle');
  // The client must always boot fresh (no service worker layer in front of it).
  const boot = await page.evaluate(() => /** @type {any} */ (window).__NFS_APP_BOOT);
  expect(boot).toBeTruthy();
});

test('Start leaves the idle gate and connects', async ({ page, browserName }) => {
  // The Start gate requires a WebCodecs AudioDecoder. That's reliable on Chromium; WebKit's WebCodecs
  // is incomplete (Start correctly can't proceed) and Firefox won't launch on the build box — so the
  // decode/connect path is verified on chromium. The engine-agnostic shell/cache/idle specs still run
  // everywhere.
  test.skip(browserName !== 'chromium', 'needs WebCodecs AudioDecoder (Chromium)');
  await page.goto('/');
  await expect(page.locator('#start')).toBeVisible();

  await page.locator('#start').click();

  // First-connect may pop the "name this device" modal — dismiss it so it can't mask later state.
  const skip = page.locator('#nameskip');
  try {
    await skip.click({ timeout: 3000 });
  } catch {
    /* modal didn't appear (already named / not shown) — fine */
  }

  // After Start the idle gate is dismissed and the client begins connecting/buffering/playing.
  await expect(page.locator('#start')).toBeHidden();
  await expect(page.locator('#state')).not.toHaveText('idle');
  // It reached the server: the server name populates once the WebSocket + config handshake lands.
  await expect(page.locator('#srv')).not.toBeEmpty();
});
