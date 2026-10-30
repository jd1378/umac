//! H.264 front end for a hardware decoder (Vulkan Video): turns one access unit from the
//! network into a decode job with validated parameter sets, picture order counts and the
//! reference picture set, so the GPU side only executes. Restricted to what a low-latency
//! encoder emits (I and P slices, frames only, sliding-window references); anything else is
//! rejected and the caller asks for a new keyframe.
#![forbid(unsafe_code)]

pub mod bits;
pub mod params;
pub mod slice;

use std::{collections::BTreeMap, sync::Arc};

pub use params::{Pps, Sps};
use slice::{Mmco, SliceHeader};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Error {
    #[error("truncated bitstream")]
    Truncated,
    #[error("invalid bitstream: {0}")]
    Invalid(&'static str),
    #[error("unsupported: {0}")]
    Unsupported(&'static str),
    #[error("no keyframe received yet")]
    NeedKeyframe,
}

/// Bytes accepted for one access unit.
pub const MAX_ACCESS_UNIT: usize = 8 << 20;
/// Slices accepted per picture.
pub const MAX_SLICES: usize = 256;

/// A reference picture the current picture may predict from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reference {
    /// DPB slot holding the decoded picture.
    pub slot: u8,
    /// FrameNum for short-term pictures, LongTermFrameIdx for long-term ones.
    pub frame_num: u32,
    pub long_term: bool,
    pub poc: [i32; 2],
}

#[derive(Debug, Clone)]
pub struct DecodeJob {
    pub sps: Arc<Sps>,
    pub pps: Arc<Pps>,
    /// True when the parameter sets changed since the previous job (new session parameters).
    pub new_parameters: bool,
    /// Slice NAL units, each behind a 00 00 01 start code, as the hardware expects them.
    pub bitstream: Vec<u8>,
    pub slice_offsets: Vec<u32>,
    pub idr: bool,
    pub intra: bool,
    pub reference: bool,
    pub frame_num: u32,
    pub idr_pic_id: u16,
    pub poc: [i32; 2],
    /// DPB slot the decoded picture is written to.
    pub slot: u8,
    pub references: Vec<Reference>,
    /// DPB slots the session needs: references plus the picture being decoded.
    pub slots: u8,
}

#[derive(Debug, Clone, Copy)]
struct RefPic {
    slot: u8,
    frame_num: u32,
    /// LongTermFrameIdx when marked long-term.
    long_term: Option<u32>,
    poc: [i32; 2],
}

/// Decoding state across access units of one stream.
#[derive(Default)]
pub struct Parser {
    sps: BTreeMap<u8, Arc<Sps>>,
    pps: BTreeMap<u8, Arc<Pps>>,
    active: Option<(Arc<Sps>, Arc<Pps>)>,
    started: bool,
    refs: Vec<RefPic>,
    prev_ref_poc_msb: i32,
    prev_ref_poc_lsb: u32,
    prev_frame_num: u32,
    prev_frame_num_offset: u32,
    prev_ref_frame_num: u32,
    /// MaxLongTermFrameIdx; `None` means "no long-term frame indices".
    max_long_term_idx: Option<u32>,
}

impl Parser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Forgets the reference state; decoding resumes at the next IDR picture.
    pub fn reset(&mut self) {
        self.started = false;
        self.refs.clear();
    }

    /// Parses one Annex-B access unit. On error the stream must restart at an IDR picture.
    pub fn push(&mut self, access_unit: &[u8]) -> Result<DecodeJob, Error> {
        let result = self.push_inner(access_unit);
        if result.is_err() {
            self.reset();
        }
        result
    }

    fn push_inner(&mut self, access_unit: &[u8]) -> Result<DecodeJob, Error> {
        if access_unit.len() > MAX_ACCESS_UNIT {
            return Err(Error::Invalid("access unit too large"));
        }
        let mut changed = false;
        let mut first: Option<(SliceHeader, bool, u8)> = None;
        let mut bitstream = Vec::with_capacity(access_unit.len() + 64);
        let mut slice_offsets = Vec::new();
        let mut intra = true;
        for nal in bits::nal_units(access_unit) {
            let header = nal[0];
            if header & 0x80 != 0 {
                return Err(Error::Invalid("forbidden_zero_bit set"));
            }
            let nal_ref_idc = (header >> 5) & 3;
            match header & 0x1f {
                7 => {
                    let sps = Sps::parse(&bits::unescape(&nal[1..]))?;
                    changed |= self.sps.get(&sps.id).is_none_or(|s| **s != sps);
                    self.sps.insert(sps.id, Arc::new(sps));
                }
                8 => {
                    let pps = Pps::parse(&bits::unescape(&nal[1..]))?;
                    changed |= self.pps.get(&pps.id).is_none_or(|p| **p != pps);
                    self.pps.insert(pps.id, Arc::new(pps));
                }
                t @ (1 | 5) => {
                    let idr = t == 5;
                    if slice_offsets.len() >= MAX_SLICES {
                        return Err(Error::Invalid("too many slices"));
                    }
                    let rbsp = bits::unescape(&nal[1..]);
                    let pps_id = bits::BitReader::new(&rbsp).peek_pps_id()?;
                    let pps = self.pps.get(&pps_id).ok_or(Error::Invalid("slice refers to an unknown PPS"))?.clone();
                    let sps = self.sps.get(&pps.sps_id).ok_or(Error::Invalid("PPS refers to an unknown SPS"))?.clone();
                    let h = SliceHeader::parse(&rbsp, idr, nal_ref_idc, &sps, &pps)?;
                    intra &= h.intra;
                    match &first {
                        None => {
                            if self.active.as_ref().is_none_or(|(s, p)| !Arc::ptr_eq(s, &sps) || !Arc::ptr_eq(p, &pps)) {
                                changed = true;
                                self.active = Some((sps, pps));
                            }
                            first = Some((h, idr, nal_ref_idc));
                        }
                        Some((f, fidr, fref)) => {
                            if h.frame_num != f.frame_num || h.pps_id != f.pps_id || idr != *fidr || (nal_ref_idc != 0) != (*fref != 0) {
                                return Err(Error::Invalid("slices of different pictures in one access unit"));
                            }
                        }
                    }
                    slice_offsets.push(bitstream.len() as u32);
                    bitstream.extend_from_slice(&[0, 0, 1]);
                    bitstream.extend_from_slice(nal);
                }
                _ => {}
            }
        }
        let (h, idr, nal_ref_idc) = first.ok_or(Error::Invalid("access unit without slices"))?;
        if !self.started && !idr {
            return Err(Error::NeedKeyframe);
        }
        let (sps, pps) = self.active.clone().expect("set with the first slice");
        let max_frame_num = 1u32 << sps.log2_max_frame_num;
        if idr {
            self.refs.clear();
            self.prev_ref_poc_msb = 0;
            self.prev_ref_poc_lsb = 0;
            self.prev_frame_num_offset = 0;
            self.prev_frame_num = 0;
            self.prev_ref_frame_num = 0;
        } else if h.frame_num != self.prev_ref_frame_num && h.frame_num != (self.prev_ref_frame_num + 1) % max_frame_num {
            return Err(Error::Invalid("frame_num gap (lost reference picture)"));
        }
        let poc = self.poc(&sps, &h, idr, nal_ref_idc);
        let references: Vec<Reference> = self
            .refs
            .iter()
            .map(|r| Reference { slot: r.slot, frame_num: r.long_term.unwrap_or(r.frame_num), long_term: r.long_term.is_some(), poc: r.poc })
            .collect();
        let slots = sps.max_num_ref_frames.max(1) + 1;
        let slot = (0..slots).find(|s| !self.refs.iter().any(|r| r.slot == *s)).ok_or(Error::Invalid("no free DPB slot"))?;

        if nal_ref_idc != 0 {
            self.mark(&sps, &h, idr, slot, poc)?;
            self.prev_ref_poc_lsb = h.poc_lsb;
            self.prev_ref_poc_msb = poc[0] - h.poc_lsb as i32;
            self.prev_ref_frame_num = h.frame_num;
        }
        self.prev_frame_num = h.frame_num;
        self.started = true;
        Ok(DecodeJob {
            sps,
            pps,
            new_parameters: changed,
            bitstream,
            slice_offsets,
            idr,
            intra,
            reference: nal_ref_idc != 0,
            frame_num: h.frame_num,
            idr_pic_id: h.idr_pic_id,
            poc,
            slot,
            references,
            slots,
        })
    }

    /// Picture order count (top, bottom), 8.2.1.
    fn poc(&mut self, sps: &Sps, h: &SliceHeader, idr: bool, nal_ref_idc: u8) -> [i32; 2] {
        if sps.poc_type == 0 {
            let max_lsb = 1i64 << sps.log2_max_poc_lsb;
            let (prev_msb, prev_lsb) = (self.prev_ref_poc_msb as i64, self.prev_ref_poc_lsb as i64);
            let lsb = h.poc_lsb as i64;
            let msb = if lsb < prev_lsb && prev_lsb - lsb >= max_lsb / 2 {
                prev_msb + max_lsb
            } else if lsb > prev_lsb && lsb - prev_lsb > max_lsb / 2 {
                prev_msb - max_lsb
            } else {
                prev_msb
            };
            let top = (msb + lsb).clamp(i32::MIN as i64, i32::MAX as i64) as i32;
            return [top, top.saturating_add(h.delta_poc_bottom)];
        }
        let max_frame_num = 1u32 << sps.log2_max_frame_num;
        let offset = if idr {
            0
        } else if self.prev_frame_num > h.frame_num {
            self.prev_frame_num_offset.wrapping_add(max_frame_num)
        } else {
            self.prev_frame_num_offset
        };
        self.prev_frame_num_offset = offset;
        let abs = offset.wrapping_add(h.frame_num) as i64;
        let poc = if idr {
            0
        } else if nal_ref_idc == 0 {
            2 * abs - 1
        } else {
            2 * abs
        };
        let poc = poc.clamp(i32::MIN as i64, i32::MAX as i64) as i32;
        [poc, poc]
    }

    /// Reference picture marking after decoding a reference picture (8.2.5).
    fn mark(&mut self, sps: &Sps, h: &SliceHeader, idr: bool, slot: u8, poc: [i32; 2]) -> Result<(), Error> {
        let max_refs = sps.max_num_ref_frames.max(1) as usize;
        let mut current_long_term = None;
        if idr {
            // The reference list was cleared for the IDR picture already.
            if h.idr_long_term {
                current_long_term = Some(0);
                self.max_long_term_idx = Some(0);
            } else {
                self.max_long_term_idx = None;
            }
        } else if let Some(ops) = &h.mmcos {
            let max_frame_num = 1i64 << sps.log2_max_frame_num;
            let current = h.frame_num as i64;
            let pic_num = |frame_num: u32| if frame_num as i64 > current { frame_num as i64 - max_frame_num } else { frame_num as i64 };
            for op in ops {
                match *op {
                    Mmco::ForgetShortTerm(diff) => {
                        let target = current - (diff as i64 + 1);
                        self.refs.retain(|r| r.long_term.is_some() || pic_num(r.frame_num) != target);
                    }
                    Mmco::ForgetLongTerm(num) => self.refs.retain(|r| r.long_term != Some(num)),
                    Mmco::ShortToLong(diff, idx) => {
                        self.check_long_term_idx(idx)?;
                        let target = current - (diff as i64 + 1);
                        self.refs.retain(|r| r.long_term != Some(idx));
                        let r = self
                            .refs
                            .iter_mut()
                            .find(|r| r.long_term.is_none() && pic_num(r.frame_num) == target)
                            .ok_or(Error::Invalid("marking refers to a missing short-term picture"))?;
                        r.long_term = Some(idx);
                    }
                    Mmco::MaxLongTermIdx(plus1) => {
                        self.max_long_term_idx = plus1.checked_sub(1);
                        let max = self.max_long_term_idx;
                        self.refs.retain(|r| r.long_term.is_none_or(|i| max.is_some_and(|m| i <= m)));
                    }
                    Mmco::CurrentToLong(idx) => {
                        self.check_long_term_idx(idx)?;
                        self.refs.retain(|r| r.long_term != Some(idx));
                        current_long_term = Some(idx);
                    }
                }
            }
        } else if self.refs.len() >= max_refs {
            // Sliding window: drop the oldest short-term picture.
            let max_frame_num = 1i64 << sps.log2_max_frame_num;
            let wrap = |f: u32| if f > h.frame_num { f as i64 - max_frame_num } else { f as i64 };
            let oldest = (0..self.refs.len())
                .filter(|&i| self.refs[i].long_term.is_none())
                .min_by_key(|&i| wrap(self.refs[i].frame_num))
                .ok_or(Error::Invalid("sliding window with only long-term references"))?;
            self.refs.remove(oldest);
        }
        if self.refs.len() >= max_refs {
            return Err(Error::Invalid("reference pictures exceed max_num_ref_frames"));
        }
        self.refs.push(RefPic { slot, frame_num: h.frame_num, long_term: current_long_term, poc });
        Ok(())
    }

    fn check_long_term_idx(&self, idx: u32) -> Result<(), Error> {
        if self.max_long_term_idx.is_none_or(|m| idx > m) {
            return Err(Error::Invalid("long_term_frame_idx above MaxLongTermFrameIdx"));
        }
        Ok(())
    }
}

impl bits::BitReader<'_> {
    /// Reads `first_mb_in_slice`, `slice_type` and returns `pic_parameter_set_id`.
    fn peek_pps_id(&mut self) -> Result<u8, Error> {
        self.ue()?;
        self.ue()?;
        Ok(self.ue_max(255, "slice pps id")? as u8)
    }
}
