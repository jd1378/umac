//! Codec-agnostic encoder interface: VideoToolbox (HEVC/H.264) and PyroWave (Metal).

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncoderConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
}

#[derive(Debug, Clone)]
pub struct EncodedFrame {
    /// Annex-B bitstream; keyframes carry their parameter sets.
    pub data: Vec<u8>,
    pub keyframe: bool,
    /// Capture time, microseconds on the host monotonic clock.
    pub capture_us: u64,
    /// When the encoder handed the frame out, same clock.
    pub encoded_us: u64,
    /// PyroWave only: how `data` splits into independently decodable packets.
    pub pyro: Option<PyroFrame>,
}

/// A PyroWave frame: `data` holds the packets back to back, each one datagram on the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PyroFrame {
    pub packets: Vec<std::ops::Range<usize>>,
    pub width: u16,
    pub height: u16,
    pub chroma_444: bool,
}

/// Receives encoder output. Encoders may call it from their own threads.
pub type FrameSink = Box<dyn Fn(EncodedFrame) + Send + Sync>;

pub trait Encoder: Send {
    fn config(&self) -> EncoderConfig;
    /// Submits one captured frame. `frame` is a platform image handle (a `CVPixelBufferRef`
    /// on macOS); the encoder retains it for as long as it needs it.
    fn encode(&mut self, frame: *mut std::ffi::c_void, capture_us: u64, force_keyframe: bool) -> anyhow::Result<()>;
}
