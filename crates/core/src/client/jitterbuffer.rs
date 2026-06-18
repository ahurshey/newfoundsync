//! Bounded, seq-keyed reorder buffer for received audio frames.
//!
//! Ported from `ensemble/internal/sink/jitter.go`. Not thread-safe; the scheduler
//! that owns it serializes all access. Holds opaque payload bytes (the AUDIO
//! packet payload — Opus or PCM); decoding happens at play time in the scheduler's
//! driver, so frames that are skipped are never decoded.

use std::collections::hash_map::Entry;
use std::collections::HashMap;

/// Default capacity in frames (~5.1 s at 20 ms/frame).
pub const DEFAULT_CAPACITY: usize = 256;

struct Slot {
    pts: i64,
    payload: Vec<u8>,
}

/// A frame popped from the buffer for playout.
pub struct PoppedFrame {
    pub pts: i64,
    pub payload: Vec<u8>,
}

/// Bounded seq-keyed reorder buffer.
pub struct JitterBuffer {
    slots: HashMap<u64, Slot>,
    cap: usize,
    next_seq: u64, // seq the scheduler plays next
    has_next: bool, // false until the first frame fixes the seq origin
    max_seq: u64,  // highest seq ever inserted (gap-vs-end watermark)
    has_max: bool,
}

impl JitterBuffer {
    pub fn new(capacity: usize) -> Self {
        let cap = if capacity == 0 {
            DEFAULT_CAPACITY
        } else {
            capacity
        };
        JitterBuffer {
            slots: HashMap::new(),
            cap,
            next_seq: 0,
            has_next: false,
            max_seq: 0,
            has_max: false,
        }
    }

    /// Fix `next_seq` on the first frame of a session.
    pub fn set_origin(&mut self, seq: u64) {
        self.next_seq = seq;
        self.has_next = true;
    }

    /// The seq the scheduler will play next.
    pub fn next_seq(&self) -> u64 {
        self.next_seq
    }

    /// Store a frame. Returns `false` if rejected (already passed, or the buffer is
    /// full and `seq` is no nearer-future than its furthest slot). A duplicate seq
    /// overwrites idempotently (e.g. FEC double-delivery).
    pub fn insert(&mut self, seq: u64, pts: i64, payload: &[u8]) -> bool {
        if self.has_next && seq < self.next_seq {
            return false; // already passed → late
        }
        if !self.has_max || seq > self.max_seq {
            self.max_seq = seq;
            self.has_max = true;
        }
        if let Entry::Occupied(mut e) = self.slots.entry(seq) {
            // idempotent overwrite (e.g. FEC double-delivery)
            e.insert(Slot {
                pts,
                payload: payload.to_vec(),
            });
            return true;
        }
        if self.slots.len() >= self.cap {
            // full: evict the furthest-future slot if this one is nearer.
            let furthest = self.slots.keys().copied().max().unwrap();
            if seq >= furthest {
                return false; // this frame is furthest-out; drop it
            }
            self.slots.remove(&furthest);
        }
        self.slots.insert(
            seq,
            Slot {
                pts,
                payload: payload.to_vec(),
            },
        );
        true
    }

    /// Remove and return the slot for `seq`, or `None` if absent.
    pub fn pop(&mut self, seq: u64) -> Option<PoppedFrame> {
        self.slots.remove(&seq).map(|s| PoppedFrame {
            pts: s.pts,
            payload: s.payload,
        })
    }

    /// Bump `next_seq` after a play, silence, or skip.
    pub fn advance(&mut self) {
        self.next_seq += 1;
    }

    /// Whether the scheduler still has frames to play up to the highest received
    /// seq. When `false`, the buffer has drained to its end and the scheduler
    /// should go idle rather than synthesize silence past the last frame.
    pub fn has_pending(&self) -> bool {
        self.has_next && self.has_max && self.next_seq <= self.max_seq
    }

    /// Empty the buffer and clear the seq origin (new generation / re-anchor).
    pub fn reset(&mut self) {
        self.slots.clear();
        self.next_seq = 0;
        self.has_next = false;
        self.max_seq = 0;
        self.has_max = false;
    }

    pub fn len(&self) -> usize {
        self.slots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(b: u8) -> Vec<u8> {
        vec![b; 4]
    }

    #[test]
    fn insert_pop_advance_in_order() {
        let mut j = JitterBuffer::new(8);
        j.set_origin(10);
        assert!(j.insert(10, 100, &p(1)));
        assert!(j.insert(11, 120, &p(2)));
        assert!(j.has_pending());
        assert_eq!(j.pop(10).unwrap().payload, p(1));
        j.advance();
        assert_eq!(j.next_seq(), 11);
        assert_eq!(j.pop(11).unwrap().payload, p(2));
        j.advance();
        assert!(!j.has_pending(), "drained to end → not pending");
    }

    #[test]
    fn rejects_late_seq_below_next() {
        let mut j = JitterBuffer::new(8);
        j.set_origin(10);
        assert!(!j.insert(9, 0, &p(1)), "seq below next must be rejected as late");
    }

    #[test]
    fn duplicate_overwrites_idempotently() {
        let mut j = JitterBuffer::new(8);
        j.set_origin(0);
        assert!(j.insert(5, 100, &p(1)));
        assert!(j.insert(5, 100, &p(2)), "duplicate seq overwrites");
        assert_eq!(j.len(), 1);
        assert_eq!(j.pop(5).unwrap().payload, p(2));
    }

    #[test]
    fn full_buffer_evicts_furthest_future_for_nearer() {
        let mut j = JitterBuffer::new(3);
        j.set_origin(0);
        assert!(j.insert(10, 0, &p(1)));
        assert!(j.insert(20, 0, &p(2)));
        assert!(j.insert(30, 0, &p(3)));
        // full; a nearer frame (5) evicts the furthest (30)
        assert!(j.insert(5, 0, &p(4)));
        assert_eq!(j.len(), 3);
        assert!(j.pop(30).is_none(), "furthest-future slot evicted");
        assert!(j.pop(5).is_some(), "nearer frame admitted");
    }

    #[test]
    fn full_buffer_drops_furthest_out_frame() {
        let mut j = JitterBuffer::new(2);
        j.set_origin(0);
        assert!(j.insert(10, 0, &p(1)));
        assert!(j.insert(20, 0, &p(2)));
        // full; a frame further out than the furthest (30 > 20) is dropped
        assert!(!j.insert(30, 0, &p(3)));
        assert_eq!(j.len(), 2);
    }

    #[test]
    fn reset_clears_everything() {
        let mut j = JitterBuffer::new(8);
        j.set_origin(3);
        j.insert(3, 0, &p(1));
        j.reset();
        assert_eq!(j.len(), 0);
        assert!(!j.has_pending());
    }
}
