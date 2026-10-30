//! Motion-JPEG: one baseline JPEG per frame, decoded on the CPU with zune-jpeg.
//!
//! The JPEG comes from the network, so its headers are checked here before the decoder sees
//! it: only baseline 8-bit YCbCr 4:2:0 within `MAX_DIMENSION`, the only shape the host sends.
//! VideoToolbox writes BT.709 YCbCr in full range; the output is converted to the limited
//! range NV12 the renderer expects.

use anyhow::{Result, bail};
use zune_jpeg::{
    JpegDecoder,
    zune_core::{bytestream::ZCursor, colorspace::ColorSpace, options::DecoderOptions},
};

use super::{Decoder, MAX_DIMENSION, Nv12Frame, Picture};

/// What the headers say about a JPEG we are willing to decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub width: u32,
    pub height: u32,
}

/// Walks the markers up to the scan and accepts only baseline 8-bit 4:2:0 YCbCr.
pub fn check_header(data: &[u8]) -> Result<Header> {
    if data.len() < 4 || data[..2] != [0xFF, 0xD8] {
        bail!("not a JPEG");
    }
    let mut at = 2;
    let mut header = None;
    while at + 4 <= data.len() {
        if data[at] != 0xFF {
            bail!("expected a marker at {at}");
        }
        let marker = data[at + 1];
        match marker {
            // Fill bytes and standalone markers carry no length.
            0xFF => {
                at += 1;
                continue;
            }
            0x01 | 0xD0..=0xD7 => {
                at += 2;
                continue;
            }
            0xDA => {
                return header.ok_or_else(|| anyhow::anyhow!("scan before frame header"));
            }
            _ => {}
        }
        let len = u16::from_be_bytes([data[at + 2], data[at + 3]]) as usize;
        if len < 2 || at + 2 + len > data.len() {
            bail!("segment length {len} out of bounds");
        }
        let seg = &data[at + 4..at + 2 + len];
        match marker {
            0xC0 => {
                if header.is_some() {
                    bail!("second frame header");
                }
                header = Some(frame_header(seg)?);
            }
            // Every other start-of-frame: extended, progressive, lossless, arithmetic.
            0xC1..=0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF => bail!("unsupported JPEG process 0x{marker:02X}"),
            _ => {}
        }
        at += 2 + len;
    }
    bail!("no scan")
}

fn frame_header(seg: &[u8]) -> Result<Header> {
    if seg.len() != 6 + 3 * 3 {
        bail!("expected 3 components");
    }
    let (precision, height, width, components) = (seg[0], u16::from_be_bytes([seg[1], seg[2]]) as u32, u16::from_be_bytes([seg[3], seg[4]]) as u32, seg[5]);
    if precision != 8 || components != 3 {
        bail!("expected 8-bit, 3 components");
    }
    if width == 0 || height == 0 || width > MAX_DIMENSION || height > MAX_DIMENSION || width % 2 != 0 || height % 2 != 0 {
        bail!("bad size {width}x{height}");
    }
    // Sampling factors: luma 2x2, chroma 1x1 (4:2:0).
    let sampling = [seg[7], seg[10], seg[13]];
    if sampling != [0x22, 0x11, 0x11] {
        bail!("expected 4:2:0 sampling, got {sampling:02X?}");
    }
    Ok(Header { width, height })
}

/// Full range to limited range: luma 0..255 to 16..235, chroma 0..255 to 16..240.
fn range_luts() -> ([u8; 256], [u8; 256]) {
    let (mut y, mut c) = ([0u8; 256], [0u8; 256]);
    for i in 0..256 {
        y[i] = (16.0 + i as f32 * 219.0 / 255.0).round() as u8;
        c[i] = (16.0 + i as f32 * 224.0 / 255.0).round() as u8;
    }
    (y, c)
}

/// Interleaved full resolution YCbCr (full range) to limited range NV12.
pub fn to_nv12(ycc: &[u8], width: u32, height: u32) -> Nv12Frame {
    let (w, h) = (width as usize, height as usize);
    let (ly, lc) = range_luts();
    let mut y = vec![0u8; w * h];
    for (out, px) in y.iter_mut().zip(ycc.as_chunks::<3>().0) {
        *out = ly[px[0] as usize];
    }
    let mut uv = vec![0u8; w * h / 2];
    let row = w * 3;
    for cy in 0..h / 2 {
        let (top, bottom) = (&ycc[2 * cy * row..][..row], &ycc[(2 * cy + 1) * row..][..row]);
        let out = &mut uv[cy * w..][..w];
        for cx in 0..w / 2 {
            let i = cx * 6;
            for k in 1..3 {
                let sum = top[i + k] as u32 + top[i + 3 + k] as u32 + bottom[i + k] as u32 + bottom[i + 3 + k] as u32;
                out[2 * cx + k - 1] = lc[((sum + 2) / 4) as usize];
            }
        }
    }
    Nv12Frame { width, height, y, y_stride: w, uv, uv_stride: w }
}

#[derive(Default)]
pub struct MjpegDecoder;

