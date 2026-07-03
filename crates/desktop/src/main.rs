// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Alex Hurshman and the Newfoundsync contributors.

//! Newfoundsync server (web client edition).
//!
//! Captures this PC's audio (and optionally the screen), encodes it (Opus +
//! H.264 / GPU), and serves a web client over HTTP. Browsers on the LAN open
//! `http://<this-pc>:47000`, buffer a few seconds, clock-sync, and play in
//! lock-step. The browser is the client; this app is the source picker + server.
//!
//! Run with no flags for the GUI (pick your source visually — flags below seed
//! it). `--headless` runs server-only from those flags.

mod capture;
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
use newfoundsync_core::discovery;
use newfoundsync_core::video::{EncoderBackend, Fps, Resolution, VideoConfig};

use media::{CaptureSource, MediaOptions};
use webserver::StreamState;

#[derive(Parser)]
#[command(name = "newfoundsync", about = "LAN audio/video sharing with a web client")]
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
    let name = cli.name.clone().unwrap_or_else(default_name);

    // Parse the media config once; both GUI and headless use it.
    let codec = CodecKind::parse(&cli.codec)
        .ok_or_else(|| anyhow!("unknown codec '{}' (use opus or pcm)", cli.codec))?;
    let encoder = EncoderBackend::parse(&cli.encoder)
        .ok_or_else(|| anyhow!("unknown encoder '{}' (use av1 or vp9)", cli.encoder))?;
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
    if cli.headless {
        run_headless(
            name,
            capture_source,
            video,
            encoder,
            codec,
            cli.bitrate,
            cli.buffer_ms,
            port,
            !cli.insecure_http,
        )
    } else {
        gui::run(
            port,
            name,
            gui::InitialConfig {
                capture_source,
                video,
                encoder,
                buffer_ms: cli.buffer_ms,
                codec,
                bitrate: cli.bitrate,
            },
        )
    }
}

#[allow(clippy::too_many_arguments)]
fn run_headless(
    name: String,
    capture_source: CaptureSource,
    video: Option<VideoConfig>,
    encoder: EncoderBackend,
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
    })?;

    let host = discovery::primary_lan_ipv4()
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
