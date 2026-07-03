// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Alex Hurshman and the Newfoundsync contributors.

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

use shiguredo_svt_av1::{ColorFormat, EncodeOptions, Encoder as SvtAv1Enc, EncoderConfig, FrameData, RcMode};
use std::num::NonZeroUsize;

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

/// Software AV1 encoder (SVT-AV1) fed BGRA frames. AV1 is royalty-free, so this is the
/// distribution-default codec when no hardware AV1 encoder is present. CPU-only here;
/// RTC/CBR low-delay preset with screen-content tools. Output is low-overhead OBU —
/// exactly what a browser `VideoDecoder` (av01) consumes, so no reframing is needed.
pub struct Av1Encoder {
    enc: SvtAv1Enc,
    width: usize,
    height: usize,
    cw: usize, // chroma plane width  = ceil(width / 2)
    ch: usize, // chroma plane height = ceil(height / 2)
    y: Vec<u8>, // reused I420 scratch planes (tightly packed, no stride)
    u: Vec<u8>,
    v: Vec<u8>,
    pending_key: bool, // set by force_keyframe(); consumed on the next encode
}

impl Av1Encoder {
    pub fn new(width: u32, height: u32, fps: u32, bitrate_kbps: u32) -> Result<Self> {
        let (w, h) = (width as usize, height as usize);
        let mut cfg = EncoderConfig::new(w, h, ColorFormat::I420);
        // Realtime, low-delay: CBR ⇒ SVT uses pred_structure=1 (low-delay P) and enables
        // per-frame forced keyframes; RTC mode + screen-content tools + no lookahead.
        cfg.rate_control_mode = RcMode::Cbr;
        cfg.target_bit_rate = bitrate_kbps.saturating_mul(1000); // bits/sec
        cfg.fps_numerator = fps.max(1);
        cfg.fps_denominator = 1;
        cfg.enc_mode = 12; // preset 0..=13 (higher = faster); ~12 for realtime screen share
        cfg.rtc = Some(true);
        cfg.screen_content_mode = Some(1);
        cfg.look_ahead_distance = Some(0);
        cfg.scene_change_detection = false;
        // ~2-second GOP so late joiners / droppers recover without a back-channel.
        cfg.intra_period_length = NonZeroUsize::new((fps.max(1) * 2) as usize);
        let enc = SvtAv1Enc::new(cfg).context("create SVT-AV1 encoder")?;
        let cw = w.div_ceil(2);
        let ch = h.div_ceil(2);
        Ok(Av1Encoder {
            enc,
            width: w,
            height: h,
            cw,
            ch,
            y: vec![0u8; w * h],
            u: vec![128u8; cw * ch],
            v: vec![128u8; cw * ch],
            pending_key: false,
        })
    }

    /// Encode one tightly-packed BGRA frame. Returns AV1 low-overhead OBU bytes (may be
    /// empty while the encoder's pipeline fills — the caller tolerates empty returns).
    pub fn encode_bgra(&mut self, bgra: &[u8]) -> Result<Vec<u8>> {
        debug_assert_eq!(bgra.len(), self.width * self.height * 4);
        bgra_to_i420(
            bgra, self.width, self.height, self.cw, self.ch, &mut self.y, &mut self.u, &mut self.v,
        );
        let force_keyframe = std::mem::take(&mut self.pending_key);
        self.enc
            .encode(
                &FrameData::I420 { y: &self.y, u: &self.u, v: &self.v },
                &EncodeOptions { force_keyframe },
            )
            .context("SVT-AV1 encode")?;
        // Drain every packet ready for this input (steady-state low-delay RTC is 1:1).
        let mut out = Vec::new();
        while let Some(pkt) = self.enc.next_frame() {
            out.extend_from_slice(pkt.data());
        }
        Ok(out)
    }

    /// Make the next encoded frame a keyframe (on join / resolution switch).
    pub fn force_keyframe(&mut self) {
        self.pending_key = true;
    }
}

