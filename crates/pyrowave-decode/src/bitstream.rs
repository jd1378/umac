//! Bitstream parser and validator (spec v1). Every block is checked before anything is kept,
//! so the GPU only ever sees blocks whose implied reads fit inside the block itself, and a
//! rejected packet changes no state.

use crate::layout::{Chroma, Layout};

const HEADER_BYTES: usize = 8;
const SEQUENCE_MASK: u8 = 7;
/// Marks a block that did not arrive (decoded as zero coefficients).
pub const MISSING: u32 = u32::MAX;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Error {
    #[error("{0} trailing bytes do not form a block")]
    Trailing(usize),
    #[error("block declares {declared} bytes but only {left} remain")]
    Truncated { declared: usize, left: usize },
    #[error("payload_words {0} is below the 2 word header")]
    TooFewWords(u32),
    #[error("block index {0} out of range ({1} blocks)")]
    BlockIndex(u32, u32),
    #[error("block contents need {needed} bytes but declare {declared}")]
    Overrun { needed: usize, declared: usize },
    #[error("start of frame says {0}x{1}, decoder is {2}x{3}")]
    Dimensions(u32, u32, u32, u32),
    #[error("start of frame chroma does not match the decoder")]
    ChromaMismatch,
    #[error("unknown extended header code {0}")]
    ExtendedCode(u32),
    #[error("total_blocks {0} exceeds the frame's {1} blocks")]
    TotalBlocks(u32, u32),
    #[error("frame exceeds {0} bytes")]
    FrameTooLarge(usize),
}

/// A frame ready for the GPU: block words packed back to back, and per block index the word
/// offset of its header, or [`MISSING`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub sequence: u8,
    pub words: Vec<u32>,
    pub offsets: Vec<u32>,
    pub received: u32,
    pub total_blocks: Option<u32>,
}

impl Frame {
    pub fn is_complete(&self) -> bool {
        self.total_blocks.is_some_and(|t| self.received >= t)
    }
}

enum Item<'a> {
    Start { sequence: u8, total_blocks: u32 },
    Block { sequence: u8, index: u32, bytes: &'a [u8] },
}

fn u32_at(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]])
}

/// Bytes a block's contents occupy: header, code words, q scales, magnitude planes and signs.
/// Mirrors what the dequantizer reads, so a block that passes never reads past its end.
pub fn block_content_bytes(block: &[u8]) -> Option<usize> {
    let ballot = u32_at(block, 0) & 0xffff;
    let n = ballot.count_ones() as usize;
    let tables = HEADER_BYTES + 3 * n;
    if tables > block.len() {
        return None;
    }
    let mut offset = tables;
    let mut significant = 0usize;
    for i in 0..n {
        let code = u16::from_le_bytes([block[HEADER_BYTES + 2 * i], block[HEADER_BYTES + 2 * i + 1]]) as u32;
        let q_bits = (block[HEADER_BYTES + 2 * n + i] & 0xf) as usize;
        for sub in 0..8 {
            let planes = q_bits + ((code >> (2 * sub)) & 3) as usize;
            let end = offset + planes;
            if end > block.len() {
                return None;
            }
            // An 8x8 block with an all zero code word decodes to zeros (its planes are skipped).
            if code != 0 {
                let any = block[offset..end].iter().fold(0u8, |a, &b| a | b);
                significant += any.count_ones() as usize;
            }
            offset = end;
        }
    }
    Some(offset + significant.div_ceil(8))
}

pub struct Assembler {
    layout: Layout,
    max_frame_words: usize,
    current: Option<Frame>,
    /// Superseded by a newer sequence before it was taken.
    finished: Option<Frame>,
    /// Last sequence handed out, so its late packets are ignored.
    taken: Option<u8>,
    pub duplicates: u64,
}

fn newer(seq: u8, than: u8) -> Option<bool> {
    let diff = seq.wrapping_sub(than) & SEQUENCE_MASK;
    match diff {
        0 => Some(false),
        d if d > SEQUENCE_MASK / 2 => None,
        _ => Some(true),
    }
}

