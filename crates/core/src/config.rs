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
/// Lower bound. Below roughly this the buffer is thinner than normal Wi-Fi jitter, so playout
/// underruns constantly and the stream crackles instead of playing.
pub const MIN_BUFFER_MS: i64 = 200;

/// Clamp a requested client buffer into the supported range.
///
/// The single definition of these bounds. They were previously open-coded as a bare `200` and
/// `MAX_BUFFER_MS` at three call sites in the GUI, and — the actual bug — the HEADLESS entry point
/// never clamped at all, so `--buffer-ms 999999` or a negative value went straight into the pipeline.
/// The browser clamps the value it receives too (`app.js`), which is defence in depth, not a reason to
/// ship a nonsense value: the operator's own UI would still be displaying it.
pub fn clamp_buffer_ms(ms: i64) -> i64 {
    ms.clamp(MIN_BUFFER_MS, MAX_BUFFER_MS)
}
/// Default server lead (ms): how far ahead of `mono_now()` the server stamps a
/// frame's PTS, giving receivers budget to clock-sync and buffer before playout.
pub const DEFAULT_LEAD_MS: i64 = 50;

/// HTTP(S) port the web client + WebSocket are served on (browse to `https://ip:PORT`). The only
/// port this app uses — the separate audio/clock/video UDP ports of the pre-WebSocket design are
/// gone, so there is exactly one firewall hole to open.
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
        assert_eq!(FRAME_SAMPLES * CHANNELS * BYTES_PER_SAMPLE, FRAME_BYTES);
        // 48 kHz for FRAME_DURATION_MS must yield exactly FRAME_SAMPLES — if these ever disagree the
        // encoder gets short frames and Opus rejects them.
        assert_eq!(SAMPLE_RATE as i64 * FRAME_DURATION_MS / 1000, FRAME_SAMPLES as i64);
    }

    #[test]
    fn mono_now_is_monotonic_nondecreasing() {
        let a = mono_now();
        let b = mono_now();
        let c = mono_now();
        assert!(a <= b && b <= c, "mono_now must be non-decreasing");
    }

    #[test]
    fn clamp_buffer_ms_bounds_every_input() {
        // In-range values pass through untouched.
        assert_eq!(clamp_buffer_ms(DEFAULT_BUFFER_MS), DEFAULT_BUFFER_MS);
        assert_eq!(clamp_buffer_ms(MIN_BUFFER_MS), MIN_BUFFER_MS);
        assert_eq!(clamp_buffer_ms(MAX_BUFFER_MS), MAX_BUFFER_MS);
        // The cases the headless CLI used to pass through unclamped.
        assert_eq!(clamp_buffer_ms(999_999), MAX_BUFFER_MS);
        assert_eq!(clamp_buffer_ms(0), MIN_BUFFER_MS);
        assert_eq!(clamp_buffer_ms(-5_000), MIN_BUFFER_MS, "a negative buffer must not survive");
        assert_eq!(clamp_buffer_ms(i64::MAX), MAX_BUFFER_MS);
        assert_eq!(clamp_buffer_ms(i64::MIN), MIN_BUFFER_MS);
    }

    #[test]
    fn buffer_bounds_are_coherent() {
        // A default outside its own bounds would mean the untouched value gets silently changed.
        assert!(MIN_BUFFER_MS < MAX_BUFFER_MS);
        assert!(MIN_BUFFER_MS <= DEFAULT_BUFFER_MS && DEFAULT_BUFFER_MS <= MAX_BUFFER_MS);
        // The lead must fit inside the smallest buffer, or the shallowest setting can never prime.
        assert!(DEFAULT_LEAD_MS < MIN_BUFFER_MS);
    }
}
