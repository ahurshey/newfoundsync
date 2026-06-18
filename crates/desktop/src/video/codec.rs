//! Software H.264 encode/decode via openh264 (desktop video pipeline).
//!
//! Encode takes a tightly-packed BGRA frame (what the screen capture produces),
//! converts to RGB then I420, and emits an H.264 Annex-B bitstream. Decode yields
//! RGBA (ready for an egui texture). Software codec gets the whole pipeline
//! working/testable now; a hardware encoder is a later drop-in for smooth 4K60.

use anyhow::{Context, Result};
use openh264::decoder::Decoder;
use openh264::encoder::{BitRate, Encoder, EncoderConfig, FrameRate, RateControlMode, UsageType};
use openh264::formats::{RgbSliceU8, YUVBuffer, YUVSource};
use openh264::OpenH264API;
use rayon::prelude::*;

use newfoundsync_core::video::EncoderBackend;

/// Threads for the software encoder / parallel conversions: the machine's core
/// count, capped (diminishing returns + slice-count quality cost past ~8).
fn worker_threads() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .clamp(1, 8)
}

/// H.264 encoder fed BGRA frames.
pub struct H264Encoder {
    enc: Encoder,
    width: usize,
    height: usize,
    rgb: Vec<u8>, // reused BGRA→RGB scratch
}

impl H264Encoder {
    pub fn new(width: u32, height: u32, fps: u32, bitrate_kbps: u32) -> Result<Self> {
        let cfg = EncoderConfig::new()
            .rate_control_mode(RateControlMode::Bitrate)
            .bitrate(BitRate::from_bps(bitrate_kbps * 1000))
            .max_frame_rate(FrameRate::from_hz(fps as f32))
            // We're encoding a desktop, not camera video — this preset is both
            // faster and sharper on screen content (text, sharp edges).
            .usage_type(UsageType::ScreenContentRealTime)
            // Slice-based multithreading across cores for higher throughput.
            .num_threads(worker_threads() as u16)
            .skip_frames(false);
        let enc =
            Encoder::with_api_config(OpenH264API::from_source(), cfg).context("create H.264 encoder")?;
        Ok(H264Encoder {
            enc,
            width: width as usize,
            height: height as usize,
            rgb: vec![0u8; (width * height * 3) as usize],
        })
    }

    /// Encode one tightly-packed BGRA frame (`width*height*4` bytes). Returns the
    /// H.264 bytes (may be empty if the encoder skipped/buffered the frame).
    pub fn encode_bgra(&mut self, bgra: &[u8]) -> Result<Vec<u8>> {
        let n = self.width * self.height;
        debug_assert_eq!(bgra.len(), n * 4);
        // BGRA → RGB, parallelized per pixel across cores.
        self.rgb
            .par_chunks_mut(3)
            .zip(bgra.par_chunks(4))
            .for_each(|(o, i)| {
                o[0] = i[2];
                o[1] = i[1];
                o[2] = i[0];
            });
        let yuv = YUVBuffer::from_rgb8_source(RgbSliceU8::new(&self.rgb, (self.width, self.height)));
        let bitstream = self.enc.encode(&yuv).context("H.264 encode")?;
        Ok(bitstream.to_vec())
    }

    /// Make the next encoded frame an IDR keyframe (on join / resolution switch).
    pub fn force_keyframe(&mut self) {
        self.enc.force_intra_frame();
    }
}

/// The active video encoder — software (openh264) or, on Windows, GPU hardware
/// (Media Foundation). The server holds this; both arms expose the same API.
pub enum VideoEncoder {
    Cpu(H264Encoder),
    #[cfg(target_os = "windows")]
    Hardware(crate::video::mf_encoder::MfH264Encoder),
}

