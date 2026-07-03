// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Alex Hurshman and the Newfoundsync contributors.

//! Video encoders for the desktop capture pipeline: software AV1 (SVT-AV1) and, on
//! Windows, GPU hardware via Media Foundation (HEVC, or AV1 where the GPU has an AV1
//! encoder). Each takes a tightly-packed BGRA frame (what the screen capture produces)
//! and emits an elementary stream the browser's WebCodecs decoder reads: AV1 low-overhead
//! OBU, or HEVC Annex-B. Also holds the codec-aware keyframe detectors shared by the local
//! capture path and the web-cast relay.
//!
//! H.264 is intentionally NOT encoded here: the only software H.264 (openh264) is
//! patent-encumbered when compiled from source, so it was removed to keep distributable
//! binaries clean. Browser web-cast sources still send H.264 (the browser's own licensed
//! codec) and the server merely relays it — `annexb_has_h264_idr` re-derives the keyframe
//! flag for that path.

use anyhow::{Context, Result};
use rayon::prelude::*;

use shiguredo_svt_av1::{ColorFormat, EncodeOptions, Encoder as SvtAv1Enc, EncoderConfig, FrameData, RcMode};
use std::num::NonZeroUsize;

use newfoundsync_core::video::EncoderBackend;

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
        cfg.target_bit_rate = bitrate_kbps.saturating_mul(1000) as usize; // bits/sec
        cfg.fps_numerator = fps.max(1) as usize;
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

/// The active video encoder — software AV1 (SVT-AV1) or, on Windows, GPU hardware via
/// Media Foundation (HEVC, or AV1 where the GPU supports it). The server holds one; all
/// arms expose the same encode/keyframe API.
pub enum VideoEncoder {
    Av1Cpu(Av1Encoder),
    #[cfg(target_os = "windows")]
    Av1Gpu(crate::video::mf_encoder::MfEncoder),
}

impl VideoEncoder {
    /// Build the video encoder: hardware AV1 (Media Foundation) where the GPU has an AV1
    /// encoder, otherwise software SVT-AV1. `backend` is retained for CLI/GUI wiring; AV1 is the
    /// only video codec the server encodes natively now (HEVC/H.264 were removed).
    pub fn new(
        backend: EncoderBackend,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_kbps: u32,
    ) -> Result<VideoEncoder> {
        let _ = backend;
        // AV1 (royalty-free). Prefer a hardware AV1 encoder (Media Foundation) where the GPU has
        // one; otherwise fall back to software SVT-AV1. Same "av01" stream either way.
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
        Ok(VideoEncoder::Av1Cpu(e))
    }

    pub fn encode_bgra(&mut self, bgra: &[u8]) -> Result<Vec<u8>> {
        match self {
            VideoEncoder::Av1Cpu(e) => e.encode_bgra(bgra),
            #[cfg(target_os = "windows")]
            VideoEncoder::Av1Gpu(e) => e.encode_bgra(bgra),
        }
    }

    pub fn force_keyframe(&mut self) {
        match self {
            VideoEncoder::Av1Cpu(e) => e.force_keyframe(),
            #[cfg(target_os = "windows")]
            VideoEncoder::Av1Gpu(e) => e.force_keyframe(),
        }
    }

    /// True if this emitted access unit is a keyframe the browser can start/recover decoding
    /// from — for AV1, a Sequence Header OBU (type 1) rides with each keyframe temporal unit.
    /// (H.264 IDR detection for the web-cast relay lives in `annexb_has_h264_idr`.)
    pub fn is_keyframe(&self, au: &[u8]) -> bool {
        match self {
            VideoEncoder::Av1Cpu(_) => obu_has_av1_keyframe(au),
            #[cfg(target_os = "windows")]
            VideoEncoder::Av1Gpu(_) => obu_has_av1_keyframe(au),
        }
    }

    /// Human-readable label of the backend actually in use (for logs/telemetry).
    pub fn backend_label(&self) -> &'static str {
        match self {
            VideoEncoder::Av1Cpu(_) => "CPU (SVT-AV1)",
            #[cfg(target_os = "windows")]
            VideoEncoder::Av1Gpu(_) => "GPU AV1 (Media Foundation)",
        }
    }
}

/// H.264: 1-byte NAL header; nal_type = byte & 0x1f; IDR slice = 5. Scans Annex-B start codes.
/// `pub` and codec-independent so the web-cast relay can re-derive the keyframe flag from a
/// browser-uploaded H.264 AU (never trust the caster's wire byte) — even though the server no
/// longer *encodes* H.264 itself.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn av1_keyframe_detection() {
        // OBU header byte = type<<3 | ext<<2 | has_size<<1 | reserved.
        // Sequence Header (type 1, has_size) + 0-byte payload → keyframe TU.
        assert!(obu_has_av1_keyframe(&[0x0A, 0x00]));
        // A lone Frame OBU (type 6) with no sequence header → not a keyframe.
        assert!(!obu_has_av1_keyframe(&[0x32, 0x00]));
        // Temporal Delimiter (type 2) + Frame (type 6), no seq header → not a keyframe.
        assert!(!obu_has_av1_keyframe(&[0x12, 0x00, 0x32, 0x00]));
        // Temporal Delimiter + Sequence Header + Frame → keyframe.
        assert!(obu_has_av1_keyframe(&[0x12, 0x00, 0x0A, 0x00, 0x32, 0x00]));
    }

    #[test]
    fn h264_idr_detection() {
        // Annex-B start code + NAL header; type = byte & 0x1f (5 = IDR).
        assert!(annexb_has_h264_idr(&[0, 0, 1, 0x65])); // 0x65 & 0x1f == 5
        assert!(!annexb_has_h264_idr(&[0, 0, 1, 0x61])); // type 1 (non-IDR)
    }
}
