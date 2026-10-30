//! One streamed window: capture feeding an encoder, with keyframe control and resize handling.

use std::{
    ffi::c_void,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use screencapturekit::prelude::{SCDisplay, SCWindow};
use umac_proto::{Codec, WindowId};

use super::{
    pyrowave::{Knobs, PyroEncoder},
    capture::{Rect, WindowCapture},
    ffi::{CVPixelBufferGetHeight, CVPixelBufferGetWidth, CVPixelBufferRef, CVPixelBufferRelease, CVPixelBufferRetain},
    videotoolbox::VtEncoder,
};
use crate::{
    clock,
    encode::{EncodedFrame, Encoder, EncoderConfig},
};

/// A window idle this long gets its refine frame.
const REFINE_AFTER_US: u64 = 120_000;

/// Minimum spacing between forced keyframes; repeated requests inside it are coalesced.
const KEYFRAME_SPACING_US: u64 = 100_000;

#[derive(Debug, Clone, Copy)]
pub struct StreamParams {
    pub window: WindowId,
    pub codec: Codec,
    pub fps: u32,
    pub bitrate_kbps: u32,
    /// PyroWave budget in bits per pixel times 1000.
    pub pyro_bpp_milli: u32,
    /// PyroWave rate cap in bytes per frame for a changing window.
    pub pyro_cap_bytes: u32,
    /// Largest datagram the connection carries, for PyroWave packet sizes.
    pub max_datagram: u32,
}

/// Receives encoded frames; returns false if the frame had to be dropped, which forces the
/// next frame to be a keyframe so the client's reference chain stays intact.
pub type OnEncoded = Arc<dyn Fn(WindowId, EncodedFrame) -> bool + Send + Sync>;

struct Retained(CVPixelBufferRef);
unsafe impl Send for Retained {}
impl Drop for Retained {
    fn drop(&mut self) {
        unsafe { CVPixelBufferRelease(self.0) }
    }
}

struct Shared {
    params: StreamParams,
    knobs: Arc<Knobs>,
    encoder: Mutex<Option<Box<dyn Encoder>>>,
    failed_size: Mutex<Option<(u32, u32)>>,
    force_keyframe: Arc<AtomicBool>,
    last_keyframe_us: AtomicU64,
    last_frame: Mutex<Option<Retained>>,
    last_frame_us: AtomicU64,
    /// The last captured frame was already re-sent at refine quality.
    refined: AtomicBool,
    on_encoded: OnEncoded,
}

impl Shared {
    fn captured(&self, buffer: *mut c_void, now: u64) {
        self.refined.store(false, Ordering::Relaxed);
        self.submit(buffer, now);
    }

    fn new(params: StreamParams, on_encoded: OnEncoded) -> Arc<Self> {
        let knobs = Knobs::default();
        knobs.bpp_milli.store(params.pyro_bpp_milli, Ordering::Relaxed);
        knobs.target_milli.store(params.pyro_bpp_milli, Ordering::Relaxed);
        knobs.cap_bytes.store(params.pyro_cap_bytes, Ordering::Relaxed);
        knobs.packet_boundary.store(packet_boundary(params.max_datagram), Ordering::Relaxed);
        Arc::new(Self {
            params,
            knobs: Arc::new(knobs),
            encoder: Mutex::new(None),
            failed_size: Mutex::new(None),
            force_keyframe: Arc::new(AtomicBool::new(false)),
            last_keyframe_us: AtomicU64::new(0),
            last_frame: Mutex::new(None),
            last_frame_us: AtomicU64::new(0),
            refined: AtomicBool::new(false),
            on_encoded,
        })
    }

    fn submit(&self, buffer: *mut c_void, now: u64) {
        let size = unsafe { (CVPixelBufferGetWidth(buffer) as u32, CVPixelBufferGetHeight(buffer) as u32) };
        let mut enc = self.encoder.lock().unwrap();
        if enc.as_ref().is_none_or(|e| (e.config().width, e.config().height) != size) {
            *enc = None;
            if *self.failed_size.lock().unwrap() == Some(size) {
                return;
            }
            let cfg = EncoderConfig { width: size.0, height: size.1, fps: self.params.fps, bitrate_kbps: self.params.bitrate_kbps };
            let (window, sink, force) = (self.params.window, self.on_encoded.clone(), self.force_keyframe.clone());
            let frame_sink = Box::new(move |f: EncodedFrame| {
                if !sink(window, f) {
                    force.store(true, Ordering::Relaxed);
                }
            });
            match create_encoder(self.params.codec, cfg, frame_sink, &self.knobs) {
                Ok(e) => {
                    tracing::info!(window, ?size, codec = ?self.params.codec, "encoder ready");
                    *self.failed_size.lock().unwrap() = None;
                    *enc = Some(e);
                    // A fresh session starts with a keyframe anyway.
                    self.last_keyframe_us.store(now, Ordering::Relaxed);
                }
                Err(e) => {
                    tracing::error!(window, ?size, "encoder creation failed: {e:#}");
                    *self.failed_size.lock().unwrap() = Some(size);
                    return;
                }
            }
        }
        let force = self.take_force(now);
        if let Err(e) = enc.as_mut().unwrap().encode(buffer, now, force) {
            tracing::warn!(window = self.params.window, "encode: {e:#}");
            self.force_keyframe.store(true, Ordering::Relaxed);
        }
        drop(enc);
        let retained = Retained(unsafe { CVPixelBufferRetain(buffer) });
        *self.last_frame.lock().unwrap() = Some(retained);
        self.last_frame_us.store(now, Ordering::Relaxed);
    }

    fn take_force(&self, now: u64) -> bool {
        if !self.force_keyframe.load(Ordering::Relaxed)
            || now.saturating_sub(self.last_keyframe_us.load(Ordering::Relaxed)) < KEYFRAME_SPACING_US
        {
            return false;
        }
        self.force_keyframe.store(false, Ordering::Relaxed);
        self.last_keyframe_us.store(now, Ordering::Relaxed);
        true
    }
}

/// The one place that maps a negotiated codec to an implementation.
fn create_encoder(codec: Codec, cfg: EncoderConfig, sink: crate::encode::FrameSink, knobs: &Arc<Knobs>) -> anyhow::Result<Box<dyn Encoder>> {
    match codec {
        Codec::Hevc | Codec::H264 => Ok(Box::new(VtEncoder::new(codec, cfg, sink)?)),
        Codec::PyroWave => Ok(Box::new(PyroEncoder::new(cfg, sink, knobs.clone())?)),
    }
}

/// Largest PyroWave packet that fits a datagram of `max_datagram` bytes with its header.
pub fn packet_boundary(max_datagram: u32) -> u32 {
    max_datagram.saturating_sub(umac_proto::video::PYRO_HEADER_LEN as u32)
}

pub struct WindowStream {
    capture: WindowCapture,
    shared: Arc<Shared>,
    pub size: (u32, u32),
}

impl WindowStream {
    pub fn start(window: &SCWindow, params: StreamParams, inset: f64, on_encoded: OnEncoded) -> anyhow::Result<Self> {
        let shared = Shared::new(params, on_encoded);
        let s = shared.clone();
        let (capture, size) = WindowCapture::start(window, params.fps, inset, Box::new(move |buf, t| s.captured(buf, t)))?;
        Ok(Self { capture, shared, size })
    }

    /// Streams a popup by capturing its on-screen region (see `WindowCapture::start_region`).
    pub fn start_region(display: &SCDisplay, rect: Rect, params: StreamParams, on_encoded: OnEncoded) -> anyhow::Result<Self> {
        let shared = Shared::new(params, on_encoded);
        let s = shared.clone();
        let (capture, size) = WindowCapture::start_region(display, rect, params.fps, Box::new(move |buf, t| s.captured(buf, t)))?;
        Ok(Self { capture, shared, size })
    }

    /// Streams a whole display, cursor included.
    pub fn start_display(display: &SCDisplay, params: StreamParams, on_encoded: OnEncoded) -> anyhow::Result<Self> {
        let shared = Shared::new(params, on_encoded);
        let s = shared.clone();
        let (capture, size) = WindowCapture::start_display(display, params.fps, Box::new(move |buf, t| s.captured(buf, t)))?;
        Ok(Self { capture, shared, size })
    }

    pub fn set_region(&mut self, rect: Rect) -> anyhow::Result<()> {
        self.size = self.capture.set_region(rect)?;
        Ok(())
    }

    pub fn params(&self) -> StreamParams {
        self.shared.params
    }

    /// PyroWave knobs (budget, packet size, drop counter); unused by other codecs.
    pub fn knobs(&self) -> &Arc<Knobs> {
        &self.shared.knobs
    }

    pub fn scale(&self) -> f32 {
        self.capture.scale
    }

    pub fn resize(&mut self, width: u32, height: u32) -> anyhow::Result<()> {
        self.size = (width, height);
        self.capture.resize(width, height)
    }

    pub fn request_keyframe(&self) {
        self.shared.force_keyframe.store(true, Ordering::Relaxed);
    }

    /// An idle window produces no frames, so a pending keyframe request would never be
    /// served. Re-encode the last captured frame in that case.
    /// PyroWave frames are intra-only and never sharpen on their own, so once a window stops
    /// changing its last frame is sent again, once, at refine quality.
    pub fn serve_idle_refine(&self) {
        let s = &self.shared;
        if s.params.codec != Codec::PyroWave
            || s.refined.load(Ordering::Relaxed)
            || clock::now_us().saturating_sub(s.last_frame_us.load(Ordering::Relaxed)) < REFINE_AFTER_US
        {
            return;
        }
        let last = s.last_frame.lock().unwrap().as_ref().map(|r| Retained(unsafe { CVPixelBufferRetain(r.0) }));
        if let Some(frame) = last {
            s.refined.store(true, Ordering::Relaxed);
            s.knobs.refine.store(true, Ordering::Relaxed);
            s.submit(frame.0, clock::now_us());
        }
    }

    pub fn serve_idle_keyframe(&self) {
        let now = clock::now_us();
        let s = &self.shared;
        if !s.force_keyframe.load(Ordering::Relaxed) || now.saturating_sub(s.last_frame_us.load(Ordering::Relaxed)) < KEYFRAME_SPACING_US {
            return;
        }
        let last = s.last_frame.lock().unwrap().as_ref().map(|r| Retained(unsafe { CVPixelBufferRetain(r.0) }));
        if let Some(frame) = last {
            s.submit(frame.0, now);
        }
    }
}
