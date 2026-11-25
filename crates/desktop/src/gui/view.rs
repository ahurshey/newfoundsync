// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Alex Hurshman and the Newfoundsync contributors.

//! The server window's layout.
//!
//! The window is built around what it is used for day to day: seeing that sound is reaching the
//! stream and who is listening, and turning a room up or down. Those get the header and the main
//! area. Choosing what to share sits in a column on the left; connecting a device and the settings
//! that are touched once step aside into their own views.
//!
//! ```text
//! ┌ header ── name · status chip · level meter ─────────── Add a device · Settings ┐
//! ├ banners ─ only when something is wrong (sound stopped, a failed switch, …) ────┤
//! │ SHARING        │ DEVICES                                                       │
//! │  source        │  master                                                       │
//! │  screen        │  one row per device: name + health · mute · volume · sync     │
//! │  delay         │  (or: first-connection steps / Add a device / Settings)       │
//! │  sound quality │                                                               │
//! ├ pending ─ only when a setting differs from what is streaming: Discard · Apply ─┤
//! └ footer ── what is being shared ────────────────────────────────────── version ─┘
//! ```
//!
//! Everything here only DRAWS and collects clicks; what a click does lives in `gui.rs`.

use eframe::egui::{self, Align, Color32, Layout, RichText};

use super::*;

/// Height of a device row's name block: a name line and a health line.
const ROW_H: f32 = 36.0;
/// Width reserved for a row's volume readout ("100%") and for its sync button ("+500 ms").
const PCT_W: f32 = 42.0;
const SYNC_W: f32 = 78.0;
/// Width of the sharing column, in points.
const SHARE_W: f32 = 300.0;

/// The one-line state of the stream, shown as the chip in the header.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Chip {
    Starting,
    Stopped,
    SoundStopped,
    WaitingForCast,
    NoOutput,
    Ready,
    /// Devices are connected and the pipeline is healthy, but the source has been silent for a while.
    Quiet,
    Live,
}

/// Everything about the stream that a frame needs, read once at the top of it.
struct Live {
    busy: bool,
    waiting_on_portal: bool,
    /// Label of the capture device being served; `None` if no stream ever came up.
    serving: Option<String>,
    video: bool,
    error: Option<String>,
    /// Streaming from a dummy output: the stream carries silence.
    dummy: bool,
    /// The source is a web cast (no local capture).
    is_cast: bool,
    /// A client currently holds the cast slot.
    cast_held: bool,
    /// Audio was flowing and stopped — the same test the watchdog, `/health` and `/status` use.
    stalled: bool,
    stall_s: i64,
    video_stalled: bool,
    capture_closed: bool,
    encoder_failed: bool,
    clients_n: usize,
    /// A local source that has carried only digital silence for several seconds. Not a fault —
    /// paused music looks like this — but "Live" in green would overstate it.
    quiet: bool,
}

/// Which chip the header shows. Ordered worst-first, so a fault is never hidden behind "Live".
fn chip_for(l: &Live) -> Chip {
    if l.busy {
        Chip::Starting
    } else if l.serving.is_none() {
        Chip::Stopped
    } else if l.stalled {
        Chip::SoundStopped
    } else if l.is_cast && !l.cast_held {
        // Not a fault: with a cast source, nothing plays until a device presses Cast.
        Chip::WaitingForCast
    } else if l.dummy {
        Chip::NoOutput
    } else if l.clients_n == 0 {
        Chip::Ready
    } else if l.quiet {
        Chip::Quiet
    } else {
        Chip::Live
    }
}

fn plural(n: usize, one: &str) -> String {
    if n == 1 {
        format!("1 {one}")
    } else {
        format!("{n} {one}s")
    }
}

/// "5 min", "2 h", "3 days" — how long ago, coarsely.
fn ago(secs: u64) -> String {
    match secs {
        0..=89 => "a moment".to_string(),
        90..=5_399 => format!("{} min", (secs + 30) / 60),
        5_400..=129_599 => format!("{} h", (secs + 1_800) / 3_600),
        _ => format!("{} days", (secs + 43_200) / 86_400),
    }
}

/// Secondary text: hints and captions.
fn small(text: impl Into<String>) -> RichText {
    RichText::new(text).size(12.0).color(c_dim())
}

/// An uppercase section label.
fn eyebrow(ui: &mut egui::Ui, text: &str) {
    ui.label(RichText::new(text).strong().size(11.5).color(c_dim()));
}

/// A bordered surface that sets one thing apart (the master volume, an open sync panel).
fn card(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::NONE
        .fill(c_surface())
        .stroke(egui::Stroke::new(1.0_f32, c_border()))
        .corner_radius(egui::CornerRadius::from(10))
        .inner_margin(egui::Margin::symmetric(12, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui);
        });
}

/// The status chip: a dot and a few words on a tinted ground.
fn pill(ui: &mut egui::Ui, text: &str, fg: Color32, bg: Color32) {
    egui::Frame::NONE
        .fill(bg)
        .corner_radius(egui::CornerRadius::from(12))
        .inner_margin(egui::Margin::symmetric(10, 3))
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            // Drawn, not a glyph: egui's bundled font has no "●", so a literal renders as a tofu box.
            let (dot, _) = ui.allocate_exact_size(egui::vec2(8.0, 16.0), egui::Sense::hover());
            ui.painter().circle_filled(dot.center(), 4.0, fg);
            ui.label(RichText::new(text).size(12.5).strong().color(fg));
        });
}

/// The stereo level meter: two thin bars, left channel over right.
fn meter(ui: &mut egui::Ui, levels: [f32; 2]) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(110.0, 16.0), egui::Sense::hover());
    for (i, level) in levels.iter().enumerate() {
        let top = rect.top() + 2.0 + i as f32 * 7.0;
        let track = egui::Rect::from_min_size(egui::pos2(rect.left(), top), egui::vec2(rect.width(), 5.0));
        ui.painter().rect_filled(track, 2.5, c_surface_alt());
        let w = track.width() * level.clamp(0.0, 1.0);
        if w >= 1.0 {
            let lit = egui::Rect::from_min_size(track.min, egui::vec2(w, 5.0));
            // The top of the scale is close to clipping; say so with the warning colour.
            ui.painter().rect_filled(lit, 2.5, if *level > 0.97 { c_warn() } else { c_ok() });
        }
    }
    resp
}

/// One half of a two- or three-way switch, filling the width it is given.
fn seg(ui: &mut egui::Ui, selected: bool, text: &str, height: f32) -> egui::Response {
    ui.add_sized(
        [ui.available_width(), height],
        egui::Button::new(RichText::new(text).color(if selected { c_text() } else { c_text2() })).selected(selected),
    )
}

/// A two-line choice — a name over its value — with both lines centred. (A plain button lays a
/// two-line label out left-aligned inside itself, which reads as a mistake in a row of three.)
fn seg2(ui: &mut egui::Ui, selected: bool, top: &str, bottom: &str) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 42.0), egui::Sense::click());
    if ui.is_rect_visible(rect) {
        let v = ui.style().interact_selectable(&resp, selected);
        ui.painter().rect(
            rect.expand(v.expansion),
            v.corner_radius,
            v.weak_bg_fill,
            v.bg_stroke,
            egui::StrokeKind::Inside,
        );
        let fg = if selected { ui.visuals().selection.stroke.color } else { c_text2() };
        let x = rect.center().x;
        ui.painter().text(
            egui::pos2(x, rect.top() + 14.0),
            egui::Align2::CENTER_CENTER,
            top,
            egui::FontId::proportional(13.5),
            fg,
        );
        ui.painter().text(
            egui::pos2(x, rect.bottom() - 12.0),
            egui::Align2::CENTER_CENTER,
            bottom,
            egui::FontId::proportional(12.0),
            if selected { fg } else { c_dim() },
        );
    }
    resp
}

/// A choice in the source list: a radio button with a title, one line saying what it does, and
/// (when it is the selected one) whatever extra controls it needs. Returns true when picked.
fn option_row(
    ui: &mut egui::Ui,
    selected: bool,
    title: &str,
    desc: &str,
    sub: impl FnOnce(&mut egui::Ui),
) -> bool {
    let mut picked = false;
    egui::Frame::NONE
        .fill(if selected { c_accent_bg() } else { Color32::TRANSPARENT })
        .corner_radius(egui::CornerRadius::from(8))
        .inner_margin(egui::Margin::symmetric(8, 5))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 2.0;
            if ui.add(egui::RadioButton::new(selected, RichText::new(title).color(c_text()))).clicked() {
                picked = true;
            }
            ui.horizontal_top(|ui| {
                ui.add_space(18.0); // line the description up under the title, past the radio
                ui.vertical(|ui| {
                    ui.add(egui::Label::new(small(desc)).wrap());
                    if selected {
                        ui.spacing_mut().item_spacing.y = 5.0;
                        sub(ui);
                    }
                });
            });
        });
    picked
}

