//! H.264 / HEVC through libavcodec, VA-API accelerated when available.

use std::{ffi::CStr, ptr, sync::Once};

use anyhow::{Result, anyhow, bail};
use ffmpeg_next::ffi as ff;
use umac_proto::Codec;

use super::{Decoder, MAX_DIMENSION, Nv12Frame, Picture};

pub struct FfmpegDecoder {
    ctx: *mut ff::AVCodecContext,
    pkt: *mut ff::AVPacket,
    frame: *mut ff::AVFrame,
    sw_frame: *mut ff::AVFrame,
    desc: String,
    hw_checked: bool,
    /// Consecutive failed VA surface exports; zero-copy stops for this decoder after a few.
    export_failures: u32,
    /// Overrides the process-wide zero-copy switch (tests compare both paths side by side).
    pub(crate) export: Option<bool>,
}

// The raw pointers are owned exclusively by this struct and only touched through `&mut self`.
unsafe impl Send for FfmpegDecoder {}

fn av_err(code: i32) -> anyhow::Error {
    let mut buf = [0 as std::ffi::c_char; 128];
    unsafe { ff::av_strerror(code, buf.as_mut_ptr(), buf.len()) };
    anyhow!("ffmpeg: {}", unsafe { CStr::from_ptr(buf.as_ptr()) }.to_string_lossy())
}

const EAGAIN: i32 = -libc::EAGAIN;

unsafe extern "C" fn pick_vaapi(_ctx: *mut ff::AVCodecContext, fmts: *const ff::AVPixelFormat) -> ff::AVPixelFormat {
    let mut p = fmts;
    unsafe {
        while *p != ff::AVPixelFormat::AV_PIX_FMT_NONE {
            if *p == ff::AVPixelFormat::AV_PIX_FMT_VAAPI {
                return *p;
            }
            p = p.add(1);
        }
        // No VA-API surface format offered: fall back to the first (software) format.
        *fmts
    }
}

impl FfmpegDecoder {
    pub fn new(codec: Codec) -> Result<Self> {
        static INIT: Once = Once::new();
        INIT.call_once(|| {
            let _ = ffmpeg_next::init();
            ffmpeg_next::util::log::set_level(ffmpeg_next::util::log::Level::Error);
        });
        let (id, name) = match codec {
            Codec::Hevc => (ff::AVCodecID::AV_CODEC_ID_HEVC, "hevc"),
            Codec::H264 => (ff::AVCodecID::AV_CODEC_ID_H264, "h264"),
            Codec::PyroWave | Codec::Mjpeg => bail!("not an ffmpeg codec"),
        };
        match unsafe { Self::open(id, true) } {
            Ok(mut d) => {
                d.desc = format!("{name} vaapi");
                Ok(d)
            }
            Err(e) => {
                tracing::warn!("VA-API decoder unavailable ({e}), using software decoding");
                let mut d = unsafe { Self::open(id, false)? };
                d.desc = format!("{name} software");
                Ok(d)
            }
        }
    }

    unsafe fn open(id: ff::AVCodecID, hw: bool) -> Result<Self> {
        unsafe {
            let codec = ff::avcodec_find_decoder(id);
            if codec.is_null() {
                bail!("decoder not found in libavcodec");
            }
            let mut ctx = ff::avcodec_alloc_context3(codec);
            if ctx.is_null() {
                bail!("avcodec_alloc_context3 failed");
            }
            (*ctx).flags |= ff::AV_CODEC_FLAG_LOW_DELAY as i32;
            (*ctx).thread_count = 1;
            if hw {
                // Zero-copy pictures keep their VA surface until the GPU has drawn them, so the
                // fixed surface pool needs room beyond the reference frames.
                (*ctx).extra_hw_frames = 8;
                let mut dev = ptr::null_mut();
                let ret = ff::av_hwdevice_ctx_create(
                    &mut dev,
                    ff::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
                    ptr::null(),
                    ptr::null_mut(),
                    0,
                );
                if ret < 0 {
                    ff::avcodec_free_context(&mut ctx);
                    return Err(av_err(ret));
                }
                (*ctx).hw_device_ctx = dev;
                (*ctx).get_format = Some(pick_vaapi);
            }
            let ret = ff::avcodec_open2(ctx, codec, ptr::null_mut());
            if ret < 0 {
                ff::avcodec_free_context(&mut ctx);
                return Err(av_err(ret));
            }
            Ok(Self {
                ctx,
                pkt: ff::av_packet_alloc(),
                frame: ff::av_frame_alloc(),
                sw_frame: ff::av_frame_alloc(),
                desc: String::new(),
                hw_checked: !hw,
                export_failures: 0,
                export: None,
            })
        }
    }

    /// Hands out the current frame: as an exported VA surface when zero-copy is on, else
    /// copied to CPU memory. A failed export copies that frame; repeated failures stop
    /// exporting for this decoder.
    unsafe fn picture(&mut self) -> Result<Picture> {
        unsafe {
            #[cfg(target_os = "linux")]
            if (*self.frame).format == ff::AVPixelFormat::AV_PIX_FMT_VAAPI as i32 && self.export.unwrap_or_else(super::zero_copy) {
                match super::dmabuf::DmabufFrame::export(self.frame) {
                    Ok(f) => {
                        self.hw_checked = true;
                        self.export_failures = 0;
                        return Ok(Picture::Dmabuf(f));
                    }
                    Err(e) => {
                        self.export_failures += 1;
                        tracing::warn!(failures = self.export_failures, "VA surface export failed ({e:#}); copying this frame");
                        if self.export_failures >= super::MAX_ZERO_COPY_FAILURES {
                            tracing::warn!("VA surface export keeps failing; copying frames from this decoder");
                            self.export = Some(false);
                        }
                    }
                }
            }
            Ok(Picture::Cpu(self.convert()?))
        }
    }

