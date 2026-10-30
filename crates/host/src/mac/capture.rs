//! Per-window ScreenCaptureKit streams. Normal windows are captured from their own backing
//! store, so capture keeps working while the window is covered or partly off screen.

use std::ffi::c_void;

use anyhow::{Context, anyhow};
use screencapturekit::{
    cg::CGRect,
    prelude::*,
    stream::configuration::colors::{color_matrix, color_space},
};
use umac_proto::WindowId;

use crate::clock;

/// Called on the capture queue with a borrowed `CVPixelBufferRef` (NV12, BT.709 limited),
/// its capture time and the regions that changed since the previous frame (`[x, y, w, h]` as
/// ScreenCaptureKit reports them; `None` when it does not say). The buffer is only valid
/// during the call unless retained.
pub type FrameCallback = Box<dyn Fn(*mut c_void, u64, Option<Vec<[f64; 4]>>) + Send + Sync>;

/// Global rectangle in points.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

pub struct WindowCapture {
    stream: SCStream,
    config: SCStreamConfiguration,
    pub scale: f32,
    /// Points cropped off the top of the window (its title bar).
    inset: f64,
    /// For region captures: the display origin, so global rects map to display coordinates.
    display_origin: Option<(f64, f64)>,
}

fn window_filter(window: &SCWindow) -> anyhow::Result<SCContentFilter> {
    SCContentFilter::create().with_window(window).build().map_err(|e| anyhow!("content filter: {e}"))
}

/// Pixel extent of `points` at `scale`, rounded down to even: 4:2:0 encoders (PyroWave in
/// particular) need even sizes. The source rect is cut to match, so nothing is rescaled.
pub fn pixels(points: f64, scale: f64) -> u32 {
    ((points * scale).round() as u32 & !1).max(2)
}

fn window_pixels(width: f64, height: f64, scale: f32) -> (u32, u32) {
    (pixels(width, scale as f64), pixels(height, scale as f64))
}

/// Backing scale and pixel size a capture of `window` with `inset` will have, without
/// starting it, so the client can open the window before capture runs.
pub fn window_size(window: &SCWindow, inset: f64) -> anyhow::Result<(f32, (u32, u32))> {
    let scale = window_filter(window)?.point_pixel_scale().max(1.0);
    let frame = window.frame();
    Ok((scale, window_pixels(frame.size.width, (frame.size.height - inset).max(1.0), scale)))
}

pub fn find_window(id: WindowId) -> anyhow::Result<SCWindow> {
    SCShareableContent::get()
        .map_err(|e| anyhow!("listing shareable content: {e}"))?
        .windows()
        .into_iter()
        .find(|w| w.window_id() == id)
        .ok_or_else(|| anyhow!("window {id} not found"))
}

/// Displays rarely change, and listing shareable content costs 30 to 50 ms, which would
/// delay every popup; so displays are cached briefly.
const DISPLAY_CACHE: std::time::Duration = std::time::Duration::from_secs(10);
static DISPLAYS: std::sync::Mutex<Option<(Vec<SCDisplay>, std::time::Instant)>> = std::sync::Mutex::new(None);

/// Fresh list of displays (also refreshes the cache). Index order is what clients see.
pub fn displays() -> anyhow::Result<Vec<SCDisplay>> {
    let content = SCShareableContent::get().map_err(|e| anyhow!("listing shareable content: {e}"))?;
    let list = content.displays();
    *DISPLAYS.lock().unwrap() = Some((list.clone(), std::time::Instant::now()));
    Ok(list)
}

/// The display containing the global point, or the first display.
pub fn display_at(x: f64, y: f64) -> anyhow::Result<SCDisplay> {
    let mut cache = DISPLAYS.lock().unwrap();
    if cache.as_ref().is_none_or(|(_, t)| t.elapsed() > DISPLAY_CACHE) {
        let content = SCShareableContent::get().map_err(|e| anyhow!("listing shareable content: {e}"))?;
        *cache = Some((content.displays(), std::time::Instant::now()));
    }
    let displays = &cache.as_ref().unwrap().0;
    displays
        .iter()
        .find(|d| {
            let f = d.frame();
            x >= f.origin.x && y >= f.origin.y && x < f.origin.x + f.size.width && y < f.origin.y + f.size.height
        })
        .or(displays.first())
        .cloned()
        .ok_or_else(|| anyhow!("no display"))
}

