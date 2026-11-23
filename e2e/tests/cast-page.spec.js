// @ts-check
// The page a Google Cast device (a TV) runs: the same client, served at /cast on the plain-HTTP port
// next to the main one (main port + 1), where it must start by itself, ask for PCM, show no controls
// and name itself after the TV. Google's receiver library is blocked here: off a Cast device it only
// retries a socket that does not exist, and the page must not depend on it to play.
const { test, expect } = require('@playwright/test');

function castBase(baseURL) {
  const u = new URL(baseURL || 'http://localhost:47155');
  u.port = String(Number(u.port) + 1);
  return u.origin;
}

test.beforeEach(async ({ page }) => {
  await page.route('**/cast_receiver_framework.js', (r) => r.abort());
});

test('the Cast page starts by itself, as the TV, with no controls', async ({ page, baseURL, browserName }) => {
  test.skip(browserName !== 'chromium', 'the playback path is verified on Chromium');
  const sockets = [];
  page.on('websocket', (ws) => sockets.push(ws.url()));
  await page.goto(`${castBase(baseURL)}/cast`);

  await expect(page.locator('html')).toHaveClass(/\bcast\b/);
  await expect(page.locator('#start')).toBeHidden();
  await expect(page.locator('#controls')).toBeHidden();
  await expect(page.locator('#namemodal')).toBeHidden();
  // Left the idle gate with nobody pressing Start.
  await expect(page.locator('#state')).not.toHaveText('idle', { timeout: 10_000 });
  // Named by the server (/cast/whoami): no Cast device was found from this address, so it is "TV at …".
  await expect(page.locator('#devline')).toContainText('TV at ');
  expect(sockets.some((u) => u.includes('/ws?cast=1')), `sockets: ${sockets.join(', ')}`).toBeTruthy();
  const ready = await page.evaluate(() => /** @type {any} */ (window).__NFS_APP_READY);
  expect(ready).toBeTruthy();
});

test('PCM from the server comes out as the floats onAudioData schedules', async ({ page, baseURL }) => {
  await page.goto(`${castBase(baseURL)}/cast`);
  const out = await page.evaluate(() => {
    // Two stereo frames of s16le at an odd offset, as they sit in a [type][pts:8][pcm] message.
    const msg = new Uint8Array(9 + 8);
    const pcm = new DataView(msg.buffer, 9);
    [16384, -32768, -16384, 32767].forEach((s, i) => pcm.setInt16(i * 2, s, true));
    /** @type {any} */
    let got = null;
    const dec = new AudioDecoder({ output: (ad) => { got = ad; }, error: () => {} });
    dec.configure({ codec: 'opus', sampleRate: 48000, numberOfChannels: 2 });
    dec.decode(new EncodedAudioChunk({ type: 'key', timestamp: 1234, data: new Uint8Array(msg.buffer, 9) }));
    const left = new Float32Array(2), right = new Float32Array(2);
    got.copyTo(left, { planeIndex: 0, format: 'f32-planar' });
    got.copyTo(right, { planeIndex: 1, format: 'f32-planar' });
    return { frames: got.numberOfFrames, rate: got.sampleRate, ts: got.timestamp, left: [...left], right: [...right] };
  });
  expect(out.frames).toBe(2);
  expect(out.rate).toBe(48000);
  expect(out.ts).toBe(1234);
  expect(out.left).toEqual([0.5, -0.5]);
  expect(out.right[0]).toBe(-1);
  expect(out.right[1]).toBeCloseTo(1, 4);
});

test('the ordinary page is untouched by the Cast mode', async ({ page }) => {
  await page.goto('/');
  await expect(page.locator('html')).not.toHaveClass(/\bcast\b/);
  await expect(page.locator('#start')).toBeVisible();
  const native = await page.evaluate(() => typeof AudioDecoder !== 'function' || String(AudioDecoder).includes('[native code]'));
  expect(native, 'the real WebCodecs AudioDecoder, not the PCM stand-in').toBeTruthy();
});

test('the Android TV app finds the server and opens the page as itself', async ({ page, baseURL }) => {
  const hello = await page.request.get(`${castBase(baseURL)}/hello`);
  expect(hello.ok()).toBeTruthy();
  const h = await hello.json();
  expect(h.app).toBe('newfoundsync');
  expect(typeof h.name).toBe('string');

  let castLibrary = false;
  page.on('request', (r) => { if (r.url().includes('cast_receiver_framework')) castLibrary = true; });
  await page.goto(`${castBase(baseURL)}/cast?app=1&id=tv-e2e0001&name=Den%20TV`);
  await expect(page.locator('#devline')).toContainText('Den TV');
  await expect(page.locator('#state')).not.toHaveText('idle', { timeout: 10_000 });
  expect(castLibrary, 'the app page must not load Google\'s Cast library').toBe(false);
});

test('the Android TV app page on the main port is hands-free but decodes itself (video too)', async ({ page, browserName }) => {
  test.skip(browserName !== 'chromium', 'the WebCodecs path is verified on Chromium');
  const sockets = [];
  page.on('websocket', (ws) => sockets.push(ws.url()));
  await page.goto('/?app=1&id=tv-e2e0002&name=Den%20TV');
  await expect(page.locator('html')).toHaveClass(/\btv\b/);
  await expect(page.locator('#start')).toBeHidden();
  await expect(page.locator('#controls')).toBeHidden();
  await expect(page.locator('#devline')).toContainText('Den TV');
  await expect(page.locator('#state')).not.toHaveText('idle', { timeout: 10_000 });
  expect(sockets.length, 'connected').toBeGreaterThan(0);
  expect(sockets.every((u) => !u.includes('cast=1')), 'not the PCM connection').toBeTruthy();
  const native = await page.evaluate(() => String(AudioDecoder).includes('[native code]'));
  expect(native, 'the real WebCodecs decoder, not the PCM stand-in').toBeTruthy();
});
