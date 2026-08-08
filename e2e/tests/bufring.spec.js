// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Alex Hurshman and the Newfoundsync contributors.
//
// Buffering-ring tests. These drive the REAL index.html + app.js over a static file server rather
// than the Rust binary: everything under test here is client-side (canvas painting, button state,
// class toggling), so booting the server would add minutes and prove nothing extra.
//
// app.js is served verbatim with a short shim appended. Top-level `let`/`const` in a classic script
// are NOT properties of window, so the shim re-exports the two state objects and adds a setter for
// the playback vars the ring reads. That setter is what lets these tests drive the REAL bufTick
// (its own frac maths, its own countdown label, its own indeterminate fallback) instead of poking
// BUF.frac and painting a frame the shipped code would never have produced. Function declarations
// (showBuffering, bufDraw, …) are already on window, so everything else here is the shipped code.

const { test, expect } = require("@playwright/test");
const http = require("http");
const fs = require("fs");
const path = require("path");

const WEB = path.join(__dirname, "..", "..", "crates", "desktop", "web");
const ICON = path.join(__dirname, "..", "..", "branding", "icon-256.png");

// Appended to the served app.js. Assignments live in the same script scope as the originals, so
// they set the real module-scope bindings the shipped code reads.
const SHIM = `
window.__TEST_BUF = BUF;
window.__TEST_VIZ = VIZ;
window.__TEST_SET = function (o) {
  if ("started" in o) started = o.started;
  if ("ac" in o) ac = o.ac;
  if ("offsetNs" in o) offsetNs = o.offsetNs;
  if ("firstPlayoutAc" in o) firstPlayoutAc = o.firstPlayoutAc;
  if ("bufferMs" in o) bufferMs = o.bufferMs;
  if ("analyser" in o) analyser = o.analyser;
  if ("aPlayable" in o) aPlayable = o.aPlayable;
};
window.__TEST_GET = function () { return { everPlayed: everPlayed, aPlayable: aPlayable }; };
// A stand-in AnalyserNode so vizStart() gets past its \`!analyser\` guard and the canvas hand-off
// can actually be observed. Real frequency data is irrelevant here — occupancy of the rAF slot is.
window.__TEST_ANALYSER = { frequencyBinCount: 64, getByteFrequencyData: function (a) { a.fill(90); } };
`;

let server, base;

test.beforeAll(async () => {
  server = http.createServer((req, res) => {
    const url = req.url.split("?")[0];
    const send = (body, type) => { res.writeHead(200, { "content-type": type }); res.end(body); };
    if (url === "/version") return send("test", "text/plain");
    if (url === "/" || url === "/index.html") {
      return send(fs.readFileSync(path.join(WEB, "index.html"), "utf8").replace(/__NFS_BUILD__/g, "test"), "text/html");
    }
    if (url === "/app.js") {
      // The build token must be substituted here TOO, not just in index.html: the shell's <head>
      // watchdog compares index's tag, /version and app.js's own stamp, and any mismatch makes it
      // unregister + reload as a "stale shell" — an endless heal loop instead of a running client.
      const js = fs.readFileSync(path.join(WEB, "app.js"), "utf8").replace(/__NFS_BUILD__/g, "test");
      return send(js + SHIM, "text/javascript");
    }
    if (url === "/nfs-dsp.js") return send(fs.readFileSync(path.join(WEB, "nfs-dsp.js")), "text/javascript");
    if (url === "/manifest.webmanifest") return send(fs.readFileSync(path.join(WEB, "manifest.webmanifest")), "application/manifest+json");
    if (url === "/icon-256.png" || url === "/favicon.png") return send(fs.readFileSync(ICON), "image/png");
    res.writeHead(404); res.end("nope");
  });
  await new Promise((r) => server.listen(0, "127.0.0.1", r));
  base = "http://127.0.0.1:" + server.address().port + "/";
});

test.afterAll(async () => { await new Promise((r) => server.close(r)); });

