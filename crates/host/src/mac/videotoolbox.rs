//! Hardware HEVC/H.264 encoding through VideoToolbox, tuned for latency over efficiency.

use std::{ffi::c_void, ptr};

use anyhow::{Context, bail};
use core_foundation::{
    array::{CFArray, CFArrayGetCount, CFArrayGetValueAtIndex},
    base::{CFType, TCFType},
    boolean::CFBoolean,
    dictionary::{CFDictionary, CFDictionaryContainsKey, CFDictionaryRef},
    number::CFNumber,
    string::{CFString, CFStringRef},
};
use umac_proto::Codec;

use super::ffi::*;
use crate::{
    annexb::avcc_to_annexb,
    clock,
    encode::{EncodedFrame, Encoder, EncoderConfig, FrameSink},
};

struct Shared {
    codec: Codec,
    sink: FrameSink,
}

pub struct VtEncoder {
    session: VTCompressionSessionRef,
    shared: *mut Shared,
    config: EncoderConfig,
    #[cfg_attr(not(test), allow(dead_code))]
    pub low_latency_rc: bool,
}

// The session is thread-safe per Apple docs; `shared` is only read from the callback.
unsafe impl Send for VtEncoder {}

fn key(k: CFStringRef) -> CFString {
    unsafe { CFString::wrap_under_get_rule(k) }
}

fn dict(pairs: &[(CFStringRef, CFType)]) -> CFDictionary<CFString, CFType> {
    let pairs: Vec<_> = pairs.iter().map(|(k, v)| (key(*k), v.clone())).collect();
    CFDictionary::from_CFType_pairs(&pairs)
}

impl VtEncoder {
    pub fn new(codec: Codec, config: EncoderConfig, sink: FrameSink) -> anyhow::Result<Self> {
        let codec_type = match codec {
            Codec::Hevc => kCMVideoCodecType_HEVC,
            Codec::H264 => kCMVideoCodecType_H264,
            Codec::PyroWave => bail!("PyroWave is not a VideoToolbox codec"),
        };
        let shared = Box::into_raw(Box::new(Shared { codec, sink }));
        // Low-latency rate control is not available for every codec/OS combination, so fall
        // back to plain real-time mode rather than failing the stream.
        let mut last_err = 0;
        for low_latency in [true, false] {
            let mut spec = vec![(
                unsafe { kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder },
                CFBoolean::true_value().as_CFType(),
            )];
            if low_latency {
                spec.push((
                    unsafe { kVTVideoEncoderSpecification_EnableLowLatencyRateControl },
                    CFBoolean::true_value().as_CFType(),
                ));
            }
            let spec = dict(&spec);
            let mut session: VTCompressionSessionRef = ptr::null_mut();
            let status = unsafe {
                VTCompressionSessionCreate(
                    ptr::null(),
                    config.width as i32,
                    config.height as i32,
                    codec_type,
                    spec.as_concrete_TypeRef(),
                    ptr::null(),
                    ptr::null(),
                    Some(output_callback),
                    shared.cast(),
                    &mut session,
                )
            };
            if status == 0 && !session.is_null() {
                if !low_latency {
                    tracing::warn!(?codec, "low-latency rate control unavailable, using plain real-time mode");
                }
                let enc = Self { session, shared, config, low_latency_rc: low_latency };
                enc.configure().context("configuring compression session")?;
                return Ok(enc);
            }
            last_err = status;
        }
        drop(unsafe { Box::from_raw(shared) });
        bail!("VTCompressionSessionCreate failed: OSStatus {last_err}")
    }

    fn set(&self, k: CFStringRef, v: &CFType) -> anyhow::Result<()> {
        let status = unsafe { VTSessionSetProperty(self.session, k, v.as_CFTypeRef()) };
        if status != 0 {
            bail!("VTSessionSetProperty({}) failed: {status}", key(k));
        }
        Ok(())
    }