    unsafe fn convert(&mut self) -> Result<Nv12Frame> {
        unsafe {
            let mut src = self.frame;
            if (*src).format == ff::AVPixelFormat::AV_PIX_FMT_VAAPI as i32 {
                ff::av_frame_unref(self.sw_frame);
                (*self.sw_frame).format = ff::AVPixelFormat::AV_PIX_FMT_NV12 as i32;
                let ret = ff::av_hwframe_transfer_data(self.sw_frame, src, 0);
                if ret < 0 {
                    return Err(av_err(ret));
                }
                src = self.sw_frame;
            } else if !self.hw_checked {
                tracing::warn!("VA-API rejected this stream, decoding in software");
                self.desc = self.desc.replace("vaapi", "software");
            }
            self.hw_checked = true;
            read_nv12(src)
        }
    }
}

/// Copies a VA-API (or software) frame to CPU memory.
///
/// # Safety
/// `src` must be a valid decoded frame.
pub(super) unsafe fn copy_nv12(src: *const ff::AVFrame) -> Result<Nv12Frame> {
    unsafe {
        if (*src).format != ff::AVPixelFormat::AV_PIX_FMT_VAAPI as i32 {
            return read_nv12(src);
        }
        let mut sw = ff::av_frame_alloc();
        if sw.is_null() {
            bail!("av_frame_alloc failed");
        }
        (*sw).format = ff::AVPixelFormat::AV_PIX_FMT_NV12 as i32;
        let ret = ff::av_hwframe_transfer_data(sw, src, 0);
        let out = if ret < 0 { Err(av_err(ret)) } else { read_nv12(sw) };
        ff::av_frame_free(&mut sw);
        out
    }
}

/// Packs a CPU frame (NV12 or planar 4:2:0) into an `Nv12Frame`.
unsafe fn read_nv12(src: *const ff::AVFrame) -> Result<Nv12Frame> {
    unsafe {
        let (w, h) = ((*src).width, (*src).height);
        if w <= 0 || h <= 0 || w as u32 > MAX_DIMENSION || h as u32 > MAX_DIMENSION {
            bail!("decoded picture has bad size {w}x{h}");
        }
        let (w, h) = (w as usize, h as usize);
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        let plane = |i: usize, row: usize, len: usize| {
            let base = (*src).data[i];
            let stride = (*src).linesize[i] as isize;
            std::slice::from_raw_parts(base.offset(row as isize * stride), len)
        };
        let mut y = Vec::with_capacity(w * h);
        for r in 0..h {
            y.extend_from_slice(plane(0, r, w));
        }
        let mut uv = Vec::with_capacity(cw * 2 * ch);
        let fmt = (*src).format;
        if fmt == ff::AVPixelFormat::AV_PIX_FMT_NV12 as i32 {
            for r in 0..ch {
                uv.extend_from_slice(plane(1, r, cw * 2));
            }
        } else if fmt == ff::AVPixelFormat::AV_PIX_FMT_YUV420P as i32
            || fmt == ff::AVPixelFormat::AV_PIX_FMT_YUVJ420P as i32
        {
            for r in 0..ch {
                let (u, v) = (plane(1, r, cw), plane(2, r, cw));
                uv.extend(u.iter().zip(v).flat_map(|(a, b)| [*a, *b]));
            }
        } else {
            bail!("unsupported decoded pixel format {fmt}");
        }
        Ok(Nv12Frame { width: w as u32, height: h as u32, y, y_stride: w, uv, uv_stride: cw * 2 })
    }
}

impl Decoder for FfmpegDecoder {
    fn decode(&mut self, data: &[u8]) -> Result<Option<Picture>> {
        let len = i32::try_from(data.len()).map_err(|_| anyhow!("packet too large"))?;
        unsafe {
            // av_new_packet allocates the zeroed padding libavcodec's bitstream readers expect.
            let ret = ff::av_new_packet(self.pkt, len);
            if ret < 0 {
                return Err(av_err(ret));
            }
            ptr::copy_nonoverlapping(data.as_ptr(), (*self.pkt).data, data.len());
            let ret = ff::avcodec_send_packet(self.ctx, self.pkt);
            ff::av_packet_unref(self.pkt);
            if ret < 0 && ret != EAGAIN {
                return Err(av_err(ret));
            }
            let mut out = None;
            loop {
                let ret = ff::avcodec_receive_frame(self.ctx, self.frame);
                if ret == EAGAIN {
                    break;
                }
                if ret < 0 {
                    return Err(av_err(ret));
                }
                let res = self.picture();
                ff::av_frame_unref(self.frame);
                out = Some(res?);
            }
            Ok(out)
        }
    }

    fn describe(&self) -> &str {
        &self.desc
    }
}

impl Drop for FfmpegDecoder {
    fn drop(&mut self) {
        unsafe {
            ff::av_frame_free(&mut self.sw_frame);
            ff::av_frame_free(&mut self.frame);
            ff::av_packet_free(&mut self.pkt);
            ff::avcodec_free_context(&mut self.ctx);
        }
    }
}