/// A full-width notice under the header. `actions` draws its buttons, if it has any.
fn banner(
    ui: &mut egui::Ui,
    fg: Color32,
    bg: Color32,
    title: &str,
    body: &str,
    has_actions: bool,
    actions: impl FnOnce(&mut egui::Ui),
) {
    egui::Frame::NONE.fill(bg).inner_margin(egui::Margin::symmetric(14, 8)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(title).strong().color(fg));
            ui.label(RichText::new(body).color(c_text()));
        });
        if has_actions {
            ui.horizontal(|ui| actions(ui));
        }
    });
}

impl ServerApp {
    /// Read the stream's state for this frame, and advance the level meter.
    fn live(&mut self) -> Live {
        let st = self.status.lock().unwrap().clone();
        let stream = self.stream_rx.borrow().clone();
        let cast_held = self.cast.lock().map(|c| c.is_some()).unwrap_or(false);
        let h = &stream.health;
        let stall_ms = h.audio_stall_ms();

        // The pipeline holds its last peak while no frames arrive, so a dead source would freeze
        // the bars at whatever was playing. No fresh frame means no level.
        let mut raw = h.level();
        if stall_ms.map_or(true, |ms| ms > 300) {
            raw = [0.0; 2];
        }
        let dt = self.meter_at.elapsed().as_secs_f32().min(0.5);
        self.meter_at = Instant::now();
        for c in 0..2 {
            self.meter[c] = raw[c].max(self.meter[c] - dt * 1.5).max(0.0);
        }
        // How long a local source has been silent. A cast has no level to judge by.
        let silent = stream.cast_relay.is_none() && st.serving.is_some() && raw[0].max(raw[1]) == 0.0;
        self.silent_since = if silent { self.silent_since.or(Some(Instant::now())) } else { None };

        Live {
            busy: self.starting.load(Ordering::Relaxed),
            waiting_on_portal: st.waiting_on_portal,
            dummy: st.is_dummy(),
            serving: st.serving.as_ref().map(|_| st.device_label()),
            video: st.video,
            error: st.error.clone(),
            is_cast: stream.cast_relay.is_some(),
            cast_held,
            stalled: webserver::audio_stalled(&stream, cast_held),
            stall_s: stall_ms.unwrap_or(0) / 1000,
            video_stalled: h.video_expected.load(Ordering::Relaxed)
                && h.video_stall_ms().map(|ms| ms > webserver::STALL_MS).unwrap_or(false),
            capture_closed: h.capture_closed.load(Ordering::Relaxed),
            encoder_failed: h.video_encoder_failed.load(Ordering::Relaxed),
            clients_n: self.clients.load(Ordering::Relaxed),
            quiet: self.silent_since.is_some_and(|t| t.elapsed() > Duration::from_secs(5)),
        }
    }

    fn set_theme(&mut self, ctx: &egui::Context, light: bool) {
        LIGHT_THEME.store(light, Ordering::Relaxed);
        ctx.set_visuals(theme_visuals());
        if let Err(e) = crate::settings::set_many(&[("theme", if light { "light" } else { "dark" }.to_string())]) {
            tracing::debug!("could not save the theme: {e}");
        }
    }

    fn set_zoom(&mut self, ctx: &egui::Context, zoom: f32) {
        let zoom = (zoom.clamp(ZOOM_MIN, ZOOM_MAX) * 20.0).round() / 20.0;
        ctx.set_zoom_factor(zoom);
        self.zoom = zoom;
        if let Err(e) = crate::settings::set_many(&[("ui_zoom", format!("{zoom:.2}"))]) {
            tracing::debug!("could not save the text size: {e}");
        }
    }

    // ===== header ====================================================================

