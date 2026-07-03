// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Alex Hurshman and the Newfoundsync contributors.

//! On-wire frame codec — the load-bearing byte contract.
//!
//! Ported field-for-field from `ensemble/internal/stream/wire.go`. Do NOT reorder
//! header fields: the clock and transport layers serialize against this exact
//! layout, and it matches the reference project's test vectors. Hand-rolled
//! big-endian byte ops (no serde) so the layout stays auditable and zero-alloc.

use thiserror::Error;

/// Magic byte starting every framed packet (audio / fec / clock / control).
pub const MAGIC: u8 = 0xE5;

/// Fixed on-wire size of [`Header`], in bytes.
pub const HEADER_SIZE: usize = 24;

/// Packet types. Numbering kept identical to `ensemble` so byte tables match.
///
/// The `0x30..=0x41` master→client control plane from `ensemble` is intentionally
/// **not** ported: in Newfoundsync the client controls its own subscribe / volume
/// / delay, so there is no master→client orchestration.
pub mod msg {
    /// Audio frame: header + Opus (or PCM) payload. seq/pts/gen meaningful.
    pub const AUDIO: u8 = 0x01;
    /// XOR parity for FEC: header + parity payload. (Reserved; not emitted in v1.)
    pub const FEC: u8 = 0x02;
    /// Video frame: header + video sub-header + (fragment of) an encoded frame.
    /// Reserved for the video feature; sub-header/fragmentation per the pipeline design.
    pub const VIDEO: u8 = 0x03;
    /// Clock request (client → server).
    pub const CLOCK_REQ: u8 = 0x10;
    /// Clock reply (server → client).
    pub const CLOCK_RSP: u8 = 0x11;
    /// Subscribe / keepalive (client → server); payload flag: prime-me.
    pub const HELLO: u8 = 0x20;
    /// "Leaving, stop sending" (client → server).
    pub const BYE: u8 = 0x21;
    /// "Got lost, re-prime and resume" (client → server).
    pub const RESTART: u8 = 0x22;
    /// "Gen/settings changed: resubscribe" (server → client); payload flag: stop.
    pub const RECONFIG: u8 = 0x23;
}

/// HELLO/RESTART payload flag: please burst-prime me.
pub const FLAG_PRIME_ME: u8 = 0x01;
/// RECONFIG payload flag: this is the stop / end-of-session notice.
pub const FLAG_STOP: u8 = 0x01;

/// The common frame header preceding every payload.
///
/// Byte layout (big-endian, offsets in bytes):
///
/// ```text
/// off size field        meaning
///   0    1  magic        0xE5, sanity / framing
///   1    1  typ          packet type
///   2    4  gen          session generation (u32); receivers drop stale gens
///   6    8  seq          frame sequence number (u64), 0-based per session
///  14    8  pts          presentation timestamp, master-clock ns (i64)
///  22    2  payload_len  payload byte count following the header (u16)
///  ----      total 24 bytes ----
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Header {
    pub magic: u8,
    pub typ: u8,
    pub gen: u32,
    pub seq: u64,
    pub pts: i64,
    pub payload_len: u16,
}

impl Header {
    /// New header with the magic byte set and `payload_len` left 0 (filled by
    /// [`append_frame`] or set explicitly).
    pub fn new(typ: u8, gen: u32, seq: u64, pts: i64) -> Self {
        Header {
            magic: MAGIC,
            typ,
            gen,
            seq,
            pts,
            payload_len: 0,
        }
    }

    /// Write the 24-byte header into `dst[..HEADER_SIZE]` (big-endian).
    ///
    /// # Panics
    /// Panics if `dst.len() < HEADER_SIZE`.
    pub fn encode(&self, dst: &mut [u8]) {
        assert!(dst.len() >= HEADER_SIZE, "header dst too short");
        dst[0] = self.magic;
        dst[1] = self.typ;
        dst[2..6].copy_from_slice(&self.gen.to_be_bytes());
        dst[6..14].copy_from_slice(&self.seq.to_be_bytes());
        dst[14..22].copy_from_slice(&self.pts.to_be_bytes());
        dst[22..24].copy_from_slice(&self.payload_len.to_be_bytes());
    }

    /// Parse a 24-byte header from `src`. Does not validate magic/type.
    pub fn decode(src: &[u8]) -> Result<Header, WireError> {
        if src.len() < HEADER_SIZE {
            return Err(WireError::Short);
        }
        Ok(Header {
            magic: src[0],
            typ: src[1],
            gen: u32::from_be_bytes(src[2..6].try_into().unwrap()),
            seq: u64::from_be_bytes(src[6..14].try_into().unwrap()),
            pts: i64::from_be_bytes(src[14..22].try_into().unwrap()),
            payload_len: u16::from_be_bytes(src[22..24].try_into().unwrap()),
        })
    }
}

