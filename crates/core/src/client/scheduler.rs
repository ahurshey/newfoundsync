//! PTS-deadline playout scheduler — the phase lock for tight multiroom sync.
//!
//! Ported from the `ensemble/internal/sink/sink.go` loop, reduced to the v1
//! decision logic (no rate servo / resampler yet — those are later refinements).
//!
//! The design separates *policy* from *blocking*: [`Scheduler::tick`] is a pure,
//! non-sleeping decision function over the current time and clock, so it is
//! deterministically unit-testable with a fake clock and synthetic frames. The
//! real driver thread (landing with the cpal/transport integration) loops:
//! call `tick(mono_now(), clock)`, then sleep / decode / write / skip per the
//! returned [`Tick`].
//!
//! Deadline: `local = master_to_local(pts + buffer_ns + delay_offset_ns)`.
//! - **Gap** at its proper deadline → emit one silence frame (keeps device cadence).
//! - **Late** slot (> one frame past its deadline) → drop instantly; writing
//!   silence for it would delay every later frame forever and the backlog could
//!   never drain.

use crate::clock::MasterClock;
use crate::config;

use super::jitterbuffer::JitterBuffer;

/// Snapshot of playout counters for telemetry / the UI.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PlayoutStats {
    pub played: u64,
    pub silence: u64,
    pub late_drop: u64,
    pub stale_gen: u64,
    pub buffered: usize,
}

/// The outcome of one scheduler step.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Tick {
    /// Disarmed or nothing pending up to the highest received seq — go idle until
    /// a frame arrives or the watchdog fires.
    Idle,
    /// Clock not yet CONFIDENT — wait a short interval and retry (do not play).
    Unsynced,
    /// Next slot's deadline is in the future; sleep until this local-ns instant.
    Sleep { until_local_ns: i64 },
    /// Deadline reached with a real frame present — decode `frame` and write it.
    Played { seq: u64, frame: Vec<u8> },
    /// Gap at its proper deadline — write one silence frame.
    Silence { seq: u64 },
    /// Slot was more than one frame past its deadline — dropped; re-tick at once.
    SkippedLate { seq: u64 },
}

/// Per-session playout state: jitter buffer + deadline math + counters.
pub struct Scheduler {
    jb: JitterBuffer,
    gen: u32,
    armed: bool,
    origin_seq: u64,
    origin_pts: i64,
    origin_set: bool,
    buffer_ns: i64,
    /// Per-client output-delay offset, ADDED to the deadline (positive = play
    /// later). The manual alignment slider drives this; changing it re-anchors.
    /// We never auto-subtract device latency mid-session (that jolts the schedule
    /// — ensemble's D63 lesson); alignment is user-driven and re-primes on change.
    delay_offset_ns: i64,
    /// Consecutive late skips since the last successful play. A long run means a
    /// persistent timeline offset (e.g. a high-latency capture source), so we
    /// re-anchor the origin to resync rather than drop frames forever.
    late_run: u32,
    stats: PlayoutStats,
}

/// Re-anchor after this many consecutive late skips (≈ this×20 ms). Well above
/// the normal prime-burst drop at startup, so steady playout never trips it.
const REANCHOR_LATE_RUN: u32 = 50;

impl Scheduler {
    pub fn new(capacity: usize, buffer_ms: i64) -> Self {
        Scheduler {
            jb: JitterBuffer::new(capacity),
            gen: 0,
            armed: false,
            origin_seq: 0,
            origin_pts: 0,
            origin_set: false,
            buffer_ns: buffer_ms.max(1) * 1_000_000,
            delay_offset_ns: 0,
            late_run: 0,
            stats: PlayoutStats::default(),
        }
    }

    /// Arm for a new generation: discard queued frames, set gen, clear per-session
    /// counters, re-establish the seq/pts origin on the next pushed frame.
    pub fn reset(&mut self, gen: u32) {
        self.jb.reset();
        self.gen = gen;
        self.armed = true;
        self.origin_set = false;
        self.stats = PlayoutStats::default();
    }

    /// End the session: stop playing, discard buffered frames.
    pub fn disarm(&mut self) {
        self.armed = false;
        self.jb.reset();
        self.stats.buffered = 0;
    }

    pub fn armed_gen(&self) -> (u32, bool) {
        (self.gen, self.armed)
    }

    pub fn stats(&self) -> PlayoutStats {
        self.stats
    }

    /// Update the jitter-buffer depth live (takes effect on the next slot).
    pub fn set_buffer_ms(&mut self, ms: i64) {
        self.buffer_ns = ms.max(1) * 1_000_000;
    }

