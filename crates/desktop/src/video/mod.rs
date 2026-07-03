// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Alex Hurshman and the Newfoundsync contributors.

//! Desktop video pipeline (Windows): screen capture → H.264 encode. Transport is
//! the web server (WebSocket); the browser decodes via WebCodecs and renders to a
//! canvas, A/V-synced to the audio via the shared master clock.

pub mod codec;

#[cfg(target_os = "windows")]
pub mod capture;
#[cfg(target_os = "windows")]
pub mod mf_encoder;
#[cfg(target_os = "windows")]
pub mod gpu;
