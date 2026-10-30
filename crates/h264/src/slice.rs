//! Slice headers, parsed up to the reference picture marking (the rest is for the hardware).

use crate::{
    Error,
    bits::BitReader,
    params::{Pps, Sps},
};

/// Memory management control operations that are supported (others are rejected).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mmco {
    /// Mark a short-term picture unused: `difference_of_pic_nums_minus1`.
    ForgetShortTerm(u32),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SliceHeader {
    pub first_mb: u32,
    pub intra: bool,
    pub pps_id: u8,
    pub frame_num: u32,
    pub idr_pic_id: u16,
    pub poc_lsb: u32,
    pub delta_poc_bottom: i32,
    /// Adaptive marking; `None` means the sliding window.
    pub mmcos: Option<Vec<Mmco>>,
}

/// At most this many list modification or marking operations per slice.
const MAX_OPS: usize = 66;

impl SliceHeader {
    pub fn parse(rbsp_after_nal_header: &[u8], idr: bool, nal_ref_idc: u8, sps: &Sps, pps: &Pps) -> Result<Self, Error> {
        let mut r = BitReader::new(rbsp_after_nal_header);
        let mbs = sps.width_mbs * sps.height_mbs;
        let first_mb = r.ue_max(mbs - 1, "first_mb_in_slice")?;
        let intra = match r.ue_max(9, "slice_type")? % 5 {
            0 => false,
            2 => true,
            1 => return Err(Error::Unsupported("B slices")),
            _ => return Err(Error::Unsupported("SP/SI slices")),
        };
        if idr && !intra {
            return Err(Error::Invalid("IDR picture with a P slice"));
        }
        let pps_id = r.ue_max(255, "slice pps id")? as u8;
        if pps_id != pps.id {
            return Err(Error::Invalid("slice refers to another PPS"));
        }
        let frame_num = r.bits(sps.log2_max_frame_num as u32)?;
        let idr_pic_id = if idr { r.ue_max(65535, "idr_pic_id")? as u16 } else { 0 };
        let (mut poc_lsb, mut delta_poc_bottom) = (0, 0);
        if sps.poc_type == 0 {
            poc_lsb = r.bits(sps.log2_max_poc_lsb as u32)?;
            if pps.bottom_field_pic_order_in_frame_present {
                delta_poc_bottom = r.se_range(-(1 << 30), 1 << 30, "delta_pic_order_cnt_bottom")?;
            }
        }
        let mut num_ref_idx_l0_active = pps.num_ref_idx_l0_default_active as u32;
        if !intra {
            if r.flag()? {
                num_ref_idx_l0_active = r.ue_max(31, "num_ref_idx_l0_active")? + 1;
            }
            // ref_pic_list_modification: skipped, the hardware applies it.
            if r.flag()? {
                for i in 0.. {
                    if i >= MAX_OPS {
                        return Err(Error::Invalid("too many list modifications"));
                    }
                    match r.ue_max(5, "modification_of_pic_nums_idc")? {
                        0..=2 => {
                            r.ue()?;
                        }
                        3 => break,
                        _ => return Err(Error::Unsupported("multiview list modification")),
                    }
                }
            }
            if pps.weighted_pred {
                skip_pred_weight_table(&mut r, num_ref_idx_l0_active)?;
            }
        }
        let mut mmcos = None;
        if nal_ref_idc != 0 {
            if idr {
                r.flag()?;
                if r.flag()? {
                    return Err(Error::Unsupported("long-term reference pictures"));
                }
            } else if r.flag()? {
                let mut ops = Vec::new();
                loop {
                    if ops.len() >= MAX_OPS {
                        return Err(Error::Invalid("too many marking operations"));
                    }
                    match r.ue_max(6, "memory_management_control_operation")? {
                        0 => break,
                        1 => ops.push(Mmco::ForgetShortTerm(r.ue_max(1 << 16, "difference_of_pic_nums")?)),
                        _ => return Err(Error::Unsupported("marking operations other than forgetting a short-term picture")),
                    }
                }
                mmcos = Some(ops);
            }
        }
        Ok(Self { first_mb, intra, pps_id, frame_num, idr_pic_id, poc_lsb, delta_poc_bottom, mmcos })
    }
}

fn skip_pred_weight_table(r: &mut BitReader<'_>, refs: u32) -> Result<(), Error> {
    r.ue_max(7, "luma_log2_weight_denom")?;
    r.ue_max(7, "chroma_log2_weight_denom")?;
    for _ in 0..refs {
        if r.flag()? {
            r.se()?;
            r.se()?;
        }
        if r.flag()? {
            for _ in 0..4 {
                r.se()?;
            }
        }
    }
    Ok(())
}
