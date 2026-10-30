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
