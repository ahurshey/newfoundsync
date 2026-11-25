// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Alex Hurshman and the Newfoundsync contributors.

//! Native server GUI (eframe): who is listening and how loud, whether sound is actually reaching
//! the stream, what is being shared, and how a new device connects.
//!
//! This file holds the window's state, its threads and everything that ACTS on the stream; the
//! layout itself is in `gui/view.rs`.
//!
//! Threading: the GUI thread only ever sends [`MediaOptions`] (cheap, `Send`) to a
//! dedicated **media-control thread** that owns the live `Media` (which holds a
//! `!Send` capture stream) and the `watch` sender. Building a capture can block for
//! up to several seconds, so doing it on the control thread keeps the window
//! responsive. A separate thread runs the tokio web server. Selecting a new source
//! rebuilds the stream; connected browsers reconnect and pick up the new source.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use eframe::egui;
use tokio::sync::watch;

use newfoundsync_core::codec::CodecKind;
use newfoundsync_core::video::{EncodeDevice, EncoderBackend, Fps, Resolution, VideoConfig};
use newfoundsync_core::{config, net};

use crate::media::{self, CaptureSource, Media, MediaOptions, VideoTarget};
use crate::webserver::{self, ClientRegistry, StreamState};

mod view;

// Both platforms now supply `sessions::{AudioApp, list_sources}` — Windows from a window/session
// enumeration, Linux from the live PulseAudio sink-input list. Same types, different semantics
// (see capture/linux_sessions.rs); the picker below is written to tolerate both.
#[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
use crate::capture::sessions::{self, AudioApp};
#[cfg(target_os = "linux")]
use crate::capture::pulse::{self, RouteSnapshot};

/// How often the "send to the output device" list is re-read while its source is selected.
///
/// Unlike the capture picker, this one DOES poll, because it is not only a list: each pass also
/// moves a remembered app that is in the wrong place (see `pulse::sync_routes` for why the sound
/// server cannot be trusted to). It has no button to flicker, and the window already repaints once
/// a second for the client counts, so the poll costs one short-lived connection per pass.
#[cfg(target_os = "linux")]
const ROUTE_SYNC_EVERY: Duration = Duration::from_secs(2);

/// State behind the "send these apps to the output device" checkboxes (Linux).
#[cfg(target_os = "linux")]
#[derive(Default)]
struct AppRouting {
    /// Apps the operator ticked, by `pulse::RoutedApp::key`. Persisted across launches.
    wanted: std::collections::BTreeSet<String>,
    /// Apps unticked since the last pass; the next pass sends them back to the speakers.
    release: Vec<String>,
    /// What the last pass found. `None` until the first one returns.
    snapshot: Option<RouteSnapshot>,
    /// A pass in flight on a worker thread — never run on the GUI thread, it talks to the sound
    /// server.
    rx: Option<mpsc::Receiver<Result<RouteSnapshot, String>>>,
    last_sync: Option<std::time::Instant>,
    /// A box was toggled: run a pass now rather than at the next tick.
    dirty: bool,
    error: String,
}

#[derive(PartialEq, Clone, Copy)]
enum SourceKind {
    AllApps,
    App,
    System,
    /// Linux: the "Newfoundsync" output device — only apps sent to it, silent on this machine.
    VirtualSink,
    /// No local capture — a web client casts its screen/audio up and the server relays it.
    WebUplink,
}

/// What the screen-video capture grabs — mirrors the audio source picker.
#[derive(PartialEq, Clone, Copy)]
enum VideoSourceKind {
    Off,
    Screen,
    Window,
    /// The casting web client's screen (pairs with a [`SourceKind::WebUplink`] audio source). No
    /// local capture — the caster H.264-encodes its screen and the server relays it.
    WebCast,
}

// (display label, parse token)
const RES_LABELS: [(&str, &str); 4] = [
    ("720p", "720p"),
    ("1080p", "1080p"),
    ("1440p", "1440p"),
    ("2160p (4K)", "2160p"),
];
const ENC_LABELS: [(&str, &str); 2] = [("AV1", "av1"), ("VP9", "vp9")];

/// What each codec is actually FOR, shown on hover. Both are royalty-free — that is why these are
/// the only two offered — so saying it in the label told the operator nothing about which to pick.
/// This does.
const AV1_HINT: &str =
    "Best picture for the bandwidth, and the only one that can encode on the GPU (Windows or the \
     Linux screencast Flatpak). But most \
     phones have no AV1 decoding silicon, so they decode it on the CPU — that's the one that gets \
     hot and stutters.";
const VP9_HINT: &str =
    "Choose this for phones. Almost every Android has a hardware VP9 decoder, so playback is far \
     lighter and cooler there, and it plays on older devices that show a black screen on AV1. Costs \
     more CPU on THIS machine (it encodes on the CPU only) and a little more bandwidth.";

/// Initial server config (from CLI flags) used to seed the GUI + first stream.
pub struct InitialConfig {
    pub capture_source: CaptureSource,
    /// macOS: start with the "Silent on this Mac" option on (see `MediaOptions::silent_local`).
    pub silent_local: bool,
    pub video: Option<VideoConfig>,
    pub encoder: EncoderBackend,
    pub encode_device: EncodeDevice,
    pub buffer_ms: i64,
    pub codec: CodecKind,
    pub bitrate: i32,
    /// Shared A/V offset (ns) — see `MediaOptions::av_offset_ns`. Held as an Arc rather than a value
    /// so it survives an Apply: the stream restarts, the handle (and the operator's tuning) does not.
    pub av_offset_ns: std::sync::Arc<std::sync::atomic::AtomicI64>,
}

impl InitialConfig {
    fn to_options(&self, name: &str) -> MediaOptions {
        MediaOptions {
            name: name.to_string(),
            codec: self.codec,
            bitrate: self.bitrate,
            lead_ms: config::DEFAULT_LEAD_MS,
            buffer_ms: self.buffer_ms,
            av_offset_ns: self.av_offset_ns.clone(),
            capture_source: self.capture_source,
            silent_local: self.silent_local,
            video: self.video,
            video_target: VideoTarget::PrimaryMonitor, // CLI/initial stream starts on the monitor
            encoder: self.encoder,
            encode_device: self.encode_device,
        }
    }
}

