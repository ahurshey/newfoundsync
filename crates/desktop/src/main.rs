// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Alex Hurshman and the Newfoundsync contributors.

//! Newfoundsync server (web client edition).
//!
//! Captures this PC's audio (and optionally the screen), encodes it (Opus +
//! AV1/VP9), and serves a web client over HTTP. Browsers on the LAN open
//! `http://<this-pc>:47000`, buffer a few seconds, clock-sync, and play in
//! lock-step. The browser is the client; this app is the source picker + server.
//!
//! Run with no flags for the GUI (pick your source visually — flags below seed
//! it). `--headless` runs server-only from those flags.

mod capture;
#[cfg(feature = "gui")]
mod gui;
mod media;
mod settings;
mod tls;
mod video;
mod webserver;

use std::net::SocketAddr;
use std::sync::atomic::AtomicUsize;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use clap::Parser;
use tokio::sync::watch;

use newfoundsync_core::codec::CodecKind;
use newfoundsync_core::config;
use newfoundsync_core::net;
use newfoundsync_core::video::{EncodeDevice, EncoderBackend, Fps, Resolution, VideoConfig};

use media::{CaptureSource, MediaOptions};
use webserver::StreamState;

/// Full build identity: crate version + the git commit stamped in by `build.rs`. Reported by
/// `--version`, logged once at startup, and served by `/health` — so a field report can always be
/// tied to exact bytes (hand-copied builds on several machines are otherwise indistinguishable).
/// A `const` (not a function) because clap's `version` needs a `'static` str.
pub const BUILD_ID: &str = concat!(env!("CARGO_PKG_VERSION"), " (", env!("NFS_GIT_SHA"), ")");

#[derive(Parser)]
#[command(
    name = "newfoundsync",
    about = "LAN audio/video sharing with a web client",
    version = BUILD_ID
)]
struct Cli {
    /// HTTP port for the web client + WebSocket. Overrides the saved GUI setting for this run;
    /// if omitted, the port last set in the GUI is used, else the default (47000).
    #[arg(long)]
    port: Option<u16>,
    /// Display name shown to clients.
    #[arg(long)]
    name: Option<String>,
    /// Run without the GUI (server-only, driven by the flags below).
    #[arg(long)]
    headless: bool,
    /// Audio codec: opus (default) or pcm.
    #[arg(long, default_value = "opus")]
    codec: String,
    /// Opus bitrate in bits/sec (ignored for pcm).
    #[arg(long, default_value_t = config::DEFAULT_BITRATE_BPS)]
    bitrate: i32,
    /// Client buffer in ms (= end-to-end latency AND dropout cushion).
    #[arg(long, default_value_t = config::DEFAULT_BUFFER_MS)]
    buffer_ms: i64,
    /// Also share the screen (video).
    #[arg(long)]
    video: bool,
    /// Video resolution: 720p | 1080p | 1440p | 2160p.
    #[arg(long, default_value = "1080p")]
    resolution: String,
    /// Video frame rate: 30 or 60.
    #[arg(long, default_value = "30", value_parser = ["30", "60"])]
    fps: String,
    /// Video codec: av1 (royalty-free default; GPU AV1 or CPU SVT-AV1) | vp9 (royalty-free CPU fallback).
    #[arg(long, default_value = "av1")]
    encoder: String,
    /// Where video encodes: auto (GPU if available, else CPU) | gpu (fail if unavailable) | cpu.
    /// Audio is unaffected. Separate from --encoder, which picks the CODEC.
    #[arg(long, default_value = "auto")]
    encode_device: String,
    /// Audio source: allapps (survives mute) | system | app | web (a web client casts up to here).
    #[arg(long, default_value = "allapps")]
    capture: String,
    /// PID to capture when --capture app.
    #[arg(long, required_if_eq("capture", "app"))]
    app_pid: Option<u32>,
    /// Serve plain HTTP instead of HTTPS. WebCodecs then only works via localhost or
    /// behind a TLS-terminating reverse proxy — not on a bare LAN IP.
    #[arg(long)]
    insecure_http: bool,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    // First line in every log: which build produced everything below it.
    tracing::info!(build = BUILD_ID, "newfoundsync starting");
    let name = cli.name.clone().unwrap_or_else(default_name);