impl Assembler {
    /// `max_frame_bytes` caps the coded size of one frame.
    pub fn new(layout: Layout, max_frame_bytes: usize) -> Self {
        Self { layout, max_frame_words: max_frame_bytes / 4, current: None, finished: None, taken: None, duplicates: 0 }
    }

    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    fn parse<'a>(&self, mut data: &'a [u8]) -> Result<Vec<Item<'a>>, Error> {
        let mut items = Vec::new();
        while !data.is_empty() {
            if data.len() < HEADER_BYTES {
                return Err(Error::Trailing(data.len()));
            }
            let w0 = u32_at(data, 0);
            let w1 = u32_at(data, 4);
            let sequence = ((w0 >> 28) & 7) as u8;
            if w0 >> 31 != 0 {
                let (w, h) = ((w0 & 0x3fff) + 1, ((w0 >> 14) & 0x3fff) + 1);
                if (w, h) != (self.layout.width, self.layout.height) {
                    return Err(Error::Dimensions(w, h, self.layout.width, self.layout.height));
                }
                let code = (w1 >> 24) & 3;
                if code != 0 {
                    return Err(Error::ExtendedCode(code));
                }
                let chroma = if (w1 >> 26) & 1 == 1 { Chroma::Yuv444 } else { Chroma::Yuv420 };
                if chroma != self.layout.chroma {
                    return Err(Error::ChromaMismatch);
                }
                let total_blocks = w1 & 0xff_ffff;
                if total_blocks > self.layout.block_count {
                    return Err(Error::TotalBlocks(total_blocks, self.layout.block_count));
                }
                items.push(Item::Start { sequence, total_blocks });
                data = &data[HEADER_BYTES..];
                continue;
            }
            let words = (w0 >> 16) & 0xfff;
            if words < 2 {
                return Err(Error::TooFewWords(words));
            }
            let declared = words as usize * 4;
            if declared > data.len() {
                return Err(Error::Truncated { declared, left: data.len() });
            }
            let index = w1 >> 8;
            if index >= self.layout.block_count {
                return Err(Error::BlockIndex(index, self.layout.block_count));
            }
            let bytes = &data[..declared];
            match block_content_bytes(bytes) {
                Some(needed) if needed <= declared => {}
                Some(needed) => return Err(Error::Overrun { needed, declared }),
                None => return Err(Error::Overrun { needed: declared + 1, declared }),
            }
            // A block with an empty ballot is not supposed to be sent; it carries nothing.
            if w0 & 0xffff != 0 {
                items.push(Item::Block { sequence, index, bytes });
            }
            data = &data[declared..];
        }
        Ok(items)
    }

    /// Frame for `sequence`, starting a new one (and retiring the current) when it is newer.
    /// `None` for sequences older than what is current or already taken.
    fn frame_for(&mut self, sequence: u8) -> Option<&mut Frame> {
        if self.taken.is_some_and(|t| newer(sequence, t) != Some(true)) {
            return None;
        }
        let start_new = match &self.current {
            None => true,
            Some(f) => newer(sequence, f.sequence)?,
        };
        if start_new {
            if let Some(old) = self.current.take() {
                self.finished = Some(old);
            }
            self.current = Some(Frame {
                sequence,
                words: Vec::new(),
                offsets: vec![MISSING; self.layout.block_count as usize],
                received: 0,
                total_blocks: None,
            });
        }
        self.current.as_mut()
    }

    /// Validates a whole packet, then adds its blocks. A rejected packet leaves no trace.
    pub fn push_packet(&mut self, data: &[u8]) -> Result<(), Error> {
        let items = self.parse(data)?;
        let added: usize = items.iter().map(|i| if let Item::Block { bytes, .. } = i { bytes.len() / 4 } else { 0 }).sum();
        let current = self.current.as_ref().map_or(0, |f| f.words.len());
        if current + added > self.max_frame_words {
            return Err(Error::FrameTooLarge(self.max_frame_words * 4));
        }
        for item in items {
            match item {
                Item::Start { sequence, total_blocks } => {
                    if let Some(f) = self.frame_for(sequence) {
                        f.total_blocks = Some(total_blocks);
                    }
                }
                Item::Block { sequence, index, bytes } => {
                    let Some(f) = self.frame_for(sequence) else { continue };
                    let slot = &mut f.offsets[index as usize];
                    if *slot != MISSING {
                        self.duplicates += 1;
                        continue;
                    }
                    *slot = f.words.len() as u32;
                    f.words.extend(bytes.as_chunks::<4>().0.iter().map(|c| u32::from_le_bytes(*c)));
                    f.received += 1;
                }
            }
        }
        Ok(())
    }

    /// True when the current frame has every block its start of frame header announced.
    pub fn current_complete(&self) -> bool {
        self.current.as_ref().is_some_and(Frame::is_complete)
    }

    /// The next frame to decode: one superseded by a newer sequence, else the current frame
    /// when complete or when `force` (timeout). Missing blocks decode as zeros.
    pub fn take_frame(&mut self, force: bool) -> Option<Frame> {
        let frame = match self.finished.take() {
            Some(f) => f,
            None if force || self.current_complete() => self.current.take()?,
            None => return None,
        };
        if self.taken.is_none_or(|t| newer(frame.sequence, t) == Some(true)) {
            self.taken = Some(frame.sequence);
        }
        (frame.received > 0).then_some(frame)
    }

    /// Forgets everything (e.g. after a stream restart).
    pub fn reset(&mut self) {
        self.current = None;
        self.finished = None;
        self.taken = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout() -> Layout {
        Layout::new(256, 144, Chroma::Yuv420).unwrap()
    }

    fn start(seq: u8, total: u32) -> Vec<u8> {
        let w0 = 255u32 | (143 << 14) | ((seq as u32) << 28) | (1 << 31);
        let w1 = total;
        [w0.to_le_bytes(), w1.to_le_bytes()].concat()
    }

    /// A minimal valid block: one 8x8 group, q_bits 1, code word 0x0001 (one extra plane in
    /// subblock 0), so 9 plane bytes, with one significant coefficient and its sign.
    fn block(seq: u8, index: u32) -> Vec<u8> {
        let mut b = vec![0u8; 8];
        let mut content = vec![0x01, 0x00, 0x11]; // code word, q scale (scale 1, 1 plane)
        content.extend([0x01, 0x00]); // subblock 0: 2 planes
        content.extend([0u8; 7]); // subblocks 1..7: 1 plane each
        content.push(0x01); // sign byte
        let words = (8 + content.len()).div_ceil(4);
        let w0 = 1u32 | ((words as u32) << 16) | ((seq as u32) << 28);
        b[..4].copy_from_slice(&w0.to_le_bytes());
        b[4..8].copy_from_slice(&((index << 8) | 40).to_le_bytes());
        b.extend(content);
        b.resize(words * 4, 0);
        b
    }

    fn asm() -> Assembler {
        Assembler::new(layout(), 1 << 20)
    }

    #[test]
    fn complete_frame() {
        let mut a = asm();
        let pkt = [start(0, 2), block(0, 3), block(0, 7)].concat();
        a.push_packet(&pkt).unwrap();
        assert!(a.current_complete());
        let f = a.take_frame(false).unwrap();
        assert_eq!((f.received, f.offsets[3], f.offsets[7]), (2, 0, 6));
        assert_eq!(f.offsets[0], MISSING);
        // Late packets of a taken sequence are ignored.
        a.push_packet(&block(0, 9)).unwrap();
        assert!(a.take_frame(true).is_none());
    }

    #[test]
    fn newer_sequence_retires_partial_frame() {
        let mut a = asm();
        a.push_packet(&[start(1, 3), block(1, 0)].concat()).unwrap();
        assert!(a.take_frame(false).is_none());
        a.push_packet(&block(2, 0)).unwrap();
        assert_eq!(a.take_frame(false).unwrap().sequence, 1);
        // Older sequence after that is dropped.
        a.push_packet(&block(1, 5)).unwrap();
        assert_eq!(a.take_frame(true).unwrap().received, 1);
    }

    #[test]
    fn rejections() {
        let mut a = asm();
        assert_eq!(a.push_packet(&[0u8; 5]), Err(Error::Trailing(5)));
        let mut b = block(0, 0);
        b[2] = 1; // payload_words = 1
        b[3] &= 0xf0;
        assert_eq!(a.push_packet(&b), Err(Error::TooFewWords(1)));
        let b = block(0, 0);
        assert!(matches!(a.push_packet(&b[..b.len() - 4]), Err(Error::Truncated { .. })));
        let count = layout().block_count;
        assert_eq!(a.push_packet(&block(0, count)), Err(Error::BlockIndex(count, count)));
        // Claims more planes than it carries.
        let mut b = block(0, 0);
        b[10] = 0x1f; // q_bits 15
        assert!(matches!(a.push_packet(&b), Err(Error::Overrun { .. })));
        let mut s = start(0, 1);
        s[0] = 0;
        assert!(matches!(a.push_packet(&s), Err(Error::Dimensions(..))));
        let mut s = start(0, 1);
        s[7] |= 1 << 2; // chroma 444
        assert_eq!(a.push_packet(&s), Err(Error::ChromaMismatch));
        let mut s = start(0, 1);
        s[7] |= 1; // code 1
        assert_eq!(a.push_packet(&s), Err(Error::ExtendedCode(1)));
        assert!(matches!(a.push_packet(&start(0, count + 1)), Err(Error::TotalBlocks(..))));
        // A rejected packet leaves no trace, even if it started with valid blocks.
        let pkt = [block(0, 1), block(0, count)].concat();
        assert!(a.push_packet(&pkt).is_err());
        assert!(a.take_frame(true).is_none());
    }

    #[test]
    fn duplicates_and_frame_cap() {
        let mut a = asm();
        a.push_packet(&[block(0, 1), block(0, 1)].concat()).unwrap();
        assert_eq!(a.duplicates, 1);
        let mut small = Assembler::new(layout(), 16);
        assert_eq!(small.push_packet(&block(0, 1)), Err(Error::FrameTooLarge(16)));
    }

    struct XorShift(u64);
    impl XorShift {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
    }

    #[test]
    fn random_and_mutated_input_never_panics() {
        let mut rng = XorShift(0x9e37_79b9_7f4a_7c15);
        let golden = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/256x144.420.b13824.pkts")).unwrap();
        let mut a = asm();
        for round in 0..20_000 {
            let len = (rng.next() % 1500) as usize;
            let mut pkt: Vec<u8> = if round % 2 == 0 {
                (0..len).map(|_| rng.next() as u8).collect()
            } else {
                let start = (rng.next() as usize) % golden.len().saturating_sub(len).max(1);
                golden[start..(start + len).min(golden.len())].to_vec()
            };
            for _ in 0..(rng.next() % 6) {
                if !pkt.is_empty() {
                    let i = (rng.next() as usize) % pkt.len();
                    pkt[i] ^= 1 << (rng.next() % 8);
                }
            }
            let _ = a.push_packet(&pkt);
            if round % 7 == 0 {
                let _ = a.take_frame(true);
            }
        }
    }
}
