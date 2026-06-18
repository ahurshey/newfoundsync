//! Clock-datagram codec: header + `t1|t2|t3` (three big-endian i64).
//!
//! Ported from `ensemble/internal/clock/payload.go`. A clock packet is a normal
//! [`Header`] (with `pts = 0`) followed by a fixed 24-byte payload.

use crate::proto::{Header, WireError, HEADER_SIZE, MAGIC};

/// Fixed clock payload: `t1|t2|t3`, three big-endian i64.
pub const CLOCK_PAYLOAD_SIZE: usize = 24;
/// Total clock datagram: header + payload (24 + 24 = 48).
pub const CLOCK_PACKET_SIZE: usize = HEADER_SIZE + CLOCK_PAYLOAD_SIZE;

/// Write a clock datagram into `dst[..CLOCK_PACKET_SIZE]`.
///
/// `typ` is [`proto::msg::CLOCK_REQ`] or [`proto::msg::CLOCK_RSP`]; `gen` is the
/// session generation; `seq` is the probe sequence (echoed in replies). On a
/// request, `t2` and `t3` are 0.
///
/// # Panics
/// Panics if `dst.len() < CLOCK_PACKET_SIZE`.
pub fn encode_clock(dst: &mut [u8], typ: u8, gen: u32, seq: u64, t1: i64, t2: i64, t3: i64) {
    assert!(dst.len() >= CLOCK_PACKET_SIZE, "clock dst too short");
    let mut h = Header::new(typ, gen, seq, 0);
    h.payload_len = CLOCK_PAYLOAD_SIZE as u16;
    h.encode(dst);
    dst[HEADER_SIZE..HEADER_SIZE + 8].copy_from_slice(&t1.to_be_bytes());
    dst[HEADER_SIZE + 8..HEADER_SIZE + 16].copy_from_slice(&t2.to_be_bytes());
    dst[HEADER_SIZE + 16..HEADER_SIZE + 24].copy_from_slice(&t3.to_be_bytes());
}

/// A decoded clock datagram.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClockPacket {
    pub header: Header,
    pub t1: i64,
    pub t2: i64,
    pub t3: i64,
}

/// Parse a clock datagram, validating magic, payload length, and buffer size.
pub fn decode_clock(pkt: &[u8]) -> Result<ClockPacket, WireError> {
    let header = Header::decode(pkt)?;
    if header.magic != MAGIC {
        return Err(WireError::BadMagic);
    }
    if (header.payload_len as usize) < CLOCK_PAYLOAD_SIZE || pkt.len() < CLOCK_PACKET_SIZE {
        return Err(WireError::Short);
    }
    let t1 = i64::from_be_bytes(pkt[HEADER_SIZE..HEADER_SIZE + 8].try_into().unwrap());
    let t2 = i64::from_be_bytes(pkt[HEADER_SIZE + 8..HEADER_SIZE + 16].try_into().unwrap());
    let t3 = i64::from_be_bytes(pkt[HEADER_SIZE + 16..HEADER_SIZE + 24].try_into().unwrap());
    Ok(ClockPacket { header, t1, t2, t3 })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto;

    #[test]
    fn clock_roundtrip() {
        let mut buf = [0u8; CLOCK_PACKET_SIZE];
        encode_clock(
            &mut buf,
            proto::msg::CLOCK_RSP,
            9,
            12345,
            0x0011_2233_4455_6677,
            0x7766_5544_3322_1100,
            0x0102_0304_0506_0708,
        );

        let p = decode_clock(&buf).unwrap();
        assert_eq!(p.header.typ, proto::msg::CLOCK_RSP);
        assert_eq!(p.header.gen, 9);
        assert_eq!(p.header.seq, 12345);
        assert_eq!(p.header.pts, 0, "clock packets carry pts=0");
        assert_eq!(p.header.payload_len as usize, CLOCK_PAYLOAD_SIZE);
        assert_eq!(p.t1, 0x0011_2233_4455_6677);
        assert_eq!(p.t2, 0x7766_5544_3322_1100);
        assert_eq!(p.t3, 0x0102_0304_0506_0708);
    }

    #[test]
    fn request_has_zero_t2_t3() {
        let mut buf = [0u8; CLOCK_PACKET_SIZE];
        encode_clock(&mut buf, proto::msg::CLOCK_REQ, 0, 1, 42, 0, 0);
        let p = decode_clock(&buf).unwrap();
        assert_eq!((p.t1, p.t2, p.t3), (42, 0, 0));
    }

    #[test]
    fn rejects_short_and_bad_magic() {
        assert_eq!(decode_clock(&[0u8; 10]), Err(WireError::Short));
        let mut buf = [0u8; CLOCK_PACKET_SIZE]; // magic 0
        assert_eq!(decode_clock(&buf), Err(WireError::BadMagic));
        // valid magic but truncated buffer
        encode_clock(&mut buf, proto::msg::CLOCK_REQ, 0, 0, 0, 0, 0);
        assert_eq!(decode_clock(&buf[..40]), Err(WireError::Short));
    }
}
