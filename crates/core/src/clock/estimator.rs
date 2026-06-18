//! NTP-style offset estimator.
//!
//! Ported from `ensemble/internal/clock/sample.go`. Keeps the last [`WINDOW_SIZE`]
//! samples and reports the median offset of the [`BEST_N`] with smallest RTT,
//! gated on [`CONFIDENT_SAMPLES`]. Integer-only math throughout (no float, no
//! averaging) so results are deterministic and immune to f64 precision loss.

/// One completed NTP-style exchange.
///
/// ```text
/// offset = ((t2 - t1) + (t3 - t4)) / 2   (master_ns - local_ns)
/// rtt    = (t4 - t1) - (t3 - t2)          (>= 0, smaller is better)
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sample {
    pub offset: i64,
    pub rtt: i64,
}

impl Sample {
    /// Compute offset and RTT from the four NTP timestamps (all ns). `t2`/`t3`
    /// come off the wire (untrusted): use wrapping deltas so a malicious or
    /// version-mismatched peer can't overflow-panic the follower thread. These
    /// are ns deltas where wraparound is already semantically tolerated; a wild
    /// reply just yields a garbage sample the best-RTT median filters out.
    pub fn new(t1: i64, t2: i64, t3: i64, t4: i64) -> Self {
        let off = t2.wrapping_sub(t1).wrapping_add(t3.wrapping_sub(t4)) / 2;
        let rtt = t4.wrapping_sub(t1).wrapping_sub(t3.wrapping_sub(t2));
        Sample {
            offset: off,
            rtt,
        }
    }
}

/// Window size — the "last 30" samples retained.
pub const WINDOW_SIZE: usize = 30;
/// Number of best-RTT samples whose offsets are medianed.
pub const BEST_N: usize = 5;
/// Samples required before the estimate is considered CONFIDENT enough to use.
///
/// Below this, the best-RTT median is too easily skewed by ONE delayed reply
/// (common on Wi-Fi) — a hundreds-of-ms offset error that would start playout out
/// of phase, which a rate correction cannot fix (it corrects rate, not a fixed
/// offset). The cold-start probe burst reaches this bar in a few hundred ms, so
/// gating on it costs little startup latency but guarantees a phase-accurate first
/// frame. Median-of-5 tolerates up to two outliers.
pub const CONFIDENT_SAMPLES: usize = 5;

/// Keeps the last [`WINDOW_SIZE`] samples and reports the median offset of the
/// [`BEST_N`] with smallest RTT. Not thread-safe; the follower's mutex guards it.
#[derive(Default)]
pub struct Estimator {
    ring: Vec<Sample>, // up to WINDOW_SIZE, oldest-first
    count: u64,        // total samples ever added (stats/debug)
}

impl Estimator {
    pub fn new() -> Self {
        Estimator {
            ring: Vec::with_capacity(WINDOW_SIZE),
            count: 0,
        }
    }

    /// Insert a sample, evicting the oldest when the window is full.
    pub fn add(&mut self, s: Sample) {
        self.count += 1;
        if self.ring.len() < WINDOW_SIZE {
            self.ring.push(s);
        } else {
            // Full: drop the oldest (front), append the newest.
            self.ring.rotate_left(1);
            *self.ring.last_mut().unwrap() = s;
        }
    }

    /// Current estimate, or `None` until [`CONFIDENT_SAMPLES`] exist. This is the
    /// gate for playout and PTS stamping.
    pub fn offset(&self) -> Option<i64> {
        if self.ring.len() < CONFIDENT_SAMPLES {
            return None;
        }
        self.estimate().map(|(off, _)| off)
    }

    /// `(median offset of best-RTT samples, smallest RTT in window)`, or `None`
    /// when empty. Used by stats/logging even before the confident gate.
    pub fn estimate(&self) -> Option<(i64, i64)> {
        if self.ring.is_empty() {
            return None;
        }
        // Sort a copy by RTT ascending to pick the best-RTT samples.
        let mut by_rtt = self.ring.clone();
        by_rtt.sort_by_key(|s| s.rtt);

        let n = BEST_N.min(by_rtt.len());
        let mut offsets: Vec<i64> = by_rtt[..n].iter().map(|s| s.offset).collect();
        offsets.sort_unstable();
        // Lower-middle median (deterministic, integer-only).
        let median = offsets[(offsets.len() - 1) / 2];
        Some((median, by_rtt[0].rtt))
    }