    fn configure(&self) -> anyhow::Result<()> {
        let c = self.config;
        let codec = unsafe { (*self.shared).codec };
        unsafe {
            self.set(kVTCompressionPropertyKey_RealTime, &CFBoolean::true_value().as_CFType())?;
            self.set(kVTCompressionPropertyKey_AllowFrameReordering, &CFBoolean::false_value().as_CFType())?;
            let profile = if codec == Codec::Hevc { kVTProfileLevel_HEVC_Main_AutoLevel } else { kVTProfileLevel_H264_High_AutoLevel };
            self.set(kVTCompressionPropertyKey_ProfileLevel, &key(profile).as_CFType())?;
            let bps = c.bitrate_kbps as i64 * 1000;
            self.set(kVTCompressionPropertyKey_AverageBitRate, &CFNumber::from(bps).as_CFType())?;
            // Cap bursts at 1.5x the average over one second so keyframes don't flood Wi-Fi.
            let limits = CFArray::from_CFTypes(&[CFNumber::from(bps * 3 / 16), CFNumber::from(1.0f64)]);
            if let Err(e) = self.set(kVTCompressionPropertyKey_DataRateLimits, &limits.as_CFType()) {
                tracing::debug!("data rate limits not supported: {e}");
            }
            self.set(kVTCompressionPropertyKey_ExpectedFrameRate, &CFNumber::from(c.fps as i32).as_CFType())?;
            self.set(kVTCompressionPropertyKey_ColorPrimaries, &key(kCVImageBufferColorPrimaries_ITU_R_709_2).as_CFType())?;
            self.set(kVTCompressionPropertyKey_TransferFunction, &key(kCVImageBufferTransferFunction_ITU_R_709_2).as_CFType())?;
            self.set(kVTCompressionPropertyKey_YCbCrMatrix, &key(kCVImageBufferYCbCrMatrix_ITU_R_709_2).as_CFType())?;
            if let Err(e) = self.set(kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality, &CFBoolean::true_value().as_CFType()) {
                tracing::debug!("speed priority not supported: {e}");
            }
            VTCompressionSessionPrepareToEncodeFrames(self.session);
        }
        Ok(())
    }
}

impl Encoder for VtEncoder {
    fn config(&self) -> EncoderConfig {
        self.config
    }

    fn encode(&mut self, frame: *mut c_void, capture_us: u64, force_keyframe: bool) -> anyhow::Result<()> {
        let props = force_keyframe.then(|| {
            dict(&[(unsafe { kVTEncodeFrameOptionKey_ForceKeyFrame }, CFBoolean::true_value().as_CFType())])
        });
        let mut flags = 0u32;
        let status = unsafe {
            VTCompressionSessionEncodeFrame(
                self.session,
                frame,
                CMTime::micros(capture_us),
                CMTime::INVALID,
                props.as_ref().map_or(ptr::null(), |d| d.as_concrete_TypeRef()),
                capture_us as usize as *mut c_void,
                &mut flags,
            )
        };
        if status != 0 {
            bail!("VTCompressionSessionEncodeFrame failed: {status}");
        }
        Ok(())
    }
}

impl Drop for VtEncoder {
    fn drop(&mut self) {
        unsafe {
            // Flush so no callback can run after `shared` is freed.
            VTCompressionSessionCompleteFrames(self.session, CMTime::INVALID);
            VTCompressionSessionInvalidate(self.session);
            core_foundation::base::CFRelease(self.session as *const c_void);
            drop(Box::from_raw(self.shared));
        }
    }
}

unsafe extern "C" fn output_callback(
    refcon: *mut c_void,
    frame_refcon: *mut c_void,
    status: OSStatus,
    flags: u32,
    sample: CMSampleBufferRef,
) {
    let shared = unsafe { &*(refcon as *const Shared) };
    if status != 0 || sample.is_null() || flags & kVTEncodeInfo_FrameDropped != 0 {
        tracing::warn!(status, flags, "encoder dropped a frame");
        return;
    }
    match unsafe { extract(sample, shared.codec) } {
        Ok((data, keyframe)) => (shared.sink)(EncodedFrame { data, keyframe, capture_us: frame_refcon as usize as u64, encoded_us: clock::now_us(), pyro: None }),
        Err(e) => tracing::warn!("bad encoder output: {e}"),
    }
}

unsafe fn is_keyframe(sample: CMSampleBufferRef) -> bool {
    unsafe {
        let attachments = CMSampleBufferGetSampleAttachmentsArray(sample, 0);
        if attachments.is_null() || CFArrayGetCount(attachments) == 0 {
            return true;
        }
        let first = CFArrayGetValueAtIndex(attachments, 0) as CFDictionaryRef;
        CFDictionaryContainsKey(first, kCMSampleAttachmentKey_NotSync as *const c_void) == 0
    }
}

