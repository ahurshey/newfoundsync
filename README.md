# Newfoundsync

**Lightweight LAN audio (and optional screen) sharing to any browser, with tight,
Sonos-like multi-room sync.** One Windows PC — the **server** — captures its sound
and serves a small web app over your local network. Every other device (phone,
tablet, laptop, TV browser) just **opens a URL** — nothing to install — buffers a
few seconds, clock-syncs, and plays back in lock-step with everything else.

Built in Rust. The sync core is ported from the proven
[`ensemble`](../ensemble) project, with capture/discovery ideas from
[`soundsync`](../soundsync). Windows server today (a Linux server is a planned
port); the client is any modern browser.

```
┌──────────────── server (this PC) ────────────────┐        ┌──────── browser client ────────┐
│ WASAPI capture → Opus 48k/20ms, PTS-stamped on a  │  WSS   │ clock-sync → jitter buffer →    │
│ monotonic master clock → axum HTTPS + WebSocket   │ ─────▶ │ WebCodecs decode → Web Audio    │  ×N, all
│ fan-out  (+ optional HEVC screen video)           │        │ deadline-scheduled playout      │  in sync
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
  become silence, late frames are dropped, and a gentle single-sample drift nudge bounds
  long-run clock drift. Audio decodes via **WebCodecs Opus** and plays through a gapless
  **Web Audio** scheduler.
- **Per-device alignment.** A **sync** slider (ms) per device, set by ear, or…
- **Auto-calibration (mic).** A device emits a coded spread-spectrum signal (MLS / Gold
  code) and listens on its own microphone to measure and correct its real speaker→ear
  offset. **Calibrate all** orchestrates several devices at once (distinct codes + TDMA
  slots so their self-tests don't collide). Each client reports its measured sync back to
  the server so the mixer shows every device's *actual* offset.
- **Optional screen video.** Share the screen alongside audio: hardware **HEVC** (Media
  Foundation, GPU) with an **H.264** (openh264) CPU fallback, decoded via WebCodecs and
  kept aligned to the same master clock.

## The server app

Run with **no flags** for the GUI; `--headless` runs server-only from the flags.

**GUI**
- **Connect strip** — the `https://…:47000` URL, a **Copy/Open** button, and a scannable **QR**.
- **Audio / Video / Buffer** — pick the source, optional screen video (resolution, fps,
  quality, GPU/CPU encoder), and the buffer depth (**Snappy 1 s / Balanced 3 s / Rock-solid
  6 s**, slider up to 15 s). Hit **Apply** to switch the live stream.
- **Connected Clients mixer** — **master** volume + per-client **volume**, **sync**,
  **mute**, double-click **rename** (remembered across reconnects), each device's
  **reported actual sync**, and **Calibrate all**.
- **Light / dark** toggle and zoom controls.

**Headless** — `--headless` serves with no window (the stable path on machines whose
windowed GUI is flaky); everything is driven by flags or the browser-side controls.

**`/status`** — browse to `https://<server>:47000/status` for a live, read-only list of
connected clients (name, status, sync, volume, calibration). Works in headless mode.

## Build (Windows / Linux)

Needs **Rust** (stable) and, for the Opus codec, a **C toolchain + CMake** (the vendored
libopus is compiled at build time). The web client is embedded into the binary, so there's
no separate front-end build.

On Windows, CMake ships with the Visual Studio Build Tools but may not be on your `PATH`:

```powershell
$env:PATH = "C:\Program Files (x86)\Microsoft Visual Studio\18\BuildTools\Common7\IDE\CommonExtensions\Microsoft\CMake\CMake\bin;" + $env:PATH
cargo build --release          # builds crates/core + crates/desktop
```

…or install CMake standalone (`winget install Kitware.CMake`); the MSVC compiler is found
automatically. On Linux you need `cmake`, a C compiler, and ALSA dev headers
(`libasound2-dev`). The binary lands at `target\release\newfoundsync.exe`
(`target/release/newfoundsync` on Linux).

> **Capture is Windows-only today** (WASAPI loopback + WGC/Media Foundation video). A Linux
> server (PipeWire capture) is a planned port; the browser client already works everywhere.

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
| `--capture` | `allapps` | `allapps` (all but us, survives mute) · `system` (endpoint) · `app` |
| `--app-pid` | — | Target PID when `--capture app` |
| `--video` | off | Also share the screen |
| `--resolution` | `1080p` | `720p` · `1080p` · `1440p` · `2160p` |
| `--fps` | `30` | `30` or `60` |
| `--encoder` | `auto` | `auto` (GPU→CPU) · `hardware` · `cpu` |
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

- **`crates/core`** — the platform-neutral engine: wire protocol, NTP-style clock sync,
  codec (Opus/PCM), jitter buffer + deadline scheduler, the monotonic clock, and shared config.
- **`crates/desktop`** — the Windows server binary (`newfoundsync`): WASAPI capture, Opus +
  HEVC/H.264 encode, the axum **HTTPS + WebSocket** server, the **embedded web client**, the
  **egui** GUI, and the CLI.
- **`crates/desktop/web`** — the browser client (`index.html`, `app.js`, `sw.js`): WSS
  transport, clock sync, jitter buffer, WebCodecs decode, Web Audio playout, per-device
  controls, and mic calibration.
- **`nfs-watchdog.ps1`** — optional helper that keeps a headless server alive (auto-restart +
  crash-log capture) on machines where the windowed GUI is unstable.

## Status

**Works today:** WASAPI capture (all-apps / per-app / system), Opus/PCM, HTTPS+WebSocket
streaming to browser clients, NTP clock sync, jitter-buffered deadline-scheduled playout,
drift nudging, per-device + master volume, per-device sync trim, mic auto-calibration
(single + "Calibrate all"), client-reported sync, optional HEVC/H.264 screen video, an egui
server GUI with a live client mixer, a `/status` page, light/dark themes, and a headless CLI.
The sync core is covered by the workspace test suite, including a hardware-free end-to-end
loopback test.

**Planned:** a Linux server (PipeWire capture), FEC for lossy Wi-Fi, a full PI rate servo,
and system-tray minimize.

This is a **trusted-LAN** tool: the TLS cert exists only to satisfy the browser's
secure-context requirement (self-signed, accept-once). No accounts, no cloud, no
internet-facing operation.

## License

Newfoundsync is **free software**, licensed under the **GNU General Public License,
version 3 or later** — see [LICENSE](LICENSE). You're free to use, study, share, and
modify it; if you distribute it (modified or not), you must pass those same freedoms on:
keep it under the GPL and make the corresponding source available.

Copyright © 2026 Alex Hurshman and the Newfoundsync contributors.

> **Codec note.** H.264 and HEVC are patent-encumbered formats. This project's *source*
> is GPL-licensed, but distributing binaries that encode or decode them may carry separate
> patent-licensing obligations in some jurisdictions, independent of this software license.