    fn ui_header(&mut self, ui: &mut egui::Ui, live: &Live) {
        ui.horizontal(|ui| {
            ui.label(RichText::new("Newfoundsync").size(18.0).strong().color(c_text()));
            ui.add_space(2.0);
            let (text, fg, bg) = match chip_for(live) {
                Chip::Starting => ("Starting…".to_string(), c_accent_hi(), c_accent_bg()),
                Chip::Stopped => ("Stopped".to_string(), c_err(), c_err_bg()),
                Chip::SoundStopped => ("Sound stopped".to_string(), c_err(), c_err_bg()),
                Chip::WaitingForCast => ("Waiting for a cast".to_string(), c_text2(), c_surface_alt()),
                Chip::NoOutput => ("No output device".to_string(), c_warn(), c_warn_bg()),
                Chip::Ready => ("Ready · no devices".to_string(), c_text2(), c_surface_alt()),
                Chip::Quiet => (
                    format!("Nothing playing · {}", plural(live.clients_n, "device")),
                    c_text2(),
                    c_surface_alt(),
                ),
                Chip::Live => (format!("Live · {}", plural(live.clients_n, "device")), c_ok(), c_ok_bg()),
            };
            pill(ui, &text, fg, bg);
            ui.add_space(4.0);

            let levels = if live.busy || live.stalled { [0.0; 2] } else { self.meter };
            let caption = if live.is_cast {
                if live.cast_held {
                    "cast audio arriving"
                } else {
                    "no cast yet"
                }
            } else if live.stalled || live.serving.is_none() {
                "no sound"
            } else if levels[0].max(levels[1]) > 0.0 {
                "sound is flowing"
            } else {
                "silence"
            };
            let hint = "The level of the sound going into the stream, left channel over right. \
                        Flat bars with the source playing means the sound is not reaching the server. \
                        A cast is relayed without being decoded, so it has no level to show.";
            meter(ui, levels).on_hover_text(hint);
            // The caption is the first thing to go in a narrow window; the bars say the same.
            if ui.available_width() > 330.0 {
                ui.label(small(caption)).on_hover_text(hint);
            }

            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                // Right-to-left: the FIRST widget sits rightmost.
                if ui
                    .add(egui::Button::new("Settings").selected(self.view == MainView::Settings))
                    .on_hover_text("Port, theme, text size and build details")
                    .clicked()
                {
                    self.view = if self.view == MainView::Settings { MainView::Devices } else { MainView::Settings };
                }
                if ui
                    .add(egui::Button::new("Add a device").selected(self.view == MainView::AddDevice))
                    .on_hover_text("Show the address and QR code another device opens")
                    .clicked()
                {
                    self.view = if self.view == MainView::AddDevice { MainView::Devices } else { MainView::AddDevice };
                }
            });
        });
    }

    // ===== banners ===================================================================

    /// Whether anything needs saying under the header this frame.
    fn has_alerts(&self, live: &Live) -> bool {
        (live.stalled && !live.busy)
            || live.dummy
            || live.video_stalled
            || live.capture_closed
            || live.encoder_failed
            || live.error.as_ref().is_some_and(|e| self.dismissed_error.as_ref() != Some(e))
    }

    fn ui_alerts(&mut self, ui: &mut egui::Ui, live: &Live) {
        ui.spacing_mut().item_spacing.y = 0.0;
        if live.stalled && !live.busy {
            let listeners = if live.clients_n == 0 {
                "No devices are connected.".to_string()
            } else if live.clients_n == 1 {
                "1 device is still connected and hears silence.".to_string()
            } else {
                format!("{} devices are still connected and hear silence.", live.clients_n)
            };
            let body = if live.is_cast {
                format!("The casting device stopped sending audio {} s ago. {listeners}", live.stall_s)
            } else {
                format!("The source stopped producing audio {} s ago. {listeners}", live.stall_s)
            };
            let caster = self.cast.lock().ok().and_then(|c| *c);
            let offer_all = !live.is_cast && self.source != SourceKind::AllApps;
            let has_actions = offer_all || (live.is_cast && caster.is_some());
            banner(ui, c_err(), c_err_bg(), "No sound is reaching the stream.", &body, has_actions, |ui| {
                if offer_all && ui.button("Share all apps instead").clicked() {
                    self.pick_local(SourceKind::AllApps);
                    self.apply();
                }
                if let (true, Some(cid)) = (live.is_cast, caster) {
                    if ui
                        .button("Stop the cast")
                        .on_hover_text("Free the cast slot so another device can take it.")
                        .clicked()
                    {
                        self.stop_cast(cid);
                    }
                }
            });
        }
        if live.dummy {
            banner(
                ui,
                c_warn(),
                c_warn_bg(),
                "This PC has no real audio output.",
                "It is using the Dummy Output, so nothing is audible here and the stream carries only \
                 what apps play into that device: silence whenever nothing does. This is usual in a \
                 virtual machine, or when every sound card profile is switched off. Turn an output \
                 on in your system sound settings, then restart the stream.",
                true,
                |ui| {
                    if ui
                        .button("Restart the stream")
                        .on_hover_text(
                            "If `pactl list cards` shows a card, `pactl set-card-profile <card> \
                             output:analog-stereo` switches its output on.",
                        )
                        .clicked()
                    {
                        self.apply();
                    }
                },
            );
        }
        if live.capture_closed {
            banner(
                ui,
                c_warn(),
                c_warn_bg(),
                "Screen sharing was closed.",
                "The shared window or display is gone, or access was revoked, so devices see a frozen \
                 picture. Choose the screen again and press Apply.",
                false,
                |_| {},
            );
        } else if live.video_stalled {
            banner(
                ui,
                c_warn(),
                c_warn_bg(),
                "The picture has stopped.",
                "The video encoder was producing and has stopped, so devices see a frozen picture. \
                 Sound is not affected.",
                false,
                |_| {},
            );
        }
        if live.encoder_failed {
            banner(
                ui,
                c_warn(),
                c_warn_bg(),
                "The video encoder could not start.",
                "Devices get sound only. Try the other codec, or CPU encoding, under Video options.",
                false,
                |_| {},
            );
        }
        if let Some(err) = live.error.as_ref().filter(|e| self.dismissed_error.as_ref() != Some(*e)) {
            banner(ui, c_err(), c_err_bg(), "That did not work.", err, true, |ui| {
                if ui.button("Copy details").clicked() {
                    ui.ctx().copy_text(err.clone());
                    self.say("Copied.");
                }
                if ui.button("Dismiss").clicked() {
                    self.dismissed_error = Some(err.clone());
                }
            });
        }
    }

    // ===== sharing column ============================================================

    fn ui_sharing(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical().id_salt("sharing").auto_shrink([false, false]).show(ui, |ui| {
            // Keep the content clear of the scroll bar when the column does scroll.
            ui.set_width(ui.available_width() - 8.0);
            eyebrow(ui, "SHARING");
            ui.add_space(2.0);

            // One question first: is the sound from this computer, or sent up by a device? The two
            // used to be mixed into one audio list and one video list that silently changed each
            // other ("Web client cast" appeared in both).
            let cast = self.source == SourceKind::WebUplink;
            ui.columns(2, |c| {
                if seg(&mut c[0], !cast, "This computer", 28.0).clicked() {
                    self.set_cast_mode(false);
                }
                if seg(&mut c[1], cast, "A device casts", 28.0)
                    .on_hover_text("A connected device becomes the source by pressing Cast.")
                    .clicked()
                {
                    self.set_cast_mode(true);
                }
            });
            ui.add_space(6.0);

            if self.source == SourceKind::WebUplink {
                self.ui_cast_source(ui);
            } else {
                self.ui_local_source(ui);
            }

            if self.video_kind != VideoSourceKind::Off {
                ui.add_space(4.0);
                self.ui_video_options(ui);
            }

            ui.add_space(12.0);
            eyebrow(ui, "DELAY");
            ui.add_space(2.0);
            // Presets matched to this app's whole-home philosophy (multi-second buffers for
            // dropout immunity), with the raw ms slider beneath for fine control.
            ui.columns(3, |c| {
                for (i, (name, secs, ms)) in
                    [("Snappy", "1 s", 1000), ("Balanced", "3 s", 3000), ("Rock-solid", "6 s", 6000)].into_iter().enumerate()
                {
                    if seg2(&mut c[i], self.buffer_ms == ms, name, secs).clicked() {
                        self.buffer_ms = ms;
                    }
                }
            });
            ui.spacing_mut().slider_width = (ui.available_width() - 84.0).max(80.0);
            ui.add(
                egui::Slider::new(&mut self.buffer_ms, (config::MIN_BUFFER_MS as i32)..=(config::MAX_BUFFER_MS as i32))
                    .suffix(" ms"),
            )
            .on_hover_text(
                "How far behind the source every device plays. Lost Wi-Fi packets are re-sent and \
                 this buffer hides the stall: bigger rides out worse Wi-Fi but adds delay. It is \
                 the same on every device, which is what keeps them in step.",
            );

            ui.add_space(12.0);
            eyebrow(ui, "SOUND QUALITY");
            ui.add_space(2.0);
            egui::ComboBox::from_id_salt("quality")
                .selected_text(self.quality_label())
                .width(ui.available_width() - 8.0)
                .show_ui(ui, |ui| {
                    for (label, codec, bitrate) in QUALITY_PRESETS {
                        let on = self.codec == codec && (codec == CodecKind::Pcm || self.bitrate == bitrate);
                        if ui.selectable_label(on, label).clicked() {
                            self.codec = codec;
                            if codec == CodecKind::Opus {
                                self.bitrate = bitrate;
                            }
                        }
                    }
                });
            ui.add_space(8.0);
        });
    }

    /// "This computer": which of its sounds, and whether its screen goes along.
    fn ui_local_source(&mut self, ui: &mut egui::Ui) {
        // What each source means differs per platform, so the descriptions do too.
        #[cfg(target_os = "windows")]
        const ALL_APPS: &str = "Everything except this app. Keeps playing when Windows is muted.";
        #[cfg(target_os = "linux")]
        const ALL_APPS: &str = "Everything this computer plays.";
        #[cfg(not(any(target_os = "windows", target_os = "linux")))]
        const ALL_APPS: &str = "Everything this Mac plays.";
        #[cfg(target_os = "windows")]
        const SYSTEM: &str = "Mirrors the speakers. Goes silent when Windows is muted.";
        #[cfg(not(target_os = "windows"))]
        const SYSTEM: &str = "Mirrors the speakers and follows system mute.";

        if option_row(ui, self.source == SourceKind::AllApps, "All apps", ALL_APPS, |ui| {
            #[cfg(target_os = "macos")]
            self.ui_silent_toggle(ui);
            #[cfg(not(target_os = "macos"))]
            let _ = ui;
        }) {
            self.pick_local(SourceKind::AllApps);
        }

        #[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
        {
            // The wording differs per platform because the underlying thing differs. Windows lists
            // windows and audio sessions, so "one window or app" is honest. Linux can only see
            // streams that are playing, so promising "window" there would be a lie the operator
            // discovers only after picking something.
            #[cfg(target_os = "windows")]
            const ONE_APP: (&str, &str) = ("One window or app", "Only the window or app you pick.");
            #[cfg(target_os = "linux")]
            const ONE_APP: (&str, &str) = ("One app", "Only that app. It appears once it is playing.");
            // macOS lists every app that has opened audio, playing or not; helper processes
            // (browser audio, WebKit) are folded into the app that owns them.
            #[cfg(target_os = "macos")]
            const ONE_APP: (&str, &str) = ("One app", "Only the app you pick. It appears once it has played sound.");
            let selected = self.source == SourceKind::App;
            if option_row(ui, selected, ONE_APP.0, ONE_APP.1, |ui| {
                self.ui_app_picker(ui);
                #[cfg(target_os = "macos")]
                self.ui_silent_toggle(ui);
            }) {
                self.pick_local(SourceKind::App);
                // Arriving at the per-app source: enumerate now, so the list is current the first
                // time it is opened rather than whatever was cached at startup.
                self.refresh_apps();
            }
        }

        if option_row(ui, self.source == SourceKind::System, "Speaker output", SYSTEM, |_| {}) {
            self.pick_local(SourceKind::System);
        }

        // Linux only: the device is a PipeWire/PulseAudio sink, and no other platform's capture
        // can host one. Not shown disabled elsewhere: on Windows the all-apps and per-app sources
        // keep streaming with the output muted, which already answers the same need.
        #[cfg(target_os = "linux")]
        {
            let selected = self.source == SourceKind::VirtualSink;
            let picked = option_row(
                ui,
                selected,
                "Newfoundsync output device",
                "Apps you send to it are streamed, and silent on this PC.",
                |ui| self.ui_app_routing(ui),
            );
            if picked {
                self.pick_local(SourceKind::VirtualSink);
            }
        }

        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.label("Screen");
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if LOCAL_SCREEN {
                    let text = match self.video_kind {
                        VideoSourceKind::Screen => "This screen",
                        VideoSourceKind::Window => "One window",
                        _ => "Off",
                    };
                    egui::ComboBox::from_id_salt("screen").selected_text(text).width(130.0).show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.video_kind, VideoSourceKind::Off, "Off");
                        ui.selectable_value(&mut self.video_kind, VideoSourceKind::Screen, "This screen");
                        #[cfg(target_os = "windows")]
                        ui.selectable_value(&mut self.video_kind, VideoSourceKind::Window, "One window");
                    });
                } else {
                    // Offering it anyway used to mean picking "Whole screen", pressing Apply, and
                    // silently getting audio — the stream looked healthy and devices never saw a
                    // picture. Say it is not there, and what to do instead.
                    ui.label(small("not in this build")).on_hover_text(
                        "Capturing this machine's screen is not built into this server. To share a \
                         screen, choose \"A device casts\" and cast from a browser.",
                    );
                }
            });
        });
        self.last_local_video = self.video_kind;
        #[cfg(target_os = "windows")]
        if self.video_kind == VideoSourceKind::Window {
            self.ui_window_picker(ui);
        }
    }

    /// "A device casts": nothing to pick but whether its screen is relayed too.
    fn ui_cast_source(&mut self, ui: &mut egui::Ui) {
        ui.add(
            egui::Label::new(small(
                "A connected device presses Cast and becomes the source. Nothing plays until one does.",
            ))
            .wrap(),
        );
        ui.add_space(4.0);
        let mut relay = self.video_kind == VideoSourceKind::WebCast;
        if ui
            .checkbox(&mut relay, "Relay its screen as well")
            .on_hover_text(
                "The device casts its screen with its audio, and the server sends both to everyone \
                 at the video options below.",
            )
            .changed()
        {
            self.video_kind = if relay { VideoSourceKind::WebCast } else { VideoSourceKind::Off };
        }
    }

    /// "Silent on this Mac" under All apps and One app: the capture's process tap mutes what it
    /// captures, so the Mac does not play it a whole buffer ahead of every synced device. The
    /// macOS counterpart of Linux's "Newfoundsync output device".
    #[cfg(target_os = "macos")]
    fn ui_silent_toggle(&mut self, ui: &mut egui::Ui) {
        ui.checkbox(&mut self.silent_local, "Silent on this Mac").on_hover_text(
            "Mutes what is being streamed on this Mac's own speakers while it streams, so you only \
             hear it in sync on your devices. The sound comes back when you stop or switch source.",
        );
    }

    /// The app list under "One app": a picker and a refresh button.
    #[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
    fn ui_app_picker(&mut self, ui: &mut egui::Ui) {
        let apps = self.apps.clone();
        ui.horizontal(|ui| {
            let label = if self.selected_pid.is_some() { self.selected_name.clone() } else { "Choose…".to_string() };
            let combo = egui::ComboBox::from_id_salt("app_pick")
                .selected_text(label)
                .width((ui.available_width() - 44.0).max(80.0))
                .show_ui(ui, |ui| {
                    for a in &apps {
                        if ui.selectable_label(self.selected_pid == Some(a.pid), &a.name).clicked() {
                            self.selected_pid = Some(a.pid);
                            self.selected_name = a.name.clone();
                        }
                    }
                    if apps.is_empty() {
                        // The empty state has to explain itself on Linux: an empty list is the
                        // NORMAL state when nothing happens to be playing, not a failure to
                        // enumerate, and "press refresh" would be useless advice.
                        #[cfg(target_os = "windows")]
                        ui.label("No windows or apps found. Press refresh.");
                        #[cfg(target_os = "linux")]
                        ui.label("Nothing is playing right now. Start playback and it appears here.");
                        #[cfg(target_os = "macos")]
                        ui.label("No app has played sound yet. Start playback, then refresh.");
                    }
                });
            // Opening the picker enumerates. With no background scan, this is what keeps the list
            // honest: what you are about to choose from was just read, not cached from whenever
            // the window last happened to look.
            if combo.response.clicked() {
                self.refresh_apps();
            }
            if ui
                .add_enabled(!self.refreshing, egui::Button::new("⟳"))
                .on_hover_text("Refresh the list")
                .clicked()
            {
                self.refresh_apps();
            }
        });
    }

    /// The checkboxes under the "Newfoundsync" output device source: which apps play into it.
    #[cfg(target_os = "linux")]
    fn ui_app_routing(&mut self, ui: &mut egui::Ui) {
        if !self.routing.error.is_empty() {
            ui.add(
                egui::Label::new(
                    RichText::new(format!("Could not reach the sound server: {}", self.routing.error))
                        .size(12.0)
                        .color(c_err()),
                )
                .wrap(),
            );
        }
        let Some(snap) = self.routing.snapshot.clone() else {
            return; // the first look is still in flight
        };
        let remembered: Vec<String> = self
            .routing
            .wanted
            .iter()
            .filter(|k| !snap.apps.iter().any(|a| a.key == **k))
            .cloned()
            .collect();
        if !snap.device_live {
            ui.add(egui::Label::new(small("Press Apply to create the device, then tick the apps to send to it.")).wrap());
            if !self.routing.wanted.is_empty() {
                let names: Vec<&str> = self.routing.wanted.iter().map(|s| s.as_str()).collect();
                ui.add(egui::Label::new(small(format!("Remembered from last time: {}", names.join(", ")))).wrap());
            }
            return;
        }
        let mut toggled: Option<(String, bool)> = None;
        for app in &snap.apps {
            // An app someone sent here with the system mixer is shown ticked as well: the box says
            // where the app plays, and unticking it has to work either way.
            let mut on = app.on_device || self.routing.wanted.contains(&app.key);
            if ui.checkbox(&mut on, app.label.as_str()).changed() {
                toggled = Some((app.key.clone(), on));
            }
        }
        for key in &remembered {
            // Ticked earlier, not playing now. Kept visible so it can be unticked — otherwise the
            // only way to stop an app being sent here would be to catch it mid-song.
            let mut on = true;
            if ui
                .checkbox(&mut on, RichText::new(format!("{key}  (not playing)")).color(c_dim()))
                .on_hover_text("Remembered: it is sent here as soon as it plays.")
                .changed()
            {
                toggled = Some((key.clone(), false));
            }
        }
        if snap.apps.is_empty() && remembered.is_empty() {
            ui.add(egui::Label::new(small("Nothing is playing right now. Press play in your app and it appears here.")).wrap());
        }
        if let Some(problem) = &snap.problem {
            ui.add(egui::Label::new(RichText::new(problem).size(12.0).color(c_err())).wrap());
        }
        ui.add(
            egui::Label::new(small(
                "To hear it on this PC in sync, open the address under Add a device in a browser \
                 here. Don't tick that browser, and keep your real speakers as the default output: \
                 either one echoes.",
            ))
            .wrap(),
        );
        if let Some((key, on)) = toggled {
            self.set_app_routed(&key, on);
        }
    }

    /// The window list under "Screen: One window" (Windows only — per-window capture is WGC).
    #[cfg(target_os = "windows")]
    fn ui_window_picker(&mut self, ui: &mut egui::Ui) {
        let windows: Vec<AudioApp> = self.apps.iter().filter(|a| a.hwnd.is_some()).cloned().collect();
        ui.horizontal(|ui| {
            let label = if self.video_hwnd.is_some() && !self.video_name.is_empty() {
                self.video_name.clone()
            } else {
                "Choose a window…".to_string()
            };
            let combo = egui::ComboBox::from_id_salt("vid_pick")
                .selected_text(label)
                .width((ui.available_width() - 44.0).max(80.0))
                .show_ui(ui, |ui| {
                    for a in &windows {
                        if ui.selectable_label(self.video_hwnd == a.hwnd, &a.name).clicked() {
                            self.video_pid = Some(a.pid);
                            self.video_name = a.name.clone();
                            self.video_hwnd = a.hwnd;
                            // Sharing one window shares that window's sound with it.
                            self.source = SourceKind::App;
                            self.last_local_source = SourceKind::App;
                            self.selected_pid = Some(a.pid);
                            self.selected_name = a.name.clone();
                        }
                    }
                    if windows.is_empty() {
                        ui.label("No windows found. Press refresh.");
                    }
                });
            if combo.response.clicked() {
                self.refresh_apps();
            }
            if ui
                .add_enabled(!self.refreshing, egui::Button::new("⟳"))
                .on_hover_text("Refresh the list")
                .clicked()
            {
                self.refresh_apps();
            }
        });
    }

    /// Resolution, codec, where it encodes, quality — and the live A/V offset.
    fn ui_video_options(&mut self, ui: &mut egui::Ui) {
        egui::CollapsingHeader::new(RichText::new("Video options").color(c_text2()))
            .id_salt("vid_adv")
            .default_open(false)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label("Resolution");
                    egui::ComboBox::from_id_salt("res").selected_text(RES_LABELS[self.res_idx].0).show_ui(ui, |ui| {
                        for (i, (label, _)) in RES_LABELS.iter().enumerate() {
                            ui.selectable_value(&mut self.res_idx, i, *label);
                        }
                    });
                    ui.checkbox(&mut self.fps60, "60 fps");
                });
                ui.horizontal(|ui| {
                    ui.label("Codec").on_hover_text(
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
                            ui.selectable_value(&mut self.enc_idx, 0, ENC_LABELS[0].0).on_hover_text(AV1_HINT);
                            if vp9_available {
                                ui.selectable_value(&mut self.enc_idx, 1, ENC_LABELS[1].0).on_hover_text(VP9_HINT);
                            } else {
                                ui.add_enabled(false, egui::Button::new("VP9 · not in this build"))
                                    .on_disabled_hover_text(
                                        "This build was compiled without the `vp9` feature (it links \
                                         libvpx via vcpkg). Rebuild with --features vp9 to enable it.",
                                    );
                            }
                        });
                });
                // WHERE video encodes, as opposed to WHICH codec above. Worth exposing because the
                // right answer is machine-specific and neither choice is universally better: the
                // GPU encoder costs almost no CPU but some hardware encoders are lower quality per
                // bitrate (or, on this project's own bug reports, flaky), while SVT-AV1 gives up
                // CPU headroom that a busy server may not have.
                ui.horizontal(|ui| {
                    ui.label("Encode on").on_hover_text(
                        "Auto tries the GPU and quietly falls back to CPU — what this has always \
                         done. GPU only refuses to start video if the GPU has no usable AV1 encoder, \
                         instead of spending the CPU you were trying to protect. CPU only never \
                         touches the GPU, which is the way out of a flaky hardware encoder.",
                    );
                    // The screencast Flatpak has VA-API/NVENC backends; the ordinary Linux Flatpak
                    // intentionally does not, so only expose a choice where it works.
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
                                if ui.selectable_value(&mut self.encode_device, d, label).clicked() {
                                    // Persist immediately rather than on Apply. Apply rebuilds the
                                    // stream and reconnects every listener; the operator reaching
                                    // for this is usually mid-diagnosis of a bad encoder and
                                    // shouldn't lose the setting to a crash before they get around
                                    // to applying it.
                                    if let Err(e) = crate::settings::save_encode_device(d) {
                                        tracing::warn!("couldn't save encode device: {e}");
                                    }
                                }
                            }
                        });
                    #[cfg(not(any(target_os = "windows", all(target_os = "linux", feature = "linux-hw-encode"))))]
                    ui.label(small("CPU (SVT-AV1)")).on_hover_text(
                        "Not a missing feature — a hardware one. Most GPUs can DECODE AV1/VP9 but \
                         only encode H.264/HEVC, which this app doesn't ship. Hardware AV1 encoding \
                         needs Intel Arc / Meteor Lake or newer, AMD RX 7000 or newer, or an RTX \
                         40-series; no NVIDIA GPU has ever encoded VP9. On Windows the GPU path \
                         exists because Media Foundation exposes an AV1 encoder where the hardware \
                         has one.",
                    );
                });
                ui.horizontal(|ui| {
                    ui.label("Quality");
                    ui.spacing_mut().slider_width = (ui.available_width() - 70.0).max(60.0);
                    ui.add(egui::Slider::new(&mut self.video_quality_pct, 40..=250).suffix("%"));
                });
                let res = Resolution::parse(RES_LABELS[self.res_idx].1).unwrap_or(Resolution::P1080);
                let est = VideoConfig {
                    resolution: res,
                    fps: if self.fps60 { Fps::F60 } else { Fps::F30 },
                    quality_pct: self.video_quality_pct,
                }
                .suggested_bitrate_kbps();
                ui.add(
                    egui::Label::new(small(format!(
                        "About {:.1} Mbps. Lower it for weaker hardware or Wi-Fi, raise it for a sharper picture.",
                        est as f32 / 1000.0
                    )))
                    .wrap(),
                );
            });

        // A/V OFFSET — live, deliberately NOT Apply-gated.
        //
        // Judging lip-sync needs continuous playback, so a control you can only change by
        // restarting the stream is a control you cannot actually tune. This writes straight to
        // the atomic the audio producer reads per frame.
        //
        // Only meaningful with video on, so it is hidden when video is off rather than sitting
        // there inviting a change that does nothing.
        ui.add_space(2.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("A/V offset").color(c_text2())).on_hover_text(
                "Shifts AUDIO against video, for everyone, instantly. Drag right if you hear the \
                 sound BEFORE the lips move; left if the sound lags the picture. It is needed \
                 because a player writes its audio to the speakers ahead of time, so the sound we \
                 capture belongs with a picture slightly later than the one we capture with it — \
                 and nothing on the system reports that gap.",
            );
            let mut ms = self.av_offset_ms;
            ui.spacing_mut().slider_width = (ui.available_width() - 104.0).max(60.0);
            let resp = ui.add(egui::Slider::new(&mut ms, -500..=500).show_value(false));
            // The LIVE value has to move on every change — that is the whole point of the
            // control, you tune it by ear against a running stream.
            if resp.changed() {
                self.apply_av_offset(ms);
            }
            // Saving is a different question. save_key() reads the whole settings file, writes a
            // temp copy and renames it, and `changed()` fires on every mouse-move of a drag — so
            // persisting there did that dance ~60 times a second for the length of a drag. Once
            // the drag ends is exactly as durable and costs one write.
            if resp.drag_stopped() || (resp.changed() && !resp.dragged()) {
                self.save_av_offset();
            }
            ui.label(RichText::new(format!("{:>+4} ms", self.av_offset_ms)).monospace().color(c_dim()));
            if ui.button("⟲").on_hover_text("Back to 0 ms").clicked() {
                self.apply_av_offset(0);
                self.save_av_offset();
            }
        });
        ui.add(egui::Label::new(small("Applies at once. Positive delays the sound, for sound that arrives before the picture.")).wrap());
    }

    // ===== devices ===================================================================

    fn ui_devices(&mut self, ui: &mut egui::Ui, rows: &[ClientRow], live: &Live) {
        let n_identified = rows.iter().filter(|r| r.identified).count();
        ui.horizontal(|ui| {
            eyebrow(ui, "DEVICES");
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if self.calibrating {
                    if ui.button("Stop calibration").clicked() {
                        self.stop_calibrate_all(true);
                    }
                    ui.label(RichText::new("Aligning all devices…").size(12.0).color(c_accent_hi()));
                } else {
                    // The undo for "Calibrate all". Enabled whenever anything is connected — unlike
                    // calibration it needs no mics, no second device, and no room: it is the way
                    // back to a known state when an alignment went wrong.
                    if ui
                        .add_enabled(n_identified > 0, egui::Button::new("Reset sync"))
                        .on_hover_text(
                            "Reset every connected device's sync to 0 ms — clears the server \
                             offsets and tells each device to drop its own calibration trim.",
                        )
                        .on_disabled_hover_text("No devices connected.")
                        .clicked()
                    {
                        self.reset_sync(None);
                    }
                    // Always shown so the feature is discoverable; it only ENABLES once there are
                    // 2+ devices to align. (A lone device self-calibrates from its own browser.)
                    if ui
                        .add_enabled(n_identified >= 2, egui::Button::new("Calibrate all"))
                        .on_hover_text(
                            "The earliest-connected device plays a sync code; the rest listen on \
                             their mics and align at once. Devices must be in the same room with \
                             working mics.",
                        )
                        .on_disabled_hover_text(
                            "Connect 2 or more devices (same room, working mics) to align them. A \
                             single device calibrates itself from its own browser.",
                        )
                        .clicked()
                        && !self.start_calibrate_all()
                    {
                        self.say("Need at least two connected devices to calibrate.");
                    }
                }
            });
        });
        ui.add_space(2.0);
        card(ui, |ui| self.ui_master(ui));
        ui.add_space(6.0);

        let caster: Option<u64> = self.cast.lock().ok().and_then(|s| *s);
        // If the client being renamed vanished, don't strand the half-typed buffer: commit it
        // (keyed by stable id) and clear the edit state.
        if let Some(ed) = self.editing_client.clone() {
            if !rows.iter().any(|r| r.identified && r.stable_id == ed) {
                let nm = self.edit_name_buf.trim().to_string();
                if !nm.is_empty() {
                    self.client_names.insert(ed, nm);
                    self.devices_dirty = true;
                }
                self.editing_client = None;
                self.edit_name_buf.clear();
            }
        }

        // Remembered devices that are not connected now, most recently seen first.
        let now = unix_now();
        let mut offline: Vec<(String, String, u64)> = self
            .known
            .iter()
            .filter(|(id, k)| {
                !rows.iter().any(|r| r.identified && &r.stable_id == *id)
                    && now.saturating_sub(k.last_seen) < 30 * 86_400
            })
            .map(|(id, k)| {
                let name = self.client_names.get(id).cloned().unwrap_or_else(|| k.name.clone());
                (id.clone(), name, now.saturating_sub(k.last_seen))
            })
            .collect();
        offline.sort_by(|a, b| a.2.cmp(&b.2).then_with(|| a.1.cmp(&b.1)));
        offline.truncate(8);

        egui::ScrollArea::vertical().id_salt("devices").auto_shrink([false, false]).show(ui, |ui| {
            ui.set_width(ui.available_width() - 4.0);
            if rows.is_empty() {
                ui.add_space(6.0);
                ui.label(small("No devices are connected right now."));
                ui.add_space(6.0);
            }
            for r in rows {
                if !r.identified {
                    ui.label(RichText::new("A device is connecting…").italics().color(c_dim()));
                    ui.separator();
                    continue;
                }
                self.ui_device_row(ui, r, live, caster);
                ui.separator();
            }
            for (id, name, secs) in &offline {
                ui.push_id(("offline", id), |ui| {
                    ui.horizontal(|ui| {
                        ui.vertical(|ui| {
                            ui.spacing_mut().item_spacing.y = 1.0;
                            ui.label(RichText::new(name).color(c_dim()));
                            ui.label(small(format!("Offline · last seen {} ago", ago(*secs))));
                        });
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if ui
                                .button("Forget")
                                .on_hover_text("Forget this device's name, volume and sync. It is remembered again when it reconnects.")
                                .clicked()
                            {
                                self.forget_device(id);
                            }
                        });
                    });
                });
                ui.separator();
            }
        });
    }

    /// The master volume: one control that scales every device, and mutes the house.
    fn ui_master(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(RichText::new("Master").strong().color(c_text()));
            if ui
                .add(egui::Button::new(if self.master_muted { "🔇" } else { "🔊" }).selected(self.master_muted))
                .on_hover_text(if self.master_muted {
                    "Unmute everyone (restores the master level)"
                } else {
                    "Mute everyone instantly — the master level is kept"
                })
                .clicked()
            {
                self.master_muted = !self.master_muted;
            }
            // Fills whatever the window leaves, re-measured every frame, so the bar tracks a
            // resize instead of sitting at a fixed length. Only the readout is reserved.
            ui.spacing_mut().slider_rail_height = 10.0;
            ui.spacing_mut().slider_width = (ui.available_width() - PCT_W - 12.0).max(80.0);
            let resp = ui
                .add(egui::Slider::new(&mut self.master_vol, 0.0..=1.0).show_value(false))
                .on_hover_text("Scales every device's volume — applies instantly");
            if resp.drag_stopped() || (resp.changed() && !resp.dragged()) {
                let _ = crate::settings::set_many(&[("master_vol", format!("{:.3}", self.master_vol))]);
            }
            ui.label(
                RichText::new(if self.master_muted {
                    " off".to_string()
                } else {
                    format!("{:>3}%", (self.master_vol * 100.0).round() as i32)
                })
                .monospace()
                .color(c_text2()),
            );
        });
    }

    /// One connected device: its name and health, then mute, volume and sync.
    fn ui_device_row(&mut self, ui: &mut egui::Ui, r: &ClientRow, live: &Live, caster: Option<u64>) {
        let id = r.stable_id.clone();
        // Salt each row by stable id so a mid-list disconnect can't hand an in-flight slider drag /
        // rename focus to a different client.
        ui.push_id(&id, |ui| {
            let display = self.display_name(r);
            let muted = self.client_muted.get(&id).copied().unwrap_or(false);
            let casting = caster == Some(r.conn_id);
            let dropping = self
                .drops
                .get(&r.conn_id)
                .and_then(|d| d.1)
                .is_some_and(|t| t.elapsed() < Duration::from_secs(10));

            // What to say about it, worst first. The server can vouch for "connected" and for
            // frames it failed to deliver; it cannot see whether the device is in sync.
            let (dot, tone, note): (Color32, Color32, String) = if live.stalled {
                (c_err(), c_err(), "Connected · hearing silence".into())
            } else if dropping {
                (c_warn(), c_warn(), format!("Falling behind · {} frames dropped", r.dropped))
            } else if casting {
                (c_accent(), c_accent_hi(), "Casting to everyone".into())
            } else if !r.calib_status.is_empty() {
                let ok = r.calib_status.contains("aligned") || r.calib_status == "reference";
                (if ok { c_ok() } else { c_accent() }, c_dim(), format!("Calibration: {}", r.calib_status))
            } else if muted || self.master_muted {
                (c_dim(), c_dim(), "Muted here".into())
            } else if r.dropped > 0 {
                (c_ok(), c_dim(), format!("Connected · {} frames dropped earlier", r.dropped))
            } else {
                (c_ok(), c_dim(), "Connected".into())
            };

            let name_w = (ui.available_width() * 0.34).clamp(130.0, 250.0);
            ui.horizontal(|ui| {
                ui.allocate_ui_with_layout(egui::vec2(name_w, ROW_H), Layout::top_down(Align::Min), |ui| {
                    ui.set_min_width(name_w);
                    ui.spacing_mut().item_spacing.y = 1.0;
                    if self.editing_client.as_deref() == Some(id.as_str()) {
                        let resp = ui.add(
                            egui::TextEdit::singleline(&mut self.edit_name_buf)
                                .desired_width(name_w - 8.0)
                                .hint_text("name this device"),
                        );
                        if !resp.has_focus() && !resp.lost_focus() {
                            resp.request_focus();
                        }
                        if resp.lost_focus() {
                            let nm = self.edit_name_buf.trim().to_string();
                            // Cleared, or left as what the device calls itself: no override. (An
                            // override equal to the device's own name would pin it, and a later
                            // rename made ON the device would never show here.)
                            if nm.is_empty() || nm == r.name.trim() {
                                self.client_names.remove(&id);
                            } else {
                                self.client_names.insert(id.clone(), nm);
                            }
                            self.editing_client = None;
                            self.devices_dirty = true;
                        }
                    } else {
                        let resp = ui
                            .add(
                                egui::Label::new(RichText::new(&display).strong().color(c_text()))
                                    .truncate()
                                    .sense(egui::Sense::click()),
                            )
                            .on_hover_text("Double-click to rename");
                        if resp.double_clicked() {
                            self.edit_name_buf = display.clone();
                            self.editing_client = Some(id.clone());
                        }
                    }
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 5.0;
                        ui.spacing_mut().interact_size.y = 14.0;
                        let (d, _) = ui.allocate_exact_size(egui::vec2(8.0, 14.0), egui::Sense::hover());
                        ui.painter().circle_filled(d.center(), 3.5, dot);
                        ui.add(egui::Label::new(RichText::new(&note).size(12.0).color(tone)).truncate());
                    });
                });

                if ui
                    .add(egui::Button::new(if muted { "🔇" } else { "🔊" }).selected(muted))
                    .on_hover_text("Mute / unmute this device")
                    .clicked()
                {
                    self.client_muted.insert(id.clone(), !muted);
                    self.devices_dirty = true;
                }
                // The slider takes what is left after the two fixed readouts, re-measured every
                // frame so it tracks a resize. The floor keeps it draggable in a narrow window.
                let gap = ui.spacing().item_spacing.x;
                ui.spacing_mut().slider_width = (ui.available_width() - PCT_W - SYNC_W - 3.0 * gap - 6.0).max(70.0);
                let v = self.client_vols.entry(id.clone()).or_insert(1.0);
                let resp = ui.add(egui::Slider::new(v, 0.0..=1.0).show_value(false));
                let pct = (*v * 100.0).round() as i32;
                if resp.changed() {
                    self.devices_dirty = true;
                }
                ui.add_sized(
                    [PCT_W, 20.0],
                    egui::Label::new(
                        RichText::new(if muted { "off".to_string() } else { format!("{pct}%") })
                            .monospace()
                            .color(c_text2()),
                    ),
                );

                // Show the device's ACTUAL effective sync (reported back = its own
                // calibration/slider + our pushed offset), so calibrated clients read their real,
                // differing offsets instead of the commanded 0. Falls back to the commanded value
                // until the first report arrives.
                let commanded = self.client_trims.get(&id).copied().unwrap_or(0);
                let shown = r.reported_trim.unwrap_or(commanded);
                let open = self.sync_open.as_deref() == Some(id.as_str());
                if ui
                    .add_sized(
                        [SYNC_W, 24.0],
                        egui::Button::new(RichText::new(format!("{shown:+} ms")).monospace().size(12.5)).selected(open),
                    )
                    .on_hover_text("Sync: how much later (+) or earlier (−) this device plays. Click to adjust.")
                    .clicked()
                {
                    self.sync_open = if open { None } else { Some(id.clone()) };
                }
            });

            if self.sync_open.as_deref() == Some(id.as_str()) {
                self.ui_sync_panel(ui, &id, r.reported_trim);
            }
            if casting {
                ui.horizontal(|ui| {
                    ui.label(small("This device is the source right now."));
                    if ui
                        .button("Stop its cast")
                        .on_hover_text("Kick this device off the cast slot so another one can claim it.")
                        .clicked()
                    {
                        self.stop_cast(r.conn_id);
                    }
                });
            }
        });
    }

    /// The sync controls for one device: nudges of 1 and 10 ms, and a reset.
    ///
    /// These replace a slider that covered 1000 ms in the width of a thumb — about 8 ms per pixel,
    /// for an adjustment that is judged in single milliseconds.
    fn ui_sync_panel(&mut self, ui: &mut egui::Ui, id: &str, reported: Option<i32>) {
        let commanded = self.client_trims.get(id).copied().unwrap_or(0);
        let mut nudge: Option<i32> = None;
        let mut reset = false;
        card(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(small("Sync"));
                for step in [-10, -1] {
                    if ui.button(format!("−{}", -step)).clicked() {
                        nudge = Some(step);
                    }
                }
                ui.add_sized(
                    [64.0, 20.0],
                    egui::Label::new(
                        RichText::new(format!("{:+} ms", reported.unwrap_or(commanded))).monospace().strong().color(c_text()),
                    ),
                );
                for step in [1, 10] {
                    if ui.button(format!("+{step}")).clicked() {
                        nudge = Some(step);
                    }
                }
                if ui
                    .button("Reset")
                    .on_hover_text(
                        "Back to 0 ms — clears BOTH the server offset and the device's own \
                         calibration trim.",
                    )
                    .clicked()
                {
                    reset = true;
                }
                let own = reported.map(|r| r - commanded).unwrap_or(0);
                ui.label(small(match (reported, own) {
                    (None, _) => "Positive plays later. The device has not reported its own value yet.".to_string(),
                    (Some(_), 0) => "Positive plays later.".to_string(),
                    (Some(_), own) => {
                        format!("Positive plays later. {commanded:+} ms set here, {own:+} ms from the device's own calibration.")
                    }
                }));
            });
        });
        if let Some(step) = nudge {
            self.client_trims.insert(id.to_string(), (commanded + step).clamp(-500, 500));
            self.devices_dirty = true;
        }
        if reset {
            self.reset_sync(Some(id));
        }
    }

    // ===== add a device / first run ==================================================

    /// The address, the QR code and the three steps. `first` is the version shown instead of an
    /// empty mixer when the server has never had a device.
    fn ui_add_device(&mut self, ui: &mut egui::Ui, first: bool) {
        let qr = self.qr_texture(ui.ctx());
        egui::ScrollArea::vertical().id_salt("add").auto_shrink([false, false]).show(ui, |ui| {
            ui.set_width(ui.available_width() - 4.0);
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(if first { "Connect your first device" } else { "Add a device" })
                        .size(18.0)
                        .strong()
                        .color(c_text()),
                );
                if !first {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.button("Back to devices").clicked() {
                            self.view = MainView::Devices;
                        }
                    });
                }
            });
            ui.add_space(8.0);
            let side = 196.0;
            // Side by side when there is room; stacked in a narrow window, where a horizontal row
            // would run off the edge instead of wrapping.
            let wide = ui.available_width() >= side + 300.0;
            let details = |this: &mut Self, ui: &mut egui::Ui| {
                ui.add(
                    egui::Label::new(RichText::new(&this.url).monospace().size(17.0).strong().color(c_accent_hi()))
                        .selectable(true)
                        .wrap(),
                );
                ui.horizontal(|ui| {
                    if ui.button("Copy address").clicked() {
                        ui.ctx().copy_text(this.url.clone());
                        this.say("Address copied.");
                    }
                    if ui.button("Open on this PC").on_hover_text("Open the address in your default browser").clicked() {
                        open_url(&this.url);
                    }
                });
                ui.add_space(6.0);
                for (n, step) in [
                    "Scan the code, or type the address into a browser on the same Wi-Fi.",
                    "Accept the security warning once (Advanced, then Proceed). It appears because \
                     this PC makes its own certificate, which browsers need before they allow playback.",
                    "Press Start. The device shows up in this window.",
                ]
                .iter()
                .enumerate()
                {
                    ui.horizontal_top(|ui| {
                        ui.label(RichText::new(format!("{}.", n + 1)).strong().color(c_text2()));
                        ui.add(egui::Label::new(RichText::new(*step).color(c_text2())).wrap());
                    });
                }
                if this.url.contains("<this-pc>") {
                    ui.add_space(4.0);
                    ui.add(
                        egui::Label::new(
                            RichText::new(
                                "This PC has no network address yet. Connect it to your network; the \
                                 address appears here on its own.",
                            )
                            .color(c_warn()),
                        )
                        .wrap(),
                    );
                }
            };
            let code = |ui: &mut egui::Ui| {
                if let Some(tex) = &qr {
                    ui.image(egui::load::SizedTexture::new(tex.id(), egui::vec2(side, side)));
                }
            };
            if wide {
                ui.horizontal_top(|ui| {
                    code(ui);
                    ui.add_space(10.0);
                    ui.vertical(|ui| details(self, ui));
                });
            } else {
                code(ui);
                ui.add_space(6.0);
                details(self, ui);
            }
        });
    }

    // ===== settings ==================================================================

    fn ui_settings(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical().id_salt("settings").auto_shrink([false, false]).show(ui, |ui| {
            ui.set_width(ui.available_width() - 4.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new("Settings").size(18.0).strong().color(c_text()));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.button("Back to devices").clicked() {
                        self.view = MainView::Devices;
                    }
                });
            });

            ui.add_space(10.0);
            eyebrow(ui, "APPEARANCE");
            ui.horizontal(|ui| {
                ui.label("Theme");
                let light = lt();
                if ui.add(egui::Button::new("Dark").selected(!light)).clicked() && light {
                    self.set_theme(ui.ctx(), false);
                }
                if ui.add(egui::Button::new("Light").selected(light)).clicked() && !light {
                    self.set_theme(ui.ctx(), true);
                }
            });
            ui.horizontal(|ui| {
                ui.label("Text size");
                if ui.add_enabled(self.zoom > ZOOM_MIN + 0.001, egui::Button::new("−")).on_hover_text("Smaller").clicked() {
                    self.set_zoom(ui.ctx(), self.zoom - ZOOM_STEP);
                }
                ui.label(RichText::new(format!("{:>3.0}%", self.zoom * 100.0)).monospace());
                if ui.add_enabled(self.zoom < ZOOM_MAX - 0.001, egui::Button::new("+")).on_hover_text("Bigger").clicked() {
                    self.set_zoom(ui.ctx(), self.zoom + ZOOM_STEP);
                }
                if (self.zoom - 1.0).abs() > 0.001 && ui.button("Reset").clicked() {
                    self.set_zoom(ui.ctx(), 1.0);
                }
            });
            ui.label(small("Both are remembered. Ctrl + and Ctrl − change the text size too."));

            ui.add_space(14.0);
            eyebrow(ui, "ADDRESS");
            // Port: editable + persisted. The server binds the port once at startup, so a change
            // applies on the next launch — we save it and say so.
            ui.horizontal(|ui| {
                ui.label("Port");
                ui.add(egui::DragValue::new(&mut self.port_edit).range(1024.0..=65535.0).speed(1.0))
                    .on_hover_text("The port devices connect to (1024–65535). Default 47000.");
                if ui.add_enabled(self.port_edit != self.port, egui::Button::new("Save")).clicked() {
                    self.port_msg = match crate::settings::save_port(self.port_edit) {
                        Ok(()) => format!("Saved. Restart Newfoundsync to serve on port {}.", self.port_edit),
                        Err(e) => format!("Couldn't save the port: {e}"),
                    };
                }
            });
            if self.port_msg.is_empty() {
                ui.label(small(format!("Serving on port {} now. A change takes effect the next time the server starts.", self.port)));
            } else {
                ui.label(RichText::new(&self.port_msg).size(12.0).color(c_accent_hi()));
            }

            ui.add_space(14.0);
            eyebrow(ui, "ABOUT");
            ui.label(RichText::new(format!("Newfoundsync {}", crate::BUILD_ID)).monospace().color(c_text2()));
            ui.label(small(format!("Shown to devices as \"{}\".", self.server_name)));
            ui.horizontal(|ui| {
                if ui
                    .button("Open the status page")
                    .on_hover_text("A read-only list of connected devices, viewable from any browser on the network.")
                    .clicked()
                {
                    open_url(&format!("{}/status", self.url));
                }
                if ui
                    .button("Open the health report")
                    .on_hover_text("The pipeline's raw counters, for a bug report.")
                    .clicked()
                {
                    open_url(&format!("{}/health", self.url));
                }
            });
        });
    }

    // ===== pending bar + footer ======================================================

    /// The bar that appears the moment a setting differs from what is streaming.
    ///
    /// It is pinned to the window, not to the column the settings scroll in. Apply used to be the
    /// last thing in that column: add one more block above it and the only button that made a
    /// change take effect was off screen, with nothing saying a change was waiting.
    fn ui_pending(&mut self, ui: &mut egui::Ui, live: &Live) {
        ui.horizontal(|ui| {
            if live.busy {
                ui.spinner();
                ui.label(if live.waiting_on_portal {
                    "Starting… approve the screen-share prompt if it appears."
                } else {
                    "Starting…"
                });
                return;
            }
            let blocker = self.apply_blocker();
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let apply = egui::Button::new(RichText::new("Apply").strong().color(Color32::WHITE)).fill(c_accent());
                let resp = ui.add_enabled(blocker.is_none(), apply);
                if resp
                    .on_hover_text("Rebuilds the stream with the new settings.")
                    .on_disabled_hover_text(blocker.unwrap_or_default())
                    .clicked()
                {
                    self.apply();
                }
                if ui.button("Discard").on_hover_text("Put the settings back to what is streaming.").clicked() {
                    self.discard_changes();
                }
                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                    ui.label(RichText::new(plural(self.change_count().max(1), "change")).strong().color(c_text()));
                    let rest = match blocker {
                        Some(why) => RichText::new(why).color(c_err()),
                        None => RichText::new(if live.clients_n == 0 {
                            "not applied. No devices are connected.".to_string()
                        } else {
                            format!(
                                "not applied. Applying reconnects {}, about 1 s of silence.",
                                plural(live.clients_n, "device")
                            )
                        })
                        .color(c_text2()),
                    };
                    ui.add(egui::Label::new(rest).truncate());
                });
            });
        });
    }

    fn ui_footer(&mut self, ui: &mut egui::Ui, live: &Live) {
        if self.notice.as_ref().is_some_and(|(_, at)| at.elapsed() > Duration::from_secs(5)) {
            self.notice = None;
        }
        // Right-to-left OUTER with a left-to-right INNER is the egui idiom for "pin one thing to
        // the right, give the rest to the left": the outer layout places the version first (so it
        // lands on the right edge), then the inner child receives whatever width is left.
        ui.horizontal(|ui| {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                // What a release tag corresponds to, so a bug report can read the build off the
                // window. Matches the web client's footer stamp. The commit is in Settings.
                ui.label(RichText::new(format!("v{}", env!("CARGO_PKG_VERSION"))).monospace().size(12.0).color(c_dim()));
                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                    let (text, col) = match (&self.notice, &live.serving) {
                        (Some((n, _)), _) => (n.clone(), c_accent_hi()),
                        (None, Some(dev)) => (
                            format!(
                                "Sharing {dev}{} · {} · {:.1} s delay",
                                if live.video { " + screen" } else { "" },
                                self.applied
                                    .as_ref()
                                    .map(|a| match a.codec {
                                        CodecKind::Pcm => "PCM".to_string(),
                                        CodecKind::Opus => format!("Opus {} kbps", a.bitrate / 1000),
                                    })
                                    .unwrap_or_default(),
                                self.applied.as_ref().map(|a| a.buffer_ms).unwrap_or(self.buffer_ms) as f32 / 1000.0,
                            ),
                            c_dim(),
                        ),
                        (None, None) => ("Nothing is being shared.".to_string(), c_err()),
                    };
                    ui.add(egui::Label::new(RichText::new(text).size(12.0).color(col)).truncate());
                });
            });
        });
    }
}

