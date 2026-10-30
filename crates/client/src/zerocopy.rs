//! Samples decoded VA-API surfaces in place: each dmabuf plane is imported as a Vulkan image
//! (R8 luma, RG8 chroma) on the wgpu device and wrapped as a wgpu texture.
//!
//! Invariants:
//! - Imported images are cached per dmabuf (inode, offsets, modifier); ffmpeg reuses a fixed
//!   surface pool, so after warm-up no frame imports anything.
//! - An image is only sampled while the `DmabufFrame` it came from is alive (the caller holds
//!   it until the GPU finished), so the decoder never writes a surface being read.
//! - Before each new frame is drawn, its images are acquired from the foreign (decoder) queue
//!   family in a small submission on the same queue, which makes the decoder's writes visible.
//! - Every Vulkan call happens on the thread that owns the `Gpu`.

use std::{
    collections::HashMap,
    ffi::CStr,
    os::fd::{AsRawFd, BorrowedFd, IntoRawFd},
};

use anyhow::{Context, Result, anyhow, bail, ensure};
use ash::vk;
use wgpu::hal::{api::Vulkan, vulkan as hal_vk};

use crate::decode::dmabuf::{DmabufFrame, Plane};

/// Device extensions needed on top of wgpu's own.
const EXTENSIONS: [&CStr; 4] = [
    ash::khr::external_memory_fd::NAME,
    ash::ext::external_memory_dma_buf::NAME,
    ash::ext::image_drm_format_modifier::NAME,
    ash::ext::queue_family_foreign::NAME,
];
/// Cached surfaces; ffmpeg's pool is about refs + extra_hw_frames, so this is never reached
/// unless surfaces churn, in which case the cache restarts.
const MAX_CACHED: usize = 48;
const RING: usize = 4;

