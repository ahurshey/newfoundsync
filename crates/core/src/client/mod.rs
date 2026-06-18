//! Client side: discover a server, clock-sync to it, subscribe, and play its
//! audio in sync. This module currently holds the pure playout brain — the
//! [`jitterbuffer`] (reorder buffer) and the [`scheduler`] (PTS-deadline decision
//! logic). The network subscription, Opus decode, and rtrb hand-off to cpal land
//! with the transport/playback integration.

pub mod jitterbuffer;
pub mod playout;
pub mod scheduler;
pub mod subscribe;

#[allow(unused_imports)]
pub use jitterbuffer::JitterBuffer;
#[allow(unused_imports)]
pub use playout::{PlayoutDriver, PlayoutSink};
#[allow(unused_imports)]
pub use scheduler::{PlayoutStats, Scheduler, Tick};
#[allow(unused_imports)]
pub use subscribe::Subscriber;