impl VideoEncoder {
    /// Build the encoder for the requested backend. `Auto` tries GPU, falling
    /// back to CPU; `Hardware` errors if the GPU encoder is unavailable; `Cpu`
    /// always uses openh264.
    pub fn new(
        backend: EncoderBackend,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_kbps: u32,
    ) -> Result<VideoEncoder> {
        let cpu = || H264Encoder::new(width, height, fps, bitrate_kbps).map(VideoEncoder::Cpu);

        match backend {
            EncoderBackend::Cpu => cpu(),
            #[cfg(target_os = "windows")]
            EncoderBackend::Hardware => {
                let hw = crate::video::mf_encoder::MfH264Encoder::new(
                    width,
                    height,
                    fps,
                    bitrate_kbps,
                )
                .context("hardware (Media Foundation) encoder")?;
                tracing::info!("video: hardware (GPU) encoder active");
                Ok(VideoEncoder::Hardware(hw))
            }
            #[cfg(target_os = "windows")]
            EncoderBackend::Auto => {
                match crate::video::mf_encoder::MfH264Encoder::new(width, height, fps, bitrate_kbps)
                {
                    Ok(hw) => {
                        tracing::info!("video: hardware (GPU) encoder active");
                        Ok(VideoEncoder::Hardware(hw))
                    }
                    Err(e) => {
                        tracing::warn!("video: GPU encoder unavailable ({e:#}); using CPU");
                        cpu()
                    }
                }
            }
            #[cfg(not(target_os = "windows"))]
            EncoderBackend::Hardware | EncoderBackend::Auto => {
                tracing::warn!("video: hardware encode is Windows-only; using CPU");
                cpu()
            }
        }
    }

    pub fn encode_bgra(&mut self, bgra: &[u8]) -> Result<Vec<u8>> {
        match self {
            VideoEncoder::Cpu(e) => e.encode_bgra(bgra),
            #[cfg(target_os = "windows")]
            VideoEncoder::Hardware(e) => e.encode_bgra(bgra),
        }
    }

    pub fn force_keyframe(&mut self) {
        match self {
            VideoEncoder::Cpu(e) => e.force_keyframe(),
            #[cfg(target_os = "windows")]
            VideoEncoder::Hardware(e) => e.force_keyframe(),
        }
    }

    /// Human-readable label of the backend actually in use (for logs/telemetry).
    pub fn backend_label(&self) -> &'static str {
        match self {
            VideoEncoder::Cpu(_) => "CPU (openh264)",
            #[cfg(target_os = "windows")]
            VideoEncoder::Hardware(_) => "GPU (Media Foundation)",
        }
    }
}

/// H.264 decoder producing RGBA frames.
pub struct H264Decoder {
    dec: Decoder,
}

impl H264Decoder {
    pub fn new() -> Result<Self> {
        Ok(H264Decoder {
            dec: Decoder::new().context("create H.264 decoder")?,
        })
    }

    /// Decode an H.264 access unit. Returns `(width, height, rgba)` when a frame
    /// is produced (the decoder may buffer and return `None`).
    pub fn decode_rgba(&mut self, data: &[u8]) -> Result<Option<(u32, u32, Vec<u8>)>> {
        match self.dec.decode(data).context("H.264 decode")? {
            Some(yuv) => {
                let (w, h) = yuv.dimensions();
                let mut rgba = vec![0u8; w * h * 4];
                yuv.write_rgba8(&mut rgba);
                Ok(Some((w as u32, h as u32, rgba)))
            }
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn h264_encode_decode_roundtrip() {
        let (w, h) = (320u32, 240u32);
        let mut enc = H264Encoder::new(w, h, 30, 1000).expect("encoder");
        let mut dec = H264Decoder::new().expect("decoder");

        // A simple BGRA gradient frame.
        let mut bgra = vec![0u8; (w * h * 4) as usize];
        for y in 0..h {
            for x in 0..w {
                let i = ((y * w + x) * 4) as usize;
                bgra[i] = (x % 256) as u8; // B
                bgra[i + 1] = (y % 256) as u8; // G
                bgra[i + 2] = 128; // R
                bgra[i + 3] = 255; // A
            }
        }

        enc.force_keyframe();
        // Encode a few frames; the first IDR should decode.
        let mut decoded = None;
        for _ in 0..3 {
            let bits = enc.encode_bgra(&bgra).expect("encode");
            if bits.is_empty() {
                continue;
            }
            if let Some(frame) = dec.decode_rgba(&bits).expect("decode") {
                decoded = Some(frame);
                break;
            }
        }
        let (dw, dh, rgba) = decoded.expect("decoder produced a frame");
        assert_eq!((dw, dh), (w, h), "decoded dimensions match");
        assert_eq!(rgba.len(), (w * h * 4) as usize);
        // Alpha channel is filled opaque by write_rgba8.
        assert!(rgba.iter().skip(3).step_by(4).all(|&a| a == 255));
    }
}