/// Opens a wgpu device with the dmabuf import extensions, or `None` if the adapter lacks them.
pub fn open_device(adapter: &wgpu::Adapter) -> Option<(wgpu::Device, wgpu::Queue)> {
    let hal = unsafe { adapter.as_hal::<Vulkan>() }?;
    if let Some(missing) = EXTENSIONS.iter().find(|e| !hal.physical_device_capabilities().supports_extension(e)) {
        tracing::info!(?missing, "zero-copy decode unavailable");
        return None;
    }
    let (features, limits, hints) = (wgpu::Features::empty(), wgpu::Limits::default(), wgpu::MemoryHints::default());
    let open = unsafe {
        hal.open_with_callback(
            features,
            &limits,
            &hints,
            Some(Box::new(|args: hal_vk::CreateDeviceCallbackArgs| {
                for e in EXTENSIONS {
                    if !args.extensions.contains(&e) {
                        args.extensions.push(e);
                    }
                }
            })),
        )
    };
    drop(hal);
    let open = open.inspect_err(|e| tracing::warn!("opening zero-copy device: {e}")).ok()?;
    let desc = wgpu::DeviceDescriptor { required_features: features, required_limits: limits, memory_hints: hints, ..Default::default() };
    unsafe { adapter.create_device_from_hal(open, &desc) }.inspect_err(|e| tracing::warn!("zero-copy device: {e}")).ok()
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct Key {
    files: [(u64, u64); 2],
    planes: [(u64, u64); 2],
    modifier: u64,
    size: (u32, u32),
}

struct Entry {
    images: [vk::Image; 2],
    /// Laid out for sampling by an earlier acquire; the first acquire starts from UNDEFINED.
    acquired: bool,
    bind_group: wgpu::BindGroup,
}

pub struct Importer {
    device: ash::Device,
    instance: ash::Instance,
    physical: vk::PhysicalDevice,
    family: u32,
    queue: vk::Queue,
    memory_fd: ash::khr::external_memory_fd::Device,
    pool: vk::CommandPool,
    ring: Vec<(vk::CommandBuffer, vk::Fence)>,
    next: usize,
    cache: HashMap<Key, Entry>,
    supported: HashMap<(vk::Format, u64), bool>,
}

impl Importer {
    pub fn new(device: &wgpu::Device) -> Result<Self> {
        let hal = unsafe { device.as_hal::<Vulkan>() }.context("not a Vulkan device")?;
        let (raw, family) = (hal.raw_device().clone(), hal.queue_family_index());
        let instance = hal.shared_instance().raw_instance().clone();
        let (physical, queue) = (hal.raw_physical_device(), hal.raw_queue());
        drop(hal);
        let memory_fd = ash::khr::external_memory_fd::Device::new(&instance, &raw);
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
                    Ok(f) => ring.push((cmd, f)),
                    Err(e) => {
                        for (_, f) in ring {
                            raw.destroy_fence(f, None);
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
                pool,
                ring,
                next: 0,
                cache: HashMap::new(),
                supported: HashMap::new(),
            })
        }
    }

    /// Bind group sampling `frame`, ready for drawing on the next submission.
    pub fn import(
        &mut self,
        device: &wgpu::Device,
        layout: &wgpu::BindGroupLayout,
        sampler: &wgpu::Sampler,
        frame: &DmabufFrame,
    ) -> Result<wgpu::BindGroup> {
        let key = key(frame)?;
        if !self.cache.contains_key(&key) {
            if self.cache.len() >= MAX_CACHED {
                tracing::debug!("zero-copy cache full; restarting it");
                self.cache.clear();
            }
            let entry = self.create(device, layout, sampler, frame)?;
            self.cache.insert(key, entry);
        }
        let entry = self.cache.get(&key).unwrap();
        let (images, acquired) = (entry.images, entry.acquired);
        self.acquire(images, acquired)?;
        let entry = self.cache.get_mut(&key).unwrap();
        entry.acquired = true;
        Ok(entry.bind_group.clone())
    }

    fn create(
        &mut self,
        device: &wgpu::Device,
        layout: &wgpu::BindGroupLayout,
        sampler: &wgpu::Sampler,
        frame: &DmabufFrame,
    ) -> Result<Entry> {
        let (w, h) = (frame.width, frame.height);
        let y = self.texture(device, &frame.planes[0], vk::Format::R8_UNORM, wgpu::TextureFormat::R8Unorm, w, h)?;
        let uv = self.texture(
            device,
            &frame.planes[1],
            vk::Format::R8G8_UNORM,
            wgpu::TextureFormat::Rg8Unorm,
            w.div_ceil(2),
            h.div_ceil(2),
        )?;
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
        Ok(Entry { images: [y.0, uv.0], acquired: false, bind_group })
    }

    fn supports(&mut self, format: vk::Format, modifier: u64) -> bool {
        let (instance, physical) = (&self.instance, self.physical);
        *self.supported.entry((format, modifier)).or_insert_with(|| unsafe {
            let mut list = vk::DrmFormatModifierPropertiesListEXT::default();
            let mut props = vk::FormatProperties2::default().push_next(&mut list);
            instance.get_physical_device_format_properties2(physical, format, &mut props);
            let mut mods = vec![vk::DrmFormatModifierPropertiesEXT::default(); list.drm_format_modifier_count as usize];
            let mut list = vk::DrmFormatModifierPropertiesListEXT::default().drm_format_modifier_properties(&mut mods);
            let mut props = vk::FormatProperties2::default().push_next(&mut list);
            instance.get_physical_device_format_properties2(physical, format, &mut props);
            let ok = mods.iter().any(|m| {
                m.drm_format_modifier == modifier
                    && m.drm_format_modifier_plane_count == 1
                    && m.drm_format_modifier_tiling_features.contains(vk::FormatFeatureFlags::SAMPLED_IMAGE)
            });
            if !ok {
                tracing::info!(?format, modifier = format_args!("{modifier:#x}"), "modifier not importable");
            }
            ok
        })
    }

    /// Imports one plane as a Vulkan image bound to the dmabuf, wrapped as a wgpu texture
    /// that destroys the image and memory once wgpu is done with it.
    fn texture(
        &mut self,
        device: &wgpu::Device,
        plane: &Plane,
        vk_format: vk::Format,
        format: wgpu::TextureFormat,
        width: u32,
        height: u32,
    ) -> Result<(vk::Image, wgpu::Texture)> {
        ensure!(self.supports(vk_format, plane.modifier), "format modifier not supported for sampling");
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
        let memory = match unsafe { self.bind_memory(image, plane) } {
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
        // The image is moved to SHADER_READ_ONLY_OPTIMAL by `acquire` before any draw.
        let texture = unsafe { device.create_texture_from_hal::<Vulkan>(hal_tex, &desc, wgpu::TextureUses::RESOURCE) };
        Ok((image, texture))
    }

    /// Imports a duplicate of the plane's dmabuf fd as dedicated memory for `image`.
    unsafe fn bind_memory(&self, image: vk::Image, plane: &Plane) -> Result<vk::DeviceMemory> {
        unsafe {
            let reqs = self.device.get_image_memory_requirements(image);
            ensure!(plane.object_size == 0 || reqs.size <= plane.object_size, "dmabuf smaller than the image");
            let fd = BorrowedFd::borrow_raw(plane.fd).try_clone_to_owned().context("dup dmabuf fd")?;
            let mut props = vk::MemoryFdPropertiesKHR::default();
            self.memory_fd.get_memory_fd_properties(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT, fd.as_raw_fd(), &mut props)?;
            let bits = reqs.memory_type_bits & props.memory_type_bits;
            ensure!(bits != 0, "no memory type fits the dmabuf");
            let size = if plane.object_size > 0 { plane.object_size } else { reqs.size };
            let mut import = vk::ImportMemoryFdInfoKHR::default()
                .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
                .fd(fd.as_raw_fd());
            let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
            let alloc = vk::MemoryAllocateInfo::default()
                .allocation_size(size)
                .memory_type_index(bits.trailing_zeros())
                .push_next(&mut import)
                .push_next(&mut dedicated);
            let memory = self.device.allocate_memory(&alloc, None).context("importing dmabuf")?;
            // A successful import owns the fd.
            let _ = fd.into_raw_fd();
            if let Err(e) = self.device.bind_image_memory(image, memory, 0) {
                self.device.free_memory(memory, None);
                bail!("binding dmabuf memory: {e}");
            }
            Ok(memory)
        }
    }

    /// Takes the images over from the decoder's (foreign) queue family so its writes are
    /// visible, and leaves them in SHADER_READ_ONLY_OPTIMAL as wgpu expects.
    fn acquire(&mut self, images: [vk::Image; 2], acquired: bool) -> Result<()> {
        let (cmd, fence) = self.ring[self.next];
        self.next = (self.next + 1) % self.ring.len();
        let old = if acquired { vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL } else { vk::ImageLayout::UNDEFINED };
        let barriers = images.map(|image| {
            vk::ImageMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::empty())
                .dst_access_mask(vk::AccessFlags::SHADER_READ)
                .old_layout(old)
                .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                .src_queue_family_index(vk::QUEUE_FAMILY_FOREIGN_EXT)
                .dst_queue_family_index(self.family)
                .image(image)
                .subresource_range(vk::ImageSubresourceRange {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    base_mip_level: 0,
                    level_count: 1,
                    base_array_layer: 0,
                    layer_count: 1,
                })
        });
        unsafe {
            // The slot was used RING frames ago; its fence is long signaled in practice.
            self.device.wait_for_fences(&[fence], true, 1_000_000_000)?;
            self.device.reset_fences(&[fence])?;
            self.device.begin_command_buffer(
                cmd,
                &vk::CommandBufferBeginInfo::default().flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )?;
            self.device.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::FRAGMENT_SHADER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &barriers,
            );
            self.device.end_command_buffer(cmd)?;
            let cmds = [cmd];
            // Same queue as wgpu, and only from the thread that drives wgpu, so submission
            // order puts this barrier before the draw that samples the images.
            self.device.queue_submit(self.queue, &[vk::SubmitInfo::default().command_buffers(&cmds)], fence)?;
        }
        Ok(())
    }
}

impl Drop for Importer {
    fn drop(&mut self) {
        unsafe {
            let fences: Vec<vk::Fence> = self.ring.iter().map(|(_, f)| *f).collect();
            let _ = self.device.wait_for_fences(&fences, true, 1_000_000_000);
            for f in fences {
                self.device.destroy_fence(f, None);
            }
            self.device.destroy_command_pool(self.pool, None);
        }
    }
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