    // Clamp ONCE, here at the entry point, so every downstream path (headless and GUI seed alike) gets
    // a sane value. The headless path previously passed --buffer-ms through unclamped.
    let buffer_ms = config::clamp_buffer_ms(cli.buffer_ms);
    if buffer_ms != cli.buffer_ms {
        tracing::warn!(
            requested = cli.buffer_ms,
            using = buffer_ms,
            "--buffer-ms outside the supported range ({}..={} ms); clamped",
            config::MIN_BUFFER_MS,
            config::MAX_BUFFER_MS
        );
    }

    // Parse the media config once; both GUI and headless use it.
    let codec = CodecKind::parse(&cli.codec)
        .ok_or_else(|| anyhow!("unknown codec '{}' (use opus or pcm)", cli.codec))?;
    let encoder = EncoderBackend::parse(&cli.encoder)
        .ok_or_else(|| anyhow!("unknown encoder '{}' (use av1 or vp9)", cli.encoder))?;
    // `--encoder` has long accepted device-flavoured words (gpu/cpu/hw/auto/...) as aliases for
    // av1. They are kept working so nobody's script breaks, but they never selected a device and
    // still don't -- someone who wrote `--encoder cpu` to keep video off the GPU has been getting
    // GPU encoding this whole time, with nothing in the log to say so. Now there is.
    if matches!(
        cli.encoder.to_ascii_lowercase().as_str(),
        "auto" | "hardware" | "hw" | "gpu" | "cpu" | "software" | "sw"
    ) {
        let token = cli.encoder.to_ascii_lowercase();
        tracing::warn!(
            "--encoder {token} names the CODEC and is only an alias for av1 — it has never \
             selected GPU vs CPU. Use --encode-device {token} instead."
        );
    }
    let encode_device = EncodeDevice::parse(&cli.encode_device).ok_or_else(|| {
        anyhow!("unknown encode device '{}' (use auto, gpu or cpu)", cli.encode_device)
    })?;
    let capture_source = match cli.capture.as_str() {
        "allapps" | "exclude" | "all" => CaptureSource::AllExceptSelf,
        "system" => CaptureSource::System,
        "web" | "uplink" | "cast" => CaptureSource::WebUplink,
        "app" => CaptureSource::App {
            pid: cli
                .app_pid
                .ok_or_else(|| anyhow!("--capture app requires --app-pid <PID>"))?,
        },
        other => return Err(anyhow!("unknown capture '{other}' (use allapps|system|app|web)")),
    };
    let video = if cli.video {
        // A request for local screen capture used to be accepted here and then dropped deep in
        // media.rs with nothing but a warn! — so `--video` appeared to work, the stream came up
        // "Live", and clients simply never received a picture. Refuse at the entry point instead,
        // and name what to do about it.
        //
        // Keyed on whether this BUILD has a capture backend, not on the OS: Windows always has one
        // (WGC), Linux has one with --features linux-capture (portal + PipeWire), and anything else
        // has none. Gating on the OS would refuse a Linux build that can in fact capture.
        //
        // The web-uplink exception is real, not a courtesy: the relay is deliberately not gated at
        // all (media.rs computes `video_on` as `opts.video.is_some()` for a web uplink), because a
        // cast carries the browser's own encoded frames and needs no local capture.
        #[cfg(not(any(target_os = "windows", all(target_os = "linux", feature = "linux-capture"))))]
        if !matches!(capture_source, CaptureSource::WebUplink) {
            return Err(anyhow!(
                "--video captures this machine's screen, and this build has no capture backend \n                 for it. On Linux, rebuild with --features linux-capture (needs a desktop \n                 session with an xdg-desktop-portal backend). Otherwise relay a browser's cast: \n                 `--capture web --video` (both flags -- `--capture web` alone relays audio only)."
            ));
        }
        let resolution = Resolution::parse(&cli.resolution).ok_or_else(|| {
            anyhow!("unknown resolution '{}' (720p|1080p|1440p|2160p)", cli.resolution)
        })?;
        Some(VideoConfig {
            resolution,
            fps: if cli.fps == "60" { Fps::F60 } else { Fps::F30 },
            quality_pct: 100, // headless: baseline quality (the GUI exposes the slider)
        })
    } else {
        None
    };