/** Load the client and reveal the logo block (start() normally does this; we skip the connect). */
async function openClient(page, { vizOff = false } = {}) {
  const errors = [];
  page.on("pageerror", (e) => errors.push(String(e)));
  if (vizOff) await page.addInitScript(() => { try { localStorage.setItem("nfs_viz", "0"); } catch (e) {} });
  await page.goto(base);
  // app.js sets __NFS_APP_READY only after init FINISHES — the stamp the shell's self-heal uses. If a
  // top-level `const` were read before its declaration (temporal dead zone) this would never appear.
  await page.waitForFunction(() => window.__NFS_APP_READY === "test", null, { timeout: 10000 });
  await page.evaluate(() => { document.getElementById("vlogo").style.display = "flex"; });
  return errors;
}

/** Bright-pixel stats for the ring canvas: how much is lit, and where its centre of mass sits. */
async function ringStats(page) {
  return page.evaluate(() => {
    const c = document.getElementById("viz");
    const d = c.getContext("2d").getImageData(0, 0, c.width, c.height).data;
    let lit = 0, sx = 0, sy = 0, sum = 0;
    for (let i = 0; i < d.length; i += 4) {
      sum += d[i + 3]; // total alpha — a cheap signature of the whole frame
      if (d[i + 3] < 140) continue; // ignore the faint "unlit bar" stubs
      const p = i / 4;
      lit++; sx += p % c.width; sy += Math.floor(p / c.width);
    }
    // cx/cy are null — NOT 0 — when nothing is lit. 0 is a legitimate centroid (a full ring is
    // centred), so returning it for "blank" let a canvas that went dark read as a large movement
    // and pass the "the comet moved" assertion. Null forces callers to check `lit` first.
    return { lit, sum, cx: lit ? sx / lit - c.width / 2 : null, cy: lit ? sy / lit - c.height / 2 : null };
  });
}

/** Distance the lit centroid travelled; throws if either sample was blank. */
function moved(a, b) {
  expect(a.lit, "first sample must have lit pixels").toBeGreaterThan(0);
  expect(b.lit, "second sample must have lit pixels").toBeGreaterThan(0);
  return Math.hypot(a.cx - b.cx, a.cy - b.cy);
}

/**
 * Assert the ring is genuinely animating. Centroid movement alone is too weak early in the connect
 * phase, when only a handful of bars are lit and the wake barely shifts the centre of mass — so
 * compare the whole-frame alpha signature as well, and require both samples to be non-blank.
 */
function assertAnimating(a, b) {
  expect(a.lit, "first sample must have lit pixels").toBeGreaterThan(0);
  expect(b.lit, "second sample must have lit pixels").toBeGreaterThan(0);
  const centroidShift = Math.hypot(a.cx - b.cx, a.cy - b.cy);
  const frameDelta = Math.abs(a.sum - b.sum) / Math.max(1, a.sum);
  expect(
    centroidShift > 0.5 || frameDelta > 0.01,
    `ring appears static (centroid moved ${centroidShift.toFixed(2)}px, frame changed ${(frameDelta * 100).toFixed(2)}%)`
  ).toBe(true);
}

test("app.js finishes init — no temporal-dead-zone crash from the ring state", async ({ page }) => {
  const errors = await openClient(page);
  expect(errors).toEqual([]);
});

test("connecting: ring spins, button becomes an inert 'Connecting…' label", async ({ page }) => {
  await openClient(page);
  await page.evaluate(() => showBuffering(true, "Connecting…"));
  const btn = page.locator("#viztoggle");
  await expect(btn).toHaveText("Connecting…");
  await expect(btn).toBeDisabled();
  await expect(btn).toHaveClass(/busy/);
  await expect(page.locator("#vlogo")).toHaveClass(/buffering/);

  // Let the creep open up an arc first. Sampling immediately after activation is not a fair test of
  // motion: the fill is still near zero, so one or two bars carry the wake and the other 54 dim
  // stubs swamp the frame signature — which is exactly how this assertion went flaky.
  await page.waitForTimeout(500);

  // The wake must be visibly moving. assertAnimating insists BOTH samples are non-blank, so a ring
  // that simply died cannot satisfy it by centroid distance alone.
  const a = await ringStats(page);
  await page.waitForTimeout(350);
  const b = await ringStats(page);
  assertAnimating(a, b);
  // …and it stays alive rather than fading out over time.
  await page.waitForTimeout(500);
  expect((await ringStats(page)).lit).toBeGreaterThan(0);
});

