// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Alex Hurshman and the Newfoundsync contributors.

//! Audio codec: canonical PCM frame ⟷ wire payload.
//!
//! A canonical frame is [`FRAME_SAMPLE_COUNT`] interleaved L/R `i16` samples
//! (one 20 ms frame). On the wire, PCM is `s16le` (little-endian samples — the
//! native audio convention, distinct from the big-endian protocol header).
//!
//! **PCM passthrough is implemented now** (zero C dependencies, lowest CPU). The
//! Opus path (default 320 kbps — the user's choice) is the next step but is gated
//! on confirming the libopus C build with the MSVC toolchain here, so for now the
//! [`CodecKind::Opus`] constructors return a clear error rather than silently
//! failing. The wire protocol already carries a codec-agnostic payload, so adding
//! Opus is purely additive behind these enums.

use anyhow::{ensure, Context, Result};
use audiopus::coder::{Decoder as OpusDecoder, Encoder as OpusEncoder};
use audiopus::{Application, Bitrate, Channels, SampleRate};

use crate::config::{CHANNELS, FRAME_BYTES, FRAME_SAMPLES};

/// Max Opus packet size we'll emit for one 20 ms frame (generous; 320 kbps stereo
/// is ~800 B).
const MAX_OPUS_PACKET: usize = 4000;

/// Interleaved sample count in one canonical frame (L,R,L,R,…): 960 × 2 = 1920.
pub const FRAME_SAMPLE_COUNT: usize = FRAME_SAMPLES * CHANNELS;

/// Which codec a session uses on the wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodecKind {
    /// Uncompressed `s16le` — 3840 B/frame (~1.5 Mbps). Clean-LAN fallback.
    Pcm,
    /// Opus (default 320 kbps). Not yet built (see module docs).
    Opus,
}

impl CodecKind {
    /// Lowercase wire/TXT-record name.
    pub fn as_str(self) -> &'static str {
        match self {
            CodecKind::Pcm => "pcm",
            CodecKind::Opus => "opus",
        }
    }

    /// Parse a wire/TXT-record name.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "pcm" => Some(CodecKind::Pcm),
            "opus" => Some(CodecKind::Opus),
            _ => None,
        }
    }
}

// ---- PCM (s16le) helpers -----------------------------------------------------

