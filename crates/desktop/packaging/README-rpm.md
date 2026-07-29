# Newfoundsync — Fedora / RHEL `.rpm` packaging

Two packages come out of one source tree, mirroring the Debian setup:

| Package | Contents | For |
|---|---|---|
| `newfoundsync` | `/usr/bin/newfoundsync` + a systemd unit | headless server (no GUI deps) |
| `newfoundsync-gui` | `/usr/bin/newfoundsync` + `.desktop` entry + icons | desktop app |

They **Conflict** with each other (and the GUI one **Obsoletes** the headless one) because both own
`/usr/bin/newfoundsync` — installing either displaces the other instead of fighting over the path.

> **Must be built on Linux.** Cross-compiling from Windows/macOS isn't supported: the Linux audio
> path links libpulse, and the GUI links the X11/Wayland/GL stack.

## Build dependencies

```bash
sudo dnf install -y gcc pkgconf-pkg-config rpm-build \
  pulseaudio-libs-devel opus-devel alsa-lib-devel \
  libxkbcommon-devel libxkbcommon-x11-devel wayland-devel \
  libX11-devel libXcursor-devel libXrandr-devel libXi-devel mesa-libGL-devel
```

`opus-devel` is the one that's easy to miss — without it `audiopus_sys` tries to build Opus from
vendored source and dies on missing autotools. The `libxkbcommon`/`wayland`/`libX*`/`mesa-libGL`
packages are only needed for the **GUI** build; a headless-only box can skip them.

Rust must be recent (eframe 0.34 needs ≥ 1.92), so prefer rustup over the distro package:

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
cargo install cargo-generate-rpm --locked
```

## Building

Run both from the **repo root** — the asset paths in `[package.metadata.generate-rpm]` are relative
to the invocation directory, unlike cargo-deb's package-relative ones.

```bash
# headless
cargo build --release -p newfoundsync --no-default-features
cargo generate-rpm -p crates/desktop

# desktop (GUI)
cargo build --release -p newfoundsync
cargo generate-rpm -p crates/desktop --variant gui
```

`cargo generate-rpm` does **not** build anything or infer features from the variant — it packages
whatever is currently at `target/release/newfoundsync`. Build the matching binary first, or you
will ship a headless binary inside the GUI package (or vice versa). Output lands in
`target/generate-rpm/`.

## Installing and checking

```bash
sudo dnf install -y ./target/generate-rpm/newfoundsync-gui-*.rpm
rpm -qi newfoundsync-gui              # metadata
rpm -ql newfoundsync-gui              # file list
rpm -qR newfoundsync-gui              # dependencies
newfoundsync --version
```

Headless smoke test (serves on 47000, needs no audio hardware with `--capture web`):

```bash
newfoundsync --headless --capture web
sudo systemctl enable --now newfoundsync    # or run it as a service
```

## Notes

- **The GUI package's runtime dependencies are hand-listed.** eframe and wgpu `dlopen`
  libxkbcommon, libwayland-client, libX11/libXcursor/libXrandr/libXi, libGL and the Vulkan loader,
  so rpmbuild's `find-requires` cannot discover them from the ELF — it only sees linked sonames.
  They are declared explicitly under `[package.metadata.generate-rpm.variants.gui.requires]`. If
  you add a GUI dependency that is loaded at runtime rather than linked, add it there too, or the
  package will install cleanly and then fail to open a window.
- **Audio source.** `--capture system` records the default sink's monitor; `--capture app --app-pid
  <PID>` records a single application (the app keeps playing out the speakers). `--capture web`
  relays a browser's cast and needs no local audio at all.
- **On a VM with no sound card**, PipeWire only offers its `auto_null` Dummy Output. The server says
  so loudly rather than pretending to work — nothing is audible on such a host, and a whole-system
  capture carries only what applications still push into the dummy. Per-app capture still works
  there, which makes it a reasonable way to test on headless hardware.
- Screen/video capture is Windows-only; on Linux have a browser cast it up
  (`--capture web --video`).

## The AV1 encoder off Windows (`video-encode`) — currently broken on Fedora

On Debian/Ubuntu, `--features video-encode-source` builds the AV1 encoder (see
`README-debian.md`). **On Fedora and other `lib64` distros it does not link**, and the cause is
upstream, not here:

```
out/lib64/libSvtAv1Enc.a          <- where CMake installs it on Fedora
rustc-link-search=native=out/lib  <- where shiguredo_svt_av1's build.rs looks
error: could not find native static library `SvtAv1Enc`
```

`shiguredo_svt_av1`'s build script returns `dst.join("lib")` unconditionally, while CMake's
GNUInstallDirs resolves to `lib64` on Fedora/RHEL/SUSE. Everything before that step succeeds — the
deps are `cmake nasm git gcc-c++ clang-devel`, and SVT-AV1 itself compiles fine — so this is purely
the install-dir mismatch.

Until it is fixed upstream, build the encoder on a Debian-family host. This blocks nothing that
ships today: the `.rpm`s are audio + web-cast relay and do not include the encoder.