    /// Set the per-client delay offset (ns) and re-anchor: clear the buffer and
    /// re-establish the origin on the next pushed frame, so the seq/pts math
    /// doesn't drift across the change.
    pub fn set_delay_offset_ns(&mut self, ns: i64) {
        self.delay_offset_ns = ns;
        self.jb.reset();
        self.origin_set = false;
        self.stats.buffered = 0;
    }

    pub fn delay_offset_ns(&self) -> i64 {
        self.delay_offset_ns
    }

    /// Enqueue a received frame. Drops + counts stale-gen / unarmed / late frames.
    /// Sets the session origin on the first accepted frame.
    pub fn push(&mut self, gen: u32, seq: u64, pts: i64, payload: &[u8]) {
        if !self.armed || gen != self.gen {
            self.stats.stale_gen += 1;
            return;
        }
        if !self.origin_set {
            self.jb.set_origin(seq);
            self.origin_seq = seq;
            self.origin_pts = pts;
            self.origin_set = true;
        }
        if self.jb.insert(seq, pts, payload) {
            self.stats.buffered = self.jb.len();
        } else {
            self.stats.late_drop += 1;
        }
    }

    /// Master-clock PTS for a seq, from the session origin.
    fn slot_pts(&self, seq: u64) -> i64 {
        self.origin_pts + (seq.wrapping_sub(self.origin_seq) as i64) * config::FRAME_NANOS
    }

