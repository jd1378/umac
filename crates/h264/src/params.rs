//! Sequence and picture parameter sets, restricted to what a low-latency hardware encoder
//! emits: 8-bit 4:2:0 progressive frames, Baseline/Main/High, no scaling matrices.

use crate::{Error, bits::BitReader};

/// Largest picture side accepted, in pixels.
pub const MAX_SIDE: u32 = 8192;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sps {
    pub profile_idc: u8,
    pub constraint_flags: u8,
    pub level_idc: u8,
    pub id: u8,
    pub log2_max_frame_num: u8,
    pub poc_type: u8,
    pub log2_max_poc_lsb: u8,
    pub max_num_ref_frames: u8,
    pub gaps_in_frame_num_allowed: bool,
    pub width_mbs: u32,
    pub height_mbs: u32,
    pub direct_8x8_inference: bool,
    pub crop: [u32; 4],
}

impl Sps {
    pub fn parse(rbsp: &[u8]) -> Result<Self, Error> {
        let mut r = BitReader::new(rbsp);
        let profile_idc = r.bits(8)? as u8;
        let constraint_flags = r.bits(8)? as u8;
        let level_idc = r.bits(8)? as u8;
        let id = r.ue_max(31, "sps id")? as u8;
        if !matches!(profile_idc, 66 | 77 | 100) {
            return Err(Error::Unsupported("profile other than Baseline, Main or High"));
        }
        if profile_idc == 100 {
            if r.ue()? != 1 {
                return Err(Error::Unsupported("chroma format other than 4:2:0"));
            }
            if r.ue()? != 0 || r.ue()? != 0 {
                return Err(Error::Unsupported("bit depth other than 8"));
            }
            if r.flag()? {
                return Err(Error::Unsupported("lossless transform bypass"));
            }
            if r.flag()? {
                return Err(Error::Unsupported("sequence scaling matrices"));
            }
        }
        let log2_max_frame_num = r.ue_max(12, "log2_max_frame_num")? as u8 + 4;
        let poc_type = r.ue_max(2, "pic_order_cnt_type")? as u8;
        let log2_max_poc_lsb = match poc_type {
            0 => r.ue_max(12, "log2_max_pic_order_cnt_lsb")? as u8 + 4,
            1 => return Err(Error::Unsupported("pic_order_cnt_type 1")),
            _ => 0,
        };
        let max_num_ref_frames = r.ue_max(16, "max_num_ref_frames")? as u8;
        let gaps_in_frame_num_allowed = r.flag()?;
        let width_mbs = r.ue_max(MAX_SIDE / 16 - 1, "picture width")? + 1;
        let height_mbs = r.ue_max(MAX_SIDE / 16 - 1, "picture height")? + 1;
        if !r.flag()? {
            return Err(Error::Unsupported("interlaced coding"));
        }
        let direct_8x8_inference = r.flag()?;
        let mut crop = [0; 4];
        if r.flag()? {
            for c in &mut crop {
                *c = r.ue_max(MAX_SIDE, "frame cropping")?;
            }
            // 4:2:0 crop units are two pixels.
            if (crop[0] + crop[1]) * 2 >= width_mbs * 16 || (crop[2] + crop[3]) * 2 >= height_mbs * 16 {
                return Err(Error::Invalid("frame cropping larger than the picture"));
            }
        }
        // VUI is not needed for decoding and is not parsed.
        Ok(Self {
            profile_idc,
            constraint_flags,
            level_idc,
            id,
            log2_max_frame_num,
            poc_type,
            log2_max_poc_lsb,
            max_num_ref_frames,
            gaps_in_frame_num_allowed,
            width_mbs,
            height_mbs,
            direct_8x8_inference,
            crop,
        })
    }

    /// Coded size in pixels (whole macroblocks).
    pub fn coded_size(&self) -> (u32, u32) {
        (self.width_mbs * 16, self.height_mbs * 16)
    }

    /// Displayed size after cropping.
    pub fn display_size(&self) -> (u32, u32) {
        let (w, h) = self.coded_size();
        (w - 2 * (self.crop[0] + self.crop[1]), h - 2 * (self.crop[2] + self.crop[3]))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pps {
    pub id: u8,
    pub sps_id: u8,
    pub entropy_coding_mode: bool,
    pub bottom_field_pic_order_in_frame_present: bool,
    pub num_ref_idx_l0_default_active: u8,
    pub num_ref_idx_l1_default_active: u8,
    pub weighted_pred: bool,
    pub weighted_bipred_idc: u8,
    pub pic_init_qp_minus26: i8,
    pub pic_init_qs_minus26: i8,
    pub chroma_qp_index_offset: i8,
    pub deblocking_filter_control_present: bool,
    pub constrained_intra_pred: bool,
    pub transform_8x8_mode: bool,
    pub second_chroma_qp_index_offset: i8,
}

impl Pps {
    pub fn parse(rbsp: &[u8]) -> Result<Self, Error> {
        let mut r = BitReader::new(rbsp);
        let id = r.ue_max(255, "pps id")? as u8;
        let sps_id = r.ue_max(31, "pps sps id")? as u8;
        let entropy_coding_mode = r.flag()?;
        let bottom_field_pic_order_in_frame_present = r.flag()?;
        if r.ue()? != 0 {
            return Err(Error::Unsupported("slice groups"));
        }
        let num_ref_idx_l0_default_active = r.ue_max(31, "num_ref_idx_l0_default")? as u8 + 1;
        let num_ref_idx_l1_default_active = r.ue_max(31, "num_ref_idx_l1_default")? as u8 + 1;
        let weighted_pred = r.flag()?;
        let weighted_bipred_idc = r.bits(2)? as u8;
        if weighted_bipred_idc > 2 {
            return Err(Error::Invalid("weighted_bipred_idc"));
        }
        let pic_init_qp_minus26 = r.se_range(-26, 25, "pic_init_qp")? as i8;
        let pic_init_qs_minus26 = r.se_range(-26, 25, "pic_init_qs")? as i8;
        let chroma_qp_index_offset = r.se_range(-12, 12, "chroma_qp_index_offset")? as i8;
        let deblocking_filter_control_present = r.flag()?;
        let constrained_intra_pred = r.flag()?;
        if r.flag()? {
            return Err(Error::Unsupported("redundant pictures"));
        }
        let (mut transform_8x8_mode, mut second_chroma_qp_index_offset) = (false, chroma_qp_index_offset);
        if more_rbsp_data(rbsp, &r) {
            transform_8x8_mode = r.flag()?;
            if r.flag()? {
                return Err(Error::Unsupported("picture scaling matrices"));
            }
            second_chroma_qp_index_offset = r.se_range(-12, 12, "second_chroma_qp_index_offset")? as i8;
        }
        Ok(Self {
            id,
            sps_id,
            entropy_coding_mode,
            bottom_field_pic_order_in_frame_present,
            num_ref_idx_l0_default_active,
            num_ref_idx_l1_default_active,
            weighted_pred,
            weighted_bipred_idc,
            pic_init_qp_minus26,
            pic_init_qs_minus26,
            chroma_qp_index_offset,
            deblocking_filter_control_present,
            constrained_intra_pred,
            transform_8x8_mode,
            second_chroma_qp_index_offset,
        })
    }
}

/// True if syntax remains before the RBSP stop bit.
fn more_rbsp_data(rbsp: &[u8], r: &BitReader<'_>) -> bool {
    let Some(last) = rbsp.iter().rposition(|&b| b != 0) else { return false };
    let stop_bit = last * 8 + 7 - rbsp[last].trailing_zeros() as usize;
    r.position() < stop_bit
}
