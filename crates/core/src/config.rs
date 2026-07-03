// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Alex Hurshman and the Newfoundsync contributors.

//! Canonical audio constants, runtime defaults, and the single monotonic clock.
//!
//! Mirrors the canonical-format constants from `ensemble/internal/stream/wire.go`
//! and the wall-anchored monotonic clock from `ensemble/internal/clock/clock.go`.

use std::sync::OnceLock;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

// ---- Canonical PCM format — every audio frame on the wire is exactly this. ----

/// Sample rate in Hz.
pub const SAMPLE_RATE: u32 = 48_000;
/// Channel count (stereo).
pub const CHANNELS: usize = 2;
/// Bytes per sample, per channel (s16le).
pub const BYTES_PER_SAMPLE: usize = 2;
/// Frame duration in milliseconds.
pub const FRAME_DURATION_MS: i64 = 20;
/// Samples per channel per frame (48000 * 20 / 1000).
pub const FRAME_SAMPLES: usize = 960;
/// Total PCM bytes in one canonical frame (FRAME_SAMPLES * CHANNELS * BYTES_PER_SAMPLE).
pub const FRAME_BYTES: usize = FRAME_SAMPLES * CHANNELS * BYTES_PER_SAMPLE; // 3840
/// PTS step per frame, in nanoseconds.
pub const FRAME_NANOS: i64 = FRAME_DURATION_MS * 1_000_000;

// ---- Runtime defaults --------------------------------------------------------

/// Default Opus bitrate (bits/sec). 510 kbps is the codec's max (libopus clamps
/// anything higher, so "512k" effectively means this) — transparent quality.
/// User-configurable via `--bitrate` / the UI.
pub const DEFAULT_BITRATE_BPS: i32 = 510_000;
/// Default client buffer depth (ms) = end-to-end latency AND how long a Wi-Fi
/// stall we can ride through without a gap. This is a *whole-home media* tool,
/// not a low-latency monitor: we trade a few seconds of startup delay for
/// dropout immunity (Snapcast defaults to 1 s, AirPlay 2 to ~2 s; 3 s is a
/// generous, resilient middle). User-adjustable up to [`MAX_BUFFER_MS`].
pub const DEFAULT_BUFFER_MS: i64 = 3_000;
/// Upper bound for the buffer slider — enough to ride out a truly awful link.
pub const MAX_BUFFER_MS: i64 = 15_000;
/// Default server lead (ms): how far ahead of `mono_now()` the server stamps a
/// frame's PTS, giving receivers budget to clock-sync and buffer before playout.
pub const DEFAULT_LEAD_MS: i64 = 50;

/// Default service ports. Fixed (not OS-assigned) so a client can reach a server
/// by IP alone (manual connect) and users can open predictable firewall holes.
/// The server falls back to an OS-assigned port if one is already in use.
pub const DEFAULT_AUDIO_PORT: u16 = 47010;
pub const DEFAULT_CLOCK_PORT: u16 = 47011;
pub const DEFAULT_VIDEO_PORT: u16 = 47012;
/// HTTP port the web client + WebSocket are served on (browse to `http://ip:PORT`).
pub const DEFAULT_HTTP_PORT: u16 = 47000;

// ---- The one monotonic clock -------------------------------------------------

struct MonoEpoch {
    instant: Instant,
    /// Wall time at process start (ns since UNIX epoch). Read exactly once, here,
    /// and NEVER again — the audio path only ever adds monotonic `elapsed()` to it.
    wall0_ns: i64,
}

fn epoch() -> &'static MonoEpoch {
    static EPOCH: OnceLock<MonoEpoch> = OnceLock::new();
    EPOCH.get_or_init(|| {
        let wall0_ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as i64)
            .unwrap_or(0);
        MonoEpoch {
            instant: Instant::now(),
            wall0_ns,
        }
    })
}

/// Wall-anchored monotonic nanoseconds: wall time captured once at process start
/// plus the monotonic elapsed since. The clock ticks monotonically (immune to NTP
/// steps) while cross-node offsets read as the real host skew rather than an
/// arbitrary process-start delta.
///
/// EVERY local-time value in the audio path — PTS stamping on the server, playout
/// deadlines and clock-offset translation on the client — MUST come from this one
/// function. Feeding `SystemTime` anywhere into that path silently corrupts the
/// offset math (it steps under NTP and mixes in inter-process start deltas).
pub fn mono_now() -> i64 {
    let e = epoch();
    e.wall0_ns + e.instant.elapsed().as_nanos() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_frame_math() {
        assert_eq!(FRAME_BYTES, 3840);
        assert_eq!(FRAME_NANOS, 20_000_000);
        assert_eq!(FRAME_SAMPLES * CHANNELS * BYTES_PER_SAMPLE, FRAME_BYTES);
    }

    #[test]
    fn mono_now_is_monotonic_nondecreasing() {
        let a = mono_now();
        let b = mono_now();
        let c = mono_now();
        assert!(a <= b && b <= c, "mono_now must be non-decreasing");
    }
}