/// Launch the GUI. Blocks until the window is closed.
pub fn run(port: u16, server_name: String, init: InitialConfig) -> Result<()> {
    // The single active web-caster's conn_id (web-uplink source), shared with the web server.
    let cast: webserver::CastState = Arc::new(Mutex::new(None));
    let clients = Arc::new(AtomicUsize::new(0));
    let clients_reg: ClientRegistry = Arc::new(Mutex::new(HashMap::new()));
    let status = Arc::new(Mutex::new(StreamStatus::default()));
    let starting = Arc::new(AtomicBool::new(true));
    let (cmd_tx, cmd_rx) = mpsc::channel::<MediaOptions>();
    let (ready_tx, ready_rx) = mpsc::channel::<Option<watch::Receiver<Arc<StreamState>>>>();

    let initial_opts = init.to_options(&server_name);
    // If the chosen source can't start (e.g. no PipeWire monitor on Linux, or capture is
    // unavailable in a sandbox), we still open the GUI by falling back to this no-capture web-cast
    // relay — the window must never fail to appear just because a local capture failed.
    let fallback_opts = MediaOptions {
        capture_source: CaptureSource::WebUplink,
        video: None,
        ..init.to_options(&server_name)
    };

    // Media-control thread: owns the live Media + watch sender. Builds captures off
    // the UI thread (a capture start can block for seconds).
    {
        let status = status.clone();
        let starting = starting.clone();
        std::thread::Builder::new()
            .name("media-control".into())
            .spawn(move || match media::start(initial_opts) {
                Ok(m) => {
                    let (tx, rx) = watch::channel(Arc::new(StreamState::from_media(&m)));
                    status.lock().unwrap().now_serving(&m);
                    starting.store(false, Ordering::Relaxed);
                    let _ = ready_tx.send(Some(rx));
                    control_loop(m, tx, cmd_rx, status, starting);
                }
                Err(e) => {
                    // The chosen source couldn't start (e.g. no PipeWire monitor / capture
                    // unavailable). Don't kill the GUI — fall back to a no-capture web-cast relay
                    // so the window still opens; show the error and let the operator Apply a source.
                    tracing::error!("initial audio source failed: {e:#}; opening GUI in relay mode");
                    match media::start(fallback_opts) {
                        Ok(m) => {
                            let (tx, rx) = watch::channel(Arc::new(StreamState::from_media(&m)));
                            {
                                let mut st = status.lock().unwrap();
                                st.now_serving(&m);
                                st.fell_back = true;
                                st.error = Some(format!(
                                    "The audio source could not start: {e:#}. The server is waiting \
                                     for a device to cast instead. Choose a source and press Apply."
                                ));
                            }
                            starting.store(false, Ordering::Relaxed);
                            let _ = ready_tx.send(Some(rx));
                            control_loop(m, tx, cmd_rx, status, starting);
                        }
                        Err(e2) => {
                            status.lock().unwrap().error =
                                Some(format!("Couldn't start: {e:#} (fallback also failed: {e2:#})"));
                            starting.store(false, Ordering::Relaxed);
                            let _ = ready_tx.send(None);
                        }
                    }
                }
            })?;
    }

    let state_rx = match ready_rx.recv() {
        Ok(Some(rx)) => rx,
        _ => return Err(anyhow!("could not start audio capture — see console for details")),
    };

    // The window reads the SAME stream handle the web server serves from, so its status, its level
    // meter and its "sound stopped" banner come from the live pipeline's own counters.
    let stream_rx = state_rx.clone();

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    {
        let clients = clients.clone();
        let clients_reg = clients_reg.clone();
        let cast = cast.clone();
        // The Android TV app's plain-HTTP port (search + sound-only fallback), next to the main one.
        let tv_addr = SocketAddr::from(([0, 0, 0, 0], webserver::tv_port(port)));
        std::thread::Builder::new()
            .name("web-server".into())
            .spawn(move || match tokio::runtime::Runtime::new() {
                Ok(rt) => {
                    if let Err(e) =
                        rt.block_on(webserver::run(state_rx, clients, clients_reg, cast, addr, true, Some(tv_addr)))
                    {
                        tracing::error!("web server exited: {e:#}");
                    }
                }
                Err(e) => tracing::error!("tokio runtime: {e}"),
            })?;
    }

    let url = connect_url(port);

    // Seed UI widgets from the initial config so flags are reflected.
    let (source, selected_pid) = match init.capture_source {
        CaptureSource::AllExceptSelf => (SourceKind::AllApps, None),
        CaptureSource::System => (SourceKind::System, None),
        CaptureSource::VirtualSink => (SourceKind::VirtualSink, None),
        CaptureSource::WebUplink => (SourceKind::WebUplink, None),
        CaptureSource::App { pid } => (SourceKind::App, Some(pid)),
    };
    let (video_kind, res_idx, fps60) = match init.video {
        Some(v) => {
            // A web-uplink source carries the caster's screen, not a local capture.
            let kind = if matches!(init.capture_source, CaptureSource::WebUplink) {
                VideoSourceKind::WebCast
            } else {
                VideoSourceKind::Screen
            };
            // `--video` on a platform with no local screen capture would otherwise seed a selection
            // the picker renders as disabled but apply() still acts on, asking for video the server
            // then silently drops. Normalize here, at the seed, so the state is unrepresentable —
            // same rule the VP9 `enc_idx` seed follows just below.
            #[cfg(not(any(target_os = "windows", all(target_os = "linux", feature = "linux-capture"), all(target_os = "macos", feature = "mac-capture"))))]
            let kind = if kind == VideoSourceKind::Screen { VideoSourceKind::Off } else { kind };
            (kind, res_to_idx(v.resolution), v.fps == Fps::F60)
        }
        // No `--video`: screen sharing starts OFF every launch (it is never restored — a server that
        // began sharing the screen because it happened to be on last time would be a nasty surprise),
        // but the options it would use are the ones last applied.
        None => (
            VideoSourceKind::Off,
            crate::settings::get("video_res")
                .and_then(|r| RES_LABELS.iter().position(|(_, tok)| *tok == r))
                .unwrap_or(1),
            crate::settings::get("video_fps60").as_deref() == Some("1"),
        ),
    };
    // The requested source could not start and the stream is really the cast relay: make the window
    // SAY that. It used to keep showing the source that failed, with nothing pending, so the one
    // button that could fix it was disabled until the operator changed something else first.
    let fell_back = status.lock().unwrap().fell_back;
    let (source, selected_pid, video_kind) = if fell_back {
        (SourceKind::WebUplink, None, VideoSourceKind::Off)
    } else {
        (source, selected_pid, video_kind)
    };
    // Normalize the seed HERE, not in the UI: VP9 lives behind the non-default `vp9` feature, so on a
    // build without it `enc_idx == 1` must be unrepresentable from the start. Doing this in the codec
    // picker instead would be wrong twice over — that picker sits in a collapsed-by-default disclosure
    // (so it wouldn't run at all until expanded), and mutating state during rendering would flip the
    // Apply button to "changes pending" the moment the operator merely opened the panel.
    let enc_idx = match init.encoder {
        EncoderBackend::Vp9 if cfg!(feature = "vp9") => 1,
        _ => 0,
    };

    // What the server remembers about each device (see settings::DeviceMemo).
    let memos = crate::settings::load_devices();
    let mut client_vols = HashMap::new();
    let mut client_trims = HashMap::new();
    let mut client_names = HashMap::new();
    let mut client_muted = HashMap::new();
    let mut known = HashMap::new();
    for (id, m) in memos {
        client_vols.insert(id.clone(), m.vol);
        client_trims.insert(id.clone(), m.trim_ms);
        client_muted.insert(id.clone(), m.muted);
        if m.custom_name {
            client_names.insert(id.clone(), m.name.clone());
        }
        known.insert(id, KnownDevice { name: m.name, last_seen: m.last_seen });
    }
    let zoom = crate::settings::get("ui_zoom")
        .and_then(|z| z.parse::<f32>().ok())
        .filter(|z| z.is_finite())
        .map(|z| z.clamp(ZOOM_MIN, ZOOM_MAX))
        .unwrap_or(1.0);
    LIGHT_THEME.store(crate::settings::get("theme").as_deref() == Some("light"), Ordering::Relaxed);
    let last_local_source = if source == SourceKind::WebUplink { SourceKind::AllApps } else { source };

    let mut app = ServerApp {
        server_name,
        url,
        last_lan_check: Instant::now(),
        clients,
        clients_reg,
        cast: cast.clone(),
        stream_rx,
        view: MainView::Devices,
        master_vol: crate::settings::get("master_vol")
            .and_then(|v| v.parse::<f32>().ok())
            .filter(|v| v.is_finite())
            .map(|v| v.clamp(0.0, 1.0))
            .unwrap_or(1.0),
        master_muted: false,
        av_offset_ms: (init.av_offset_ns.load(std::sync::atomic::Ordering::Relaxed) / 1_000_000)
            as i32,
        av_offset_ns: init.av_offset_ns.clone(),
        client_vols,
        client_trims,
        client_names,
        client_muted,
        known,
        devices_dirty: false,
        devices_saved_at: Instant::now(),
        seen_stamped_at: Instant::now(),
        connected: std::collections::HashSet::new(),
        editing_client: None,
        edit_name_buf: String::new(),
        sync_open: None,
        drops: HashMap::new(),
        applied: None,
        applied_before: None,
        applied_audio_name: String::new(),
        seen_failures: 0,
        dismissed_error: None,
        notice: None,
        qr_tex: None,
        meter: [0.0; 2],
        meter_at: Instant::now(),
        silent_since: None,
        last_logged_size: None,
        calibrating: false,
        status,
        starting,
        cmd_tx,
        codec: init.codec,
        bitrate: init.bitrate,
        source,
        last_local_source,
        last_local_video: if video_kind == VideoSourceKind::WebCast { VideoSourceKind::Off } else { video_kind },
        #[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
        apps: Vec::new(),
        #[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
        apps_rx: None,
        #[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
        refreshing: false,
        #[cfg(target_os = "linux")]
        routing: AppRouting {
            wanted: crate::settings::load_routed_apps().into_iter().collect(),
            ..Default::default()
        },
        selected_pid,
        selected_name: String::new(),
        silent_local: init.silent_local,
        video_kind,
        #[cfg(target_os = "windows")]
        video_pid: None,
        #[cfg(target_os = "windows")]
        video_name: String::new(),
        #[cfg(target_os = "windows")]
        video_hwnd: None,
        res_idx,
        fps60,
        enc_idx,
        // A saved choice wins over the CLI default: the GUI's picker is the thing the operator
        // last touched, and it is the only way to set this without editing a shortcut.
        encode_device: crate::settings::load_encode_device().unwrap_or(init.encode_device),
        video_quality_pct: init
            .video
            .map(|v| v.quality_pct)
            .or_else(|| crate::settings::get("video_quality").and_then(|q| q.parse().ok()))
            .unwrap_or(100)
            .clamp(40, 250),
        buffer_ms: init.buffer_ms as i32,
        port,
        port_edit: port,
        port_msg: String::new(),
        zoom,
        did_initial_zoom: false,
    };
    #[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
    app.refresh_apps();

    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([1040.0, 720.0])
        // These are winit LOGICAL pixels, which the OS display scale multiplies. egui lays out in
        // POINTS, and points = logical / zoom_factor — so at the default text size (zoom 1.0) the
        // two are the same number, and the floor below is the layout's real minimum: the 300-point
        // sharing column plus a device row that still has a draggable volume slider. Both columns
        // scroll vertically, so going below the content's natural height scrolls instead of clipping.
        //
        // Keep the floor modest. Dragging a CORNER stops the moment EITHER axis hits its floor, so an
        // over-wide minimum makes the whole window feel immovable even while the height has room.
        .with_min_inner_size([720.0, 440.0])
        .with_title("Newfoundsync server")
        // WAYLAND ICONS COME FROM HERE, NOT FROM with_icon BELOW.
        //
        // Wayland has no protocol for a client to hand the compositor a window icon (the recent
        // xdg-toplevel-icon-v1 aside, which this stack does not speak). Instead the compositor
        // matches the toplevel's app_id against an installed .desktop file and uses that file's
        // Icon=. Without an app_id set, winit supplies its own default, KWin finds no matching
        // entry, and the TITLEBAR falls back to a generic placeholder — the Wayland logo — while the
        // taskbar and application menu still look right, because those read the .desktop file
        // directly and never needed the window at all. That asymmetry is the whole symptom.
        //
        // The string must match the .desktop basename EXACTLY (and the Flatpak app id, which is the
        // same): flatpak/ca.newfoundsync.Newfoundsync.desktop.
        .with_app_id(APP_ID);
    // X11 and Windows DO take a pixel icon from the client — Wayland ignores this one. Keep both:
    // the app must look right under either session type.
    if let Ok(icon) = eframe::icon_data::from_png_bytes(include_bytes!("../../../branding/icon-256.png")) {
        viewport = viewport.with_icon(std::sync::Arc::new(icon));
    }
    let options = eframe::NativeOptions {
        viewport,
        // WHY THIS EXISTS: dragging the window corner used to leave the frame trailing well behind
        // the pointer, and the further you dragged the further behind it got.
        //
        // It is a latency bug in the present path, not slow layout. Our `App::ui` is cheap and the
        // QR texture is cached, so the frame itself costs a millisecond or two. What costs the rest:
        //
        //   1. egui-wgpu reconfigures the surface SYNCHRONOUSLY on every single WindowEvent::Resized
        //      (`Painter::on_window_resized` -> `configure_surface`), with no coalescing. On Vulkan
        //      `Surface::configure` calls `release_resources`, which is a full `vkDeviceWaitIdle` --
        //      it blocks until every frame still queued for presentation has actually been shown.
        //   2. With the default `desired_maximum_frame_latency` (wgpu's `get_default_config` picks 2)
        //      there are up to TWO frames queued at any moment, so that wait is up to two monitor
        //      refreshes -- ~33 ms at 60 Hz -- BEFORE the swapchain rebuild even starts.
        //   3. With the default `present_mode` (AutoVsync -> Fifo) the queue is drained strictly on
        //      the refresh clock, which is what makes the wait that long in the first place.
        //
        // Wayland makes the result visible in a way X11 and Windows do not: a resize is client-driven
        // there, so the window edge does not move until WE commit a buffer at the new size. Every
        // millisecond of present latency is a millisecond of the frame lagging the mouse.
        //
        // Note that `NativeOptions::vsync` is NOT the knob -- it is documented as "only affects the
        // `glow` backend", and eframe 0.34 defaults to wgpu. Setting it does nothing here, which is
        // most of why this is hard to find.
        wgpu_options: eframe::egui_wgpu::WgpuConfiguration {
            // wgpu's own docs recommend exactly this for our shape of program: "Choose `1` to
            // minimize latency above all else. [...] For applications like GUIs doing a small amount
            // of GPU work each frame that need low latency, this is a reasonable choice." One frame
            // in flight instead of two halves the `vkDeviceWaitIdle` above.
            desired_maximum_frame_latency: Some(1),
            // Do not gate the queue on the refresh clock. This is the term that actually kills the
            // stall: with nothing waiting on vblank, the wait-idle inside the resize reconfigure
            // returns as soon as the GPU is done (~1 ms) instead of at the next refresh.
            //
            // This does NOT mean tearing on a Wayland session -- the compositor composites whole
            // frames, and we never ask for the tearing-control protocol. The real cost is that while
            // egui is asking for continuous repaints (a hover animation, a slider drag) frames are no
            // longer capped at the refresh rate, so a core works harder than it needs to. That is an
            // acceptable trade for a window that is idle almost all the time -- it repaints once a
            // second when nothing is happening. If the idle/hover CPU ever matters more than resize
            // smoothness, drop THIS line and keep the frame-latency one above; that alone recovers
            // roughly half the win.
            present_mode: eframe::wgpu::PresentMode::AutoNoVsync,
            ..Default::default()
        },
        ..Default::default()
    };
    eframe::run_native(
        "Newfoundsync",
        options,
        Box::new(|cc| {
            setup_style(&cc.egui_ctx, zoom);
            Ok(Box::new(app))
        }),
    )
    .map_err(|e| anyhow!("GUI error: {e}"))
}

