// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Alex Hurshman and the Newfoundsync contributors.

//! Software VP9 encoder via libvpx (the royalty-free fallback to AV1).
//!
//! Realtime, low-latency VP9 — `VPX_DL_REALTIME`, CBR, no lookahead, row-mt — the same
//! libvpx path WebRTC uses. Takes tightly-packed BGRA, converts to I420, and emits raw VP9
//! elementary frames the browser's WebCodecs `vp09` decoder reads directly (no IVF container).
//!
//! libvpx is a C library supplied at build time (Windows: vcpkg `libvpx:x64-windows-static`).
//! On a machine with no VP9 hardware encoder (i.e. everything except recent Intel QuickSync),
//! this is the only VP9 path — it runs on the CPU.

use std::os::raw::c_int;

use anyhow::{bail, Result};
use vpx_sys::*;

use crate::video::codec::bgra_to_i420;

pub struct Vp9Encoder {
    ctx: vpx_codec_ctx_t,
    img: vpx_image_t,
    width: usize,
    height: usize,
    cw: usize,
    ch: usize,
    y: Vec<u8>,
    u: Vec<u8>,
    v: Vec<u8>,
    frame_idx: i64,
    pending_key: bool,
}

impl Vp9Encoder {
    pub fn new(width: u32, height: u32, fps: u32, bitrate_kbps: u32) -> Result<Self> {
        let (w, h) = (width as usize, height as usize);
        if w % 2 != 0 || h % 2 != 0 {
            bail!("dimensions must be even ({width}x{height})");
        }
        unsafe {
            let iface = vpx_codec_vp9_cx();
            let mut cfg: vpx_codec_enc_cfg_t = std::mem::zeroed();
            if vpx_codec_enc_config_default(iface, &mut cfg, 0) != vpx_codec_err_t::VPX_CODEC_OK {
                bail!("vpx_codec_enc_config_default(vp9) failed");
            }
            cfg.g_w = width;
            cfg.g_h = height;
            cfg.g_timebase.num = 1;
            cfg.g_timebase.den = fps.max(1) as c_int;
            cfg.rc_target_bitrate = bitrate_kbps.max(1);
            cfg.rc_end_usage = vpx_rc_mode::VPX_CBR; // live rate control (low-delay)
            cfg.g_lag_in_frames = 0; // no lookahead → minimal latency
            cfg.g_error_resilient = 0;
            cfg.kf_mode = vpx_kf_mode::VPX_KF_AUTO;
            cfg.kf_max_dist = (fps.max(1) * 2) as u32; // ~2s GOP so joiners recover
            cfg.g_threads = std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4)
                .min(16) as u32;

            let mut ctx: vpx_codec_ctx_t = std::mem::zeroed();
            if vpx_codec_enc_init_ver(
                &mut ctx,
                iface,
                &cfg,
                0,
                VPX_ENCODER_ABI_VERSION as c_int,
            ) != vpx_codec_err_t::VPX_CODEC_OK
            {
                bail!("vpx_codec_enc_init(vp9) failed");
            }
            // Realtime / low-latency controls (same as WebRTC's VP9 path).
            vpx_codec_control_(&mut ctx, vp8e_enc_control_id::VP8E_SET_CPUUSED as c_int, 7 as c_int);
            vpx_codec_control_(&mut ctx, vp8e_enc_control_id::VP8E_SET_ENABLEAUTOALTREF as c_int, 0 as c_int);
            vpx_codec_control_(&mut ctx, vp8e_enc_control_id::VP9E_SET_ROW_MT as c_int, 1 as c_int);
            vpx_codec_control_(&mut ctx, vp8e_enc_control_id::VP9E_SET_TILE_COLUMNS as c_int, 1 as c_int);

            let mut img: vpx_image_t = std::mem::zeroed();
            if vpx_img_alloc(&mut img, vpx_img_fmt::VPX_IMG_FMT_I420, width, height, 1).is_null() {
                vpx_codec_destroy(&mut ctx);
                bail!("vpx_img_alloc failed");
            }
            let cw = w.div_ceil(2);
            let ch = h.div_ceil(2);
            Ok(Vp9Encoder {
                ctx,
                img,
                width: w,
                height: h,
                cw,
                ch,
                y: vec![0u8; w * h],
                u: vec![128u8; cw * ch],
                v: vec![128u8; cw * ch],
                frame_idx: 0,
                pending_key: false,
            })
        }
    }

    /// Encode one tightly-packed BGRA frame → raw VP9 elementary bytes (may be empty).
    pub fn encode_bgra(&mut self, bgra: &[u8]) -> Result<Vec<u8>> {
        debug_assert_eq!(bgra.len(), self.width * self.height * 4);
        bgra_to_i420(
            bgra, self.width, self.height, self.cw, self.ch, &mut self.y, &mut self.u, &mut self.v,
        );
        unsafe {
            // Copy our tightly-packed planes into libvpx's (strided) image buffers.
            copy_plane(self.img.planes[0], self.img.stride[0], &self.y, self.width, self.height);
            copy_plane(self.img.planes[1], self.img.stride[1], &self.u, self.cw, self.ch);
            copy_plane(self.img.planes[2], self.img.stride[2], &self.v, self.cw, self.ch);

            let flags: vpx_enc_frame_flags_t = if std::mem::take(&mut self.pending_key) {
                VPX_EFLAG_FORCE_KF as vpx_enc_frame_flags_t
            } else {
                0
            };
            let r = vpx_codec_encode(
                &mut self.ctx,
                &self.img,
                self.frame_idx,
                1,
                flags,
                VPX_DL_REALTIME as _,
            );
            if r != vpx_codec_err_t::VPX_CODEC_OK {
                bail!("vpx_codec_encode(vp9) failed");
            }
            self.frame_idx += 1;

            let mut out = Vec::new();
            let mut iter: vpx_codec_iter_t = std::ptr::null();
            loop {
                let pkt = vpx_codec_get_cx_data(&mut self.ctx, &mut iter);
                if pkt.is_null() {
                    break;
                }
                if (*pkt).kind == vpx_codec_cx_pkt_kind::VPX_CODEC_CX_FRAME_PKT {
                    let frame = (*pkt).data.frame;
                    if !frame.buf.is_null() && frame.sz > 0 {
                        out.extend_from_slice(std::slice::from_raw_parts(
                            frame.buf as *const u8,
                            frame.sz,
                        ));
                    }
                }
            }
            Ok(out)
        }
    }

    /// Make the next encoded frame a keyframe (on join / resolution switch).
    pub fn force_keyframe(&mut self) {
        self.pending_key = true;
    }
}

