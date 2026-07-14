// @ts-check
// Playwright config for the Newfoundsync web-client end-to-end / full-loop tests.
//
// It boots the REAL server binary headless over plain HTTP on localhost (localhost is a secure
// context, so WebCodecs + getUserMedia + service-worker APIs all work without a TLS cert) and drives
// the actual embedded client. This is the integration coverage the Rust unit tests can't reach — the
// browser lifecycle (reload / cache / service worker / Start gate / decode / WS cleanup) that has been
// the real source of bugs.
//
// Run:   npm test                 (all installed browser projects)
//        npm run test:chromium     (Chromium only — fastest)
//        npm run test:headed       (watch it drive a real window)
// Point at a specific binary:  NFS_EXE=/path/to/newfoundsync[.exe] npm test
const { defineConfig, devices } = require('@playwright/test');
const path = require('path');

const IS_WIN = process.platform === 'win32';
const DEFAULT_EXE = path.resolve(
  __dirname,
  '..',
  'target',
  'release',
  IS_WIN ? 'newfoundsync.exe' : 'newfoundsync'
);
const EXE = process.env.NFS_EXE || DEFAULT_EXE;
const PORT = Number(process.env.NFS_PORT || 47155);
const BASE = `http://localhost:${PORT}`;

module.exports = defineConfig({
  testDir: './tests',
  // The reload loop reloads the page many times; give tests room. Individual asserts still fail fast.
  timeout: 120_000,
  expect: { timeout: 15_000 },
  // One shared server + one client-state machine per run — keep it serial so tests don't fight.
  fullyParallel: false,
  workers: 1,
  forbidOnly: !!process.env.CI,
  retries: process.env.CI ? 1 : 0,
  reporter: [['list'], ['html', { open: 'never' }]],
  use: {
    baseURL: BASE,
    trace: 'retain-on-failure',
    video: 'retain-on-failure',
    // Headless browsers need these so the app's audio-unlock + calibration/cast getUserMedia paths
    // don't stall on a permission prompt or autoplay gate.
    launchOptions: {
      args: IS_WIN
        ? [
            '--autoplay-policy=no-user-gesture-required',
            '--use-fake-ui-for-media-stream',
            '--use-fake-device-for-media-stream',
          ]
        : [
            '--autoplay-policy=no-user-gesture-required',
            '--use-fake-ui-for-media-stream',
            '--use-fake-device-for-media-stream',
          ],
    },
  },
  projects: [
    { name: 'chromium', use: { ...devices['Desktop Chrome'] } },
    // WebKit ≈ Safari's engine. It runs the shell / cache / service-worker / reload / idle lifecycle
    // (where the real breakage has lived) — closing the "only ever tested on Chrome" gap. Its
    // WebCodecs is incomplete, so the Start/decode smoke self-scopes to chromium (see smoke.spec.js).
    { name: 'webkit', use: { ...devices['Desktop Safari'] } },
    // Firefox is supported in principle but hit a Windows-local `spawn UNKNOWN` launch error on the
    // build box (works on Linux/CI). Re-enable after `npx playwright install firefox`:
    // { name: 'firefox', use: { ...devices['Desktop Firefox'] } },
  ],
  // Boot the real server for the duration of the run, then kill it. `--capture web` (web-uplink relay)
  // needs NO local audio device, so this runs identically on the Windows build box and on headless
  // Linux CI; the client still connects + serves, and the specs assert on state, not sound.
  webServer: {
    command: `"${EXE}" --headless --insecure-http --capture web --port ${PORT}`,
    url: `${BASE}/version`,
    reuseExistingServer: !process.env.CI,
    timeout: 30_000,
    stdout: 'pipe',
    stderr: 'pipe',
  },
});
