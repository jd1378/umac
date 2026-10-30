//! A decoded VA-API surface exported as DRM PRIME dmabufs, so the renderer can sample it
//! without copying. The mapped frame keeps a reference to the VA surface: while a
//! `DmabufFrame` lives, the decoder cannot reuse that surface.

use std::os::fd::RawFd;

use anyhow::{Result, bail, ensure};
use ffmpeg_next::ffi as ff;

use super::MAX_DIMENSION;

const fn fourcc(c: &[u8; 4]) -> u32 {
    c[0] as u32 | (c[1] as u32) << 8 | (c[2] as u32) << 16 | (c[3] as u32) << 24
}
const DRM_FORMAT_NV12: u32 = fourcc(b"NV12");
const DRM_FORMAT_R8: u32 = fourcc(b"R8  ");
/// Two bytes per pixel, R in the low byte: U then V, like NV12's chroma plane.
const DRM_FORMAT_GR88: u32 = fourcc(b"GR88");

/// One plane of the picture inside a dmabuf object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Plane {
    /// Borrowed from the mapped frame; valid while the `DmabufFrame` lives.
    pub fd: RawFd,
    pub object_size: u64,
    pub modifier: u64,
    pub offset: u64,
    pub pitch: u64,
}

pub struct DmabufFrame {
    frame: *mut ff::AVFrame,
    pub width: u32,
    pub height: u32,
    /// Luma (R8) then interleaved chroma (RG8 at half size).
    pub planes: [Plane; 2],
}

// The mapped frame is only read through `planes` and freed once in `drop`; ffmpeg's frame and
// buffer reference counting is thread safe.
unsafe impl Send for DmabufFrame {}
unsafe impl Sync for DmabufFrame {}

impl DmabufFrame {
    /// Maps a decoded VA-API frame to DRM PRIME. The READ map waits for decoding to finish.
    ///
    /// # Safety
    /// `src` must be a valid, decoded `AV_PIX_FMT_VAAPI` frame.
    pub unsafe fn export(src: *const ff::AVFrame) -> Result<Self> {
        unsafe {
            let mut frame = ff::av_frame_alloc();
            ensure!(!frame.is_null(), "av_frame_alloc failed");
            (*frame).format = ff::AVPixelFormat::AV_PIX_FMT_DRM_PRIME as i32;
            let ret = ff::av_hwframe_map(frame, src, ff::AV_HWFRAME_MAP_READ as i32);
            if ret < 0 {
                ff::av_frame_free(&mut frame);
                bail!("av_hwframe_map: error {ret}");
            }
            let parsed = (|| {
                let desc = (*frame).data[0] as *const ff::AVDRMFrameDescriptor;
                ensure!(!desc.is_null(), "no DRM descriptor");
                let (w, h) = ((*src).width, (*src).height);
                ensure!(w > 0 && h > 0 && w as u32 <= MAX_DIMENSION && h as u32 <= MAX_DIMENSION, "bad size {w}x{h}");
                Ok((w as u32, h as u32, planes(&*desc)?))
            })();
            match parsed {
                Ok((width, height, planes)) => Ok(Self { frame, width, height, planes }),
                Err(e) => {
                    ff::av_frame_free(&mut frame);
                    Err(e)
                }
            }
        }
    }
}

impl Drop for DmabufFrame {
    fn drop(&mut self) {
        // Unmaps (closing the exported fds) and releases the VA surface.
        unsafe { ff::av_frame_free(&mut self.frame) };
    }
}

