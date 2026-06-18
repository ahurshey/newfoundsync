//! Audio source server (TCP): accepts client subscriptions, stamps each frame
//! with a sequence number and a master-clock PTS, and streams it to every live
//! subscriber over a reliable TCP connection.
//!
//! Each client gets a dedicated writer thread fed by a deep bounded channel, so
//! the capture/encode pipeline (which calls [`AudioServer::release_frame`] once
//! per 20 ms frame) never blocks on a slow client. A Wi-Fi stall just queues that
//! client's frames; TCP delivers them on recovery and the client's large jitter
//! buffer hides the latency — so a dropout never reaches the speakers. All clients
//! ride the same master clock + buffer, so they play in lock-step.

use std::io::{self, Write};
use std::net::{SocketAddr, TcpListener, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::config::mono_now;
use crate::proto::{append_frame, msg, Header, HEADER_SIZE};

/// Per-client send queue depth in frames (~20 s of audio at 50 fps). Deep enough
/// to hold well over any buffer depth so a Wi-Fi stall queues rather than drops;
/// frames are `Arc`-shared so it's cheap across clients.
const CLIENT_QUEUE: usize = 1024;

/// A connected client: a deep channel into its dedicated writer thread.
struct ClientTx {
    tx: SyncSender<Arc<Vec<u8>>>,
}

/// A running audio source server. Stops its threads when dropped.
pub struct AudioServer {
    clients: Arc<Mutex<Vec<ClientTx>>>,
    gen: u32,
    lead_ns: i64,
    seq: Arc<AtomicU64>,
    local_addr: SocketAddr,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl AudioServer {
    /// Bind the audio TCP port and start accepting subscriptions.
    ///
    /// - `gen` is the session generation stamped on every frame.
    /// - `lead_ms` is how far ahead of `mono_now()` each frame's PTS is stamped.
    /// - `_ring_frames` is ignored (TCP needs no prime ring — the client buffers
    ///   live frames). Kept for call-site compatibility.
    pub fn start<A: ToSocketAddrs>(
        bind: A,
        gen: u32,
        lead_ms: i64,
        _ring_frames: usize,
    ) -> io::Result<Self> {
        let listener = TcpListener::bind(bind)?;
        let local_addr = listener.local_addr()?;
        listener.set_nonblocking(true)?;

        let clients: Arc<Mutex<Vec<ClientTx>>> = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));

        let handle = {
            let clients = clients.clone();
            let stop = stop.clone();
            thread::Builder::new()
                .name("audio-accept".into())
                .spawn(move || accept_loop(listener, clients, stop))?
        };

        Ok(AudioServer {
            clients,
            gen,
            lead_ns: lead_ms.max(0) * 1_000_000,
            seq: Arc::new(AtomicU64::new(0)),
            local_addr,
            stop,
            handle: Some(handle),
        })
    }

    /// The address the server bound to.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Number of currently connected subscribers.
    pub fn subscriber_count(&self) -> usize {
        self.clients.lock().unwrap().len()
    }

    /// Stamp `payload` (one canonical 20 ms frame — Opus or PCM) with the next seq
    /// and a PTS of `mono_now() + lead`, and fan it out to every live subscriber.
    pub fn release_frame(&self, payload: &[u8]) {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let pts = mono_now() + self.lead_ns;
        let mut framed = Vec::with_capacity(HEADER_SIZE + payload.len());
        append_frame(Header::new(msg::AUDIO, self.gen, seq, pts), &mut framed, payload);
        let frame = Arc::new(framed);
        self.clients.lock().unwrap().retain(|c| match c.tx.try_send(frame.clone()) {
            Ok(()) => true,
            // Client > CLIENT_QUEUE behind: drop this frame for it, keep it.
            Err(TrySendError::Full(_)) => true,
            // Writer gone: client disconnected.
            Err(TrySendError::Disconnected(_)) => false,
        });
    }
}

impl Drop for AudioServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// Accept loop: each new client gets a dedicated blocking-writer thread fed by a
/// channel, so a slow client never stalls the encoder or the other rooms.
fn accept_loop(listener: TcpListener, clients: Arc<Mutex<Vec<ClientTx>>>, stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((s, _)) => {
                s.set_nodelay(true).ok();
                s.set_write_timeout(Some(Duration::from_secs(10))).ok();
                let (tx, rx) = sync_channel::<Arc<Vec<u8>>>(CLIENT_QUEUE);
                thread::Builder::new()
                    .name("audio-writer".into())
                    .spawn(move || {
                        let mut s = s;
                        for buf in rx.iter() {
                            if s.write_all(&buf).is_err() {
                                break; // peer gone / write timeout
                            }
                        }
                    })
                    .ok();
                clients.lock().unwrap().push(ClientTx { tx });
                tracing::info!("audio client connected");
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(100));
            }
            Err(_) => break,
        }
    }
}
