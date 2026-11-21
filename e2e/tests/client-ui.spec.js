// @ts-check
// The client's own controls: the state line in the middle of the screen, volume and mute, the sync
// steps, and naming the device. Driven the way a listener drives them — through the page.
const { test, expect } = require('@playwright/test');

async function play(page) {
  await page.goto('/');
  await page.locator('#start').click();
  try { await page.locator('#nameskip').click({ timeout: 3000 }); } catch { /* not shown */ }
  await expect(page.locator('#controls')).toBeVisible();
}

test.beforeEach(({ browserName }) => {
  test.skip(browserName !== 'chromium', 'needs WebCodecs AudioDecoder to leave the idle gate');
});

test('the header names the server and this device', async ({ page }) => {
  await page.addInitScript(() => { try { localStorage.setItem('nfs_cname', 'Kitchen iPad'); localStorage.setItem('nfs_named', '1'); } catch (e) {} });
  await page.goto('/');
  // Before connecting there is no server name yet, so the title is the app's own.
  await expect(page.locator('#brand')).toBeVisible();
  await expect(page.locator('#devline')).toHaveText('This device: Kitchen iPad');
  await page.locator('#start').click();
  await expect(page.locator('#srv')).not.toBeEmpty();
  await expect(page.locator('#brand')).toBeHidden();
});

test('the state is said in words, and an idle cast relay is "waiting", not "playing"', async ({ page, baseURL }) => {
  const health = await (await page.request.get(`${baseURL}/health`)).json();
  test.skip(!health.castSource || health.casting, 'needs a cast-relay server with nobody casting');
  await play(page);
  // The server tells its clients nobody is casting; without that the client could only say
  // "playing" (the clock is anchored) over a stream that carries nothing.
  await expect(page.locator('#statusline')).toHaveText('Waiting for a cast', { timeout: 15000 });
  await expect(page.locator('#statussub')).toContainText('nobody is casting');
});

test('mute says what a tap will do, and shows the result beside the slider', async ({ page }) => {
  await play(page);
  const mute = page.locator('#mute');
  await expect(mute).toHaveAttribute('aria-label', 'Mute');
  await expect(page.locator('#volpct')).toHaveText('100%');
  await mute.click();
  await expect(mute).toHaveAttribute('aria-pressed', 'true');
  await expect(mute).toHaveAttribute('aria-label', 'Unmute');
  await expect(page.locator('#volpct')).toHaveText('off');
  await mute.click();
  await expect(page.locator('#volpct')).toHaveText('100%');
});

test('sync can be stepped by one millisecond, and the slider is wide enough to use', async ({ page }) => {
  await page.setViewportSize({ width: 390, height: 780 }); // a phone: where the slider used to be 7 px wide
  await play(page);
  await expect(page.locator('#syncpanel')).toBeHidden();
  await page.locator('#syncbtn').click();
  await expect(page.locator('#syncpanel')).toBeVisible();

  await page.locator('#trimup').click();
  await page.locator('#trimup1').click();
  await page.locator('#trimup1').click();
  await page.locator('#trimdown1').click();
  expect(await page.evaluate(() => trimMs)).toBe(11);
  await expect(page.locator('#trimval')).toHaveText('+11.0 ms');
  await expect(page.locator('#synclabel')).toHaveText('+11 ms'); // the button itself carries the offset

  const width = await page.locator('#trim').evaluate((el) => el.getBoundingClientRect().width);
  expect(width, 'the sync slider must be usable on a phone').toBeGreaterThan(200);
});

test('a trim beyond the slider\'s usual range widens it instead of pinning the thumb', async ({ page }) => {
  await play(page);
  // Calibration can land anywhere in the full range. If the slider stayed at ±500 the thumb would sit
  // at the end, and the first touch would drag a calibrated +800 ms down to +500.
  const r = await page.evaluate(() => {
    setTrim(120);
    const narrow = [els.trim.min, els.trim.max];
    setTrim(800);
    return { narrow, wide: [els.trim.min, els.trim.max], value: els.trim.value };
  });
  expect(r.narrow).toEqual(['-500', '500']);
  expect(r.wide).toEqual(['-2000', '3000']);
  expect(r.value).toBe('800');
});

test('only one panel is open at a time', async ({ page }) => {
  await play(page);
  await page.locator('#syncbtn').click();
  await expect(page.locator('#syncpanel')).toBeVisible();
  await page.locator('#castbtn').click();
  await expect(page.locator('#castroles')).toBeVisible();
  await expect(page.locator('#syncpanel')).toBeHidden();
  await expect(page.locator('#syncbtn')).toHaveAttribute('aria-expanded', 'false');
  await expect(page.locator('#castbtn')).toHaveAttribute('aria-expanded', 'true');
});

test('renaming under More updates the header and survives a reload', async ({ page }) => {
  await page.goto('/');
  await page.locator('#morebtn').click();
  await page.locator('#devname').fill('Den speaker');
  await page.locator('#devnamesave').click();
  await expect(page.locator('#devline')).toHaveText('This device: Den speaker');
  await page.reload();
  await expect(page.locator('#devline')).toHaveText('This device: Den speaker');
  // Named by hand, so the first-connect prompt has nothing left to ask.
  expect(await page.evaluate(() => localStorage.getItem('nfs_named'))).toBe('1');
});

test('the diagnostics are one tap away and stay open once opened', async ({ page }) => {
  await page.goto('/');
  await expect(page.locator('#details')).toBeHidden();
  await page.locator('#detailsbtn').click();
  await expect(page.locator('#details')).toBeVisible();
  await page.reload();
  await expect(page.locator('#details')).toBeVisible();
});