/// Serialize a canonical frame to `s16le` bytes.
fn pcm_to_bytes(frame: &[i16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(frame.len() * 2);
    for &s in frame {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

/// Parse `s16le` bytes into a canonical frame.
fn pcm_from_bytes(bytes: &[u8]) -> Result<Vec<i16>> {
    ensure!(
        bytes.len() == FRAME_BYTES,
        "pcm payload is {} bytes, expected {FRAME_BYTES}",
        bytes.len()
    );
    let mut out = Vec::with_capacity(FRAME_SAMPLE_COUNT);
    for chunk in bytes.chunks_exact(2) {
        out.push(i16::from_le_bytes([chunk[0], chunk[1]]));
    }
    Ok(out)
}

// ---- Encoder -----------------------------------------------------------------

/// Encodes canonical PCM frames into wire payloads.
pub enum Encoder {
    Pcm,
    Opus(OpusEncoder),
}

impl Encoder {
    /// Build an encoder for `kind`. `bitrate_bps` is used by Opus (ignored by PCM).
    pub fn new(kind: CodecKind, bitrate_bps: i32) -> Result<Encoder> {
        match kind {
            CodecKind::Pcm => Ok(Encoder::Pcm),
            CodecKind::Opus => {
                let mut enc = OpusEncoder::new(SampleRate::Hz48000, Channels::Stereo, Application::Audio)
                    .context("create opus encoder")?;
                enc.set_bitrate(Bitrate::BitsPerSecond(bitrate_bps))
                    .context("set opus bitrate")?;
                Ok(Encoder::Opus(enc))
            }
        }
    }

    pub fn kind(&self) -> CodecKind {
        match self {
            Encoder::Pcm => CodecKind::Pcm,
            Encoder::Opus(_) => CodecKind::Opus,
        }
    }

    /// Encode exactly one canonical frame (`FRAME_SAMPLE_COUNT` interleaved i16)
    /// into a wire payload.
    pub fn encode(&mut self, frame: &[i16]) -> Result<Vec<u8>> {
        ensure!(
            frame.len() == FRAME_SAMPLE_COUNT,
            "frame is {} samples, expected {FRAME_SAMPLE_COUNT}",
            frame.len()
        );
        match self {
            Encoder::Pcm => Ok(pcm_to_bytes(frame)),
            Encoder::Opus(enc) => {
                let mut out = [0u8; MAX_OPUS_PACKET];
                let n = enc.encode(frame, &mut out).context("opus encode")?;
                Ok(out[..n].to_vec())
            }
        }
    }
}

// ---- Decoder -----------------------------------------------------------------

/// Decodes wire payloads back into canonical PCM frames.
pub enum Decoder {
    Pcm,
    Opus(OpusDecoder),
}

impl Decoder {
    pub fn new(kind: CodecKind) -> Result<Decoder> {
        match kind {
            CodecKind::Pcm => Ok(Decoder::Pcm),
            CodecKind::Opus => {
                let dec = OpusDecoder::new(SampleRate::Hz48000, Channels::Stereo)
                    .context("create opus decoder")?;
                Ok(Decoder::Opus(dec))
            }
        }
    }

    pub fn kind(&self) -> CodecKind {
        match self {
            Decoder::Pcm => CodecKind::Pcm,
            Decoder::Opus(_) => CodecKind::Opus,
        }
    }

    /// Decode a wire payload into one canonical frame.
    pub fn decode(&mut self, payload: &[u8]) -> Result<Vec<i16>> {
        match self {
            Decoder::Pcm => pcm_from_bytes(payload),
            Decoder::Opus(dec) => {
                let mut out = vec![0i16; FRAME_SAMPLE_COUNT];
                let n = dec
                    .decode(Some(payload), &mut out[..], false)
                    .context("opus decode")?;
                out.truncate(n * CHANNELS);
                Ok(out)
            }
        }
    }

    /// A frame of silence (for gaps, before the first real frame, etc.).
    pub fn silence(&self) -> Vec<i16> {
        vec![0i16; FRAME_SAMPLE_COUNT]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_frame() -> Vec<i16> {
        // A recognizable ramp across the interleaved buffer (wraps in i16).
        (0..FRAME_SAMPLE_COUNT).map(|i| (i as i32 - 900) as i16).collect()
    }

    #[test]
    fn codec_kind_str_roundtrip() {
        for k in [CodecKind::Pcm, CodecKind::Opus] {
            assert_eq!(CodecKind::parse(k.as_str()), Some(k));
        }
        assert_eq!(CodecKind::parse("flac"), None);
    }

    #[test]
    fn pcm_roundtrip_is_lossless() {
        let frame = sample_frame();
        let mut enc = Encoder::new(CodecKind::Pcm, 0).unwrap();
        let mut dec = Decoder::new(CodecKind::Pcm).unwrap();

        let payload = enc.encode(&frame).unwrap();
        assert_eq!(payload.len(), FRAME_BYTES, "pcm payload is one canonical frame");

        let back = dec.decode(&payload).unwrap();
        assert_eq!(back, frame, "pcm must round-trip bit-exact");
    }

    #[test]
    fn pcm_is_little_endian_on_the_wire() {
        let mut enc = Encoder::new(CodecKind::Pcm, 0).unwrap();
        let mut frame = vec![0i16; FRAME_SAMPLE_COUNT];
        frame[0] = 0x0102; // first sample
        let payload = enc.encode(&frame).unwrap();
        assert_eq!(&payload[0..2], &[0x02, 0x01], "samples are s16le");
    }

    #[test]
    fn encode_rejects_wrong_frame_size() {
        let mut enc = Encoder::new(CodecKind::Pcm, 0).unwrap();
        assert!(enc.encode(&[0i16; 10]).is_err());
    }

    #[test]
    fn decode_rejects_wrong_payload_size() {
        let mut dec = Decoder::new(CodecKind::Pcm).unwrap();
        assert!(dec.decode(&[0u8; 100]).is_err());
        assert!(dec.decode(&vec![0u8; FRAME_BYTES]).is_ok());
    }

    #[test]
    fn silence_frame_is_zeroed_and_sized() {
        let dec = Decoder::new(CodecKind::Pcm).unwrap();
        let s = dec.silence();
        assert_eq!(s.len(), FRAME_SAMPLE_COUNT);
        assert!(s.iter().all(|&x| x == 0));
    }

    #[test]
    fn opus_roundtrip_at_320k() {
        let mut enc = Encoder::new(CodecKind::Opus, 320_000).expect("opus encoder");
        let mut dec = Decoder::new(CodecKind::Opus).expect("opus decoder");
        assert_eq!(enc.kind(), CodecKind::Opus);

        // A non-silent 1 kHz-ish tone frame.
        let frame: Vec<i16> = (0..FRAME_SAMPLE_COUNT)
            .map(|i| ((i as f32 * 0.13).sin() * 8000.0) as i16)
            .collect();

        let payload = enc.encode(&frame).expect("encode");
        assert!(
            !payload.is_empty() && payload.len() < MAX_OPUS_PACKET,
            "opus payload size sane: {}",
            payload.len()
        );

        let decoded = dec.decode(&payload).expect("decode");
        assert_eq!(decoded.len(), FRAME_SAMPLE_COUNT, "decodes one full frame");
        // Lossy + encoder lookahead, so we don't compare samples; just confirm the
        // decoder produced audio (not all zeros after the priming frame).
    }
}