/// Copy a tightly-packed plane (`w` bytes/row) into a libvpx destination of `stride` bytes/row.
unsafe fn copy_plane(dst: *mut u8, stride: c_int, src: &[u8], w: usize, h: usize) {
    let stride = stride as usize;
    for row in 0..h {
        std::ptr::copy_nonoverlapping(src[row * w..].as_ptr(), dst.add(row * stride), w);
    }
}

impl Drop for Vp9Encoder {
    fn drop(&mut self) {
        unsafe {
            vpx_img_free(&mut self.img);
            vpx_codec_destroy(&mut self.ctx);
        }
    }
}

/// VP9 keyframe detection from the raw elementary frame's uncompressed header (profile 0):
/// frame_marker (bits 7..6) == 0b10, then profile (2 bits), show_existing_frame (bit 3),
/// frame_type (bit 2) == 0 ⇒ KEY_FRAME. `pub` so the wire keyframe flag can be re-derived
/// from the encoded bytes, matching the other codecs' detectors.
pub fn vp9_frame_is_keyframe(frame: &[u8]) -> bool {
    let Some(&b) = frame.first() else { return false };
    if (b >> 6) != 0b10 {
        return false; // not a VP9 frame marker
    }
    let show_existing_frame = (b >> 3) & 1;
    if show_existing_frame == 1 {
        return false; // a repeat of a previously coded frame, not a keyframe
    }
    (b >> 2) & 1 == 0 // frame_type: 0 = KEY_FRAME (profile 0)
}

#[cfg(test)]
mod tests {
    use super::vp9_frame_is_keyframe;

    #[test]
    fn vp9_keyframe_detection() {
        // Profile-0 uncompressed header, byte 0: marker(2)=10 | profile_lo | profile_hi |
        // show_existing_frame | frame_type | ...
        assert!(vp9_frame_is_keyframe(&[0x80])); // marker=10, show_existing=0, frame_type=0 → KEY
        assert!(!vp9_frame_is_keyframe(&[0x84])); // frame_type=1 → inter frame
        assert!(!vp9_frame_is_keyframe(&[0x88])); // show_existing_frame=1 → repeat, not a keyframe
        assert!(!vp9_frame_is_keyframe(&[0x00])); // frame_marker != 10 → not a VP9 frame
        assert!(!vp9_frame_is_keyframe(&[])); // empty
    }
}