// Connecting has no known duration, so its share of the circle is capped and merely approached.
test("the connect creep advances but can never exceed its quarter of the ring", async ({ page }) => {
  await openClient(page);
  await page.evaluate(() => showBuffering(true, "Connecting…"));

  const samples = [];
  for (let i = 0; i < 5; i++) {
    await page.waitForTimeout(400);
    samples.push(await page.evaluate(() => window.__TEST_BUF.frac));
  }
  // Strictly forward…
  for (let i = 1; i < samples.length; i++) expect(samples[i]).toBeGreaterThan(samples[i - 1]);
  // …and decelerating, never reaching the quarter mark it eases toward.
  for (const f of samples) expect(f).toBeLessThan(0.25);
  expect(samples[samples.length - 1]).toBeGreaterThan(0.15); // but it did get most of the way
  // Later steps are smaller than earlier ones — that is the ease, not a linear crawl.
  expect(samples[4] - samples[3]).toBeLessThan(samples[1] - samples[0]);
});

/**
 * Put the client in the state bufTick treats as "the countdown is known": clock anchored, and the
 * first audio scheduled `remain` seconds from now. A frozen fake AudioContext means bufTick derives
 * frac and the label itself — nothing here writes BUF.frac.
 */
async function setCountdown(page, remain, total = 3) {
  await page.evaluate(
    ([r, t]) => {
      window.__TEST_SET({
        started: true,
        offsetNs: 0,
        bufferMs: t * 1000,
        ac: { currentTime: 0 },
        firstPlayoutAc: r,
      });
    },
    [remain, total]
  );
  // One rAF for bufTick to observe the new state, one more for the paint it triggers.
  await page.evaluate(() => new Promise((r) => requestAnimationFrame(() => requestAnimationFrame(r))));
}

test("determinate: bufTick derives the fill and the countdown, clockwise from twelve o'clock", async ({ page }) => {
  await openClient(page);
  await page.evaluate(() => showBuffering(false, "Buffering…"));
  // Let the connect creep actually travel before the countdown lands — otherwise the countdown is
  // known on the very first frame, hand-off is legitimately 0, and this proves nothing about the
  // two phases joining up.
  await page.waitForTimeout(300);

  const q = [];
  // remain 3s..0s over a 3s buffer ⇒ progress 0, .25, .5, .75, 1 — computed by the SHIPPED code.
  for (const remain of [3, 2.25, 1.5, 0.75, 0]) {
    await setCountdown(page, remain);
    const frac = await page.evaluate(() => window.__TEST_BUF.frac);
    const label = await page.locator("#viztoggle").textContent();
    q.push({ ...(await ringStats(page)), frac, label });
  }

  // The buffer maps onto what the connect creep LEFT, so the two phases are one sweep: the fill
  // starts at the hand-off point rather than at zero, and still finishes at exactly full.
  const handoff = await page.evaluate(() => window.__TEST_BUF.handoff);
  expect(handoff).toBeGreaterThan(0); // the creep really did move before the countdown arrived
  [0, 0.25, 0.5, 0.75, 1].forEach((p, i) => {
    expect(q[i].frac).toBeCloseTo(handoff + (1 - handoff) * p, 3);
  });
  expect(q[0].frac).toBeCloseTo(handoff, 3);
  expect(q[4].frac).toBeCloseTo(1, 3);
  // …and the label is the remaining wait, to one decimal.
  expect(q.map((x) => x.label)).toEqual([
    "Buffering… 3.0s", "Buffering… 2.3s", "Buffering… 1.5s", "Buffering… 0.8s", "Buffering… 0.0s",
  ]);
  expect(await page.evaluate(() => window.__TEST_BUF.det)).toBe(true);

  // More buffer filled ⇒ strictly more lit pixels.
  for (let i = 1; i < q.length; i++) expect(q[i].lit).toBeGreaterThan(q[i - 1].lit);

  // Direction: a quarter full ⇒ mass in the TOP-RIGHT quadrant (canvas y grows downward, so up is
  // negative). Half full ⇒ mass swung to the right. Full ⇒ centred again.
  expect(q[1].cx).toBeGreaterThan(8);
  expect(q[1].cy).toBeLessThan(-8);
  expect(q[2].cx).toBeGreaterThan(20);
  expect(Math.abs(q[4].cx)).toBeLessThan(6);
  expect(Math.abs(q[4].cy)).toBeLessThan(6);
});

