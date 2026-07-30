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
use newfoundsync_core::config::mono_now;
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
    /// `mono_now()` at the moment this picture was taken. The video PTS is derived from THIS, not
    /// from when the encoder finished, so the timestamp describes the content and not the pipeline.
    pub captured_ns: i64,
}

/// Shared latest-frame slot — capture writes, the video-producer thread takes.
pub type FrameSlot = Arc<Mutex<Option<CapturedFrame>>>;

/// What the capture Handler is initialized with.
pub struct CaptureInit {
    pub slot: FrameSlot,
    /// Set when Windows closes the capture session (see `Handler::on_closed`). This is the ONLY
    /// trustworthy "capture died" signal — see the note on [`ScreenCapture::closed`].
    pub closed: Arc<std::sync::atomic::AtomicBool>,
}

type CapErr = Box<dyn std::error::Error + Send + Sync>;

struct Handler {
    slot: FrameSlot,
    scratch: Vec<u8>,
    first_frame_logged: bool,
    closed: Arc<std::sync::atomic::AtomicBool>,
}

impl GraphicsCaptureApiHandler for Handler {
    type Flags = CaptureInit;
    type Error = CapErr;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        let CaptureInit { slot, closed } = ctx.flags;
        Ok(Handler {
            slot,
            scratch: Vec::new(),
            first_frame_logged: false,
            closed,
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
        // Stamp BEFORE the copy: this is as close as we get to when the picture was on screen.
        let captured_ns = mono_now();
        // Copy BGRA into the slot for the video-producer thread to scale + encode.
        let fb = frame.buffer()?;
        let bgra = fb.as_nopadding_buffer(&mut self.scratch);
        if let Ok(mut guard) = self.slot.lock() {
            *guard = Some(CapturedFrame {
                width: w,
                height: h,
                bgra: bgra.to_vec(),
                captured_ns,
            });
        }
        Ok(())
    }

    /// Windows closed the capture session — the shared window/display went away, the device was lost,
    /// or a session switch revoked access. This is the AUTHORITATIVE "capture died" signal.
    ///
    /// It matters that this is the signal used, and not frame silence: WGC delivery is
    /// change-driven, so a static screen legitimately produces NO frames for an unbounded time (a
    /// still slide, a fullscreen photo). Inferring death from silence therefore reports a fault on the
    /// most ordinary state a presentation tool has, which is worse than reporting nothing.
    fn on_closed(&mut self) -> Result<(), Self::Error> {
        self.closed.store(true, std::sync::atomic::Ordering::Relaxed);
        tracing::warn!(
            "screen capture session was CLOSED by Windows — the shared window/display is gone or \
             access was revoked; clients will see the last captured frame from now on"
        );
        Ok(())
    }
}

/// A running primary-monitor capture. Stops on drop.
pub struct ScreenCapture {
    control: Option<CaptureControl<Handler, CapErr>>,
    /// Set once Windows closes the capture session (`Handler::on_closed`).
    ///
    /// Read this — do NOT infer capture death from a lack of new frames. WGC is change-driven: a
    /// static screen produces no frames at all, for an unbounded time. A silence-based heuristic
    /// therefore fires on a still slide, which is the single most common state for a
    /// share-your-screen tool.
    pub closed: Arc<std::sync::atomic::AtomicBool>,
    pub slot: FrameSlot,
}

impl ScreenCapture {
    pub fn start_primary() -> Result<ScreenCapture> {
        let slot: FrameSlot = Arc::new(Mutex::new(None));
        let closed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let monitor = Monitor::primary().context("get primary monitor")?;
        let settings = Settings::new(
            monitor,
            CursorCaptureSettings::Default,
            DrawBorderSettings::Default,
            SecondaryWindowSettings::Default,
            MinimumUpdateIntervalSettings::Default,
            DirtyRegionSettings::Default,
            ColorFormat::Bgra8,
            CaptureInit { slot: slot.clone(), closed: closed.clone() },
        );
        let control = Handler::start_free_threaded(settings).context("start screen capture")?;
        Ok(ScreenCapture { control: Some(control), slot, closed })
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
        let closed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let settings = Settings::new(
            Window::from_raw_hwnd(raw),
            CursorCaptureSettings::Default,
            DrawBorderSettings::Default,
            SecondaryWindowSettings::Default,
            interval,
            DirtyRegionSettings::Default,
            ColorFormat::Bgra8,
            CaptureInit { slot: slot.clone(), closed: closed.clone() },
        );
        let control = Handler::start_free_threaded(settings).context("start window capture")?;
        Ok(ScreenCapture { control: Some(control), slot, closed })
    }
}

impl Drop for ScreenCapture {
    fn drop(&mut self) {
        if let Some(c) = self.control.take() {
            // `stop()` joins the WGC capture thread and returns BOTH its own control error and that
            // thread's `Result`. Discarding it destroyed the only record of a capture that died from
            // device loss or a session-switch access denial — the exact "video just stopped" report.
            if let Err(e) = c.stop() {
                tracing::warn!("screen capture stopped with an error: {e}");
            }
        }
    }
}
