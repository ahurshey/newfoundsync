// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Alex Hurshman and the Newfoundsync contributors.

//! Newfoundsync core — the platform-neutral pieces the desktop binary shares:
//! the wire protocol, codec (PCM/Opus) + frame constants, monotonic clock + audio
//! config, video config, and mDNS discovery. Platform-specific audio *capture*
//! lives in the desktop crate; the live clock-sync / jitter-buffer / playout logic
//! lives in the browser client (crates/desktop/web/app.js) and the server clock
//! reply in crates/desktop/src/webserver.rs.

// Some modules expose a fuller API than any single consumer uses (reserved wire
// constants, telemetry fields, helper methods). Allowed crate-wide rather than
// scattering per-item attributes.
#![allow(dead_code)]

pub mod codec;
pub mod config;
pub mod discovery;
pub mod proto;
pub mod video;
