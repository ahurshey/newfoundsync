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
use newfoundsync_core::video::{EncodeDevice, EncoderBackend, Fps, Resolution, VideoConfig};
use newfoundsync_core::{config, net};

use crate::media::{self, CaptureSource, Media, MediaOptions, VideoTarget};
use crate::webserver::{self, ClientRegistry, StreamState};

// Both platforms now supply `sessions::{AudioApp, list_sources}` — Windows from a window/session
// enumeration, Linux from the live PulseAudio sink-input list. Same types, different semantics
// (see capture/linux_sessions.rs); the picker below is written to tolerate both.
#[cfg(any(target_os = "windows", target_os = "linux"))]
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
    let status = Arc::new(Mutex::new(String::from("Starting…")));
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
                    *status.lock().unwrap() = serving_text(&m);
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
                            *status.lock().unwrap() = format!(
                                "⚠ Audio source unavailable ({e}). Relaying web casts — pick a source and Apply."
                            );
                            starting.store(false, Ordering::Relaxed);
                            let _ = ready_tx.send(Some(rx));
                            control_loop(m, tx, cmd_rx, status, starting);
                        }
                        Err(e2) => {
                            *status.lock().unwrap() =
                                format!("Couldn't start: {e:#} (fallback also failed: {e2:#})");
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

    let lan = net::primary_lan_ipv4()
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
            // `--video` on a platform with no local screen capture would otherwise seed a selection
            // the picker renders as disabled but apply() still acts on, asking for video the server
            // then silently drops. Normalize here, at the seed, so the state is unrepresentable —
            // same rule the VP9 `enc_idx` seed follows just below.
            #[cfg(not(any(target_os = "windows", all(target_os = "linux", feature = "linux-capture"), all(target_os = "macos", feature = "mac-capture"))))]
            let kind = if kind == VideoSourceKind::Screen { VideoSourceKind::Off } else { kind };
            (kind, res_to_idx(v.resolution), v.fps == Fps::F60)
        }
        None => (VideoSourceKind::Off, 1, false),
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

    let mut app = ServerApp {
        server_name,
        url,
        clients,
        clients_reg,
        cast: cast.clone(),
        master_vol: 1.0,
        master_muted: false,
        av_offset_ms: (init.av_offset_ns.load(std::sync::atomic::Ordering::Relaxed) / 1_000_000)
            as i32,
        av_offset_ns: init.av_offset_ns.clone(),
        client_vols: HashMap::new(),
        client_trims: HashMap::new(),
        client_names: HashMap::new(),
        editing_client: None,
        edit_name_buf: String::new(),
        client_muted: HashMap::new(),
        applied: None,
        qr_tex: None,
        stream_live: false,
        last_logged_size: None,
        calibrating: false,
        status,
        capture_is_dummy: false,
        starting,
        cmd_tx,
        codec: init.codec,
        bitrate: init.bitrate,
        source,
        #[cfg(any(target_os = "windows", target_os = "linux"))]
        apps: Vec::new(),
        #[cfg(any(target_os = "windows", target_os = "linux"))]
        apps_rx: None,
        #[cfg(any(target_os = "windows", target_os = "linux"))]
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
        // A saved choice wins over the CLI default: the GUI's picker is the thing the operator
        // last touched, and it is the only way to set this without editing a shortcut.
        encode_device: crate::settings::load_encode_device().unwrap_or(init.encode_device),
        video_quality_pct: init.video.map(|v| v.quality_pct).unwrap_or(100),
        buffer_ms: init.buffer_ms as i32,
        port,
        port_edit: port,
        port_msg: String::new(),
        did_initial_zoom: false,
    };
    #[cfg(any(target_os = "windows", target_os = "linux"))]
    app.refresh_apps();

    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([1024.0, 576.0]) // 16:9 — wide two-column control panel
        // MIND THE UNITS — confusing them is exactly why this window used to refuse to get small.
        // These are winit LOGICAL pixels, which the OS display scale multiplies, while egui lays out
        // in POINTS where points = logical / zoom_factor (UI_ZOOM_BASE = 0.6). So the current
        // [640, 240] is 1067 x 400 points of layout, and on a 300%-scaled display a 1920 x 720
        // physical-pixel floor. The old [880, 495] was 2640 x 1485 physical there — most of a 4K
        // screen, from a number that reads like it should be small.
        //
        // Width matters as much as height: dragging a CORNER stops the moment EITHER axis hits its
        // floor, so an over-wide minimum makes the whole window feel immovable even while the height
        // still has room — which is how this actually presented. The floor is kept wide enough for the
        // client mixer row (icon + Vol slider + % + Sync slider + ms), which does not scroll
        // horizontally; both body columns scroll vertically and the QR shrinks, so going below the
        // content's natural height just scrolls instead of clipping.
        .with_min_inner_size([640.0, 240.0])
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
fn c_text2() -> egui::Color32 { if lt() { egui::Color32::from_rgb(0x3a, 0x46, 0x57) } else { egui::Color32::from_rgb(0xd6, 0xe0, 0xea) } }
fn c_dim() -> egui::Color32 { if lt() { egui::Color32::from_rgb(0x5c, 0x68, 0x78) } else { egui::Color32::from_rgb(0xad, 0xbd, 0xcd) } }
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

/// Horizontal inset between the window edge and the panels, in points. Deliberately small — the goal
/// is just to stop the cards touching the frame (and to stop the leftmost glyphs being clipped), not
/// to add a wide gutter that eats space we spent effort reclaiming.
/// The application id, in the reverse-DNS form the desktop stack expects. Must stay identical to
/// the Flatpak app id, the `.desktop` file's basename, and the installed icon's basename — a
/// mismatch in any of them is what makes a desktop show a placeholder icon instead of ours.
const APP_ID: &str = "ca.newfoundsync.Newfoundsync";