/// Accepts NV12 as one layer with two planes, or as separate R8 + GR88 layers.
fn planes(d: &ff::AVDRMFrameDescriptor) -> Result<[Plane; 2]> {
    let objects = usize::try_from(d.nb_objects).unwrap_or(0);
    let layers = usize::try_from(d.nb_layers).unwrap_or(0);
    ensure!((1..=d.objects.len()).contains(&objects), "{objects} dmabuf objects");
    let layer_planes = |i: usize| usize::try_from(d.layers[i].nb_planes).unwrap_or(0);
    let refs: Vec<&ff::AVDRMPlaneDescriptor> = match layers {
        1 if d.layers[0].format == DRM_FORMAT_NV12 && layer_planes(0) == 2 => {
            vec![&d.layers[0].planes[0], &d.layers[0].planes[1]]
        }
        2 if d.layers[0].format == DRM_FORMAT_R8
            && d.layers[1].format == DRM_FORMAT_GR88
            && layer_planes(0) == 1
            && layer_planes(1) == 1 =>
        {
            vec![&d.layers[0].planes[0], &d.layers[1].planes[0]]
        }
        _ => bail!("unsupported dmabuf layout: {layers} layers, format {:#x}", d.layers[0].format),
    };
    let plane = |p: &ff::AVDRMPlaneDescriptor| -> Result<Plane> {
        let index = usize::try_from(p.object_index).ok().filter(|&i| i < objects);
        let Some(index) = index else { bail!("plane references object {}", p.object_index) };
        let o = &d.objects[index];
        ensure!(o.fd >= 0, "invalid dmabuf fd");
        let (offset, pitch) = (u64::try_from(p.offset)?, u64::try_from(p.pitch)?);
        ensure!(pitch > 0, "zero pitch");
        Ok(Plane { fd: o.fd, object_size: o.size as u64, modifier: o.format_modifier, offset, pitch })
    };
    Ok([plane(refs[0])?, plane(refs[1])?])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// (object index, offset, pitch)
    type P = (i32, isize, isize);

    fn desc(layers: &[(u32, &[P])]) -> ff::AVDRMFrameDescriptor {
        let mut d: ff::AVDRMFrameDescriptor = unsafe { std::mem::zeroed() };
        d.nb_objects = 1;
        d.objects[0].fd = 7;
        d.objects[0].size = 1 << 20;
        d.objects[0].format_modifier = 0x0200_0000_0000_0001;
        d.nb_layers = layers.len() as i32;
        for (i, (format, planes)) in layers.iter().enumerate() {
            d.layers[i].format = *format;
            d.layers[i].nb_planes = planes.len() as i32;
            for (j, &(object, offset, pitch)) in planes.iter().enumerate() {
                d.layers[i].planes[j].object_index = object;
                d.layers[i].planes[j].offset = offset as _;
                d.layers[i].planes[j].pitch = pitch as _;
            }
        }
        d
    }

    #[test]
    fn separate_layers_and_composed_nv12() {
        let sep = planes(&desc(&[(DRM_FORMAT_R8, &[(0, 0, 2048)]), (DRM_FORMAT_GR88, &[(0, 2_228_224, 2048)])])).unwrap();
        assert_eq!((sep[0].offset, sep[1].offset, sep[1].pitch, sep[0].fd), (0, 2_228_224, 2048, 7));
        assert_eq!(sep[0].modifier, 0x0200_0000_0000_0001);
        let nv12 = planes(&desc(&[(DRM_FORMAT_NV12, &[(0, 0, 1920), (0, 2_088_960, 1920)])])).unwrap();
        assert_eq!(nv12[1].offset, 2_088_960);
    }

    #[test]
    fn rejects_unexpected_layouts() {
        // Chroma with the bytes swapped would decode with U and V exchanged.
        assert!(planes(&desc(&[(DRM_FORMAT_R8, &[(0, 0, 64)]), (fourcc(b"RG88"), &[(0, 64, 64)])])).is_err());
        assert!(planes(&desc(&[(DRM_FORMAT_NV12, &[(0, 0, 64)])])).is_err());
        assert!(planes(&desc(&[(DRM_FORMAT_R8, &[(1, 0, 64)]), (DRM_FORMAT_GR88, &[(0, 64, 64)])])).is_err());
        assert!(planes(&desc(&[(DRM_FORMAT_NV12, &[(0, 0, 0), (0, 64, 64)])])).is_err());
        assert!(planes(&desc(&[])).is_err());
    }
}
