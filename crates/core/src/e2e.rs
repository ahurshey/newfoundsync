//! End-to-end loopback test of the full sync pipeline, with no audio hardware,
//! mDNS, or codec involved: a clock server + audio server on loopback, and a
//! client that clock-syncs, subscribes, jitter-buffers, and deadline-schedules
//! synthetic 20 ms frames into a recording sink.
//!
//! Proves: client-initiated subscription + UDP return-path fan-out, the
//! confident-clock gate, jitter buffering, and ordered PTS-deadline playout —
//! the whole pipeline minus real audio I/O. Timings are deliberately generous to
//! stay robust to thread scheduling.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::clock::{ClockServer, Follower, MasterClock};
use crate::client::playout::{PlayoutDriver, PlayoutSink};
use crate::client::scheduler::Scheduler;
use crate::client::subscribe::Subscriber;
use crate::config;
use crate::server::AudioServer;

/// Sink that records the seq of every frame it is asked to play.
struct CollectSink {
    played: Arc<Mutex<Vec<u64>>>,
    silence: Arc<AtomicU64>,
}

impl PlayoutSink for CollectSink {
    fn play(&mut self, seq: u64, _payload: &[u8]) {
        self.played.lock().unwrap().push(seq);
    }
    fn silence(&mut self, _seq: u64) {
        self.silence.fetch_add(1, Ordering::Relaxed);
    }
}

fn wait_until<F: Fn() -> bool>(timeout: Duration, f: F) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if f() {
            return true;
        }
        thread::sleep(Duration::from_millis(10));
    }
    f()
}

#[test]
fn end_to_end_loopback_playout_in_order() {
    // --- Server side: clock + audio source on loopback ---
    let clock_srv = ClockServer::start("127.0.0.1:0").expect("clock server");
    let audio_srv = AudioServer::start("127.0.0.1:0", 1, config::DEFAULT_LEAD_MS, 32)
        .expect("audio server");

    // --- Client side: follower + scheduler + driver + subscription ---
    let follower: Arc<Follower> = Arc::new(Follower::start(clock_srv.local_addr()).expect("follower"));
    // Small buffer keeps this loopback test fast (the shipped default is multi-second).
    let scheduler = Arc::new(Mutex::new(Scheduler::new(256, 200)));
    let played = Arc::new(Mutex::new(Vec::<u64>::new()));
    let silence = Arc::new(AtomicU64::new(0));
    let sink = CollectSink {
        played: played.clone(),
        silence: silence.clone(),
    };
    let clock_dyn: Arc<dyn MasterClock + Send + Sync> = follower.clone();
    let _driver = PlayoutDriver::start(scheduler.clone(), clock_dyn, Box::new(sink));
    let _sub = Subscriber::start(audio_srv.local_addr(), scheduler.clone()).expect("subscriber");

    // Wait for the cold-start burst to reach confident clock sync.
    assert!(
        wait_until(Duration::from_secs(3), || follower.is_synced()),
        "client should reach confident clock sync"
    );
    // Wait for the server to register the subscription.
    assert!(
        wait_until(Duration::from_secs(1), || audio_srv.subscriber_count() == 1),
        "server should register exactly one subscriber"
    );

    // Emit 60 frames at ~20 ms cadence (~1.2 s of audio).
    const N: u32 = 60;
    for i in 0..N {
        let payload = vec![(i & 0xff) as u8; 16];
        audio_srv.release_frame(&payload);
        thread::sleep(Duration::from_millis(20));
    }
    // Let the ~250 ms playout pipeline (50 ms lead + 200 ms buffer) drain.
    thread::sleep(Duration::from_millis(600));

    let got = played.lock().unwrap().clone();
    assert!(
        got.len() >= 30,
        "expected >=30 frames played, got {} (silence={})",
        got.len(),
        silence.load(Ordering::Relaxed)
    );
    // Strictly increasing seqs: ordered, de-duplicated playout.
    for w in got.windows(2) {
        assert!(w[1] > w[0], "played seqs must strictly increase: {got:?}");
    }
    // Sanity: we should have played most of what we sent.
    assert!(
        got.len() <= N as usize,
        "played more frames ({}) than sent ({N})",
        got.len()
    );
}

/// Sink that decodes each played payload and checks it carries the value the
/// server encoded for that seq — proving codec + transport + scheduler compose
/// and the payload survives the wire bit-exact.
struct DecodingSink {
    dec: crate::codec::Decoder,
    played: Arc<Mutex<Vec<u64>>>,
    mismatches: Arc<AtomicU64>,
}

impl PlayoutSink for DecodingSink {
    fn play(&mut self, seq: u64, payload: &[u8]) {
        match self.dec.decode(payload) {
            Ok(frame) => {
                let expected = (seq % 1000) as i16;
                if frame.iter().all(|&s| s == expected) {
                    self.played.lock().unwrap().push(seq);
                } else {
                    self.mismatches.fetch_add(1, Ordering::Relaxed);
                }
            }
            Err(_) => {
                self.mismatches.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
    fn silence(&mut self, _seq: u64) {}
}

#[test]
fn end_to_end_pcm_codec_integrity() {
    use crate::codec::{CodecKind, Encoder, FRAME_SAMPLE_COUNT};

    let clock_srv = ClockServer::start("127.0.0.1:0").expect("clock server");
    let audio_srv =
        AudioServer::start("127.0.0.1:0", 1, config::DEFAULT_LEAD_MS, 32).expect("audio server");

    let follower: Arc<Follower> =
        Arc::new(Follower::start(clock_srv.local_addr()).expect("follower"));
    // Small buffer keeps this loopback test fast (the shipped default is multi-second).
    let scheduler = Arc::new(Mutex::new(Scheduler::new(256, 200)));
    let played = Arc::new(Mutex::new(Vec::<u64>::new()));
    let mismatches = Arc::new(AtomicU64::new(0));
    let sink = DecodingSink {
        dec: crate::codec::Decoder::new(CodecKind::Pcm).unwrap(),
        played: played.clone(),
        mismatches: mismatches.clone(),
    };
    let clock_dyn: Arc<dyn MasterClock + Send + Sync> = follower.clone();
    let _driver = PlayoutDriver::start(scheduler.clone(), clock_dyn, Box::new(sink));
    let _sub = Subscriber::start(audio_srv.local_addr(), scheduler.clone()).expect("subscriber");

    assert!(wait_until(Duration::from_secs(3), || follower.is_synced()));
    assert!(wait_until(Duration::from_secs(1), || audio_srv.subscriber_count() == 1));

    // Server encodes one PCM frame per seq, filled with (seq % 1000).
    const N: u32 = 60;
    let mut enc = Encoder::new(CodecKind::Pcm, 0).unwrap();
    for i in 0..N {
        let v = (i as u64 % 1000) as i16;
        let frame = vec![v; FRAME_SAMPLE_COUNT];
        let payload = enc.encode(&frame).unwrap();
        audio_srv.release_frame(&payload);
        thread::sleep(Duration::from_millis(20));
    }
    thread::sleep(Duration::from_millis(600));

    assert_eq!(
        mismatches.load(Ordering::Relaxed),
        0,
        "every decoded frame must carry its seq's value"
    );
    let got = played.lock().unwrap().clone();
    assert!(got.len() >= 30, "expected >=30 verified frames, got {}", got.len());
    for w in got.windows(2) {
        assert!(w[1] > w[0], "verified seqs must strictly increase: {got:?}");
    }
}
