//! PyroWave encoding on the GPU through the vendored Metal encoder
//! (`third_party/pyrowave-metal`, plain C API in `pyrowave_metal.h`).
//!
//! Safety invariants for everything below:
//! - The device is created once and never destroyed, so encoders never outlive it.
//! - An encoder is used from one thread at a time (`&mut self`), as the library requires.
//! - `encode` hands the library the capture's IOSurface and waits for the packets (which
//!   blocks until the GPU finished) before returning, so the surface, owned by the caller for
//!   the duration of the call, outlives every GPU access to it.

use std::{
    ffi::{CStr, c_char, c_int, c_void},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicU32, AtomicU64, Ordering},
    },
};

use anyhow::{anyhow, bail};

use super::ffi::CVPixelBufferRef;
use crate::{
    clock,
    encode::{EncodedFrame, Encoder, EncoderConfig, FrameSink, PyroFrame},
    pyro,
};

#[allow(non_camel_case_types)]
mod sys {
    use std::ffi::{c_char, c_int, c_void};

    pub type pyrowave_result = c_int;
    pub const PYROWAVE_SUCCESS: pyrowave_result = 0;
    pub const PYROWAVE_CHROMA_SUBSAMPLING_420: c_int = 0;

    #[repr(C)]
    pub struct pyrowave_device_opaque {
        _private: [u8; 0],
    }
    #[repr(C)]
    pub struct pyrowave_encoder_opaque {
        _private: [u8; 0],
    }
    pub type pyrowave_device = *mut pyrowave_device_opaque;
    pub type pyrowave_encoder = *mut pyrowave_encoder_opaque;

    #[repr(C)]
    pub struct pyrowave_device_create_info {
        pub mtl_device: *mut c_void,
        pub message_callback: Option<unsafe extern "C" fn(*mut c_void, *const c_char)>,
        pub message_userdata: *mut c_void,
    }

    #[repr(C)]
    pub struct pyrowave_encoder_create_info {
        pub device: pyrowave_device,
        pub width: c_int,
        pub height: c_int,
        pub chroma: c_int,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    pub struct pyrowave_packet {
        pub offset: usize,
        pub size: usize,
    }

    #[repr(C)]
    pub struct pyrowave_rate_control {
        pub maximum_bitstream_size: usize,
    }

    #[repr(C)]
    pub struct pyrowave_gpu_input {
        pub planes: [*mut c_void; 3],
    }

    unsafe extern "C" {
        pub fn pyrowave_result_to_string(result: pyrowave_result) -> *const c_char;
        pub fn pyrowave_device_create(info: *const pyrowave_device_create_info, device: *mut pyrowave_device) -> pyrowave_result;
        pub fn pyrowave_encoder_create(info: *const pyrowave_encoder_create_info, encoder: *mut pyrowave_encoder) -> pyrowave_result;
        pub fn pyrowave_encoder_destroy(encoder: pyrowave_encoder);
        pub fn pyrowave_encoder_encode_gpu(
            encoder: pyrowave_encoder,
            input: *const pyrowave_gpu_input,
            rate_control: *const pyrowave_rate_control,
        ) -> pyrowave_result;
        pub fn pyrowave_encoder_compute_num_packets(encoder: pyrowave_encoder, packet_boundary: usize, num_packets: *mut usize) -> pyrowave_result;
        pub fn pyrowave_encoder_packetize(
            encoder: pyrowave_encoder,
            packets: *mut pyrowave_packet,
            packet_boundary: usize,
            out_packets: *mut usize,
            bitstream: *mut c_void,
            size: usize,
        ) -> pyrowave_result;
    }
}

#[link(name = "CoreVideo", kind = "framework")]
unsafe extern "C" {
    fn CVPixelBufferGetIOSurface(buffer: CVPixelBufferRef) -> *mut c_void;
}

fn check(r: sys::pyrowave_result, what: &str) -> anyhow::Result<()> {
    if r == sys::PYROWAVE_SUCCESS {
        return Ok(());
    }
    // SAFETY: documented to return a static string for any value, never NULL.
    let msg = unsafe { CStr::from_ptr(sys::pyrowave_result_to_string(r)) }.to_string_lossy();
    bail!("{what}: {msg}")
}

unsafe extern "C" fn log_message(_userdata: *mut c_void, msg: *const c_char) {
    if !msg.is_null() {
        // SAFETY: the library passes a NUL terminated string valid for the call.
        let msg = unsafe { CStr::from_ptr(msg) }.to_string_lossy();
        tracing::warn!("pyrowave: {msg}");
    }
}

struct Device(sys::pyrowave_device);
// SAFETY: the device is immutable after creation apart from its own mutex-guarded pipeline
// cache, and is shared read-only by encoders on different threads.
unsafe impl Send for Device {}
unsafe impl Sync for Device {}

/// The process-wide device (compiling the pipelines is expensive), or why it is unavailable.
fn device() -> anyhow::Result<sys::pyrowave_device> {
    static DEVICE: OnceLock<Result<Device, String>> = OnceLock::new();
    let d = DEVICE.get_or_init(|| {
        let info = sys::pyrowave_device_create_info {
            mtl_device: std::ptr::null_mut(),
            message_callback: Some(log_message),
            message_userdata: std::ptr::null_mut(),
        };
        let mut dev = std::ptr::null_mut();
        // SAFETY: valid info struct and out pointer.
        let r = unsafe { sys::pyrowave_device_create(&info, &mut dev) };
        check(r, "creating the PyroWave device").map(|_| Device(dev)).map_err(|e| format!("{e:#}"))
    });
    d.as_ref().map(|d| d.0).map_err(|e| anyhow!("{e}"))
}

/// Live encoder knobs, shared with the session so rate control applies without a restart.
#[derive(Debug, Default)]
pub struct Knobs {
    /// Bits per pixel times 1000.
    pub bpp_milli: AtomicU32,
    /// Largest packet that fits a datagram after the PyroWave header.
    pub packet_boundary: AtomicU32,
    /// Packets larger than the boundary (a single big block); sent split over datagrams.
    pub oversized: AtomicU64,
    /// Encode call durations in microseconds (submit to packets ready), drained by the session.
    pub encode_us: Mutex<Vec<u32>>,
}

pub struct PyroEncoder {
    enc: sys::pyrowave_encoder,
    cfg: EncoderConfig,
    sink: FrameSink,
    knobs: Arc<Knobs>,
    packets: Vec<sys::pyrowave_packet>,
}

// SAFETY: the encoder handle is only used through `&mut self`, one thread at a time.
unsafe impl Send for PyroEncoder {}

impl PyroEncoder {
    pub fn new(cfg: EncoderConfig, sink: FrameSink, knobs: Arc<Knobs>) -> anyhow::Result<Self> {
        if !cfg.width.is_multiple_of(2) || !cfg.height.is_multiple_of(2) || cfg.width > 16384 || cfg.height > 16384 {
            bail!("PyroWave 4:2:0 needs even sizes up to 16384, got {}x{}", cfg.width, cfg.height);
        }
        let info = sys::pyrowave_encoder_create_info {
            device: device()?,
            width: cfg.width as c_int,
            height: cfg.height as c_int,
            chroma: sys::PYROWAVE_CHROMA_SUBSAMPLING_420,
        };
        let mut enc = std::ptr::null_mut();
        // SAFETY: valid info (device lives forever) and out pointer.
        check(unsafe { sys::pyrowave_encoder_create(&info, &mut enc) }, "creating the PyroWave encoder")?;
        Ok(Self { enc, cfg, sink, knobs, packets: Vec::new() })
    }
}

impl Drop for PyroEncoder {
    fn drop(&mut self) {
        // SAFETY: created by pyrowave_encoder_create; no GPU work is pending after `encode`.
        unsafe { sys::pyrowave_encoder_destroy(self.enc) }
    }
}

impl Encoder for PyroEncoder {
    fn config(&self) -> EncoderConfig {
        self.cfg
    }

