//! Clock server: answers `CLOCK_REQ` (0x10) with `CLOCK_RSP` (0x11), stamping the
//! receive time `t2` on entry and the send time `t3` just before reply.
//!
//! Ported from the server half of `ensemble/internal/clock/clock.go`. Runs on its
//! own dedicated UDP socket (the advertised clock port); the handler is cheap and
//! never filters by generation — the server is the time source, so it echoes the
//! request's gen/seq unchanged.

use std::io;
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::clock::payload::{decode_clock, encode_clock, CLOCK_PACKET_SIZE};
use crate::config::mono_now;
use crate::proto::msg;

/// A running clock server. Stops its thread when dropped.
pub struct ClockServer {
    local_addr: SocketAddr,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl ClockServer {
    /// Bind a UDP socket and start serving clock requests.
    pub fn start<A: ToSocketAddrs>(bind: A) -> io::Result<Self> {
        let socket = UdpSocket::bind(bind)?;
        // A read timeout lets the loop check the stop flag periodically.
        socket.set_read_timeout(Some(Duration::from_millis(200)))?;
        let local_addr = socket.local_addr()?;
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let handle = thread::Builder::new()
            .name("clock-server".into())
            .spawn(move || serve(socket, stop_thread))?;
        Ok(ClockServer {
            local_addr,
            stop,
            handle: Some(handle),
        })
    }

    /// The address the server bound to (port is resolved if 0 was requested).
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }
}

impl Drop for ClockServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn serve(socket: UdpSocket, stop: Arc<AtomicBool>) {
    let mut buf = [0u8; 1500];
    let mut out = [0u8; CLOCK_PACKET_SIZE];
    while !stop.load(Ordering::Relaxed) {
        match socket.recv_from(&mut buf) {
            Ok((n, from)) => {
                let t2 = mono_now(); // stamp receive ASAP
                let pkt = match decode_clock(&buf[..n]) {
                    Ok(p) => p,
                    Err(_) => continue,
                };
                if pkt.header.typ != msg::CLOCK_REQ {
                    continue;
                }
                let t3 = mono_now(); // stamp send just before WriteTo
                encode_clock(
                    &mut out,
                    msg::CLOCK_RSP,
                    pkt.header.gen,
                    pkt.header.seq,
                    pkt.t1,
                    t2,
                    t3,
                );
                let _ = socket.send_to(&out, from);
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
