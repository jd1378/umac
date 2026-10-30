//! Per-window ScreenCaptureKit streams. Captures the window's own backing store, so it keeps
//! working while the window is covered or partly off screen.

use std::ffi::c_void;

use anyhow::{Context, anyhow};
use screencapturekit::{
    cg::CGRect,
    prelude::*,
    stream::configuration::colors::{color_matrix, color_space},
};
use umac_proto::WindowId;

use crate::clock;

/// Called on the capture queue with a borrowed `CVPixelBufferRef` (NV12, BT.709 limited)
/// and its capture time. The buffer is only valid during the call unless retained.
pub type FrameCallback = Box<dyn Fn(*mut c_void, u64) + Send + Sync>;

pub struct WindowCapture {
    stream: SCStream,
    config: SCStreamConfiguration,
    pub scale: f32,
    /// Points cropped off the top of the window (its title bar).
    inset: f64,
}

pub fn find_window(id: WindowId) -> anyhow::Result<SCWindow> {
    SCShareableContent::get()
        .map_err(|e| anyhow!("listing shareable content: {e}"))?
        .windows()
        .into_iter()
        .find(|w| w.window_id() == id)
        .ok_or_else(|| anyhow!("window {id} not found"))
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

impl WindowCapture {
    /// Starts capturing `window` at its native pixel size, minus `inset` points at the top.
    /// Returns the capture and the pixel size it was configured for.
    pub fn start(window: &SCWindow, fps: u32, inset: f64, on_frame: FrameCallback) -> anyhow::Result<(Self, (u32, u32))> {
        let filter = SCContentFilter::create().with_window(window).build().map_err(|e| anyhow!("content filter: {e}"))?;
        let scale = filter.point_pixel_scale().max(1.0);
        let frame = window.frame();
        let (w, h) = (frame.size.width, (frame.size.height - inset).max(1.0));
        let size = ((w * scale as f64).round() as u32, (h * scale as f64).round() as u32);
        let config = stream_config(size.0, size.1, fps)?.with_source_rect(CGRect::new(0.0, inset, w, h));
        let mut stream = SCStream::new(&filter, &config).map_err(|e| anyhow!("creating stream: {e}"))?;
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
                        on_frame(buffer, clock::now_us());
                    }
                },
                SCStreamOutputType::Screen,
            )
            .map_err(|e| anyhow!("adding output handler: {e}"))?;
        stream.start_capture().map_err(|e| anyhow!("starting capture: {e}")).context("is Screen Recording allowed?")?;
        Ok((Self { stream, config, scale, inset }, size))
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
