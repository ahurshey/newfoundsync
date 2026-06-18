//! Clock follower: probes a master's clock and maintains the offset estimate that
//! the playout scheduler translates deadlines through.
//!
//! Ported from the follower half of `ensemble/internal/clock/clock.go`. Fires a
//! cold-start burst on launch so the estimator reaches CONFIDENT
//! ([`super::estimator::CONFIDENT_SAMPLES`]) within a few hundred ms — a
//! phase-accurate first frame matters more than a fast one — then steady 1 Hz
//! probing. v1 binds one follower to one master for its lifetime; switching
//! servers tears down and recreates the follower (simpler than ensemble's live
//! re-point, which it does not need).

use std::collections::HashMap;
use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::clock::estimator::{Estimator, Sample};
use crate::clock::payload::{decode_clock, encode_clock, CLOCK_PACKET_SIZE};
use crate::clock::MasterClock;
use crate::config::mono_now;
use crate::proto::msg;

const COLD_START_BURST: usize = 8;
const COLD_START_SPACING: Duration = Duration::from_millis(40);
const PROBE_INTERVAL: Duration = Duration::from_secs(1);
const PENDING_TTL_NS: i64 = 5_000_000_000; // prune lost probes after 5 s

struct Inner {
    est: Estimator,
    seq: u64,
    pending: HashMap<u64, i64>, // probe seq -> t1 (local send time)
    synced: bool,
    probes: u64,
    replies: u64,
}

/// Snapshot of follower state for diagnostics / the UI.
#[derive(Clone, Copy, Debug, Default)]
pub struct FollowerStats {
    pub synced: bool,
    pub offset_ns: i64,
    pub rtt_ns: i64,
    pub samples: usize,
    pub probes: u64,
    pub replies: u64,
}

/// A running clock follower. Stops its threads when dropped.
pub struct Follower {
    socket: Arc<UdpSocket>,
    dst: SocketAddr,
    gen: u32,
    inner: Arc<Mutex<Inner>>,
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
}

impl Follower {
    /// Bind an ephemeral UDP socket and start probing `master`.
    pub fn start(master: SocketAddr) -> io::Result<Self> {
        Self::start_with_gen(master, 0)
    }

    /// As [`start`], with an explicit session generation for the probes (the
    /// server echoes it; v1 uses 0).
    pub fn start_with_gen(master: SocketAddr, gen: u32) -> io::Result<Self> {
        // Bind a wildcard ephemeral socket on the same IP family as the master.
        let bind: SocketAddr = if master.is_ipv6() {
            "[::]:0".parse().unwrap()
        } else {
            "0.0.0.0:0".parse().unwrap()
        };
        let socket = Arc::new(UdpSocket::bind(bind)?);
        socket.set_read_timeout(Some(Duration::from_millis(200)))?;

        let inner = Arc::new(Mutex::new(Inner {
            est: Estimator::new(),
            seq: 0,
            pending: HashMap::new(),
            synced: false,
            probes: 0,
            replies: 0,
        }));
        let stop = Arc::new(AtomicBool::new(false));

        let mut f = Follower {
            socket,
            dst: master,
            gen,
            inner,
            stop,
            threads: Vec::new(),
        };
        f.spawn_recv()?;
        f.spawn_probe()?;
        Ok(f)
    }

    fn spawn_recv(&mut self) -> io::Result<()> {
        let socket = self.socket.clone();
        let inner = self.inner.clone();
        let stop = self.stop.clone();
        let h = thread::Builder::new()
            .name("clock-follower-recv".into())
            .spawn(move || recv_loop(socket, inner, stop))?;
        self.threads.push(h);
        Ok(())
    }

    fn spawn_probe(&mut self) -> io::Result<()> {
        let socket = self.socket.clone();
        let inner = self.inner.clone();
        let stop = self.stop.clone();
        let dst = self.dst;
        let gen = self.gen;
        let h = thread::Builder::new()
            .name("clock-follower-probe".into())
            .spawn(move || probe_loop(socket, inner, stop, dst, gen))?;
        self.threads.push(h);
        Ok(())
    }

    /// Follower diagnostics.
    pub fn stats(&self) -> FollowerStats {
        let g = self.inner.lock().unwrap();
        let (offset_ns, rtt_ns, ok) = match g.est.estimate() {
            Some((off, rtt)) => (off, rtt, g.est.offset().is_some()),
            None => (0, 0, false),
        };
        FollowerStats {
            synced: ok,
            offset_ns,
            rtt_ns,
            samples: g.est.len(),
            probes: g.probes,
            replies: g.replies,
        }
    }

    /// Whether the offset estimate is CONFIDENT enough to drive playout.
    pub fn is_synced(&self) -> bool {
        self.inner.lock().unwrap().est.offset().is_some()
    }
}

