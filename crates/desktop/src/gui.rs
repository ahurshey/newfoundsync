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

use crate::media::{self, CaptureSource, Media, MediaOptions};
use crate::webserver::{self, StreamState};

#[cfg(target_os = "windows")]
use crate::capture::sessions::{self, AudioApp};

#[derive(PartialEq, Clone, Copy)]
enum SourceKind {
    AllApps,
    App,
    System,
}

// (display label, parse token)
const RES_LABELS: [(&str, &str); 4] = [
    ("720p", "720p"),
    ("1080p", "1080p"),
    ("1440p", "1440p"),
    ("2160p (4K)", "2160p"),
];
const ENC_LABELS: [(&str, &str); 3] = [
    ("Auto (GPU, CPU fallback)", "auto"),
    ("GPU only", "hardware"),
    ("CPU only", "cpu"),
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
            encoder: self.encoder,
        }
    }
}

/// Launch the GUI. Blocks until the window is closed.
pub fn run(port: u16, server_name: String, init: InitialConfig) -> Result<()> {
    let clients = Arc::new(AtomicUsize::new(0));
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
        std::thread::Builder::new()
            .name("web-server".into())
            .spawn(move || match tokio::runtime::Runtime::new() {
                Ok(rt) => {
                    if let Err(e) = rt.block_on(webserver::run(state_rx, clients, addr, true)) {
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
        CaptureSource::App { pid } => (SourceKind::App, Some(pid)),
    };
    let (video_on, res_idx, fps60) = match init.video {
        Some(v) => (true, res_to_idx(v.resolution), v.fps == Fps::F60),
        None => (false, 1, false),
    };
    let enc_idx = match init.encoder {
        EncoderBackend::Auto => 0,
        EncoderBackend::Hardware => 1,
        EncoderBackend::Cpu => 2,
    };

    let mut app = ServerApp {
        server_name,
        url,
        clients,
        status,
        starting,
        cmd_tx,
        codec: init.codec,
        bitrate: init.bitrate,
        source,
        #[cfg(target_os = "windows")]
        apps: Vec::new(),
        selected_pid,
        selected_name: String::new(),
        video_on,
        res_idx,
        fps60,
        enc_idx,
        buffer_ms: init.buffer_ms as i32,
    };
    #[cfg(target_os = "windows")]
    app.refresh_apps();

    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([500.0, 580.0])
        .with_min_inner_size([420.0, 460.0])
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

// ---- Harbour Glass palette (matches the web client) -------------------------
const C_BG: egui::Color32 = egui::Color32::from_rgb(0x0b, 0x0f, 0x15);
const C_SURFACE: egui::Color32 = egui::Color32::from_rgb(0x16, 0x1c, 0x26);
const C_SURFACE_ALT: egui::Color32 = egui::Color32::from_rgb(0x1d, 0x25, 0x31);
const C_BORDER: egui::Color32 = egui::Color32::from_rgb(0x2a, 0x33, 0x40);
const C_TEXT: egui::Color32 = egui::Color32::from_rgb(0xe8, 0xee, 0xf5);
const C_TEXT2: egui::Color32 = egui::Color32::from_rgb(0xc9, 0xd4, 0xe0);
const C_DIM: egui::Color32 = egui::Color32::from_rgb(0x94, 0xa1, 0xb2);
const C_ACCENT: egui::Color32 = egui::Color32::from_rgb(0x3b, 0x8e, 0xff);
const C_ACCENT_HI: egui::Color32 = egui::Color32::from_rgb(0x5a, 0xa2, 0xff);
const C_OK: egui::Color32 = egui::Color32::from_rgb(0x3f, 0xb9, 0x50);
const C_ERR: egui::Color32 = egui::Color32::from_rgb(0xf8, 0x51, 0x49);

/// Apply the dark navy "Harbour Glass" theme once at startup.
fn setup_style(ctx: &egui::Context) {
    use egui::{Color32, CornerRadius, FontId, Stroke, TextStyle};

    let mut v = egui::Visuals::dark();
    v.window_fill = C_BG;
    v.panel_fill = C_BG;
    v.faint_bg_color = C_SURFACE;
    v.extreme_bg_color = C_SURFACE_ALT;
    v.window_corner_radius = CornerRadius::from(10);
    v.hyperlink_color = C_ACCENT_HI;
    v.slider_trailing_fill = true;
    v.selection.bg_fill = C_ACCENT;
    v.selection.stroke = Stroke::new(1.0, C_TEXT);

    let cr = CornerRadius::from(8);
    let w = &mut v.widgets;
    w.noninteractive.corner_radius = cr;
    w.noninteractive.bg_fill = C_SURFACE;
    w.noninteractive.weak_bg_fill = C_SURFACE;
    w.noninteractive.bg_stroke = Stroke::new(1.0, C_BORDER);
    w.noninteractive.fg_stroke = Stroke::new(1.0, C_DIM);
    w.inactive.corner_radius = cr;
    w.inactive.bg_fill = C_SURFACE_ALT;
    w.inactive.weak_bg_fill = C_SURFACE_ALT;
    w.inactive.bg_stroke = Stroke::new(1.0, C_BORDER);
    w.inactive.fg_stroke = Stroke::new(1.0, C_TEXT2);
    w.hovered.corner_radius = cr;
    w.hovered.bg_fill = Color32::from_rgb(0x24, 0x30, 0x44);
    w.hovered.weak_bg_fill = Color32::from_rgb(0x24, 0x30, 0x44);
    w.hovered.bg_stroke = Stroke::new(1.0, Color32::from_rgb(0x34, 0x40, 0x4f));
    w.hovered.fg_stroke = Stroke::new(1.0, C_TEXT);
    w.hovered.expansion = 1.0;
    w.active.corner_radius = cr;
    w.active.bg_fill = Color32::from_rgb(0x2a, 0x38, 0x50);
    w.active.weak_bg_fill = Color32::from_rgb(0x2a, 0x38, 0x50);
    w.active.bg_stroke = Stroke::new(1.0, C_ACCENT);
    w.active.fg_stroke = Stroke::new(1.0, Color32::WHITE);
    w.active.expansion = 0.0;
    w.open = w.hovered.clone();

    let mut s = (*ctx.style()).clone();
    s.visuals = v;
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

    // Open a touch more compact on high-DPI / 4K screens. The −/+ buttons fine-tune
    // it live, so this is just the starting point (90%).
    ctx.set_zoom_factor(0.9);
}

/// An eyebrow section label (uppercase, dim, small).
fn eyebrow(ui: &mut egui::Ui, text: &str) {
    ui.label(egui::RichText::new(text).strong().size(13.0).color(C_DIM));
    ui.add_space(6.0);
}

/// A bordered "card" container for grouping a section.
fn card<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) {
    egui::Frame::group(ui.style())
        .fill(C_SURFACE)
        .stroke(egui::Stroke::new(1.0, C_BORDER))
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

struct ServerApp {
    server_name: String,
    url: String,
    clients: Arc<AtomicUsize>,
    status: Arc<Mutex<String>>,
    starting: Arc<AtomicBool>,
    cmd_tx: mpsc::Sender<MediaOptions>,
    codec: CodecKind,
    bitrate: i32,
    source: SourceKind,
    #[cfg(target_os = "windows")]
    apps: Vec<AudioApp>,
    selected_pid: Option<u32>,
    selected_name: String,
    video_on: bool,
    res_idx: usize,
    fps60: bool,
    enc_idx: usize,
    buffer_ms: i32,
}

impl ServerApp {
    #[cfg(target_os = "windows")]
    fn refresh_apps(&mut self) {
        self.apps = sessions::list_sources(std::process::id());
        match self.selected_pid {
            Some(pid) => {
                if let Some(a) = self.apps.iter().find(|a| a.pid == pid) {
                    self.selected_name = a.name.clone();
                } else {
                    self.selected_pid = None;
                    self.selected_name.clear();
                }
            }
            None => {}
        }
    }

    /// Build options from the current UI and hand them to the control thread.
    fn apply(&mut self) {
        let capture_source = match self.source {
            SourceKind::AllApps => CaptureSource::AllExceptSelf,
            SourceKind::System => CaptureSource::System,
            SourceKind::App => match self.selected_pid {
                Some(pid) => CaptureSource::App { pid },
                None => {
                    *self.status.lock().unwrap() = "Pick an app first, then Apply.".into();
                    return;
                }
            },
        };
        let video = if self.video_on {
            let res = Resolution::parse(RES_LABELS[self.res_idx].1).unwrap_or(Resolution::P1080);
            Some(VideoConfig {
                resolution: res,
                fps: if self.fps60 { Fps::F60 } else { Fps::F30 },
            })
        } else {
            None
        };
        let encoder = EncoderBackend::parse(ENC_LABELS[self.enc_idx].1).unwrap_or(EncoderBackend::Auto);

        let opts = MediaOptions {
            name: self.server_name.clone(),
            codec: self.codec,
            bitrate: self.bitrate,
            lead_ms: config::DEFAULT_LEAD_MS,
            buffer_ms: self.buffer_ms.clamp(200, config::MAX_BUFFER_MS as i32) as i64,
            capture_source,
            video,
            encoder,
        };
        self.starting.store(true, Ordering::Relaxed);
        *self.status.lock().unwrap() = "Starting…".into();
        let _ = self.cmd_tx.send(opts);
    }
}

impl eframe::App for ServerApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        ui.ctx().request_repaint_after(Duration::from_secs(1)); // keep counts/status live
        let busy = self.starting.load(Ordering::Relaxed);

        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.add_space(2.0);
            // Title row + a discoverable zoom control (Ctrl +/− also work).
            ui.horizontal(|ui| {
                ui.heading("Newfoundsync");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.small_button("+").on_hover_text("Bigger UI (Ctrl +)").clicked() {
                        let z = ui.ctx().zoom_factor();
                        ui.ctx().set_zoom_factor((z + 0.1).min(2.5));
                    }
                    let pct = (ui.ctx().zoom_factor() * 100.0).round() as i32;
                    if ui
                        .small_button(format!("{pct}%"))
                        .on_hover_text("Reset UI size (Ctrl 0)")
                        .clicked()
                    {
                        ui.ctx().set_zoom_factor(1.0);
                    }
                    if ui.small_button("−").on_hover_text("Smaller UI (Ctrl −)").clicked() {
                        let z = ui.ctx().zoom_factor();
                        ui.ctx().set_zoom_factor((z - 0.1).max(0.6));
                    }
                });
            });
            ui.add_space(6.0);
            ui.label(
                egui::RichText::new("Open this on any phone or PC on the same Wi-Fi:").color(C_DIM),
            );
            ui.add_space(6.0);

            // URL plate
            egui::Frame::group(ui.style())
                .fill(C_SURFACE)
                .stroke(egui::Stroke::new(1.0, C_BORDER))
                .corner_radius(egui::CornerRadius::from(10))
                .inner_margin(egui::Margin::from(9.0))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new(&self.url)
                                .monospace()
                                .size(19.0)
                                .strong()
                                .color(C_ACCENT_HI),
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
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new(
                    "First visit on each device: accept the security warning (Advanced → proceed) — \
                     it's a self-signed certificate, needed so the browser allows playback.",
                )
                .size(11.5)
                .color(egui::Color32::from_rgb(0x7f, 0x8e, 0xa3)),
            );
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("Connected clients:").color(C_DIM));
                ui.label(
                    egui::RichText::new(self.clients.load(Ordering::Relaxed).to_string())
                        .strong()
                        .color(C_ACCENT),
                );
            });
            ui.add_space(9.0);

            // ---- Audio source ----
            card(ui, |ui| {
                eyebrow(ui, "AUDIO SOURCE");
                ui.radio_value(
                    &mut self.source,
                    SourceKind::AllApps,
                    "All apps  —  recommended (keeps playing when Windows is muted)",
                );
                #[cfg(target_os = "windows")]
                {
                    let apps = self.apps.clone();
                    ui.horizontal(|ui| {
                        ui.radio_value(&mut self.source, SourceKind::App, "Just one window / app:");
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
                                    }
                                }
                                if apps.is_empty() {
                                    ui.label("(no windows / apps found — click Refresh)");
                                }
                            });
                        if ui.button("⟳ Refresh").clicked() {
                            self.refresh_apps();
                        }
                    });
                }
                ui.radio_value(
                    &mut self.source,
                    SourceKind::System,
                    "Full system output  —  goes silent when Windows is muted",
                );
            });
            ui.add_space(9.0);

            // ---- Screen video ----
            card(ui, |ui| {
                eyebrow(ui, "SCREEN VIDEO");
                ui.checkbox(&mut self.video_on, "Also share the screen");
                if self.video_on {
                    ui.add_space(2.0);
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
                        ui.label("Encoder:");
                        egui::ComboBox::from_id_salt("enc")
                            .selected_text(ENC_LABELS[self.enc_idx].0)
                            .show_ui(ui, |ui| {
                                for (i, (label, _)) in ENC_LABELS.iter().enumerate() {
                                    ui.selectable_value(&mut self.enc_idx, i, *label);
                                }
                            });
                    });
                }
            });
            ui.add_space(9.0);

            // ---- Buffer ----
            card(ui, |ui| {
                eyebrow(ui, "BUFFER");
                ui.add(
                    egui::Slider::new(&mut self.buffer_ms, 200..=(config::MAX_BUFFER_MS as i32))
                        .suffix(" ms"),
                );
                ui.add_space(2.0);
                ui.label(
                    egui::RichText::new(
                        "Bigger buffer = more dropout-proof but more delay. Same on every client → lock-step.",
                    )
                    .size(11.0)
                    .color(egui::Color32::from_rgb(0x7f, 0x8e, 0xa3)),
                );
            });
            ui.add_space(10.0);

            // ---- Apply (the one accent surface) ----
            let label = if busy {
                "Starting…"
            } else {
                "▶  Apply / restart stream"
            };
            let btn = egui::Button::new(
                egui::RichText::new(label).strong().size(15.0).color(egui::Color32::WHITE),
            )
            .fill(C_ACCENT)
            .corner_radius(egui::CornerRadius::from(10));
            if ui
                .add_enabled(!busy, btn.min_size(egui::vec2(ui.available_width(), 40.0)))
                .clicked()
            {
                self.apply();
            }
            ui.add_space(6.0);

            // status, tinted by content
            let st = self.status.lock().unwrap().clone();
            let col = if st.starts_with("Couldn't") || st.starts_with("Pick") {
                C_ERR
            } else if st.starts_with("Serving") {
                C_OK
            } else if busy || st.starts_with("Starting") {
                C_ACCENT_HI
            } else {
                C_DIM
            };
            ui.label(egui::RichText::new(st).monospace().size(13.0).color(col));
        });
    }
}
