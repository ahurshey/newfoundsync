//! Newfoundsync core — the platform-neutral audio-sync engine shared by the
//! desktop binary (Windows/Linux) and the Android client.
//!
//! Everything here builds on every target: the wire protocol, NTP-style clock
//! sync, codec (PCM/Opus), jitter buffer + deadline scheduler, cpal playback,
//! UDP server fan-out, mDNS discovery, and the client runtime. Platform-specific
//! audio *capture* lives in the desktop crate.

// The modules expose a fuller API than any single consumer uses — reserved wire
// constants, telemetry fields, helper methods — kept for completeness and the
// roadmap. Allowed crate-wide rather than scattering per-item attributes.
#![allow(dead_code)]

pub mod client;
pub mod clock;
pub mod codec;
pub mod config;
pub mod discovery;
pub mod playback;
pub mod proto;
pub mod runtime;
pub mod server;
pub mod video;

#[cfg(test)]
mod e2e;