/// BGRA → I420 planar (BT.601 studio range), 2×2-averaged chroma. Planes are tightly
/// packed (no row stride), which is what SVT-AV1 expects. Parallelized per row.
fn bgra_to_i420(
    bgra: &[u8],
    w: usize,
    h: usize,
    cw: usize,
    _ch: usize,
    y_plane: &mut [u8],
    u_plane: &mut [u8],
    v_plane: &mut [u8],
) {
    // Y plane: one task per row.
    y_plane.par_chunks_mut(w).enumerate().for_each(|(row, yrow)| {
        let base = row * w * 4;
        for x in 0..w {
            let p = base + x * 4;
            let b = bgra[p] as i32;
            let g = bgra[p + 1] as i32;
            let r = bgra[p + 2] as i32;
            yrow[x] = (((66 * r + 129 * g + 25 * b + 128) >> 8) + 16).clamp(0, 255) as u8;
        }
    });
    // Chroma planes: one task per output row (each covers a 2×2 input block), edge-clamped.
    u_plane
        .par_chunks_mut(cw)
        .zip(v_plane.par_chunks_mut(cw))
        .enumerate()
        .for_each(|(cy, (urow, vrow))| {
            for cx in 0..cw {
                let (mut sr, mut sg, mut sb, mut n) = (0i32, 0i32, 0i32, 0i32);
                for dy in 0..2 {
                    for dx in 0..2 {
                        let x = cx * 2 + dx;
                        let y = cy * 2 + dy;
                        if x < w && y < h {
                            let p = (y * w + x) * 4;
                            sb += bgra[p] as i32;
                            sg += bgra[p + 1] as i32;
                            sr += bgra[p + 2] as i32;
                            n += 1;
                        }
                    }
                }
                let (r, g, b) = (sr / n, sg / n, sb / n);
                urow[cx] = (((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128).clamp(0, 255) as u8;
                vrow[cx] = (((112 * r - 94 * g - 18 * b + 128) >> 8) + 128).clamp(0, 255) as u8;
            }
        });
}

/// The active video encoder — software H.264 (openh264), software AV1 (SVT-AV1), or, on
/// Windows, GPU hardware (Media Foundation: HEVC, or AV1 where the GPU supports it).
/// The server holds one; all arms expose the same encode/keyframe API.
pub enum VideoEncoder {
    Cpu(H264Encoder),
    Av1Cpu(Av1Encoder),
    #[cfg(target_os = "windows")]
    Av1Gpu(crate::video::mf_encoder::MfEncoder),
    #[cfg(target_os = "windows")]
    Hardware(crate::video::mf_encoder::MfEncoder),
}

impl VideoEncoder {
    /// Build the video encoder. Video is now **HEVC (H.265)**, which has no software
    /// encoder here (openh264 is H.264-only) — so it's GPU-only via Media Foundation with
    /// NO fallback. The `backend` selection is therefore moot for video (all map to GPU HEVC);
    /// it's kept in the signature so the GUI/CLI flag stays wired.
    pub fn new(
        backend: EncoderBackend,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_kbps: u32,
    ) -> Result<VideoEncoder> {
        // Codec follows the backend: `Cpu` = software H.264 (openh264, cross-platform); anything
        // else = GPU HEVC (Media Foundation, Windows-only). HEVC has no software encoder here and
        // H.264 has no GPU encoder here, so this one choice selects both codec AND backend. The GUI
        // "Codec" picker maps HEVC→Auto and H.264→Cpu; `--encoder cpu` selects H.264 on the CLI.
        if backend == EncoderBackend::Av1 {
            // AV1 (royalty-free). Prefer a hardware AV1 encoder (Media Foundation) where the GPU
            // has one; otherwise fall back to software SVT-AV1. Same "av01" stream to clients either way.
            #[cfg(target_os = "windows")]
            {
                match crate::video::mf_encoder::MfEncoder::new_av1(width, height, fps, bitrate_kbps) {
                    Ok(hw) => {
                        tracing::info!("video: GPU AV1 (Media Foundation) encoder active");
                        return Ok(VideoEncoder::Av1Gpu(hw));
                    }
                    Err(e) => tracing::info!("video: no GPU AV1 encoder ({e:#}); using software SVT-AV1"),
                }
            }
            let e = Av1Encoder::new(width, height, fps, bitrate_kbps)
                .context("software AV1 (SVT-AV1) encoder")?;
            tracing::info!("video: CPU AV1 (SVT-AV1) encoder active");
            return Ok(VideoEncoder::Av1Cpu(e));
        }
        if backend == EncoderBackend::Cpu {
            let e = H264Encoder::new(width, height, fps, bitrate_kbps)
                .context("software H.264 (openh264) encoder")?;
            tracing::info!("video: CPU H.264 (openh264) encoder active");
            return Ok(VideoEncoder::Cpu(e));
        }
        #[cfg(target_os = "windows")]
        {
            let hw = crate::video::mf_encoder::MfEncoder::new(width, height, fps, bitrate_kbps)
                .context("GPU (Media Foundation) HEVC encoder — HEVC has no software encoder; pick the H.264 or AV1 codec for CPU encode")?;
            tracing::info!("video: GPU HEVC encoder active");
            Ok(VideoEncoder::Hardware(hw))
        }
        #[cfg(not(target_os = "windows"))]
        {
            let _ = (width, height, fps, bitrate_kbps);
            anyhow::bail!("HEVC video requires Windows (Media Foundation); select the H.264 or AV1 codec for software encode")
        }
    }

    pub fn encode_bgra(&mut self, bgra: &[u8]) -> Result<Vec<u8>> {
        match self {
            VideoEncoder::Cpu(e) => e.encode_bgra(bgra),
            VideoEncoder::Av1Cpu(e) => e.encode_bgra(bgra),
            #[cfg(target_os = "windows")]
            VideoEncoder::Av1Gpu(e) => e.encode_bgra(bgra),
            #[cfg(target_os = "windows")]
            VideoEncoder::Hardware(e) => e.encode_bgra(bgra),
        }
    }

    pub fn force_keyframe(&mut self) {
        match self {
            VideoEncoder::Cpu(e) => e.force_keyframe(),
            VideoEncoder::Av1Cpu(e) => e.force_keyframe(),
            #[cfg(target_os = "windows")]
            VideoEncoder::Av1Gpu(e) => e.force_keyframe(),
            #[cfg(target_os = "windows")]
            VideoEncoder::Hardware(e) => e.force_keyframe(),
        }
    }

    /// True if this emitted access unit is a keyframe the browser can start/recover decoding
    /// from. Codec-aware: H.264 (IDR = NAL type 5, 1-byte header), HEVC (IRAP = types 16..=23,
    /// 2-byte header), and AV1 (a Sequence Header OBU — type 1 — rides with each keyframe
    /// temporal unit). One scan across all codecs would misdetect and leave the client stuck.
    pub fn is_keyframe(&self, au: &[u8]) -> bool {
        match self {
            VideoEncoder::Cpu(_) => annexb_has_h264_idr(au),
            VideoEncoder::Av1Cpu(_) => obu_has_av1_keyframe(au),
            #[cfg(target_os = "windows")]
            VideoEncoder::Av1Gpu(_) => obu_has_av1_keyframe(au),
            #[cfg(target_os = "windows")]
            VideoEncoder::Hardware(_) => annexb_has_hevc_irap(au),
        }
    }

    /// Human-readable label of the backend actually in use (for logs/telemetry).
    pub fn backend_label(&self) -> &'static str {
        match self {
            VideoEncoder::Cpu(_) => "CPU (openh264)",
            VideoEncoder::Av1Cpu(_) => "CPU (SVT-AV1)",
            #[cfg(target_os = "windows")]
            VideoEncoder::Av1Gpu(_) => "GPU AV1 (Media Foundation)",
            #[cfg(target_os = "windows")]
            VideoEncoder::Hardware(_) => "GPU HEVC (Media Foundation)",
        }
    }
}

/// H.264: 1-byte NAL header; nal_type = byte & 0x1f; IDR slice = 5. Scans Annex-B start codes.
/// `pub` so the web-cast relay can re-derive the keyframe flag from an uploaded AU (never trust the
/// caster's wire byte) — matching the local capture path.
pub fn annexb_has_h264_idr(au: &[u8]) -> bool {
    let mut i = 0usize;
    while i + 3 < au.len() {
        if au[i] == 0 && au[i + 1] == 0 && au[i + 2] == 1 {
            if au[i + 3] & 0x1f == 5 {
                return true;
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    false
}

/// HEVC: 2-byte NAL header; nal_type = (byte0 >> 1) & 0x3f; IRAP (BLA/IDR/CRA) = 16..=23.
#[cfg(target_os = "windows")]
fn annexb_has_hevc_irap(au: &[u8]) -> bool {
    let mut i = 0usize;
    while i + 3 < au.len() {
        if au[i] == 0 && au[i + 1] == 0 && au[i + 2] == 1 {
            let t = (au[i + 3] >> 1) & 0x3f;
            if (16..=23).contains(&t) {
                return true;
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    false
}

/// AV1 (low-overhead OBU): a keyframe temporal unit carries a Sequence Header OBU
/// (obu_type == 1), which SVT-AV1 (and the MF encoder) emit with every keyframe. Scans
/// the OBUs via their in-band size fields and returns true if a sequence header is present.
/// `pub` so a future AV1 web-cast relay can re-derive the flag from an uploaded AU.
pub fn obu_has_av1_keyframe(au: &[u8]) -> bool {
    const OBU_SEQUENCE_HEADER: u8 = 1;
    let mut i = 0usize;
    while i < au.len() {
        let b = au[i];
        let obu_type = (b >> 3) & 0x0f;
        let has_ext = (b >> 2) & 1 == 1;
        let has_size = (b >> 1) & 1 == 1;
        i += 1;
        if has_ext {
            i += 1; // extension header byte
        }
        if !has_size {
            // Low-overhead OBUs always carry a size; without one we can't safely skip.
            return obu_type == OBU_SEQUENCE_HEADER;
        }
        // LEB128 payload size.
        let mut size: usize = 0;
        let mut shift = 0u32;
        loop {
            if i >= au.len() {
                return false;
            }
            let byte = au[i];
            i += 1;
            size |= ((byte & 0x7f) as usize) << shift;
            if byte & 0x80 == 0 {
                break;
            }
            shift += 7;
            if shift >= 35 {
                return false;
            }
        }
        if obu_type == OBU_SEQUENCE_HEADER {
            return true;
        }
        i = i.saturating_add(size);
    }
    false
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
