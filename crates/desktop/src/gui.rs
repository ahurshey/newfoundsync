// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Alex Hurshman and the Newfoundsync contributors.

//! Native server GUI (eframe). Pick the audio source to share — all apps, one
//! specific app, or the full system output — optionally share the screen, and see
//! the URL clients open plus how many are connected.
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
use std::time::Duration;

use anyhow::{anyhow, Result};
use eframe::egui;
use tokio::sync::watch;

use newfoundsync_core::codec::CodecKind;
use newfoundsync_core::video::{EncoderBackend, Fps, Resolution, VideoConfig};
use newfoundsync_core::{config, discovery};

use crate::media::{self, CaptureSource, Media, MediaOptions, VideoTarget};
use crate::webserver::{self, ClientRegistry, StreamState};

#[cfg(target_os = "windows")]
use crate::capture::sessions::{self, AudioApp};

#[derive(PartialEq, Clone, Copy)]
enum SourceKind {
    AllApps,
    App,
    System,
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
const ENC_LABELS: [(&str, &str); 3] = [
    ("Auto (GPU HEVC)", "auto"),
    ("GPU only (HEVC)", "hardware"),
    ("AV1 (royalty-free)", "av1"),
];

/// Initial server config (from CLI flags) used to seed the GUI + first stream.
pub struct InitialConfig {
    pub capture_source: CaptureSource,
    pub video: Option<VideoConfig>,
    pub encoder: EncoderBackend,
    pub buffer_ms: i64,
    pub codec: CodecKind,
    pub bitrate: i32,
}

impl InitialConfig {
    fn to_options(&self, name: &str) -> MediaOptions {
        MediaOptions {
            name: name.to_string(),
            codec: self.codec,
            bitrate: self.bitrate,
            lead_ms: config::DEFAULT_LEAD_MS,
            buffer_ms: self.buffer_ms,
            capture_source: self.capture_source,
            video: self.video,
            video_target: VideoTarget::PrimaryMonitor, // CLI/initial stream starts on the monitor
            encoder: self.encoder,
        }
    }
}

/// Launch the GUI. Blocks until the window is closed.
pub fn run(port: u16, server_name: String, init: InitialConfig) -> Result<()> {
    // The single active web-caster's conn_id (web-uplink source), shared with the web server.
    let cast: webserver::CastState = Arc::new(Mutex::new(None));
    let clients = Arc::new(AtomicUsize::new(0));
    let clients_reg: ClientRegistry = Arc::new(Mutex::new(HashMap::new()));
    let status = Arc::new(Mutex::new(String::from("Starting…")));
    let starting = Arc::new(AtomicBool::new(true));
    let (cmd_tx, cmd_rx) = mpsc::channel::<MediaOptions>();
    let (ready_tx, ready_rx) = mpsc::channel::<Option<watch::Receiver<Arc<StreamState>>>>();

    let initial_opts = init.to_options(&server_name);

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
                    *status.lock().unwrap() = serving_text(&m);
                    starting.store(false, Ordering::Relaxed);
                    let _ = ready_tx.send(Some(rx));
                    control_loop(m, tx, cmd_rx, status, starting);
                }
                Err(e) => {
                    *status.lock().unwrap() = format!("Couldn't start: {e:#}");
                    starting.store(false, Ordering::Relaxed);
                    let _ = ready_tx.send(None);
                }
            })?;
    }

    let state_rx = match ready_rx.recv() {
        Ok(Some(rx)) => rx,
        _ => return Err(anyhow!("could not start audio capture — see console for details")),
    };

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    {
        let clients = clients.clone();
        let clients_reg = clients_reg.clone();
        let cast = cast.clone();
        std::thread::Builder::new()
            .name("web-server".into())
            .spawn(move || match tokio::runtime::Runtime::new() {
                Ok(rt) => {
                    if let Err(e) =
                        rt.block_on(webserver::run(state_rx, clients, clients_reg, cast, addr, true))
                    {
                        tracing::error!("web server exited: {e:#}");
                    }
                }
                Err(e) => tracing::error!("tokio runtime: {e}"),
            })?;
    }

    let lan = discovery::primary_lan_ipv4()
        .map(|ip| ip.to_string())
        .unwrap_or_else(|| "<this-pc>".into());
    let url = format!("https://{lan}:{port}");

    // Seed UI widgets from the initial config so flags are reflected.
    let (source, selected_pid) = match init.capture_source {
        CaptureSource::AllExceptSelf => (SourceKind::AllApps, None),
        CaptureSource::System => (SourceKind::System, None),
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
            (kind, res_to_idx(v.resolution), v.fps == Fps::F60)
        }
        None => (VideoSourceKind::Off, 1, false),
    };
    let enc_idx = match init.encoder {
        EncoderBackend::Auto => 0,
        EncoderBackend::Hardware => 1,
        EncoderBackend::Av1 => 2,
    };
    // The Codec picker exposes only HEVC/GPU (0) and AV1 (2); "GPU only" (1) has no GUI option
    // and `is_av1 == (enc_idx == 2)` would render it as HEVC. Fold a `--encoder hardware` launch
    // into Auto (0) so the GUI state and the encoder never disagree. (Auto and Hardware both resolve
    // to GPU HEVC, so this changes no encoding behavior — only the picker's consistency.)
    let enc_idx = if enc_idx == 1 { 0 } else { enc_idx };

    let mut app = ServerApp {
        server_name,
        url,
        clients,
        clients_reg,
        cast: cast.clone(),
        master_vol: 1.0,
        client_vols: HashMap::new(),
        client_trims: HashMap::new(),
        client_names: HashMap::new(),
        editing_client: None,
        edit_name_buf: String::new(),
        client_muted: HashMap::new(),
        applied: None,
        qr_tex: None,
        stream_live: false,
        calibrating: false,
        status,
        starting,
        cmd_tx,
        codec: init.codec,
        bitrate: init.bitrate,
        source,
        #[cfg(target_os = "windows")]
        apps: Vec::new(),
        #[cfg(target_os = "windows")]
        apps_rx: None,
        #[cfg(target_os = "windows")]
        refreshing: false,
        selected_pid,
        selected_name: String::new(),
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
        video_quality_pct: init.video.map(|v| v.quality_pct).unwrap_or(100),
        buffer_ms: init.buffer_ms as i32,
        port,
        port_edit: port,
        port_msg: String::new(),
        did_initial_zoom: false,
    };
    #[cfg(target_os = "windows")]
    app.refresh_apps();

    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([1024.0, 576.0]) // 16:9 — wide two-column control panel
        .with_min_inner_size([880.0, 495.0])
        .with_title("Newfoundsync server");
    // Brand the window/taskbar with the Newfoundland badge.
    if let Ok(icon) = eframe::icon_data::from_png_bytes(include_bytes!("../../../branding/icon-256.png")) {
        viewport = viewport.with_icon(std::sync::Arc::new(icon));
    }
    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    eframe::run_native(
        "Newfoundsync",
        options,
        Box::new(|cc| {
            setup_style(&cc.egui_ctx);
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

/// The egui zoom factor that the UI is designed around — shown to the user as "100%". The window
/// opens here, and the −/+ control and the percentage readout are all relative to this baseline,
/// so 100% means "the size this app is tuned for" rather than the raw 1.0 device scale.
const UI_ZOOM_BASE: f32 = 0.6;

/// Apply the theme + desktop-tuned spacing/fonts/zoom once at startup.
fn setup_style(ctx: &egui::Context) {
    use egui::{FontId, TextStyle};

    let mut s = (*ctx.style()).clone();
    s.visuals = theme_visuals();
    // Denser panels (tighter than the web-derived defaults — this is a desktop window).
    s.spacing.item_spacing = egui::vec2(8.0, 7.0);
    s.spacing.button_padding = egui::vec2(12.0, 6.0);
    s.spacing.interact_size.y = 28.0;
    s.spacing.indent = 16.0;
    s.spacing.slider_rail_height = 6.0;
    // Slightly larger, crisper text.
    s.text_styles.insert(TextStyle::Heading, FontId::proportional(24.0));
    s.text_styles.insert(TextStyle::Body, FontId::proportional(15.5));
    s.text_styles.insert(TextStyle::Button, FontId::proportional(15.5));
    s.text_styles.insert(TextStyle::Monospace, FontId::monospace(15.0));
    s.text_styles.insert(TextStyle::Small, FontId::proportional(12.0));
    ctx.set_style(s);

    // Open at the design baseline (shown as 100%). The −/+ buttons fine-tune it live.
    ctx.set_zoom_factor(UI_ZOOM_BASE);
    // Disable egui's built-in Ctrl+/−/0 zoom: it steps/resets the RAW factor (Ctrl 0 → 1.0), which
    // would disagree with our rebased readout. The −/+ buttons (relative to UI_ZOOM_BASE) own zoom.
    ctx.options_mut(|o| o.zoom_with_keyboard = false);
}

/// An eyebrow section label (uppercase, dim, small).
fn eyebrow(ui: &mut egui::Ui, text: &str) {
    ui.label(egui::RichText::new(text).strong().size(13.0).color(c_dim()));
    ui.add_space(6.0);
}

/// A sliding light/dark toggle switch: a pill track with an animated knob that slides right
/// in light mode. Clicking flips `LIGHT_THEME` and re-applies the visuals live.
fn theme_toggle(ui: &mut egui::Ui) {
    let light = LIGHT_THEME.load(Ordering::Relaxed);
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(40.0, 20.0), egui::Sense::click());
    if resp.clicked() {
        LIGHT_THEME.store(!light, Ordering::Relaxed);
        ui.ctx().set_visuals(theme_visuals());
        ui.ctx().request_repaint();
    }
    if ui.is_rect_visible(rect) {
        let how_on = ui.ctx().animate_bool(resp.id, light); // 0 = dark (left), 1 = light (right)
        let radius = 0.5 * rect.height();
        ui.painter()
            .rect_filled(rect, radius, if light { c_accent() } else { c_surface_alt() });
        let cx = egui::lerp((rect.left() + radius)..=(rect.right() - radius), how_on);
        ui.painter()
            .circle_filled(egui::pos2(cx, rect.center().y), radius - 2.5, egui::Color32::WHITE);
    }
    resp.on_hover_text("Toggle light / dark theme");
}

/// A bordered "card" container for grouping a section.
fn card<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) {
    egui::Frame::group(ui.style())
        .fill(c_surface())
        .stroke(egui::Stroke::new(1.0, c_border()))
        .corner_radius(egui::CornerRadius::from(10))
        .inner_margin(11.0)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui);
        });
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

