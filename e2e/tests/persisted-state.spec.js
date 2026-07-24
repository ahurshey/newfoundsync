// @ts-check
// Regression tests for the persisted-localStorage STARTUP CRASH — the real cause of "corruption after
// refresh" that the clean-profile specs missed. A saved non-zero `nfs_trim` makes loadTrim() assign
// `aligned` while that `let` is still in its temporal dead zone, throwing a ReferenceError that aborts
// app.js BEFORE the Start handler is wired → an inert Start button that only clearing site data (which
// also wipes nfs_trim) appears to "fix". These specs seed the persisted state the other tests never had.
const { test, expect } = require('@playwright/test');

async function health(page) {
  return await page.evaluate(async () => ({
    boot: /** @type {any} */ (window).__NFS_APP_BOOT || null,
    ready: /** @type {any} */ (window).__NFS_APP_READY || null,
    trimVal: document.getElementById('trimval')?.textContent || null,
    sw: 'serviceWorker' in navigator ? (await navigator.serviceWorker.getRegistrations()).length : 0,
    cache: self.caches ? (await caches.keys()).length : 0,
  }));
}

test.describe('persisted-state startup (nfs_trim TDZ regression)', () => {
  test('a saved non-zero nfs_trim does not crash startup; Start works across reloads (chromium)', async ({ page, browserName }) => {
    test.skip(browserName !== 'chromium', 'decode/Start path — chromium');
    const errors = [];
    page.on('pageerror', (e) => errors.push('pageerror: ' + e.message));
    page.on('console', (m) => { if (m.type() === 'error' && !/chrome-extension/.test(m.location()?.url || '')) errors.push(m.text()); });

    await page.addInitScript(() => {
      localStorage.setItem('nfs_trim', '10');
      localStorage.setItem('nfs_aligned', '1');
    });

    await page.goto('/', { waitUntil: 'load' });
    await expect(page.locator('#start'), 'Start gate should render').toBeVisible();
    expect(errors, 'startup must not throw with a persisted non-zero trim').toEqual([]);
    expect((await health(page)).ready, 'app.js must FINISH init (readiness stamp) with a persisted trim').toBeTruthy();

    // The load-bearing check: Start must actually work (its handler was wired, i.e. init didn't abort).
    await page.locator('#start').click();
    try { await page.locator('#nameskip').click({ timeout: 3000 }); } catch (e) { /* modal may not show */ }
    await expect(page.locator('#start'), 'Start did nothing → init aborted before wiring it').toBeHidden();
    await expect(page.locator('#state')).not.toHaveText('idle');

    // And it survives a reload with the trim still persisted (addInitScript re-seeds each navigation).
    await page.reload({ waitUntil: 'load' });
    await expect(page.locator('#start')).toBeVisible();
    expect((await health(page)).ready, 'readiness after reload').toBeTruthy();
    expect(errors, 'no errors after reload').toEqual([]);
  });

  test('a saved zero trim with nfs_aligned=1 starts cleanly (chromium)', async ({ page, browserName }) => {
    test.skip(browserName !== 'chromium', 'chromium');
    const errors = [];
    page.on('pageerror', (e) => errors.push(e.message));
    await page.addInitScript(() => { localStorage.setItem('nfs_trim', '0'); localStorage.setItem('nfs_aligned', '1'); });
    await page.goto('/', { waitUntil: 'load' });
    await expect(page.locator('#start')).toBeVisible();
    expect(errors, 'zero-trim + aligned must not throw').toEqual([]);
    expect((await health(page)).ready, 'readiness with zero-trim + aligned').toBeTruthy();
  });

  test('a corrupt persisted trim (Infinity) is normalized, not fatal (chromium)', async ({ page, browserName }) => {
    test.skip(browserName !== 'chromium', 'chromium');
    const errors = [];
    page.on('pageerror', (e) => errors.push(e.message));
    await page.addInitScript(() => { localStorage.setItem('nfs_trim', 'Infinity'); });
    await page.goto('/', { waitUntil: 'load' });
    await expect(page.locator('#start')).toBeVisible();
    const h = await health(page);
    expect(errors, 'corrupt trim must not throw').toEqual([]);
    expect(h.ready, 'readiness with corrupt trim').toBeTruthy();
    expect(h.trimVal || '', 'trim display must be finite, not Infinity').not.toMatch(/inf/i);
  });

  // Structural guard for the readiness sentinel. It only means "all top-level init completed" if it is
  // literally the LAST statement in app.js — it originally sat ~20% in (right after the Start handler),
  // so a crash in the other 80% (calibration, cast, the later listeners) still reported healthy and the
  // <head> watchdog never fired. Asserting position (not just presence) is what stops it drifting back
  // up the file the next time someone appends init.
  test('__NFS_APP_READY is the last statement in app.js (chromium)', async ({ page, browserName, baseURL }) => {
    test.skip(browserName !== 'chromium', 'source-level invariant — check once');
    const res = await page.request.get(new URL('/app.js', baseURL).href);
    expect(res.ok(), '/app.js should be served').toBeTruthy();
    const src = await res.text();

    // Strip blank lines and whole-line comments; whatever remains is executable top-level source.
    const code = src
      .split('\n')
      .map((l) => l.trim())
      .filter((l) => l && !l.startsWith('//'));
    expect(code.length, 'app.js should not be empty').toBeGreaterThan(100);

    const idx = code.findIndex((l) => l.startsWith('window.__NFS_APP_READY'));
    expect(idx, 'app.js must stamp window.__NFS_APP_READY').toBeGreaterThanOrEqual(0);
    expect(
      code.length - 1 - idx,
      `readiness stamp must be the LAST statement, but ${code.length - 1 - idx} statement(s) follow it — ` +
        'anything after it runs UNVERIFIED and a crash there would still report the client healthy',
    ).toBe(0);
  });

  test('?reset clears persisted trim/alignment + SW + caches (chromium)', async ({ page, browserName }) => {
    test.skip(browserName !== 'chromium', 'chromium');
    // Seed via a normal load (NOT addInitScript, which would re-seed after the reset's redirect).
    await page.goto('/', { waitUntil: 'load' });
    await page.evaluate(() => { localStorage.setItem('nfs_trim', '25'); localStorage.setItem('nfs_aligned', '1'); });
    await page.goto('/?reset', { waitUntil: 'load' });
    await page.waitForFunction(() => !location.search.toLowerCase().includes('reset'), null, { timeout: 10000 });
    await expect(page.locator('#start')).toBeVisible();
    const st = await page.evaluate(async () => ({
      trim: localStorage.getItem('nfs_trim'),
      aligned: localStorage.getItem('nfs_aligned'),
      sw: (await navigator.serviceWorker.getRegistrations()).length,
      cache: (await caches.keys()).length,
    }));
    expect(st.trim, 'nfs_trim cleared by ?reset').toBeNull();
    expect(st.aligned, 'nfs_aligned cleared by ?reset').toBeNull();
    expect(st.sw, 'no SW after reset').toBe(0);
    expect(st.cache, 'no cache after reset').toBe(0);
  });
});