    /// One non-blocking decision step. See [`Tick`].
    pub fn tick(&mut self, now_ns: i64, clock: &dyn MasterClock) -> Tick {
        if !self.armed || !self.jb.has_pending() {
            return Tick::Idle;
        }
        let seq = self.jb.next_seq();
        let target = self.slot_pts(seq) + self.buffer_ns + self.delay_offset_ns;
        let local = match clock.master_to_local(target) {
            Some(l) => l,
            None => return Tick::Unsynced,
        };
        if now_ns < local {
            return Tick::Sleep {
                until_local_ns: local,
            };
        }

        // Deadline reached or passed: commit the slot now.
        let present = self.jb.pop(seq);
        let late = now_ns > local + config::FRAME_NANOS;
        self.jb.advance();
        self.stats.buffered = self.jb.len();

        if late {
            if present.is_some() {
                self.stats.late_drop += 1;
            }
            self.late_run += 1;
            if self.late_run >= REANCHOR_LATE_RUN {
                // Persistent offset (e.g. a high-latency capture source): the
                // timeline anchor is stale. Re-anchor to the live stream — the
                // next pushed frame re-establishes origin at "now".
                self.late_run = 0;
                self.origin_set = false;
                self.jb.reset();
                self.stats.buffered = 0;
            }
            return Tick::SkippedLate { seq };
        }
        self.late_run = 0;
        match present {
            Some(slot) => {
                self.stats.played += 1;
                Tick::Played {
                    seq,
                    frame: slot.payload,
                }
            }
            None => {
                self.stats.silence += 1;
                Tick::Silence { seq }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{FRAME_NANOS, FRAME_SAMPLES, CHANNELS};

    /// Fake clock with a fixed offset: `master_to_local(m) = m - offset`. `None`
    /// offset models "unsynced".
    struct FakeClock {
        offset: Option<i64>,
    }
    impl MasterClock for FakeClock {
        fn master_to_local(&self, master_ns: i64) -> Option<i64> {
            self.offset.map(|o| master_ns - o)
        }
        fn master_now(&self) -> Option<i64> {
            self.offset
        }
    }

    fn frame(b: u8) -> Vec<u8> {
        vec![b; 8]
    }

    const BUFFER_MS: i64 = 200;
    const BUFFER_NS: i64 = BUFFER_MS * 1_000_000;

    fn armed(offset: Option<i64>) -> (Scheduler, FakeClock) {
        let mut s = Scheduler::new(64, BUFFER_MS);
        s.reset(1);
        (s, FakeClock { offset })
    }

    #[test]
    fn idle_when_no_frames() {
        let (mut s, c) = armed(Some(0));
        assert_eq!(s.tick(0, &c), Tick::Idle);
    }

    #[test]
    fn unsynced_gate_holds_playout() {
        let (mut s, c) = armed(None);
        s.push(1, 0, 0, &frame(1));
        // far past any deadline, but clock is unsynced → must not play
        assert_eq!(s.tick(10 * FRAME_NANOS, &c), Tick::Unsynced);
    }

    #[test]
    fn sleeps_until_deadline_then_plays() {
        let (mut s, c) = armed(Some(0));
        // origin seq=0 pts=0; deadline = pts + buffer + 0 = BUFFER_NS (offset 0).
        s.push(1, 0, 0, &frame(7));
        assert_eq!(
            s.tick(0, &c),
            Tick::Sleep {
                until_local_ns: BUFFER_NS
            }
        );
        // at the deadline → play
        match s.tick(BUFFER_NS, &c) {
            Tick::Played { seq, frame } => {
                assert_eq!(seq, 0);
                assert_eq!(frame, frame_bytes(7));
            }
            other => panic!("expected Played, got {other:?}"),
        }
        assert_eq!(s.stats().played, 1);
    }

    fn frame_bytes(b: u8) -> Vec<u8> {
        vec![b; 8]
    }

    #[test]
    fn offset_shifts_local_deadline() {
        // master is 1_000 ns ahead of local → local deadline = target - 1_000.
        let (mut s, c) = armed(Some(1_000));
        s.push(1, 0, 0, &frame(1));
        assert_eq!(
            s.tick(0, &c),
            Tick::Sleep {
                until_local_ns: BUFFER_NS - 1_000
            }
        );
    }

    #[test]
    fn gap_emits_silence_at_its_deadline() {
        let (mut s, c) = armed(Some(0));
        // origin at seq 5; seq 7 present, seq 6 absent.
        s.push(1, 5, 0, &frame(5)); // origin_seq=5, origin_pts=0
        s.push(1, 7, 2 * FRAME_NANOS, &frame(7));

        // seq 5 plays at its deadline (pts=0)
        let d5 = BUFFER_NS;
        assert!(matches!(s.tick(d5, &c), Tick::Played { seq: 5, .. }));
        // seq 6 is a gap → silence at deadline pts = FRAME_NANOS
        let d6 = FRAME_NANOS + BUFFER_NS;
        assert_eq!(s.tick(d6, &c), Tick::Silence { seq: 6 });
        // seq 7 plays
        let d7 = 2 * FRAME_NANOS + BUFFER_NS;
        assert!(matches!(s.tick(d7, &c), Tick::Played { seq: 7, .. }));
        assert_eq!(s.stats().played, 2);
        assert_eq!(s.stats().silence, 1);
    }

    #[test]
    fn late_slot_is_skipped_not_silenced() {
        let (mut s, c) = armed(Some(0));
        s.push(1, 0, 0, &frame(1));
        // now is more than one frame past the deadline → skip, drop the frame
        let way_late = BUFFER_NS + FRAME_NANOS + 1;
        assert_eq!(s.tick(way_late, &c), Tick::SkippedLate { seq: 0 });
        assert_eq!(s.stats().late_drop, 1);
        assert_eq!(s.stats().played, 0);
    }

    #[test]
    fn within_one_frame_of_deadline_still_plays() {
        let (mut s, c) = armed(Some(0));
        s.push(1, 0, 0, &frame(1));
        // exactly one frame past the deadline is NOT late (strict >)
        let edge = BUFFER_NS + FRAME_NANOS;
        assert!(matches!(s.tick(edge, &c), Tick::Played { seq: 0, .. }));
    }

    #[test]
    fn backlog_of_late_frames_drains_via_repeated_skip() {
        let (mut s, c) = armed(Some(0));
        for seq in 0..5u64 {
            s.push(1, seq, seq as i64 * FRAME_NANOS, &frame(seq as u8));
        }
        // We are far in the future: every early slot is late and skipped instantly.
        let now = 100 * FRAME_NANOS;
        let mut skipped = 0;
        loop {
            match s.tick(now, &c) {
                Tick::SkippedLate { .. } => skipped += 1,
                Tick::Idle => break,
                other => panic!("expected skip/idle while draining, got {other:?}"),
            }
        }
        assert_eq!(skipped, 5, "all late frames skipped, then idle");
    }

    #[test]
    fn stale_gen_frames_dropped_and_counted() {
        let (mut s, c) = armed(Some(0));
        s.push(2, 0, 0, &frame(1)); // wrong gen (armed gen is 1)
        assert_eq!(s.stats().stale_gen, 1);
        assert_eq!(s.tick(BUFFER_NS, &c), Tick::Idle, "nothing buffered");
    }

    #[test]
    fn set_delay_offset_reanchors_origin() {
        let (mut s, c) = armed(Some(0));
        s.push(1, 0, 0, &frame(1));
        s.set_delay_offset_ns(30 * 1_000_000); // +30 ms, re-anchor
        // buffer cleared, origin reset → idle until a fresh frame
        assert_eq!(s.tick(BUFFER_NS, &c), Tick::Idle);
        // next frame at seq 100 becomes the new origin; deadline includes +30ms
        s.push(1, 100, 0, &frame(2));
        assert_eq!(
            s.tick(0, &c),
            Tick::Sleep {
                until_local_ns: BUFFER_NS + 30 * 1_000_000
            }
        );
    }

    #[test]
    fn canonical_silence_frame_size_sanity() {
        // Guards the silence-frame size the driver will write for gaps.
        assert_eq!(FRAME_SAMPLES * CHANNELS, 1920);
    }
}
