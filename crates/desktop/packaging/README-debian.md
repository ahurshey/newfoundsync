# Newfoundsync — headless server `.deb` (Debian / Ubuntu)

This builds an **audio-only headless server** (no GUI, no screen video — those are Windows-only).
The Linux box serves the embedded web client over HTTPS + WebSocket; LAN browsers clock-sync and
play audio in lock-step. Audio comes from a browser **web-cast uplink** (`--capture web`, the
default in the bundled systemd unit) or a PulseAudio/PipeWire **monitor** (`--capture system`).

> **Must be built on Linux.** Cross-compiling from Windows/macOS isn't supported (ALSA/C deps).
> Build on the target box (or any Debian/Ubuntu machine of the same arch).

## 1. Build dependencies (Ubuntu 24.04 / 26.04)

```bash
sudo apt update
sudo apt install -y build-essential pkg-config libasound2-dev
# Rust toolchain (skip if already installed):
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
. "$HOME/.cargo/env"
cargo install cargo-deb
```

`libasound2-dev` is ALSA for cpal (audio capture). **No** libvpx / SVT-AV1 / NASM / CMake are
needed — the video encoders are Windows-only and aren't compiled into this build.

## 2. Get the code onto the box

The v0.0.1 source must be present. Either:

```bash
git clone https://github.com/ahurshey/newfoundsync      # once the repo is pushed
# — or copy your working tree over (no build artefacts):
rsync -av --exclude target --exclude vcpkg_installed ./newfoundsync/ user@server:~/newfoundsync/
```

## 3. Build the `.deb`

```bash
cd newfoundsync
cargo deb -p newfoundsync --no-default-features
# → target/debian/newfoundsync_0.0.1-1_amd64.deb
```

`--no-default-features` drops the `gui` feature (so no `eframe`/X11/Wayland/GL). Build *with* the
GUI only if you really want it — you'd then also need the X11/Wayland/GL `-dev` packages.

## 4. Install + run

```bash
sudo apt install ./target/debian/newfoundsync_0.0.1-1_amd64.deb
sudo systemctl enable --now newfoundsync
systemctl status newfoundsync
journalctl -u newfoundsync -f          # logs + the connect URL
sudo ufw allow 47000/tcp               # open the port if a firewall is on
```

Open `https://<server-ip>:47000` in a LAN browser and accept the self-signed cert once
(WebCodecs requires a secure context, even on a LAN IP).

## Notes / gotchas

- **Audio source.** The unit defaults to `--capture web` (a browser casts audio up; others listen)
  because a headless server usually has no audio device. To capture the host's own output instead,
  edit `/lib/systemd/system/newfoundsync.service`, change `--capture web` → `--capture system`, and
  ensure a PulseAudio/PipeWire **monitor** source is visible to the service. `DynamicUser=yes` will
  **not** see a per-user PipeWire socket — for `system` capture, run under a real login user with
  `loginctl enable-linger <user>` and a user PipeWire instance instead.
- **State dir.** The persisted self-signed TLS cert + saved port live under `/var/lib/newfoundsync`
  (the unit points `HOME` there). If the log shows a cert/settings write error, check that path.
- **Young port.** Newfoundsync is Windows-first; this Linux target compiles cleanly here for both
  the default and `--no-default-features` configs, but it hasn't been run end-to-end on Linux yet.
  If the build or run errors on your box, paste the output and we'll fix the platform gate.