/// Append header + payload to `dst`, setting `payload_len = payload.len()`.
pub fn append_frame(mut header: Header, dst: &mut Vec<u8>, payload: &[u8]) {
    header.payload_len = payload.len() as u16;
    let mut hdr = [0u8; HEADER_SIZE];
    header.encode(&mut hdr);
    dst.extend_from_slice(&hdr);
    dst.extend_from_slice(payload);
}

/// Parse header + exactly `payload_len` payload bytes from a single datagram.
/// Returns the header and a sub-slice of `buf` aliasing the payload.
pub fn decode_frame(buf: &[u8]) -> Result<(Header, &[u8]), WireError> {
    let h = Header::decode(buf)?;
    if h.magic != MAGIC {
        return Err(WireError::BadMagic);
    }
    let end = HEADER_SIZE + h.payload_len as usize;
    if buf.len() < end {
        return Err(WireError::Short);
    }
    Ok((h, &buf[HEADER_SIZE..end]))
}

/// FEC parity: `dst[i] ^= src[i]` for `i < len(src)`; shorter `src` is treated as
/// zero-padded (no-op past its end).
pub fn xor_into(dst: &mut [u8], src: &[u8]) {
    let n = src.len().min(dst.len());
    for i in 0..n {
        dst[i] ^= src[i];
    }
}

/// Wire-decoding errors.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum WireError {
    #[error("wire: buffer shorter than header or declared payload")]
    Short,
    #[error("wire: bad magic")]
    BadMagic,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Golden bytes: a header with distinctive fields must encode to exactly these
    /// 24 big-endian bytes (and decode back identically).
    #[test]
    fn header_golden_bytes_roundtrip() {
        let h = Header {
            magic: MAGIC,
            typ: msg::AUDIO,
            gen: 0x0102_0304,
            seq: 0x0102_0304_0506_0708,
            pts: 0x1122_3344_5566_7788,
            payload_len: 0x00FF,
        };
        let mut buf = [0u8; HEADER_SIZE];
        h.encode(&mut buf);

        let expected: [u8; HEADER_SIZE] = [
            0xE5, // magic
            0x01, // type
            0x01, 0x02, 0x03, 0x04, // gen
            0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, // seq
            0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, // pts
            0x00, 0xFF, // payload_len
        ];
        assert_eq!(buf, expected, "header byte layout must match ensemble");

        let decoded = Header::decode(&buf).unwrap();
        assert_eq!(decoded, h);
    }

    #[test]
    fn append_and_decode_frame() {
        let payload = b"hello-audio-payload";
        let mut buf = Vec::new();
        append_frame(Header::new(msg::AUDIO, 7, 42, 1_000_000), &mut buf, payload);

        assert_eq!(buf.len(), HEADER_SIZE + payload.len());
        let (h, p) = decode_frame(&buf).unwrap();
        assert_eq!(h.typ, msg::AUDIO);
        assert_eq!(h.gen, 7);
        assert_eq!(h.seq, 42);
        assert_eq!(h.pts, 1_000_000);
        assert_eq!(h.payload_len as usize, payload.len());
        assert_eq!(p, payload);
    }

    #[test]
    fn decode_frame_rejects_bad_magic() {
        let mut buf = vec![0u8; HEADER_SIZE];
        // valid-length header but magic 0x00
        assert_eq!(decode_frame(&buf), Err(WireError::BadMagic));
        buf[0] = MAGIC;
        // magic ok, payload_len 0 → empty payload ok
        let (h, p) = decode_frame(&buf).unwrap();
        assert_eq!(h.payload_len, 0);
        assert!(p.is_empty());
    }

    #[test]
    fn decode_frame_rejects_short_buffers() {
        assert_eq!(Header::decode(&[0u8; 10]), Err(WireError::Short));
        // claims 100-byte payload but buffer only holds the header
        let mut buf = vec![0u8; HEADER_SIZE];
        buf[0] = MAGIC;
        buf[22..24].copy_from_slice(&100u16.to_be_bytes());
        assert_eq!(decode_frame(&buf), Err(WireError::Short));
    }

    #[test]
    fn xor_into_parity_and_recovery() {
        let a = [0x11u8, 0x22, 0x33, 0x44];
        let b = [0xAAu8, 0xBB, 0xCC, 0xDD];
        // parity = a ^ b
        let mut parity = [0u8; 4];
        xor_into(&mut parity, &a);
        xor_into(&mut parity, &b);
        // recover a from parity ^ b
        let mut recovered = parity;
        xor_into(&mut recovered, &b);
        assert_eq!(recovered, a);
    }

    #[test]
    fn xor_into_zero_pads_shorter_src() {
        let mut dst = [0xFFu8; 4];
        xor_into(&mut dst, &[0x0F, 0x0F]); // only first two bytes affected
        assert_eq!(dst, [0xF0, 0xF0, 0xFF, 0xFF]);
    }
}
