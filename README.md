# Newfoundsync

**Lightweight LAN audio (and optional screen) sharing to any browser, with tight,
Sonos-like multi-room sync.** One PC (Windows or Linux) — the **server** — captures its sound
and serves a small web app over your local network. Every other device (phone,
tablet, laptop, TV browser) just **opens a URL** — nothing to install — buffers a
few seconds, clock-syncs, and plays back in lock-step with everything else.

Built in Rust. The sync core is ported from the proven
[`ensemble`](../ensemble) project, with capture ideas from
[`soundsync`](../soundsync). The server runs on **Windows and Linux** — Linux does audio
(PipeWire/PulseAudio monitor capture, whole-system or per-app) and web-cast relay; local *screen*
capture is still Windows-only. The client is any modern browser.

```
┌──────────────── server (this PC) ────────────────┐        ┌──────── browser client ────────┐
│ WASAPI capture → Opus 48k/20ms, PTS-stamped on a  │  WSS   │ clock-sync → jitter buffer →    │
│ monotonic master clock → axum HTTPS + WebSocket   │ ─────▶ │ WebCodecs decode → Web Audio    │  ×N, all
│ fan-out  (+ optional AV1/VP9 screen video)        │        │ deadline-scheduled playout      │  in sync
└───────────────────────────────────────────────────┘        └─────────────────────────────────┘
```

## Why it's nice

- **No client install.** The "client" is a web page. Open `https://<server>:47000`
  on anything with a browser and it plays. A QR code in the server UI makes it a scan-and-go.
- **Tight sync.** Every client schedules each frame against one shared clock, so all
  speakers emit the same sample at the same instant — sub-millisecond clock offset on a LAN.
- **Resilient.** A few-second jitter buffer over a reliable WebSocket rides out Wi-Fi
  stalls without gaps. Trades a little startup latency for whole-home robustness.
- **Per-room tuning.** Master + per-device volume, and a per-device sync trim (ms) to
  compensate for Bluetooth/HDMI/soundcard latency — by ear, or auto-calibrated by mic.

## How it works

- **Capture (Windows).** Three sources:
  - **All apps** *(default)* — WASAPI **process-loopback** excluding ourselves. Captures
    every other app **plus** system sounds, and **keeps streaming even when Windows is muted**.
  - **Just one window / app** — process-loopback scoped to a single process tree (e.g. the
    browser playing YouTube Music). Pick it from the live app list.
  - **Full system output** — WASAPI endpoint loopback (cpal). Mirrors the speakers, so it
    **goes silent when Windows is muted**.
- **Codec.** Opus, 48 kHz stereo, **510 kbps by default** (libopus' transparent max; tune
  with `--bitrate` or the UI), or uncompressed **PCM**. One 20 ms frame per message.
- **Transport.** **HTTPS + WebSocket** (TLS via a persisted self-signed cert). Reliable
  (TCP): lost packets are re-sent and the jitter buffer hides the stall. HTTPS is required
  because browsers only expose **WebCodecs** in a secure context — even on a LAN IP.
- **Clock sync.** An NTP-style follower in the browser probes the server's monotonic clock
  over the WebSocket (cold-start burst, then ~1 Hz; median of the best-RTT samples) and
  gates playout until confident.
- **Playout.** Each frame carries a presentation timestamp (PTS) on the server's master
  clock. Every client computes `playout = master→local(pts + buffer + per-device trim)`,
  so all devices land the same sample together. A jitter buffer reorders packets, gaps
  become silence, and late frames are dropped. Long-run clock drift is bounded by a **PI
  rate servo**: a deadbanded proportional term plus a leaky integrator trim the playback
  rate by at most ±1 %, slew-limited so corrections stay inaudible. Audio decodes via
  **WebCodecs Opus** and plays through a gapless **Web Audio** scheduler.