    // Effective HTTP port: an explicit --port wins, else the port last saved in the GUI,
    // else the built-in default. (The GUI lets users change + save this; it applies next launch.)
    let port = cli.port.or_else(settings::load_port).unwrap_or(config::DEFAULT_HTTP_PORT);
    // The GUI build (default `gui` feature) opens the picker window unless --headless. A build
    // without the `gui` feature (the headless Linux/server .deb) has no GUI at all → always
    // server-only, regardless of the flag.
    #[cfg(feature = "gui")]
    if !cli.headless {
        return gui::run(
            port,
            name,
            gui::InitialConfig {
                capture_source,
                video,
                encoder,
                encode_device,
                buffer_ms,
                codec,
                bitrate: cli.bitrate,
            },
        );
    }
    #[cfg(not(feature = "gui"))]
    if !cli.headless {
        tracing::info!("built without the `gui` feature — running headless (server-only)");
    }
    run_headless(
        name,
        capture_source,
        video,
        encoder,
        encode_device,
        codec,
        cli.bitrate,
        buffer_ms,
        port,
        !cli.insecure_http,
    )
}

#[allow(clippy::too_many_arguments)]
fn run_headless(
    name: String,
    capture_source: CaptureSource,
    video: Option<VideoConfig>,
    encoder: EncoderBackend,
    encode_device: EncodeDevice,
    codec: CodecKind,
    bitrate: i32,
    buffer_ms: i64,
    port: u16,
    use_tls: bool,
) -> Result<()> {
    let media = media::start(MediaOptions {
        name: name.clone(),
        codec,
        bitrate,
        lead_ms: config::DEFAULT_LEAD_MS,
        buffer_ms,
        capture_source,
        video,
        video_target: media::VideoTarget::PrimaryMonitor, // headless: whole monitor (no window picker)
        encoder,
        encode_device,
    })?;

    let host = net::primary_lan_ipv4()
        .map(|ip| ip.to_string())
        .unwrap_or_else(|| "<this-pc>".to_string());
    let scheme = if use_tls { "https" } else { "http" };
    println!(
        "Newfoundsync '{}' serving — open  {}://{}:{}  in a browser on the LAN.",
        name, scheme, host, port
    );
    if use_tls {
        println!("  (one-time: accept the self-signed certificate — 'proceed anyway')");
    }
    println!(
        "  source: {}   video: {}   buffer: {:.1}s",
        media.capture_device,
        if media.config.video { "on" } else { "off" },
        buffer_ms as f64 / 1000.0,
    );

    let clients = Arc::new(AtomicUsize::new(0));
    // Per-client registry (headless: no GUI to drive it, but the server still
    // tracks identities for the control channel — harmless and keeps the API one shape).
    let clients_reg = Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
    // The sender must outlive the server (a dropped sender makes every client
    // reconnect-loop); `block_on` below keeps this whole scope alive.
    let (_state_tx, state_rx) = watch::channel(Arc::new(StreamState::from_media(&media)));
    let _keep_media = media;

    // Active web-caster slot (first client to request cast wins). Headless: no GUI stop button,
    // but the slot still gates which client may relay, and frees on disconnect/stop.
    let cast = Arc::new(std::sync::Mutex::new(None));
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(webserver::run(state_rx, clients, clients_reg, cast, addr, use_tls))?;
    Ok(())
}

fn default_name() -> String {
    std::env::var("COMPUTERNAME")
        .ok()
        .or_else(|| std::env::var("HOSTNAME").ok())
        .unwrap_or_else(|| "Newfoundsync".to_string())
}
