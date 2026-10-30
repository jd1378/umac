//! NAL unit splitting and RBSP bit reading.

use crate::Error;

/// Splits an Annex-B byte stream into NAL units (without start codes, still escaped).
pub fn nal_units(data: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let ends: Vec<usize> = starts
        .iter()
        .skip(1)
        .map(|&s| {
            // Drop the start code and any zero bytes before it (4 byte start codes, trailing zeros).
            let mut e = s - 3;
            while e > 0 && data[e - 1] == 0 {
                e -= 1;
            }
            e
        })
        .chain(std::iter::once(data.len()))
        .collect();
    starts.into_iter().zip(ends).filter(|(s, e)| s < e).map(move |(s, e)| &data[s..e])
}

/// Removes emulation prevention bytes (00 00 03 -> 00 00).
pub fn unescape(nal: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal.len());
    let mut zeros = 0;
    for &b in nal {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

pub struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> BitReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    /// Bits consumed so far.
    pub fn position(&self) -> usize {
        self.pos
    }

    pub fn bit(&mut self) -> Result<bool, Error> {
        let byte = *self.data.get(self.pos / 8).ok_or(Error::Truncated)?;
        let b = (byte >> (7 - self.pos % 8)) & 1 == 1;
        self.pos += 1;
        Ok(b)
    }

    pub fn bits(&mut self, n: u32) -> Result<u32, Error> {
        debug_assert!(n <= 32);
        let mut v = 0u32;
        for _ in 0..n {
            v = (v << 1) | self.bit()? as u32;
        }
        Ok(v)
    }

    pub fn flag(&mut self) -> Result<bool, Error> {
        self.bit()
    }

    /// Unsigned Exp-Golomb, limited to 32 bit values.
    pub fn ue(&mut self) -> Result<u32, Error> {
        let mut zeros = 0;
        while !self.bit()? {
            zeros += 1;
            if zeros > 31 {
                return Err(Error::Invalid("exp-golomb code too long"));
            }
        }
        let rest = self.bits(zeros)?;
        Ok(((1u64 << zeros) - 1 + rest as u64) as u32)
    }

    pub fn se(&mut self) -> Result<i32, Error> {
        let k = self.ue()? as i64;
        Ok(if k % 2 == 1 { ((k + 1) / 2) as i32 } else { -(k / 2) as i32 })
    }

    /// `ue` checked against an inclusive maximum.
    pub fn ue_max(&mut self, max: u32, what: &'static str) -> Result<u32, Error> {
        let v = self.ue()?;
        if v > max {
            return Err(Error::Invalid(what));
        }
        Ok(v)
    }

    pub fn se_range(&mut self, min: i32, max: i32, what: &'static str) -> Result<i32, Error> {
        let v = self.se()?;
        if !(min..=max).contains(&v) {
            return Err(Error::Invalid(what));
        }
        Ok(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_three_and_four_byte_start_codes() {
        let data = [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 9];
        let nals: Vec<&[u8]> = nal_units(&data).collect();
        assert_eq!(nals, vec![&[0x67, 1, 2][..], &[0x68, 3][..], &[0x65, 9][..]]);
        assert_eq!(nal_units(&[1, 2, 3]).count(), 0);
    }

    #[test]
    fn unescapes_emulation_prevention() {
        assert_eq!(unescape(&[0, 0, 3, 1, 0, 0, 3, 0, 0, 3]), vec![0, 0, 1, 0, 0, 0, 0]);
    }

    #[test]
    fn exp_golomb() {
        // 1 | 010 | 011 | 00100 | 00101 -> 0, 1, 2, 3, 4
        let mut r = BitReader::new(&[0b1010_0110, 0b0100_0010, 0b1000_0000]);
        assert_eq!([r.ue().unwrap(), r.ue().unwrap(), r.ue().unwrap(), r.ue().unwrap(), r.ue().unwrap()], [0, 1, 2, 3, 4]);
        let mut r = BitReader::new(&[0b0100_1100]);
        assert_eq!((r.se().unwrap(), r.se().unwrap()), (1, -1));
        assert!(BitReader::new(&[0, 0, 0, 0, 0]).ue().is_err());
    }
}