    /// Intra-only: every frame decodes on its own, so `force_keyframe` has nothing to do.
    fn encode(&mut self, frame: *mut c_void, capture_us: u64, _force_keyframe: bool) -> anyhow::Result<()> {
        // SAFETY: `frame` is a live CVPixelBuffer for the duration of this call (caller).
        let surface = unsafe { CVPixelBufferGetIOSurface(frame) };
        if surface.is_null() {
            bail!("capture frame is not IOSurface backed");
        }
        let started = std::time::Instant::now();
        let bpp = self.knobs.bpp_milli.load(Ordering::Relaxed) as f64 / 1000.0;
        let budget = pyro::frame_bytes(self.cfg.width, self.cfg.height, bpp).max(4096);
        let boundary = (self.knobs.packet_boundary.load(Ordering::Relaxed) as usize).max(256);
        let input = sys::pyrowave_gpu_input { planes: [surface, std::ptr::null_mut(), std::ptr::null_mut()] };
        let rate = sys::pyrowave_rate_control { maximum_bitstream_size: budget };
        // SAFETY: encoder valid; input surface valid until we return (see module docs).
        check(unsafe { sys::pyrowave_encoder_encode_gpu(self.enc, &input, &rate) }, "encoding")?;

        let mut count = 0usize;
        // SAFETY: valid encoder after a successful encode; blocks until the GPU is done.
        check(unsafe { sys::pyrowave_encoder_compute_num_packets(self.enc, boundary, &mut count) }, "counting packets")?;
        self.packets.resize(count, sys::pyrowave_packet::default());
        // Packet framing adds a few bytes per packet on top of the coded blocks.
        let mut data = vec![0u8; budget + count * 64 + 4096];
        let mut written = 0usize;
        // SAFETY: `packets` has room for `count` entries and `data` for `data.len()` bytes.
        check(
            unsafe {
                sys::pyrowave_encoder_packetize(self.enc, self.packets.as_mut_ptr(), boundary, &mut written, data.as_mut_ptr().cast(), data.len())
            },
            "packetizing",
        )?;
        let encoded_us = clock::now_us();

        let mut ranges = Vec::with_capacity(written.min(count));
        let mut end = 0;
        for p in &self.packets[..written.min(count)] {
            let Some(stop) = p.offset.checked_add(p.size).filter(|&s| s <= data.len()) else {
                bail!("encoder reported a packet outside its buffer");
            };
            end = end.max(stop);
            if p.size > boundary {
                self.knobs.oversized.fetch_add(1, Ordering::Relaxed);
            }
            ranges.push(p.offset..stop);
        }
        if let Ok(mut v) = self.knobs.encode_us.lock()
            && v.len() < 4096
        {
            v.push(started.elapsed().as_micros() as u32);
        }
        data.truncate(end);
        let pyro = PyroFrame { packets: ranges, width: self.cfg.width as u16, height: self.cfg.height as u16, chroma_444: false };
        (self.sink)(EncodedFrame { data, keyframe: true, capture_us, encoded_us, pyro: Some(pyro) });
        Ok(())
    }
}
