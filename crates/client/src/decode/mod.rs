//! Codec-agnostic decoding. A decoder turns one encoded frame into one picture, with no
//! reordering or buffering, so latency stays at a single frame.

#[cfg(target_os = "linux")]
pub mod dmabuf;
mod ffmpeg;

use std::sync::atomic::{AtomicU8, Ordering};

use anyhow::{Result, bail};
use umac_proto::{Codec, ColorFormat};

/// Largest picture accepted from the network, per side.
pub const MAX_DIMENSION: u32 = 8192;

/// A decoded picture in CPU memory, NV12 layout (full-res Y plane, half-res interleaved UV).
pub struct Nv12Frame {
    pub width: u32,
    pub height: u32,
    pub y: Vec<u8>,
    pub y_stride: usize,
    pub uv: Vec<u8>,
    pub uv_stride: usize,
}

/// A decoded picture: copied to CPU memory, or still in the decoder's GPU surface.
pub enum Picture {
    Cpu(Nv12Frame),
    #[cfg(target_os = "linux")]
    Dmabuf(dmabuf::DmabufFrame),
}

impl Picture {
    pub fn size(&self) -> (u32, u32) {
        match self {
            Picture::Cpu(f) => (f.width, f.height),
            #[cfg(target_os = "linux")]
            Picture::Dmabuf(f) => (f.width, f.height),
        }
    }
}

const ZC_UNSET: u8 = 0;
const ZC_ON: u8 = 1;
const ZC_OFF: u8 = 2;
static ZERO_COPY: AtomicU8 = AtomicU8::new(ZC_UNSET);

/// Whether decoders should hand out GPU surfaces instead of copying. Enabled by the renderer
/// once its device can import dmabufs; any import failure turns it off for good.
pub fn zero_copy() -> bool {
    ZERO_COPY.load(Ordering::Relaxed) == ZC_ON
}

pub fn set_zero_copy(on: bool) {
    let next = if on { ZC_ON } else { ZC_OFF };
    // Once off (unsupported, failed, or UMAC_ZERO_COPY=0), stay off.
    let _ = ZERO_COPY.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |cur| (cur != ZC_OFF).then_some(next));
}

/// `UMAC_ZERO_COPY=0` forces the copy path, for comparisons.
pub fn zero_copy_allowed() -> bool {
    std::env::var("UMAC_ZERO_COPY").map_or(true, |v| v != "0")
}

pub trait Decoder: Send {
    /// Decodes one complete encoded frame. `Ok(None)` means the decoder produced nothing yet.
    fn decode(&mut self, data: &[u8]) -> Result<Option<Picture>>;
    /// Short description for logs, e.g. "hevc vaapi".
    fn describe(&self) -> &str;
}

/// An HEVC/H.264 decoder that always (or never) exports VA surfaces, regardless of the
/// process-wide switch.
#[cfg(test)]
pub(crate) fn create_with_export(codec: Codec, export: bool) -> Result<Box<dyn Decoder>> {
    let mut d = ffmpeg::FfmpegDecoder::new(codec)?;
    d.export = Some(export);
    Ok(Box::new(d))
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

        // Copies explicitly: GPU tests running in parallel may turn the zero-copy switch on.
        let mut dec = create_with_export(Codec::Hevc, false).unwrap();
        eprintln!("decoder: {}", dec.describe());
        let mut produced = 0;
        for (i, p) in packets.iter().enumerate() {
            let f = dec.decode(&p.data).unwrap();
            // Low delay: every packet must yield its picture immediately.
            let Some(Picture::Cpu(f)) = f else { panic!("no CPU picture for packet {i}") };
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
