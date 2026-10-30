//! Samples decoded VA-API surfaces in place: each dmabuf plane is imported as a Vulkan image
//! (R8 luma, RG8 chroma) on the wgpu device and wrapped as a wgpu texture.
//!
//! Invariants:
//! - Imported images are cached per dmabuf (inode, offsets, modifier), tagged with the window
//!   and decoder surface pool they came from. A window's entries go when its pool changes or
//!   it closes; the rest are evicted least recently used under a count and byte budget.
//! - Every draw is bracketed wlroots-style: acquire from the foreign (decoder) queue family
//!   (GENERAL to SHADER_READ_ONLY) before wgpu's submission, release back after it. Both are
//!   raw submissions on wgpu's queue from the one thread that drives wgpu.
//! - Nothing referenced by a raw submission is freed before its fence signals: evicted entries
//!   and drawn frames wait in `pending` until then, so the decoder never rewrites a surface
//!   that is still being sampled and no image is destroyed under a pending barrier.

use std::{
    collections::HashMap,
    ffi::CStr,
    os::fd::{AsRawFd, BorrowedFd, IntoRawFd},
    sync::Arc,
};

use anyhow::{Context, Result, anyhow, bail, ensure};
use ash::vk;
use wgpu::hal::{api::Vulkan, vulkan as hal_vk};

use crate::decode::dmabuf::{DmabufFrame, Plane};

/// Device extensions needed on top of wgpu's own.
pub const EXTENSIONS: [&CStr; 4] = [
    ash::khr::external_memory_fd::NAME,
    ash::ext::external_memory_dma_buf::NAME,
    ash::ext::image_drm_format_modifier::NAME,
    ash::ext::queue_family_foreign::NAME,
];
/// Cache budget. A decoder pool is about 25 surfaces, so this fits a few windows at once.
const MAX_CACHED: usize = 96;
const MAX_CACHED_BYTES: u64 = 512 << 20;
/// Raw submissions in flight: an acquire and a release per draw.
const RING: usize = 8;

