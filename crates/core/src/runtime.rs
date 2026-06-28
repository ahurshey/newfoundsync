//! Client runtime: assemble the platform-neutral modules into a running client.
//!
//! [`ClientHandle`] starts everything in its constructor and tears it down on
//! drop, so callers (the desktop UI/CLI) just build one and
//! hold it. The server runtime lives in the desktop crate because it depends on
//! platform-specific capture.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};

use crate::client::jitterbuffer::DEFAULT_CAPACITY;
use crate::client::playout::PlayoutDriver;
use crate::client::scheduler::Scheduler;
use crate::client::subscribe::Subscriber;
use crate::clock::{Follower, MasterClock};
use crate::codec::{CodecKind, Decoder};
use crate::discovery::DiscoveredServer;
use crate::playback::{AudioOutput, OutputStats, RtrbSink, Volume};

/// A running client: discover-selected server → clock-sync → subscribe → play.
pub struct ClientHandle {
    follower: Arc<Follower>,
    scheduler: Arc<Mutex<Scheduler>>,
    _output: AudioOutput,
    _driver: PlayoutDriver,
    _subscriber: Subscriber,
    stats: Arc<OutputStats>,
    volume: Volume,
    pub server_name: String,
    pub output_device: String,
}

impl ClientHandle {
    pub fn start(server: &DiscoveredServer, volume: Volume, buffer_ms: i64) -> Result<ClientHandle> {
        let clock_addr = SocketAddr::new(server.ip, server.clock_port);
        let audio_addr = SocketAddr::new(server.ip, server.audio_port);

        let follower = Arc::new(Follower::start(clock_addr).context("start clock follower")?);
        let scheduler = Arc::new(Mutex::new(Scheduler::new(DEFAULT_CAPACITY, buffer_ms)));

        let codec = CodecKind::parse(&server.codec).unwrap_or(CodecKind::Pcm);
        let decoder = Decoder::new(codec).context("build decoder")?;

        let stats = Arc::new(OutputStats::default());
        let (output, producer) =
            AudioOutput::open_default(8, stats.clone()).context("open audio output")?;
        let output_device = output.device_name.clone();

        let sink = RtrbSink::new(producer, decoder, volume.clone());
        let clock_dyn: Arc<dyn MasterClock + Send + Sync> = follower.clone();
        let driver = PlayoutDriver::start(scheduler.clone(), clock_dyn, Box::new(sink));
        let subscriber = Subscriber::start(audio_addr, scheduler.clone()).context("subscribe")?;

        tracing::info!(
            server = %server.name,
            %audio_addr,
            %clock_addr,
            device = %output_device,
            "client connected"
        );

        Ok(ClientHandle {
            follower,
            scheduler,
            _output: output,
            _driver: driver,
            _subscriber: subscriber,
            stats,
            volume,
            server_name: server.name.clone(),
            output_device,
        })
    }

    /// The clock follower this client is synced to — shared with the video path
    /// so audio and video present against the same master clock (A/V lip-sync).
    pub fn follower(&self) -> Arc<Follower> {
        self.follower.clone()
    }

    /// Set the per-client output-delay alignment offset, in milliseconds.
    pub fn set_delay_ms(&self, ms: i64) {
        self.scheduler
            .lock()
            .unwrap()
            .set_delay_offset_ns(ms * 1_000_000);
    }

    pub fn set_volume(&self, v: f32) {
        self.volume.set(v);
    }

    /// Live status snapshot for the UI.
    pub fn status(&self) -> ClientStatus {
        let fstats = self.follower.stats();
        let pstats = self.scheduler.lock().unwrap().stats();
        ClientStatus {
            synced: fstats.synced,
            offset_ns: fstats.offset_ns,
            rtt_ns: fstats.rtt_ns,
            buffered: pstats.buffered,
            played: pstats.played,
            silence: pstats.silence,
            late_drop: pstats.late_drop,
            underrun_samples: self
                .stats
                .underrun_samples
                .load(std::sync::atomic::Ordering::Relaxed),
        }
    }
}

/// Client telemetry for display.
#[derive(Clone, Copy, Debug, Default)]
pub struct ClientStatus {
    pub synced: bool,
    pub offset_ns: i64,
    pub rtt_ns: i64,
    pub buffered: usize,
    pub played: u64,
    pub silence: u64,
    pub late_drop: u64,
    pub underrun_samples: u64,
}
