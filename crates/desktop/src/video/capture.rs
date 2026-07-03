// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Alex Hurshman and the Newfoundsync contributors.

//! Windows screen capture (Windows.Graphics.Capture) → BGRA frames.
//!
//! WGC delivers frames on a dedicated thread via a callback; we keep only the latest
//! frame in a shared slot (overwrite-on-arrival), so the encoder can pull at its own
//! target fps and naturally drop stale frames. Encoding runs on the video-producer
//! thread from system-memory BGRA (→ AV1 / VP9). There is no GPU zero-copy fast-lane —
//! that path was HEVC-specific and was removed together with HEVC.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context as _, Result};
use windows_capture::capture::{CaptureControl, Context, GraphicsCaptureApiHandler};
use windows_capture::frame::Frame;
use windows_capture::graphics_capture_api::InternalCaptureControl;
use windows_capture::monitor::Monitor;
use windows_capture::settings::{
    ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
    MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
};
use windows_capture::window::Window;

/// One captured frame: tightly-packed BGRA (`width*height*4`).
pub struct CapturedFrame {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}

/// Shared latest-frame slot — capture writes, the video-producer thread takes.
pub type FrameSlot = Arc<Mutex<Option<CapturedFrame>>>;

/// What the capture Handler is initialized with.
pub struct CaptureInit {
    pub slot: FrameSlot,
}

type CapErr = Box<dyn std::error::Error + Send + Sync>;

struct Handler {
    slot: FrameSlot,
    scratch: Vec<u8>,
    first_frame_logged: bool,
}

impl GraphicsCaptureApiHandler for Handler {
    type Flags = CaptureInit;
    type Error = CapErr;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        let CaptureInit { slot } = ctx.flags;
        Ok(Handler {
            slot,
            scratch: Vec::new(),
            first_frame_logged: false,
        })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        _ctl: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        let (w, h) = (frame.width(), frame.height());
        if !self.first_frame_logged {
            // Confirms capture is actually delivering frames (a window can "start" but never
            // fire FrameArrived if the compositor isn't re-presenting it — see start_window).
            tracing::info!(width = w, height = h, "video capture: first frame arrived");
            self.first_frame_logged = true;
        }
        // Copy BGRA into the slot for the video-producer thread to scale + encode.
        let fb = frame.buffer()?;
        let bgra = fb.as_nopadding_buffer(&mut self.scratch);
        if let Ok(mut guard) = self.slot.lock() {
            *guard = Some(CapturedFrame {
                width: w,
                height: h,
                bgra: bgra.to_vec(),
            });
        }
        Ok(())
    }

    fn on_closed(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
}

/// A running primary-monitor capture. Stops on drop.
pub struct ScreenCapture {
    control: Option<CaptureControl<Handler, CapErr>>,
    pub slot: FrameSlot,
}

impl ScreenCapture {
    pub fn start_primary() -> Result<ScreenCapture> {
        let slot: FrameSlot = Arc::new(Mutex::new(None));
        let monitor = Monitor::primary().context("get primary monitor")?;
        let settings = Settings::new(
            monitor,
            CursorCaptureSettings::Default,
            DrawBorderSettings::Default,
            SecondaryWindowSettings::Default,
            MinimumUpdateIntervalSettings::Default,
            DirtyRegionSettings::Default,
            ColorFormat::Bgra8,
            CaptureInit { slot: slot.clone() },
        );
        let control = Handler::start_free_threaded(settings).context("start screen capture")?;
        Ok(ScreenCapture { control: Some(control), slot })
    }

    /// Capture a single window by its raw `HWND` value (from the source picker).
    pub fn start_window(hwnd: isize) -> Result<ScreenCapture> {
        let raw = hwnd as *mut std::ffi::c_void;
        // The window may have closed between the picker refresh and Apply — fail clearly.
        if !Window::from_raw_hwnd(raw).is_valid() {
            anyhow::bail!("the selected window is no longer open — pick it again or share the whole screen");
        }
        // CRITICAL: a monitor is re-presented every vsync (WGC fires continuously), but a single
        // window only fires FrameArrived when the compositor re-presents THAT window — so an
        // occluded/minimized window delivers ZERO frames. A custom minimum update interval makes
        // WGC sample it on a timer regardless of whether it's repainting.
        match Self::start_window_inner(raw, MinimumUpdateIntervalSettings::Custom(Duration::from_millis(16))) {
            Ok(c) => Ok(c),
            Err(e) => {
                tracing::warn!("window capture with a timed update interval failed ({e:#}); retrying with the default interval");
                Self::start_window_inner(raw, MinimumUpdateIntervalSettings::Default)
            }
        }
    }

    fn start_window_inner(
        raw: *mut std::ffi::c_void,
        interval: MinimumUpdateIntervalSettings,
    ) -> Result<ScreenCapture> {
        let slot: FrameSlot = Arc::new(Mutex::new(None));
        let settings = Settings::new(
            Window::from_raw_hwnd(raw),
            CursorCaptureSettings::Default,
            DrawBorderSettings::Default,
            SecondaryWindowSettings::Default,
            interval,
            DirtyRegionSettings::Default,
            ColorFormat::Bgra8,
            CaptureInit { slot: slot.clone() },
        );
        let control = Handler::start_free_threaded(settings).context("start window capture")?;
        Ok(ScreenCapture { control: Some(control), slot })
    }
}

impl Drop for ScreenCapture {
    fn drop(&mut self) {
        if let Some(c) = self.control.take() {
            let _ = c.stop();
        }
    }
}
