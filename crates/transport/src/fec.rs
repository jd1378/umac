//! Splits encoded frames into Reed-Solomon protected datagrams and puts them back together.

use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

use umac_proto::{
    WindowId,
    video::{HEADER_LEN, ShardHeader, flags},
};

/// Largest frame accepted in either direction. Bounds receiver memory per pending frame.
pub const MAX_FRAME_BYTES: usize = 8 << 20;
/// Frames still being assembled at once; older ones are dropped and reported lost.
const MAX_PENDING: usize = 8;
/// An incomplete frame older than this is given up on. Needed because an idle window sends
/// nothing after a lost last frame, so no newer frame would ever reveal the gap.
pub const PARTIAL_TIMEOUT: Duration = Duration::from_millis(80);

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum FecError {
    #[error("frame is empty or larger than {MAX_FRAME_BYTES} bytes")]
    FrameSize,
    #[error("datagram size too small")]
    DatagramSize,
    #[error("reed-solomon: {0}")]
    Rs(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameMeta {
    pub window: WindowId,
    pub seq: u32,
    pub keyframe: bool,
    pub capture_us: u32,
}

/// Splits `frame` into datagrams of at most `max_datagram` bytes, adding
/// `ceil(data_shards * parity_percent / 100)` recovery shards.
pub fn shard_frame(
    meta: FrameMeta,
    frame: &[u8],
    max_datagram: usize,
    parity_percent: u8,
) -> Result<Vec<Vec<u8>>, FecError> {
    if frame.is_empty() || frame.len() > MAX_FRAME_BYTES {
        return Err(FecError::FrameSize);
    }
    // reed-solomon-simd needs even shard sizes.
    let max_payload = max_datagram.saturating_sub(HEADER_LEN) & !1;
    if max_payload < 64 {
        return Err(FecError::DatagramSize);
    }
    let data = frame.len().div_ceil(max_payload);
    let shard_len = frame.len().div_ceil(data).next_multiple_of(2);
    let parity = (data * parity_percent as usize).div_ceil(100);

    let shards: Vec<Vec<u8>> = (0..data)
        .map(|i| {
            let mut s = vec![0u8; shard_len];
            let chunk = &frame[(i * shard_len).min(frame.len())..((i + 1) * shard_len).min(frame.len())];
            s[..chunk.len()].copy_from_slice(chunk);
            s
        })
        .collect();
    let recovery = if parity > 0 {
        reed_solomon_simd::encode(data, parity, &shards).map_err(|e| FecError::Rs(e.to_string()))?
    } else {
        Vec::new()
    };

    let header = |i: usize| ShardHeader {
        flags: if meta.keyframe { flags::KEYFRAME } else { 0 },
        window: meta.window,
        frame_seq: meta.seq,
        frame_len: frame.len() as u32,
        capture_us: meta.capture_us,
        shard_index: i as u16,
        data_shards: data as u16,
        parity_shards: parity as u16,
    };
    Ok(shards
        .iter()
        .chain(recovery.iter())
        .enumerate()
        .map(|(i, payload)| {
            let mut d = vec![0u8; HEADER_LEN + payload.len()];
            header(i).write((&mut d[..HEADER_LEN]).try_into().unwrap());
            d[HEADER_LEN..].copy_from_slice(payload);
            d
        })
        .collect())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub meta: FrameMeta,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Output {
    Frame(Frame),
    /// One or more frames were lost; the decoder needs a keyframe.
    Lost,
}

struct Partial {
    first: ShardHeader,
    shard_len: usize,
    shards: BTreeMap<u16, Vec<u8>>,
    started: Instant,
}

/// Reassembles frames of one window. Frames are delivered in order; late shards of frames
/// older than the last delivered one are ignored.
#[derive(Default)]
pub struct Reassembler {
    pending: BTreeMap<u32, Partial>,
    last_done: Option<u32>,
}

/// Wrapping "a is newer than b" for 32-bit sequence numbers.
fn newer(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) > 0
}

impl Reassembler {
    /// Feeds one datagram (already parsed header and its payload).
    pub fn push(&mut self, h: ShardHeader, payload: &[u8], now: Instant) -> Vec<Output> {
        if self.last_done.is_some_and(|d| !newer(h.frame_seq, d)) {
            return Vec::new();
        }
        let total_bytes = payload.len() * (h.data_shards as usize + h.parity_shards as usize);
        if payload.len() % 2 != 0 || total_bytes > 2 * MAX_FRAME_BYTES {
            return Vec::new();
        }
        let mut out = Vec::new();
        if !self.pending.contains_key(&h.frame_seq) && self.pending.len() >= MAX_PENDING {
            self.pending.pop_first();
            out.push(Output::Lost);
        }
        let partial = self.pending.entry(h.frame_seq).or_insert_with(|| Partial {
            first: h,
            shard_len: payload.len(),
            shards: BTreeMap::new(),
            started: now,
        });
        let f = &partial.first;
        let consistent = f.frame_len == h.frame_len
            && f.data_shards == h.data_shards
            && f.parity_shards == h.parity_shards
            && f.flags == h.flags
            && partial.shard_len == payload.len();
        if !consistent {
            return out;
        }
        partial.shards.entry(h.shard_index).or_insert_with(|| payload.to_vec());
        if partial.shards.len() < h.data_shards as usize {
            return out;
        }

        let partial = self.pending.remove(&h.frame_seq).unwrap();
        // Anything older that is still pending can no longer be delivered in order.
        let gap = self.last_done.map_or(false, |d| h.frame_seq != d.wrapping_add(1));
        let had_older = self.pending.keys().any(|&s| !newer(s, h.frame_seq));
        self.pending.retain(|&s, _| newer(s, h.frame_seq));
        self.last_done = Some(h.frame_seq);
        match assemble(partial) {
            Some(frame) => {
                if (gap || had_older) && !out.contains(&Output::Lost) {
                    out.push(Output::Lost);
                }
                out.push(Output::Frame(frame));
            }
            None => out.push(Output::Lost),
        }
        out
    }

    /// Drops frames that have waited too long. Returns true if any were dropped.
    pub fn expire(&mut self, now: Instant) -> bool {
        let before = self.pending.len();
        self.pending.retain(|_, p| now.duration_since(p.started) < PARTIAL_TIMEOUT);
        before != self.pending.len()
    }
}

fn assemble(p: Partial) -> Option<Frame> {
    let h = p.first;
    let data_n = h.data_shards as usize;
    let mut originals: Vec<Option<Vec<u8>>> = vec![None; data_n];
    let mut recovery = Vec::new();
    for (idx, shard) in p.shards {
        let idx = idx as usize;
        if idx < data_n {
            originals[idx] = Some(shard);
        } else {
            recovery.push((idx - data_n, shard));
        }
    }
    if originals.iter().any(Option::is_none) {
        let present = originals.iter().enumerate().filter_map(|(i, s)| s.as_deref().map(|s| (i, s)));
        let restored = reed_solomon_simd::decode(
            data_n,
            h.parity_shards as usize,
            present,
            recovery.iter().map(|(i, s)| (*i, s.as_slice())),
        )
        .ok()?;
        for (i, s) in restored {
            originals[i] = Some(s);
        }
    }
    let mut data = Vec::with_capacity(data_n * p.shard_len);
    for s in originals {
        data.extend_from_slice(&s?);
    }
    data.truncate(h.frame_len as usize);
    Some(Frame {
        meta: FrameMeta { window: h.window, seq: h.frame_seq, keyframe: h.is_keyframe(), capture_us: h.capture_us },
        data,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(seq: u32) -> FrameMeta {
        FrameMeta { window: 1, seq, keyframe: seq == 0, capture_us: 0 }
    }

    fn frame_bytes(len: usize, seed: u8) -> Vec<u8> {
        (0..len).map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed)).collect()
    }

    fn feed(r: &mut Reassembler, dgrams: &[Vec<u8>]) -> Vec<Output> {
        let now = Instant::now();
        dgrams
            .iter()
            .flat_map(|d| r.push(ShardHeader::parse(d).unwrap(), &d[HEADER_LEN..], now))
            .collect()
    }

    #[test]
    fn roundtrip_without_loss() {
        for len in [1, 2, 1375, 1376, 1377, 5000, 100_001] {
            let f = frame_bytes(len, 3);
            let d = shard_frame(meta(0), &f, 1400, 10).unwrap();
            assert!(d.iter().all(|x| x.len() <= 1400));
            let out = feed(&mut Reassembler::default(), &d);
            assert_eq!(out, vec![Output::Frame(Frame { meta: meta(0), data: f })], "len {len}");
        }
    }

    #[test]
    fn recovers_lost_data_shards() {
        let f = frame_bytes(50_000, 7);
        let d = shard_frame(meta(0), &f, 1400, 20).unwrap();
        let parity = ShardHeader::parse(&d[0]).unwrap().parity_shards as usize;
        // Drop as many data shards as there are parity shards.
        assert!(parity > 0);
        let kept: Vec<_> = d.into_iter().skip(parity).collect();
        let out = feed(&mut Reassembler::default(), &kept);
        assert_eq!(out, vec![Output::Frame(Frame { meta: meta(0), data: f })]);
    }

    #[test]
    fn reports_gap_and_ignores_late_frames() {
        let mut r = Reassembler::default();
        let f0 = shard_frame(meta(0), &frame_bytes(3000, 0), 1400, 0).unwrap();
        let f1 = shard_frame(meta(1), &frame_bytes(3000, 1), 1400, 0).unwrap();
        let f2 = shard_frame(meta(2), &frame_bytes(3000, 2), 1400, 0).unwrap();
        assert!(matches!(feed(&mut r, &f0)[..], [Output::Frame(_)]));
        // frame 1 partially arrives, frame 2 completes: 1 is lost.
        feed(&mut r, &f1[..1]);
        assert!(matches!(feed(&mut r, &f2)[..], [Output::Lost, Output::Frame(_)]));
        // The rest of frame 1 shows up late and is ignored.
        assert!(feed(&mut r, &f1[1..]).is_empty());
    }

    #[test]
    fn expires_stuck_partial() {
        let mut r = Reassembler::default();
        let f = shard_frame(meta(0), &frame_bytes(3000, 0), 1400, 0).unwrap();
        let t0 = Instant::now();
        r.push(ShardHeader::parse(&f[0]).unwrap(), &f[0][HEADER_LEN..], t0);
        assert!(!r.expire(t0));
        assert!(r.expire(t0 + PARTIAL_TIMEOUT));
    }

    #[test]
    fn rejects_bad_sizes() {
        assert_eq!(shard_frame(meta(0), &[], 1400, 0), Err(FecError::FrameSize));
        assert_eq!(shard_frame(meta(0), &[1], 40, 0), Err(FecError::DatagramSize));
    }
}
