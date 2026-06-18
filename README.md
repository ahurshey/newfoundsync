# Newfoundsync

**Lightweight LAN audio sharing with tight multiroom sync.** One machine (the
**server**) captures its audio and streams it to other machines (**clients**) on
the same local network, which play it back in tight, Sonos-like sync. Clients
auto-discover servers over mDNS — no configuration, no cloud, trusted-LAN only.

Windows-first (Linux is a planned port). Built in Rust; the sync core is ported
from the proven [`ensemble`](../ensemble) project, with capture/discovery ideas
from [`soundsync`](../soundsync).

```
┌─────────── server ───────────┐         ┌──────── client ────────┐   ┌──── client ────┐
│ WASAPI loopback → Opus 20ms   │  UDP    │ clock-sync → jitter →   │   │  … plays in     │
│ frames, stamped on a master   │ ──────▶ │ deadline-scheduled      │   │  lock-step      │
│ clock, fanned out + mDNS ad   │         │ playout → speakers      │   │                 │
└───────────────────────────────┘         └─────────────────────────┘   └─────────────────┘
```

## How it works

- **Capture** — system-wide WASAPI loopback (cpal), folded to stereo and
  resampled to a canonical 48 kHz / 20 ms frame.
- **Codec** — Opus (default, 320 kbps) or raw PCM. One frame → one UDP datagram.
- **Discovery** — the server advertises `_newfoundsync._udp.local.` over mDNS;
  clients browse and pick one.
- **Clock sync** — an NTP-style follower probes the server's clock (cold-start
  burst, then 1 Hz; median of the 5 best-RTT of the last 30 samples) and gates
  playout until confident. Measured offset on a LAN is sub-millisecond.
- **Playout** — each frame carries a presentation timestamp (PTS) on the shared
  master clock. Every client schedules `playout = master_to_local(pts + buffer +
  delay)`, so all speakers emit the same sample at the same instant. A jitter
  buffer reorders packets, gaps become silence, late frames are dropped, and a
  gentle single-sample drift nudge bounds long-run clock drift.
- **Per-device alignment** — a delay slider (ms) compensates for sound cards /
  Bluetooth / HDMI that add different output latencies. This is the primary
  cross-room aligner.

## Layout

A Cargo workspace:

- `crates/core` — the platform-neutral engine (protocol, clock sync, codec,
  jitter buffer + scheduler, cpal playback, UDP server, mDNS, client runtime).
- `crates/desktop` — the Windows/Linux binary (`newfoundsync`): capture, server,
  egui UI, CLI.
- `crates/android` — the Android client cdylib (eframe + the shared engine).

## Build — Desktop (Windows / Linux)

Needs **Rust** (stable) and, for the Opus codec, a **C toolchain + CMake** (the
vendored libopus is compiled at build time).

On Windows, CMake ships with the Visual Studio Build Tools but may not be on your
`PATH`. Add it for the build (Build Tools 2026 shown):

```powershell
$env:PATH = "C:\Program Files (x86)\Microsoft Visual Studio\18\BuildTools\Common7\IDE\CommonExtensions\Microsoft\CMake\CMake\bin;" + $env:PATH
cargo build --release          # builds crates/core + crates/desktop
```

…or install CMake standalone (`winget install Kitware.CMake`). The MSVC compiler
is located automatically. On Linux you need `cmake`, a C compiler, and the ALSA
dev headers (`libasound2-dev`); the binary is self-contained once built.

The release binary lands at `target\release\newfoundsync.exe` (or
`target/release/newfoundsync` on Linux).

> **Linux capture note:** sharing system audio uses a PulseAudio/PipeWire
> `.monitor` source. The client (playback) works on any cpal-supported output.

## Build — Android (client only)

Android can't capture system audio, so it's a **client**. Prereqs: the Android
**SDK + NDK** (r27+), `rustup target add aarch64-linux-android`, and
`cargo install cargo-apk`.

`audiopus_sys` ships only a Windows libopus and can't cross-compile, so libopus
is built per-ABI with the NDK and linked via `OPUS_LIB_DIR`. The helper scripts
handle this — build a signed release APK per ABI into `dist/`:

```powershell
$env:ANDROID_NDK_HOME = "$env:LOCALAPPDATA\Android\Sdk\ndk\27.2.12479018"
# one-time: a throwaway dev signing key (see crates/android/Cargo.toml)
keytool -genkeypair -keystore crates\android\release.keystore -alias newfoundsync `
  -keyalg RSA -keysize 2048 -validity 10000 -storepass changeit -keypass changeit -dname "CN=Newfoundsync Dev"

pwsh crates\android\build-apk.ps1            # arm64 + x86_64 → dist\newfoundsync-<abi>.apk
```

Install on a phone (arm64) or emulator (x86_64):

```powershell
adb install -r dist\newfoundsync-aarch64.apk     # phone
adb install -r dist\newfoundsync-x86_64.apk      # emulator
```

### Emulator smoke-test

```powershell
$sdk = "$env:LOCALAPPDATA\Android\Sdk"
& "$sdk\cmdline-tools\latest\bin\sdkmanager.bat" "emulator" "system-images;android-35;google_apis;x86_64"
& "$sdk\cmdline-tools\latest\bin\avdmanager.bat" create avd -n nfs_test -k "system-images;android-35;google_apis;x86_64" -d pixel_6
& "$sdk\emulator\emulator.exe" -avd nfs_test -no-window -gpu swiftshader_indirect
adb install -r dist\newfoundsync-x86_64.apk
adb shell monkey -p me.newfoundsync.client -c android.intent.category.LAUNCHER 1
```

> The emulator's NAT doesn't bridge mDNS multicast to the host LAN, so it won't
> discover a server running on your PC — discovery needs a real device on the
> same Wi-Fi. The emulator is for verifying the app launches and renders.

## Run

**GUI (default):**

```powershell
newfoundsync
```

Pick **Server** to share this PC's audio, or **Client** to play from a discovered
server (with volume and per-device delay sliders and live sync telemetry).

**Headless CLI:**

```powershell
newfoundsync server                 # share this PC's audio (Opus 320k by default)
newfoundsync server --codec pcm     # uncompressed instead
newfoundsync client                 # discover and play the first server found
newfoundsync client --server kitchen --volume 0.8 --delay-ms 30
```

Run `newfoundsync <cmd> --help` for all options.

> **Tip:** on a single machine the server captures the same device the client
> plays to, which can feed back — use two machines for a real test.

## Status

**Works today:** system-loopback capture, Opus/PCM, mDNS discovery, NTP clock
sync, jitter-buffered deadline-scheduled playout, drift nudging, per-client delay
alignment, volume, an egui control panel, and a CLI. The sync core is covered by
53 unit/integration tests (including a hardware-free end-to-end loopback test).

**Planned (v2):** per-application capture (WASAPI process loopback), FEC for lossy
Wi-Fi, a full PI rate servo, system-tray minimize, automatic cross-room latency
calibration, and a Linux port (cpal output is already cross-platform).

This is a trusted-LAN tool: no auth, no TLS, no internet-facing operation.
