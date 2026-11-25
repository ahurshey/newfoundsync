// @ts-check
// The pages the Android TV app opens (android/): both hands-free (start by themselves, no controls,
// named by the app). The HTTPS one (`/?app=1…`, main port) decodes with WebCodecs and plays video; the
// sound-only fallback (`/tv?app=1…`) lives on the plain-HTTP port next to it (main port + 1) and takes
// PCM from the server, because a plain-HTTP page has no WebCodecs. The app finds servers with /hello.
const { test, expect } = require('@playwright/test');

function tvBase(baseURL) {
  const u = new URL(baseURL || 'http://localhost:47155');
  u.port = String(Number(u.port) + 1);
  return u.origin;
}

test('/hello says this is a Newfoundsync server', async ({ page, baseURL }) => {
  const hello = await page.request.get(`${tvBase(baseURL)}/hello`);
  expect(hello.ok()).toBeTruthy();
  const h = await hello.json();
  expect(h.app).toBe('newfoundsync');
  expect(typeof h.name).toBe('string');
  expect('certSha256' in h && 'httpsPort' in h).toBeTruthy(); // null here: the test server is plain HTTP
});

test('the TV app page is hands-free but decodes itself (video too)', async ({ page, browserName }) => {
  test.skip(browserName !== 'chromium', 'the WebCodecs path is verified on Chromium');
  const sockets = [];
  page.on('websocket', (ws) => sockets.push(ws.url()));
  await page.goto('/?app=1&id=tv-e2e0002&name=Den%20TV');
  await expect(page.locator('html')).toHaveClass(/\bhandsfree\b/);
  await expect(page.locator('html')).toHaveClass(/\btv\b/);
  await expect(page.locator('#start')).toBeHidden();
  await expect(page.locator('#controls')).toBeHidden();
  await expect(page.locator('#namemodal')).toBeHidden();
  await expect(page.locator('#devline')).toContainText('Den TV');
  await expect(page.locator('#state')).not.toHaveText('idle', { timeout: 10_000 });
  expect(sockets.length, 'connected').toBeGreaterThan(0);
  expect(sockets.every((u) => !u.includes('pcm=1')), 'not the PCM connection').toBeTruthy();
  const native = await page.evaluate(() => String(AudioDecoder).includes('[native code]'));
  expect(native, 'the real WebCodecs decoder, not the PCM stand-in').toBeTruthy();
});

test('the sound-only page starts by itself and asks for PCM', async ({ page, baseURL, browserName }) => {
  test.skip(browserName !== 'chromium', 'the playback path is verified on Chromium');
  const sockets = [];
  page.on('websocket', (ws) => sockets.push(ws.url()));
  await page.goto(`${tvBase(baseURL)}/tv?app=1&id=tv-e2e0001&name=Den%20TV`);
  await expect(page.locator('html')).toHaveClass(/\bhandsfree\b/);
  await expect(page.locator('#start')).toBeHidden();
  await expect(page.locator('#controls')).toBeHidden();
  await expect(page.locator('#state')).not.toHaveText('idle', { timeout: 10_000 });
  await expect(page.locator('#devline')).toContainText('Den TV');
  expect(sockets.some((u) => u.includes('/ws?pcm=1')), `sockets: ${sockets.join(', ')}`).toBeTruthy();
  const ready = await page.evaluate(() => /** @type {any} */ (window).__NFS_APP_READY);
  expect(ready).toBeTruthy();
});

test('PCM from the server comes out as the floats onAudioData schedules', async ({ page, baseURL }) => {
  await page.goto(`${tvBase(baseURL)}/tv?app=1&id=tv-e2e0001&name=Den`);
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

test('the ordinary page is untouched by the TV modes', async ({ page }) => {
  await page.goto('/');
  await expect(page.locator('html')).not.toHaveClass(/\bhandsfree\b/);
  await expect(page.locator('#start')).toBeVisible();
  const native = await page.evaluate(() => typeof AudioDecoder !== 'function' || String(AudioDecoder).includes('[native code]'));
  expect(native, 'the real WebCodecs AudioDecoder, not the PCM stand-in').toBeTruthy();
});
