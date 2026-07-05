// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Alex Hurshman and the Newfoundsync contributors.

//! Audio capture. v1 ships system-wide loopback (the whole output mix) via cpal's
//! WASAPI loopback (building an *input* stream on the default *output* device,
//! which cpal turns into a loopback capture and COM-initializes itself).
//!
//! Per-application capture (WASAPI `ActivateAudioInterfaceAsync` +
//! `PROCESS_LOOPBACK`) and an app picker are the v2 follow-up (`app.rs` +
//! `enumerate.rs`) — kept out of v1 so capture is the only unproven surface.

// cpal-based capture (Windows loopback + macOS default input). Linux uses PulseAudio/PipeWire
// instead (below) because cpal/ALSA can't see PipeWire `.monitor` sources.
#[cfg(not(target_os = "linux"))]
pub mod system;
#[cfg(target_os = "linux")]
pub mod pulse;

#[cfg(target_os = "windows")]
pub mod process;
#[cfg(target_os = "windows")]
pub mod sessions;

#[cfg(not(target_os = "linux"))]
#[allow(unused_imports)]
pub use system::SystemCapture;