// ---- Harbour Glass palette (matches the web client). Runtime light/dark: the header toggle
// flips LIGHT_THEME and every colour below switches, mirroring the web client's two themes. ----
static LIGHT_THEME: AtomicBool = AtomicBool::new(false); // false = dark (default)
#[inline]
fn lt() -> bool { LIGHT_THEME.load(Ordering::Relaxed) }
fn c_bg() -> egui::Color32 { if lt() { egui::Color32::from_rgb(0xf4, 0xf6, 0xfa) } else { egui::Color32::from_rgb(0x0b, 0x0f, 0x15) } }
fn c_surface() -> egui::Color32 { if lt() { egui::Color32::from_rgb(0xff, 0xff, 0xff) } else { egui::Color32::from_rgb(0x16, 0x1c, 0x26) } }
fn c_surface_alt() -> egui::Color32 { if lt() { egui::Color32::from_rgb(0xee, 0xf1, 0xf6) } else { egui::Color32::from_rgb(0x1d, 0x25, 0x31) } }
fn c_border() -> egui::Color32 { if lt() { egui::Color32::from_rgb(0xd6, 0xdd, 0xe6) } else { egui::Color32::from_rgb(0x2a, 0x33, 0x40) } }
fn c_text() -> egui::Color32 { if lt() { egui::Color32::from_rgb(0x16, 0x20, 0x2e) } else { egui::Color32::from_rgb(0xe8, 0xee, 0xf5) } }
fn c_text2() -> egui::Color32 { if lt() { egui::Color32::from_rgb(0x3a, 0x46, 0x57) } else { egui::Color32::from_rgb(0xc9, 0xd4, 0xe0) } }
fn c_dim() -> egui::Color32 { if lt() { egui::Color32::from_rgb(0x5c, 0x68, 0x78) } else { egui::Color32::from_rgb(0x94, 0xa1, 0xb2) } }
fn c_accent() -> egui::Color32 { if lt() { egui::Color32::from_rgb(0x25, 0x63, 0xeb) } else { egui::Color32::from_rgb(0x3b, 0x8e, 0xff) } }
fn c_accent_hi() -> egui::Color32 { if lt() { egui::Color32::from_rgb(0x1d, 0x4e, 0xd8) } else { egui::Color32::from_rgb(0x5a, 0xa2, 0xff) } }
fn c_ok() -> egui::Color32 { if lt() { egui::Color32::from_rgb(0x1a, 0x9c, 0x3e) } else { egui::Color32::from_rgb(0x3f, 0xb9, 0x50) } }
fn c_err() -> egui::Color32 { if lt() { egui::Color32::from_rgb(0xd1, 0x2d, 0x24) } else { egui::Color32::from_rgb(0xf8, 0x51, 0x49) } }
fn c_warn() -> egui::Color32 { if lt() { egui::Color32::from_rgb(0xb9, 0x78, 0x1a) } else { egui::Color32::from_rgb(0xd8, 0xa5, 0x31) } }
// Tinted grounds for a status chip, a banner or the selected option: the semantic colour at low
// strength, so the text colour above still reads on it in both themes.
fn c_ok_bg() -> egui::Color32 { if lt() { egui::Color32::from_rgb(0xe6, 0xf5, 0xea) } else { egui::Color32::from_rgb(0x14, 0x30, 0x1c) } }
fn c_err_bg() -> egui::Color32 { if lt() { egui::Color32::from_rgb(0xfd, 0xec, 0xea) } else { egui::Color32::from_rgb(0x3b, 0x1f, 0x1f) } }
fn c_warn_bg() -> egui::Color32 { if lt() { egui::Color32::from_rgb(0xfb, 0xf3, 0xe2) } else { egui::Color32::from_rgb(0x36, 0x2b, 0x14) } }
fn c_accent_bg() -> egui::Color32 { if lt() { egui::Color32::from_rgb(0xe8, 0xef, 0xfd) } else { egui::Color32::from_rgb(0x16, 0x29, 0x4a) } }

/// Build the egui visuals for the CURRENT theme (dark or light). Called at startup and again
/// whenever the header toggle flips `LIGHT_THEME`, so the whole window re-themes live.
fn theme_visuals() -> egui::Visuals {
    use egui::{Color32, CornerRadius, Stroke};
    let light = lt();
    let mut v = if light { egui::Visuals::light() } else { egui::Visuals::dark() };
    v.window_fill = c_bg();
    v.panel_fill = c_bg();
    v.faint_bg_color = c_surface();
    v.extreme_bg_color = c_surface_alt();
    v.window_corner_radius = CornerRadius::from(10);
    v.hyperlink_color = c_accent_hi();
    v.slider_trailing_fill = true;
    v.selection.bg_fill = c_accent();
    v.selection.stroke = Stroke::new(1.0, if light { Color32::WHITE } else { c_text() });

    // Hover/active surface tints differ per theme (darker-lift on dark, gentle shade on light).
    let (hover_bg, hover_stroke, active_bg) = if light {
        (Color32::from_rgb(0xe2, 0xe8, 0xf2), Color32::from_rgb(0xc2, 0xcd, 0xdb), Color32::from_rgb(0xd5, 0xdf, 0xf0))
    } else {
        (Color32::from_rgb(0x24, 0x30, 0x44), Color32::from_rgb(0x34, 0x40, 0x4f), Color32::from_rgb(0x2a, 0x38, 0x50))
    };
    let cr = CornerRadius::from(8);
    let w = &mut v.widgets;
    w.noninteractive.corner_radius = cr;
    w.noninteractive.bg_fill = c_surface();
    w.noninteractive.weak_bg_fill = c_surface();
    w.noninteractive.bg_stroke = Stroke::new(1.0, c_border());
    w.noninteractive.fg_stroke = Stroke::new(1.0, c_dim());
    w.inactive.corner_radius = cr;
    w.inactive.bg_fill = c_surface_alt();
    w.inactive.weak_bg_fill = c_surface_alt();
    w.inactive.bg_stroke = Stroke::new(1.0, c_border());
    w.inactive.fg_stroke = Stroke::new(1.0, c_text2());
    w.hovered.corner_radius = cr;
    w.hovered.bg_fill = hover_bg;
    w.hovered.weak_bg_fill = hover_bg;
    w.hovered.bg_stroke = Stroke::new(1.0, hover_stroke);
    w.hovered.fg_stroke = Stroke::new(1.0, c_text());
    w.hovered.expansion = 1.0;
    w.active.corner_radius = cr;
    w.active.bg_fill = active_bg;
    w.active.weak_bg_fill = active_bg;
    w.active.bg_stroke = Stroke::new(1.0, c_accent());
    w.active.fg_stroke = Stroke::new(1.0, if light { Color32::BLACK } else { Color32::WHITE });
    w.active.expansion = 0.0;
    w.open = w.hovered.clone();
    v
}