- **Per-device alignment.** A **sync** slider (ms) per device, set by ear, or…
- **Auto-calibration (mic).** A device emits a coded spread-spectrum signal (MLS / Gold
  code) and listens on its own microphone to measure and correct its real speaker→ear
  offset. **Calibrate all** orchestrates several devices at once (distinct codes + TDMA
  slots so their self-tests don't collide). Each client reports its measured sync back to
  the server so the mixer shows every device's *actual* offset.
- **Optional screen video.** Share the screen alongside audio: royalty-free **AV1** (GPU
  via Media Foundation where the hardware supports it, else CPU SVT-AV1) with a **VP9**
  (libvpx, CPU) fallback, decoded via WebCodecs and kept aligned to the same master clock.

## The server app

Run with **no flags** for the GUI; `--headless` runs server-only from the flags.

**GUI**
- **Connect strip** — the `https://…:47000` URL, a **Copy/Open** button, and a scannable **QR**.
- **Audio / Video / Buffer** — pick the source, optional screen video (resolution, fps,
  quality, **AV1 / VP9** encoder), and the buffer depth (**Snappy 1 s / Balanced 3 s / Rock-solid
  6 s**, slider up to 15 s). Hit **Apply** to switch the live stream.
- **Connected Clients mixer** — **master** volume + per-client **volume**, **sync**,
  **mute**, double-click **rename** (remembered across reconnects), each device's
  **reported actual sync**, and **Calibrate all**.
- **Light / dark** toggle and zoom controls.

**Headless** — `--headless` serves with no window (the stable path on machines whose
windowed GUI is flaky); everything is driven by flags or the browser-side controls.

**`/status`** — browse to `https://<server>:47000/status` for a live, read-only list of
connected clients (name, status, sync, volume, **frames dropped**, calibration), plus the
running build id in the footer. Works in headless mode.

### Diagnosing a problem

A shared listening session can degrade *silently* — audio stops, or one device drifts — while
every indicator still reads normal. These surfaces exist so you don't have to reproduce a
fault on your own hardware to understand it:

**`--version`** — the crate version plus the exact git commit the binary was built from
(`0.0.2 (a1b2c3d4e5f6)`, with `-dirty` appended if it was built over uncommitted changes).
Hand-copied binaries are otherwise impossible to tell apart, so quote this in any bug report.

**`/health`** — JSON, safe to `curl`, answers the two questions a report can't otherwise
settle:

```bash
curl -sk https://<server>:47000/health
```

- `build` / `gitSha` — *which* build this box is actually running. `-dirty` is appended when
  the compiled sources differed from the commit (it is scoped to files that affect the
  binary, so editing docs or CI does not flag a build).
- `audioErrors`, `videoErrors` — encode failures. Non-zero means the pipeline is running but
  failing, which sounds like silence or looks like a frozen picture.
- `lastAudioAgeMs`, `lastVideoAgeMs` — milliseconds since the last frame was published; `-1`
  (and only `-1`) means none has been produced yet. A climbing value with a steady client
  count means the *source* stopped, not the network.
- `videoEncoderFailed` — the encoder never initialized, so clients were told video was on
  but will never receive a frame.
- `audioFrames`, `videoFrames`, `captureFrames` — read these together, and mind what each
  one actually proves:
  - `videoFrames` counts *encoder output*, and the encoder re-encodes its last frame when
    capture goes idle. So `videoFrames` climbing while **`captureFrames` stays flat** means
    screen capture died and the picture is frozen — the encoder is fine.
  - `audioFrames` counts frames *published*, not sound anyone can hear. The default
    `--capture allapps` source pads silence to hold a steady 20 ms cadence, so this keeps
    climbing through a muted or dead device. It proves the pipeline is turning, not that the
    room can hear it — confirm audibility at a client.

- `audioStalled` / `captureStalled` — the straight answer, so you don't have to reimplement
  the threshold. Crucially these mean *"it was producing and stopped"*, **not** "it hasn't
  started": a cast source waiting for its first caster reports `false`, because a watchdog
  that cries wolf on every startup is one you learn to ignore.

**The server now tells you when it goes quiet.** A watchdog checks the pipeline every second
and logs once on each transition, so a stopped stream is not silent in the log too:

```
ERROR AUDIO HAS STOPPED — the pipeline was producing and is now silent; clients are still
      connected and will hear nothing  stalled_ms=2412 clients=3
INFO  audio recovered — frames are flowing again
```

It distinguishes a **frozen picture** from a dead encoder, which look identical from the
outside: the encoder happily re-encodes its last captured frame forever, so `videoFrames`
keeps climbing while `captureFrames` stays flat. `/status` shows a banner for either, since
that page is the headless diagnostic surface.

**Logs** — the default level is `info`, which now includes each client connecting,
identifying, and disconnecting *with the reason* (a normal close, a write timeout, a queue
overflow, or a tripped abuse guard — previously all indistinguishable silence). Encode
failures log at `warn`, rate-limited to one line every 5 s with a count of those suppressed.
For more detail:

```bash
RUST_LOG=newfoundsync=debug newfoundsync --headless
```

**"It stutters on one device"** — check that device's **Dropped** count on `/status`. Non-zero
means the server shed frames for *that client* because it couldn't keep up; zero points at the
network or the browser's own decoding instead.

## Build (Windows / Linux)

Needs **Rust** (stable) and, for the Opus codec, a **C toolchain + CMake** (the vendored
libopus is compiled at build time). The web client is embedded into the binary, so there's
no separate front-end build.

> **Two gotchas that will stop your build.** Both look like mysterious failures the first time:
>
> 1. **Stop the running server first.** Linking overwrites `newfoundsync.exe` in place, so if a
>    previous server is still running the build fails at the *very end* with
>    `error: linking with link.exe failed: exit code 101` / `Access is denied`. Fix:
>    `Stop-Process -Name newfoundsync -Force` (or just close the window) and rebuild.
> 2. **The `VPX_*` variables are per-shell.** They are environment variables, not saved
>    anywhere, so they vanish when you open a new terminal and the next build fails to find
>    libvpx. Either re-export them each session (see the VP9 section below) or make them
>    permanent in `.cargo/config.toml`:
>
>    ```toml
>    # .cargo/config.toml — repo-local, so no per-shell setup
>    [env]
>    VPX_LIB_DIR = { value = "vcpkg_installed/x64-windows-static/lib", relative = true }
>    VPX_INCLUDE_DIR = { value = "vcpkg_installed/x64-windows-static/include", relative = true }
>    VPX_VERSION = "1.13.0"
>    VPX_STATIC = "1"
>    ```

On Windows, CMake ships with the Visual Studio Build Tools but may not be on your `PATH`:

```powershell
$env:PATH = "C:\Program Files (x86)\Microsoft Visual Studio\18\BuildTools\Common7\IDE\CommonExtensions\Microsoft\CMake\CMake\bin;" + $env:PATH
cargo build --release          # builds crates/core + crates/desktop
```

…or install CMake standalone (`winget install Kitware.CMake`); the MSVC compiler is found
automatically. The binary lands at `target\release\newfoundsync.exe`
(`target/release/newfoundsync` on Linux).

**On Linux**, build headless — that's the shipping configuration (no X11/Wayland/GL stack needed):

```bash
sudo apt install -y build-essential pkg-config cmake \
  libasound2-dev libopus-dev libpulse-dev
cargo build --release -p newfoundsync --no-default-features
```

`libpulse-dev` is easy to miss and the build hard-fails without it — it's the PipeWire/PulseAudio
monitor capture (Fedora: `pulseaudio-libs-devel`). `libopus-dev` spares you building Opus from
vendored source (which then wants autotools). Building *with* the GUI additionally needs the
X11/Wayland/GL `-dev` packages — see `crates/desktop/packaging/README-debian.md`.

### Video codecs (AV1 default, VP9 opt-in)

Video is **AV1** by default and needs **no extra setup**: where your GPU has a hardware AV1 encoder
the server uses it via Media Foundation, otherwise it falls back to the CPU **SVT-AV1** encoder,
which ships as a prebuilt static library.

**VP9 is opt-in**, behind the `vp9` cargo feature. It links **libvpx**, a C library you supply via
[vcpkg](https://github.com/microsoft/vcpkg) plus four environment variables — and it used to be an
unconditional dependency, which meant `cargo build --release` failed on a fresh clone before you'd
done any of that. Now a plain clone builds and runs; you only need the setup below if you
specifically want `--encoder vp9`:

```bash
cargo build --release --features vp9
```

Without the feature, `--encoder vp9` logs a warning and uses AV1 instead (also royalty-free), and
advertises `av01` to clients so the advertised codec always matches what is actually sent.

libvpx is pinned to 1.13.1 in `vcpkg.json` (to match the Rust bindings), so a one-time setup builds it:

```powershell
git clone https://github.com/microsoft/vcpkg C:\vcpkg
C:\vcpkg\bootstrap-vcpkg.bat
$env:VCPKG_ROOT = "C:\vcpkg"
# From the repo root — builds libvpx 1.13.1 into .\vcpkg_installed\ :
& "$env:VCPKG_ROOT\vcpkg.exe" install --triplet x64-windows-static
# env-libvpx-sys links `libvpx`, but vcpkg names it vpx.lib — alias it:
Copy-Item vcpkg_installed\x64-windows-static\lib\vpx.lib vcpkg_installed\x64-windows-static\lib\libvpx.lib
$env:VPX_LIB_DIR     = "$PWD\vcpkg_installed\x64-windows-static\lib"
$env:VPX_INCLUDE_DIR = "$PWD\vcpkg_installed\x64-windows-static\include"
$env:VPX_VERSION = "1.13.0"; $env:VPX_STATIC = "1"
cargo build --release --features vp9
```

> If the link step reports CRT conflicts (`LNK4098`), add
> `$env:RUSTFLAGS = "-Ctarget-feature=+crt-static"`. (It linked fine without it in our testing,
> but a static libvpx can require it on some toolchains.)

> **What runs where.** *Audio* capture works on both, whole-system or per-app: Windows via WASAPI
> loopback, Linux via a PipeWire/PulseAudio monitor (per-app narrows that monitor to one
> application's stream, so the app keeps playing out the speakers). Per-*window* capture stays
> Windows-only — on Linux audio belongs to a process, and nothing maps a window to a stream.
> *Screen video* capture is **Windows-only**
> (WGC + Media Foundation / SVT-AV1). To get video on Linux, have a browser cast it up —
> `--capture web --video` (the `--video` flag is required; `--capture web` alone relays audio only,
> and add `--resolution`/`--fps` to dictate the quality the caster encodes to). The browser client
> works everywhere.

## Run

**GUI (default):**

```powershell
newfoundsync
```

**Headless (server-only):**

```powershell
newfoundsync --headless                          # all apps + system sounds, Opus 510k, 3s buffer
newfoundsync --headless --capture system         # mirror the speakers (respects mute)
newfoundsync --headless --capture app --app-pid 1234
newfoundsync --headless --video --resolution 1440p --fps 60
```

| Flag | Default | Meaning |
|------|---------|---------|
| `--port` | `47000` | HTTP(S) port for the web client + WebSocket |
| `--name` | hostname | Display name shown to clients |
| `--headless` | off | No GUI; serve from these flags |
| `--codec` | `opus` | `opus` or `pcm` |
| `--bitrate` | `510000` | Opus bits/sec (ignored for PCM) |
| `--buffer-ms` | `3000` | Client buffer = end-to-end latency **and** dropout cushion (≤ 15000) |
| `--capture` | `allapps` | `allapps` (all but us, survives mute) · `system` (endpoint) · `app` · `web` (a web client casts audio/video up to this server) |
| `--app-pid` | — | Target PID when `--capture app` |
| `--video` | off | Also share the screen |
| `--resolution` | `1080p` | `720p` · `1080p` · `1440p` · `2160p` |
| `--fps` | `30` | `30` or `60` |
| `--encoder` | `av1` | `av1` (royalty-free; GPU AV1 or CPU SVT-AV1) · `vp9` (royalty-free CPU fallback, needs libvpx) |
| `--insecure-http` | off | Plain HTTP (WebCodecs then only works via localhost / a TLS proxy) |

Run `newfoundsync --help` for the full list.

## Connect a client

1. On any device on the same network, open the server's URL — **`https://<server-ip>:47000`**
   (shown in the GUI and as a QR code).
2. Accept the **one-time self-signed certificate** ("Advanced → proceed") — required so the
   browser grants the secure context WebCodecs needs. The cert is persisted, so it's once per device.
3. Tap **Start** and it joins the sync.

> Clients must be on the same LAN/subnet as the server. Some corporate/guest Wi-Fi isolates
> wireless devices from each other — if a phone can't reach the server but a wired PC can,
> that's the network, not the app (a phone hotspot is a quick way to confirm).

## Layout

A Cargo workspace:

- **`crates/core`** — the small platform-neutral pieces: the Opus/PCM codec, the canonical frame +
  buffer constants, the monotonic master clock, video config, and a LAN-address helper. Note what is
  *not* here — the **wire protocol** lives beside the code that encodes and decodes it
  (`crates/desktop/src/webserver.rs` + `media.rs` server-side, `web/app.js` client-side), and the
  **clock-sync / jitter-buffer / playout** logic lives in the browser client. A `proto.rs` here once
  claimed to be the authoritative byte contract while being entirely unreferenced *and* contradicting
  the live wire; it was deleted rather than left to mislead.
- **`crates/desktop`** — the server binary (`newfoundsync`): audio capture (WASAPI on Windows,
  PipeWire/PulseAudio on Linux), Opus + AV1/VP9 encode (Windows), the axum **HTTPS + WebSocket**
  server, the **embedded web client**, the **egui** GUI, and the CLI.
- **`crates/desktop/web`** — the browser client (`index.html`, `app.js`, `sw.js`): WSS
  transport, clock sync, jitter buffer, WebCodecs decode, Web Audio playout, per-device
  controls, and mic calibration.
- **`nfs-watchdog.ps1`** — optional helper that keeps a headless server alive (auto-restart +
  crash-log capture) on machines where the windowed GUI is unstable.

### Working on the web client

`index.html` / `app.js` / `sw.js` / the manifest are compiled into the binary, so a one-line JS
change would normally mean a full ~3-minute rebuild (which on Windows also fails at link if the
previous server is still running). Point `NFS_WEB_DIR` at the source directory and the server reads
those four files from disk **per request** instead — then a client change is just F5:

```powershell
$env:NFS_WEB_DIR = "$PWD\crates\desktop\web"
.\target\release\newfoundsync.exe
```

```bash
NFS_WEB_DIR=crates/desktop/web ./target/release/newfoundsync
```

The build tag is recomputed from the on-disk bytes, so the client's self-heal check still agrees and
won't reload-loop. It logs a warning while active, only ever serves those four fixed filenames (the
request URI is never used to build a path), and falls back to the embedded copy if a file is missing —
so a half-saved edit can't take the server down. **Unset it for a real run**; the shipped binary must
serve its own embedded client.

## Tests

Three suites, all run by CI on pushes to `main` and on PRs against it
(`.github/workflows/ci.yml`):

```bash
cargo test --workspace --no-default-features    # sync core, codec, video config, buffer bounds
```

```bash
cd e2e && npm run test:unit                     # the calibration DSP, no browser, milliseconds
```

```bash
cd e2e && npm ci && npx playwright install chromium && npx playwright test --project=chromium
```

The DSP suite is the one that catches a **silent** sync bug. The signal math lives in
`crates/desktop/web/nfs-dsp.js` — extracted from `app.js` precisely so it can be asserted without a
browser — and the tests pin the properties correlation depends on: the code is deterministic per seed
(both devices must generate the identical signal), peak-normalized, genuinely periodic, band-limited,
and the resampler doesn't skew a ramp. A flipped filter sign fails two of them immediately; before,
the only thing that caught it was a human standing between two speakers.

One test earns special mention: `worker-dsp-parity.test.js` executes the DSP source that `app.js`
actually injects into the calibration worker (via `.toString()`) inside a bare VM sandbox, and asserts
byte-identical output to the module. That's what guarantees the signal the reference *plays* and the
template the follower *correlates* can never drift apart — and it fails loudly if any of those
functions ever starts depending on module scope, which would break only in the worker.

The browser suite boots the **real** server binary headless on localhost and drives Chromium
against it, so it covers the shell/reload/service-worker lifecycle that has historically been the
actual source of bugs. Build the release binary first — the harness launches
`target/release/newfoundsync`.

CI runs three jobs, which between them compile every configuration that ships:

| Job | Covers |
|---|---|
| `cargo test + browser E2E (Linux)` | headless Linux build, the Rust suite, and Chromium E2E |
| `cargo check with the GUI feature (Linux)` | `gui.rs` — the largest file in the repo |
| `cargo test (Windows)` | WASAPI capture, the Media Foundation encoder, WGC screen capture |

That last pair exists because CI used to build only Linux + `--no-default-features`, which skipped
`gui.rs` and every Windows-gated module — half the Rust in the repo, so a borrow error in the largest
file could ship green. VP9/libvpx stays out of CI by being an opt-in feature, so no job needs vcpkg.

## Status

**Works today:** audio capture (WASAPI all-apps / per-app / system on Windows; PipeWire monitor —
whole-system or per-app — on Linux), Opus/PCM, HTTPS+WebSocket streaming to browser clients, NTP clock sync, jitter-buffered
deadline-scheduled playout, the PI rate servo, per-device + master volume, per-device sync trim, mic
auto-calibration (single + "Calibrate all"), client-reported sync, optional AV1/VP9 screen video
(Windows) or a browser cast as the source, an egui server GUI with a live client mixer, `/status` +
`/health` pages, light/dark themes, and a headless CLI.

**Test coverage, honestly:** the workspace suite covers the codec round-trips, frame/buffer
constants, and video config — all pure functions. The **sync and playout path itself is not unit
tested**: the clock-offset selection, the PI servo, and the calibration correlation live in
`app.js`, where the browser suite only asserts that playback *starts*, not that it lands at the
right instant. Sync regressions are still caught by a human standing between two speakers.

**Since shipped:** the Linux server (PipeWire/PulseAudio monitor capture, `.deb` packaging), the
PI rate servo, web-client casting (a browser becomes the source), and the `/health` diagnostic
endpoint.

**Planned:** native Linux *screen* capture via the PipeWire ScreenCast portal, VA-API GPU encode,
Flatpak packaging, FEC for lossy Wi-Fi, and system-tray minimize.

This is a **trusted-LAN** tool: the TLS cert exists only to satisfy the browser's
secure-context requirement (self-signed, accept-once). No accounts, no cloud, no
internet-facing operation.

## License

Newfoundsync is **free software**, licensed under the **GNU General Public License,
version 3 or later** — see [LICENSE](LICENSE). You're free to use, study, share, and
modify it; if you distribute it (modified or not), you must pass those same freedoms on:
keep it under the GPL and make the corresponding source available.

Copyright © 2026 Alex Hurshman and the Newfoundsync contributors.

> **Codec note.** Newfoundsync encodes screen video with **AV1** or **VP9** — both
> royalty-free (AOMedia / Google), so distributing the binaries carries no video-codec
> patent-licensing obligation. The app ships **no H.264/HEVC encoder**; the web-client
> *cast* path merely relays the browser's own H.264, which the browser is already licensed
> for. (Not legal advice — consult a lawyer before *marketing* the project as royalty-free.)