fn serving_text(m: &Media) -> String {
    format!(
        "Serving: {}{}",
        m.capture_device,
        if m.config.video { "  + screen" } else { "" }
    )
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
    status: Arc<Mutex<String>>,
    starting: Arc<AtomicBool>,
) {
    while let Ok(opts) = cmd_rx.recv() {
        starting.store(true, Ordering::Relaxed);
        *status.lock().unwrap() = "Starting…".into();
        match media::start(opts) {
            Ok(m) => {
                let _ = tx.send(Arc::new(StreamState::from_media(&m)));
                *status.lock().unwrap() = serving_text(&m);
                current = m; // drop the previous stream (threads joined) now that the new one is live
            }
            Err(e) => {
                // Keep serving the previous source; just report the failure.
                *status.lock().unwrap() = format!("Couldn't switch: {e:#}");
            }
        }
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
    video_kind: VideoSourceKind,
    #[cfg(target_os = "windows")]
    video_hwnd: Option<isize>,
    res_idx: usize,
    fps60: bool,
    enc_idx: usize,
    video_quality_pct: u16,
    buffer_ms: i32,
    codec: CodecKind,
    bitrate: i32,
}

struct ServerApp {
    server_name: String,
    url: String,
    clients: Arc<AtomicUsize>,
    /// Live per-client registry shared with the web server — render the list and
    /// push per-client volume through each entry's `ctrl_tx`.
    clients_reg: ClientRegistry,
    /// The single active web-caster's conn_id (shared with the web server). The GUI reads it to
    /// surface that caster's "Stop cast" control and writes `None` to free the slot when stopped.
    cast: webserver::CastState,
    /// Server master volume (0..=1): scales every client's effective remote volume.
    master_vol: f32,
    /// Per-client (pre-master) volume, keyed by the client's *stable* id so it
    /// survives reconnects. Absent ⇒ 1.0 (full) for a client we haven't touched.
    client_vols: HashMap<String, f32>,
    /// Per-client server sync offset in ms, keyed by stable id (survives reconnects).
    /// Absent ⇒ 0. Adds to the device's own trim — positive = play later.
    client_trims: HashMap<String, i32>,
    /// Server-assigned display name override, keyed by stable id. Wins over the
    /// client's self-reported HELLO name; survives that device reconnecting.
    client_names: HashMap<String, String>,
    /// Stable id of the client whose name is being edited inline (double-click), if any.
    editing_client: Option<String>,
    /// Scratch buffer backing the inline rename text field.
    edit_name_buf: String,
    /// Server-side mute, keyed by stable id. Muted ⇒ effective remote volume 0 (the
    /// device's own slider value is preserved and restored on un-mute). Survives reconnect.
    client_muted: HashMap<String, bool>,
    /// The stream config last handed to the control thread, so the Apply button can show a
    /// "changes pending" state (source/video/buffer are NOT live; they take effect on Apply).
    applied: Option<AppliedConfig>,
    /// Cached QR texture of the connect URL (built once; the URL is fixed for the session).
    qr_tex: Option<egui::TextureHandle>,
    /// True once a stream has successfully come up. The live-status pill keys off this, not the
    /// transient status string (which a failed switch / "pick first" validation overwrites while
    /// the previous stream keeps serving).
    stream_live: bool,
    /// True while a server-orchestrated "Calibrate all" run is active (Phase B).
    calibrating: bool,
    status: Arc<Mutex<String>>,
    starting: Arc<AtomicBool>,
    cmd_tx: mpsc::Sender<MediaOptions>,
    codec: CodecKind,
    bitrate: i32,
    source: SourceKind,
    #[cfg(target_os = "windows")]
    apps: Vec<AudioApp>,
    // Async source enumeration: refresh_apps() spawns a worker and stashes the receiver here;
    // poll_refresh() (called each frame) applies the result. Never blocks the GUI thread.
    #[cfg(target_os = "windows")]
    apps_rx: Option<mpsc::Receiver<Vec<AudioApp>>>,
    #[cfg(target_os = "windows")]
    refreshing: bool,
    selected_pid: Option<u32>,
    selected_name: String,
    // VIDEO SOURCE: off / whole screen / a specific window — its own picker, like audio.
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
    video_quality_pct: u16, // HEVC quality as % of baseline bitrate (slider; 100 = default)
    buffer_ms: i32,
    /// The HTTP port the server is CURRENTLY bound to (fixed for this run; used in the URL/QR).
    port: u16,
    /// Edit buffer for the GUI port field. Saving it persists for the NEXT launch (a live rebind
    /// isn't worth juggling the TLS socket for a rare change), so a "restart to apply" note shows.
    port_edit: u16,
    /// Transient status under the port field ("Saved — restart to apply", or an error).
    port_msg: String,
    did_initial_zoom: bool, // applied the high-DPI default UI scale once (first frame)
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
                let eff = if muted {
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

    /// Plain-words description of the audio quality currently being streamed — codec,
    /// sample rate, channels, bitrate, and a one-word verdict — for the operator.
    fn audio_quality_text(&self) -> String {
        match self.codec {
            CodecKind::Pcm => {
                "Lossless PCM · 48 kHz · 16-bit stereo — bit-perfect, uncompressed".to_string()
            }
            CodecKind::Opus => {
                let kbps = (self.bitrate / 1000).max(1);
                let verdict = if self.bitrate >= 256_000 {
                    "transparent (indistinguishable from the original)"
                } else if self.bitrate >= 160_000 {
                    "excellent, near-CD"
                } else if self.bitrate >= 96_000 {
                    "very good"
                } else if self.bitrate >= 64_000 {
                    "good"
                } else {
                    "voice-grade"
                };
                format!("Opus · 48 kHz stereo · {kbps} kbps — {verdict}")
            }
        }
    }

    /// Snapshot of the apply-relevant settings as they currently sit in the UI. Compared
    /// against `self.applied` to decide whether the Apply button shows "changes pending".
    fn current_config(&self) -> AppliedConfig {
        AppliedConfig {
            source: self.source,
            selected_pid: self.selected_pid,
            video_kind: self.video_kind,
            #[cfg(target_os = "windows")]
            video_hwnd: self.video_hwnd,
            res_idx: self.res_idx,
            fps60: self.fps60,
            enc_idx: self.enc_idx,
            video_quality_pct: self.video_quality_pct,
            // Record the value apply() actually sends (clamped), so the dirty comparison and
            // the applied baseline agree even if buffer_ms was seeded out of range from the CLI.
            buffer_ms: self.buffer_ms.clamp(200, config::MAX_BUFFER_MS as i32),
            codec: self.codec,
            bitrate: self.bitrate,
        }
    }

    /// Lazily build (and cache) a black-on-white QR texture of the connect URL so phones
    /// can scan instead of hand-typing `https://<ip>:<port>`. The URL is fixed for the
    /// session, so this runs once. Returns None if QR generation fails (URL too long, etc.).
    fn qr_texture(&mut self, ctx: &egui::Context) -> Option<egui::TextureHandle> {
        if self.qr_tex.is_none() {
            let code = qrcode::QrCode::new(self.url.as_bytes()).ok()?;
            let w = code.width();
            let quiet = 4usize; // mandatory light border so scanners lock on
            let scale = 4usize; // px per module
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
            self.qr_tex = Some(ctx.load_texture("connect-qr", img, egui::TextureOptions::NEAREST));
        }
        self.qr_tex.clone()
    }

    /// Kick off a source enumeration on a worker thread. Does NOT block the GUI thread —
    /// `list_sources` internally spawns an MTA thread and joins it, so joining it on the
    /// GUI/STA thread would freeze the window (and could deadlock against the worker's COM
    /// teardown). The result is applied later by `poll_refresh`.
    #[cfg(target_os = "windows")]
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
    #[cfg(target_os = "windows")]
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

    /// Build options from the current UI and hand them to the control thread.
    fn apply(&mut self) {
        let capture_source = match self.source {
            SourceKind::AllApps => CaptureSource::AllExceptSelf,
            SourceKind::System => CaptureSource::System,
            SourceKind::WebUplink => CaptureSource::WebUplink,
            SourceKind::App => match self.selected_pid {
                Some(pid) => CaptureSource::App { pid },
                None => {
                    *self.status.lock().unwrap() = "Pick an app first, then Apply.".into();
                    return;
                }
            },
        };
        // Video: validate a window pick the same way audio validates an app pick.
        #[cfg(target_os = "windows")]
        if self.video_kind == VideoSourceKind::Window && self.video_hwnd.is_none() {
            *self.status.lock().unwrap() = "Pick a window for video first, then Apply.".into();
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
        let encoder = EncoderBackend::parse(ENC_LABELS[self.enc_idx].1).unwrap_or(EncoderBackend::Auto);

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
            buffer_ms: self.buffer_ms.clamp(200, config::MAX_BUFFER_MS as i32) as i64,
            capture_source,
            video,
            video_target,
            encoder,
        };
        self.starting.store(true, Ordering::Relaxed);
        *self.status.lock().unwrap() = "Starting…".into();
        let _ = self.cmd_tx.send(opts);
        self.applied = Some(self.current_config()); // baseline for the "changes pending" state
    }

    // ===== 16:9 layout sections (composed by `ui` below) ============================

    /// Full-width "connect" strip under the header: the URL plate (left) + the QR code,
    /// scan hint and one-time-cert disclosure (right). Read-only.
    fn ui_connect_strip(&mut self, ui: &mut egui::Ui, qr: &Option<egui::TextureHandle>) {
        ui.horizontal_top(|ui| {
            ui.vertical(|ui| {
                ui.label(
                    egui::RichText::new("Open this on any phone or PC on the same Wi-Fi:").color(c_dim()),
                );
                ui.add_space(4.0);
                egui::Frame::group(ui.style())
                    .fill(c_surface())
                    .stroke(egui::Stroke::new(1.0, c_border()))
                    .corner_radius(egui::CornerRadius::from(10))
                    .inner_margin(egui::Margin::from(9.0))
                    .show(ui, |ui| {
                        ui.set_width(520.0); // fixed so the QR group fits to the right
                        ui.horizontal(|ui| {
                            ui.label(
                                egui::RichText::new(&self.url)
                                    .monospace()
                                    .size(19.0)
                                    .strong()
                                    .color(c_accent_hi()),
                            );
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if ui.button("Copy").clicked() {
                                    ui.ctx().copy_text(self.url.clone());
                                }
                                if ui
                                    .button("Open ↗")
                                    .on_hover_text("Open this address in your default browser")
                                    .clicked()
                                {
                                    open_url(&self.url);
                                }
                            });
                        });
                    });
                // Port: editable + persisted. The server binds the port once at startup, so a change
                // applies on the next launch — we save it and show a "restart to apply" note here.
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("Port").size(12.5).color(c_dim()));
                    ui.add(egui::DragValue::new(&mut self.port_edit).range(1024.0..=65535.0).speed(1.0))
                        .on_hover_text("HTTP port the web client is served on (1024–65535). Default 47000.");
                    if ui.button("Save").clicked() {
                        self.port_msg = if self.port_edit == self.port {
                            format!("Already serving on port {}.", self.port)
                        } else {
                            match crate::settings::save_port(self.port_edit) {
                                Ok(()) => format!("Saved — restart Newfoundsync to serve on port {}.", self.port_edit),
                                Err(e) => format!("Couldn't save port: {e}"),
                            }
                        };
                    }
                    if !self.port_msg.is_empty() {
                        ui.label(egui::RichText::new(&self.port_msg).size(11.5).color(c_accent_hi()));
                    }
                });
            });
            ui.add_space(16.0);
            ui.vertical(|ui| {
                if let Some(tex) = qr {
                    ui.image(egui::load::SizedTexture::new(tex.id(), tex.size_vec2()));
                }
                // Cert help folded into a hover (was a standalone "First time on a device?" line) to
                // reclaim vertical space — hover "Scan to connect" for the one-time-cert explanation.
                ui.label(egui::RichText::new("📷  Scan to connect").strong().color(c_text()))
                    .on_hover_text(
                        "First time on a device? Accept the one-time security warning \
                         (Advanced -> proceed) — it's a self-signed certificate, needed so the \
                         browser allows playback.",
                    );
            });
        });
    }

    /// LEFT (hero) column: the connected-clients mixer. Builds the registry snapshot, renders
    /// master + per-client rows (in their own scroll area), then pushes state + runs the
    /// Calibrate-all flags — all the live, no-Apply controls.
    fn ui_clients(&mut self, ui: &mut egui::Ui, clients_n: usize) {
        // Snapshot the live registry (don't hold the lock across egui closures).
        let mut snapshot: Vec<(u64, String, String, bool, String, Option<i32>)> = Vec::new();
        if let Ok(reg) = self.clients_reg.lock() {
            for e in reg.values() {
                snapshot.push((e.conn_id, e.stable_id.clone(), e.name.clone(), e.identified, e.calib_status.clone(), e.reported_trim_ms));
            }
        }
        snapshot.sort_by_key(|c| c.0); // stable order ≈ connect order
        let n_identified = snapshot.iter().filter(|c| c.3).count();
        let calibrating = self.calibrating;
        let mut do_calibrate = false; // set inside the card closure, acted on after it
        let mut do_stop_calib = false;
        let mut do_stop_cast: Option<u64> = None; // caster's conn_id when the operator kicks it
        // The active web-caster (if any) — only its row shows the operator "Stop cast" control.
        let active_caster: Option<u64> = self.cast.lock().ok().and_then(|s| *s);

        // If the client being renamed vanished, don't strand the half-typed buffer: commit it
        // (keyed by stable id) and clear the edit state.
        if let Some(ed) = self.editing_client.clone() {
            let present = snapshot.iter().any(|(_, sid, _, ident, _, _)| *ident && sid == &ed);
            if !present {
                let nm = self.edit_name_buf.trim().to_string();
                if !nm.is_empty() {
                    self.client_names.insert(ed, nm);
                }
                self.editing_client = None;
                self.edit_name_buf.clear();
            }
        }

        card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("CONNECTED CLIENTS").strong().size(13.0).color(c_dim()))
                    .on_hover_text("Volume & sync are live — they apply instantly, no Apply needed.");
                ui.label(egui::RichText::new(format!("({clients_n})")).strong().color(c_accent()));
            });
            ui.add_space(6.0);
            // Master volume — scales every client's remote (server-controlled) volume.
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("Master").color(c_text2()));
                ui.add(egui::Slider::new(&mut self.master_vol, 0.0..=1.0).show_value(false));
                ui.label(
                    egui::RichText::new(format!("{:>3}%", (self.master_vol * 100.0).round() as i32))
                        .color(c_dim()),
                );
            });
            if snapshot.is_empty() {
                ui.add_space(2.0);
                ui.label(
                    egui::RichText::new(
                        "No clients yet — open the URL above on a phone, tablet, or laptop.",
                    )
                    .size(11.5)
                    .color(c_dim()),
                );
            }
            // ---- Calibrate all (Phase B): align every device at once over the mic ----
            if n_identified >= 2 || calibrating {
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    if calibrating {
                        if ui.button("⏹ Stop calibration").clicked() {
                            do_stop_calib = true;
                        }
                        ui.label(
                            egui::RichText::new("Aligning all devices…").size(11.5).color(c_accent()),
                        );
                    } else if ui
                        .button("Calibrate all")
                        .on_hover_text(
                            "Earliest-connected device plays a sync code; the rest listen on \
                             their mics and align at once. Devices must be in the same room \
                             with working mics. (Uses the coded signal.)",
                        )
                        .clicked()
                    {
                        do_calibrate = true;
                    }
                });
            }
            ui.add_space(4.0);
            // Per-client rows scroll independently — the one routinely-growing region. Distinct
            // id_salt from the right column's scroll area so their scroll states don't collide.
            egui::ScrollArea::vertical()
                .id_salt("clients")
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    for (conn_id, stable_id, name, identified, calib_status, reported_trim) in &snapshot {
                        if !*identified {
                            ui.label(egui::RichText::new("connecting…").italics().color(c_dim()));
                            ui.add_space(4.0);
                            continue;
                        }
                        // Salt each row by stable id so a mid-list disconnect can't hand an
                        // in-flight slider drag / rename focus to a different client.
                        ui.push_id(stable_id, |ui| {
                            ui.separator();
                            let default_name = if name.is_empty() {
                                format!("Client {conn_id}")
                            } else {
                                name.clone()
                            };
                            let display =
                                self.client_names.get(stable_id).cloned().unwrap_or(default_name);
                            if self.editing_client.as_deref() == Some(stable_id.as_str()) {
                                let resp = ui.add(
                                    egui::TextEdit::singleline(&mut self.edit_name_buf)
                                        .desired_width(240.0)
                                        .hint_text("name this device"),
                                );
                                if !resp.has_focus() && !resp.lost_focus() {
                                    resp.request_focus();
                                }
                                if resp.lost_focus() {
                                    let nm = self.edit_name_buf.trim().to_string();
                                    if nm.is_empty() {
                                        self.client_names.remove(stable_id);
                                    } else {
                                        self.client_names.insert(stable_id.clone(), nm);
                                    }
                                    self.editing_client = None;
                                }
                            } else {
                                let resp = ui
                                    .add(
                                        egui::Label::new(
                                            egui::RichText::new(&display).strong().color(c_text()),
                                        )
                                        .sense(egui::Sense::click()),
                                    )
                                    .on_hover_text("double-click to rename");
                                if resp.double_clicked() {
                                    self.edit_name_buf = display.clone();
                                    self.editing_client = Some(stable_id.clone());
                                }
                            }
                            let muted = self.client_muted.get(stable_id).copied().unwrap_or(false);
                            ui.horizontal(|ui| {
                                ui.spacing_mut().slider_width = 88.0; // fits two sliders + labels even at the 880px min width
                                if ui
                                    .selectable_label(muted, if muted { "🔇" } else { "🔊" })
                                    .on_hover_text("Mute / unmute this device")
                                    .clicked()
                                {
                                    self.client_muted.insert(stable_id.clone(), !muted);
                                }
                                ui.label(egui::RichText::new("Vol").size(12.0).color(c_dim()));
                                let v = self.client_vols.entry(stable_id.clone()).or_insert(1.0);
                                ui.add(egui::Slider::new(v, 0.0..=1.0).show_value(false));
                                let pct = (*v * 100.0).round() as i32;
                                ui.label(
                                    egui::RichText::new(if muted {
                                        " off".to_string()
                                    } else {
                                        format!("{pct:>3}%")
                                    })
                                    .monospace()
                                    .color(c_dim()),
                                );
                                ui.add_space(8.0);
                                ui.label(egui::RichText::new("Sync").size(12.0).color(c_dim()));
                                let t = self.client_trims.entry(stable_id.clone()).or_insert(0);
                                ui.add(egui::Slider::new(t, -500..=500).show_value(false));
                                let ms = *t;
                                // Show the device's ACTUAL effective sync (reported back = its own
                                // calibration/slider + our pushed offset), so calibrated clients read
                                // their real, differing offsets instead of the commanded 0. Falls back
                                // to the commanded value (dim) until the first report arrives.
                                let reported = *reported_trim;
                                let shown = reported.unwrap_or(ms);
                                ui.label(
                                    egui::RichText::new(format!("{shown:>+5} ms"))
                                        .monospace()
                                        .color(if reported.is_some() { c_text2() } else { c_dim() }),
                                )
                                .on_hover_text(if reported.is_some() {
                                    "the device's actual sync — its own calibration/slider plus the server offset"
                                } else {
                                    "server-commanded offset (the device hasn't reported its actual sync yet)"
                                });
                            });
                            if !calib_status.is_empty() {
                                let ok = calib_status.contains("aligned") || calib_status == "reference";
                                ui.label(
                                    egui::RichText::new(format!("   {calib_status}"))
                                        .size(11.0)
                                        .color(if ok { c_ok() } else { c_dim() }),
                                );
                            }
                            if active_caster == Some(*conn_id) {
                                ui.horizontal(|ui| {
                                    ui.label(egui::RichText::new("📡 casting").size(11.0).color(c_accent()));
                                    if ui
                                        .button("⏹ Stop cast")
                                        .on_hover_text("Kick this device off the cast slot so another client can claim it.")
                                        .clicked()
                                    {
                                        do_stop_cast = Some(*conn_id);
                                    }
                                });
                            }
                            ui.add_space(6.0);
                        });
                    }
                });
        });
        // Deliver any changed volume / sync, then act on the deferred cast/calibrate flags.
        self.push_client_state();
        if let Some(cid) = do_stop_cast {
            // Operator kicked the caster. The server is the SOLE authority on the slot and the
            // caster's own stopCast doesn't notify it — so free the slot IMMEDIATELY (letting another
            // client claim it) AND tell the caster's browser to tear down its uplink. No ack wait.
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
        if do_calibrate {
            if !self.start_calibrate_all() {
                *self.status.lock().unwrap() = "Need at least two connected devices to calibrate.".into();
            }
        } else if do_stop_calib {
            self.stop_calibrate_all(true);
        } else if self.calibrating {
            let ids: Vec<&(u64, String, String, bool, String, Option<i32>)> =
                snapshot.iter().filter(|c| c.3).collect();
            let ref_id = ids.iter().map(|c| c.0).min();
            let mut followers = 0;
            let mut done = 0;
            for c in &ids {
                if Some(c.0) == ref_id {
                    continue;
                }
                followers += 1;
                let s = c.4.as_str();
                if s.contains("aligned") || s.contains("failed") || s.contains("no lock") {
                    done += 1;
                }
            }
            if ref_id.is_none() || followers == 0 || done == followers {
                self.stop_calibrate_all(false);
            }
        }
        if self.calibrating {
            ui.ctx().request_repaint_after(Duration::from_millis(300));
        }
    }

    /// RIGHT column, top: the (Apply-gated) audio source picker + quality readout.
    fn ui_audio_source(&mut self, ui: &mut egui::Ui) {
        let audio_quality = self.audio_quality_text();
        card(ui, |ui| {
            eyebrow(ui, "AUDIO SOURCE");
            // Coupling: choosing a LOCAL audio source drops a cast-video selection (you can't relay
            // the caster's screen while capturing local audio — the uplink is one source).
            if ui
                .radio_value(
                    &mut self.source,
                    SourceKind::AllApps,
                    "All apps  —  recommended (keeps playing when Windows is muted)",
                )
                .clicked()
                && self.video_kind == VideoSourceKind::WebCast
            {
                self.video_kind = VideoSourceKind::Off;
            }
            #[cfg(target_os = "windows")]
            {
                let apps = self.apps.clone();
                ui.horizontal(|ui| {
                    if ui
                        .radio_value(&mut self.source, SourceKind::App, "Just one window / app:")
                        .clicked()
                        && self.video_kind == VideoSourceKind::WebCast
                    {
                        self.video_kind = VideoSourceKind::Off;
                    }
                    let label = if self.selected_pid.is_some() {
                        self.selected_name.clone()
                    } else {
                        "(choose)".to_string()
                    };
                    egui::ComboBox::from_id_salt("app_pick")
                        .selected_text(label)
                        .show_ui(ui, |ui| {
                            for a in &apps {
                                if ui
                                    .selectable_label(self.selected_pid == Some(a.pid), &a.name)
                                    .clicked()
                                {
                                    self.selected_pid = Some(a.pid);
                                    self.selected_name = a.name.clone();
                                    self.source = SourceKind::App;
                                    if self.video_kind == VideoSourceKind::WebCast {
                                        self.video_kind = VideoSourceKind::Off;
                                    }
                                }
                            }
                            if apps.is_empty() {
                                ui.label("(no windows / apps found — click Refresh)");
                            }
                        });
                    let label = if self.refreshing { "⟳ Refreshing…" } else { "⟳ Refresh" };
                    if ui.add_enabled(!self.refreshing, egui::Button::new(label)).clicked() {
                        self.refresh_apps();
                    }
                });
            }
            if ui
                .radio_value(
                    &mut self.source,
                    SourceKind::System,
                    "Full system output  —  goes silent when Windows is muted",
                )
                .clicked()
                && self.video_kind == VideoSourceKind::WebCast
            {
                self.video_kind = VideoSourceKind::Off;
            }
            if ui
                .radio_value(
                    &mut self.source,
                    SourceKind::WebUplink,
                    "Web client cast  —  a client casts its audio up to here",
                )
                .on_hover_text(
                    "Audio-only cast: a connected web client taps \"Cast\" and becomes the source. To \
                     also relay the caster's SCREEN, pick \"Web client cast\" under Video Source instead.",
                )
                .clicked()
            {
                // The cast as an AUDIO source = audio only; drop any video selection so it's
                // unambiguous. (Audio + video from the cast is the Video Source option, which also
                // selects this radio.)
                self.video_kind = VideoSourceKind::Off;
            }
            ui.add_space(7.0);
            ui.label(
                egui::RichText::new(format!("🎧 Streaming {audio_quality}"))
                    .size(11.5)
                    .color(c_dim()),
            );
        });
    }

    /// RIGHT column, middle: the (Apply-gated) video source + its quality/encoder disclosure.
    fn ui_video_source(&mut self, ui: &mut egui::Ui) {
        card(ui, |ui| {
            eyebrow(ui, "VIDEO SOURCE");
            // Two ways to cast from a web client: "Web client cast" under AUDIO SOURCE = audio only;
            // "Web client cast" here (below) = audio + video. Local Screen/Window pairs with a local
            // audio source — selecting one auto-reconciles the other so the pickers never disagree.
            ui.radio_value(&mut self.video_kind, VideoSourceKind::Off, "Off  —  audio only");
            if ui
                .radio_value(&mut self.video_kind, VideoSourceKind::Screen, "Whole screen")
                .clicked()
                && self.source == SourceKind::WebUplink
            {
                self.source = SourceKind::AllApps; // a local screen can't pair with a cast audio source
            }
            #[cfg(target_os = "windows")]
            {
                let windows: Vec<AudioApp> =
                    self.apps.iter().filter(|a| a.hwnd.is_some()).cloned().collect();
                ui.horizontal(|ui| {
                    if ui
                        .radio_value(&mut self.video_kind, VideoSourceKind::Window, "Just one window / app:")
                        .clicked()
                        && self.source == SourceKind::WebUplink
                    {
                        self.source = SourceKind::AllApps;
                    }
                    let label = if self.video_hwnd.is_some() && !self.video_name.is_empty() {
                        self.video_name.clone()
                    } else {
                        "(choose)".to_string()
                    };
                    egui::ComboBox::from_id_salt("vid_pick")
                        .selected_text(label)
                        .show_ui(ui, |ui| {
                            for a in &windows {
                                if ui
                                    .selectable_label(self.video_hwnd == a.hwnd, &a.name)
                                    .clicked()
                                {
                                    self.video_pid = Some(a.pid);
                                    self.video_name = a.name.clone();
                                    self.video_hwnd = a.hwnd;
                                    self.video_kind = VideoSourceKind::Window;
                                    self.source = SourceKind::App;
                                    self.selected_pid = Some(a.pid);
                                    self.selected_name = a.name.clone();
                                }
                            }
                            if windows.is_empty() {
                                ui.label("(no windows found — click Refresh)");
                            }
                        });
                    let label = if self.refreshing { "⟳ Refreshing…" } else { "⟳ Refresh" };
                    if ui.add_enabled(!self.refreshing, egui::Button::new(label)).clicked() {
                        self.refresh_apps();
                    }
                });
            }
            // Cast as a VIDEO source = audio + video from the caster. Selecting it also selects the
            // "Web client cast" AUDIO source (it's one uplink), so the two pickers never contradict.
            if ui
                .radio_value(
                    &mut self.video_kind,
                    VideoSourceKind::WebCast,
                    "Web client cast  —  a client casts its screen + audio up to here",
                )
                .on_hover_text(
                    "The web client casts its SCREEN + audio; the server re-broadcasts both to everyone \
                     at the quality below. (Also selects Web client cast as the audio source.)",
                )
                .clicked()
            {
                self.source = SourceKind::WebUplink;
            }
            if self.video_kind != VideoSourceKind::Off {
                ui.add_space(2.0);
                egui::CollapsingHeader::new(
                    egui::RichText::new("Quality, resolution & encoder").size(12.5).color(c_dim()),
                )
                .id_salt("vid_adv")
                .default_open(false)
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label("Resolution:");
                        egui::ComboBox::from_id_salt("res")
                            .selected_text(RES_LABELS[self.res_idx].0)
                            .show_ui(ui, |ui| {
                                for (i, (label, _)) in RES_LABELS.iter().enumerate() {
                                    ui.selectable_value(&mut self.res_idx, i, *label);
                                }
                            });
                        ui.checkbox(&mut self.fps60, "60 fps");
                    });
                    ui.horizontal(|ui| {
                        ui.label("Codec:").on_hover_text(
                            "AV1 = royalty-free (SVT-AV1 on the CPU, or your GPU's AV1 encoder if it \
                             has one) — the clean-to-distribute default. HEVC (H.265) = GPU hardware \
                             encode, best quality-per-bit and lightest on the CPU, but patent-encumbered \
                             and not every browser decodes it (Firefox can't).",
                        );
                        // Codec ⟺ backend here: HEVC = GPU HEVC (enc_idx 0 = "auto"); AV1 = enc_idx 2 =
                        // "av1" (GPU AV1 where the hardware supports it, else CPU SVT-AV1).
                        let is_av1 = self.enc_idx == 2;
                        egui::ComboBox::from_id_salt("codec")
                            .selected_text(if is_av1 { "AV1 · royalty-free" } else { "HEVC (H.265) · GPU" })
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut self.enc_idx, 0, "HEVC (H.265) · GPU");
                                ui.selectable_value(&mut self.enc_idx, 2, "AV1 · royalty-free");
                            });
                    });
                    ui.horizontal(|ui| {
                        ui.label("Quality:");
                        ui.add(egui::Slider::new(&mut self.video_quality_pct, 40..=250).suffix("%"));
                    });
                    {
                        let res = Resolution::parse(RES_LABELS[self.res_idx].1)
                            .unwrap_or(Resolution::P1080);
                        let est = VideoConfig {
                            resolution: res,
                            fps: if self.fps60 { Fps::F60 } else { Fps::F30 },
                            quality_pct: self.video_quality_pct,
                        }
                        .suggested_bitrate_kbps();
                        ui.label(
                            egui::RichText::new(format!(
                                "🎬  ≈ {:.1} Mbps target — lower for weaker hardware/Wi-Fi, higher for sharper video.",
                                est as f32 / 1000.0
                            ))
                            .size(11.0)
                            .color(c_dim()),
                        );
                    }
                });
            }
        });
    }

    /// RIGHT column: the buffer card (the former "Advanced" section, now a plain Apply-gated card).
    fn ui_buffer(&mut self, ui: &mut egui::Ui) {
        card(ui, |ui| {
            eyebrow(ui, "BUFFER");
            // Presets matched to this app's whole-home philosophy (multi-second buffers for
            // dropout immunity), with the raw ms slider beneath for fine control.
            ui.horizontal(|ui| {
                for (label, ms) in [("Snappy · 1s", 1000), ("Balanced · 3s", 3000), ("Rock-solid · 6s", 6000)] {
                    if ui.selectable_label(self.buffer_ms == ms, label).clicked() {
                        self.buffer_ms = ms;
                    }
                }
            });
            ui.add_space(4.0);
            // Buffer rationale folded into a hover on the slider (was a standalone 2-line paragraph)
            // to reclaim vertical space for the ad strip below.
            ui.add(
                egui::Slider::new(&mut self.buffer_ms, 200..=(config::MAX_BUFFER_MS as i32))
                    .suffix(" ms"),
            )
            .on_hover_text(
                "Reliable TCP stream (WebSocket/TLS): lost Wi-Fi packets are re-sent and this \
                 jitter buffer hides the stall. Bigger = more dropout-proof but more delay; \
                 identical on every client -> lock-step.",
            );
        });
    }

    /// RIGHT column, bottom: the single accent surface — the dirty-state Apply button.
    fn ui_apply(&mut self, ui: &mut egui::Ui, busy: bool, dirty: bool, clients_n: usize) {
        ui.label(
            egui::RichText::new("Source, video & buffer changes take effect when you Apply.")
                .size(11.0)
                .color(c_dim()),
        );
        ui.add_space(4.0);
        let label = if busy {
            "Starting…".to_string()
        } else if dirty {
            if clients_n > 0 {
                format!(
                    "Apply changes — reconnects {clients_n} device{}",
                    if clients_n == 1 { "" } else { "s" }
                )
            } else {
                "Apply changes".to_string()
            }
        } else {
            "Stream is up to date".to_string()
        };
        let (fill, txt_col) = if dirty && !busy {
            (c_accent(), egui::Color32::WHITE)
        } else {
            (c_surface(), c_dim())
        };
        let btn = egui::Button::new(egui::RichText::new(label).strong().size(15.0).color(txt_col))
            .fill(fill)
            .corner_radius(egui::CornerRadius::from(10));
        let resp = ui.add_enabled(dirty && !busy, btn.min_size(egui::vec2(ui.available_width(), 40.0)));
        let resp = if dirty && !busy {
            resp.on_hover_text(format!(
                "Rebuilds the stream with your new settings. {} (~1s of silence).",
                if clients_n == 0 {
                    "No devices are connected".to_string()
                } else if clients_n == 1 {
                    "1 connected device will reconnect".to_string()
                } else {
                    format!("All {clients_n} connected devices will reconnect")
                }
            ))
        } else {
            resp
        };
        if resp.clicked() {
            self.apply();
        }
    }
}

