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
    // The four below are `Option`s rather than clap defaults so "not given" can be told apart from
    // "given the default": when a flag is omitted, the choice last applied in the server window is
    // used, and only then the built-in default. A flag always wins for that run.
    /// Audio codec: opus or pcm. Omitted ⇒ the last choice in the GUI, else opus.
    #[arg(long)]
    codec: Option<String>,
    /// Opus bitrate in bits/sec (ignored for pcm). Omitted ⇒ the last choice in the GUI, else 510000.
    #[arg(long)]
    bitrate: Option<i32>,
    /// Client buffer in ms (= end-to-end latency AND dropout cushion). Omitted ⇒ the last choice in
    /// the GUI, else 3000.
    #[arg(long)]
    buffer_ms: Option<i64>,
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
    /// Omitted ⇒ the last choice in the GUI, else av1.
    #[arg(long)]
    encoder: Option<String>,
    /// Where video encodes: auto (GPU if available, else CPU) | gpu (fail if unavailable) | cpu.
    /// Audio is unaffected. Separate from --encoder, which picks the CODEC.
    #[arg(long, default_value = "auto")]
    encode_device: String,
    /// Shift AUDIO against video, in milliseconds (-500..=500). Positive delays the audio, which is
    /// the direction that fixes sound arriving BEFORE the picture. Omitted ⇒ the value saved from the
    /// GUI slider, else 0.
    #[arg(long)]
    av_offset_ms: Option<i32>,
    /// Audio source: allapps (survives mute) | system | app | sink (Linux: a "Newfoundsync" output
    /// device; only apps sent to it are streamed, and they are silent here) | web (a web client
    /// casts up to here). Omitted ⇒ the source last applied in the GUI, else allapps. (A single-app
    /// source is never restored: its PID does not survive a restart.)
    #[arg(long)]
    capture: Option<String>,
    /// macOS: keep what is captured silent on this Mac while it streams (all apps or one app).
    /// Omitted ⇒ the choice last applied in the GUI.
    #[arg(long)]
    silent: bool,
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
    let buffer_req = cli
        .buffer_ms
        .or_else(|| settings::get("buffer_ms").and_then(|v| v.parse().ok()))
        .unwrap_or(config::DEFAULT_BUFFER_MS);
    let buffer_ms = config::clamp_buffer_ms(buffer_req);
    if buffer_ms != buffer_req {
        tracing::warn!(
            requested = buffer_req,
            using = buffer_ms,
            "--buffer-ms outside the supported range ({}..={} ms); clamped",
            config::MIN_BUFFER_MS,
            config::MAX_BUFFER_MS
        );
    }

    // Parse the media config once; both GUI and headless use it.
    // A flag that does not parse is an error; a SAVED value that does not parse (an older or newer
    // build wrote it, or someone edited the file) is ignored in favour of the default — a stale
    // settings file must never stop the server from starting.
    let codec = match &cli.codec {
        Some(c) => CodecKind::parse(c).ok_or_else(|| anyhow!("unknown codec '{c}' (use opus or pcm)"))?,
        None => settings::get("codec").and_then(|c| CodecKind::parse(&c)).unwrap_or(CodecKind::Opus),
    };
    let bitrate = cli
        .bitrate
        .or_else(|| settings::get("bitrate").and_then(|v| v.parse().ok()))
        .unwrap_or(config::DEFAULT_BITRATE_BPS);
    let encoder_arg = cli.encoder.clone().unwrap_or_default();
    let encoder = match &cli.encoder {
        Some(e) => EncoderBackend::parse(e).ok_or_else(|| anyhow!("unknown encoder '{e}' (use av1 or vp9)"))?,
        None => settings::get("video_encoder")
            .and_then(|e| EncoderBackend::parse(&e))
            .unwrap_or(EncoderBackend::Av1),
    };
    // `--encoder` has long accepted device-flavoured words (gpu/cpu/hw/auto/...) as aliases for
    // av1. They are kept working so nobody's script breaks, but they never selected a device and
    // still don't -- someone who wrote `--encoder cpu` to keep video off the GPU has been getting
    // GPU encoding this whole time, with nothing in the log to say so. Now there is.
    if matches!(
        encoder_arg.to_ascii_lowercase().as_str(),
        "auto" | "hardware" | "hw" | "gpu" | "cpu" | "software" | "sw"
    ) {
        let token = encoder_arg.to_ascii_lowercase();
        tracing::warn!(
            "--encoder {token} names the CODEC and is only an alias for av1 — it has never \
             selected GPU vs CPU. Use --encode-device {token} instead."
        );
    }
    let encode_device = EncodeDevice::parse(&cli.encode_device).ok_or_else(|| {
        anyhow!("unknown encode device '{}' (use auto, gpu or cpu)", cli.encode_device)
    })?;
    // Only the sources that mean the same thing after a restart are ever saved (see gui::apply), and
    // the saved one is re-checked here so a file from another platform cannot name a source this
    // build would refuse.
    let saved_capture = settings::get("source").filter(|s| match s.as_str() {
        "allapps" | "system" | "web" => true,
        "sink" => cfg!(target_os = "linux"),
        _ => false,
    });
    let capture_arg = cli.capture.clone().or(saved_capture).unwrap_or_else(|| "allapps".to_string());
    let capture_source = match capture_arg.as_str() {
        "allapps" | "exclude" | "all" => CaptureSource::AllExceptSelf,
        "system" => CaptureSource::System,
        "web" | "uplink" | "cast" => CaptureSource::WebUplink,
        "app" => CaptureSource::App {
            pid: cli
                .app_pid
                .ok_or_else(|| anyhow!("--capture app requires --app-pid <PID>"))?,
        },
        // Refused up front on other platforms, where there is no sound server to host the device:
        // accepting it would start a server whose only source can never produce a frame.
        "sink" | "device" | "virtual" => {
            if !cfg!(target_os = "linux") {
                return Err(anyhow!(
                    "--capture sink creates a PipeWire/PulseAudio output device and is Linux-only"
                ));
            }
            CaptureSource::VirtualSink
        }
        other => return Err(anyhow!("unknown capture '{other}' (use allapps|system|app|sink|web)")),
    };
    if cli.silent && !cfg!(target_os = "macos") {
        return Err(anyhow!(
            "--silent is macOS-only. On Linux use --capture sink (the Newfoundsync output device); on \
             Windows the all-apps and app sources keep streaming with the PC muted."
        ));
    }
    let silent_local = cfg!(target_os = "macos")
        && (cli.silent || settings::get("silent").as_deref() == Some("1"));
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
        #[cfg(not(any(target_os = "windows", all(target_os = "linux", feature = "linux-capture"), all(target_os = "macos", feature = "mac-capture"))))]
        if !matches!(capture_source, CaptureSource::WebUplink) {
            return Err(anyhow!(
                "--video captures this machine's screen, and this build has no capture backend \n                 for it. On Linux rebuild with --features linux-capture, on macOS --features mac-capture (needs a desktop \n                 session with an xdg-desktop-portal backend). Otherwise relay a browser's cast: \n                 `--capture web --video` (both flags -- `--capture web` alone relays audio only)."
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
    // ONE handle, shared by the GUI slider and the audio producer. The CLI wins for this run if
    // given, else the value the operator last tuned by ear (it is a property of the machine, so it
    // belongs in settings rather than being re-found on every launch).
    //
    // MUST stay above the #[cfg] below, and outside it: both paths use it (the headless call at the
    // bottom of this function takes it too). It previously sat BETWEEN the attribute and the `if`,
    // which silently re-targeted the attribute onto this `let` — so a headless build compiled the
    // binding away and the gui::run call in, and `cargo check --no-default-features` failed with
    // four errors. An intervening comment does not rebind an attribute, but an intervening
    // STATEMENT does; the braced form below cannot be re-targeted that way.
    let av_offset_ns = std::sync::Arc::new(std::sync::atomic::AtomicI64::new(
        cli.av_offset_ms.or_else(settings::load_av_offset_ms).unwrap_or(0).clamp(-500, 500) as i64
            * 1_000_000,
    ));
    // The GUI build (default `gui` feature) opens the picker window unless --headless. A build
    // without the `gui` feature (the headless Linux/server package) has no GUI at all → always
    // server-only, regardless of the flag.
    #[cfg(feature = "gui")]
    {
        if !cli.headless {
            let result = gui::run(
                port,
                name,
                gui::InitialConfig {
                    capture_source,
                    silent_local,
                    video,
                    encoder,
                    encode_device,
                    buffer_ms,
                    codec,
                    bitrate,
                    av_offset_ns: av_offset_ns.clone(),
                },
            );
            // Closing the window returns here with the live capture still owned by the
            // media-control thread, which process exit kills without running its destructors. The
            // "Newfoundsync" output device belongs to the sound server, not to us, so it has to be
            // removed by hand or it outlives the app with programs still playing into it.
            #[cfg(target_os = "linux")]
            capture::pulse::remove_virtual_sink();
            return result;
        }
    }
    #[cfg(not(feature = "gui"))]
    if !cli.headless {
        tracing::info!("built without the `gui` feature — running headless (server-only)");
    }
    let result = run_headless(
        name,
        capture_source,
        silent_local,
        video,
        encoder,
        encode_device,
        codec,
        bitrate,
        buffer_ms,
        port,
        !cli.insecure_http,
        av_offset_ns,
    );
    // Normally already gone (run_headless drops its capture on the way out); this covers the paths
    // that do not, so an early error cannot leave the output device behind.
    #[cfg(target_os = "linux")]
    capture::pulse::remove_virtual_sink();
    result
}

#[allow(clippy::too_many_arguments)]
fn run_headless(
    name: String,
    capture_source: CaptureSource,
    silent_local: bool,
    video: Option<VideoConfig>,
    encoder: EncoderBackend,
    encode_device: EncodeDevice,
    codec: CodecKind,
    bitrate: i32,
    buffer_ms: i64,
    port: u16,
    use_tls: bool,
    av_offset_ns: std::sync::Arc<std::sync::atomic::AtomicI64>,
) -> Result<()> {
    let media = media::start(MediaOptions {
        name: name.clone(),
        codec,
        bitrate,
        lead_ms: config::DEFAULT_LEAD_MS,
        buffer_ms,
        capture_source,
        silent_local,
        video,
        av_offset_ns,
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
    // The Android TV app's plain-HTTP port (search + sound-only fallback), next to the main one.
    let tv_addr = SocketAddr::from(([0, 0, 0, 0], webserver::tv_port(port)));
    runtime.block_on(webserver::run(state_rx, clients, clients_reg, cast, addr, use_tls, Some(tv_addr)))?;
    Ok(())
}

fn default_name() -> String {
    std::env::var("COMPUTERNAME")
        .ok()
        .or_else(|| std::env::var("HOSTNAME").ok())
        .unwrap_or_else(|| "Newfoundsync".to_string())
}