// A dropped socket runs teardownConnection(), which nulls the countdown AND clears the stats timer
// that is showBuffering's only periodic caller — so bufTick has to notice on its own, or the ring
// sits under a countdown label that stopped ticking for the whole backoff (~19 s once it caps).
// The chosen behaviour is HOLD: keep the fill, keep the wake running, say "Reconnecting…".
test("a dropped connection holds the fill and keeps the wake alive, never going backwards", async ({ page }) => {
  await openClient(page);
  await page.evaluate(() => showBuffering(false, "Buffering…"));
  await setCountdown(page, 1.5);
  const held = await page.evaluate(() => window.__TEST_BUF.frac);
  expect(held).toBeGreaterThan(0.4);

  // Socket drops. No showBuffering() call follows — nothing is left to make one.
  await page.evaluate(() => window.__TEST_SET({ offsetNs: null, firstPlayoutAc: null }));
  await page.waitForTimeout(400);

  expect(await page.evaluate(() => window.__TEST_BUF.det)).toBe(false);
  await expect(page.locator("#viztoggle")).toHaveText("Reconnecting…");
  // Held, not reset: the connect ease computes far below this, and the monotonic clamp wins.
  expect(await page.evaluate(() => window.__TEST_BUF.frac)).toBeCloseTo(held, 5);

  // Still alive — the wake is what says so while the fill is deliberately frozen.
  const a = await ringStats(page);
  await page.waitForTimeout(350);
  assertAnimating(a, await ringStats(page));

  // Reconnected: a FRESH countdown must carry on forward from the held position, not restart at 0.
  await setCountdown(page, 3);
  expect(await page.evaluate(() => window.__TEST_BUF.handoff)).toBeCloseTo(held, 5);
  expect(await page.evaluate(() => window.__TEST_BUF.frac)).toBeCloseTo(held, 5);
  await setCountdown(page, 0);
  expect(await page.evaluate(() => window.__TEST_BUF.frac)).toBeCloseTo(1, 3);
});

// Reduced motion keeps the fill (real progress may move) and drops only the ripple.
test("prefers-reduced-motion: the fill still advances, the wake does not animate", async ({ page }) => {
  await page.emulateMedia({ reducedMotion: "reduce" }); // must precede goto — read once at load
  await openClient(page);
  await page.evaluate(() => showBuffering(false, "Buffering…"));
  await setCountdown(page, 1.5);

  // Frozen countdown ⇒ frozen fill ⇒ byte-identical frames, no ripple.
  const a = await ringStats(page);
  await page.waitForTimeout(400);
  const b = await ringStats(page);
  expect(b.sum).toBe(a.sum);
  expect(b.lit).toBe(a.lit);

  // But progress itself is not suppressed.
  await setCountdown(page, 0.3);
  const c = await ringStats(page);
  expect(c.lit).toBeGreaterThan(a.lit);
});

// Canvas ownership is the core mechanic: two rAF loops on one canvas would flicker. With a real
// `analyser` present, vizStart() is no longer a no-op, so the hand-off can be observed.
test("the ring and the visualizer never own the canvas at the same time", async ({ page }) => {
  await openClient(page);
  await page.evaluate(() => window.__TEST_SET({ analyser: window.__TEST_ANALYSER }));

  // Visualizer running first.
  await page.evaluate(() => vizStart());
  expect(await page.evaluate(() => window.__TEST_VIZ.raf)).not.toBe(0);

  // Ring takes over: the visualizer loop must be stopped, not merely ignored.
  await page.evaluate(() => showBuffering(true, "Connecting…"));
  await page.evaluate(() => new Promise((r) => requestAnimationFrame(r)));
  expect(await page.evaluate(() => window.__TEST_VIZ.raf)).toBe(0);
  expect(await page.evaluate(() => window.__TEST_BUF.raf)).not.toBe(0);

  // vizStart() must refuse to restart while the ring holds the canvas.
  await page.evaluate(() => vizStart());
  expect(await page.evaluate(() => window.__TEST_VIZ.raf)).toBe(0);

  // Done: the ring stops and hands back.
  await page.evaluate(() => showBuffering(null));
  await page.evaluate(() => new Promise((r) => requestAnimationFrame(r)));
  expect(await page.evaluate(() => window.__TEST_BUF.raf)).toBe(0);
  expect(await page.evaluate(() => window.__TEST_BUF.active)).toBe(false);
  expect(await page.evaluate(() => window.__TEST_VIZ.raf)).not.toBe(0);
});