impl eframe::App for ServerApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // Fill the whole window with the theme background. eframe's framebuffer clear colour
        // doesn't track our runtime theme, so without this the backdrop stays dark behind the
        // light theme (white cards floating on a black window). Paint c_bg() at the very back.
        ui.ctx()
            .layer_painter(egui::LayerId::background())
            .rect_filled(ui.ctx().screen_rect(), 0.0, c_bg());
        // Default the UI scale to the design baseline (shown as 100%) on the first frame
        // (deterministic on every display). Users still adjust live with the ± buttons.
        if !self.did_initial_zoom {
            self.did_initial_zoom = true;
            ui.ctx().set_zoom_factor(UI_ZOOM_BASE);
        }
        ui.ctx().request_repaint_after(Duration::from_secs(1)); // keep counts/status live
        // Seed the "applied" baseline BEFORE poll_refresh() can self-heal a vanished source: it
        // must reflect what the stream was actually started with (the CLI/init config), so a heal
        // that rewrites the UI shows up as a pending change instead of being absorbed silently.
        if self.applied.is_none() {
            self.applied = Some(self.current_config());
        }
        #[cfg(target_os = "windows")]
        {
            self.poll_refresh();
            if self.refreshing {
                ui.ctx().request_repaint_after(Duration::from_millis(50)); // pick up the result promptly
            }
        }
        let busy = self.starting.load(Ordering::Relaxed);
        let st = self.status.lock().unwrap().clone();
        let clients_n = self.clients.load(Ordering::Relaxed);
        // Once a stream has come up, remember it. The live pill must NOT be inferred from the
        // transient status string — a failed switch or a "pick first" validation overwrites the
        // message while the previous stream keeps serving every client.
        if st.starts_with("Serving") {
            self.stream_live = true;
        }
        let stream_live = self.stream_live;
        let dirty = self.applied.as_ref() != Some(&self.current_config());
        let qr = self.qr_texture(ui.ctx()); // built once, then cached

        ui.add_space(2.0);
        // ---- Header band (full width): title + live status pill + zoom ----
        ui.horizontal(|ui| {
            ui.heading("Newfoundsync");
            let (pill, pcol) = if busy {
                ("Starting…".to_string(), c_accent_hi())
            } else if stream_live {
                (format!("Live · {clients_n} listening"), c_ok())
            } else {
                ("Stopped".to_string(), c_err())
            };
            // Status LED — drawn, not a glyph: egui's bundled font has no "●", so a literal renders
            // as a tofu box. A painted dot always renders and matches the status colour.
            let (dot, _) = ui.allocate_exact_size(egui::vec2(13.0, 16.0), egui::Sense::hover());
            ui.painter().circle_filled(egui::pos2(dot.left() + 6.0, dot.center().y), 4.5, pcol);
            ui.label(egui::RichText::new(pill).color(pcol).size(12.5).strong());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // Match the web client's header: theme toggle on the LEFT, then just − / + (no
                // percentage readout/reset). In a right-to-left layout the FIRST widget sits
                // rightmost, so add +, − (renders left→right as − +), then the toggle LAST (leftmost).
                // Step ±10% of the baseline; the window opens at UI_ZOOM_BASE and can shrink well
                // below it (down to 40% of baseline) so users can make it as tiny as they like.
                let step = UI_ZOOM_BASE * 0.1;
                if ui.small_button("+").on_hover_text("Bigger UI").clicked() {
                    let z = ui.ctx().zoom_factor();
                    ui.ctx().set_zoom_factor((z + step).min(UI_ZOOM_BASE * 2.5));
                }
                if ui.small_button("−").on_hover_text("Smaller UI").clicked() {
                    let z = ui.ctx().zoom_factor();
                    ui.ctx().set_zoom_factor((z - step).max(UI_ZOOM_BASE * 0.4));
                }
                ui.add_space(6.0);
                theme_toggle(ui); // leftmost — matches the client (toggle, then − +)
            });
        });
        ui.separator();
        ui.add_space(6.0);
        // ---- Connect strip (full width): URL plate + QR ----
        self.ui_connect_strip(ui, &qr);
        ui.add_space(8.0);
        ui.separator();
        ui.add_space(8.0);
        // ---- Body: two columns — config rail (~42%) | clients (hero, ~58%) ----
        // Reserve enough for the footer (separator + spacing + one status line) so the columns
        // don't overdraw it — there's no outer ScrollArea to absorb overflow.
        let footer_h = 44.0;
        let body_h = (ui.available_height() - footer_h).max(0.0);
        let full = ui.available_width();
        let gap = 12.0;
        let left_w = ((full - gap) * 0.42).max(0.0); // config rail (narrower) on the left
        ui.horizontal_top(|ui| {
            ui.allocate_ui_with_layout(
                egui::vec2(left_w, body_h),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    // Config rail in its own scroll area — a safety valve at the min window height.
                    egui::ScrollArea::vertical()
                        .id_salt("cfg")
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            self.ui_audio_source(ui);
                            ui.add_space(9.0);
                            self.ui_video_source(ui);
                            ui.add_space(9.0);
                            self.ui_buffer(ui);
                            ui.add_space(10.0);
                            self.ui_apply(ui, busy, dirty, clients_n);
                        });
                },
            );
            ui.add_space(gap);
            ui.allocate_ui_with_layout(
                egui::vec2(ui.available_width(), body_h),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    // Clients mixer is the hero (wider) column, now on the right.
                    self.ui_clients(ui, clients_n);
                },
            );
        });
        // ---- Footer band (full width): the tinted status line ----
        ui.separator();
        let st = self.status.lock().unwrap().clone();
        let col = if st.starts_with("Couldn't") || st.starts_with("Pick") {
            c_err()
        } else if st.starts_with("Serving") {
            c_ok()
        } else if busy || st.starts_with("Starting") {
            c_accent_hi()
        } else {
            c_dim()
        };
        // Truncate (ellipsize) — a long `Couldn't …` anyhow chain must stay one line, not wrap
        // off the bottom of the (non-scrolling) window.
        ui.add(egui::Label::new(egui::RichText::new(st).monospace().size(13.0).color(col)).truncate());
    }
}