impl MasterClock for Follower {
    fn master_to_local(&self, master_ns: i64) -> Option<i64> {
        let off = self.inner.lock().unwrap().est.offset()?;
        // Saturating: master_ns can be an attacker-influenced PTS and `off` an
        // extreme estimate — clamp rather than overflow-panic this hot path.
        Some(master_ns.saturating_sub(off))
    }
    fn master_now(&self) -> Option<i64> {
        let off = self.inner.lock().unwrap().est.offset()?;
        Some(mono_now().saturating_add(off))
    }
}

impl Drop for Follower {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for h in self.threads.drain(..) {
            let _ = h.join();
        }
    }
}

/// Send one clock request to the master.
fn send_probe(socket: &UdpSocket, inner: &Mutex<Inner>, dst: SocketAddr, gen: u32) {
    let (seq, t1) = {
        let mut g = inner.lock().unwrap();
        let t1 = mono_now();
        let seq = g.seq;
        g.seq += 1;
        g.pending.insert(seq, t1);
        // prune lost probes
        let cutoff = t1 - PENDING_TTL_NS;
        g.pending.retain(|_, &mut sent| sent >= cutoff);
        g.probes += 1;
        (seq, t1)
    };
    let mut buf = [0u8; CLOCK_PACKET_SIZE];
    encode_clock(&mut buf, msg::CLOCK_REQ, gen, seq, t1, 0, 0);
    let _ = socket.send_to(&buf, dst);
}

fn probe_loop(
    socket: Arc<UdpSocket>,
    inner: Arc<Mutex<Inner>>,
    stop: Arc<AtomicBool>,
    dst: SocketAddr,
    gen: u32,
) {
    // Cold-start burst: reach CONFIDENT in a few hundred ms.
    for i in 0..COLD_START_BURST {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        send_probe(&socket, &inner, dst, gen);
        if i != COLD_START_BURST - 1 {
            interruptible_sleep(COLD_START_SPACING, &stop);
        }
    }
    // Steady 1 Hz.
    while !stop.load(Ordering::Relaxed) {
        interruptible_sleep(PROBE_INTERVAL, &stop);
        if stop.load(Ordering::Relaxed) {
            return;
        }
        send_probe(&socket, &inner, dst, gen);
    }
}

fn recv_loop(socket: Arc<UdpSocket>, inner: Arc<Mutex<Inner>>, stop: Arc<AtomicBool>) {
    let mut buf = [0u8; 1500];
    while !stop.load(Ordering::Relaxed) {
        match socket.recv_from(&mut buf) {
            Ok((n, _from)) => {
                let t4 = mono_now(); // stamp arrival ASAP
                let pkt = match decode_clock(&buf[..n]) {
                    Ok(p) => p,
                    Err(_) => continue,
                };
                if pkt.header.typ != msg::CLOCK_RSP {
                    continue;
                }
                let mut g = inner.lock().unwrap();
                let Some(t1) = g.pending.remove(&pkt.header.seq) else {
                    continue; // unknown / duplicate / late
                };
                g.est.add(Sample::new(t1, pkt.t2, pkt.t3, t4));
                g.replies += 1;
                if !g.synced && g.est.offset().is_some() {
                    g.synced = true;
                }
            }
            Err(e)
                if e.kind() == io::ErrorKind::WouldBlock
                    || e.kind() == io::ErrorKind::TimedOut =>
            {
                continue
            }
            Err(_) => continue,
        }
    }
}

/// Sleep up to `d`, waking early (in ~50 ms steps) if `stop` is set.
fn interruptible_sleep(d: Duration, stop: &AtomicBool) {
    let step = Duration::from_millis(50);
    let mut remaining = d;
    while remaining > Duration::ZERO {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        let chunk = remaining.min(step);
        thread::sleep(chunk);
        remaining = remaining.saturating_sub(chunk);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::server::ClockServer;
    use std::time::Instant;

    /// End-to-end clock plane on loopback: a follower pointed at a local server
    /// must reach CONFIDENT sync, and — since both share this process's monotonic
    /// clock — report an offset near zero with working master/local translation.
    #[test]
    fn follower_syncs_to_local_server() {
        let server = ClockServer::start("127.0.0.1:0").expect("start clock server");
        let follower = Follower::start(server.local_addr()).expect("start follower");

        // Poll up to 3 s for the cold-start burst to fill the confident window.
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline && !follower.is_synced() {
            thread::sleep(Duration::from_millis(20));
        }
        assert!(follower.is_synced(), "follower should reach confident sync on loopback");

        let stats = follower.stats();
        assert!(stats.replies >= 5, "expected >=5 replies, got {}", stats.replies);
        // Same-process clock → true offset is 0; allow generous slack for jitter.
        assert!(
            stats.offset_ns.abs() < 50_000_000,
            "offset should be near zero on loopback, got {} ns",
            stats.offset_ns
        );

        // master_to_local should be ~identity (offset ~ 0).
        let m = mono_now() + 1_000_000;
        let local = follower.master_to_local(m).expect("synced → Some");
        assert!(
            (m - local).abs() < 50_000_000,
            "translation drift too large: {} ns",
            m - local
        );

        let mn = follower.master_now().expect("synced → Some");
        assert!((mn - mono_now()).abs() < 50_000_000);
    }
}
