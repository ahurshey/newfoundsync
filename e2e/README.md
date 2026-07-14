# Newfoundsync browser end-to-end tests

Playwright harness that boots the **real** server binary headless and drives the **actual** web
client in a real browser. This is the integration coverage the Rust `cargo test` suite can't reach —
the browser lifecycle (reload / cache / service worker / Start gate / decode / WebSocket cleanup) that
has been the project's main source of bugs.

Why this exists: unit tests are great for zeroing in on a *known* bug, but they don't *find*
integration bugs. The class of failure that hurt most here (a service worker serving a stale/corrupt
shell after N refreshes) can only be caught by exercising the whole loop in a browser.

## Run it

```bash
cd e2e
npm install                 # first time
npx playwright install chromium   # first time (add: firefox webkit for cross-browser)
npm run test:chromium       # fastest single-browser run
npm test                    # every installed browser project
npm run test:headed         # watch it drive a visible window
npm run report              # open the last HTML report
```

The config (`playwright.config.js`) auto-starts `../target/release/newfoundsync.exe` (override with
`NFS_EXE=/path/to/binary`) on `--headless --insecure-http --port 47155`. Plain HTTP is fine because
**localhost is a secure context**, so WebCodecs / getUserMedia / service-worker APIs all work without
a TLS cert. Build the binary first: `cargo build --release -p newfoundsync`.

## What it covers today

- **`reload-cache.spec.js`** — THE regression for the reload/cache bug: reloads 15× and asserts the
  shell always boots, **no service worker is ever registered**, **Cache Storage stays empty**, and no
  console errors occur. This would have caught the original bug on the first run.
- **`smoke.spec.js`** — serves + reports `/version`; loads the idle client; presses **Start** and
  confirms it leaves the idle gate, dismisses the first-connect name modal, and connects to the
  server (WebSocket + config + clock-sync + decoder-setup path).

Cross-browser (closing the "only ever tested on Chrome" gap):

- **Chromium** — full loop, all specs.
- **WebKit** (≈ Safari's engine) — runs the shell / cache / reload / idle lifecycle green, so the
  cache fix + client shell are verified on Safari's engine too. Its WebCodecs is incomplete, so the
  **Start/decode** smoke self-scopes to Chromium (`test.skip(browserName !== 'chromium')`).
- **Firefox** — supported in principle but hit a Windows-local `spawn UNKNOWN` launch error on the
  build box (works on Linux/CI); re-enable in `playwright.config.js` after `npx playwright install
  firefox`.

## Roadmap (turning the rest of the testing conversation into capabilities)

1. **Broaden the harness** (next): Stop→Start with no stale-audio overlap; kill a client mid-stream and
   assert the server frees the cast slot within ~5 s; drive a 2nd browser as a caster and assert a
   receiver gets frames; assert calibration UI appears with 2 clients. These map 1:1 onto the fixes
   just shipped, so each becomes a permanent guard.
2. **CI** (`.github/workflows`): run `cargo test --workspace` + this harness (chromium) on push, so
   regressions surface without a human. (Needs the build deps installed in the runner: libopus, VPX.)
3. **A sync-measurement tool** ("hear" the desync): a small standalone app where a 3rd device captures
   two clients over its mic and reports A/V desync via cross-correlation / FFT — turning the invisible
   (are they actually in sync?) into a number the AI can assert on. Reuses the MLS/Gold-code
   correlation already in the calibration path.
4. **CLI-first sensing**: the server already exposes `/version` and `/status`; add a machine-readable
   `/health` (client count, active caster, uptime) so smoke checks stay a simple `curl`.

## Notes / gotchas

- Headless has no audio output device, so tests assert on **state**, not sound.
- Playwright browsers + results are git-ignored (`node_modules/`, `test-results/`, `playwright-report/`).
