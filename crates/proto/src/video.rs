//! Video datagram header.
//!
//! One encoded frame is split into `data_shards` equal-size shards plus `parity_shards`
//! Reed-Solomon shards; each shard rides in one QUIC datagram behind this header.
//! Any `data_shards` of the total reconstruct the frame.

use crate::WindowId;

pub const HEADER_LEN: usize = 24;
const MAGIC: u8 = 0xB7;

pub mod flags {
    pub const KEYFRAME: u8 = 1 << 0;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShardHeader {
    pub flags: u8,
    pub window: WindowId,
    /// Per-window frame counter; wraps.
    pub frame_seq: u32,
    /// Exact encoded frame length (last data shard is zero padded).
    pub frame_len: u32,
    /// Host capture timestamp in microseconds, for latency measurement only.
    pub capture_us: u32,
    pub shard_index: u16,
    pub data_shards: u16,
    pub parity_shards: u16,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum HeaderError {
    #[error("datagram shorter than header")]
    Short,
    #[error("bad magic")]
    Magic,
    #[error("inconsistent shard fields")]
    Invalid,
}

impl ShardHeader {
    pub fn is_keyframe(&self) -> bool {
        self.flags & flags::KEYFRAME != 0
    }

    pub fn write(&self, out: &mut [u8; HEADER_LEN]) {
        out[0] = MAGIC;
        out[1] = self.flags;
        out[2..6].copy_from_slice(&self.window.to_le_bytes());
        out[6..10].copy_from_slice(&self.frame_seq.to_le_bytes());
        out[10..14].copy_from_slice(&self.frame_len.to_le_bytes());
        out[14..18].copy_from_slice(&self.capture_us.to_le_bytes());
        out[18..20].copy_from_slice(&self.shard_index.to_le_bytes());
        out[20..22].copy_from_slice(&self.data_shards.to_le_bytes());
        out[22..24].copy_from_slice(&self.parity_shards.to_le_bytes());
    }

    /// Parses and sanity checks a header; the payload is `datagram[HEADER_LEN..]`.
    pub fn parse(datagram: &[u8]) -> Result<Self, HeaderError> {
        if datagram.len() < HEADER_LEN {
            return Err(HeaderError::Short);
        }
        if datagram[0] != MAGIC {
            return Err(HeaderError::Magic);
        }
        let u16_at = |i: usize| u16::from_le_bytes([datagram[i], datagram[i + 1]]);
        let u32_at = |i: usize| u32::from_le_bytes(datagram[i..i + 4].try_into().unwrap());
        let h = Self {
            flags: datagram[1],
            window: u32_at(2),
            frame_seq: u32_at(6),
            frame_len: u32_at(10),
            capture_us: u32_at(14),
            shard_index: u16_at(18),
            data_shards: u16_at(20),
            parity_shards: u16_at(22),
        };
        let total = h.data_shards as u32 + h.parity_shards as u32;
        let shard_len = (datagram.len() - HEADER_LEN) as u64;
        let ok = h.data_shards > 0
            && (h.shard_index as u32) < total
            && shard_len > 0
            && h.frame_len > 0
            && h.frame_len as u64 <= shard_len * h.data_shards as u64;
        if !ok {
            return Err(HeaderError::Invalid);
        }
        Ok(h)
    }
}

/// PyroWave datagrams: one encoder packet (a run of whole blocks) per datagram. Every packet
/// decodes on its own, so there is no FEC and no keyframe: a lost packet only blurs the blocks
/// it carried. The magic differs from [`ShardHeader`]'s so the two can never be confused.
pub const PYRO_HEADER_LEN: usize = 22;
const PYRO_MAGIC: u8 = 0xB8;

pub mod pyro_flags {
    /// 4:4:4 chroma; otherwise 4:2:0.
    pub const CHROMA_444: u8 = 1 << 0;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PyroHeader {
    pub flags: u8,
    pub window: WindowId,
    /// Per-window frame counter; wraps.
    pub frame_seq: u32,
    /// Host capture timestamp in microseconds (same clock as [`ShardHeader::capture_us`]).
    pub capture_us: u32,
    pub packet_index: u16,
    /// Packets in this frame; a frame is complete once all arrived.
    pub packet_count: u16,
    /// Coded size; a change means the encoder was reconfigured.
    pub width: u16,
    pub height: u16,
}

impl PyroHeader {
    pub fn write(&self, out: &mut [u8; PYRO_HEADER_LEN]) {
        out[0] = PYRO_MAGIC;
        out[1] = self.flags;
        out[2..6].copy_from_slice(&self.window.to_le_bytes());
        out[6..10].copy_from_slice(&self.frame_seq.to_le_bytes());
        out[10..14].copy_from_slice(&self.capture_us.to_le_bytes());
        out[14..16].copy_from_slice(&self.packet_index.to_le_bytes());
        out[16..18].copy_from_slice(&self.packet_count.to_le_bytes());
        out[18..20].copy_from_slice(&self.width.to_le_bytes());
        out[20..22].copy_from_slice(&self.height.to_le_bytes());
    }

    /// Parses and sanity checks a header; the packet is `datagram[PYRO_HEADER_LEN..]`.
    pub fn parse(datagram: &[u8]) -> Result<Self, HeaderError> {
        if datagram.len() <= PYRO_HEADER_LEN {
            return Err(HeaderError::Short);
        }
        if datagram[0] != PYRO_MAGIC {
            return Err(HeaderError::Magic);
        }
        let u16_at = |i: usize| u16::from_le_bytes([datagram[i], datagram[i + 1]]);
        let u32_at = |i: usize| u32::from_le_bytes(datagram[i..i + 4].try_into().unwrap());
        let h = Self {
            flags: datagram[1],
            window: u32_at(2),
            frame_seq: u32_at(6),
            capture_us: u32_at(10),
            packet_index: u16_at(14),
            packet_count: u16_at(16),
            width: u16_at(18),
            height: u16_at(20),
        };
        if h.packet_count == 0 || h.packet_index >= h.packet_count || h.width == 0 || h.height == 0 {
            return Err(HeaderError::Invalid);
        }
        Ok(h)
    }
}

/// Which header a video datagram carries.
pub fn is_pyro(datagram: &[u8]) -> bool {
    datagram.first() == Some(&PYRO_MAGIC)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> ShardHeader {
        ShardHeader {
            flags: flags::KEYFRAME,
            window: 42,
            frame_seq: 9,
            frame_len: 2500,
            capture_us: 123,
            shard_index: 3,
            data_shards: 3,
            parity_shards: 1,
        }
    }

    #[test]
    fn pyro_roundtrip_and_validation() {
        let h = PyroHeader { flags: 0, window: 7, frame_seq: 3, capture_us: 9, packet_index: 1, packet_count: 4, width: 1920, height: 1080 };
        let mut buf = vec![0u8; PYRO_HEADER_LEN + 100];
        h.write((&mut buf[..PYRO_HEADER_LEN]).try_into().unwrap());
        assert!(is_pyro(&buf));
        assert_eq!(PyroHeader::parse(&buf).unwrap(), h);
        assert_eq!(PyroHeader::parse(&buf[..PYRO_HEADER_LEN]), Err(HeaderError::Short));
        let bad = PyroHeader { packet_index: 4, ..h };
        bad.write((&mut buf[..PYRO_HEADER_LEN]).try_into().unwrap());
        assert_eq!(PyroHeader::parse(&buf), Err(HeaderError::Invalid));
    }

    #[test]
    fn roundtrip() {
        let h = sample();
        let mut buf = vec![0u8; HEADER_LEN + 1000];
        h.write((&mut buf[..HEADER_LEN]).try_into().unwrap());
        assert_eq!(ShardHeader::parse(&buf).unwrap(), h);
    }

    #[test]
    fn rejects_bad_fields() {
        let mut h = sample();
        h.shard_index = 4;
        let mut buf = vec![0u8; HEADER_LEN + 1000];
        h.write((&mut buf[..HEADER_LEN]).try_into().unwrap());
        assert_eq!(ShardHeader::parse(&buf), Err(HeaderError::Invalid));

        // frame_len would need more shards than declared.
        let mut h = sample();
        h.frame_len = 3001;
        h.shard_index = 0;
        h.write((&mut buf[..HEADER_LEN]).try_into().unwrap());
        assert_eq!(ShardHeader::parse(&buf), Err(HeaderError::Invalid));
        assert_eq!(ShardHeader::parse(&buf[..10]), Err(HeaderError::Short));
    }
}