fn stream_config(width: u32, height: u32, fps: u32) -> anyhow::Result<SCStreamConfiguration> {
    let fps = fps.clamp(1, 120);
    let cfg = SCStreamConfiguration::new()
        .with_width(width.max(2))
        .with_height(height.max(2))
        .with_pixel_format(PixelFormat::YCbCr_420v)
        .with_shows_cursor(false)
        .with_queue_depth(5)
        .with_minimum_frame_interval(&CMTime::new(1, fps as i32))
        .with_color_matrix(color_matrix::ITU_R_709_2)?
        .with_color_space_name(color_space::SRGB)?;
    Ok(cfg)
}

fn apply_region(config: &mut SCStreamConfiguration, rect: Rect, display: (f64, f64), scale: f64) -> (u32, u32) {
    let size = (pixels(rect.width, scale), pixels(rect.height, scale));
    config
        .set_width(size.0)
        .set_height(size.1)
        .set_source_rect(CGRect::new(rect.x - display.0, rect.y - display.1, size.0 as f64 / scale, size.1 as f64 / scale));
    size
}

/// The `SCStreamFrameInfoDirtyRects` attachment as `[x, y, w, h]` rectangles. ScreenCaptureKit
/// stores them as CGRect dictionaries, which the crate's reader does not decode.
fn dirty_rects(sample: *mut c_void) -> Option<Vec<[f64; 4]>> {
    use core_foundation::{
        array::CFArray,
        base::{CFType, TCFType},
        dictionary::CFDictionary,
        string::CFString,
    };
    use core_graphics::geometry::{CGPoint, CGRect, CGSize};

    use super::ffi::{CGRectMakeWithDictionaryRepresentation, CMSampleBufferGetSampleAttachmentsArray, SCStreamFrameInfoDirtyRects};
    // SAFETY: `sample` is the live sample buffer of this callback; the returned array and
    // its elements are borrowed (get rule) and only read here.
    unsafe {
        let attachments = CMSampleBufferGetSampleAttachmentsArray(sample, 0);
        if attachments.is_null() {
            return None;
        }
        let attachments: CFArray<CFType> = CFArray::wrap_under_get_rule(attachments);
        let first = attachments.get(0)?;
        let info = first.downcast::<CFDictionary>()?;
        let info: CFDictionary<CFString, CFType> = CFDictionary::wrap_under_get_rule(info.as_concrete_TypeRef());
        let list = info.find(CFString::wrap_under_get_rule(SCStreamFrameInfoDirtyRects))?.downcast::<CFArray>()?;
        let list: CFArray<CFType> = CFArray::wrap_under_get_rule(list.as_concrete_TypeRef());
        let mut out = Vec::with_capacity(list.len() as usize);
        for item in list.iter() {
            let Some(dict) = item.downcast::<CFDictionary>() else { continue };
            let mut r = CGRect::new(&CGPoint::new(0.0, 0.0), &CGSize::new(0.0, 0.0));
            if CGRectMakeWithDictionaryRepresentation(dict.as_concrete_TypeRef(), &mut r) {
                out.push([r.origin.x, r.origin.y, r.size.width, r.size.height]);
            }
        }
        // A delivered frame with no changed region would be odd; send it whole then.
        (!out.is_empty()).then_some(out)
    }
}

fn open_stream(filter: &SCContentFilter, config: &SCStreamConfiguration, on_frame: FrameCallback) -> anyhow::Result<SCStream> {
    let mut stream = SCStream::new(filter, config).map_err(|e| anyhow!("creating stream: {e}"))?;
    stream
        .add_output_handler(
            move |sample: CMSampleBuffer, kind: SCStreamOutputType| {
                if kind != SCStreamOutputType::Screen {
                    return;
                }
                // Idle frames carry no pixels: the window did not change.
                if sample.frame_status() != Some(screencapturekit::cm::SCFrameStatus::Complete) {
                    return;
                }
                let buffer = sample.image_buffer_ptr_borrowed();
                if !buffer.is_null() {
                    // Latency is measured from the display refresh the frame belongs to, not
                    // from when ScreenCaptureKit got around to delivering it.
                    let arrival = clock::now_us();
                    let shown = sample.display_time().and_then(clock::from_mach);
                    if let Some(t) = shown {
                        note_display_offset(arrival as i64 - t as i64);
                    }
                    let dirty = dirty_rects(sample.as_ptr());
                    on_frame(buffer, shown.unwrap_or(arrival), dirty);
                }
            },
            SCStreamOutputType::Screen,
        )
        .map_err(|e| anyhow!("adding output handler: {e}"))?;
    stream.start_capture().map_err(|e| anyhow!("starting capture: {e}")).context("is Screen Recording allowed?")?;
    Ok(stream)
}

