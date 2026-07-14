// @ts-check
// Reproduces the WORST-CASE stuck user: their browser has the OLD cache-first service worker, and that
// worker serves a STALE shell — an old index.html with no <head> self-heal watchdog AND an old app.js
// with no SW-cleanup and no boot stamp. So neither client-side recovery path runs; the ONLY thing that
// can save them is the shipped self-destruct /sw.js, fetched by the browser's SW update check. This
// test plants exactly that state and asserts the self-destruct alone unregisters the worker, clears its
// caches, and recovers the page to the current no-SW client.
//
// If this FAILS, the upgrade path is genuinely broken (that's the user's bug). If it PASSES, any device
// that actually loads the current (0.0.2+) server heals on its own — so a still-stuck user is loading an
// OLD server binary (one that still serves the old caching /sw.js), not the current one.
const { test, expect } = require('@playwright/test');

// Stale shell the legacy worker will serve from cache — deliberately WITHOUT any recovery code.
const STALE_INDEX =
  '<!doctype html><html><head><meta charset=utf-8><title>OLD</title>' +
  '<script>window.__NFS_STALE_INDEX=true;</script></head>' +
  '<body><div id="oldshell">stale shell</div><script src="/app.js"></script></body></html>';
const STALE_APP_JS = 'window.__NFS_STALE_APP=true; /* old app.js: no SW cleanup, no __NFS_APP_BOOT stamp */';

// A stand-in for the removed worker: cache-first for the shell, caches a STALE index + app.js, claims
// clients — i.e. exactly "serves a stale shell that a plain reload cannot escape".
const LEGACY_SW =
  'self.addEventListener("install", (e) => { e.waitUntil((async () => {' +
  '  const c = await caches.open("nfs-shell-legacy");' +
  '  await c.put("/", new Response(' + JSON.stringify(STALE_INDEX) + ', { headers: { "content-type": "text/html" } }));' +
  '  await c.put("/app.js", new Response(' + JSON.stringify(STALE_APP_JS) + ', { headers: { "content-type": "text/javascript" } }));' +
  '  await self.skipWaiting();' +
  '})()); });' +
  'self.addEventListener("activate", (e) => e.waitUntil(self.clients.claim()));' +
  'self.addEventListener("fetch", (e) => {' +
  '  const u = new URL(e.request.url);' +
  '  if (e.request.mode === "navigate" || u.pathname === "/app.js") {' +
  '    e.respondWith(caches.match(e.request).then((hit) => hit || fetch(e.request)));' +
  '  }' +
  '});';

test('worst-case stale legacy SW self-heals via the shipped self-destruct /sw.js (chromium)', async ({ page, context, browserName }) => {
  test.skip(browserName !== 'chromium', 'SW migration path — verify on chromium');

  // 1. Serve the fake legacy SW at /sw.js only while we plant it.
  const fakeSw = (route) => route.fulfill({ contentType: 'text/javascript; charset=utf-8', body: LEGACY_SW });
  await context.route('**/sw.js', fakeSw);

  await page.goto('/', { waitUntil: 'load' });
  await page.evaluate(async () => {
    await navigator.serviceWorker.register('/sw.js');
    await navigator.serviceWorker.ready;
  });
  await page.reload({ waitUntil: 'load' });

  // Confirm the stuck state: controlled by the legacy SW, serving the STALE shell (no recovery code).
  const before = await page.evaluate(async () => ({
    sw: (await navigator.serviceWorker.getRegistrations()).length,
    controlled: !!navigator.serviceWorker.controller,
    staleIndex: /** @type {any} */ (window).__NFS_STALE_INDEX === true,
    staleApp: /** @type {any} */ (window).__NFS_STALE_APP === true,
    boot: /** @type {any} */ (window).__NFS_APP_BOOT,
  }));
  expect(before.controlled, 'page should be controlled by the legacy SW').toBeTruthy();
  expect(before.staleIndex, 'legacy SW should be serving the stale index').toBeTruthy();
  expect(before.staleApp, 'legacy SW should be serving the stale app.js').toBeTruthy();
  expect(before.boot, 'stale app.js must NOT stamp the boot marker').toBeFalsy();

  // 2. Stop faking — the REAL shipped /sw.js (self-destruct stub) is served from now on.
  await context.unroute('**/sw.js', fakeSw);

  // 3. Reloads: the browser's SW update check fetches the real /sw.js (bypasses the SW fetch handler),
  //    installs the self-destruct, which clears caches + navigates clients + unregisters. Give it time.
  for (let i = 0; i < 4; i++) {
    await page.reload({ waitUntil: 'load' });
    await page.waitForTimeout(1200);
    const healed = await page.evaluate(async () => (await navigator.serviceWorker.getRegistrations()).length === 0);
    if (healed) break;
  }

  // 4. Recovered: no SW, empty Cache Storage, the CURRENT client (fresh index + booted app.js).
  const after = await page.evaluate(async () => ({
    sw: (await navigator.serviceWorker.getRegistrations()).length,
    controlled: !!navigator.serviceWorker.controller,
    caches: await caches.keys(),
    staleIndex: /** @type {any} */ (window).__NFS_STALE_INDEX === true,
    boot: /** @type {any} */ (window).__NFS_APP_BOOT,
  }));
  expect(after.sw, 'self-destruct should have unregistered the legacy SW').toBe(0);
  expect(after.controlled, 'page should no longer be SW-controlled').toBeFalsy();
  expect(after.caches, 'caches should be cleared').toEqual([]);
  expect(after.staleIndex, 'should no longer be serving the stale index').toBeFalsy();
  expect(after.boot, 'current app.js should boot after recovery').toBeTruthy();
  await expect(page.locator('#start')).toBeVisible();
});
