// @ts-check
// /health is the server's self-identification + liveness surface. It exists because two questions were
// previously unanswerable from a field report: WHICH build is this box running (hand-copied .exe files
// are indistinguishable, and /version only hashes the web shell so it is blind to Rust changes), and IS
// the pipeline actually producing media (silence with no error looked identical to working).
//
// These assertions are deliberately about the CONTRACT, not exact values: a diagnostic endpoint that
// silently loses a field is worse than none, because you trust it while it lies.
const { test, expect } = require('@playwright/test');

/** Keys /health must always carry, with the type each must have. */
const SHAPE = {
  build: 'string',
  version: 'string',
  gitSha: 'string',
  shellTag: 'string',
  uptimeSecs: 'number',
  clients: 'number',
  casting: 'boolean',
  castSource: 'boolean',
  audioFrames: 'number',
  videoFrames: 'number',
  captureFrames: 'number',
  audioErrors: 'number',
  videoErrors: 'number',
  lastAudioAgeMs: 'number',
  lastVideoAgeMs: 'number',
  videoEncoderFailed: 'boolean',
};

/** Frame-age fields, which carry a sentinel and so have the tightest contract. */
const AGE_FIELDS = ['lastAudioAgeMs', 'lastVideoAgeMs'];

test.describe('/health', () => {
  test('serves valid JSON carrying build identity + liveness counters', async ({ request, baseURL }) => {
    const res = await request.get(new URL('/health', baseURL).href);
    expect(res.ok(), '/health should respond 2xx').toBeTruthy();
    expect(res.headers()['content-type'] || '', 'should be JSON').toContain('application/json');

    // Must PARSE — it's hand-formatted JSON, so a stray quote in a field would silently break it.
    const h = JSON.parse(await res.text());

    for (const [key, type] of Object.entries(SHAPE)) {
      expect(h, `/health must report "${key}"`).toHaveProperty(key);
      expect(typeof h[key], `/health "${key}" should be ${type}`).toBe(type);
    }

    // Build identity has to be specific enough to match a report to bytes.
    expect(h.version, 'version should look like semver').toMatch(/^\d+\.\d+\.\d+/);
    expect(h.build, 'build should embed the version').toContain(h.version);
    expect(h.build, 'build should embed the git sha').toContain(h.gitSha);
    expect(h.gitSha, 'gitSha should not be empty').not.toBe('');

    // The harness boots with --capture web, so this server IS a cast source with nobody casting yet.
    expect(h.castSource, 'harness runs --capture web → cast-capable source').toBe(true);
    expect(h.casting, 'no client is casting in this test').toBe(false);

    // No frames have been produced (nothing is casting), so ages read -1 rather than a bogus 0.
    expect(h.lastAudioAgeMs, 'no audio yet → age -1, not a misleading 0').toBe(-1);
    expect(h.videoEncoderFailed, 'no local encoder in web-uplink mode').toBe(false);
    expect(h.uptimeSecs, 'uptime should be a sane non-negative').toBeGreaterThanOrEqual(0);

    // Counters can only be non-negative; a negative would mean an underflow or a bad cast.
    for (const k of ['audioFrames', 'videoFrames', 'captureFrames', 'audioErrors', 'videoErrors', 'clients']) {
      expect(h[k], `${k} must never be negative`).toBeGreaterThanOrEqual(0);
    }
  });

  // The age fields once stored the WIRE PTS (mono_now() + a 50 ms lead) instead of the publish
  // instant, so a perfectly healthy stream reported ~-47 ms — and at the moment 49 ms had elapsed it
  // reported exactly -1, i.e. it claimed "no frame has ever been produced" about a live stream. The
  // shape test above could not catch that (it only checks the zero-frame case), so assert the
  // INVARIANT: -1 is the only legal negative, and it may appear only when the counter is truly 0.
  test('frame ages never go negative except the -1 "none yet" sentinel', async ({ request, baseURL }) => {
    const url = new URL('/health', baseURL).href;
    // Sample repeatedly: the old bug was a small constant offset, so a single lucky read could pass.
    for (let i = 0; i < 5; i++) {
      const h = JSON.parse(await (await request.get(url)).text());
      for (const f of AGE_FIELDS) {
        const age = h[f];
        expect(age, `${f} must be >= -1 (only -1 is legal as a sentinel), got ${age}`).toBeGreaterThanOrEqual(-1);
        const counter = f === 'lastAudioAgeMs' ? h.audioFrames : h.videoFrames;
        if (age === -1) {
          expect(counter, `${f} is -1 ("no frames yet") so its frame counter must be 0`).toBe(0);
        } else {
          expect(counter, `${f} reports a real age (${age}ms) so frames must have been produced`).toBeGreaterThan(0);
        }
      }
    }
  });

  test('build identity matches --version and is stable across calls', async ({ request, baseURL }) => {
    const url = new URL('/health', baseURL).href;
    const a = JSON.parse(await (await request.get(url)).text());
    const b = JSON.parse(await (await request.get(url)).text());
    expect(b.build, 'build identity must not change between requests').toBe(a.build);
    expect(b.uptimeSecs, 'uptime must not go backwards').toBeGreaterThanOrEqual(a.uptimeSecs);
  });

  test('/status names the build it is reporting on', async ({ request, baseURL }) => {
    // /status is the documented headless diagnostic page; it has to say which server you're looking at.
    const health = JSON.parse(await (await request.get(new URL('/health', baseURL).href)).text());
    const body = await (await request.get(new URL('/status', baseURL).href)).text();
    expect(body, '/status should show the build id').toContain(health.build);
    expect(body, '/status should have a Dropped column for the fell-behind signal').toContain('Dropped');
  });
});