test("finishing hands the canvas back and restores the Visualizer button", async ({ page }) => {
  await openClient(page);
  await page.evaluate(() => showBuffering(true, "Connecting…"));
  await expect(page.locator("#viztoggle")).toHaveClass(/busy/);

  await page.evaluate(() => showBuffering(null));
  const btn = page.locator("#viztoggle");
  await expect(btn).toHaveText("Visualizer: On");
  await expect(btn).toBeEnabled();
  await expect(btn).not.toHaveClass(/busy/);
  await expect(page.locator("#vlogo")).not.toHaveClass(/buffering/);
});

// `everPlayed` gates every showBuffering() call site, so latching it early permanently kills the
// ring for the session. Connecting to a relay with nobody casting reaches updateStats' "playing"
// branch about half a second after Start, with firstPlayoutAc still null and nothing ever heard.
test("the ring is not suppressed before any audio has actually played", async ({ page }) => {
  await openClient(page);
  await page.evaluate(() =>
    window.__TEST_SET({ started: true, offsetNs: 0, ac: { currentTime: 0 }, firstPlayoutAc: null, aPlayable: 0 })
  );
  await page.evaluate(() => updateStats());
  expect(await page.evaluate(() => window.__TEST_GET().everPlayed)).toBe(false);

  // Once audio genuinely sounds, it latches — re-buffers after that stay quiet, as intended.
  await page.evaluate(() => window.__TEST_SET({ aPlayable: 5 }));
  await page.evaluate(() => updateStats());
  expect(await page.evaluate(() => window.__TEST_GET().everPlayed)).toBe(true);
});

test("restore is idempotent, and the pop animation does not leak into the next connect", async ({ page }) => {
  await openClient(page);
  await page.evaluate(() => showBuffering(true, "Connecting…"));
  await page.evaluate(() => showBuffering(null));
  const btn = page.locator("#viztoggle");
  await expect(btn).toHaveClass(/restored/);

  // updateStats calls showBuffering(null) on every tick during playback. The idle guard must make
  // repeats no-ops, or the pop animation would re-fire ~twice a second for the whole session.
  const before = await btn.evaluate((e) => getComputedStyle(e).animationName);
  await page.waitForTimeout(450); // let vizpop finish
  await page.evaluate(() => { showBuffering(null); showBuffering(null); });
  expect(await page.evaluate(() => window.__TEST_BUF.active)).toBe(false);
  expect(before).toBe("vizpop");

  // Next connect: `restored` must be gone. #vlogo is hidden and re-shown across Stop/Start, and
  // display:none -> display:flex RESTARTS css animations — so a leftover class would pop the
  // "Connecting…" label.
  await page.evaluate(() => showBuffering(true, "Connecting…"));
  await expect(btn).not.toHaveClass(/restored/);
  await expect(btn).toHaveClass(/busy/);
  await expect(btn).toHaveText("Connecting…");
});

// The regression this design invites: #vlogo.viz-off hides the ring's own canvas, so a user who
// had switched the visualizer off would have got NO loading indicator at all.
test("ring is visible even when the visualizer is toggled off", async ({ page }) => {
  await openClient(page, { vizOff: true });
  await expect(page.locator("#vlogo")).toHaveClass(/viz-off/);
  await expect(page.locator("#viz")).toBeHidden();

  await page.evaluate(() => showBuffering(true, "Connecting…"));
  await expect(page.locator("#viz")).toBeVisible();
  // The unlit stubs paint immediately; the bright leading edge appears as the creep travels.
  expect((await ringStats(page)).sum).toBeGreaterThan(0);
  await page.waitForTimeout(300);
  expect((await ringStats(page)).lit).toBeGreaterThan(0);

  // …and hidden again afterwards, since the visualizer is still off.
  await page.evaluate(() => showBuffering(null));
  await expect(page.locator("#viz")).toBeHidden();
});

test("the old header loading bar is gone", async ({ page }) => {
  await openClient(page);
  for (const id of ["buffering", "bufbar", "bufbarfill", "buftext"]) {
    expect(await page.locator("#" + id).count()).toBe(0);
  }
});
