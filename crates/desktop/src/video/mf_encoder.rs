// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Alex Hurshman and the Newfoundsync contributors.

//! GPU hardware **AV1** encoder via Windows Media Foundation.
//!
//! Enumerates the system's hardware AV1 encoder MFT (Intel Arc/Xe, NVIDIA RTX 40+,
//! AMD RX 7000+ — whatever the GPU exposes), drives it as an async MFT, and exposes the
//! same `encode_bgra`/`force_keyframe` API as the software encoder. Input is system-memory
//! NV12 (converted from the captured BGRA); output is an AV1 low-overhead OBU stream the
//! browser's WebCodecs decoder reads. `new_av1` errors when no AV1 encoder MFT is present,
//! so the caller falls back to the software SVT-AV1 encoder.
//!
//! The encoder is created and used entirely on the video-encode thread, so the COM objects
//! never cross threads.

use std::ffi::c_void;
use std::mem::ManuallyDrop;

use anyhow::{bail, Context, Result};
use rayon::prelude::*;
use windows::core::Interface;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, COINIT_MULTITHREADED};
use windows::Win32::System::Variant::VARIANT;

pub struct MfEncoder {
    transform: IMFTransform,
    events: IMFMediaEventGenerator,
    width: u32,
    height: u32,
    provides_samples: bool,
    nv12: Vec<u8>,
    frame_idx: i64,
    sample_dur: i64, // 100-ns units per frame
}