impl eframe::App for ServerApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        // Fill the whole window with the theme background. eframe's framebuffer clear colour
        // doesn't track our runtime theme, so without this the backdrop stays dark behind the
        // light theme. Paint c_bg() at the very back.
        ctx.layer_painter(egui::LayerId::background()).rect_filled(ctx.viewport_rect(), 0.0, c_bg());

        // Text size: apply the saved one on the first frame (deterministic on every display), then
        // follow whatever changes it — the Settings buttons or egui's own Ctrl + / Ctrl − — and
        // remember it.
        if !self.did_initial_zoom {
            self.did_initial_zoom = true;
            ctx.set_zoom_factor(self.zoom);
        } else if (ctx.zoom_factor() - self.zoom).abs() > 0.004 {
            self.set_zoom(&ctx, ctx.zoom_factor());
        }

        // Seed the "applied" baseline BEFORE poll_refresh() can self-heal a vanished source: it
        // must reflect what the stream was actually started with (the CLI/init config), so a heal
        // that rewrites the UI shows up as a pending change instead of being absorbed silently.
        if self.applied.is_none() {
            self.applied = Some(self.current_config());
        }
        #[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
        {
            #[cfg(target_os = "linux")]
            self.poll_routes(&ctx);
            self.poll_refresh();
            if self.refreshing {
                ctx.request_repaint_after(Duration::from_millis(50)); // pick up the result promptly
            }
            // NO periodic rescan of the app list. Linux entries exist only while their app is
            // playing, so the list does go stale — but a scan every 2 s made the refresh button
            // flicker and disable itself every couple of seconds, which reads as the UI fighting
            // you, and it kept the window repainting forever on an idle machine. The list is
            // refreshed where it matters instead: on selecting the per-app source, on opening the
            // picker, and on pressing refresh.
        }

        let live = self.live();
        // A switch failed: the previous stream is still the one serving, so what is "applied" is
        // what it was BEFORE that Apply. Roll the baseline back, and the change shows as pending
        // again next to the error instead of the window claiming it took effect.
        let failures = self.status.lock().unwrap().failures;
        if failures != self.seen_failures {
            self.seen_failures = failures;
            if let Some(prev) = self.applied_before.take() {
                self.applied = Some(prev);
            }
        }
        self.refresh_address();
        let rows = self.client_rows();
        self.remember_devices(&rows);
        let dirty = self.applied.as_ref() != Some(&self.current_config());

        // Window-geometry diagnostic. "It won't resize any smaller" is otherwise unanswerable
        // without guessing at DPI: the min size is in LOGICAL pixels while everything egui lays out
        // is in POINTS (scaled by zoom_factor), so the two are easy to confuse. Logged at debug,
        // once per distinct size, so `RUST_LOG=newfoundsync=debug` shows exactly where the floor is.
        if let Some(r) = ctx.input(|i| i.viewport().inner_rect) {
            let size = (r.width().round() as i32, r.height().round() as i32);
            if self.last_logged_size != Some(size) {
                self.last_logged_size = Some(size);
                tracing::debug!(
                    win_pts_w = size.0,
                    win_pts_h = size.1,
                    zoom = ctx.zoom_factor(),
                    ppp = ctx.pixels_per_point(),
                    "window geometry (points; multiply by ppp for physical pixels)"
                );
            }
        }

        let bar = |fill: Color32, x: i8, y: i8| egui::Frame::NONE.fill(fill).inner_margin(egui::Margin::symmetric(x, y));

        egui::Panel::top("header")
            .frame(bar(c_surface(), 14, 9))
            .resizable(false)
            .show_inside(ui, |ui| self.ui_header(ui, &live));
        if self.has_alerts(&live) {
            egui::Panel::top("alerts")
                .frame(egui::Frame::NONE)
                .resizable(false)
                .show_inside(ui, |ui| self.ui_alerts(ui, &live));
        }
        // Bottom panels stack upward in the order they are added: the footer first, so the pending
        // bar sits directly above it.
        egui::Panel::bottom("footer")
            .frame(bar(c_bg(), 14, 5))
            .resizable(false)
            .show_inside(ui, |ui| self.ui_footer(ui, &live));
        if dirty || live.busy {
            egui::Panel::bottom("pending")
                .frame(bar(c_accent_bg(), 14, 8))
                .resizable(false)
                .show_inside(ui, |ui| self.ui_pending(ui, &live));
        }
        egui::Panel::left("sharing")
            .frame(bar(c_bg(), 14, 12))
            .resizable(false)
            .exact_size(SHARE_W)
            .show_inside(ui, |ui| self.ui_sharing(ui));
        egui::CentralPanel::default().frame(bar(c_bg(), 14, 12)).show_inside(ui, |ui| match self.view {
            MainView::Settings => self.ui_settings(ui),
            MainView::AddDevice => self.ui_add_device(ui, false),
            // A server that has never had a device has nothing to mix: teach the first
            // connection in that space instead of showing an empty list.
            MainView::Devices if rows.is_empty() && self.known.is_empty() => self.ui_add_device(ui, true),
            MainView::Devices => self.ui_devices(ui, &rows, &live),
        });

        // Deliver any changed volume / sync, move a calibration run along, save what changed.
        self.push_client_state();
        self.tick_calibration(&rows);
        self.save_devices_if_due(&ctx);

        // Repaint often enough for the meter to look alive while there is sound, and lazily
        // otherwise: counts, health lines and the chip only need about a second's resolution.
        let moving = self.meter[0].max(self.meter[1]) > 0.0;
        ctx.request_repaint_after(Duration::from_millis(if moving || self.calibrating { 66 } else { 500 }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn healthy() -> Live {
        Live {
            busy: false,
            waiting_on_portal: false,
            serving: Some("alsa_output.analog-stereo.monitor".into()),
            video: false,
            error: None,
            dummy: false,
            is_cast: false,
            cast_held: false,
            stalled: false,
            stall_s: 0,
            video_stalled: false,
            capture_closed: false,
            encoder_failed: false,
            clients_n: 3,
            quiet: false,
        }
    }

    #[test]
    fn a_healthy_stream_with_listeners_is_live() {
        assert_eq!(chip_for(&healthy()), Chip::Live);
        assert_eq!(chip_for(&Live { clients_n: 0, ..healthy() }), Chip::Ready);
    }

    #[test]
    fn a_stalled_stream_is_never_reported_as_live() {
        // The bug this replaces: the pill latched "Live" the first time a stream came up and never
        // looked again, so it stayed green through a dead source with a room full of listeners.
        assert_eq!(chip_for(&Live { stalled: true, ..healthy() }), Chip::SoundStopped);
        assert_eq!(chip_for(&Live { stalled: true, dummy: true, ..healthy() }), Chip::SoundStopped);
    }

    #[test]
    fn an_empty_cast_slot_is_waiting_not_a_fault() {
        // With a cast source nothing plays until a device casts. That is the normal resting state,
        // and it must read as neither "Live" (nothing is playing) nor an error.
        let idle = Live { is_cast: true, cast_held: false, ..healthy() };
        assert_eq!(chip_for(&idle), Chip::WaitingForCast);
        assert_eq!(chip_for(&Live { cast_held: true, ..idle }), Chip::Live);
    }

    #[test]
    fn a_silent_source_is_not_called_live() {
        assert_eq!(chip_for(&Live { quiet: true, ..healthy() }), Chip::Quiet);
        // ... but silence is the lesser news when something is actually wrong.
        assert_eq!(chip_for(&Live { quiet: true, stalled: true, ..healthy() }), Chip::SoundStopped);
    }

    #[test]
    fn starting_and_stopped_come_before_everything_else() {
        assert_eq!(chip_for(&Live { busy: true, stalled: true, ..healthy() }), Chip::Starting);
        assert_eq!(chip_for(&Live { serving: None, ..healthy() }), Chip::Stopped);
        assert_eq!(chip_for(&Live { dummy: true, ..healthy() }), Chip::NoOutput);
    }

    #[test]
    fn durations_read_coarsely() {
        assert_eq!(ago(10), "a moment");
        assert_eq!(ago(300), "5 min");
        assert_eq!(ago(7_200), "2 h");
        assert_eq!(ago(3 * 86_400), "3 days");
        assert_eq!(plural(1, "device"), "1 device");
        assert_eq!(plural(4, "device"), "4 devices");
    }
}
