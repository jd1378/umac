//! VideoToolbox emits length-prefixed NAL units (AVCC/HVCC); decoders on the client take
//! Annex-B start codes, with parameter sets repeated in front of every keyframe.

const START: [u8; 4] = [0, 0, 0, 1];

#[derive(Debug, PartialEq, Eq)]
pub struct MalformedNal;

/// Appends `parameter_sets` (VPS/SPS/PPS, no prefixes) and then the NAL units of `avcc`,
/// each behind a start code.
pub fn avcc_to_annexb(
    avcc: &[u8],
    nal_length_size: usize,
    parameter_sets: &[&[u8]],
    out: &mut Vec<u8>,
) -> Result<(), MalformedNal> {
    if !(1..=4).contains(&nal_length_size) {
        return Err(MalformedNal);
    }
    for ps in parameter_sets {
        out.extend_from_slice(&START);
        out.extend_from_slice(ps);
    }
    let mut rest = avcc;
    while !rest.is_empty() {
        if rest.len() < nal_length_size {
            return Err(MalformedNal);
        }
        let len = rest[..nal_length_size].iter().fold(0usize, |acc, b| (acc << 8) | *b as usize);
        rest = &rest[nal_length_size..];
        if len == 0 || len > rest.len() {
            return Err(MalformedNal);
        }
        out.extend_from_slice(&START);
        out.extend_from_slice(&rest[..len]);
        rest = &rest[len..];
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_with_parameter_sets() {
        let avcc = [0, 0, 0, 2, 0xAA, 0xBB, 0, 0, 0, 1, 0xCC];
        let mut out = Vec::new();
        avcc_to_annexb(&avcc, 4, &[&[0x40, 1], &[0x42, 2]], &mut out).unwrap();
        assert_eq!(out, [0, 0, 0, 1, 0x40, 1, 0, 0, 0, 1, 0x42, 2, 0, 0, 0, 1, 0xAA, 0xBB, 0, 0, 0, 1, 0xCC]);
    }

    #[test]
    fn rejects_truncated_input() {
        let mut out = Vec::new();
        assert_eq!(avcc_to_annexb(&[0, 0, 0, 5, 1, 2], 4, &[], &mut out), Err(MalformedNal));
        assert_eq!(avcc_to_annexb(&[0, 0], 4, &[], &mut out), Err(MalformedNal));
        assert_eq!(avcc_to_annexb(&[0, 0, 0, 0], 4, &[], &mut out), Err(MalformedNal));
        assert_eq!(avcc_to_annexb(&[], 5, &[], &mut out), Err(MalformedNal));
    }

    #[test]
    fn two_byte_lengths() {
        let mut out = Vec::new();
        avcc_to_annexb(&[0, 1, 9], 2, &[], &mut out).unwrap();
        assert_eq!(out, [0, 0, 0, 1, 9]);
    }
}
