// @ts-check
// THE regression test for the bug that plagued this project: after N browser refreshes the client
// would get stuck on a half-broken shell (or the Start screen) that only clearing site data /
// incognito could fix. Root cause was a service worker caching a skewed/stale shell; the fix removed
// the service worker entirely so the page is always fetched fresh.
//
// This drives the real client and hammers reload — the kind of full-loop integration check that a
// Rust unit test cannot express — and asserts the invariants that make the bug impossible:
//   * every reload fully boots (Start screen renders AND app.js executed), and
//   * no service worker is ever registered, and
//   * Cache Storage stays empty, and
//   * no console errors along the way.
const { test, expect } = require('@playwright/test');

const RELOADS = 15; // the failures were seen "every 2", then "every ~10" refreshes — 15 covers it

test('shell survives many reloads: always boots, no service worker, no cache, no errors', async ({ page }) => {
  const errors = [];
  page.on('console', (m) => {
    if (m.type() === 'error') errors.push(m.text());
  });
  page.on('pageerror', (e) => errors.push('pageerror: ' + e.message));

  await page.goto('/');

  for (let i = 1; i <= RELOADS; i++) {
    await page.reload({ waitUntil: 'load' });
    // The Start gate must render on every reload — if it's missing, the shell came up half-broken.
    await expect(page.locator('#start'), `reload #${i}: Start button missing (half-broken shell)`).toBeVisible();
    // ...and app.js must have actually executed (its very first line stamps this).
    const boot = await page.evaluate(() => /** @type {any} */ (window).__NFS_APP_BOOT);
    expect(boot, `reload #${i}: app.js did not boot (stamp missing)`).toBeTruthy();
  }

  // Post-service-worker-removal invariants: nothing may ever register a worker or cache the shell.
  const swCount = await page.evaluate(async () =>
    'serviceWorker' in navigator ? (await navigator.serviceWorker.getRegistrations()).length : 0
  );
  expect(swCount, 'a service worker got registered — must stay zero after removal').toBe(0);

  const cacheKeys = await page.evaluate(async () => (self.caches ? await caches.keys() : []));
  expect(cacheKeys, 'Cache Storage is not empty — the shell must never be cached').toEqual([]);

  expect(errors, 'console/page errors during the reload loop').toEqual([]);
});