/// The application id, in the reverse-DNS form the desktop stack expects. Must stay identical to
/// the Flatpak app id, the `.desktop` file's basename, and the installed icon's basename — a
/// mismatch in any of them is what makes a desktop show a placeholder icon instead of ours.
const APP_ID: &str = "ca.newfoundsync.Newfoundsync";

/// Text-size range the operator can pick, as an egui zoom factor, and the step of the − / + buttons.
///
/// 1.0 is the size the window is designed at: 14-point body text, which is 14 physical pixels on a
/// display at 1× scale and scales with the OS setting from there. The window used to be designed at
/// a zoom of 0.6, which looked right on a 150%-scaled Windows laptop and drew 9-pixel body text and
/// sub-7-pixel hints on an ordinary 1× monitor. The choice is saved, so anyone who wants the compact
/// window back sets it once.
const ZOOM_MIN: f32 = 0.7;
const ZOOM_MAX: f32 = 2.0;
const ZOOM_STEP: f32 = 0.1;

/// Whether this build can capture this machine's own screen. Windows always can (WGC); Linux and
/// macOS only with their capture feature. Without it the only way to share a screen is a web cast.
const LOCAL_SCREEN: bool = cfg!(any(
    target_os = "windows",
    all(target_os = "linux", feature = "linux-capture"),
    all(target_os = "macos", feature = "mac-capture")
));

/// The sound-quality choices offered in the window: label, codec, Opus bitrate.
const QUALITY_PRESETS: [(&str, CodecKind, i32); 6] = [
    ("Opus 510 kbps · transparent", CodecKind::Opus, 510_000),
    ("Opus 256 kbps · transparent", CodecKind::Opus, 256_000),
    ("Opus 160 kbps · near-CD", CodecKind::Opus, 160_000),
    ("Opus 128 kbps · very good", CodecKind::Opus, 128_000),
    ("Opus 96 kbps · good", CodecKind::Opus, 96_000),
    ("Lossless PCM · 1.5 Mbps", CodecKind::Pcm, 0),
];