/// Whether the adapter can import dmabufs (open the device with [`EXTENSIONS`]).
pub fn supported(adapter: &hal_vk::Adapter) -> bool {
    match EXTENSIONS.iter().find(|e| !adapter.physical_device_capabilities().supports_extension(e)) {
        Some(missing) => {
            tracing::info!(?missing, "zero-copy decode unavailable");
            false
        }
        None => true,
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct Key {
    files: [(u64, u64); 2],
    planes: [(u64, u64); 2],
    modifier: u64,
    size: (u32, u32),
}

struct Entry {
    bind_group: wgpu::BindGroup,
    images: [vk::Image; 2],
    owner: u64,
    pool: usize,
    bytes: u64,
    last_used: u64,
}

/// A frame ready to draw: bind group plus the images to acquire and release around the draw.
pub struct Imported {
    pub bind_group: wgpu::BindGroup,
    pub images: [vk::Image; 2],
}

/// Something that must outlive the raw submissions up to `serial`.
enum Keep {
    Entry(#[allow(dead_code)] Entry),
    Frame(#[allow(dead_code)] Arc<DmabufFrame>),
}

struct Slot {
    cmd: vk::CommandBuffer,
    fence: vk::Fence,
    serial: u64,
}

/// What a (format, modifier) pair supports: `None` if it cannot be imported and sampled.
type Support = Option<vk::Extent3D>;

pub struct Importer {
    device: ash::Device,
    instance: ash::Instance,
    physical: vk::PhysicalDevice,
    family: u32,
    queue: vk::Queue,
    memory_fd: ash::khr::external_memory_fd::Device,
    memory_types: vk::PhysicalDeviceMemoryProperties,
    pool: vk::CommandPool,
    ring: Vec<Slot>,
    next: usize,
    submitted: u64,
    pending: Vec<(u64, Keep)>,
    cache: HashMap<Key, Entry>,
    cached_bytes: u64,
    tick: u64,
    /// Current decoder surface pool per window.
    pools: HashMap<u64, usize>,
    supported: HashMap<(vk::Format, u64), Support>,
}

impl Importer {
    pub fn new(device: &wgpu::Device) -> Result<Self> {
        let hal = unsafe { device.as_hal::<Vulkan>() }.context("not a Vulkan device")?;
        let (raw, family) = (hal.raw_device().clone(), hal.queue_family_index());
        let instance = hal.shared_instance().raw_instance().clone();
        let (physical, queue) = (hal.raw_physical_device(), hal.raw_queue());
        drop(hal);
        let memory_fd = ash::khr::external_memory_fd::Device::new(&instance, &raw);
        let memory_types = unsafe { instance.get_physical_device_memory_properties(physical) };
        unsafe {
            let pool = raw.create_command_pool(
                &vk::CommandPoolCreateInfo::default()
                    .queue_family_index(family)
                    .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER),
                None,
            )?;
            let cmds = raw.allocate_command_buffers(
                &vk::CommandBufferAllocateInfo::default().command_pool(pool).command_buffer_count(RING as u32),
            );
            let cmds = match cmds {
                Ok(c) => c,
                Err(e) => {
                    raw.destroy_command_pool(pool, None);
                    return Err(e.into());
                }
            };
            let mut ring = Vec::with_capacity(RING);
            for cmd in cmds {
                match raw.create_fence(&vk::FenceCreateInfo::default().flags(vk::FenceCreateFlags::SIGNALED), None) {
                    Ok(fence) => ring.push(Slot { cmd, fence, serial: 0 }),
                    Err(e) => {
                        for s in ring {
                            raw.destroy_fence(s.fence, None);
                        }
                        raw.destroy_command_pool(pool, None);
                        return Err(e.into());
                    }
                }
            }
            Ok(Self {
                device: raw,
                instance,
                physical,
                family,
                queue,
                memory_fd,
                memory_types,
                pool,
                ring,
                next: 0,
                submitted: 0,
                pending: Vec::new(),
                cache: HashMap::new(),
                cached_bytes: 0,
                tick: 0,
                pools: HashMap::new(),
                supported: HashMap::new(),
            })
        }
    }

    /// Bind group sampling `frame` for window `owner`. The draw must be wrapped in
    /// `acquire` and `release`.
    pub fn import(
        &mut self,
        device: &wgpu::Device,
        layout: &wgpu::BindGroupLayout,
        sampler: &wgpu::Sampler,
        owner: u64,
        frame: &DmabufFrame,
    ) -> Result<Imported> {
        self.collect();
        if self.pools.insert(owner, frame.pool).is_some_and(|p| p != frame.pool) {
            // New decoder or resized stream: the old surfaces will never come back.
            self.evict(|e| e.owner == owner && e.pool != frame.pool);
        }
        let key = key(frame)?;
        self.tick += 1;
        if !self.cache.contains_key(&key) {
            let entry = self.create(device, layout, sampler, owner, frame)?;
            self.cached_bytes += entry.bytes;
            self.cache.insert(key, entry);
            self.trim(key);
            tracing::debug!(owner, entries = self.cache.len(), mib = self.cached_bytes >> 20, "zero-copy surface imported");
        }
        let entry = self.cache.get_mut(&key).expect("just inserted");
        entry.last_used = self.tick;
        Ok(Imported { bind_group: entry.bind_group.clone(), images: entry.images })
    }

    /// Drops every cached surface of a closed window.
    pub fn forget(&mut self, owner: u64) {
        self.pools.remove(&owner);
        self.evict(|e| e.owner == owner);
    }

    /// Takes the images over from the decoder before a draw that samples them.
    pub fn acquire(&mut self, images: [vk::Image; 2]) -> Result<()> {
        let barriers = images.map(|image| {
            barrier(image)
                .src_access_mask(vk::AccessFlags::empty())
                .dst_access_mask(vk::AccessFlags::SHADER_READ)
                .old_layout(vk::ImageLayout::GENERAL)
                .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                .src_queue_family_index(vk::QUEUE_FAMILY_FOREIGN_EXT)
                .dst_queue_family_index(self.family)
        });
        self.submit(&barriers, vk::PipelineStageFlags::TOP_OF_PIPE, vk::PipelineStageFlags::FRAGMENT_SHADER)?;
        Ok(())
    }

    /// Hands the images back to the decoder after the draw was submitted. `frame` stays alive
    /// until this release (and so the draw before it) has finished on the GPU.
    pub fn release(&mut self, images: [vk::Image; 2], frame: Arc<DmabufFrame>) -> Result<()> {
        let barriers = images.map(|image| {
            barrier(image)
                .src_access_mask(vk::AccessFlags::SHADER_READ)
                .dst_access_mask(vk::AccessFlags::empty())
                .old_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                .new_layout(vk::ImageLayout::GENERAL)
                .src_queue_family_index(self.family)
                .dst_queue_family_index(vk::QUEUE_FAMILY_FOREIGN_EXT)
        });
        let serial =
            self.submit(&barriers, vk::PipelineStageFlags::FRAGMENT_SHADER, vk::PipelineStageFlags::BOTTOM_OF_PIPE)?;
        self.pending.push((serial, Keep::Frame(frame)));
        Ok(())
    }

    fn evict(&mut self, mut doomed: impl FnMut(&Entry) -> bool) {
        let keys: Vec<Key> = self.cache.iter().filter(|(_, e)| doomed(e)).map(|(k, _)| *k).collect();
        for k in keys {
            self.retire(k);
        }
    }

    /// Evicts least recently used entries (never `keep`) until the cache fits its budget.
    fn trim(&mut self, keep: Key) {
        while self.cache.len() > MAX_CACHED || self.cached_bytes > MAX_CACHED_BYTES {
            let oldest = self.cache.iter().filter(|(k, _)| **k != keep).min_by_key(|(_, e)| e.last_used).map(|(k, _)| *k);
            let Some(k) = oldest else { break };
            self.retire(k);
        }
    }

    /// Moves an entry out of the cache; it is dropped once no raw submission can use it.
    fn retire(&mut self, k: Key) {
        if let Some(e) = self.cache.remove(&k) {
            self.cached_bytes -= e.bytes;
            self.pending.push((self.submitted, Keep::Entry(e)));
        }
    }

    /// Frees what waited for raw submissions that have now finished.
    fn collect(&mut self) {
        let incomplete = self
            .ring
            .iter()
            .filter(|s| s.serial > 0 && !matches!(unsafe { self.device.get_fence_status(s.fence) }, Ok(true)))
            .map(|s| s.serial)
            .min();
        let done = incomplete.map_or(self.submitted, |s| s - 1);
        self.pending.retain(|(serial, _)| *serial > done);
    }

    fn create(
        &mut self,
        device: &wgpu::Device,
        layout: &wgpu::BindGroupLayout,
        sampler: &wgpu::Sampler,
        owner: u64,
        frame: &DmabufFrame,
    ) -> Result<Entry> {
        let (w, h) = (frame.width, frame.height);
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        let y = self.texture(device, &frame.planes[0], vk::Format::R8_UNORM, wgpu::TextureFormat::R8Unorm, w, h)?;
        let uv = self.texture(device, &frame.planes[1], vk::Format::R8G8_UNORM, wgpu::TextureFormat::Rg8Unorm, cw, ch)?;
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("nv12 dmabuf"),
            layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&y.1.create_view(&Default::default())),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&uv.1.create_view(&Default::default())),
                },
                wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::Sampler(sampler) },
            ],
        });
        // Both planes usually live in one buffer object; count it once.
        let [a, b] = frame.planes;
        let bytes = if a.fd == b.fd { y.2.max(uv.2) } else { y.2 + uv.2 };
        Ok(Entry { bind_group, images: [y.0, uv.0], owner, pool: frame.pool, bytes, last_used: 0 })
    }

    /// Largest importable and linearly sampleable extent for this format and modifier.
    fn support(&mut self, format: vk::Format, modifier: u64) -> Support {
        let (instance, physical) = (&self.instance, self.physical);
        *self.supported.entry((format, modifier)).or_insert_with(|| unsafe {
            let mut list = vk::DrmFormatModifierPropertiesListEXT::default();
            let mut props = vk::FormatProperties2::default().push_next(&mut list);
            instance.get_physical_device_format_properties2(physical, format, &mut props);
            let mut mods = vec![vk::DrmFormatModifierPropertiesEXT::default(); list.drm_format_modifier_count as usize];
            let mut list = vk::DrmFormatModifierPropertiesListEXT::default().drm_format_modifier_properties(&mut mods);
            let mut props = vk::FormatProperties2::default().push_next(&mut list);
            instance.get_physical_device_format_properties2(physical, format, &mut props);
            let needed = vk::FormatFeatureFlags::SAMPLED_IMAGE | vk::FormatFeatureFlags::SAMPLED_IMAGE_FILTER_LINEAR;
            let listed = mods.iter().any(|m| {
                m.drm_format_modifier == modifier
                    && m.drm_format_modifier_plane_count == 1
                    && m.drm_format_modifier_tiling_features.contains(needed)
            });
            let mut drm = vk::PhysicalDeviceImageDrmFormatModifierInfoEXT::default()
                .drm_format_modifier(modifier)
                .sharing_mode(vk::SharingMode::EXCLUSIVE);
            let mut ext_info =
                vk::PhysicalDeviceExternalImageFormatInfo::default().handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
            let info = vk::PhysicalDeviceImageFormatInfo2::default()
                .format(format)
                .ty(vk::ImageType::TYPE_2D)
                .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
                .usage(vk::ImageUsageFlags::SAMPLED)
                .push_next(&mut drm)
                .push_next(&mut ext_info);
            let mut ext_props = vk::ExternalImageFormatProperties::default();
            let mut out = vk::ImageFormatProperties2::default().push_next(&mut ext_props);
            let queried = instance.get_physical_device_image_format_properties2(physical, &info, &mut out).is_ok();
            let max = out.image_format_properties.max_extent;
            let importable = ext_props
                .external_memory_properties
                .external_memory_features
                .contains(vk::ExternalMemoryFeatureFlags::IMPORTABLE);
            let ok = listed && queried && importable;
            if !ok {
                tracing::info!(?format, modifier = format_args!("{modifier:#x}"), listed, queried, importable, "modifier not importable");
            }
            ok.then_some(max)
        })
    }

    /// Imports one plane as a Vulkan image bound to the dmabuf, wrapped as a wgpu texture
    /// that destroys the image and memory once wgpu is done with it. Returns the bytes of
    /// memory it pins.
    fn texture(
        &mut self,
        device: &wgpu::Device,
        plane: &Plane,
        vk_format: vk::Format,
        format: wgpu::TextureFormat,
        width: u32,
        height: u32,
    ) -> Result<(vk::Image, wgpu::Texture, u64)> {
        let max = self.support(vk_format, plane.modifier).context("format modifier not supported for sampling")?;
        ensure!(width <= max.width && height <= max.height, "{width}x{height} exceeds the importable {}x{}", max.width, max.height);
        if plane.object_size > 0 {
            let end = plane.pitch.checked_mul(height as u64).and_then(|n| n.checked_add(plane.offset));
            ensure!(end.is_some_and(|e| e <= plane.object_size), "plane extends past its dmabuf");
        }
        let raw = self.device.clone();
        let layouts = [vk::SubresourceLayout { offset: plane.offset, row_pitch: plane.pitch, ..Default::default() }];
        let mut explicit =
            vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default().drm_format_modifier(plane.modifier).plane_layouts(&layouts);
        let mut external =
            vk::ExternalMemoryImageCreateInfo::default().handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(vk_format)
            .extent(vk::Extent3D { width, height, depth: 1 })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
            .usage(vk::ImageUsageFlags::SAMPLED)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .push_next(&mut external)
            .push_next(&mut explicit);
        let image = unsafe { raw.create_image(&info, None) }.context("creating dmabuf image")?;
        let (memory, bytes) = match unsafe { self.bind_memory(image, plane) } {
            Ok(m) => m,
            Err(e) => {
                unsafe { raw.destroy_image(image, None) };
                return Err(e);
            }
        };
        let size = wgpu::Extent3d { width, height, depth_or_array_layers: 1 };
        let hal_desc = wgpu::hal::TextureDescriptor {
            label: Some("dmabuf plane"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUses::RESOURCE,
            memory_flags: wgpu::hal::MemoryFlags::empty(),
            view_formats: vec![],
        };
        let destroy = Box::new(move || unsafe {
            raw.destroy_image(image, None);
            raw.free_memory(memory, None);
        });
        let hal = unsafe { device.as_hal::<Vulkan>() }.ok_or_else(|| anyhow!("not a Vulkan device"))?;
        let hal_tex = unsafe { hal.texture_from_raw(image, &hal_desc, Some(destroy), hal_vk::TextureMemory::External) };
        drop(hal);
        let desc = wgpu::TextureDescriptor {
            label: Some("dmabuf plane"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        };
        // `acquire` moves the image to SHADER_READ_ONLY_OPTIMAL before every draw.
        let texture = unsafe { device.create_texture_from_hal::<Vulkan>(hal_tex, &desc, wgpu::TextureUses::RESOURCE) };
        Ok((image, texture, bytes))
    }

    /// Imports a duplicate of the plane's dmabuf fd as dedicated memory for `image`.
    unsafe fn bind_memory(&self, image: vk::Image, plane: &Plane) -> Result<(vk::DeviceMemory, u64)> {
        unsafe {
            let reqs = self.device.get_image_memory_requirements(image);
            ensure!(plane.object_size == 0 || reqs.size <= plane.object_size, "dmabuf smaller than the image");
            let fd = BorrowedFd::borrow_raw(plane.fd).try_clone_to_owned().context("dup dmabuf fd")?;
            let mut props = vk::MemoryFdPropertiesKHR::default();
            self.memory_fd.get_memory_fd_properties(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT, fd.as_raw_fd(), &mut props)?;
            let bits = reqs.memory_type_bits & props.memory_type_bits;
            let index = memory_type(&self.memory_types, bits).context("no memory type fits the dmabuf")?;
            let size = if plane.object_size > 0 { plane.object_size } else { reqs.size };
            let mut import = vk::ImportMemoryFdInfoKHR::default()
                .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
                .fd(fd.as_raw_fd());
            let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
            let alloc = vk::MemoryAllocateInfo::default()
                .allocation_size(size)
                .memory_type_index(index)
                .push_next(&mut import)
                .push_next(&mut dedicated);
            let memory = self.device.allocate_memory(&alloc, None).context("importing dmabuf")?;
            // A successful import owns the fd.
            let _ = fd.into_raw_fd();
            if let Err(e) = self.device.bind_image_memory(image, memory, 0) {
                self.device.free_memory(memory, None);
                bail!("binding dmabuf memory: {e}");
            }
            Ok((memory, size))
        }
    }

    /// Records `barriers` into the next ring slot and submits them on wgpu's queue. Returns
    /// the submission's serial.
    fn submit(
        &mut self,
        barriers: &[vk::ImageMemoryBarrier<'_>],
        src: vk::PipelineStageFlags,
        dst: vk::PipelineStageFlags,
    ) -> Result<u64> {
        let slot = &mut self.ring[self.next];
        self.next = (self.next + 1) % RING;
        unsafe {
            // Used RING submissions ago, so this is signaled in practice.
            self.device.wait_for_fences(&[slot.fence], true, u64::MAX)?;
            self.device.reset_fences(&[slot.fence])?;
            self.device.begin_command_buffer(
                slot.cmd,
                &vk::CommandBufferBeginInfo::default().flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )?;
            self.device.cmd_pipeline_barrier(slot.cmd, src, dst, vk::DependencyFlags::empty(), &[], &[], barriers);
            self.device.end_command_buffer(slot.cmd)?;
            let cmds = [slot.cmd];
            // Same queue as wgpu and only from the thread that drives wgpu, so submission order
            // places this exactly between wgpu's own submissions.
            self.device.queue_submit(self.queue, &[vk::SubmitInfo::default().command_buffers(&cmds)], slot.fence)?;
        }
        self.submitted += 1;
        slot.serial = self.submitted;
        Ok(self.submitted)
    }
}

impl Drop for Importer {
    fn drop(&mut self) {
        unsafe {
            let fences: Vec<vk::Fence> = self.ring.iter().map(|s| s.fence).collect();
            // Unbounded: the pool and everything in `pending` may only go once the GPU is done.
            let _ = self.device.wait_for_fences(&fences, true, u64::MAX);
            self.pending.clear();
            for f in fences {
                self.device.destroy_fence(f, None);
            }
            self.device.destroy_command_pool(self.pool, None);
        }
    }
}

fn barrier<'a>(image: vk::Image) -> vk::ImageMemoryBarrier<'a> {
    vk::ImageMemoryBarrier::default().image(image).subresource_range(vk::ImageSubresourceRange {
        aspect_mask: vk::ImageAspectFlags::COLOR,
        base_mip_level: 0,
        level_count: 1,
        base_array_layer: 0,
        layer_count: 1,
    })
}

/// A memory type allowed by `bits`, preferring device local memory.
fn memory_type(props: &vk::PhysicalDeviceMemoryProperties, bits: u32) -> Option<u32> {
    let allowed = |i: &u32| bits & (1 << i) != 0;
    let mut types = (0..props.memory_type_count.min(32)).filter(allowed);
    let local = |i: &u32| props.memory_types[*i as usize].property_flags.contains(vk::MemoryPropertyFlags::DEVICE_LOCAL);
    types.clone().find(local).or_else(|| types.next())
}

/// Identity of a decoder surface: the dmabuf inode plus everything that shapes the import.
fn key(frame: &DmabufFrame) -> Result<Key> {
    let file = |fd| {
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstat(fd, &mut st) } != 0 {
            bail!("fstat dmabuf: {}", std::io::Error::last_os_error());
        }
        Ok((st.st_dev, st.st_ino))
    };
    let [a, b] = frame.planes;
    ensure!(a.modifier == b.modifier, "planes with different modifiers");
    Ok(Key {
        files: [file(a.fd)?, file(b.fd)?],
        planes: [(a.offset, a.pitch), (b.offset, b.pitch)],
        modifier: a.modifier,
        size: (frame.width, frame.height),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_type_prefers_device_local() {
        let mut p = vk::PhysicalDeviceMemoryProperties { memory_type_count: 3, ..Default::default() };
        p.memory_types[0].property_flags = vk::MemoryPropertyFlags::HOST_VISIBLE;
        p.memory_types[2].property_flags = vk::MemoryPropertyFlags::DEVICE_LOCAL;
        assert_eq!(memory_type(&p, 0b101), Some(2));
        assert_eq!(memory_type(&p, 0b001), Some(0));
        assert_eq!(memory_type(&p, 0b1000), None);
    }
}
