//! Playout driver: runs the [`Scheduler`] on its own thread, translating each
//! [`Tick`] into a sleep, a played frame, a silence frame, or an instant skip.
//!
//! The driver is decoupled from real audio output by the [`PlayoutSink`] trait:
//! the production sink decodes the payload (Opus → PCM) and writes it to the
//! lock-free `rtrb` ring feeding cpal; tests use a recording sink. This is the
//! dedicated scheduler thread the design calls for (precise `sleep_until`
//! deadlines, never an async task subject to executor jitter).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::clock::MasterClock;
use crate::config::mono_now;

use super::scheduler::{Scheduler, Tick};

/// Where played / silence frames go. `payload` is the wire payload (Opus or PCM);
/// the production sink decodes it, a test sink records it.
pub trait PlayoutSink: Send {
    /// A real frame reached its deadline — render it.
    fn play(&mut self, seq: u64, payload: &[u8]);
    /// A gap reached its deadline — render one frame of silence to keep cadence.
    fn silence(&mut self, seq: u64);
}

/// Cap on a single sleep so the driver rechecks `stop` and newly arrived frames
/// promptly even when the next deadline is far out.
const MAX_SLEEP: Duration = Duration::from_millis(50);
/// Poll interval while idle (disarmed / drained) or unsynced.
const IDLE_POLL: Duration = Duration::from_millis(20);
const UNSYNCED_POLL: Duration = Duration::from_millis(5);

/// A running playout driver. Stops its thread when dropped.
pub struct PlayoutDriver {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl PlayoutDriver {
    /// Start the driver over a shared scheduler, a clock, and an output sink.
    pub fn start(
        scheduler: Arc<Mutex<Scheduler>>,
        clock: Arc<dyn MasterClock + Send + Sync>,
        sink: Box<dyn PlayoutSink>,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let handle = thread::Builder::new()
            .name("playout".into())
            .spawn(move || drive(scheduler, clock, sink, stop_thread))
            .expect("spawn playout thread");
        PlayoutDriver {
            stop,
            handle: Some(handle),
        }
    }
}

impl Drop for PlayoutDriver {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn drive(
    scheduler: Arc<Mutex<Scheduler>>,
    clock: Arc<dyn MasterClock + Send + Sync>,
    mut sink: Box<dyn PlayoutSink>,
    stop: Arc<AtomicBool>,
) {
    while !stop.load(Ordering::Relaxed) {
        let now = mono_now();
        let tick = {
            let mut s = scheduler.lock().unwrap();
            s.tick(now, clock.as_ref())
        };
        match tick {
            Tick::Idle => thread::sleep(IDLE_POLL),
            Tick::Unsynced => thread::sleep(UNSYNCED_POLL),
            Tick::Sleep { until_local_ns } => {
                let d = until_local_ns - mono_now();
                if d > 0 {
                    let capped = (d as u64).min(MAX_SLEEP.as_nanos() as u64);
                    thread::sleep(Duration::from_nanos(capped));
                }
            }
            Tick::Played { seq, frame } => sink.play(seq, &frame),
            Tick::Silence { seq } => sink.silence(seq),
            Tick::SkippedLate { .. } => {} // re-tick immediately to drain the backlog
        }
    }
}
