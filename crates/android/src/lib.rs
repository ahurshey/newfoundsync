//! Newfoundsync Android client.
//!
//! Android can't capture other apps' audio, so the phone/tablet is a *client*: it
//! discovers a server on the LAN, clock-syncs, and plays the stream in sync via
//! the shared `newfoundsync-core` engine. The whole crate is gated to Android; on
//! other targets it compiles to an empty cdylib.

#![cfg(target_os = "android")]

use std::time::Duration;

use eframe::egui;
use winit::platform::android::activity::AndroidApp;

use newfoundsync_core::config;
use newfoundsync_core::discovery::{self, Browser};
use newfoundsync_core::playback::Volume;
use newfoundsync_core::runtime::ClientHandle;

mod multicast;

struct ClientApp {
    browser: Option<Browser>,
    client: Option<ClientHandle>,
    selected: Option<String>,
    volume: f32,
    delay_ms: i64,
    buffer_ms: i64,
    err: Option<String>,
    _mlock: Option<multicast::MulticastLock>,
}

impl eframe::App for ClientApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        ui.ctx().request_repaint_after(Duration::from_millis(500));
        egui::CentralPanel::default().show_inside(ui, |ui| {
            ui.heading("Newfoundsync");
            ui.separator();
            if let Some(client) = &self.client {
                ui.label(format!("Playing from “{}”", client.server_name));
                ui.label(format!("Output: {}", client.output_device));
                let s = client.status();
                let sync = if s.synced { "✔ synced" } else { "… syncing" };
                ui.label(format!(
                    "{sync}   offset {} µs   rtt {} µs",
                    s.offset_ns / 1000,
                    s.rtt_ns / 1000
                ));
                ui.label(format!(
                    "buffer {}   played {}   late {}",
                    s.buffered, s.played, s.late_drop
                ));
                ui.add_space(8.0);
                if ui
                    .add(egui::Slider::new(&mut self.volume, 0.0..=1.0).text("Volume"))
                    .changed()
                {
                    client.set_volume(self.volume);
                }
                if ui
                    .add(egui::Slider::new(&mut self.delay_ms, -200..=200).text("Delay (ms)"))
                    .changed()
                {
                    client.set_delay_ms(self.delay_ms);
                }
                ui.add_space(8.0);
                if ui.button("⏹ Disconnect").clicked() {
                    self.client = None;
                }
            } else {
                ui.label("Servers on your network:");
                let servers = self.browser.as_ref().map(|b| b.servers()).unwrap_or_default();
                if servers.is_empty() {
                    ui.weak("(searching…)");
                }
                for srv in &servers {
                    let sel = self.selected.as_deref() == Some(srv.fullname.as_str());
                    let label = format!("{}  ·  {} ({})", srv.name, srv.ip, srv.codec);
                    if ui.selectable_label(sel, label).clicked() {
                        self.selected = Some(srv.fullname.clone());
                    }
                }
                ui.add_space(8.0);
                if ui
                    .add_enabled(self.selected.is_some(), egui::Button::new("▶ Connect"))
                    .clicked()
                {
                    if let Some(fullname) = self.selected.clone() {
                        if let Some(srv) = servers.iter().find(|s| s.fullname == fullname) {
                            match ClientHandle::start(srv, Volume::new(self.volume), self.buffer_ms) {
                                Ok(h) => {
                                    self.client = Some(h);
                                    self.err = None;
                                }
                                Err(e) => self.err = Some(format!("{e}")),
                            }
                        }
                    }
                }
                if let Some(e) = &self.err {
                    ui.colored_label(egui::Color32::RED, e);
                }
            }
        });
    }
}

#[no_mangle]
fn android_main(app: AndroidApp) {
    android_logger::init_once(
        android_logger::Config::default().with_max_level(log::LevelFilter::Info),
    );
    log::info!("Newfoundsync Android client starting");

    // mDNS multicast responses are dropped on Android without a MulticastLock.
    let mlock = match multicast::MulticastLock::acquire(&app) {
        Ok(l) => Some(l),
        Err(e) => {
            log::warn!("multicast lock failed (discovery may not work): {e}");
            None
        }
    };

    let browser = match discovery::browse() {
        Ok(b) => Some(b),
        Err(e) => {
            log::warn!("discovery failed: {e}");
            None
        }
    };

    let state = ClientApp {
        browser,
        client: None,
        selected: None,
        volume: 1.0,
        delay_ms: 0,
        buffer_ms: config::DEFAULT_BUFFER_MS,
        err: None,
        _mlock: mlock,
    };

    // Request exactly the adapter's limits rather than wgpu's defaults: software
    // GPUs (the emulator's SwiftShader) and weak devices report lower limits
    // (e.g. max_uniform_buffer_binding_size 16384 < the 65536 default), which
    // would otherwise fail device creation. Real phone GPUs are unaffected.
    let mut wgpu_options = eframe::egui_wgpu::WgpuConfiguration::default();
    if let eframe::egui_wgpu::WgpuSetup::CreateNew(setup) = &mut wgpu_options.wgpu_setup {
        setup.device_descriptor = std::sync::Arc::new(|adapter| {
            eframe::egui_wgpu::wgpu::DeviceDescriptor {
                label: Some("newfoundsync"),
                required_limits: adapter.limits(),
                ..Default::default()
            }
        });
    }

    let options = eframe::NativeOptions {
        android_app: Some(app),
        renderer: eframe::Renderer::Wgpu,
        wgpu_options,
        ..Default::default()
    };
    if let Err(e) = eframe::run_native(
        "Newfoundsync",
        options,
        Box::new(|_cc| Ok(Box::new(state))),
    ) {
        log::error!("eframe exited with error: {e}");
    }
}