const EDGE_PAD: f32 = 10.0;

/// Width of a per-client Vol / Sync slider, in points. Two of them plus their labels and buttons
/// have to fit a client row even at the window's 880px minimum width.
const CLIENT_SLIDER_W: f32 = 88.0;

/// Space reserved at the end of the master row for its "100%" / " off" readout, so the slider can
/// take everything else. Sized for the widest string it has to hold, plus the item spacing.
const MASTER_READOUT_W: f32 = 52.0;

/// Rail thickness for the master slider, in points — three times the 6.0 the rest of the UI uses.
/// At full window width a hairline rail reads as a divider rather than something you grab, and this
/// is the one control that scales every device at once.
/// Everything on a client row that ISN'T the two sliders: the mute toggle, the "Vol" and "Sync"
/// captions, the percentage and millisecond readouts, the reset button, and the spacing between
/// them. Subtracted from the row width so the two bars can split what's actually left.
///
/// Deliberately generous — over-reserving costs a few points of bar, while under-reserving pushes
/// the reset button off the end of the row, and egui does not wrap a horizontal layout.
const CLIENT_ROW_FIXED_W: f32 = 290.0;

const MASTER_RAIL_H: f32 = 18.0;

/// Row height for the master slider. egui derives the handle radius from the row height
/// (`rect.height() / 2.5`), so raising this with the rail keeps the grab target in proportion
/// instead of leaving a small dot riding a thick bar.
const MASTER_ROW_H: f32 = 34.0;

/// Least room right of the QR worth placing the master into. Below this it goes on its own row:
/// mute button + a usable slider + the readout do not fit in less, and a horizontal layout that
/// doesn't fit does not shrink — it runs off the edge of the window.
const MASTER_MIN_INLINE: f32 = 260.0;