unsafe fn extract(sample: CMSampleBufferRef, codec: Codec) -> anyhow::Result<(Vec<u8>, bool)> {
    unsafe {
        let keyframe = is_keyframe(sample);
        let block = CMSampleBufferGetDataBuffer(sample);
        if block.is_null() {
            bail!("no data buffer");
        }
        let len = CMBlockBufferGetDataLength(block);
        let mut avcc = vec![0u8; len];
        let st = CMBlockBufferCopyDataBytes(block, 0, len, avcc.as_mut_ptr().cast());
        if st != 0 {
            bail!("CMBlockBufferCopyDataBytes: {st}");
        }

        let desc = CMSampleBufferGetFormatDescription(sample);
        let get_ps = match codec {
            Codec::H264 => CMVideoFormatDescriptionGetH264ParameterSetAtIndex,
            _ => CMVideoFormatDescriptionGetHEVCParameterSetAtIndex,
        };
        let (mut count, mut nal_len) = (0usize, 0i32);
        let st = get_ps(desc, 0, ptr::null_mut(), ptr::null_mut(), &mut count, &mut nal_len);
        if st != 0 {
            bail!("parameter set query: {st}");
        }
        let mut sets: Vec<&[u8]> = Vec::new();
        if keyframe {
            for i in 0..count {
                let (mut p, mut n) = (ptr::null(), 0usize);
                if get_ps(desc, i, &mut p, &mut n, ptr::null_mut(), ptr::null_mut()) == 0 && !p.is_null() {
                    sets.push(std::slice::from_raw_parts(p, n));
                }
            }
        }
        let mut out = Vec::with_capacity(len + 256);
        avcc_to_annexb(&avcc, nal_len as usize, &sets, &mut out).map_err(|_| anyhow::anyhow!("malformed NAL lengths"))?;
        Ok((out, keyframe))
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::Write,
        sync::{Arc, Mutex},
        time::Duration,
    };

    use super::*;

    /// IOSurface-backed NV12 frame with a moving gradient, like a capture would deliver.
    fn frame(w: usize, h: usize, t: usize) -> CVPixelBufferRef {
        let attrs = dict(&[(unsafe { kCVPixelBufferIOSurfacePropertiesKey }, CFDictionary::<CFString, CFType>::from_CFType_pairs(&[]).as_CFType())]);
        let mut buf = ptr::null_mut();
        unsafe {
            assert_eq!(CVPixelBufferCreate(ptr::null(), w, h, kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange, attrs.as_concrete_TypeRef(), &mut buf), 0);
            CVPixelBufferLockBaseAddress(buf, 0);
            for plane in 0..2 {
                let base = CVPixelBufferGetBaseAddressOfPlane(buf, plane);
                let stride = CVPixelBufferGetBytesPerRowOfPlane(buf, plane);
                let rows = if plane == 0 { h } else { h / 2 };
                for y in 0..rows {
                    let row = std::slice::from_raw_parts_mut(base.add(y * stride), w);
                    for (x, px) in row.iter_mut().enumerate() {
                        *px = if plane == 0 { (16 + (x + y + t * 8) % 220) as u8 } else { 128 };
                    }
                }
            }
            CVPixelBufferUnlockBaseAddress(buf, 0);
        }
        buf
    }

    #[test]
    fn hevc_encodes_annexb_with_low_latency() {
        let out: Arc<Mutex<Vec<(EncodedFrame, u64)>>> = Arc::default();
        let o = out.clone();
        let cfg = EncoderConfig { width: 1920, height: 1080, fps: 60, bitrate_kbps: 25_000 };
        let codec = if std::env::var_os("UMAC_TEST_H264").is_some() { Codec::H264 } else { Codec::Hevc };
        let mut enc = VtEncoder::new(codec, cfg, Box::new(move |f| {
            let lat = clock::now_us() - f.capture_us;
            o.lock().unwrap().push((f, lat));
        }))
        .unwrap();
        for t in 0..120 {
            let f = frame(1920, 1080, t);
            enc.encode(f, clock::now_us(), t == 60).unwrap();
            unsafe { CVPixelBufferRelease(f) };
            std::thread::sleep(Duration::from_millis(16));
        }
        let low_latency = enc.low_latency_rc;
        drop(enc);
        let frames = std::mem::take(&mut *out.lock().unwrap());
        assert_eq!(frames.len(), 120);
        assert!(frames[0].0.keyframe && frames[60].0.keyframe);
        assert_eq!(&frames[0].0.data[..4], &[0, 0, 0, 1]);
        // A keyframe leads with a parameter set: HEVC VPS (type 32) or H.264 SPS (type 7).
        let first = if codec == Codec::Hevc { (frames[0].0.data[4] >> 1) & 0x3f } else { frames[0].0.data[4] & 0x1f };
        assert_eq!(first, if codec == Codec::Hevc { 32 } else { 7 });
        let mut lat: Vec<u64> = frames.iter().map(|f| f.1).collect();
        lat.sort_unstable();
        eprintln!(
            "low_latency_rc={low_latency} keyframe {} B, p-frame avg {} B, latency us p50 {} p95 {} max {}",
            frames[0].0.data.len(),
            frames[1..].iter().map(|f| f.0.data.len()).sum::<usize>() / 119,
            lat[60], lat[114], lat[119]
        );
        if let Some(path) = std::env::var_os("UMAC_ENCODE_TEST_OUT") {
            let mut f = std::fs::File::create(path).unwrap();
            for (fr, _) in &frames {
                f.write_all(&fr.data).unwrap();
            }
        }
    }
}
