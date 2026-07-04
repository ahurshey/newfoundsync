// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Alex Hurshman and the Newfoundsync contributors.

//! Cross-platform helper for the web-cast relay path (no encoder, no C deps).
//!
//! A browser web-cast uplink sends its OWN H.264 (the browser's licensed encoder); the server
//! merely relays those bytes. This tiny, dependency-free bitstream helper re-derives the keyframe
//! flag from a relayed access unit. It lives outside `codec.rs` so it stays available on platforms
//! (Linux) where the native AV1/VP9 encoders — and their C deps — aren't compiled.

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

#[cfg(test)]
mod tests {
    use super::annexb_has_h264_idr;

    #[test]
    fn h264_idr_detection() {
        // Annex-B start code + NAL header; type = byte & 0x1f (5 = IDR).
        assert!(annexb_has_h264_idr(&[0, 0, 1, 0x65])); // 0x65 & 0x1f == 5
        assert!(!annexb_has_h264_idr(&[0, 0, 1, 0x61])); // type 1 (non-IDR)
    }
}
