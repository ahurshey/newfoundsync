// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Alex Hurshman and the Newfoundsync contributors.

//! Desktop video pipeline (Windows): screen capture → AV1/VP9 encode. Transport is
//! the web server (WebSocket); the browser decodes via WebCodecs and renders to a
//! canvas, A/V-synced to the audio via the shared master clock.

// Cross-platform: web-cast H.264 keyframe detection (no encoder / no C deps).
pub mod relay;

// The AV1 encoder. Always compiled on Windows — the only platform with local screen capture, so the
// only one with frames to encode. Elsewhere it is opt-in behind `video-encode`, because it links
// SVT-AV1 and the headless Linux server has nothing to feed it: un-gating it unconditionally would
// add a C library to every .deb/.rpm/Flatpak for a code path that cannot run.
//
// The feature exists so the Linux (xdg-desktop-portal/PipeWire) and macOS (ScreenCaptureKit)
// capture work can be built and CI-checked against a real encoder before any capture backend
// lands — the encoder was the hidden prerequisite, since gating it on the OS rather than on the
// capability meant "add screen capture to Linux" silently also meant "port the encoder".
#[cfg(any(target_os = "windows", feature = "video-encode"))]
pub mod codec;
// VP9 links libvpx (a C library supplied at build time), so it is behind the `vp9` feature — a fresh
// clone builds without any of that setup and gets AV1, which needs none.
//
// Gated on the FEATURE only, never the OS: the encoder is plain libvpx FFI with nothing
// Windows-specific in it, and the earlier `target_os = "windows"` gate meant "ship VP9 on Linux"
// silently also meant "port the encoder" — the same trap the AV1 encoder was in before it was gated
// on capability instead. libvpx comes from vcpkg on Windows, the Freedesktop runtime in the Flatpak,
// and a local build on macOS.
#[cfg(feature = "vp9")]
pub mod vp9;
#[cfg(target_os = "windows")]
pub mod capture;
// Linux screen capture presents the same surface (start_primary / slot / closed) under the same
// module path, so VideoProducer consumes either backend without knowing which it has.
#[cfg(all(target_os = "linux", feature = "linux-capture"))]
#[path = "capture_portal.rs"]
pub mod capture;
#[cfg(target_os = "windows")]
pub mod mf_encoder;
