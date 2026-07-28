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

/// Appended to `Media::capture_device` when the capture source is a dummy/null sink.
///
/// Measured, not assumed: a dummy sink's monitor DOES carry whatever applications play into it
/// (a 440 Hz tone played to `auto_null` came back off `auto_null.monitor` at full amplitude). So
/// this is not a promise of silence — it is a warning that the machine has no real output device.
/// Nothing is audible locally, and the stream carries only what apps still push into the dummy,
/// which is nothing at all whenever they stop.
///
/// Defined here rather than in `pulse` (its only producer) because the GUI is cross-platform and
/// must recognise the tag to raise its banner; two copies of the literal would drift apart and the
/// banner would quietly stop appearing.
pub const DUMMY_TAG: &str = "⚠ DUMMY OUTPUT — no real audio device on this machine";

#[cfg(target_os = "windows")]
pub mod process;
#[cfg(target_os = "windows")]
pub mod sessions;
// The Linux app picker deliberately answers to the same module path, so `gui.rs` imports
// `capture::sessions::{AudioApp, list_sources}` once and gets the right implementation. The two
// are NOT interchangeable in meaning — see linux_sessions.rs for what differs.
#[cfg(target_os = "linux")]
#[path = "linux_sessions.rs"]
pub mod sessions;

#[cfg(not(target_os = "linux"))]
#[allow(unused_imports)]
pub use system::SystemCapture;
