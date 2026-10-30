//! Codec-agnostic decoding. A decoder turns one encoded frame into one picture, with no
//! reordering or buffering, so latency stays at a single frame.

#[cfg(all(target_os = "linux", feature = "ffmpeg"))]
pub mod dmabuf;
#[cfg(feature = "ffmpeg")]
mod ffmpeg;
pub mod mjpeg;

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
    /// A validated PyroWave frame, decoded on the renderer's GPU when drawn.
    Pyro(crate::pyro::PyroPicture),
    /// A validated H.264 access unit, decoded with Vulkan Video on the renderer's GPU.
    #[cfg(target_os = "linux")]
    H264(Box<umac_h264::DecodeJob>),
    #[cfg(all(target_os = "linux", feature = "ffmpeg"))]
    Dmabuf(dmabuf::DmabufFrame),
}

impl Picture {
    pub fn size(&self) -> (u32, u32) {
        match self {
            Picture::Cpu(f) => (f.width, f.height),
            Picture::Pyro(p) => (p.layout.width, p.layout.height),
            #[cfg(target_os = "linux")]
            Picture::H264(j) => j.sps.display_size(),
            #[cfg(all(target_os = "linux", feature = "ffmpeg"))]
            Picture::Dmabuf(f) => (f.width, f.height),
        }
    }
}

const ZC_UNSET: u8 = 0;
const ZC_ON: u8 = 1;
const ZC_OFF: u8 = 2;
static ZERO_COPY: AtomicU8 = AtomicU8::new(ZC_UNSET);

/// Consecutive zero-copy failures (export or import) tolerated before falling back to copying.
pub const MAX_ZERO_COPY_FAILURES: u32 = 8;

/// Whether decoders should hand out GPU surfaces instead of copying. Enabled by the renderer
/// once its device can import dmabufs; repeated import failures turn it off for good.
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

/// Codecs to offer the host, most preferred first. PyroWave needs no ffmpeg and decodes in
/// well under a millisecond, so it leads whenever a GPU can run its compute decoder
/// (`UMAC_PYROWAVE=0` turns it off for comparisons). H.264 through Vulkan Video comes next,
/// then what ffmpeg can decode, if built in.
pub fn codecs() -> Vec<Codec> {
    let pyro = std::env::var("UMAC_PYROWAVE").map_or(true, |v| v != "0") && pyrowave_gpu();
    let mut c = if pyro { vec![Codec::PyroWave] } else { Vec::new() };
    if vulkan_h264() {
        c.push(Codec::H264);
    }
    if cfg!(feature = "ffmpeg") {
        c.push(Codec::Hevc);
        if !c.contains(&Codec::H264) {
            c.push(Codec::H264);
        }
    }
    c.push(Codec::Mjpeg);
    match std::env::var("UMAC_CODECS") {
        Ok(only) => restrict(c, &only),
        Err(_) => c,
    }
}

/// `UMAC_CODECS=mjpeg,h264` keeps only those codecs (in their usual order), for testing a path.
fn restrict(codecs: Vec<Codec>, only: &str) -> Vec<Codec> {
    let wanted: Vec<String> = only.split(',').map(|s| s.trim().to_ascii_lowercase()).collect();
    codecs.into_iter().filter(|c| wanted.iter().any(|w| *w == format!("{c:?}").to_ascii_lowercase())).collect()
}

/// Whether the GPU decodes H.264 with Vulkan Video (checked once). Fedora's stock Mesa leaves
/// H.264 out; RPM Fusion's `mesa-vulkan-drivers-freeworld` has it.
pub fn vulkan_h264() -> bool {
    #[cfg(target_os = "linux")]
    {
        static OK: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *OK.get_or_init(|| {
            let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
            let Ok(adapter) = pollster::block_on(instance.request_adapter(&Default::default())) else { return false };
            let ok = unsafe { adapter.as_hal::<wgpu::hal::api::Vulkan>() }.is_some_and(|a| crate::vkvideo::decode_family(&a).is_some());
            tracing::info!(ok, adapter = ?adapter.get_info().name, "Vulkan Video H.264 decoder");
            ok
        })
    }
    #[cfg(not(target_os = "linux"))]
    false
}