impl Decoder for MjpegDecoder {
    fn decode(&mut self, data: &[u8]) -> Result<Option<Picture>> {
        let Header { width, height } = check_header(data)?;
        let options = DecoderOptions::default()
            .set_max_width(width as usize)
            .set_max_height(height as usize)
            .set_strict_mode(true)
            .jpeg_set_out_colorspace(ColorSpace::YCbCr);
        let mut dec = JpegDecoder::new_with_options(ZCursor::new(data), options);
        let ycc = dec.decode().map_err(|e| anyhow::anyhow!("JPEG: {e:?}"))?;
        let info = dec.info().ok_or_else(|| anyhow::anyhow!("JPEG: no image info"))?;
        if (info.width as u32, info.height as u32) != (width, height) || ycc.len() != width as usize * height as usize * 3 {
            bail!("JPEG: decoded size does not match its header");
        }
        Ok(Some(Picture::Cpu(to_nv12(&ycc, width, height))))
    }

    fn describe(&self) -> &str {
        "mjpeg cpu"
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;

    const CODE: &[u8] = include_bytes!("../../testdata/mjpeg/code.q50.jpg");
    const MIXED: &[u8] = include_bytes!("../../testdata/mjpeg/mixed.q50.jpg");

    fn mean(v: &[u8]) -> f64 {
        v.iter().map(|&x| x as f64).sum::<f64>() / v.len() as f64
    }

    #[test]
    fn decodes_videotoolbox_jpegs_to_limited_nv12() {
        // Means of the BT.709 limited range sources the Mac encoded (measured offline).
        for (jpg, y_mean, u_mean, v_mean) in [(CODE, 44.84, 127.89, 127.71), (MIXED, 173.49, 131.2, 127.25)] {
            let mut dec = MjpegDecoder;
            let started = Instant::now();
            let Some(Picture::Cpu(f)) = dec.decode(jpg).unwrap() else { panic!("no picture") };
            eprintln!("decode {:?} ({} bytes)", started.elapsed(), jpg.len());
            assert_eq!((f.width, f.height), (1920, 1080));
            assert!(f.y.iter().all(|&y| (16..=235).contains(&y)));
            let (u, v): (Vec<u8>, Vec<u8>) = f.uv.as_chunks::<2>().0.iter().map(|p| (p[0], p[1])).unzip();
            assert!((mean(&f.y) - y_mean).abs() < 1.5, "Y mean {} vs {y_mean}", mean(&f.y));
            assert!((mean(&u) - u_mean).abs() < 1.5, "U mean {} vs {u_mean}", mean(&u));
            assert!((mean(&v) - v_mean).abs() < 1.5, "V mean {} vs {v_mean}", mean(&v));
        }
    }

    #[test]
    fn header_rules() {
        assert_eq!(check_header(CODE).unwrap(), Header { width: 1920, height: 1080 });
        assert!(check_header(b"").is_err());
        assert!(check_header(&[0xFF, 0xD8, 0xFF, 0xD9]).is_err());
        // Progressive frame header instead of baseline.
        let sof = CODE.windows(2).position(|w| w == [0xFF, 0xC0]).unwrap();
        let mut progressive = CODE.to_vec();
        progressive[sof + 1] = 0xC2;
        assert!(check_header(&progressive).is_err());
        // Oversized width.
        let mut huge = CODE.to_vec();
        huge[sof + 7..sof + 9].copy_from_slice(&0xFFFEu16.to_be_bytes());
        assert!(check_header(&huge).is_err());
        // 4:4:4 sampling.
        let mut full = CODE.to_vec();
        full[sof + 11] = 0x11;
        assert!(check_header(&full).is_err());
    }

    /// Truncated, bit-flipped and random inputs must give errors, never panics or hangs.
    #[test]
    fn malformed_input_never_panics() {
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let mut rand = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let mut dec = MjpegDecoder;
        for len in [0, 1, 2, 3, 100, 600, 5000, CODE.len() / 2, CODE.len() - 1] {
            let _ = dec.decode(&CODE[..len]);
        }
        for _ in 0..100 {
            let mut m = CODE.to_vec();
            for _ in 0..(1 + rand() % 8) {
                let i = (rand() % m.len() as u64) as usize;
                m[i] = rand() as u8;
            }
            let _ = dec.decode(&m);
        }
        for _ in 0..200 {
            let n = (rand() % 4096) as usize;
            let mut junk: Vec<u8> = (0..n).map(|_| rand() as u8).collect();
            if junk.len() >= 2 {
                junk[..2].copy_from_slice(&[0xFF, 0xD8]);
            }
            let _ = dec.decode(&junk);
        }
    }

    #[test]
    fn range_conversion_endpoints() {
        let ycc = [0u8, 0, 0, 255, 255, 255, 0, 0, 0, 255, 255, 255];
        let f = to_nv12(&ycc, 2, 2);
        assert_eq!(f.y, [16, 235, 16, 235]);
        // Chroma averages 0 and 255 over the 2x2 block: 128 maps to 128.
        assert_eq!(f.uv, [128, 128]);
    }
}
