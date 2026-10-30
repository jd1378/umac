//! Frame geometry of the PyroWave bitstream: aligned size, sub-band sizes and the linear
//! 32x32 block index (bitstream spec v1, "Block index ordering").

pub const LEVELS: usize = 5;
pub const COMPONENTS: usize = 3;
pub const BANDS: usize = 4;
const ALIGNMENT: u32 = 1 << LEVELS;
const MIN_SIZE: u32 = 4 << LEVELS;
/// Largest width or height the bitstream can signal.
pub const MAX_DIMENSION: u32 = 1 << 14;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Chroma {
    Yuv420,
    Yuv444,
}

/// One sub-band of one component at one level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Band {
    pub component: usize,
    pub level: usize,
    pub band: usize,
    pub width: u32,
    pub height: u32,
    pub blocks_x: u32,
    pub blocks_y: u32,
    /// First block index of this band.
    pub first_block: u32,
    /// Offset of this band's coefficients (f32 elements) in the coefficient buffer.
    pub coef_offset: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    pub width: u32,
    pub height: u32,
    pub chroma: Chroma,
    pub aligned_width: u32,
    pub aligned_height: u32,
    /// Every band with coded blocks, in block index order.
    pub bands: Vec<Band>,
    /// LL bands of levels below the top one, filled by the inverse transform (not coded).
    pub derived_ll: Vec<Band>,
    pub block_count: u32,
    pub coef_len: u32,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum LayoutError {
    #[error("size {0}x{1} outside 1..={MAX_DIMENSION}")]
    Size(u32, u32),
    #[error("4:2:0 needs an even width and height, got {0}x{1}")]
    OddChroma(u32, u32),
}

fn align(v: u32, a: u32) -> u32 {
    v.div_ceil(a) * a
}

impl Layout {
    pub fn new(width: u32, height: u32, chroma: Chroma) -> Result<Self, LayoutError> {
        if !(1..=MAX_DIMENSION).contains(&width) || !(1..=MAX_DIMENSION).contains(&height) {
            return Err(LayoutError::Size(width, height));
        }
        if chroma == Chroma::Yuv420 && (!width.is_multiple_of(2) || !height.is_multiple_of(2)) {
            return Err(LayoutError::OddChroma(width, height));
        }
        let aligned_width = align(width, ALIGNMENT).max(MIN_SIZE);
        let aligned_height = align(height, ALIGNMENT).max(MIN_SIZE);
        let mut bands = Vec::new();
        let mut block = 0u32;
        let mut coef = 0u32;
        let band_size = |level: usize| (aligned_width >> (level + 1), aligned_height >> (level + 1));
        for level in (0..LEVELS).rev() {
            for component in 0..COMPONENTS {
                if level == 0 && component != 0 && chroma == Chroma::Yuv420 {
                    continue;
                }
                let first_band = if level == LEVELS - 1 { 0 } else { 1 };
                for band in first_band..BANDS {
                    let (w, h) = band_size(level);
                    let (bx, by) = (w.div_ceil(32), h.div_ceil(32));
                    bands.push(Band {
                        component,
                        level,
                        band,
                        width: w,
                        height: h,
                        blocks_x: bx,
                        blocks_y: by,
                        first_block: block,
                        coef_offset: coef,
                    });
                    block += bx * by;
                    coef += w * h;
                }
            }
        }
        let mut derived_ll = Vec::new();
        for component in 0..COMPONENTS {
            let lowest = if component != 0 && chroma == Chroma::Yuv420 { 1 } else { 0 };
            for level in lowest..LEVELS - 1 {
                let (w, h) = band_size(level);
                derived_ll.push(Band {
                    component,
                    level,
                    band: 0,
                    width: w,
                    height: h,
                    blocks_x: 0,
                    blocks_y: 0,
                    first_block: 0,
                    coef_offset: coef,
                });
                coef += w * h;
            }
        }
        Ok(Self { width, height, chroma, aligned_width, aligned_height, bands, derived_ll, block_count: block, coef_len: coef })
    }

    /// The band (coded or derived) of `component` at `level`.
    pub fn band(&self, component: usize, level: usize, band: usize) -> Option<&Band> {
        self.bands
            .iter()
            .chain(self.derived_ll.iter())
            .find(|b| b.component == component && b.level == level && b.band == band)
    }

    /// Lowest level decoded for a component (chroma of 4:2:0 stops at LL0, i.e. level 1).
    pub fn lowest_level(&self, component: usize) -> usize {
        if component != 0 && self.chroma == Chroma::Yuv420 { 1 } else { 0 }
    }

    /// Size of the decoded plane of a component.
    pub fn plane_size(&self, component: usize) -> (u32, u32) {
        if component != 0 && self.chroma == Chroma::Yuv420 { (self.width / 2, self.height / 2) } else { (self.width, self.height) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aligned_sizes_and_block_order() {
        let l = Layout::new(256, 144, Chroma::Yuv420).unwrap();
        assert_eq!((l.aligned_width, l.aligned_height), (256, 160));
        // First band: Y LL4 at 8x5, one block.
        assert_eq!((l.bands[0].component, l.bands[0].level, l.bands[0].band, l.bands[0].width, l.bands[0].height), (0, 4, 0, 8, 5));
        // 4:2:0 has no chroma at level 0.
        assert!(l.bands.iter().all(|b| !(b.level == 0 && b.component != 0)));
        // Level 0 luma bands: 128x80 -> 4x3 blocks each.
        let hh0 = l.band(0, 0, 3).unwrap();
        assert_eq!((hh0.blocks_x, hh0.blocks_y), (4, 3));
        assert_eq!(l.bands.last().unwrap().first_block + 12, l.block_count);
    }

    #[test]
    fn small_sizes_use_minimum() {
        let l = Layout::new(1, 1, Chroma::Yuv444).unwrap();
        assert_eq!((l.aligned_width, l.aligned_height), (128, 128));
        assert_eq!(Layout::new(3, 2, Chroma::Yuv420), Err(LayoutError::OddChroma(3, 2)));
        assert!(Layout::new(0, 2, Chroma::Yuv444).is_err());
        assert!(Layout::new(MAX_DIMENSION + 1, 2, Chroma::Yuv444).is_err());
    }
}
