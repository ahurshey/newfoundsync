# Newfoundsync — Flatpak (graphical desktop app)

Builds the **GUI** Newfoundsync server as a sandboxed Flatpak, in-runtime (so it links against the
Freedesktop runtime, not the host). The headless server is better run from the `.deb` + systemd —
see [`../crates/desktop/packaging/README-debian.md`](../crates/desktop/packaging/README-debian.md).

## Build (any Linux with flatpak + flatpak-builder)

```bash
sudo apt install -y flatpak flatpak-builder
flatpak remote-add --user --if-not-exists flathub https://flathub.org/repo/flathub.flatpakrepo

# From the repo root — pulls the runtime/SDK/rust extension, builds in-sandbox:
flatpak-builder --user --force-clean --install-deps-from=flathub \
  --repo=/tmp/nfs-flatpak-repo build-dir flatpak/ca.newfoundsync.Newfoundsync.yml

# Single-file bundle you can carry to another machine:
flatpak build-bundle /tmp/nfs-flatpak-repo ca.newfoundsync.Newfoundsync.flatpak ca.newfoundsync.Newfoundsync
```

## Install + run the bundle on another machine

```bash
flatpak install --user ca.newfoundsync.Newfoundsync.flatpak
flatpak run ca.newfoundsync.Newfoundsync
```

## Notes

- **Sandbox permissions** (`finish-args`): Wayland / X11 + `--device=dri` for the egui window,
  `--socket=pulseaudio` to capture system audio via PipeWire/Pulse, `--share=network` to serve the
  HTTP/WebSocket + mDNS discovery on the LAN.
- The manifest builds with `--share=network` so cargo can fetch crates during the build — fine for a
  local build. A Flathub submission would instead vendor crates offline via a generated
  `cargo-sources.json` (flatpak-cargo-generator).
- Video encoders (SVT-AV1 / libvpx) are `cfg(windows)`-only, so the Linux/Flatpak build is
  audio + web-cast relay — no libvpx/SVT-AV1 C dependencies.