impl MfEncoder {
    /// Hardware **AV1** encoder (system-memory NV12 → encode_bgra), using the GPU's AV1 encoder
    /// MFT where the machine has one (Intel Arc/Xe, NVIDIA RTX 40+, AMD RX 7000+). Errors if no
    /// AV1 encoder MFT is present, so the caller can fall back to software (SVT-AV1).
    pub fn new_av1(width: u32, height: u32, fps: u32, bitrate_kbps: u32) -> Result<Self> {
        // AV1 4:2:0 wants even dimensions; our presets already are.
        if width % 2 != 0 || height % 2 != 0 {
            bail!("dimensions must be even ({width}x{height})");
        }
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED); // per-thread; ignore "already init"
            MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET).context("MFStartup")?;

            // Find a HARDWARE AV1 encoder MFT.
            let out_info = MFT_REGISTER_TYPE_INFO {
                guidMajorType: MFMediaType_Video,
                guidSubtype: MFVideoFormat_AV1,
            };
            let flags = MFT_ENUM_FLAG(MFT_ENUM_FLAG_HARDWARE.0 | MFT_ENUM_FLAG_SORTANDFILTER.0);
            let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
            let mut count: u32 = 0;
            MFTEnumEx(
                MFT_CATEGORY_VIDEO_ENCODER,
                flags,
                None,
                Some(&out_info),
                &mut activates,
                &mut count,
            )
            .context("MFTEnumEx")?;
            if count == 0 || activates.is_null() {
                bail!("no hardware AV1 encoder MFT on this system (the GPU has no AV1 encoder)");
            }
            let slice = std::slice::from_raw_parts(activates, count as usize);
            let transform: IMFTransform = match slice[0].as_ref() {
                Some(act) => act.ActivateObject().context("ActivateObject")?,
                None => {
                    CoTaskMemFree(Some(activates as *const c_void));
                    bail!("null encoder activate");
                }
            };
            // Release every enumerated activate, then free the array.
            for i in 0..count as usize {
                let _ = std::ptr::read(activates.add(i)); // owned Option drops → Release
            }
            CoTaskMemFree(Some(activates as *const c_void));

            // Unlock the async MFT so we can drive it directly.
            if let Ok(attrs) = transform.GetAttributes() {
                let _ = attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1);
            }

            let frame_size = ((width as u64) << 32) | height as u64;
            let frame_rate = ((fps as u64) << 32) | 1;
            let par = (1u64 << 32) | 1;

            // Output type FIRST (required for encoders). AV1 defaults to Main profile (av01.0…),
            // so no profile attribute is set.
            let out = MFCreateMediaType().context("MFCreateMediaType(out)")?;
            out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            out.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_AV1)?;
            out.SetUINT32(&MF_MT_AVG_BITRATE, bitrate_kbps.saturating_mul(1000))?;
            out.SetUINT64(&MF_MT_FRAME_SIZE, frame_size)?;
            out.SetUINT64(&MF_MT_FRAME_RATE, frame_rate)?;
            out.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            out.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, par)?;
            // ~2-second GOP so late joiners / frame-droppers recover without a back-channel.
            out.SetUINT32(&MF_MT_MAX_KEYFRAME_SPACING, (fps * 2).max(1))?;
            transform.SetOutputType(0, &out, 0).context("SetOutputType(AV1)")?;

            // Input type: system-memory NV12.
            let inp = MFCreateMediaType().context("MFCreateMediaType(in)")?;
            inp.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            inp.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
            inp.SetUINT64(&MF_MT_FRAME_SIZE, frame_size)?;
            inp.SetUINT64(&MF_MT_FRAME_RATE, frame_rate)?;
            inp.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            inp.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, par)?;
            transform.SetInputType(0, &inp, 0).context("SetInputType(NV12)")?;

            // Low-latency hint (best-effort; a missing/declined property must not fail init).
            if let Ok(codec_api) = transform.cast::<ICodecAPI>() {
                let _ = codec_api.SetValue(&CODECAPI_AVEncCommonLowLatency, &VARIANT::from(true));
            }

            let info = transform.GetOutputStreamInfo(0).context("GetOutputStreamInfo")?;
            let provides_samples = (info.dwFlags
                & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32
                    | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0 as u32))
                != 0;

            let events: IMFMediaEventGenerator =
                transform.cast().context("MFT is not an async event generator")?;

            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;

            Ok(MfEncoder {
                transform,
                events,
                width,
                height,
                provides_samples,
                nv12: vec![0u8; (width as usize * height as usize * 3) / 2],
                frame_idx: 0,
                sample_dur: 10_000_000 / fps.max(1) as i64,
            })
        }
    }

    pub fn encode_bgra(&mut self, bgra: &[u8]) -> Result<Vec<u8>> {
        let expected = self.width as usize * self.height as usize * 4;
        if bgra.len() < expected {
            bail!("short BGRA frame: {} < {expected}", bgra.len());
        }
        self.bgra_to_nv12(bgra);
        let out = unsafe {
            let sample = self.make_input_sample()?;
            self.pump(&sample)?
        };
        self.frame_idx += 1;
        Ok(out)
    }

    /// Feed one input sample to the async MFT and drain whatever output is ready.
    unsafe fn pump(&mut self, sample: &IMFSample) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        // Block until the MFT asks for input, draining any output that arrives meanwhile.
        loop {
            let ev = self.events.GetEvent(MF_EVENT_FLAG_NONE).context("GetEvent")?;
            match ev.GetType()? {
                t if t == METransformNeedInput.0 as u32 => {
                    self.transform.ProcessInput(0, sample, 0).context("ProcessInput")?;
                    break;
                }
                t if t == METransformHaveOutput.0 as u32 => self.drain_output(&mut out)?,
                _ => {}
            }
        }
        // Collect any output that's immediately ready (non-blocking).
        while let Ok(ev) = self.events.GetEvent(MF_EVENT_FLAG_NO_WAIT) {
            if ev.GetType()? == METransformHaveOutput.0 as u32 {
                self.drain_output(&mut out)?;
            }
        }
        Ok(out)
    }

    /// No-op: Media Foundation keyframes are driven by the GOP (`MF_MT_MAX_KEYFRAME_SPACING`,
    /// ~2 s), which already gives joiners a fresh keyframe without a back-channel.
    pub fn force_keyframe(&mut self) {}

    // --- internals ---

    unsafe fn drain_output(&mut self, out: &mut Vec<u8>) -> Result<()> {
        let provided = if self.provides_samples {
            None
        } else {
            Some(self.alloc_output_sample()?)
        };
        let mut buf = MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: 0,
            pSample: ManuallyDrop::new(provided),
            dwStatus: 0,
            pEvents: ManuallyDrop::new(None),
        };
        let mut status = 0u32;
        let hr = self
            .transform
            .ProcessOutput(0, std::slice::from_mut(&mut buf), &mut status);
        let sample_opt = ManuallyDrop::take(&mut buf.pSample);
        let _ = ManuallyDrop::take(&mut buf.pEvents);

        match hr {
            Ok(()) => {
                if let Some(sample) = sample_opt {
                    let media_buf = sample.ConvertToContiguousBuffer()?;
                    let mut ptr: *mut u8 = std::ptr::null_mut();
                    let mut len: u32 = 0;
                    media_buf.Lock(&mut ptr, None, Some(&mut len))?;
                    if !ptr.is_null() && len > 0 {
                        out.extend_from_slice(std::slice::from_raw_parts(ptr, len as usize));
                    }
                    media_buf.Unlock()?;
                }
                Ok(())
            }
            Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => Ok(()),
            Err(e) => Err(e).context("ProcessOutput"),
        }
    }

    unsafe fn make_input_sample(&self) -> Result<IMFSample> {
        let len = self.nv12.len() as u32;
        let buffer = MFCreateMemoryBuffer(len).context("MFCreateMemoryBuffer")?;
        let mut ptr: *mut u8 = std::ptr::null_mut();
        buffer.Lock(&mut ptr, None, None)?;
        std::ptr::copy_nonoverlapping(self.nv12.as_ptr(), ptr, self.nv12.len());
        buffer.Unlock()?;
        buffer.SetCurrentLength(len)?;
        let sample = MFCreateSample().context("MFCreateSample")?;
        sample.AddBuffer(&buffer)?;
        sample.SetSampleTime(self.frame_idx * self.sample_dur)?;
        sample.SetSampleDuration(self.sample_dur)?;
        Ok(sample)
    }

    unsafe fn alloc_output_sample(&self) -> Result<IMFSample> {
        let info = self.transform.GetOutputStreamInfo(0)?;
        let size = info.cbSize.max(self.width * self.height);
        let buffer = MFCreateMemoryBuffer(size)?;
        let sample = MFCreateSample()?;
        sample.AddBuffer(&buffer)?;
        Ok(sample)
    }

    /// BGRA → NV12 (BT.601 studio range), 2×2-averaged chroma. Both planes are
    /// filled in parallel across cores (row-independent work).
    fn bgra_to_nv12(&mut self, bgra: &[u8]) {
        let w = self.width as usize;
        let h = self.height as usize;
        let (y_plane, uv_plane) = self.nv12.split_at_mut(w * h);

        // Y plane: one task per row.
        y_plane.par_chunks_mut(w).enumerate().for_each(|(j, yrow)| {
            let base = j * w * 4;
            for i in 0..w {
                let p = base + i * 4;
                let b = bgra[p] as i32;
                let g = bgra[p + 1] as i32;
                let r = bgra[p + 2] as i32;
                yrow[i] = (((66 * r + 129 * g + 25 * b + 128) >> 8) + 16).clamp(0, 255) as u8;
            }
        });

        // UV plane (interleaved): one task per output row (covers two input rows).
        uv_plane.par_chunks_mut(w).enumerate().for_each(|(jo, uvrow)| {
            let j = jo * 2;
            for i in (0..w).step_by(2) {
                let mut sr = 0i32;
                let mut sg = 0i32;
                let mut sb = 0i32;
                for dy in 0..2 {
                    for dx in 0..2 {
                        let p = ((j + dy) * w + (i + dx)) * 4;
                        sb += bgra[p] as i32;
                        sg += bgra[p + 1] as i32;
                        sr += bgra[p + 2] as i32;
                    }
                }
                let (r, g, b) = (sr / 4, sg / 4, sb / 4);
                let u = ((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128;
                let v = ((112 * r - 94 * g - 18 * b + 128) >> 8) + 128;
                uvrow[i] = u.clamp(0, 255) as u8;
                uvrow[i + 1] = v.clamp(0, 255) as u8;
            }
        });
    }
}

impl Drop for MfEncoder {
    fn drop(&mut self) {
        unsafe {
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            // MFShutdown is intentionally not called: it's process-global and
            // ref-counted, and other MF users (capture) may still be active.
        }
    }
}