/// Apply the theme + desktop-tuned spacing/fonts/zoom once at startup.
fn setup_style(ctx: &egui::Context) {
    use egui::{FontId, TextStyle};
    #[cfg(any(target_os = "windows", all(target_os = "linux", feature = "linux-hw-encode")))]
    use egui::{FontData, FontDefinitions, FontFamily};

    // egui embeds Ubuntu Light only. The Flatpak carries Ubuntu Regular; the Windows test build
    // keeps the same file beside its EXE. Neither path relies on a host-installed font.
    #[cfg(any(target_os = "windows", all(target_os = "linux", feature = "linux-hw-encode")))]
    let font_path = {
        #[cfg(target_os = "windows")]
        {
            std::env::current_exe()
                .ok()
                .and_then(|path| path.parent().map(|dir| dir.join("Ubuntu-Regular.ttf")))
                .unwrap_or_else(|| std::path::PathBuf::from("Ubuntu-Regular.ttf"))
        }
        #[cfg(all(target_os = "linux", feature = "linux-hw-encode"))]
        {
            std::path::PathBuf::from("/app/share/newfoundsync/Ubuntu-Regular.ttf")
        }
    };
    #[cfg(any(target_os = "windows", all(target_os = "linux", feature = "linux-hw-encode")))]
    match std::fs::read(&font_path) {
        Ok(bytes) => {
            let mut fonts = FontDefinitions::default();
            let name = "Newfoundsync Ubuntu Regular".to_owned();
            fonts
                .font_data
                .insert(name.clone(), std::sync::Arc::new(FontData::from_owned(bytes)));
            fonts
                .families
                .get_mut(&FontFamily::Proportional)
                .expect("egui's default proportional font family")
                .insert(0, name);
            ctx.set_fonts(fonts);
        }
        Err(e) => tracing::warn!(path = %font_path.display(), "could not load regular UI font: {e}"),
    }

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

/// The "Starting…" text, made specific when a local screen capture is about to be requested.
///
/// On Linux that request opens the desktop portal's share dialog and blocks until somebody answers
/// it. A bare "Starting…" during that wait is actively misleading -- reported from the field as
/// "it flashes up the screen grabber then sticks at Starting forever", which was really a 45-second
/// wait for a prompt the operator had already dismissed. Naming the thing being waited on turns a
/// hang into an instruction.
fn starting_text(video_kind: VideoSourceKind) -> String {
    let _ = video_kind;
    #[cfg(all(target_os = "linux", feature = "linux-capture"))]
    if video_kind == VideoSourceKind::Screen {
        return "Starting… approve the screen-share prompt if it appears".to_string();
    }
    "Starting…".to_string()
}

fn serving_text(m: &Media) -> String {
    format!(
        "{SERVING_PREFIX}{}{}",
        m.capture_device,
        if m.config.video { "  + screen" } else { "" }
    )
}

/// Should the "streaming silence" banner be showing?
///
/// The one status string carries two kinds of message: the durable serving line, and transient
/// chatter ("Starting…", "Pick an app first, then Apply."). Recomputing the verdict from whatever
/// happens to be in `status` would blank a live silence warning the moment any of that chatter
/// lands — the warning would flicker away exactly when the operator went looking for it. So only a
/// real serving line is allowed to move the verdict; everything else leaves it latched.
fn dummy_verdict(latched: bool, status: &str) -> bool {
    if status.starts_with(SERVING_PREFIX) {
        status.contains(crate::capture::DUMMY_TAG)
    } else {
        latched
    }
}

const SERVING_PREFIX: &str = "Serving: ";

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
        // Same reasoning as starting_text(): if this start is going to open the portal dialog, say
        // so, because the wait is long enough to look like a freeze.
        let waiting_on_portal = cfg!(all(target_os = "linux", feature = "linux-capture"))
            && opts.video.is_some()
            && !matches!(opts.capture_source, CaptureSource::WebUplink);
        *status.lock().unwrap() = if waiting_on_portal {
            "Starting… approve the screen-share prompt if it appears".into()
        } else {
            String::from("Starting…")
        };
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
    encode_device: EncodeDevice,
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
    /// Last window size we logged, so the geometry diagnostic prints on change rather than per frame.
    last_logged_size: Option<(i32, i32)>,
    /// True while a server-orchestrated "Calibrate all" run is active (Phase B).
    calibrating: bool,
    status: Arc<Mutex<String>>,
    /// Latched from the serving line: the capture device is a dummy/null sink, so we are streaming
    /// silence. Drives the AUDIO SOURCE warning banner. See [`dummy_verdict`] for why it's latched
    /// rather than recomputed from `status` every frame.
    capture_is_dummy: bool,
    starting: Arc<AtomicBool>,
    cmd_tx: mpsc::Sender<MediaOptions>,
    codec: CodecKind,
    bitrate: i32,
    source: SourceKind,
    #[cfg(any(target_os = "windows", target_os = "linux"))]
    apps: Vec<AudioApp>,
    // Async source enumeration: refresh_apps() spawns a worker and stashes the receiver here;
    // poll_refresh() (called each frame) applies the result. Never blocks the GUI thread.
    #[cfg(any(target_os = "windows", target_os = "linux"))]
    apps_rx: Option<mpsc::Receiver<Vec<AudioApp>>>,
    #[cfg(any(target_os = "windows", target_os = "linux"))]
    refreshing: bool,
    /// When the Linux app list was last re-enumerated. Windows windows persist, so a manual
    /// refresh is fine there; a Linux list is a snapshot of what is *playing this instant*, so it
    /// has to re-scan on a timer or it will show apps that already stopped and miss ones that
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
    #[cfg(any(target_os = "windows", target_os = "linux"))]
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
    #[cfg(any(target_os = "windows", target_os = "linux"))]
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
            video,
            video_target,
            encoder,
            encode_device: self.encode_device,
        };
        self.starting.store(true, Ordering::Relaxed);
        *self.status.lock().unwrap() = starting_text(self.video_kind);
        let _ = self.cmd_tx.send(opts);
        self.applied = Some(self.current_config()); // baseline for the "changes pending" state
    }

    // ===== 16:9 layout sections (composed by `ui` below) ============================

    /// Full-width "connect" strip under the header: the URL plate (left) + the QR code,
    /// scan hint and one-time-cert disclosure (right). Read-only.
    fn ui_connect_strip(&mut self, ui: &mut egui::Ui, qr: &Option<egui::TextureHandle>) {
        let inline_master = ui.horizontal_top(|ui| {
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
            // The QR's drawn size varies with the window height, so the master can only line up with
            // it by asking — hence the side length coming back out of this block.
            let qr_side = ui.vertical(|ui| {
                let mut side = 0.0;
                if let Some(tex) = qr {
                    // The QR is the largest fixed block in the window, and drawn at its natural
                    // texture size it never yielded an inch — so in a short window it was pure
                    // reserved space pushing everything else down. Cap it to a slice of the height
                    // that's actually available, with a floor that keeps it scannable.
                    const QR_MIN: f32 = 96.0;
                    const QR_SHARE: f32 = 0.34; // of the panel height, so it yields first
                    let natural = tex.size_vec2().x;
                    side = natural.min((ui.available_height() * QR_SHARE).max(QR_MIN));
                    ui.image(egui::load::SizedTexture::new(tex.id(), egui::vec2(side, side)));
                }
                // Cert help folded into a hover (was a standalone "First time on a device?" line) to
                // reclaim vertical space — hover "Scan to connect" for the one-time-cert explanation.
                ui.label(egui::RichText::new("📷  Scan to connect").strong().color(c_text()))
                    .on_hover_text(
                        "First time on a device? Accept the one-time security warning \
                         (Advanced -> proceed) — it's a self-signed certificate, needed so the \
                         browser allows playback.",
                    );
                side
            })
            .inner;
            // MASTER VOLUME lives up here, right of the QR, rather than inside the clients card.
            //
            // It is a LIVE control — it takes effect instantly, no Apply — and this strip is the one
            // region that is always on screen. Down in the clients card it scrolled out of reach as
            // soon as a few devices connected, which is the wrong behaviour for the control that
            // mutes the whole house. It also fills the widest dead space in the window.
            //
            // Inline ONLY when the room is genuinely there. egui does not wrap a horizontal layout —
            // it overflows — so on a narrow window (or a small screen, where the URL box and QR have
            // already eaten the row) this would render past the window edge and simply not exist as
            // far as the operator is concerned. Below the threshold it drops to its own row instead,
            // which keeps it visible and full-width rather than clipped.
            let room = ui.available_width();
            let inline = room >= MASTER_MIN_INLINE;
            if inline {
                ui.add_space(20.0);
                // Centred on the QR SQUARE, not on the whole QR column (which includes the "Scan to
                // connect" caption) — the square is what the eye lines the bar up against. Allocating
                // a region of exactly the QR's height and laying out across it with Align::Center
                // does the centring from the real measurement, so it stays true as the QR resizes
                // with the window.
                // Centre the BAR on the QR's midline — not the label+bar group. The bar is what the
                // eye pairs with the QR square; centring the group instead leaves the bar sitting
                // below the QR's middle by half a label.
                //
                // Done as an explicit offset rather than a centred Layout: egui's Align::Center
                // vertically centres ITEMS in a row, but a nested `vertical()` block is sized after
                // it is placed, so it just lands at the cursor and the centring silently no-ops —
                // measured on a 1920x1080 desktop, bar at y=177 against a QR midline of y=199.
                let label_h = ui.text_style_height(&egui::TextStyle::Body);
                let pad = (qr_side * 0.5 - (label_h + 4.0 + MASTER_ROW_H * 0.5)).max(0.0);
                ui.vertical(|ui| {
                    ui.add_space(pad);
                    self.master_control(ui);
                });
            }
            inline
        })
        .inner;
        // Didn't fit beside the QR — give it the full width of its own row. Still above the columns,
        // so it is still the always-visible control it was moved up here to be.
        if !inline_master {
            ui.add_space(8.0);
            self.master_control(ui);
        }
    }

    /// The master volume control: label above, mute + slider + readout beneath. One definition, used
    /// both inline beside the QR and as its own row when the window is too narrow for that.
    fn master_control(&mut self, ui: &mut egui::Ui) {
        ui.label(egui::RichText::new("Master").strong().color(c_text()));
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            if ui
                .selectable_label(self.master_muted, if self.master_muted { "🔇" } else { "🔊" })
                .on_hover_text(if self.master_muted {
                    "Unmute everyone (restores the master level)"
                } else {
                    "Mute everyone instantly — the master level is kept"
                })
                .clicked()
            {
                self.master_muted = !self.master_muted;
            }
            // Fills whatever the window leaves, re-measured every frame, so the bar tracks a resize
            // instead of sitting at a fixed length. Only the readout is reserved. The floor keeps it
            // draggable once the URL box and QR have taken their share of a narrow window.
            //
            // The trade-off of a very long bar is that one pixel of drag becomes a very small step;
            // if that proves fiddly the answer is keyboard nudging, not shortening the bar.
            ui.spacing_mut().slider_rail_height = MASTER_RAIL_H;
            ui.spacing_mut().interact_size.y = MASTER_ROW_H;
            ui.spacing_mut().slider_width =
                (ui.available_width() - MASTER_READOUT_W).max(CLIENT_SLIDER_W);
            ui.add(egui::Slider::new(&mut self.master_vol, 0.0..=1.0).show_value(false))
                .on_hover_text("Scales every client's volume — applies instantly");
            ui.label(
                egui::RichText::new(if self.master_muted {
                    " off".to_string()
                } else {
                    format!("{:>3}%", (self.master_vol * 100.0).round() as i32)
                })
                .monospace()
                .color(c_dim()),
            );
        });
    }

    /// CLIENTS column (the right-hand one): the connected-clients mixer. Builds the registry snapshot, renders
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
        // Sync resets, deferred out of the card closure for the same reason: the client registry
        // can't be locked while `self` is borrowed by the UI.
        let mut reset_sync_for: Option<String> = None; // one device, by stable id
        let mut do_reset_all = false;
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
            // Always render the control so the feature is discoverable; it only ENABLES once there
            // are 2+ identified devices to align. (With fewer, "align all" has nothing to do — a
            // lone device self-calibrates from its own browser's Calibrate button instead.)
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                if calibrating {
                    if ui.button("⏹ Stop calibration").clicked() {
                        do_stop_calib = true;
                    }
                    ui.label(
                        egui::RichText::new("Aligning all devices…").size(11.5).color(c_accent()),
                    );
                } else {
                    let ready = n_identified >= 2;
                    let resp = ui
                        .add_enabled(ready, egui::Button::new("Calibrate all"))
                        .on_hover_text(
                            "Earliest-connected device plays a sync code; the rest listen on \
                             their mics and align at once. Devices must be in the same room \
                             with working mics. (Uses the coded signal.)",
                        )
                        .on_disabled_hover_text(
                            "Connect 2+ devices (same room, working mics) to align them. A single \
                             device self-calibrates from its own browser.",
                        );
                    if resp.clicked() {
                        do_calibrate = true;
                    }
                    // Sits to the RIGHT of "Calibrate all" and is the undo for it. Enabled whenever
                    // anything is connected — unlike calibration it needs no mics, no second device,
                    // and no room: it is the way back to a known state when an alignment went wrong.
                    if ui
                        .add_enabled(!snapshot.is_empty(), egui::Button::new("Reset all"))
                        .on_hover_text(
                            "Reset every connected device's sync to 0 ms — clears the server \
                             offsets and tells each device to drop its own calibration trim.",
                        )
                        .on_disabled_hover_text("No devices connected.")
                        .clicked()
                    {
                        do_reset_all = true;
                    }
                    if !ready {
                        ui.label(
                            egui::RichText::new("connect 2+ devices to align them")
                                .size(11.5)
                                .color(c_dim()),
                        );
                    }
                }
            });
            ui.add_space(4.0);
            // Per-client rows scroll independently — the one routinely-growing region. Distinct
            // id_salt from the config rail's scroll area so their scroll states don't collide.
            //
            // auto_shrink is [false, TRUE]: fill the width, but take only the height the rows need.
            // With `false` on the vertical axis this claimed the whole column, which dragged the
            // surrounding card frame down with it and left a tall empty bordered box under a short
            // client list. Shrinking vertically means the card ends just below the last client, and
            // the scrollbar appears only once the rows genuinely overflow the column (the enclosing
            // allocate_ui_with_layout caps the available height).
            //
            // NB this has nothing to do with how small the WINDOW can be dragged: egui layout never
            // feeds back into the OS resize floor, which comes solely from `with_min_inner_size`.
            // (Measured via WM_GETMINMAXINFO — don't go looking in the layout code for that.)
            egui::ScrollArea::vertical()
                .id_salt("clients")
                .auto_shrink([false, true])
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
                                // Both bars grow with the window, like the master above them.
                                //
                                // Measured, not fixed: take the row's width, reserve what the
                                // non-slider widgets need (mute, the two captions, both readouts,
                                // the reset button and the gaps between them), and split the rest
                                // between the two sliders — they share one `slider_width`, so it is
                                // one number for both. CLIENT_SLIDER_W stays as the FLOOR, which is
                                // what a narrow window falls back to rather than collapsing them to
                                // an undraggable stub.
                                let per_slider = ((ui.available_width() - CLIENT_ROW_FIXED_W) * 0.5)
                                    .max(CLIENT_SLIDER_W);
                                ui.spacing_mut().slider_width = per_slider;
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
                                if ui
                                    .button("⟲")
                                    .on_hover_text(
                                        "Reset this device's sync to 0 ms — clears BOTH the server \
                                         offset and the device's own calibration trim.",
                                    )
                                    .clicked()
                                {
                                    reset_sync_for = Some(stable_id.clone());
                                }
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
        // A sync reset has TWO halves and needs both. Clearing our own offset only moves the value
        // the server commands; a calibrated device keeps its whole speaker-latency compensation in
        // its OWN trim, which lives in the browser and is reachable only by asking. Sending one
        // without the other leaves the device sitting at a number nobody chose.
        if do_reset_all || reset_sync_for.is_some() {
            if let Ok(mut reg) = self.clients_reg.lock() {
                for e in reg.values_mut() {
                    if !do_reset_all && reset_sync_for.as_deref() != Some(e.stable_id.as_str()) {
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
            *self.status.lock().unwrap() = if do_reset_all {
                "Sync reset to 0 ms on every connected device.".into()
            } else {
                "Sync reset to 0 ms on that device.".into()
            };
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

    /// CONFIG RAIL (the left-hand column), top: the (Apply-gated) audio source picker + quality readout.
    fn ui_audio_source(&mut self, ui: &mut egui::Ui) {
        let audio_quality = self.audio_quality_text();
        card(ui, |ui| {
            // OS-appropriate source labels — WASAPI "mute" semantics don't apply on Linux/macOS.
            #[cfg(target_os = "windows")]
            const ALL_APPS_LABEL: &str = "All apps  —  recommended (keeps playing when Windows is muted)";
            #[cfg(target_os = "linux")]
            const ALL_APPS_LABEL: &str = "All apps  —  recommended (captures the system output)";
            #[cfg(not(any(target_os = "windows", target_os = "linux")))]
            const ALL_APPS_LABEL: &str = "All apps  —  recommended";
            #[cfg(target_os = "windows")]
            const SYSTEM_LABEL: &str = "Full system output  —  goes silent when Windows is muted";
            #[cfg(target_os = "linux")]
            const SYSTEM_LABEL: &str = "Full system output  —  mirrors the speakers (follows system mute)";
            #[cfg(not(any(target_os = "windows", target_os = "linux")))]
            const SYSTEM_LABEL: &str = "Full system output";
            eyebrow(ui, "AUDIO SOURCE");
            // Silence is the one failure the UI cannot show by staying quiet: every control below
            // looks correct while the stream carries nothing. The device name in the footer already
            // says so, but that line is small, far from here, and easy to miss — so say it in the
            // panel the operator is actually reading when they wonder why there's no sound.
            {
                let st = self.status.lock().unwrap().clone();
                self.capture_is_dummy = dummy_verdict(self.capture_is_dummy, &st);
            }
            if self.capture_is_dummy {
                ui.label(
                    egui::RichText::new(
                        "⚠  No real audio output device — using the Dummy Output.\n\
                         Nothing is audible on this machine, and this stream carries only what\n\
                         apps still play into the dummy — silence whenever nothing does.\n\
                         Every card profile is off, or there's no sound hardware (usual in a VM).\n\
                         If `pactl list cards` shows a card:\n\
                         pactl set-card-profile <card> output:analog-stereo   — then press Apply.",
                    )
                    .size(11.5)
                    .color(c_err()),
                );
                ui.add_space(6.0);
            }
            // Coupling: choosing a LOCAL audio source drops a cast-video selection (you can't relay
            // the caster's screen while capturing local audio — the uplink is one source).
            if ui
                .radio_value(
                    &mut self.source,
                    SourceKind::AllApps,
                    ALL_APPS_LABEL,
                )
                .clicked()
                && self.video_kind == VideoSourceKind::WebCast
            {
                self.video_kind = VideoSourceKind::Off;
            }
            #[cfg(any(target_os = "windows", target_os = "linux"))]
            {
                // The radio's wording differs per platform because the underlying thing differs.
                // Windows lists windows and audio sessions, so "one window / app" is honest. Linux
                // can only see streams that are playing, so promising "window" there would be a
                // lie the operator discovers only after picking something.
                #[cfg(target_os = "windows")]
                const PICK_LABEL: &str = "Just one window / app:";
                #[cfg(target_os = "linux")]
                const PICK_LABEL: &str = "Just one app (must be playing):";
                let apps = self.apps.clone();
                ui.horizontal(|ui| {
                    if ui
                        .radio_value(&mut self.source, SourceKind::App, PICK_LABEL)
                        .clicked()
                    {
                        // Arriving at the per-app source: enumerate now, so the list is current the
                        // first time it is opened rather than whatever was cached at startup.
                        self.refresh_apps();
                        if self.video_kind == VideoSourceKind::WebCast {
                            self.video_kind = VideoSourceKind::Off;
                        }
                    }
                    let label = if self.selected_pid.is_some() {
                        self.selected_name.clone()
                    } else {
                        "(choose)".to_string()
                    };
                    let combo = egui::ComboBox::from_id_salt("app_pick")
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
                                // The empty state has to explain itself on Linux: an empty list is
                                // the NORMAL state when nothing happens to be playing, not a
                                // failure to enumerate, and "click Refresh" would be useless advice.
                                #[cfg(target_os = "windows")]
                                ui.label("(no windows / apps found — click Refresh)");
                                #[cfg(target_os = "linux")]
                                ui.label("(nothing is playing audio right now — start playback and it appears here)");
                            }
                        });
                    // Opening the picker enumerates. With no background scan, this is what keeps the
                    // list honest: what you are about to choose from was just read, not cached from
                    // whenever the GUI last happened to look.
                    if combo.response.clicked() {
                        self.refresh_apps();
                    }
                    let label = if self.refreshing { "⟳ Refreshing…" } else { "⟳ Refresh" };
                    if ui.add_enabled(!self.refreshing, egui::Button::new(label)).clicked() {
                        self.refresh_apps();
                    }
                });
            }
            // Linux gets the picker (audio only); per-WINDOW capture stays Windows-only because
            // window-scoped audio has no meaning on PipeWire — audio belongs to a process's
            // stream, and nothing maps a window to one.
            #[cfg(target_os = "linux")]
            ui.label(
                egui::RichText::new(
                    "Per-window capture is Windows-only; on Linux audio belongs to an app, not a window.",
                )
                .size(11.0)
                .color(c_dim()),
            );
            #[cfg(not(any(target_os = "windows", target_os = "linux")))]
            ui.label(
                egui::RichText::new(
                    "Per-app / single-window capture is Windows-only — on this OS use All apps or Full system output.",
                )
                .size(11.0)
                .color(c_dim()),
            );
            if ui
                .radio_value(
                    &mut self.source,
                    SourceKind::System,
                    SYSTEM_LABEL,
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

    /// CONFIG RAIL, middle: the (Apply-gated) video source + its quality/encoder disclosure.
    fn ui_video_source(&mut self, ui: &mut egui::Ui) {
        card(ui, |ui| {
            eyebrow(ui, "VIDEO SOURCE");
            // Two ways to cast from a web client: "Web client cast" under AUDIO SOURCE = audio only;
            // "Web client cast" here (below) = audio + video. Local Screen/Window pairs with a local
            // audio source — selecting one auto-reconciles the other so the pickers never disagree.
            ui.radio_value(&mut self.video_kind, VideoSourceKind::Off, "Off  —  audio only");
            // Local screen capture is Windows-only (WGC): `video::capture` is not even compiled
            // elsewhere, and media.rs drops video with a log line nobody reads. Offering the radio
            // anyway meant picking "Whole screen", pressing Apply, and silently getting audio —
            // the stream looked healthy and clients simply never saw a picture. Show it, so the
            // capability is discoverable, but disabled and saying why.
            #[cfg(any(target_os = "windows", all(target_os = "linux", feature = "linux-capture"), all(target_os = "macos", feature = "mac-capture")))]
            if ui
                .radio_value(&mut self.video_kind, VideoSourceKind::Screen, "Whole screen")
                .clicked()
                && self.source == SourceKind::WebUplink
            {
                self.source = SourceKind::AllApps; // a local screen can't pair with a cast audio source
            }
            #[cfg(not(any(target_os = "windows", all(target_os = "linux", feature = "linux-capture"), all(target_os = "macos", feature = "mac-capture"))))]
            ui.add_enabled(
                false,
                egui::RadioButton::new(false, "Whole screen  —  not built into this server"),
            )
            .on_disabled_hover_text(
                "Capturing this machine's screen is only implemented on Windows. To share a \
                 screen from here, use \"Web client cast\" below and cast from a browser tab \
                 instead — that path relays real video.",
            );
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
                            "Which video codec to send. Hover either option for what it's good for — \
                             the short version is AV1 for quality, VP9 if a device won't play AV1.",
                        );
                        // VP9 links libvpx and lives behind the (non-default) `vp9` feature, so this
                        // build may be unable to honor it. Don't offer a choice we'd silently
                        // substitute AV1 for — the picker must not claim a codec we won't send.
                        // (`enc_idx` is already normalized at the seed site, so no mutation here — this
                        // closure only runs once the disclosure is expanded, and writing state during
                        // rendering would fake a pending change.)
                        let vp9_available = cfg!(feature = "vp9");
                        let is_vp9 = self.enc_idx == 1 && vp9_available;
                        egui::ComboBox::from_id_salt("codec")
                            .selected_text(if is_vp9 { ENC_LABELS[1].0 } else { ENC_LABELS[0].0 })
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut self.enc_idx, 0, ENC_LABELS[0].0)
                                    .on_hover_text(AV1_HINT);
                                if vp9_available {
                                    ui.selectable_value(&mut self.enc_idx, 1, ENC_LABELS[1].0)
                                        .on_hover_text(VP9_HINT);
                                } else {
                                    ui.add_enabled(false, egui::Button::new("VP9 · not in this build"))
                                        .on_disabled_hover_text(
                                            "This build was compiled without the `vp9` feature (it links \
                                             libvpx via vcpkg). Rebuild with --features vp9 to enable it.",
                                        );
                                }
                            });
                    });
                    // WHERE video encodes, as opposed to WHICH codec above. Worth exposing because
                    // the right answer is machine-specific and neither choice is universally
                    // better: the GPU encoder costs almost no CPU but some hardware encoders are
                    // lower quality per bitrate (or, on this project's own bug reports, flaky),
                    // while SVT-AV1 gives up CPU headroom that a busy server may not have.
                    ui.horizontal(|ui| {
                        ui.label("Encode on:").on_hover_text(
                            "Auto tries the GPU and quietly falls back to CPU — what this has \
                             always done. GPU only refuses to start video if the GPU has no usable \
                             AV1 encoder, instead of spending the CPU you were trying to protect. \
                             CPU only never touches the GPU, which is the way out of a flaky \
                             hardware encoder.",
                        );
                        // The screencast Flatpak has VA-API/NVENC backends; the ordinary Linux
                        // Flatpak intentionally does not, so only expose a choice where it works.
                        #[cfg(any(target_os = "windows", all(target_os = "linux", feature = "linux-hw-encode")))]
                        egui::ComboBox::from_id_salt("encode_device")
                            .selected_text(match self.encode_device {
                                EncodeDevice::Auto => "Auto · GPU, else CPU",
                                EncodeDevice::Gpu => "GPU only",
                                EncodeDevice::Cpu => "CPU only",
                            })
                            .show_ui(ui, |ui| {
                                for (d, label) in [
                                    (EncodeDevice::Auto, "Auto · GPU, else CPU"),
                                    (EncodeDevice::Gpu, "GPU only · fail if unavailable"),
                                    (EncodeDevice::Cpu, "CPU only · never use the GPU"),
                                ] {
                                    if ui
                                        .selectable_value(&mut self.encode_device, d, label)
                                        .clicked()
                                    {
                                        // Persist immediately rather than on Apply. Apply rebuilds
                                        // the stream and reconnects every listener; the operator
                                        // reaching for this is usually mid-diagnosis of a bad
                                        // encoder and shouldn't lose the setting to a crash before
                                        // they get around to applying it.
                                        if let Err(e) = crate::settings::save_encode_device(d) {
                                            tracing::warn!("couldn't save encode device: {e}");
                                        }
                                    }
                                }
                            });
                        #[cfg(not(any(target_os = "windows", all(target_os = "linux", feature = "linux-hw-encode"))))]
                        ui.label(
                            egui::RichText::new("CPU (SVT-AV1) — no GPU AV1/VP9 encoder here")
                                .size(11.0)
                                .color(c_dim()),
                        )
                        .on_hover_text(
                            "Not a missing feature — a hardware one. Most GPUs can DECODE AV1/VP9 \
                             but only encode H.264/HEVC, which this app doesn't ship. Hardware AV1 \
                             encoding needs Intel Arc / Meteor Lake or newer, AMD RX 7000 or newer, \
                             or an RTX 40-series; no NVIDIA GPU has ever encoded VP9. On Windows \
                             the GPU path exists because Media Foundation exposes an AV1 encoder \
                             where the hardware has one.",
                        );
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
            // A/V OFFSET — live, deliberately NOT Apply-gated.
            //
            // Judging lip-sync needs continuous playback, so a control you can only change by
            // restarting the stream is a control you cannot actually tune. This writes straight to
            // the atomic the audio producer reads per frame.
            //
            // Only meaningful with video on, so it is hidden when video is off rather than sitting
            // there inviting a change that does nothing.
            if self.video_kind != VideoSourceKind::Off {
                ui.add_space(8.0);
                ui.separator();
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("A/V offset").color(c_text2())).on_hover_text(
                        "Shifts AUDIO against video, for everyone, instantly. Drag right if you \
                         hear the sound BEFORE the lips move; left if the sound lags the picture. \
                         It is needed because a player writes its audio to the speakers ahead of \
                         time, so the sound we capture belongs with a picture slightly later than \
                         the one we capture with it — and nothing on the system reports that gap.",
                    );
                    let mut ms = self.av_offset_ms;
                    ui.spacing_mut().slider_width = CLIENT_SLIDER_W * 2.0;
                    let resp = ui.add(egui::Slider::new(&mut ms, -500..=500).show_value(false));
                    // The LIVE value has to move on every change — that is the whole point of the
                    // control, you tune it by ear against a running stream.
                    if resp.changed() {
                        self.apply_av_offset(ms);
                    }
                    // Saving is a different question. save_key() reads the whole settings file,
                    // writes a temp copy and renames it, and `changed()` fires on every mouse-move
                    // of a drag — so persisting there did that dance ~60 times a second for the
                    // length of a drag. Once the drag ends is exactly as durable and costs one write.
                    if resp.drag_stopped() || (resp.changed() && !resp.dragged()) {
                        self.save_av_offset();
                    }
                    ui.label(
                        egui::RichText::new(format!("{:>+5} ms", self.av_offset_ms))
                            .monospace()
                            .color(c_dim()),
                    );
                    if ui.button("⟲").on_hover_text("Back to 0 ms").clicked() {
                        self.apply_av_offset(0);
                        self.save_av_offset();
                    }
                });
                ui.label(
                    egui::RichText::new(
                        "+ = delay the audio (fixes sound arriving before the picture)",
                    )
                    .size(11.0)
                    .color(c_dim()),
                );
            }
        });
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

    /// CONFIG RAIL: the buffer card (the former "Advanced" section, now a plain Apply-gated card).
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
                egui::Slider::new(&mut self.buffer_ms, (config::MIN_BUFFER_MS as i32)..=(config::MAX_BUFFER_MS as i32))
                    .suffix(" ms"),
            )
            .on_hover_text(
                "Reliable TCP stream (WebSocket/TLS): lost Wi-Fi packets are re-sent and this \
                 jitter buffer hides the stall. Bigger = more dropout-proof but more delay; \
                 identical on every client -> lock-step.",
            );
        });
    }

    /// CONFIG RAIL, bottom: the single accent surface — the dirty-state Apply button.
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

        // Breathing room at the window edges.
        //
        // eframe's `App::ui` hands us the ROOT ui straight from `run_ui` — unlike a CentralPanel there
        // is no frame and therefore no margin at all, so every card sat flush against the window edge
        // and the leftmost glyphs were actually being clipped ("AUDIO SOURCE" rendering as "UDIO
        // SOURCE"). Inset a padded child and shadow the binding, so everything below lays out inside
        // the inset with no other changes. Horizontal only: the header already has its own top spacing
        // and the footer is meant to sit on the bottom edge.
        let padded = ui.max_rect().shrink2(egui::vec2(EDGE_PAD, 0.0));
        let mut ui = ui.new_child(egui::UiBuilder::new().max_rect(padded));
        let ui = &mut ui;
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
        #[cfg(any(target_os = "windows", target_os = "linux"))]
        {
            self.poll_refresh();
            if self.refreshing {
                ui.ctx().request_repaint_after(Duration::from_millis(50)); // pick up the result promptly
            }
            // NO periodic rescan. Linux entries exist only while their app is playing, so the list
            // does go stale — but a scan every 2 s made the Refresh button flicker to "⟳ Refreshing…"
            // and disable itself every couple of seconds, which reads as the UI fighting you, and it
            // kept the GUI repainting forever on an idle machine.
            //
            // The list is refreshed where it actually matters instead: when you select the per-app
            // source, when you open the picker (so what you are looking at was just enumerated), and
            // whenever you press Refresh. That is what the button is for.
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

        // Window-geometry diagnostic. "It won't resize any smaller" is otherwise unanswerable without
        // guessing at DPI: the min size is in LOGICAL pixels while everything egui lays out is in
        // POINTS (scaled by zoom_factor), so the two are easy to confuse. Logged at debug, once per
        // distinct size, so `RUST_LOG=newfoundsync=debug` shows exactly where the floor is.
        {
            let vp = ui.ctx().input(|i| i.viewport().inner_rect);
            if let Some(r) = vp {
                let size = (r.width().round() as i32, r.height().round() as i32);
                if self.last_logged_size != Some(size) {
                    self.last_logged_size = Some(size);
                    tracing::debug!(
                        win_pts_w = size.0,
                        win_pts_h = size.1,
                        zoom = ui.ctx().zoom_factor(),
                        ppp = ui.ctx().pixels_per_point(),
                        avail_h_pts = ui.available_height(),
                        "window geometry (points; multiply by ppp for physical pixels)"
                    );
                }
            }
        }

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
        // ---- Body: two columns — config rail | clients (shares set just below) ----
        // Reserve enough for the footer (separator + spacing + one status line) so the columns
        // don't overdraw it — there's no outer ScrollArea to absorb overflow.
        let footer_h = 44.0;
        let body_h = (ui.available_height() - footer_h).max(0.0);
        let full = ui.available_width();
        let gap = 12.0;
        // Column split. The clients card used to get 58% and the config rail 42%, which was backwards
        // for the actual content: the rail has long wrapping labels ("All apps — recommended (keeps
        // playing when Windows is muted)") that want the room, while the client rows are fixed-width
        // widgets (icon + Vol slider + % + Sync slider + ms) whose sliders do NOT stretch — so the
        // extra width just became empty card. Giving the rail the larger share narrows the clients
        // block to roughly what it uses and lets the whole window be narrower.
        let left_w = ((full - gap) * 0.54).max(0.0);
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
                    // Clients mixer — the right-hand column (see the split rationale above).
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

#[cfg(test)]
mod tests {
    use super::{dummy_verdict, SERVING_PREFIX};
    use crate::capture::DUMMY_TAG;

    #[test]
    fn raises_the_banner_on_a_dummy_serving_line() {
        let st = format!("{SERVING_PREFIX}auto_null.monitor  {DUMMY_TAG}");
        assert!(dummy_verdict(false, &st));
    }

    #[test]
    fn stays_quiet_for_a_real_device() {
        let st = format!("{SERVING_PREFIX}alsa_output.pci-0000_00_1f.3.analog-stereo.monitor");
        assert!(!dummy_verdict(false, &st));
        // …and a real device CLEARS a previously latched warning (the self-heal / an Apply worked).
        assert!(!dummy_verdict(true, &st));
    }

    #[test]
    fn transient_chatter_does_not_blank_a_live_warning() {
        // The bug this latch exists to prevent: the operator hits Apply, a validation message
        // overwrites the serving line, and the silence warning vanishes while still true.
        for chatter in [
            "Starting…",
            "Pick an app first, then Apply.",
            "Couldn't switch: no such device",
            "⚠ Audio source unavailable (x). Relaying web casts — pick a source and Apply.",
        ] {
            assert!(dummy_verdict(true, chatter), "{chatter} wrongly cleared the warning");
            assert!(!dummy_verdict(false, chatter), "{chatter} wrongly raised the warning");
        }
    }

    #[test]
    fn the_tag_the_banner_matches_is_the_tag_the_capture_layer_emits() {
        // Guards the cross-platform seam: pulse.rs builds the device string, gui.rs greps it. If
        // someone edits one literal, `DUMMY_TAG` is the single definition that keeps them agreeing.
        assert!(DUMMY_TAG.contains("DUMMY OUTPUT"));
        assert!(format!("{SERVING_PREFIX}auto_null.monitor  {DUMMY_TAG}").contains(DUMMY_TAG));
    }
}
