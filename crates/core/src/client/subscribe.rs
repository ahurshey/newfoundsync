//! Subscription (TCP): connects to a server's audio port and feeds received AUDIO
//! frames into the shared [`Scheduler`]. TCP gives reliable, in-order delivery, so
//! there's no keepalive / re-prime / loss handling — the connection itself is the
//! subscription, and the client's large jitter buffer absorbs network stalls.
//!
//! The receive loop uses a read timeout + manual partial-read accumulation, so it
//! checks `stop` (and exits on drop) within ~200 ms without relying on socket
//! shutdown to interrupt a blocked read.

use std::io::{self, Read};
use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::proto::{self, msg, Header, HEADER_SIZE};

use super::scheduler::Scheduler;

/// How often the recv loop wakes to re-check `stop` while idle.
const READ_TIMEOUT: Duration = Duration::from_millis(200);

/// A running subscription. Closes the connection and stops its thread on drop.
pub struct Subscriber {
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
}

impl Subscriber {
    /// Subscribe to `server`'s audio port, feeding frames into `scheduler`.
    /// Returns immediately — the connect happens on the recv thread (with a
    /// timeout), so a wrong/unreachable address can't block the caller.
    pub fn start(server: SocketAddr, scheduler: Arc<Mutex<Scheduler>>) -> io::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let t = {
            let stop = stop.clone();
            thread::Builder::new()
                .name("sub-recv".into())
                .spawn(move || {
                    let stream = match TcpStream::connect_timeout(&server, Duration::from_secs(3)) {
                        Ok(s) => s,
                        Err(e) => {
                            tracing::warn!("audio connect failed: {e}");
                            return;
                        }
                    };
                    stream.set_nodelay(true).ok();
                    stream.set_read_timeout(Some(READ_TIMEOUT)).ok();
                    recv_loop(stream, scheduler, stop);
                })?
        };
        Ok(Subscriber {
            stop,
            threads: vec![t],
        })
    }
}

impl Drop for Subscriber {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for h in self.threads.drain(..) {
            let _ = h.join();
        }
    }
}

/// Read exactly `buf.len()` bytes, honoring `stop` and the stream's read timeout.
/// Returns false on stop, EOF, or error.
fn read_full(stream: &mut TcpStream, buf: &mut [u8], stop: &AtomicBool) -> bool {
    let mut filled = 0;
    while filled < buf.len() {
        if stop.load(Ordering::Relaxed) {
            return false;
        }
        match stream.read(&mut buf[filled..]) {
            Ok(0) => return false, // peer closed
            Ok(n) => filled += n,
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                continue // timed out: re-check stop, keep waiting
            }
            Err(_) => return false,
        }
    }
    true
}

fn recv_loop(mut stream: TcpStream, scheduler: Arc<Mutex<Scheduler>>, stop: Arc<AtomicBool>) {
    let mut hdr = [0u8; HEADER_SIZE];
    let mut buf: Vec<u8> = Vec::new();
    while !stop.load(Ordering::Relaxed) {
        if !read_full(&mut stream, &mut hdr, &stop) {
            break;
        }
        let h = match Header::decode(&hdr) {
            Ok(h) if h.magic == proto::MAGIC => h,
            _ => break, // framing lost — bail
        };
        let len = h.payload_len as usize;
        buf.resize(len, 0);
        if len > 0 && !read_full(&mut stream, &mut buf, &stop) {
            break;
        }
        if h.typ != msg::AUDIO {
            continue; // ignore non-audio for v1
        }
        let mut s = scheduler.lock().unwrap();
        let (gen, armed) = s.armed_gen();
        if !armed || gen != h.gen {
            // New or replaced session: re-arm to this frame's generation.
            s.reset(h.gen);
        }
        s.push(h.gen, h.seq, h.pts, &buf[..len]);
    }
}