    /// Discard all samples (resync on master/endpoint change).
    pub fn reset(&mut self) {
        self.ring.clear();
    }

    /// Samples currently held.
    pub fn len(&self) -> usize {
        self.ring.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ring.is_empty()
    }

    /// Total samples ever added.
    pub fn count(&self) -> u64 {
        self.count
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_offset_and_rtt() {
        // Symmetric path: t1=0, t2=100, t3=110, t4=210.
        // offset = ((100-0)+(110-210))/2 = (100-100)/2 = 0
        // rtt    = (210-0)-(110-100) = 210-10 = 200
        let s = Sample::new(0, 100, 110, 210);
        assert_eq!(s.offset, 0);
        assert_eq!(s.rtt, 200);

        // Master 50 ahead of local: t1=0, t2=150, t3=160, t4=210.
        // offset = ((150-0)+(160-210))/2 = (150-50)/2 = 50
        let s = Sample::new(0, 150, 160, 210);
        assert_eq!(s.offset, 50);
        assert_eq!(s.rtt, 200);
    }

    #[test]
    fn sample_does_not_panic_on_extreme_wire_timestamps() {
        // t2/t3 are attacker-controlled (off the wire). Extreme values must wrap,
        // not overflow-panic the follower thread (debug builds have overflow checks).
        let realistic = 1_780_000_000_000_000_000i64; // ~2026 wall-ns
        let _ = Sample::new(realistic, i64::MAX, i64::MIN, realistic + 100);
        let _ = Sample::new(realistic, i64::MIN, i64::MAX, realistic + 100);
        let _ = Sample::new(i64::MIN, i64::MAX, i64::MIN, i64::MAX);
    }

    #[test]
    fn not_confident_below_threshold() {
        let mut e = Estimator::new();
        for _ in 0..CONFIDENT_SAMPLES - 1 {
            e.add(Sample {
                offset: 10,
                rtt: 5,
            });
        }
        assert_eq!(e.offset(), None, "must be unconfident below the gate");
        assert!(e.estimate().is_some(), "raw estimate exists sooner");
    }

    #[test]
    fn median_of_best_rtt_samples() {
        let mut e = Estimator::new();
        // Five low-RTT samples with offsets 100,101,102,103,104 (median 102) ...
        for (i, off) in [100, 101, 102, 103, 104].into_iter().enumerate() {
            e.add(Sample {
                offset: off,
                rtt: 1 + i as i64,
            });
        }
        // ... plus a high-RTT outlier with a wildly wrong offset that must be
        // excluded from the best-5.
        e.add(Sample {
            offset: 999_999,
            rtt: 10_000,
        });
        assert_eq!(e.offset(), Some(102));
    }

    #[test]
    fn window_evicts_oldest() {
        let mut e = Estimator::new();
        // Fill the window with rtt=1 offset=1, then push WINDOW_SIZE fresh
        // rtt=1 offset=500 samples so the best-5 are all the new ones.
        for _ in 0..WINDOW_SIZE {
            e.add(Sample { offset: 1, rtt: 1 });
        }
        for _ in 0..WINDOW_SIZE {
            e.add(Sample {
                offset: 500,
                rtt: 1,
            });
        }
        assert_eq!(e.len(), WINDOW_SIZE);
        assert_eq!(e.offset(), Some(500), "old samples must be fully evicted");
        assert_eq!(e.count(), (WINDOW_SIZE * 2) as u64);
    }

    #[test]
    fn reset_clears_window() {
        let mut e = Estimator::new();
        for _ in 0..10 {
            e.add(Sample { offset: 7, rtt: 3 });
        }
        e.reset();
        assert_eq!(e.len(), 0);
        assert_eq!(e.offset(), None);
    }
}
