// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Alex Hurshman and the Newfoundsync contributors.

//! Windows screen capture (Windows.Graphics.Capture) → BGRA frames.
//!
//! WGC delivers frames on a dedicated thread via a callback; we keep only the
//! latest frame in a shared slot (overwrite-on-arrival), so the encoder can pull
//! at its own target fps and naturally drop stale frames.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context as _, Result};
use tokio::sync::broadcast;
use windows_capture::capture::{CaptureControl, Context, GraphicsCaptureApiHandler};
use windows_capture::frame::Frame;
use windows_capture::graphics_capture_api::InternalCaptureControl;
use windows_capture::monitor::Monitor;
use windows_capture::settings::{
    ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
    MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
};
use windows_capture::window::Window;

use newfoundsync_core::config::mono_now;

use crate::media::{Frame as WireFrame, MSG_VIDEO};
use crate::video::gpu::GpuConverter;
use crate::video::mf_encoder::MfEncoder;

/// One captured frame: tightly-packed BGRA (`width*height*4`).
pub struct CapturedFrame {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}

/// Shared latest-frame slot — capture writes, encoder takes (CPU fallback path).
pub type FrameSlot = Arc<Mutex<Option<CapturedFrame>>>;

/// Parameters to (try to) build the GPU zero-copy fast-lane inside the capture callback.
/// Built on the capture thread in `Handler::new`; if it fails, we silently use the CPU slot.
#[derive(Clone)]
pub struct GpuParams {
    pub tx: broadcast::Sender<WireFrame>,
    pub lead_ns: i64,
    pub dw: u32,
    pub dh: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
}

/// What the capture Handler is initialized with: the CPU slot (always) + optional GPU params.
pub struct CaptureInit {
    pub slot: FrameSlot,
    pub gpu: Option<GpuParams>,
}

type CapErr = Box<dyn std::error::Error + Send + Sync>;

/// Assert-Send wrapper for the GPU pipeline. windows-capture's `start_free_threaded` requires
/// the Handler to be `Send`, but D3D11/MF COM objects are `!Send`. This is SOUND because the
/// pipeline is created and used ONLY on windows-capture's single capture thread (it serializes
/// `new`/`on_frame_arrived`/`on_closed` behind one mutex there); it never actually crosses threads.
struct SendCell<T>(T);
unsafe impl<T> Send for SendCell<T> {}

/// GPU fast-lane state (lives on the capture thread). Convert+encode+broadcast happen here.
struct GpuPipeline {
    converter: GpuConverter,
    encoder: MfEncoder,
    tx: broadcast::Sender<WireFrame>,
    lead_ns: i64,
}

/// Build the GPU pipeline (own VIDEO_SUPPORT device + D3D-aware encoder). Any failure → Err,
/// and the caller falls back to the CPU slot path.
fn build_gpu_pipeline(p: GpuParams) -> Result<GpuPipeline> {
    let converter = GpuConverter::try_new(p.dw, p.dh).context("GPU converter")?;
    let encoder = MfEncoder::new_d3d(p.dw, p.dh, p.fps, p.bitrate_kbps, &converter.device)
        .context("D3D-aware HEVC encoder")?;
    Ok(GpuPipeline {
        converter,
        encoder,
        tx: p.tx,
        lead_ns: p.lead_ns,
    })
}

