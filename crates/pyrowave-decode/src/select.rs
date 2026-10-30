//! Sending only what changed. Every PyroWave block is independent, so the sender can keep the
//! blocks whose synthesized area touches a changed region and drop the rest; the decoder keeps
//! the previous coefficients of blocks that are absent. Blocks the sender keeps but the encoder
//! left out (they quantized to zero) are sent as empty blocks, which zero their coefficients.

use std::ops::Range;

use crate::{
    bitstream::{Error, Item, parse_items},
    layout::Layout,
};

const BLOCK: i64 = 32;
/// Half the longest CDF 9/7 synthesis filter (9 taps), in samples of the level being produced.
const FILTER_RADIUS: i64 = 4;
/// Every block is resent at least this often while a window keeps changing, so a lost block
/// or a missed dirty rect heals within about half a second.
pub const REFRESH_FRAMES: u64 = 30;

/// A rectangle in surface pixels: `x0..x1`, `y0..y1`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x0: i64,
    pub y0: i64,
    pub x1: i64,
    pub y1: i64,
}

impl Rect {
    pub fn intersects(&self, o: &Rect) -> bool {
        self.x0 < o.x1 && o.x0 < self.x1 && self.y0 < o.y1 && o.y0 < self.y1
    }
}

/// Surface area a block can change after synthesis, in luma pixels. A coefficient of level `l`
/// sits on a grid of `s = 2^(l+1)` pixels; each of the `l + 1` synthesis steps doubles the grid
/// and spreads by the filter radius, so the area grows by `4 * (s - 1)` on every side, plus one
/// coefficient of slack. Chroma of 4:2:0 lives on a half resolution plane with one step less,
/// which this bound also covers.
pub fn footprint(layout: &Layout, index: u32) -> Option<Rect> {
    area(layout, index, true)
}

/// The pixels a block's coefficients sit on, grown by the synthesis spread when `spread`.
pub fn area(layout: &Layout, index: u32, spread: bool) -> Option<Rect> {
    let band = layout.bands.iter().find(|b| (b.first_block..b.first_block + b.blocks_x * b.blocks_y).contains(&index))?;
    let rel = index - band.first_block;
    let (bx, by) = ((rel % band.blocks_x) as i64, (rel / band.blocks_x) as i64);
    let s = 1i64 << (band.level + 1);
    let r = if spread { FILTER_RADIUS * (s - 1) + s } else { 0 };
    Some(Rect { x0: bx * BLOCK * s - r, y0: by * BLOCK * s - r, x1: (bx + 1) * BLOCK * s + r, y1: (by + 1) * BLOCK * s + r })
}

/// Picks the blocks to send for each frame.
pub struct Selector {
    footprints: Vec<Rect>,
    frame: u64,
}

impl Selector {
    pub fn new(layout: &Layout) -> Self {
        let footprints = (0..layout.block_count).map(|i| footprint(layout, i).expect("index in range")).collect();
        Self { footprints, frame: 0 }
    }

    /// Blocks to send: all of them for a full frame (or without dirty information), else those
    /// touching a dirty rect plus this frame's share of the rolling refresh.
    pub fn keep(&mut self, dirty: Option<&[Rect]>, full: bool) -> Vec<bool> {
        let phase = self.frame % REFRESH_FRAMES;
        self.frame += 1;
        let Some(dirty) = dirty.filter(|_| !full) else {
            return vec![true; self.footprints.len()];
        };
        self.footprints
            .iter()
            .enumerate()
            .map(|(i, f)| i as u64 % REFRESH_FRAMES == phase || dirty.iter().any(|d| d.intersects(f)))
            .collect()
    }
}

fn empty_block(sequence: u8, index: u32) -> [u8; 8] {
    let w0 = (2u32 << 16) | ((sequence as u32) << 28);
    let w1 = index << 8;
    let mut b = [0u8; 8];
    b[..4].copy_from_slice(&w0.to_le_bytes());
    b[4..].copy_from_slice(&w1.to_le_bytes());
    b
}

/// What [`rebuild`] produced.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Rebuilt {
    pub data: Vec<u8>,
    /// Packets, each at most `boundary` bytes unless it holds a single larger block.
    pub packets: Vec<Range<usize>>,
    pub kept: u32,
    pub total: u32,
}

