//! NTP-style master-anchored clock: a server that stamps receive/send times on
//! clock requests, and a follower that probes it and maintains an offset estimate.
//!
//! Ported from `ensemble/internal/clock`. This module currently holds the pure,
//! networking-free pieces (the offset [`estimator`] and the clock-datagram
//! [`payload`] codec); the UDP server and follower loop land with the transport
//! step. The offset math and the "median of the 5 best-RTT of the last 30,
//! confident at 5 samples" gate are the proven heart of tight sync — kept verbatim.

pub mod estimator;
pub mod follower;
pub mod payload;
pub mod server;

#[allow(unused_imports)]
pub use follower::{Follower, FollowerStats};
#[allow(unused_imports)]
pub use server::ClockServer;

// Re-export the public surface. These are consumed by the transport/server/client
// steps still landing; allow unused until those wire in.
#[allow(unused_imports)]
pub use estimator::{Estimator, Sample, BEST_N, CONFIDENT_SAMPLES, WINDOW_SIZE};
#[allow(unused_imports)]
pub use payload::{
    decode_clock, encode_clock, ClockPacket, CLOCK_PACKET_SIZE, CLOCK_PAYLOAD_SIZE,
};

/// Master-clock translation used by the playout scheduler. The real follower
/// implements this over its offset estimate; tests use a fake.
///
/// Both methods return `None` until the clock is CONFIDENT (see
/// [`estimator::CONFIDENT_SAMPLES`]); the scheduler treats `None` as "unsynced"
/// and holds playout — a phase-accurate first frame matters more than a fast one.
pub trait MasterClock {
    /// Convert a master-clock instant (ns) to local monotonic ns, or `None` if unsynced.
    fn master_to_local(&self, master_ns: i64) -> Option<i64>;
    /// Current master-clock instant (ns), or `None` if unsynced.
    fn master_now(&self) -> Option<i64>;
}
