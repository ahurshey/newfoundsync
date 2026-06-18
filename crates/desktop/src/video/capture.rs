//! Windows screen capture (Windows.Graphics.Capture) → BGRA frames.
//!
//! WGC delivers frames on a dedicated thread via a callback; we keep only the
//! latest frame in a shared slot (overwrite-on-arrival), so the encoder can pull
//! at its own target fps and naturally drop stale frames.

use std::sync::{Arc, Mutex};

use anyhow::{Context as _, Result};
use windows_capture::capture::{CaptureControl, Context, GraphicsCaptureApiHandler};
use windows_capture::frame::Frame;
use windows_capture::graphics_capture_api::InternalCaptureControl;
use windows_capture::monitor::Monitor;
use windows_capture::settings::{
    ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
    MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
};

/// One captured frame: tightly-packed BGRA (`width*height*4`).
pub struct CapturedFrame {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}

/// Shared latest-frame slot — capture writes, encoder takes.
pub type FrameSlot = Arc<Mutex<Option<CapturedFrame>>>;

type CapErr = Box<dyn std::error::Error + Send + Sync>;

struct Handler {
    slot: FrameSlot,
    scratch: Vec<u8>,
}

impl GraphicsCaptureApiHandler for Handler {
    type Flags = FrameSlot;
    type Error = CapErr;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        Ok(Handler {
            slot: ctx.flags,
            scratch: Vec::new(),
        })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        _ctl: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        let (w, h) = (frame.width(), frame.height());
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
            slot.clone(),
        );
        let control =
            Handler::start_free_threaded(settings).context("start screen capture")?;
        Ok(ScreenCapture {
            control: Some(control),
            slot,
        })
    }
}

impl Drop for ScreenCapture {
    fn drop(&mut self) {
        if let Some(c) = self.control.take() {
            let _ = c.stop();
        }
    }
}