/// Whether a GPU adapter can run the PyroWave decoder (checked once).
fn pyrowave_gpu() -> bool {
    static OK: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *OK.get_or_init(|| {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let Ok(adapter) = pollster::block_on(instance.request_adapter(&Default::default())) else { return false };
        let ok = adapter.limits().max_storage_buffers_per_shader_stage >= 6
            && adapter.get_downlevel_capabilities().flags.contains(wgpu::DownlevelFlags::COMPUTE_SHADERS);
        tracing::info!(ok, adapter = ?adapter.get_info().name, "PyroWave GPU decoder");
        ok
    })
}

pub trait Decoder: Send {
    /// Decodes one complete encoded frame. `Ok(None)` means the decoder produced nothing yet.
    fn decode(&mut self, data: &[u8]) -> Result<Option<Picture>>;
    /// Short description for logs, e.g. "hevc vaapi".
    fn describe(&self) -> &str;
}

/// An HEVC/H.264 decoder that always (or never) exports VA surfaces, regardless of the
/// process-wide switch.
#[cfg(all(test, feature = "ffmpeg"))]
pub(crate) fn create_with_export(codec: Codec, export: bool) -> Result<Box<dyn Decoder>> {
    let mut d = ffmpeg::FfmpegDecoder::new(codec)?;
    d.export = Some(export);
    Ok(Box::new(d))
}

pub fn create(codec: Codec, color: ColorFormat) -> Result<Box<dyn Decoder>> {
    if color == ColorFormat::Yuv444Bt709Limited && codec != Codec::PyroWave {
        bail!("{codec:?} streams are 4:2:0 only");
    }
    match codec {
        #[cfg(target_os = "linux")]
        Codec::H264 if vulkan_h264() => Ok(Box::new(H264Front::default())),
        #[cfg(feature = "ffmpeg")]
        Codec::Hevc | Codec::H264 => Ok(Box::new(ffmpeg::FfmpegDecoder::new(codec)?)),
        Codec::PyroWave => bail!("PyroWave streams are decoded by the PyroWave path"),
        Codec::Mjpeg => Ok(Box::new(mjpeg::MjpegDecoder)),
        #[allow(unreachable_patterns)]
        other => bail!("{other:?} is not supported by this build"),
    }
}

/// The CPU half of Vulkan Video H.264 decoding: validates each access unit and tracks
/// references, so the renderer only executes the job. Errors make the caller ask for a keyframe.
#[cfg(target_os = "linux")]
#[derive(Default)]
struct H264Front {
    parser: umac_h264::Parser,
}

#[cfg(target_os = "linux")]
impl Decoder for H264Front {
    fn decode(&mut self, data: &[u8]) -> Result<Option<Picture>> {
        let job = self.parser.push(data)?;
        let (w, h) = job.sps.display_size();
        if w > MAX_DIMENSION || h > MAX_DIMENSION {
            bail!("{w}x{h} exceeds {MAX_DIMENSION}");
        }
        Ok(Some(Picture::H264(Box::new(job))))
    }

    fn describe(&self) -> &str {
        "h264 vulkan video"
    }
}

#[cfg(all(test, feature = "ffmpeg"))]
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

#[cfg(test)]
mod restrict_tests {
    use super::*;

    #[test]
    fn codec_restriction() {
        let all = vec![Codec::PyroWave, Codec::H264, Codec::Hevc, Codec::Mjpeg];
        assert_eq!(restrict(all.clone(), "mjpeg, H264"), vec![Codec::H264, Codec::Mjpeg]);
        assert!(restrict(all, "nope").is_empty());
    }
}