/// The address devices open, from this machine's current LAN IP.
fn connect_url(port: u16) -> String {
    let lan = net::primary_lan_ipv4()
        .map(|ip| ip.to_string())
        .unwrap_or_else(|| "<this-pc>".into());
    format!("https://{lan}:{port}")
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Apply the theme + spacing/fonts/text size once at startup.
fn setup_style(ctx: &egui::Context, zoom: f32) {
    use egui::{FontId, TextStyle};

    let mut s = (*ctx.style()).clone();
    s.visuals = theme_visuals();
    s.spacing.item_spacing = egui::vec2(8.0, 6.0);
    s.spacing.button_padding = egui::vec2(10.0, 5.0);
    s.spacing.interact_size.y = 26.0;
    s.spacing.indent = 16.0;
    s.spacing.slider_rail_height = 6.0;
    s.text_styles.insert(TextStyle::Heading, FontId::proportional(18.0));
    s.text_styles.insert(TextStyle::Body, FontId::proportional(14.0));
    s.text_styles.insert(TextStyle::Button, FontId::proportional(13.5));
    s.text_styles.insert(TextStyle::Monospace, FontId::monospace(13.0));
    s.text_styles.insert(TextStyle::Small, FontId::proportional(12.0));
    ctx.set_style(s);
    ctx.set_zoom_factor(zoom);
}

/// Open a URL in the user's default browser.
fn open_url(url: &str) {
    let spawn = {
        #[cfg(target_os = "windows")]
        {
            // `cmd /C start "" "<url>"` hands the URL to the default protocol handler.
            std::process::Command::new("cmd")
                .args(["/C", "start", "", url])
                .spawn()
        }
        #[cfg(target_os = "macos")]
        {
            std::process::Command::new("open").arg(url).spawn()
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            std::process::Command::new("xdg-open").arg(url).spawn()
        }
    };
    if let Err(e) = spawn {
        tracing::warn!("couldn't open browser: {e}");
    }
}

/// What the media-control thread tells the window about the stream.
///
/// This replaces a single status STRING that carried the durable "Serving: …" line, transient
/// hints and errors all at once. Everything that read it had to guess which of the three it was
/// holding by how the sentence started, and one overwrote another: a validation hint erased the
/// serving line, a long error was cut to one line with nowhere to read the rest. Each fact now
/// has its own field.
#[derive(Clone, Default)]
struct StreamStatus {
    /// The capture device of the stream being served. `None` until the first one is up.
    serving: Option<String>,
    /// That stream carries video.
    video: bool,
    /// The last failure to start or switch, in full. Cleared by the next start that succeeds.
    error: Option<String>,
    /// Counts failed switches, so the window can tell that one happened (and roll its "applied"
    /// baseline back to what is really streaming).
    failures: u64,
    /// The start in progress is going to open the desktop portal's screen-share dialog.
    ///
    /// On Linux that request blocks until somebody answers the dialog. A bare "Starting…" during
    /// that wait is actively misleading — reported from the field as "it flashes up the screen
    /// grabber then sticks at Starting forever", which was really a 45-second wait for a prompt
    /// the operator had already dismissed. Naming the thing being waited on turns a hang into an
    /// instruction.
    waiting_on_portal: bool,
    /// The source asked for at launch could not start and the stream is the no-capture cast relay.
    fell_back: bool,
}

impl StreamStatus {
    fn now_serving(&mut self, m: &Media) {
        self.serving = Some(m.capture_device.clone());
        self.video = m.config.video;
        self.error = None;
    }

    /// The capture device is a dummy/null sink, so the stream carries silence.
    ///
    /// Silence is the one failure the window cannot show by staying quiet: every control looks
    /// correct while the stream carries nothing.
    fn is_dummy(&self) -> bool {
        self.serving.as_deref().is_some_and(|d| d.contains(crate::capture::DUMMY_TAG))
    }

    /// The device name without the dummy marker (which the window shows as a banner instead).
    fn device_label(&self) -> String {
        self.serving
            .as_deref()
            .map(|d| d.replace(crate::capture::DUMMY_TAG, "").trim().to_string())
            .unwrap_or_default()
    }
}

/// Would starting a stream with these options open the portal's screen-share dialog?
fn opens_portal(video: bool, capture_source: CaptureSource) -> bool {
    cfg!(all(target_os = "linux", feature = "linux-capture"))
        && video
        && !matches!(capture_source, CaptureSource::WebUplink)
}

fn res_to_idx(r: Resolution) -> usize {
    match r {
        Resolution::P720 => 0,
        Resolution::P1080 => 1,
        Resolution::P1440 => 2,
        Resolution::P2160 => 3,
    }
}

/// Runs on the media-control thread: owns the live `Media`, rebuilds it on each
/// command, and republishes the stream. The old `Media` is dropped (capture
/// threads joined) only after the new one is live.
fn control_loop(
    mut current: Media,
    tx: watch::Sender<Arc<StreamState>>,
    cmd_rx: mpsc::Receiver<MediaOptions>,
    status: Arc<Mutex<StreamStatus>>,
    starting: Arc<AtomicBool>,
) {
    while let Ok(opts) = cmd_rx.recv() {
        starting.store(true, Ordering::Relaxed);
        status.lock().unwrap().waiting_on_portal = opens_portal(opts.video.is_some(), opts.capture_source);
        match media::start(opts) {
            Ok(m) => {
                let _ = tx.send(Arc::new(StreamState::from_media(&m)));
                status.lock().unwrap().now_serving(&m);
                // Drop the previous stream (threads joined) now that the new one is live — and with
                // the status lock released: joining capture threads can take a moment, and the
                // window reads that lock every frame.
                current = m;
            }
            Err(e) => {
                // Keep serving the previous source; report the failure in full.
                let mut st = status.lock().unwrap();
                st.error = Some(format!("Couldn't switch: {e:#}"));
                st.failures += 1;
            }
        }
        status.lock().unwrap().waiting_on_portal = false;
        starting.store(false, Ordering::Relaxed);
    }
    drop(current); // GUI closed → stop capture
}

/// The apply-relevant stream settings, snapshotted when Apply runs. Comparing the live
/// widget values against this tells the GUI whether there are unapplied changes (source,
/// video, and buffer are NOT live — unlike volume/sync — so the operator needs a signal).
#[derive(Clone, PartialEq)]
struct AppliedConfig {
    source: SourceKind,
    selected_pid: Option<u32>,
    silent_local: bool,
    video_kind: VideoSourceKind,
    #[cfg(target_os = "windows")]
    video_hwnd: Option<isize>,
    res_idx: usize,
    fps60: bool,
    enc_idx: usize,
    encode_device: EncodeDevice,
    video_quality_pct: u16,
    buffer_ms: i32,
    codec: CodecKind,
    bitrate: i32,
}

impl Drop for ServerApp {
    /// The window is closing: write anything about a device that the debounce was still holding.
    fn drop(&mut self) {
        if self.devices_dirty {
            let now = unix_now();
            for id in &self.connected {
                if let Some(k) = self.known.get_mut(id) {
                    k.last_seen = now;
                }
            }
            self.write_devices();
        }
    }
}

/// What the main area of the window is showing.
#[derive(Clone, Copy, PartialEq)]
enum MainView {
    /// The mixer. With no device connected this shows the first-connection instructions instead.
    Devices,
    /// Address + QR code, for connecting another device.
    AddDevice,
    Settings,
}

/// A device the server has seen before, connected or not.
struct KnownDevice {
    /// What the device calls itself (its HELLO name), kept so it can be listed while offline.
    name: String,
    /// Unix seconds when it was last connected.
    last_seen: u64,
}

/// One connected client, copied out of the registry for a frame (never hold that lock across UI).
#[derive(Clone)]
struct ClientRow {
    conn_id: u64,
    stable_id: String,
    name: String,
    identified: bool,
    calib_status: String,
    reported_trim: Option<i32>,
    /// Frames the server could not deliver to it — see `webserver::ClientEntry::frames_dropped`.
    dropped: u64,
}

struct ServerApp {
    server_name: String,
    /// The address devices open. Re-derived periodically: a laptop that changes network, or a VPN
    /// coming up, moves the LAN address, and a stale address in the QR code fails with no hint.
    url: String,
    last_lan_check: Instant,
    clients: Arc<AtomicUsize>,
    /// Live per-client registry shared with the web server — render the list and
    /// push per-client volume through each entry's `ctrl_tx`.
    clients_reg: ClientRegistry,
    /// The single active web-caster's conn_id (shared with the web server). The GUI reads it to
    /// surface that caster's "Stop cast" control and writes `None` to free the slot when stopped.
    cast: webserver::CastState,
    /// The stream the web server is serving right now — for its health counters and level meter.
    stream_rx: watch::Receiver<Arc<StreamState>>,
    view: MainView,
    /// Server master volume (0..=1): scales every client's effective remote volume.
    master_vol: f32,
    /// Master mute. Kept SEPARATE from `master_vol` so muting preserves the level to come back to;
    /// folded into the pushed gain in `push_client_state`, exactly like a per-client mute.
    master_muted: bool,
    /// Live A/V offset in MILLISECONDS (what the slider edits). Mirrored into `av_offset_ns` in
    /// nanoseconds on change, which is what the audio producer actually reads.
    av_offset_ms: i32,
    av_offset_ns: std::sync::Arc<std::sync::atomic::AtomicI64>,
    /// Per-client (pre-master) volume, keyed by the client's *stable* id so it
    /// survives reconnects. Absent ⇒ 1.0 (full) for a client we haven't touched.
    client_vols: HashMap<String, f32>,
    /// Per-client server sync offset in ms, keyed by stable id (survives reconnects).
    /// Absent ⇒ 0. Adds to the device's own trim — positive = play later.
    client_trims: HashMap<String, i32>,
    /// Server-assigned display name override, keyed by stable id. Wins over the
    /// client's self-reported HELLO name; survives that device reconnecting.
    client_names: HashMap<String, String>,
    /// Server-side mute, keyed by stable id. Muted ⇒ effective remote volume 0 (the
    /// device's own slider value is preserved and restored on un-mute). Survives reconnect.
    client_muted: HashMap<String, bool>,
    /// Every device seen, by stable id. Together with the four maps above this is what gets saved
    /// (see `save_devices_if_due`), so names, levels and offsets survive a restart of the server.
    known: HashMap<String, KnownDevice>,
    /// Something about a device changed and has not been written to disk yet.
    devices_dirty: bool,
    devices_saved_at: Instant,
    /// When the connected devices' `last_seen` was last refreshed.
    seen_stamped_at: Instant,
    /// Stable ids connected on the previous frame, to notice the moment one leaves.
    connected: std::collections::HashSet<String>,
    /// Stable id of the client whose name is being edited inline (double-click), if any.
    editing_client: Option<String>,
    /// Scratch buffer backing the inline rename text field.
    edit_name_buf: String,
    /// Stable id of the device whose sync controls are open, if any.
    sync_open: Option<String>,
    /// Per connection: the dropped-frame count last seen, and when it last went UP. "Falling
    /// behind" is said only while it is still climbing; a count that stopped moving is history.
    drops: HashMap<u64, (u64, Option<Instant>)>,
    /// The stream config last handed to the control thread, so the window can show what is
    /// pending (source/video/delay/quality are NOT live; they take effect on Apply).
    applied: Option<AppliedConfig>,
    /// The baseline before the Apply in flight. If that switch fails the previous stream keeps
    /// serving, so the baseline is rolled back to this and the change shows as pending again.
    applied_before: Option<AppliedConfig>,
    /// The picked app's name as of the last Apply, so Discard can put the label back with the pid.
    applied_audio_name: String,
    seen_failures: u64,
    /// The error text the operator dismissed; the banner stays away until a different one arrives.
    dismissed_error: Option<String>,
    /// A short confirmation for the footer ("Sync reset…"), with when it was raised.
    notice: Option<(String, Instant)>,
    /// Cached QR texture of the connect URL, with the URL it encodes (rebuilt if the address moves).
    qr_tex: Option<(String, egui::TextureHandle)>,
    /// The level meter as drawn: the pipeline's peaks, with a fall rate so the bars drop smoothly.
    meter: [f32; 2],
    meter_at: Instant,
    /// Since when a local source has carried nothing but digital silence.
    silent_since: Option<Instant>,
    /// Last window size we logged, so the geometry diagnostic prints on change rather than per frame.
    last_logged_size: Option<(i32, i32)>,
    /// True while a server-orchestrated "Calibrate all" run is active (Phase B).
    calibrating: bool,
    status: Arc<Mutex<StreamStatus>>,
    starting: Arc<AtomicBool>,
    cmd_tx: mpsc::Sender<MediaOptions>,
    codec: CodecKind,
    bitrate: i32,
    source: SourceKind,
    /// The local source / screen choice to come back to when leaving "A device casts".
    last_local_source: SourceKind,
    last_local_video: VideoSourceKind,
    #[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
    apps: Vec<AudioApp>,
    // Async source enumeration: refresh_apps() spawns a worker and stashes the receiver here;
    // poll_refresh() (called each frame) applies the result. Never blocks the GUI thread.
    #[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
    apps_rx: Option<mpsc::Receiver<Vec<AudioApp>>>,
    #[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
    refreshing: bool,
    #[cfg(target_os = "linux")]
    routing: AppRouting,
    selected_pid: Option<u32>,
    selected_name: String,
    /// macOS: "Silent on this Mac" — the capture's process tap mutes what it captures.
    silent_local: bool,
    // Screen sharing: off / whole screen / a specific window / the caster's screen.
    video_kind: VideoSourceKind,
    #[cfg(target_os = "windows")]
    video_pid: Option<u32>,
    #[cfg(target_os = "windows")]
    video_name: String,
    #[cfg(target_os = "windows")]
    video_hwnd: Option<isize>,
    res_idx: usize,
    fps60: bool,
    enc_idx: usize,
    /// Where video encodes. Persisted, so a machine with a flaky hardware encoder stays on CPU
    /// across restarts instead of being re-bitten every launch.
    encode_device: EncodeDevice,
    video_quality_pct: u16, // video quality as % of baseline bitrate (slider; 100 = default)
    buffer_ms: i32,
    /// The HTTP port the server is CURRENTLY bound to (fixed for this run; used in the URL/QR).
    port: u16,
    /// Edit buffer for the GUI port field. Saving it persists for the NEXT launch (a live rebind
    /// isn't worth juggling the TLS socket for a rare change), so a "restart to apply" note shows.
    port_edit: u16,
    /// Transient status under the port field ("Saved — restart to apply", or an error).
    port_msg: String,
    /// Text size as an egui zoom factor (see [`ZOOM_MIN`]). Saved whenever it changes.
    zoom: f32,
    did_initial_zoom: bool, // applied the saved text size once (first frame)
}

impl ServerApp {
    /// Push each connected client its effective remote volume (`per-client × master`,
    /// clamped to [0,1]) and its server sync offset (ms), but only when a value changed
    /// since the last push (so dragging a slider doesn't flood, and idle frames send
    /// nothing). `e.volume` / `e.trim_ms` record what was last actually delivered.
    fn push_client_state(&self) {
        if let Ok(mut reg) = self.clients_reg.lock() {
            for e in reg.values_mut() {
                let per = self.client_vols.get(&e.stable_id).copied().unwrap_or(1.0);
                let muted = self.client_muted.get(&e.stable_id).copied().unwrap_or(false);
                let eff = if muted || self.master_muted {
                    0.0
                } else {
                    (per * self.master_vol).clamp(0.0, 1.0)
                };
                if (eff - e.volume).abs() > 1e-3
                    && e.ctrl_tx.send(webserver::set_volume_msg(eff)).is_ok()
                {
                    e.volume = eff;
                }
                let trim = self.client_trims.get(&e.stable_id).copied().unwrap_or(0);
                if trim != e.trim_ms && e.ctrl_tx.send(webserver::set_trim_msg(trim)).is_ok() {
                    e.trim_ms = trim;
                }
            }
        }
    }

    /// Start a server-orchestrated "Calibrate all" run (Phase B): the first identified client
    /// becomes the reference (loops its code); every other becomes a follower that aligns to it.
    /// Followers share the reference's code seed (so they hear it) but each gets a DISTINCT
    /// self-test seed + a TDMA slot, so their self-tests don't collide. Returns false if there
    /// aren't at least two identified clients to calibrate.
    fn start_calibrate_all(&mut self) -> bool {
        // A fixed reference seed (any value — reference + followers just have to agree).
        const REF_SEED: u32 = 0x9e37_79b9;
        if let Ok(mut reg) = self.clients_reg.lock() {
            let mut ids: Vec<u64> = reg.values().filter(|e| e.identified).map(|e| e.conn_id).collect();
            if ids.len() < 2 {
                return false;
            }
            ids.sort_unstable(); // deterministic: lowest conn_id (earliest connect) is the reference
            let ref_id = ids[0];
            let mut fslot: u8 = 0; // follower TDMA slot — first follower gets slot 0 (no wait)
            for &id in &ids {
                if let Some(e) = reg.get_mut(&id) {
                    let msg = if id == ref_id {
                        webserver::calib_role_msg(1, REF_SEED, 0, 0) // reference: emit the code
                    } else {
                        // Distinct self-test seed per follower (reserved for CDMA) + a TDMA slot
                        // (0,1,2,…) so their self-tests serialize and don't overlap acoustically.
                        let self_seed = REF_SEED ^ (0x9e37_79b1u32.wrapping_mul(fslot as u32 + 1));
                        let m = webserver::calib_role_msg(2, REF_SEED, self_seed, fslot);
                        fslot = fslot.saturating_add(1);
                        m
                    };
                    let _ = e.ctrl_tx.send(msg);
                    e.calib_status = if id == ref_id { "reference".into() } else { "queued…".into() };
                }
            }
        } else {
            return false;
        }
        self.calibrating = true;
        true
    }

    /// Tell every client to abort/finish calibration (CALIB_CTRL stop) — this un-mutes the
    /// reference and stops its emission. `clear_status` wipes the per-client results (manual
    /// Stop); auto-complete passes false so the aligned/failed results stay visible.
    fn stop_calibrate_all(&mut self, clear_status: bool) {
        if let Ok(mut reg) = self.clients_reg.lock() {
            for e in reg.values_mut() {
                let _ = e.ctrl_tx.send(webserver::calib_stop_msg());
                if clear_status {
                    e.calib_status = String::new();
                }
            }
        }
        self.calibrating = false;
    }

    /// The sound quality currently selected, in the words of the picker. A bitrate that is not one
    /// of the presets (set with `--bitrate`) is shown as what it is.
    fn quality_label(&self) -> String {
        match self.codec {
            CodecKind::Pcm => QUALITY_PRESETS[QUALITY_PRESETS.len() - 1].0.to_string(),
            CodecKind::Opus => QUALITY_PRESETS
                .iter()
                .find(|(_, c, b)| *c == CodecKind::Opus && *b == self.bitrate)
                .map(|(l, _, _)| (*l).to_string())
                .unwrap_or_else(|| format!("Opus {} kbps", (self.bitrate / 1000).max(1))),
        }
    }

    /// Raise a short confirmation in the footer.
    fn say(&mut self, text: impl Into<String>) {
        self.notice = Some((text.into(), Instant::now()));
    }

    /// The connected clients, in an order that does not move.
    ///
    /// Sorted by the name shown, then stable id — NOT by connection id, which is new on every
    /// reconnect: an Apply reconnects every device at once and used to reshuffle the list each time.
    fn client_rows(&self) -> Vec<ClientRow> {
        let mut rows: Vec<ClientRow> = Vec::new();
        if let Ok(reg) = self.clients_reg.lock() {
            for e in reg.values() {
                rows.push(ClientRow {
                    conn_id: e.conn_id,
                    stable_id: e.stable_id.clone(),
                    name: e.name.clone(),
                    identified: e.identified,
                    calib_status: e.calib_status.clone(),
                    reported_trim: e.reported_trim_ms,
                    dropped: e.frames_dropped.load(Ordering::Relaxed),
                });
            }
        }
        rows.sort_by_cached_key(|r| {
            (!r.identified, self.display_name(r).to_lowercase(), r.stable_id.clone(), r.conn_id)
        });
        rows
    }

    /// The name to show for a client: the one typed here, else its own, else a placeholder.
    fn display_name(&self, r: &ClientRow) -> String {
        if let Some(n) = self.client_names.get(&r.stable_id) {
            return n.clone();
        }
        if r.name.trim().is_empty() {
            format!("Client {}", r.conn_id)
        } else {
            r.name.clone()
        }
    }

    /// Note which devices are connected, and track each one's dropped frames.
    fn remember_devices(&mut self, rows: &[ClientRow]) {
        // `last_seen` is stamped when a device arrives, when it leaves, and every few minutes in
        // between (so a server that is killed still knows roughly when it last saw each one).
        let stamp = self.seen_stamped_at.elapsed() >= Duration::from_secs(300);
        let now = unix_now();
        let here: std::collections::HashSet<String> =
            rows.iter().filter(|r| r.identified).map(|r| r.stable_id.clone()).collect();
        for gone in self.connected.difference(&here) {
            if let Some(k) = self.known.get_mut(gone) {
                k.last_seen = now;
                self.devices_dirty = true;
            }
        }
        self.connected = here;
        for r in rows.iter().filter(|r| r.identified) {
            match self.known.get_mut(&r.stable_id) {
                Some(k) => {
                    if k.name != r.name {
                        k.name = r.name.clone();
                        self.devices_dirty = true;
                    }
                    if stamp {
                        k.last_seen = now;
                        self.devices_dirty = true;
                    }
                }
                None => {
                    self.known.insert(r.stable_id.clone(), KnownDevice { name: r.name.clone(), last_seen: now });
                    self.devices_dirty = true;
                }
            }
            let d = self.drops.entry(r.conn_id).or_insert((r.dropped, None));
            if r.dropped > d.0 {
                *d = (r.dropped, Some(Instant::now()));
            }
        }
        if stamp {
            self.seen_stamped_at = Instant::now();
        }
        // Connection ids are never reused, so entries for gone connections only ever accumulate.
        if self.drops.len() > rows.len() + 32 {
            self.drops.retain(|id, _| rows.iter().any(|r| r.conn_id == *id));
        }
    }

    /// Write the device memory to disk if it changed. Not while a slider is held — `changed()`
    /// fires on every mouse-move of a drag, and each save rewrites the whole settings file.
    fn save_devices_if_due(&mut self, ctx: &egui::Context) {
        if !self.devices_dirty
            || ctx.input(|i| i.pointer.any_down())
            || self.devices_saved_at.elapsed() < Duration::from_millis(800)
        {
            return;
        }
        self.write_devices();
    }

    fn write_devices(&mut self) {
        let mut out = HashMap::new();
        for (id, k) in &self.known {
            let custom = self.client_names.get(id);
            out.insert(
                id.clone(),
                crate::settings::DeviceMemo {
                    name: custom.cloned().unwrap_or_else(|| k.name.clone()),
                    custom_name: custom.is_some(),
                    vol: self.client_vols.get(id).copied().unwrap_or(1.0),
                    trim_ms: self.client_trims.get(id).copied().unwrap_or(0),
                    muted: self.client_muted.get(id).copied().unwrap_or(false),
                    last_seen: k.last_seen,
                },
            );
        }
        if let Err(e) = crate::settings::save_devices(&out) {
            tracing::debug!("could not save the device list: {e}");
        }
        self.devices_dirty = false;
        self.devices_saved_at = Instant::now();
    }

    /// Forget a device that is not connected: its name, level and sync offset.
    fn forget_device(&mut self, id: &str) {
        self.known.remove(id);
        self.client_vols.remove(id);
        self.client_trims.remove(id);
        self.client_names.remove(id);
        self.client_muted.remove(id);
        self.devices_dirty = true;
    }

    /// Kick the active caster off the cast slot.
    ///
    /// The server is the SOLE authority on the slot and the caster's own stopCast doesn't notify
    /// it — so free the slot IMMEDIATELY (letting another client claim it) AND tell the caster's
    /// browser to tear down its uplink. No ack wait.
    fn stop_cast(&mut self, cid: u64) {
        if let Ok(mut s) = self.cast.lock() {
            if *s == Some(cid) {
                *s = None;
            }
        }
        if let Ok(reg) = self.clients_reg.lock() {
            if let Some(e) = reg.get(&cid) {
                let _ = e.ctrl_tx.send(webserver::cast_stop_msg());
            }
        }
    }

    /// Reset sync to 0 ms on one device (`Some(stable id)`) or on every connected one (`None`).
    ///
    /// A sync reset has TWO halves and needs both. Clearing our own offset only moves the value
    /// the server commands; a calibrated device keeps its whole speaker-latency compensation in
    /// its OWN trim, which lives in the browser and is reachable only by asking. Sending one
    /// without the other leaves the device sitting at a number nobody chose.
    fn reset_sync(&mut self, only: Option<&str>) {
        if let Ok(mut reg) = self.clients_reg.lock() {
            for e in reg.values_mut() {
                if only.is_some_and(|id| id != e.stable_id) {
                    continue;
                }
                self.client_trims.insert(e.stable_id.clone(), 0);
                let _ = e.ctrl_tx.send(webserver::reset_sync_msg());
                // DELIBERATELY does not touch `e.trim_ms`. That field records what was last
                // DELIVERED and is the dedup guard in push_client_state
                // (`if trim != e.trim_ms { send(set_trim_msg(trim)) }`), so zeroing it here made
                // the guard see 0 == 0 on the next frame and skip the SET_TRIM 0 entirely.
                //
                // That silently halved the reset: MSG_RESET_SYNC clears the device's OWN trim,
                // but the client deliberately leaves the server-pushed `remoteTrimMs` alone
                // ("the server's own reset clears both ends" — app.js), so nothing cleared it.
                // The operator saw "Sync reset to 0 ms", the slider read 0, and the device kept
                // playing at the old offset until its next report repainted the row.
                //
                // Leaving the mirror alone means the guard now sees 0 != old and pushes
                // SET_TRIM 0 on the very next frame, through the one code path that owns
                // delivering trims.
            }
        }
        self.devices_dirty = true;
        self.say(if only.is_some() {
            "Sync reset to 0 ms on that device."
        } else {
            "Sync reset to 0 ms on every connected device."
        });
    }

    /// Finish a "Calibrate all" run once every follower has reported an outcome (or left).
    fn tick_calibration(&mut self, rows: &[ClientRow]) {
        if !self.calibrating {
            return;
        }
        let ids: Vec<&ClientRow> = rows.iter().filter(|c| c.identified).collect();
        let ref_id = ids.iter().map(|c| c.conn_id).min();
        let mut followers = 0;
        let mut done = 0;
        for c in &ids {
            if Some(c.conn_id) == ref_id {
                continue;
            }
            followers += 1;
            let s = c.calib_status.as_str();
            if s.contains("aligned") || s.contains("failed") || s.contains("no lock") {
                done += 1;
            }
        }
        if ref_id.is_none() || followers == 0 || done == followers {
            self.stop_calibrate_all(false);
        }
    }

    /// Switch between "this computer" and "a device casts", remembering the local choice.
    fn set_cast_mode(&mut self, cast: bool) {
        if cast == (self.source == SourceKind::WebUplink) {
            return;
        }
        if cast {
            self.last_local_source = self.source;
            self.last_local_video = self.video_kind;
            self.source = SourceKind::WebUplink;
            // A cast as the source is audio only until "relay its screen" is ticked.
            self.video_kind = VideoSourceKind::Off;
        } else {
            self.source = self.last_local_source;
            self.video_kind = self.last_local_video;
            #[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
            if self.source == SourceKind::App {
                self.refresh_apps();
            }
        }
    }

    /// Choose a local audio source.
    fn pick_local(&mut self, kind: SourceKind) {
        self.source = kind;
        self.last_local_source = kind;
        // You can't relay the caster's screen while capturing local audio — the uplink is one source.
        if self.video_kind == VideoSourceKind::WebCast {
            self.video_kind = VideoSourceKind::Off;
        }
    }

    /// How many settings differ from what is streaming. Counted in the groups the operator thinks
    /// in (source, screen, video options, delay, sound quality), not in raw fields.
    fn change_count(&self) -> usize {
        let Some(a) = &self.applied else { return 0 };
        let c = self.current_config();
        let mut n = 0;
        n += (a.source != c.source || a.selected_pid != c.selected_pid || a.silent_local != c.silent_local)
            as usize;
        #[cfg(target_os = "windows")]
        let window_changed = a.video_hwnd != c.video_hwnd;
        #[cfg(not(target_os = "windows"))]
        let window_changed = false;
        n += (a.video_kind != c.video_kind || window_changed) as usize;
        n += (a.res_idx != c.res_idx
            || a.fps60 != c.fps60
            || a.enc_idx != c.enc_idx
            || a.encode_device != c.encode_device
            || a.video_quality_pct != c.video_quality_pct) as usize;
        n += (a.buffer_ms != c.buffer_ms) as usize;
        n += (a.codec != c.codec || a.bitrate != c.bitrate) as usize;
        n
    }

    /// Why Apply cannot run right now, if it cannot.
    fn apply_blocker(&self) -> Option<&'static str> {
        if self.source == SourceKind::App && self.selected_pid.is_none() {
            return Some("Pick the app to share first.");
        }
        #[cfg(target_os = "windows")]
        if self.video_kind == VideoSourceKind::Window && self.video_hwnd.is_none() {
            return Some("Pick the window to share first.");
        }
        None
    }

    /// Put every Apply-gated control back to what is streaming.
    fn discard_changes(&mut self) {
        let Some(a) = self.applied.clone() else { return };
        self.source = a.source;
        self.selected_pid = a.selected_pid;
        self.silent_local = a.silent_local;
        self.selected_name = if a.selected_pid.is_some() { self.applied_audio_name.clone() } else { String::new() };
        self.video_kind = a.video_kind;
        #[cfg(target_os = "windows")]
        {
            self.video_hwnd = a.video_hwnd;
            let found = self.apps.iter().find(|w| w.hwnd.is_some() && w.hwnd == a.video_hwnd);
            self.video_pid = found.map(|w| w.pid);
            self.video_name = found.map(|w| w.name.clone()).unwrap_or_default();
        }
        self.res_idx = a.res_idx;
        self.fps60 = a.fps60;
        self.enc_idx = a.enc_idx;
        self.encode_device = a.encode_device;
        self.video_quality_pct = a.video_quality_pct;
        self.buffer_ms = a.buffer_ms;
        self.codec = a.codec;
        self.bitrate = a.bitrate;
        if a.source != SourceKind::WebUplink {
            self.last_local_source = a.source;
        }
    }

    /// Re-derive the connect address every few seconds, in case this machine changed network.
    fn refresh_address(&mut self) {
        if self.last_lan_check.elapsed() < Duration::from_secs(5) {
            return;
        }
        self.last_lan_check = Instant::now();
        let url = connect_url(self.port);
        if url != self.url {
            tracing::info!(old = %self.url, new = %url, "this machine's LAN address changed");
            self.url = url;
        }
    }

    /// Snapshot of the apply-relevant settings as they currently sit in the UI. Compared
    /// against `self.applied` to decide whether the Apply button shows "changes pending".
    fn current_config(&self) -> AppliedConfig {
        AppliedConfig {
            source: self.source,
            selected_pid: self.selected_pid,
            silent_local: self.silent_local,
            video_kind: self.video_kind,
            #[cfg(target_os = "windows")]
            video_hwnd: self.video_hwnd,
            res_idx: self.res_idx,
            fps60: self.fps60,
            enc_idx: self.enc_idx,
            encode_device: self.encode_device,
            video_quality_pct: self.video_quality_pct,
            // Record the value apply() actually sends (clamped), so the dirty comparison and
            // the applied baseline agree even if buffer_ms was seeded out of range from the CLI.
            buffer_ms: config::clamp_buffer_ms(self.buffer_ms as i64) as i32,
            codec: self.codec,
            bitrate: self.bitrate,
        }
    }

    /// Lazily build (and cache) a black-on-white QR texture of the connect URL so phones
    /// can scan instead of hand-typing `https://<ip>:<port>`. Rebuilt only if the address moves.
    /// Returns None if QR generation fails (URL too long, etc.).
    fn qr_texture(&mut self, ctx: &egui::Context) -> Option<egui::TextureHandle> {
        if let Some((url, tex)) = &self.qr_tex {
            if *url == self.url {
                return Some(tex.clone());
            }
        }
        let code = qrcode::QrCode::new(self.url.as_bytes()).ok()?;
        let w = code.width();
        let quiet = 4usize; // mandatory light border so scanners lock on
        let scale = 6usize; // px per module
        let dim = (w + 2 * quiet) * scale;
        let colors = code.to_colors();
        let mut rgba = vec![255u8; dim * dim * 4]; // start all-white
        for my in 0..w {
            for mx in 0..w {
                if colors[my * w + mx] == qrcode::Color::Dark {
                    for dy in 0..scale {
                        for dx in 0..scale {
                            let px = (mx + quiet) * scale + dx;
                            let py = (my + quiet) * scale + dy;
                            let i = (py * dim + px) * 4;
                            rgba[i] = 0;
                            rgba[i + 1] = 0;
                            rgba[i + 2] = 0;
                        }
                    }
                }
            }
        }
        let img = egui::ColorImage::from_rgba_unmultiplied([dim, dim], &rgba);
        let tex = ctx.load_texture("connect-qr", img, egui::TextureOptions::NEAREST);
        self.qr_tex = Some((self.url.clone(), tex.clone()));
        Some(tex)
    }

    /// Kick off a source enumeration on a worker thread. Does NOT block the GUI thread —
    /// `list_sources` internally spawns an MTA thread and joins it, so joining it on the
    /// GUI/STA thread would freeze the window (and could deadlock against the worker's COM
    /// teardown). The result is applied later by `poll_refresh`.
    #[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
    fn refresh_apps(&mut self) {
        if self.refreshing {
            return; // one already in flight
        }
        let pid = std::process::id();
        let (tx, rx) = mpsc::channel();
        if std::thread::Builder::new()
            .name("source-refresh".into())
            .spawn(move || {
                let _ = tx.send(sessions::list_sources(pid));
            })
            .is_ok()
        {
            self.apps_rx = Some(rx);
            self.refreshing = true;
        }
    }

    /// Apply a finished refresh, if one is ready. Called once per frame; never blocks.
    #[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
    fn poll_refresh(&mut self) {
        let Some(rx) = &self.apps_rx else { return };
        match rx.try_recv() {
            Ok(apps) => {
                self.apps = apps;
                self.apps_rx = None;
                self.refreshing = false;
                // Reconcile the AUDIO selection (by pid) against the fresh list.
                if let Some(pid) = self.selected_pid {
                    match self.apps.iter().find(|a| a.pid == pid).map(|a| a.name.clone()) {
                        Some(name) => self.selected_name = name,
                        None => {
                            // The picked app dropped out of the refreshed list (its PID changed,
                            // or its audio session ended). Do NOT silently fall back to
                            // SourceKind::AllApps — that captures EVERY app + system sounds, a
                            // surprising privacy/behavior downgrade for someone who deliberately
                            // picked one app (this is the reported "other windows + system noise
                            // leak through when I pick a per-window source" bug). Instead keep the
                            // radio on "Just one window/app" with no pid; apply() then shows
                            // "Pick an app first" so broadening capture is always a conscious act,
                            // never automatic.
                            self.selected_pid = None;
                            self.selected_name.clear();
                            // NB: self.source intentionally left as SourceKind::App (no silent switch).
                        }
                    }
                }
                // Reconcile the VIDEO selection — match the exact window (hwnd) first so a
                // multi-window app keeps the one picked; fall back to pid. The .map() ends the
                // borrow of self.apps before we mutate self.
                //
                // Windows-only: per-window video capture is WGC, and Linux `AudioApp`s always
                // carry `hwnd: None`, so there is nothing here for Linux to reconcile.
                #[cfg(target_os = "windows")]
                if let Some(pid) = self.video_pid {
                    let h = self.video_hwnd;
                    let found = h
                        .and_then(|hh| self.apps.iter().find(|a| a.hwnd == Some(hh)))
                        .or_else(|| self.apps.iter().find(|a| a.pid == pid && a.hwnd.is_some()))
                        .map(|a| (a.pid, a.name.clone(), a.hwnd));
                    match found {
                        Some((p, name, hw)) => {
                            self.video_pid = Some(p);
                            self.video_name = name;
                            self.video_hwnd = hw;
                        }
                        None => {
                            self.video_pid = None;
                            self.video_name.clear();
                            self.video_hwnd = None;
                            if self.video_kind == VideoSourceKind::Window {
                                self.video_kind = VideoSourceKind::Screen; // window gone → whole screen
                            }
                        }
                    }
                }
            }
            Err(mpsc::TryRecvError::Empty) => {} // still enumerating
            Err(mpsc::TryRecvError::Disconnected) => {
                self.apps_rx = None;
                self.refreshing = false; // worker died without sending; let the user retry
            }
        }
    }

    /// Collect a finished routing pass and start the next one when it is due. Called once per frame;
    /// never blocks. Only runs while the output-device source is the one selected — with any other
    /// source there is no device to send anything to.
    #[cfg(target_os = "linux")]
    fn poll_routes(&mut self, ctx: &egui::Context) {
        let r = &mut self.routing;
        if let Some(rx) = &r.rx {
            match rx.try_recv() {
                Ok(Ok(snap)) => {
                    // A box toggled while this pass was in flight makes its answer stale; showing it
                    // would bounce the box back for a moment. The pass that follows has the truth.
                    if !r.dirty {
                        r.snapshot = Some(snap);
                    }
                    r.error.clear();
                    r.rx = None;
                }
                Ok(Err(e)) => {
                    r.error = e;
                    r.rx = None;
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => r.rx = None,
            }
        }
        if self.source != SourceKind::VirtualSink {
            return;
        }
        let due = r.dirty || r.last_sync.map_or(true, |t| t.elapsed() >= ROUTE_SYNC_EVERY);
        if r.rx.is_none() && due {
            let wanted: Vec<String> = r.wanted.iter().cloned().collect();
            let release = std::mem::take(&mut r.release);
            let (tx, rx) = mpsc::channel();
            let spawned = std::thread::Builder::new().name("app-routing".into()).spawn(move || {
                let _ = tx.send(pulse::sync_routes(&wanted, &release).map_err(|e| format!("{e:#}")));
            });
            if spawned.is_ok() {
                r.rx = Some(rx);
            }
            r.dirty = false;
            r.last_sync = Some(std::time::Instant::now());
        }
        ctx.request_repaint_after(if r.rx.is_some() {
            Duration::from_millis(100) // pick the answer up promptly
        } else {
            ROUTE_SYNC_EVERY
        });
    }

    /// The operator ticked or unticked an app in the output-device list.
    #[cfg(target_os = "linux")]
    fn set_app_routed(&mut self, key: &str, on: bool) {
        let r = &mut self.routing;
        r.release.retain(|k| k != key);
        if on {
            r.wanted.insert(key.to_string());
        } else {
            r.wanted.remove(key);
            r.release.push(key.to_string());
        }
        // Show the choice at once; the pass this triggers makes it true within a moment.
        if let Some(snap) = r.snapshot.as_mut() {
            for a in snap.apps.iter_mut().filter(|a| a.key == key) {
                a.on_device = on;
            }
        }
        r.dirty = true;
        if let Err(e) = crate::settings::save_routed_apps(&r.wanted) {
            tracing::debug!("could not save the routed apps: {e}");
        }
    }

    /// Build options from the current UI and hand them to the control thread.
    fn apply(&mut self) {
        let capture_source = match self.source {
            SourceKind::AllApps => CaptureSource::AllExceptSelf,
            SourceKind::System => CaptureSource::System,
            SourceKind::VirtualSink => CaptureSource::VirtualSink,
            SourceKind::WebUplink => CaptureSource::WebUplink,
            SourceKind::App => match self.selected_pid {
                Some(pid) => CaptureSource::App { pid },
                None => return, // apply_blocker() already told the operator why
            },
        };
        if self.apply_blocker().is_some() {
            return;
        }
        let video = if self.video_kind != VideoSourceKind::Off {
            let res = Resolution::parse(RES_LABELS[self.res_idx].1).unwrap_or(Resolution::P1080);
            Some(VideoConfig {
                resolution: res,
                fps: if self.fps60 { Fps::F60 } else { Fps::F30 },
                quality_pct: self.video_quality_pct,
            })
        } else {
            None
        };
        // The one place the encoder choice leaves the GUI, so clamp here too: a build without the
        // `vp9` feature must never request VP9, whatever the UI state says. (media::start's
        // resolve_encoder would also catch it, but then the GUI and the stream would disagree.)
        let mut encoder =
            EncoderBackend::parse(ENC_LABELS[self.enc_idx].1).unwrap_or(EncoderBackend::Av1);
        if !cfg!(feature = "vp9") {
            encoder = EncoderBackend::Av1;
        }

        #[cfg(target_os = "windows")]
        let video_target = match (self.video_kind, self.video_hwnd) {
            (VideoSourceKind::Window, Some(hwnd)) => VideoTarget::Window { hwnd },
            _ => VideoTarget::PrimaryMonitor,
        };
        #[cfg(not(target_os = "windows"))]
        let video_target = VideoTarget::PrimaryMonitor;

        let opts = MediaOptions {
            name: self.server_name.clone(),
            codec: self.codec,
            bitrate: self.bitrate,
            lead_ms: config::DEFAULT_LEAD_MS,
            buffer_ms: config::clamp_buffer_ms(self.buffer_ms as i64),
            av_offset_ns: self.av_offset_ns.clone(),
            capture_source,
            silent_local: self.silent_local,
            video,
            video_target,
            encoder,
            encode_device: self.encode_device,
        };
        self.starting.store(true, Ordering::Relaxed);
        self.status.lock().unwrap().waiting_on_portal = opens_portal(opts.video.is_some(), opts.capture_source);
        let _ = self.cmd_tx.send(opts);
        // The new baseline for "changes pending" — and the old one, in case this switch fails.
        self.applied_before = self.applied.take();
        self.applied = Some(self.current_config());
        self.applied_audio_name = self.selected_name.clone();
        self.dismissed_error = None;
        self.save_stream_prefs();
    }

    /// Remember what was just applied, so the next launch starts the same way without flags.
    ///
    /// A single-app source is saved as "all apps": its PID means nothing after a restart, and the
    /// server has to start on something. Screen sharing is never saved as ON (see `run`).
    fn save_stream_prefs(&self) {
        let source = match self.source {
            SourceKind::AllApps | SourceKind::App => "allapps",
            SourceKind::System => "system",
            SourceKind::VirtualSink => "sink",
            SourceKind::WebUplink => "web",
        };
        let saved = crate::settings::set_many(&[
            ("source", source.to_string()),
            ("silent", (self.silent_local as u8).to_string()),
            ("buffer_ms", config::clamp_buffer_ms(self.buffer_ms as i64).to_string()),
            ("codec", self.codec.as_str().to_string()),
            ("bitrate", self.bitrate.to_string()),
            ("video_res", RES_LABELS[self.res_idx].1.to_string()),
            ("video_fps60", (self.fps60 as u8).to_string()),
            ("video_quality", self.video_quality_pct.to_string()),
            ("video_encoder", ENC_LABELS[self.enc_idx].1.to_string()),
        ]);
        if let Err(e) = saved {
            tracing::debug!("could not save the stream settings: {e}");
        }
    }

    /// Apply the A/V offset LIVE: the slider's ms and the nanoseconds the audio producer reads, kept
    /// in step. Cheap enough to call on every frame of a drag — it is two stores.
    fn apply_av_offset(&mut self, ms: i32) {
        self.av_offset_ms = ms.clamp(-500, 500);
        self.av_offset_ns.store(
            self.av_offset_ms as i64 * 1_000_000,
            std::sync::atomic::Ordering::Relaxed,
        );
    }

    /// Persist the current offset. Split from [`Self::apply_av_offset`] because saving is a
    /// read-modify-write of the settings file plus a rename, and doing that per mouse-move of a drag
    /// is pure churn for a value only the final position of which matters.
    fn save_av_offset(&self) {
        if let Err(e) = crate::settings::save_av_offset_ms(self.av_offset_ms) {
            tracing::debug!("could not save the A/V offset: {e}");
        }
    }

}

#[cfg(test)]
mod tests {
    use super::StreamStatus;
    use crate::capture::DUMMY_TAG;

    fn serving(device: &str) -> StreamStatus {
        StreamStatus { serving: Some(device.to_string()), ..Default::default() }
    }

    #[test]
    fn a_dummy_device_is_recognised_and_a_real_one_is_not() {
        assert!(serving(&format!("auto_null.monitor  {DUMMY_TAG}")).is_dummy());
        assert!(!serving("alsa_output.pci-0000_00_1f.3.analog-stereo.monitor").is_dummy());
        assert!(!StreamStatus::default().is_dummy(), "nothing serving yet is not a dummy");
    }

    #[test]
    fn an_error_does_not_hide_what_is_being_served() {
        // The bug the old single status string had: a failed switch or a validation hint overwrote
        // the serving line, and the silence warning vanished while it was still true.
        let mut st = serving(&format!("auto_null.monitor  {DUMMY_TAG}"));
        st.error = Some("Couldn't switch: no such device".into());
        st.failures += 1;
        assert!(st.is_dummy());
        assert_eq!(st.device_label(), "auto_null.monitor");
    }

    #[test]
    fn the_tag_the_banner_matches_is_the_tag_the_capture_layer_emits() {
        // Guards the cross-platform seam: pulse.rs builds the device string, the window greps it.
        // If someone edits one literal, `DUMMY_TAG` is the single definition that keeps them agreeing.
        assert!(DUMMY_TAG.contains("DUMMY OUTPUT"));
    }
}
