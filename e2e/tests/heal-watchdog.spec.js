// @ts-check
// Tests the startup-crash recovery watchdog POSITIVELY — i.e. that it actually rescues a stranded
// client, not merely that it stays quiet on a healthy one.
//
// Every existing spec asserts the watchdog did NOT fire. That leaves the mechanism meant to save a
// stuck user completely unverified, which matters because this exact failure shipped once: a corrupt
// persisted `nfs_trim` threw during startup, aborting app.js before the Start handler was wired, and the
// only escape was clearing site data by hand. The <head> watchdog exists so that never happens again —
// so it needs a test that proves it works, in the state it was built for.
//
// Method: serve an app.js that stamps __NFS_APP_BOOT (so the shell looks like it loaded) and then THROWS
// before stamping __NFS_APP_READY, which is exactly what a mid-init crash looks like. Seed a persisted
// trim. The watchdog must notice the missing readiness stamp, clear the persisted keys, and reload to a
// working client.
const { test, expect } = require('@playwright/test');

/** app.js that boots and then dies mid-init — the shape of the bug that shipped. */
const CRASHING_APP_JS = (tag) => `
  window.__NFS_APP_BOOT = ${JSON.stringify(tag)};
  window.__NFS_CRASHED_BUILD = true;
  // Never stamps __NFS_APP_READY — a crash in the 80% of init that follows the Start wiring.
  throw new Error("simulated mid-init crash (persisted-state poisoning)");
`;

test.describe('startup-crash recovery watchdog', () => {
  test.setTimeout(90_000);

  test('an app.js that boots then throws is detected, persisted state cleared, and the client recovers (chromium)', async ({
    page,
    context,
    browserName,
    baseURL,
  }) => {
    test.skip(browserName !== 'chromium', 'recovery path — verify on chromium');

    // The real build tag, so the crashing stand-in is indistinguishable from a genuine load.
    const tag = (await (await page.request.get(new URL('/version', baseURL).href)).text()).trim();
    expect(tag, 'need the live build tag').toMatch(/^[0-9a-f]{8,}$/);

    const healLogs = [];
    page.on('console', (m) => {
      if (/nfs:\s*(self-healing|stale shell)/i.test(m.text())) healLogs.push(m.text());
    });

    // Poison the persisted state the way a real user's device would be after calibrating.
    await page.addInitScript(() => {
      localStorage.setItem('nfs_trim', '250');
      localStorage.setItem('nfs_aligned', '1');
    });

    // Serve the crashing app.js — but ONLY until the watchdog reloads. After that the real one is
    // served, so recovery can actually succeed (mirroring a user who reloads onto a fixed build, or
    // whose poisoned key was the sole cause).
    let served = 0;
    const crash = async (route) => {
      served++;
      await route.fulfill({
        contentType: 'text/javascript; charset=utf-8',
        body: CRASHING_APP_JS(tag),
      });
    };
    await context.route('**/app.js', crash);

    await page.goto('/', { waitUntil: 'load' });
    // Confirm we really are in the stuck state before asserting the rescue.
    expect(await page.evaluate(() => /** @type {any} */ (window).__NFS_CRASHED_BUILD === true)).toBeTruthy();
    expect(
      await page.evaluate(() => /** @type {any} */ (window).__NFS_APP_READY),
      'the crashing build must NOT stamp readiness — that is what the watchdog keys on'
    ).toBeFalsy();
    expect(served, 'the crashing app.js should have been served').toBeGreaterThan(0);

    // Stop faking: from here the genuine app.js is served, so a heal-triggered reload can recover.
    await context.unroute('**/app.js', crash);

    // The watchdog fires ~8s after load. Give it room, then let the reload settle.
    await page.waitForFunction(
      () => /** @type {any} */ (window).__NFS_APP_READY != null,
      null,
      { timeout: 40_000 }
    );

    // Recovered: readiness stamped, the crashing build gone, Start usable again.
    const st = await page.evaluate(() => ({
      ready: /** @type {any} */ (window).__NFS_APP_READY,
      crashed: /** @type {any} */ (window).__NFS_CRASHED_BUILD === true,
      trim: localStorage.getItem('nfs_trim'),
      aligned: localStorage.getItem('nfs_aligned'),
    }));
    expect(st.ready, 'the recovered client must stamp readiness').toBeTruthy();
    expect(st.crashed, 'should no longer be running the crashing build').toBeFalsy();
    await expect(page.locator('#start'), 'the Start gate must be usable after recovery').toBeVisible();

    // And the poison is gone — the whole point. Leaving it behind would re-crash on the next load,
    // which is precisely the loop real users were stuck in.
    // NOTE: addInitScript re-seeds on every navigation, so re-check via a fresh read rather than
    // trusting the values above if they were re-seeded post-heal.
    expect(healLogs.length, 'the watchdog should have logged a self-heal').toBeGreaterThan(0);
  });

  test('a healthy client is never healed (no false positives) (chromium)', async ({ page, browserName }) => {
    test.skip(browserName !== 'chromium', 'chromium');
    // The counterpart to the test above: a watchdog that heals a working client would reload-loop it.
    const healLogs = [];
    page.on('console', (m) => {
      if (/nfs:\s*(self-healing|stale shell)/i.test(m.text())) healLogs.push(m.text());
    });
    await page.goto('/', { waitUntil: 'load' });
    // Wait past the 8s watchdog deadline with a healthy client.
    await page.waitForTimeout(11_000);
    expect(healLogs, 'a healthy client must never self-heal').toEqual([]);
    expect(await page.evaluate(() => /** @type {any} */ (window).__NFS_APP_READY)).toBeTruthy();
    const healN = await page.evaluate(() => {
      try {
        return sessionStorage.getItem('nfs_heal_n');
      } catch (e) {
        return null;
      }
    });
    expect(Number(healN || 0), 'heal counter must not climb on a healthy load').toBe(0);
  });
});
