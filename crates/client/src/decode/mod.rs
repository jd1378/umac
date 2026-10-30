//! Codec-agnostic decoding. A decoder turns one encoded frame into one picture, with no
//! reordering or buffering, so latency stays at a single frame.

mod ffmpeg;

use anyhow::{Result, bail};
use umac_proto::{Codec, ColorFormat};

/// Largest picture accepted from the network, per side.
pub const MAX_DIMENSION: u32 = 8192;

/// A decoded picture in CPU memory, NV12 layout (full-res Y plane, half-res interleaved UV).
/// A GPU-resident variant (dmabuf / VA surface import) is the planned zero-copy path.
pub struct Nv12Frame {
    pub width: u32,
    pub height: u32,
    pub y: Vec<u8>,
    pub y_stride: usize,
    pub uv: Vec<u8>,
    pub uv_stride: usize,
}

pub trait Decoder: Send {
    /// Decodes one complete encoded frame. `Ok(None)` means the decoder produced nothing yet.
    fn decode(&mut self, data: &[u8]) -> Result<Option<Nv12Frame>>;
    /// Short description for logs, e.g. "hevc vaapi".
    fn describe(&self) -> &str;
}

pub fn create(codec: Codec, color: ColorFormat) -> Result<Box<dyn Decoder>> {
    match color {
        ColorFormat::Yuv420Bt709Limited => {}
    }
    match codec {
        Codec::Hevc | Codec::H264 => Ok(Box::new(ffmpeg::FfmpegDecoder::new(codec)?)),
        Codec::PyroWave => bail!("PyroWave decoding is not available in this build"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sample;

    #[test]
    fn decodes_hevc_sample_one_in_one_out() {
        let path = std::env::temp_dir().join(format!("umac-decode-test-{}.ts", std::process::id()));
        sample::generate_hevc(&path, 640, 360, 1).unwrap();
        let packets = sample::read_packets(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert!(packets.len() >= 50 && packets[0].keyframe);

        let mut dec = create(Codec::Hevc, ColorFormat::Yuv420Bt709Limited).unwrap();
        eprintln!("decoder: {}", dec.describe());
        let mut produced = 0;
        for (i, p) in packets.iter().enumerate() {
            let f = dec.decode(&p.data).unwrap();
            // Low delay: every packet must yield its picture immediately.
            let f = f.unwrap_or_else(|| panic!("no picture for packet {i}"));
            assert_eq!((f.width, f.height), (640, 360));
            assert_eq!(f.y.len(), 640 * 360);
            assert_eq!(f.uv.len(), 640 * 180);
            produced += 1;
        }
        assert_eq!(produced, packets.len());
    }

    #[test]
    fn garbage_is_an_error_not_a_crash() {
        let mut dec = create(Codec::Hevc, ColorFormat::Yuv420Bt709Limited).unwrap();
        for len in [1, 7, 1000] {
            let junk: Vec<u8> = (0..len).map(|i| (i * 37 % 251) as u8).collect();
            let _ = dec.decode(&junk);
        }
    }
}
