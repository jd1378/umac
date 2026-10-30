//! Opens the renderer's Vulkan device with the optional decode features the adapter supports:
//! dmabuf import (zero-copy VA-API frames) and a Vulkan Video H.264 decode queue.

use std::ffi::CStr;

use wgpu::hal::{api::Vulkan, vulkan as hal_vk};

pub struct Opened {
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    pub dmabuf: bool,
    /// Queue family of the H.264 decode queue (queue 0 of it was created).
    pub video_family: Option<u32>,
}

static PRIORITY: [f32; 1] = [1.0];

/// `None` if the adapter supports neither, or opening failed (the caller then opens a plain
/// device).
pub fn open(adapter: &wgpu::Adapter, want_dmabuf: bool) -> Option<Opened> {
    let hal = unsafe { adapter.as_hal::<Vulkan>() }?;
    #[cfg(feature = "hevc")]
    let dmabuf = want_dmabuf && crate::zerocopy::supported(&hal);
    #[cfg(not(feature = "hevc"))]
    let dmabuf = want_dmabuf && false;
    #[cfg(feature = "h264")]
    let video_family = crate::vkvideo::decode_family(&hal);
    #[cfg(not(feature = "h264"))]
    let video_family: Option<u32> = None;
    if !dmabuf && video_family.is_none() {
        return None;
    }
    let mut extensions: Vec<&'static CStr> = Vec::new();
    #[cfg(feature = "hevc")]
    if dmabuf {
        extensions.extend(crate::zerocopy::EXTENSIONS);
    }
    #[cfg(feature = "h264")]
    if video_family.is_some() {
        extensions.extend(crate::vkvideo::EXTENSIONS);
    }
    let (features, limits, hints) = (wgpu::Features::empty(), wgpu::Limits::default(), wgpu::MemoryHints::default());
    let open = unsafe {
        hal.open_with_callback(
            features,
            &limits,
            &hints,
            Some(Box::new(move |args: hal_vk::CreateDeviceCallbackArgs| {
                for e in extensions {
                    if !args.extensions.contains(&e) {
                        args.extensions.push(e);
                    }
                }
                if let Some(family) = video_family
                    && !args.queue_create_infos.iter().any(|q| q.queue_family_index == family)
                {
                    args.queue_create_infos.push(ash::vk::DeviceQueueCreateInfo::default().queue_family_index(family).queue_priorities(&PRIORITY));
                }
            })),
        )
    };
    drop(hal);
    let open = open.inspect_err(|e| tracing::warn!("opening the decode-capable device: {e}")).ok()?;
    let desc = wgpu::DeviceDescriptor { required_features: features, required_limits: limits, memory_hints: hints, ..Default::default() };
    let (device, queue) = unsafe { adapter.create_device_from_hal(open, &desc) }.inspect_err(|e| tracing::warn!("decode-capable device: {e}")).ok()?;
    Some(Opened { device, queue, dmabuf, video_family })
}