/// Re-packetizes an encoded frame keeping only the blocks in `keep` (indexed by block). Kept
/// blocks the encoder did not emit become empty blocks. The start of frame header announces
/// the kept count, so the receiver knows when the subset is complete.
pub fn rebuild(layout: &Layout, data: &[u8], packets: &[Range<usize>], keep: &[bool], boundary: usize) -> Result<Rebuilt, Error> {
    let total = layout.block_count;
    let mut blocks: Vec<Option<&[u8]>> = vec![None; total as usize];
    let mut start: Option<([u8; 8], u8)> = None;
    for p in packets {
        for item in parse_items(layout, &data[p.clone()])? {
            match item {
                Item::Start { sequence, header, .. } => start = Some((header, sequence)),
                Item::Block { index, bytes, .. } => blocks[index as usize] = Some(bytes),
            }
        }
    }
    let Some((mut header, sequence)) = start else { return Err(Error::Trailing(0)) };
    let kept = keep.iter().take(total as usize).filter(|&&k| k).count() as u32;
    let w1 = (u32::from_le_bytes(header[4..8].try_into().unwrap()) & !0xff_ffff) | kept;
    header[4..8].copy_from_slice(&w1.to_le_bytes());

    let mut out = Rebuilt { kept, total, ..Default::default() };
    let mut packet_start = 0usize;
    out.data.extend_from_slice(&header);
    let mut push = |out: &mut Rebuilt, bytes: &[u8]| {
        if out.data.len() > packet_start && out.data.len() - packet_start + bytes.len() > boundary {
            out.packets.push(packet_start..out.data.len());
            packet_start = out.data.len();
        }
        out.data.extend_from_slice(bytes);
    };
    for (index, block) in blocks.iter().enumerate() {
        if !keep.get(index).copied().unwrap_or(false) {
            continue;
        }
        match block {
            Some(bytes) => push(&mut out, bytes),
            None => push(&mut out, &empty_block(sequence, index as u32)),
        }
    }
    if out.data.len() > packet_start {
        out.packets.push(packet_start..out.data.len());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        bitstream::{Assembler, MISSING},
        layout::Chroma,
    };

    #[test]
    fn footprints_grow_with_level() {
        let l = Layout::new(1920, 1080, Chroma::Yuv420).unwrap();
        // The very first block is luma LL of level 4: 32 coefficients on a 32 pixel grid.
        let f = footprint(&l, 0).unwrap();
        assert_eq!((f.x0, f.x1), (-(4 * 31 + 32), 1024 + 4 * 31 + 32));
        // A level 0 block covers 64 pixels plus a small margin.
        let hh0 = l.band(0, 0, 3).unwrap();
        let f = footprint(&l, hh0.first_block + 1).unwrap();
        assert_eq!((f.x0, f.x1, f.y0, f.y1), (64 - 6, 128 + 6, -6, 64 + 6));
        assert!(footprint(&l, l.block_count).is_none());
    }

    #[test]
    fn dirty_rect_selects_nearby_blocks_and_all_levels() {
        let l = Layout::new(1920, 1080, Chroma::Yuv420).unwrap();
        let mut s = Selector::new(&l);
        let dirty = [Rect { x0: 900, y0: 500, x1: 910, y1: 520 }];
        let keep = s.keep(Some(&dirty), false);
        let kept: Vec<u32> = (0..l.block_count).filter(|&i| keep[i as usize]).collect();
        // Some of every level, few of the fine ones.
        for level in 0..5 {
            assert!(l.bands.iter().filter(|b| b.level == level).any(|b| kept.iter().any(|&i| (b.first_block..b.first_block + b.blocks_x * b.blocks_y).contains(&i))));
        }
        assert!(kept.len() < l.block_count as usize / 10, "{} of {}", kept.len(), l.block_count);
        // Far away fine blocks are not kept unless refreshed this frame.
        let hh0 = l.band(0, 0, 3).unwrap();
        let far = hh0.first_block;
        assert_eq!(keep[far as usize], (far as u64).is_multiple_of(REFRESH_FRAMES));
    }

    #[test]
    fn refresh_covers_every_block() {
        let l = Layout::new(320, 180, Chroma::Yuv444).unwrap();
        let mut s = Selector::new(&l);
        let mut seen = vec![false; l.block_count as usize];
        for _ in 0..REFRESH_FRAMES {
            for (i, k) in s.keep(Some(&[]), false).iter().enumerate() {
                seen[i] |= *k;
            }
        }
        assert!(seen.iter().all(|&s| s));
        assert!(s.keep(None, false).iter().all(|&k| k));
        assert!(s.keep(Some(&[]), true).iter().all(|&k| k));
    }

    fn golden() -> (Layout, Vec<Vec<u8>>) {
        let pkts = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/320x180.420.b21600.pkts")).unwrap();
        let u32_at = |i: usize| u32::from_le_bytes(pkts[i..i + 4].try_into().unwrap()) as usize;
        let count = u32_at(0);
        let mut at = 4;
        let mut frame = Vec::new();
        for _ in 0..count {
            let size = u32_at(at);
            frame.push(pkts[at + 4..at + 4 + size].to_vec());
            at += 4 + size;
        }
        (Layout::new(320, 180, Chroma::Yuv420).unwrap(), frame)
    }

    #[test]
    fn rebuild_parses_back_to_exactly_the_kept_blocks() {
        let (layout, frame) = golden();
        let data = frame.concat();
        let mut ranges = Vec::new();
        let mut at = 0;
        for p in &frame {
            ranges.push(at..at + p.len());
            at += p.len();
        }
        let keep: Vec<bool> = (0..layout.block_count).map(|i| i % 3 == 0).collect();
        let r = rebuild(&layout, &data, &ranges, &keep, 600).unwrap();
        assert_eq!(r.kept, keep.iter().filter(|&&k| k).count() as u32);
        let mut asm = Assembler::new(layout.clone(), 1 << 20);
        for p in &r.packets {
            assert!(p.len() <= 600 || parse_items(&layout, &r.data[p.clone()]).unwrap().len() == 1);
            asm.push_packet(&r.data[p.clone()]).unwrap();
        }
        let f = asm.take_frame(false).expect("subset complete");
        for (i, &k) in keep.iter().enumerate() {
            assert_eq!(f.offsets[i] != MISSING, k, "block {i}");
        }
        // Kept blocks keep their encoded bytes.
        let full = {
            let mut a = Assembler::new(layout.clone(), 1 << 20);
            for p in &frame {
                a.push_packet(p).unwrap();
            }
            a.take_frame(false).unwrap()
        };
        for (i, &k) in keep.iter().enumerate() {
            if k && full.offsets[i] != MISSING {
                let words = |fr: &crate::Frame| {
                    let o = fr.offsets[i] as usize;
                    let n = ((fr.words[o] >> 16) & 0xfff) as usize;
                    fr.words[o..o + n].to_vec()
                };
                assert_eq!(words(&f), words(&full), "block {i}");
            }
        }
    }
}
