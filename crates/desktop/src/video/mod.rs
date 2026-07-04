// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Alex Hurshman and the Newfoundsync contributors.

//! Desktop video pipeline (Windows): screen capture → AV1/VP9 encode. Transport is
//! the web server (WebSocket); the browser decodes via WebCodecs and renders to a
//! canvas, A/V-synced to the audio via the shared master clock.

// Cross-platform: web-cast H.264 keyframe detection (no encoder / no C deps).
pub mod relay;

// The native video encoders + screen capture are Windows-only (the only platform with local
// video capture today). On Linux the server builds audio + web-cast relay only, so these — and
// their C deps (SVT-AV1, libvpx) — aren't compiled.
#[cfg(target_os = "windows")]
pub mod codec;
#[cfg(target_os = "windows")]
pub mod vp9;
#[cfg(target_os = "windows")]
pub mod capture;
#[cfg(target_os = "windows")]
pub mod mf_encoder;
