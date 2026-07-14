// @ts-check
// Aggressive reproduction of the "cache corruption after refreshing many times" report. The basic
// reload test always waits for a full load, so it never covers the patterns that a human F5-spamming
// actually produces: RAPID reloads that interrupt in-flight loads, and reload-DURING-load. This
// hammers 90 refreshes across three patterns and is instrumented so a failure shows exactly what
// broke — a half-broken shell, a self-heal reload loop, nfs_heal_n accumulation, or console errors.
const { test, expect } = require('@playwright/test');

test.describe('reload-storm stress', () => {
  test.setTimeout(180_000);

  test('heavy + rapid + interrupting reloads leave a clean, booted shell (chromium)', async ({ page, browserName }) => {
    test.skip(browserName !== 'chromium', 'stress the primary engine');

    const errors = [];
    const healLogs = [];
    page.on('console', (m) => {
      const t = m.text();
      if (m.type() === 'error') errors.push(t);
      if (/nfs:\s*(self-healing|stale shell)/i.test(t)) healLogs.push(t);
    });
    page.on('pageerror', (e) => errors.push('pageerror: ' + e.message));

    await page.goto('/', { waitUntil: 'load' });

    // (A) 40 clean reloads (baseline — this is what the existing test does, just more of it).
    for (let i = 1; i <= 40; i++) {
      await page.reload({ waitUntil: 'load' });
      await expect(page.locator('#start'), `clean reload #${i}: shell half-broken`).toBeVisible();
      expect(await page.evaluate(() => /** @type {any} */ (window).__NFS_APP_BOOT), `clean reload #${i}: app.js didn't boot`).toBeTruthy();
    }

    // (B) 30 RAPID reloads — commit only, don't wait for load; interrupts loads like F5-spam.
    for (let i = 1; i <= 30; i++) {
      await page.reload({ waitUntil: 'commit' });
      await page.waitForTimeout(25 + (i % 7) * 10);
    }

    // (C) 20 reload-DURING-load — fire a reload, interrupt it with another before it settles.
    for (let i = 1; i <= 20; i++) {
      page.reload({ waitUntil: 'load' }).catch(() => {});
      await page.waitForTimeout(50 + (i % 5) * 10);
      await page.reload({ waitUntil: 'load' }).catch(() => {});
    }

    // Settle, then assert a fully-healthy, clean shell.
    await page.goto('/', { waitUntil: 'load' });
    await expect(page.locator('#start'), 'FINAL: Start gate missing → corrupted shell').toBeVisible();
    await expect(page.locator('#state'), 'FINAL: state should read idle before Start').toHaveText('idle');
    expect(await page.evaluate(() => /** @type {any} */ (window).__NFS_APP_BOOT), 'FINAL: app.js did not boot').toBeTruthy();

    const swCount = await page.evaluate(async () =>
      'serviceWorker' in navigator ? (await navigator.serviceWorker.getRegistrations()).length : 0);
    expect(swCount, 'FINAL: a service worker exists (should be zero)').toBe(0);

    const cacheKeys = await page.evaluate(async () => (self.caches ? await caches.keys() : []));
    expect(cacheKeys, 'FINAL: Cache Storage is not empty').toEqual([]);

    // If the <head> watchdog is spuriously healing, it logs + climbs nfs_heal_n toward the cap and
    // then strands the page. Neither should happen on a healthy server.
    const healN = await page.evaluate(() => { try { return sessionStorage.getItem('nfs_heal_n'); } catch (e) { return null; } });
    expect(healLogs, `FINAL: self-heal fired ${healLogs.length}x during the storm — reload/heal loop`).toEqual([]);
    expect(Number(healN || 0), 'FINAL: nfs_heal_n accumulated (heal loop / stranding)').toBeLessThan(2);

    // Start must still work after the storm.
    await page.locator('#start').click();
    try { await page.locator('#nameskip').click({ timeout: 3000 }); } catch (e) { /* modal may not show */ }
    await expect(page.locator('#start'), 'FINAL: Start did nothing after the storm').toBeHidden();

    expect(errors, 'FINAL: console/page errors during the storm').toEqual([]);
  });
});