/// True if the Annex-B access unit contains an HEVC IRAP picture (BLA/IDR/CRA, NAL types
/// 16..=23) — a real keyframe the browser can start/recover decoding from. HEVC NAL header is
/// 2 bytes; nal_type = (byte0 >> 1) & 0x3f. Scans the 00 00 01 start code (also matches 4-byte).
pub(crate) fn annexb_has_keyframe(au: &[u8]) -> bool {
    let mut i = 0usize;
    while i + 3 < au.len() {
        if au[i] == 0 && au[i + 1] == 0 && au[i + 2] == 1 {
            let nal_type = (au[i + 3] >> 1) & 0x3f;
            if (16..=23).contains(&nal_type) {
                return true;
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    false
}

struct Handler {
    slot: FrameSlot,
    scratch: Vec<u8>,
    first_frame_logged: bool,
    gpu: Option<SendCell<GpuPipeline>>,
}

impl Handler {
    /// GPU fast-lane: convert the WGC texture → NV12 on the GPU, encode it, broadcast the AU.
    /// Returns Err on any GPU failure so the caller can permanently degrade to the CPU path.
    fn gpu_encode(&mut self, frame: &mut Frame) -> Result<()> {
        let pipe = &mut self.gpu.as_mut().expect("gpu_encode called without a pipeline").0;
        if pipe.tx.receiver_count() == 0 {
            return Ok(()); // nobody watching → skip the convert+encode
        }
        let nv12 = pipe
            .converter
            .convert(frame.as_raw_texture(), frame.device_context(), frame.device())?;
        // force_keyframe is a no-op on the GPU MFT (GOP-driven); the wire flag is derived from
        // the actual bitstream below, so a new subscriber simply waits for the next GOP IDR.
        let bits = pipe.encoder.encode_texture(&nv12)?;
        if !bits.is_empty() {
            let pts = mono_now() + pipe.lead_ns;
            let is_key = annexb_has_keyframe(&bits);
            let mut msg = Vec::with_capacity(10 + bits.len());
            msg.push(MSG_VIDEO);
            msg.extend_from_slice(&pts.to_be_bytes());
            msg.push(if is_key { 1 } else { 0 });
            msg.extend_from_slice(&bits);
            let _ = pipe.tx.send(Arc::new(msg));
        }
        Ok(())
    }
}

impl GraphicsCaptureApiHandler for Handler {
    type Flags = CaptureInit;
    type Error = CapErr;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        let CaptureInit { slot, gpu } = ctx.flags;
        // Try to stand up the GPU fast-lane on THIS (capture) thread, where the WGC device/
        // context are valid. On any failure, fall back to the CPU slot path — video still works.
        let gpu = gpu.and_then(|p| match build_gpu_pipeline(p) {
            Ok(pipe) => {
                tracing::info!("video: GPU zero-copy path active (D3D11 VideoProcessor + D3D encoder)");
                Some(SendCell(pipe))
            }
            Err(e) => {
                tracing::warn!("video: GPU zero-copy unavailable ({e:#}); using CPU NV12 path");
                None
            }
        });
        Ok(Handler {
            slot,
            scratch: Vec::new(),
            first_frame_logged: false,
            gpu,
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

        // GPU fast-lane (no CPU readback / no CPU BGRA→NV12). On ANY error, permanently degrade
        // to the CPU slot path. NEVER return Err from here — that would tear down capture.
        if self.gpu.is_some() {
            match self.gpu_encode(frame) {
                Ok(()) => return Ok(()),
                Err(e) => {
                    tracing::warn!("video: GPU encode failed ({e:#}); falling back to CPU path");
                    self.gpu = None;
                    // fall through to the CPU slot write
                }
            }
        }

        // CPU path: copy BGRA into the slot for the video-producer thread to scale + encode.
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
    pub fn start_primary(gpu: Option<GpuParams>) -> Result<ScreenCapture> {
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
            CaptureInit { slot: slot.clone(), gpu },
        );
        let control =
            Handler::start_free_threaded(settings).context("start screen capture")?;
        Ok(ScreenCapture {
            control: Some(control),
            slot,
        })
    }

    /// Capture a single window by its raw `HWND` value (from the source picker).
    pub fn start_window(hwnd: isize, gpu: Option<GpuParams>) -> Result<ScreenCapture> {
        let raw = hwnd as *mut std::ffi::c_void;
        // The window may have closed between the picker refresh and Apply — fail clearly
        // (the control thread keeps serving the previous source and reports the message).
        if !Window::from_raw_hwnd(raw).is_valid() {
            anyhow::bail!("the selected window is no longer open — pick it again or share the whole screen");
        }
        // CRITICAL: a monitor is re-presented by the desktop compositor every vsync, so WGC
        // FrameArrived fires continuously. A single window only fires FrameArrived when the
        // compositor re-presents THAT window — so an occluded / minimized / backgrounded
        // window (e.g. Chrome sitting behind this server GUI) delivers ZERO frames and the
        // video is silently dead. A custom minimum update interval makes WGC sample it on a
        // timer regardless of whether it's repainting.
        match Self::start_window_inner(raw, MinimumUpdateIntervalSettings::Custom(Duration::from_millis(16)), gpu.clone()) {
            Ok(c) => Ok(c),
            Err(e) => {
                // Older Windows without SetMinUpdateInterval support → fall back to the
                // change-driven default (a foreground, repainting window still delivers frames).
                tracing::warn!("window capture with a timed update interval failed ({e:#}); retrying with the default interval");
                Self::start_window_inner(raw, MinimumUpdateIntervalSettings::Default, gpu)
            }
        }
    }

    fn start_window_inner(
        raw: *mut std::ffi::c_void,
        interval: MinimumUpdateIntervalSettings,
        gpu: Option<GpuParams>,
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
            CaptureInit { slot: slot.clone(), gpu },
        );
        let control =
            Handler::start_free_threaded(settings).context("start window capture")?;
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