impl WindowCapture {
    /// Starts capturing `window` at its native pixel size, minus `inset` points at the top.
    /// Returns the capture and the pixel size it was configured for.
    pub fn start(window: &SCWindow, fps: u32, inset: f64, on_frame: FrameCallback) -> anyhow::Result<(Self, (u32, u32))> {
        let filter = window_filter(window)?;
        let scale = filter.point_pixel_scale().max(1.0);
        let frame = window.frame();
        let (w, h) = (frame.size.width, (frame.size.height - inset).max(1.0));
        let size = window_pixels(w, h, scale);
        let s = scale as f64;
        let config = stream_config(size.0, size.1, fps)?.with_source_rect(CGRect::new(0.0, inset, size.0 as f64 / s, size.1 as f64 / s));
        let stream = open_stream(&filter, &config, on_frame)?;
        Ok((Self { stream, config, scale, inset, display_origin: None }, size))
    }

    /// Captures the screen inside `rect` on `display`, as the window server composites it.
    /// Used for menus and other popups: a window filter on them returns the parent window's
    /// pixels instead of the menu. Popups sit on top, so the region shows the popup, with the
    /// real backdrop behind its rounded corners.
    pub fn start_region(display: &SCDisplay, rect: Rect, fps: u32, on_frame: FrameCallback) -> anyhow::Result<(Self, (u32, u32))> {
        Self::region(display, rect, fps, false, on_frame)
    }

    /// Captures a whole display with the Mac cursor drawn in (desktop mode).
    pub fn start_display(display: &SCDisplay, fps: u32, on_frame: FrameCallback) -> anyhow::Result<(Self, (u32, u32))> {
        let f = display.frame();
        let rect = Rect { x: f.origin.x, y: f.origin.y, width: f.size.width, height: f.size.height };
        Self::region(display, rect, fps, true, on_frame)
    }

    fn region(display: &SCDisplay, rect: Rect, fps: u32, cursor: bool, on_frame: FrameCallback) -> anyhow::Result<(Self, (u32, u32))> {
        let filter = SCContentFilter::create().with_display(display).build().map_err(|e| anyhow!("content filter: {e}"))?;
        let scale = filter.point_pixel_scale().max(1.0);
        let d = display.frame().origin;
        let mut config = stream_config(2, 2, fps)?.with_shows_cursor(cursor);
        let size = apply_region(&mut config, rect, (d.x, d.y), scale as f64);
        let stream = open_stream(&filter, &config, on_frame)?;
        Ok((Self { stream, config, scale, inset: 0.0, display_origin: Some((d.x, d.y)) }, size))
    }

    /// Moves or resizes a region capture; returns the new pixel size.
    pub fn set_region(&mut self, rect: Rect) -> anyhow::Result<(u32, u32)> {
        let origin = self.display_origin.ok_or_else(|| anyhow!("not a region capture"))?;
        let size = apply_region(&mut self.config, rect, origin, self.scale as f64);
        self.stream.update_configuration(&self.config).map_err(|e| anyhow!("updating stream: {e}"))?;
        Ok(size)
    }

    pub fn resize(&mut self, width: u32, height: u32) -> anyhow::Result<()> {
        let scale = self.scale as f64;
        let rect = CGRect::new(0.0, self.inset, width as f64 / scale, height as f64 / scale);
        self.config.set_width(width.max(2)).set_height(height.max(2)).set_source_rect(rect);
        self.stream.update_configuration(&self.config).map_err(|e| anyhow!("updating stream: {e}"))
    }
}

impl Drop for WindowCapture {
    fn drop(&mut self) {
        if let Err(e) = self.stream.stop_capture() {
            tracing::debug!("stop_capture: {e}");
        }
    }
}

/// Logs how far frame delivery is from the frame's display time (negative: delivered before
/// the refresh it is shown at), at debug level every 300 frames.
fn note_display_offset(us: i64) {
    static OFFSETS: std::sync::Mutex<Vec<i64>> = std::sync::Mutex::new(Vec::new());
    let Ok(mut v) = OFFSETS.lock() else { return };
    v.push(us);
    if v.len() >= 300 {
        v.sort_unstable();
        let (p50, p95) = (v[v.len() / 2], v[v.len() * 95 / 100]);
        tracing::debug!(p50_us = p50, p95_us = p95, "capture delivery relative to display time");
        v.clear();
    }
}
