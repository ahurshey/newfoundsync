// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Alex Hurshman and the Newfoundsync contributors.

//! Newfoundsync core — the platform-neutral pieces the desktop binary shares: the codec (PCM/Opus) +
//! frame constants, the monotonic clock + audio config, video config, and a LAN address helper.
//!
//! What is deliberately NOT here:
//! * **The wire protocol.** It is defined where it is used — server-side tag constants in
//!   `crates/desktop/src/webserver.rs` and `media.rs`, client-side in `crates/desktop/web/app.js`.
//!   A `proto` module here once claimed to be the authoritative byte contract while being entirely
//!   unreferenced, and its tag table had drifted into direct conflict with the live wire (it listed
//!   `0x20` as HELLO where the shipping protocol uses `0x20` for SET_VOLUME). Removed rather than
//!   left to mislead the next reader; keeping the definitions next to the code that encodes and
//!   decodes them is what actually keeps them honest.
//! * **Platform audio capture** — desktop crate (`capture/`).
//! * **The live clock-sync / jitter-buffer / playout logic** — the browser client
//!   (`crates/desktop/web/app.js`), with the server's clock reply in `webserver.rs`.

pub mod codec;
pub mod config;
pub mod net;
pub mod video;
