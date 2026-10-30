//! H.264 decoding with Vulkan Video on the renderer's device, no ffmpeg involved. The
//! `umac_h264` parser (decode thread) validates each access unit and works out picture order
//! counts and references; here a job is decoded on the device's video decode queue and the
//! picture is copied into two plain textures (R8 luma, RG8 CbCr) that the NV12 shader samples.
//!
//! Synchronization stays on the GPU: the decode signals a semaphore the copy on wgpu's queue
//! waits for, the draw is submitted after the copy on that same queue, and the next decode
//! waits for the copy through another semaphore. The CPU only waits on a fence before reusing
//! a command buffer, by which time the previous frame is normally long done. DPB images are shared concurrently by both queue families,
//! so no ownership transfers are needed.

use std::{ffi::CStr, mem, ptr, sync::Arc};

use anyhow::{Context as _, Result, anyhow, bail, ensure};
use ash::vk::{self, native as std_vk};
use umac_h264::{DecodeJob, Pps, Sps};
use wgpu::hal::{api::Vulkan, vulkan as hal_vk};

/// Device extensions for H.264 decoding.
pub const EXTENSIONS: [&CStr; 3] = [ash::khr::video_queue::NAME, ash::khr::video_decode_queue::NAME, ash::khr::video_decode_h264::NAME];

const PICTURE_FORMAT: vk::Format = vk::Format::G8_B8R8_2PLANE_420_UNORM;

/// `UMAC_VULKAN_VIDEO=0` turns this path off, for comparisons.
pub fn allowed() -> bool {
    std::env::var("UMAC_VULKAN_VIDEO").map_or(true, |v| v != "0")
}

/// The queue family that decodes H.264 (High profile) on this adapter, if any.
pub fn decode_family(adapter: &hal_vk::Adapter) -> Option<u32> {
    if !allowed() || EXTENSIONS.iter().any(|e| !adapter.physical_device_capabilities().supports_extension(e)) {
        return None;
    }
    let shared = adapter.shared_instance();
    let (instance, physical) = (shared.raw_instance(), adapter.raw_physical_device());
    let count = unsafe { instance.get_physical_device_queue_family_properties2_len(physical) };
    let mut video = vec![vk::QueueFamilyVideoPropertiesKHR::default(); count];
    let mut props: Vec<vk::QueueFamilyProperties2> =
        video.iter_mut().map(|v| vk::QueueFamilyProperties2::default().push_next(v)).collect();
    unsafe { instance.get_physical_device_queue_family_properties2(physical, &mut props) };
    let flags: Vec<vk::QueueFlags> = props.iter().map(|p| p.queue_family_properties.queue_flags).collect();
    drop(props);
    let family = (0..count).find(|&i| {
        flags[i].contains(vk::QueueFlags::VIDEO_DECODE_KHR) && video[i].video_codec_operations.contains(vk::VideoCodecOperationFlagsKHR::DECODE_H264)
    })?;
    let fns = ash::khr::video_queue::Instance::new(shared.entry(), instance);
    let mut h264 = profile_h264(std_vk::StdVideoH264ProfileIdc_STD_VIDEO_H264_PROFILE_IDC_HIGH);
    let profile = profile(&mut h264);
    let caps = capabilities(&fns, physical, &profile);
    match caps {
        Ok(_) => Some(family as u32),
        Err(e) => {
            tracing::info!("Vulkan Video H.264 profile unsupported: {e}");
            None
        }
    }
}

fn profile_h264(idc: std_vk::StdVideoH264ProfileIdc) -> vk::VideoDecodeH264ProfileInfoKHR<'static> {
    vk::VideoDecodeH264ProfileInfoKHR::default().std_profile_idc(idc).picture_layout(vk::VideoDecodeH264PictureLayoutFlagsKHR::PROGRESSIVE)
}

fn profile<'a>(h264: &'a mut vk::VideoDecodeH264ProfileInfoKHR<'static>) -> vk::VideoProfileInfoKHR<'a> {
    vk::VideoProfileInfoKHR::default()
        .video_codec_operation(vk::VideoCodecOperationFlagsKHR::DECODE_H264)
        .chroma_subsampling(vk::VideoChromaSubsamplingFlagsKHR::TYPE_420)
        .luma_bit_depth(vk::VideoComponentBitDepthFlagsKHR::TYPE_8)
        .chroma_bit_depth(vk::VideoComponentBitDepthFlagsKHR::TYPE_8)
        .push_next(h264)
}

/// What the implementation reports for a profile, the parts this decoder needs.
#[derive(Debug, Clone, Copy)]
struct Caps {
    max_extent: vk::Extent2D,
    max_dpb_slots: u32,
    max_active_references: u32,
    size_alignment: u64,
    coincide: bool,
    distinct: bool,
    std_header: vk::ExtensionProperties,
}

fn capabilities(fns: &ash::khr::video_queue::Instance, physical: vk::PhysicalDevice, profile: &vk::VideoProfileInfoKHR<'_>) -> Result<Caps> {
    let mut h264 = vk::VideoDecodeH264CapabilitiesKHR::default();
    let mut decode = vk::VideoDecodeCapabilitiesKHR::default();
    let mut caps = vk::VideoCapabilitiesKHR::default().push_next(&mut decode).push_next(&mut h264);
    unsafe { (fns.fp().get_physical_device_video_capabilities_khr)(physical, profile, &mut caps) }.result()?;
    let mut out = Caps {
        max_extent: caps.max_coded_extent,
        max_dpb_slots: caps.max_dpb_slots,
        max_active_references: caps.max_active_reference_pictures,
        size_alignment: caps.min_bitstream_buffer_size_alignment.max(1),
        coincide: false,
        distinct: false,
        std_header: caps.std_header_version,
    };
    out.coincide = decode.flags.contains(vk::VideoDecodeCapabilityFlagsKHR::DPB_AND_OUTPUT_COINCIDE);
    out.distinct = decode.flags.contains(vk::VideoDecodeCapabilityFlagsKHR::DPB_AND_OUTPUT_DISTINCT);
    Ok(out)
}

/// Device handles shared by every window's decoder.
pub struct Context {
    device: ash::Device,
    physical: vk::PhysicalDevice,
    instance_fns: ash::khr::video_queue::Instance,
    video_fns: ash::khr::video_queue::Device,
    decode_fns: ash::khr::video_decode_queue::Device,
    memory_types: vk::PhysicalDeviceMemoryProperties,
    decode_family: u32,
    decode_queue: vk::Queue,
    gfx_family: u32,
    gfx_queue: vk::Queue,
}

impl Context {
    /// `device` must have been opened with [`EXTENSIONS`] and a queue in `decode_family`.
    pub fn new(device: &wgpu::Device, decode_family: u32) -> Result<Arc<Self>> {
        let hal = unsafe { device.as_hal::<Vulkan>() }.context("not a Vulkan device")?;
        let raw = hal.raw_device().clone();
        let shared = hal.shared_instance();
        let (instance, entry) = (shared.raw_instance().clone(), shared.entry().clone());
        let (physical, gfx_family, gfx_queue) = (hal.raw_physical_device(), hal.queue_family_index(), hal.raw_queue());
        drop(hal);
        let decode_queue = unsafe { raw.get_device_queue(decode_family, 0) };
        Ok(Arc::new(Self {
            instance_fns: ash::khr::video_queue::Instance::new(&entry, &instance),
            video_fns: ash::khr::video_queue::Device::new(&instance, &raw),
            decode_fns: ash::khr::video_decode_queue::Device::new(&instance, &raw),
            memory_types: unsafe { instance.get_physical_device_memory_properties(physical) },
            device: raw,
            physical,
            decode_family,
            decode_queue,
            gfx_family,
            gfx_queue,
        }))
    }

    fn memory_type(&self, bits: u32, wanted: vk::MemoryPropertyFlags) -> Result<u32> {
        let types = &self.memory_types.memory_types[..self.memory_types.memory_type_count as usize];
        (0..types.len() as u32)
            .find(|&i| bits & (1 << i) != 0 && types[i as usize].property_flags.contains(wanted))
            .or_else(|| (0..types.len() as u32).find(|&i| bits & (1 << i) != 0))
            .ok_or_else(|| anyhow!("no memory type for {bits:#x}"))
    }

    unsafe fn allocate(&self, reqs: vk::MemoryRequirements, wanted: vk::MemoryPropertyFlags) -> Result<vk::DeviceMemory> {
        let info = vk::MemoryAllocateInfo::default().allocation_size(reqs.size).memory_type_index(self.memory_type(reqs.memory_type_bits, wanted)?);
        Ok(unsafe { self.device.allocate_memory(&info, None) }?)
    }
}

/// Zero-initialized Video Std structs: plain C data where zero is a valid (empty) value.
fn zeroed<T: Copy>() -> T {
    // SAFETY: only used for the bindgen `StdVideo*` structs, all integers, bitfields and null
    // pointers.
    unsafe { mem::zeroed() }
}

fn level(idc: u8) -> std_vk::StdVideoH264LevelIdc {
    match idc {
        9 | 11 => std_vk::StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_1_1,
        10 => std_vk::StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_1_0,
        12 => std_vk::StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_1_2,
        13 => std_vk::StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_1_3,
        20 => std_vk::StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_2_0,
        21 => std_vk::StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_2_1,
        22 => std_vk::StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_2_2,
        30 => std_vk::StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_3_0,
        31 => std_vk::StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_3_1,
        32 => std_vk::StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_3_2,
        40 => std_vk::StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_4_0,
        41 => std_vk::StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_4_1,
        42 => std_vk::StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_4_2,
        50 => std_vk::StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_5_0,
        51 => std_vk::StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_5_1,
        52 => std_vk::StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_5_2,
        60 => std_vk::StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_6_0,
        61 => std_vk::StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_6_1,
        _ => std_vk::StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_6_2,
    }
}

fn std_sps(s: &Sps) -> std_vk::StdVideoH264SequenceParameterSet {
    let mut v: std_vk::StdVideoH264SequenceParameterSet = zeroed();
    for bit in 0..6 {
        let on = (s.constraint_flags >> (7 - bit)) & 1;
        match bit {
            0 => v.flags.set_constraint_set0_flag(on as u32),
            1 => v.flags.set_constraint_set1_flag(on as u32),
            2 => v.flags.set_constraint_set2_flag(on as u32),
            3 => v.flags.set_constraint_set3_flag(on as u32),
            4 => v.flags.set_constraint_set4_flag(on as u32),
            _ => v.flags.set_constraint_set5_flag(on as u32),
        }
    }
    v.flags.set_direct_8x8_inference_flag(s.direct_8x8_inference as u32);
    v.flags.set_frame_mbs_only_flag(1);
    v.flags.set_gaps_in_frame_num_value_allowed_flag(s.gaps_in_frame_num_allowed as u32);
    v.flags.set_frame_cropping_flag((s.crop != [0; 4]) as u32);
    v.profile_idc = s.profile_idc as u32;
    v.level_idc = level(s.level_idc);
    v.chroma_format_idc = std_vk::StdVideoH264ChromaFormatIdc_STD_VIDEO_H264_CHROMA_FORMAT_IDC_420;
    v.seq_parameter_set_id = s.id;
    v.log2_max_frame_num_minus4 = s.log2_max_frame_num - 4;
    v.pic_order_cnt_type = s.poc_type as u32;
    v.log2_max_pic_order_cnt_lsb_minus4 = s.log2_max_poc_lsb.saturating_sub(4);
    v.max_num_ref_frames = s.max_num_ref_frames;
    v.pic_width_in_mbs_minus1 = s.width_mbs - 1;
    v.pic_height_in_map_units_minus1 = s.height_mbs - 1;
    [v.frame_crop_left_offset, v.frame_crop_right_offset, v.frame_crop_top_offset, v.frame_crop_bottom_offset] = s.crop;
    v
}

fn std_pps(p: &Pps) -> std_vk::StdVideoH264PictureParameterSet {
    let mut v: std_vk::StdVideoH264PictureParameterSet = zeroed();
    v.flags.set_transform_8x8_mode_flag(p.transform_8x8_mode as u32);
    v.flags.set_constrained_intra_pred_flag(p.constrained_intra_pred as u32);
    v.flags.set_deblocking_filter_control_present_flag(p.deblocking_filter_control_present as u32);
    v.flags.set_weighted_pred_flag(p.weighted_pred as u32);
    v.flags.set_bottom_field_pic_order_in_frame_present_flag(p.bottom_field_pic_order_in_frame_present as u32);
    v.flags.set_entropy_coding_mode_flag(p.entropy_coding_mode as u32);
    v.seq_parameter_set_id = p.sps_id;
    v.pic_parameter_set_id = p.id;
    v.num_ref_idx_l0_default_active_minus1 = p.num_ref_idx_l0_default_active - 1;
    v.num_ref_idx_l1_default_active_minus1 = p.num_ref_idx_l1_default_active - 1;
    v.weighted_bipred_idc = p.weighted_bipred_idc as u32;
    v.pic_init_qp_minus26 = p.pic_init_qp_minus26;
    v.pic_init_qs_minus26 = p.pic_init_qs_minus26;
    v.chroma_qp_index_offset = p.chroma_qp_index_offset;
    v.second_chroma_qp_index_offset = p.second_chroma_qp_index_offset;
    v
}

fn std_picture(job: &DecodeJob) -> std_vk::StdVideoDecodeH264PictureInfo {
    let mut v: std_vk::StdVideoDecodeH264PictureInfo = zeroed();
    v.flags.set_is_intra(job.intra as u32);
    v.flags.set_IdrPicFlag(job.idr as u32);
    v.flags.set_is_reference(job.reference as u32);
    v.seq_parameter_set_id = job.sps.id;
    v.pic_parameter_set_id = job.pps.id;
    v.frame_num = job.frame_num as u16;
    v.idr_pic_id = job.idr_pic_id;
    v.PicOrderCnt = job.poc;
    v
}

fn std_reference(frame_num: u32, long_term: bool, poc: [i32; 2]) -> std_vk::StdVideoDecodeH264ReferenceInfo {
    let mut v: std_vk::StdVideoDecodeH264ReferenceInfo = zeroed();
    v.flags.set_used_for_long_term_reference(long_term as u32);
    v.FrameNum = frame_num as u16;
    v.PicOrderCnt = poc;
    v
}

/// What a session was created for; a job that needs anything else gets a new decoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Shape {
    profile_idc: u8,
    coded: (u32, u32),
    display: (u32, u32),
    slots: u8,
}

impl Shape {
    fn of(job: &DecodeJob) -> Self {
        Self { profile_idc: job.sps.profile_idc, coded: job.sps.coded_size(), display: job.sps.display_size(), slots: job.slots }
    }
}

/// One window's decoder: session, DPB, bitstream buffer and the sampled output planes.
pub struct Decoder {
    ctx: Arc<Context>,
    shape: Shape,
    caps: Caps,
    session: vk::VideoSessionKHR,
    session_memory: Vec<vk::DeviceMemory>,
    parameters: vk::VideoSessionParametersKHR,
    /// DPB: one array image, a layer per slot. Also the decode output when they coincide.
    dpb: vk::Image,
    dpb_memory: vk::DeviceMemory,
    dpb_view: vk::ImageView,
    /// Separate decode output, when the implementation keeps DPB and output distinct.
    output: Option<(vk::Image, vk::DeviceMemory, vk::ImageView)>,
    bitstream: vk::Buffer,
    bitstream_memory: vk::DeviceMemory,
    bitstream_ptr: *mut u8,
    bitstream_size: u64,
    planes: [vk::Image; 2],
    /// Own the plane images (destroyed by wgpu after its last use).
    _textures: [wgpu::Texture; 2],
    bind_group: wgpu::BindGroup,
    decode_pool: vk::CommandPool,
    decode_cmd: vk::CommandBuffer,
    decode_fence: vk::Fence,
    copy_pool: vk::CommandPool,
    copy_cmd: vk::CommandBuffer,
    copy_fence: vk::Fence,
    /// Signaled by the last copy, waited by the next decode.
    copied: vk::Semaphore,
    /// Signaled by each decode, waited for by the copy that follows it.
    decoded: vk::Semaphore,
    copy_pending: bool,
    fresh: bool,
    planes_ready: bool,
}

// The raw handles and the mapped pointer are only used from the render thread that owns it.
unsafe impl Send for Decoder {}

impl Decoder {
    pub fn new(ctx: &Arc<Context>, device: &wgpu::Device, bind: impl Fn(&wgpu::TextureView, &wgpu::TextureView) -> wgpu::BindGroup, job: &DecodeJob) -> Result<Self> {
        let shape = Shape::of(job);
        let mut h264 = profile_h264(job.sps.profile_idc as u32);
        let profile = profile(&mut h264);
        let caps = capabilities(&ctx.instance_fns, ctx.physical, &profile)?;
        ensure!(caps.std_header.spec_version >= vk::make_api_version(0, 1, 0, 0), "unsupported H.264 std header version");
        let (w, h) = shape.coded;
        ensure!(w <= caps.max_extent.width && h <= caps.max_extent.height, "{w}x{h} exceeds the decoder's {:?}", caps.max_extent);
        ensure!(shape.slots as u32 <= caps.max_dpb_slots, "needs {} DPB slots, decoder has {}", shape.slots, caps.max_dpb_slots);
        ensure!(caps.coincide || caps.distinct, "decoder reports neither output mode");
        let mut d = Self::empty(ctx, shape, caps, device, &bind)?;
        if let Err(e) = d.create_session(&profile, job) {
            drop(d);
            return Err(e);
        }
        Ok(d)
    }

    /// Placeholder with only the output planes and command objects; `Drop` cleans up whatever
    /// was created.
    fn empty(ctx: &Arc<Context>, shape: Shape, caps: Caps, device: &wgpu::Device, bind: &dyn Fn(&wgpu::TextureView, &wgpu::TextureView) -> wgpu::BindGroup) -> Result<Self> {
        let raw = &ctx.device;
        let (dw, dh) = shape.display;
        let (y, y_tex) = plane_texture(ctx, device, vk::Format::R8_UNORM, wgpu::TextureFormat::R8Unorm, dw, dh)?;
        let (c, c_tex) = plane_texture(ctx, device, vk::Format::R8G8_UNORM, wgpu::TextureFormat::Rg8Unorm, dw.div_ceil(2), dh.div_ceil(2))?;
        let bind_group = bind(&y_tex.create_view(&Default::default()), &c_tex.create_view(&Default::default()));
        unsafe {
            let pool = |family| {
                raw.create_command_pool(&vk::CommandPoolCreateInfo::default().queue_family_index(family).flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER), None)
            };
            let decode_pool = pool(ctx.decode_family)?;
            let copy_pool = pool(ctx.gfx_family)?;
            let one = |p| raw.allocate_command_buffers(&vk::CommandBufferAllocateInfo::default().command_pool(p).command_buffer_count(1)).map(|v| v[0]);
            let fence = || raw.create_fence(&vk::FenceCreateInfo::default().flags(vk::FenceCreateFlags::SIGNALED), None);
            Ok(Self {
                ctx: ctx.clone(),
                shape,
                caps,
                session: vk::VideoSessionKHR::null(),
                session_memory: Vec::new(),
                parameters: vk::VideoSessionParametersKHR::null(),
                dpb: vk::Image::null(),
                dpb_memory: vk::DeviceMemory::null(),
                dpb_view: vk::ImageView::null(),
                output: None,
                bitstream: vk::Buffer::null(),
                bitstream_memory: vk::DeviceMemory::null(),
                bitstream_ptr: ptr::null_mut(),
                bitstream_size: 0,
                planes: [y, c],
                _textures: [y_tex, c_tex],
                bind_group,
                decode_cmd: one(decode_pool)?,
                decode_pool,
                decode_fence: fence()?,
                copy_cmd: one(copy_pool)?,
                copy_pool,
                copy_fence: fence()?,
                copied: raw.create_semaphore(&vk::SemaphoreCreateInfo::default(), None)?,
                decoded: raw.create_semaphore(&vk::SemaphoreCreateInfo::default(), None)?,
                copy_pending: false,
                fresh: true,
                planes_ready: false,
            })
        }
    }

    fn create_session(&mut self, profile: &vk::VideoProfileInfoKHR<'_>, job: &DecodeJob) -> Result<()> {
        let ctx = self.ctx.clone();
        let raw = &ctx.device;
        let (w, h) = self.shape.coded;
        let slots = self.shape.slots as u32;
        let extent = vk::Extent2D { width: w, height: h };
        let profiles = [*profile];
        let mut profile_list = vk::VideoProfileListInfoKHR::default().profiles(&profiles);
        let families = [ctx.decode_family, ctx.gfx_family];
        let sharing = if ctx.decode_family == ctx.gfx_family { vk::SharingMode::EXCLUSIVE } else { vk::SharingMode::CONCURRENT };

        let info = vk::VideoSessionCreateInfoKHR::default()
            .queue_family_index(ctx.decode_family)
            .video_profile(profile)
            .picture_format(PICTURE_FORMAT)
            .max_coded_extent(extent)
            .reference_picture_format(PICTURE_FORMAT)
            .max_dpb_slots(slots)
            .max_active_reference_pictures((slots - 1).min(self.caps.max_active_references))
            .std_header_version(&self.caps.std_header);
        unsafe {
            (ctx.video_fns.fp().create_video_session_khr)(raw.handle(), &info, ptr::null(), &mut self.session).result()?;
            let mut count = 0;
            (ctx.video_fns.fp().get_video_session_memory_requirements_khr)(raw.handle(), self.session, &mut count, ptr::null_mut()).result()?;
            let mut reqs = vec![vk::VideoSessionMemoryRequirementsKHR::default(); count as usize];
            (ctx.video_fns.fp().get_video_session_memory_requirements_khr)(raw.handle(), self.session, &mut count, reqs.as_mut_ptr()).result()?;
            let mut binds = Vec::with_capacity(reqs.len());
            for r in &reqs {
                let memory = ctx.allocate(r.memory_requirements, vk::MemoryPropertyFlags::DEVICE_LOCAL)?;
                self.session_memory.push(memory);
                binds.push(
                    vk::BindVideoSessionMemoryInfoKHR::default()
                        .memory_bind_index(r.memory_bind_index)
                        .memory(memory)
                        .memory_offset(0)
                        .memory_size(r.memory_requirements.size),
                );
            }
            (ctx.video_fns.fp().bind_video_session_memory_khr)(raw.handle(), self.session, binds.len() as u32, binds.as_ptr()).result()?;
        }
        self.create_parameters(job)?;

        // The DPB (and output) images; copied out by the graphics queue, so TRANSFER_SRC too.
        let coincide = self.caps.coincide;
        let mut dpb_usage = vk::ImageUsageFlags::VIDEO_DECODE_DPB_KHR;
        if coincide {
            dpb_usage |= vk::ImageUsageFlags::VIDEO_DECODE_DST_KHR | vk::ImageUsageFlags::TRANSFER_SRC;
        }
        let image = |usage, layers, list: &mut vk::VideoProfileListInfoKHR<'_>| -> Result<(vk::Image, vk::DeviceMemory, vk::ImageView)> {
            let info = vk::ImageCreateInfo::default()
                .image_type(vk::ImageType::TYPE_2D)
                .format(PICTURE_FORMAT)
                .extent(vk::Extent3D { width: w, height: h, depth: 1 })
                .mip_levels(1)
                .array_layers(layers)
                .samples(vk::SampleCountFlags::TYPE_1)
                .tiling(vk::ImageTiling::OPTIMAL)
                .usage(usage)
                .sharing_mode(sharing)
                .queue_family_indices(&families)
                .initial_layout(vk::ImageLayout::UNDEFINED)
                .push_next(list);
            unsafe {
                let img = raw.create_image(&info, None)?;
                let memory = match ctx.allocate(raw.get_image_memory_requirements(img), vk::MemoryPropertyFlags::DEVICE_LOCAL) {
                    Ok(m) => m,
                    Err(e) => {
                        raw.destroy_image(img, None);
                        return Err(e);
                    }
                };
                raw.bind_image_memory(img, memory, 0)?;
                let view = raw.create_image_view(
                    &vk::ImageViewCreateInfo::default()
                        .image(img)
                        .view_type(vk::ImageViewType::TYPE_2D_ARRAY)
                        .format(PICTURE_FORMAT)
                        .subresource_range(color_range(0, layers)),
                    None,
                )?;
                Ok((img, memory, view))
            }
        };
        (self.dpb, self.dpb_memory, self.dpb_view) = image(dpb_usage, slots, &mut profile_list)?;
        if !coincide {
            let usage = vk::ImageUsageFlags::VIDEO_DECODE_DST_KHR | vk::ImageUsageFlags::TRANSFER_SRC;
            self.output = Some(image(usage, 1, &mut profile_list)?);
        }
        self.grow_bitstream(job.bitstream.len() as u64, &mut profile_list)
    }

    fn create_parameters(&mut self, job: &DecodeJob) -> Result<()> {
        let ctx = self.ctx.clone();
        let raw = &ctx.device;
        let sps = [std_sps(&job.sps)];
        let pps = [std_pps(&job.pps)];
        let add = vk::VideoDecodeH264SessionParametersAddInfoKHR::default().std_sp_ss(&sps).std_pp_ss(&pps);
        let mut h264 = vk::VideoDecodeH264SessionParametersCreateInfoKHR::default().max_std_sps_count(1).max_std_pps_count(1).parameters_add_info(&add);
        let info = vk::VideoSessionParametersCreateInfoKHR::default().video_session(self.session).push_next(&mut h264);
        let mut params = vk::VideoSessionParametersKHR::null();
        unsafe {
            (ctx.video_fns.fp().create_video_session_parameters_khr)(raw.handle(), &info, ptr::null(), &mut params).result()?;
            if self.parameters != vk::VideoSessionParametersKHR::null() {
                // The previous decode, the only user, was waited for.
                (ctx.video_fns.fp().destroy_video_session_parameters_khr)(raw.handle(), self.parameters, ptr::null());
            }
        }
        self.parameters = params;
        Ok(())
    }

    fn grow_bitstream(&mut self, needed: u64, list: &mut vk::VideoProfileListInfoKHR<'_>) -> Result<()> {
        let size = (needed.max(1 << 20)).next_multiple_of(self.caps.size_alignment);
        if size <= self.bitstream_size {
            return Ok(());
        }
        let raw = &self.ctx.device;
        unsafe {
            if self.bitstream != vk::Buffer::null() {
                raw.unmap_memory(self.bitstream_memory);
                raw.destroy_buffer(self.bitstream, None);
                raw.free_memory(self.bitstream_memory, None);
                self.bitstream = vk::Buffer::null();
            }
            let buffer = raw.create_buffer(
                &vk::BufferCreateInfo::default().size(size).usage(vk::BufferUsageFlags::VIDEO_DECODE_SRC_KHR).push_next(list),
                None,
            )?;
            let memory = match self.ctx.allocate(
                raw.get_buffer_memory_requirements(buffer),
                vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
            ) {
                Ok(m) => m,
                Err(e) => {
                    raw.destroy_buffer(buffer, None);
                    return Err(e);
                }
            };
            raw.bind_buffer_memory(buffer, memory, 0)?;
            self.bitstream_ptr = raw.map_memory(memory, 0, size, vk::MemoryMapFlags::empty())?.cast();
            (self.bitstream, self.bitstream_memory, self.bitstream_size) = (buffer, memory, size);
        }
        Ok(())
    }

    /// True if `job` can be decoded by this decoder (same profile, size and DPB).
    pub fn fits(&self, job: &DecodeJob) -> bool {
        Shape::of(job) == self.shape
    }

    pub fn bind_group(&self) -> &wgpu::BindGroup {
        &self.bind_group
    }

    pub fn size(&self) -> (u32, u32) {
        self.shape.display
    }

    /// Decodes `job` and copies the picture into the sampled planes (on wgpu's queue, ahead of
    /// the next draw).
    pub fn decode(&mut self, job: &DecodeJob) -> Result<()> {
        ensure!(self.fits(job), "job does not fit this decoder");
        let ctx = self.ctx.clone();
        let raw = &ctx.device;
        if job.new_parameters {
            self.create_parameters(job)?;
        }
        let len = job.bitstream.len() as u64;
        if len > self.bitstream_size {
            let mut h264 = profile_h264(job.sps.profile_idc as u32);
            let p = [profile(&mut h264)];
            let mut list = vk::VideoProfileListInfoKHR::default().profiles(&p);
            self.grow_bitstream(len, &mut list)?;
        }
        // SAFETY: the mapping holds `bitstream_size` bytes and the last decode was waited for.
        unsafe {
            ptr::copy_nonoverlapping(job.bitstream.as_ptr(), self.bitstream_ptr, job.bitstream.len());
            ptr::write_bytes(self.bitstream_ptr.add(job.bitstream.len()), 0, (self.bitstream_size - len) as usize);
        }
        let range = len.next_multiple_of(self.caps.size_alignment).min(self.bitstream_size);

        let resource = |slot: u8, view| {
            vk::VideoPictureResourceInfoKHR::default()
                .coded_extent(vk::Extent2D { width: self.shape.coded.0, height: self.shape.coded.1 })
                .base_array_layer(slot as u32)
                .image_view_binding(view)
        };
        let ref_resources: Vec<vk::VideoPictureResourceInfoKHR> = job.references.iter().map(|r| resource(r.slot, self.dpb_view)).collect();
        let ref_std: Vec<std_vk::StdVideoDecodeH264ReferenceInfo> = job.references.iter().map(|r| std_reference(r.frame_num, r.long_term, r.poc)).collect();
        let mut ref_dpb: Vec<vk::VideoDecodeH264DpbSlotInfoKHR> = ref_std.iter().map(|s| vk::VideoDecodeH264DpbSlotInfoKHR::default().std_reference_info(s)).collect();
        let references: Vec<vk::VideoReferenceSlotInfoKHR> = ref_dpb
            .iter_mut()
            .zip(&ref_resources)
            .zip(&job.references)
            .map(|((dpb, res), r)| vk::VideoReferenceSlotInfoKHR::default().slot_index(r.slot as i32).picture_resource(res).push_next(dpb))
            .collect();

        let setup_resource = resource(job.slot, self.dpb_view);
        let setup_std = std_reference(job.frame_num, false, job.poc);
        let mut setup_dpb = vk::VideoDecodeH264DpbSlotInfoKHR::default().std_reference_info(&setup_std);
        let setup = vk::VideoReferenceSlotInfoKHR::default().slot_index(job.slot as i32).picture_resource(&setup_resource).push_next(&mut setup_dpb);
        // Begin lists the active references plus the setup slot (not yet active: index -1).
        let setup_begin = vk::VideoReferenceSlotInfoKHR::default().slot_index(-1).picture_resource(&setup_resource);
        let mut begin_slots: Vec<vk::VideoReferenceSlotInfoKHR> = job
            .references
            .iter()
            .zip(&ref_resources)
            .map(|(r, res)| vk::VideoReferenceSlotInfoKHR::default().slot_index(r.slot as i32).picture_resource(res))
            .collect();
        begin_slots.push(setup_begin);

        let output_resource = match &self.output {
            Some((_, _, view)) => resource(0, *view),
            None => setup_resource,
        };
        let std_pic = std_picture(job);
        let mut h264_pic = vk::VideoDecodeH264PictureInfoKHR::default().std_picture_info(&std_pic).slice_offsets(&job.slice_offsets);
        let decode_info = vk::VideoDecodeInfoKHR::default()
            .src_buffer(self.bitstream)
            .src_buffer_offset(0)
            .src_buffer_range(range)
            .dst_picture_resource(output_resource)
            .setup_reference_slot(&setup)
            .reference_slots(&references)
            .push_next(&mut h264_pic);

        unsafe {
            raw.wait_for_fences(&[self.decode_fence], true, u64::MAX)?;
            raw.reset_fences(&[self.decode_fence])?;
            let cmd = self.decode_cmd;
            raw.reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty())?;
            raw.begin_command_buffer(cmd, &vk::CommandBufferBeginInfo::default().flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT))?;
            if self.fresh {
                let mut barriers = vec![
                    image_barrier(self.dpb, color_range(0, self.shape.slots as u32))
                        .old_layout(vk::ImageLayout::UNDEFINED)
                        .new_layout(vk::ImageLayout::VIDEO_DECODE_DPB_KHR)
                        .dst_access_mask(DECODE_ACCESS),
                ];
                if let Some((img, _, _)) = self.output {
                    barriers.push(
                        image_barrier(img, color_range(0, 1))
                            .old_layout(vk::ImageLayout::UNDEFINED)
                            .new_layout(vk::ImageLayout::VIDEO_DECODE_DST_KHR)
                            .dst_access_mask(DECODE_ACCESS),
                    );
                }
                raw.cmd_pipeline_barrier(cmd, vk::PipelineStageFlags::TOP_OF_PIPE, vk::PipelineStageFlags::ALL_COMMANDS, vk::DependencyFlags::empty(), &[], &[], &barriers);
            }
            let begin = vk::VideoBeginCodingInfoKHR::default().video_session(self.session).video_session_parameters(self.parameters).reference_slots(&begin_slots);
            (ctx.video_fns.fp().cmd_begin_video_coding_khr)(cmd, &begin);
            if self.fresh {
                let control = vk::VideoCodingControlInfoKHR::default().flags(vk::VideoCodingControlFlagsKHR::RESET);
                (ctx.video_fns.fp().cmd_control_video_coding_khr)(cmd, &control);
            }
            (ctx.decode_fns.fp().cmd_decode_video_khr)(cmd, &decode_info);
            (ctx.video_fns.fp().cmd_end_video_coding_khr)(cmd, &vk::VideoEndCodingInfoKHR::default());
            raw.end_command_buffer(cmd)?;
            let cmds = [cmd];
            let waits = [self.copied];
            let stages = [vk::PipelineStageFlags::ALL_COMMANDS];
            let decoded = [self.decoded];
            let mut submit = vk::SubmitInfo::default().command_buffers(&cmds).signal_semaphores(&decoded);
            if self.copy_pending {
                submit = submit.wait_semaphores(&waits).wait_dst_stage_mask(&stages);
            }
            raw.queue_submit(ctx.decode_queue, &[submit], self.decode_fence)?;
            self.copy_pending = false;
            self.fresh = false;
        }
        let (src, layer, layout) = match self.output {
            Some((img, _, _)) => (img, 0, vk::ImageLayout::VIDEO_DECODE_DST_KHR),
            None => (self.dpb, job.slot as u32, vk::ImageLayout::VIDEO_DECODE_DPB_KHR),
        };
        self.copy_out(src, layer, layout)
    }

    /// Copies the decoded picture's planes into the sampled textures on wgpu's queue.
    fn copy_out(&mut self, src: vk::Image, layer: u32, layout: vk::ImageLayout) -> Result<()> {
        let ctx = self.ctx.clone();
        let raw = &ctx.device;
        let (w, h) = self.shape.display;
        unsafe {
            raw.wait_for_fences(&[self.copy_fence], true, u64::MAX)?;
            raw.reset_fences(&[self.copy_fence])?;
            let cmd = self.copy_cmd;
            raw.reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty())?;
            raw.begin_command_buffer(cmd, &vk::CommandBufferBeginInfo::default().flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT))?;
            let planes_old = if self.planes_ready { vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL } else { vk::ImageLayout::UNDEFINED };
            let pre = [
                image_barrier(src, plane_range(layer))
                    .old_layout(layout)
                    .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                    .src_access_mask(vk::AccessFlags::empty())
                    .dst_access_mask(vk::AccessFlags::TRANSFER_READ),
                image_barrier(self.planes[0], color_range(0, 1))
                    .old_layout(planes_old)
                    .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                    .src_access_mask(vk::AccessFlags::SHADER_READ)
                    .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE),
                image_barrier(self.planes[1], color_range(0, 1))
                    .old_layout(planes_old)
                    .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                    .src_access_mask(vk::AccessFlags::SHADER_READ)
                    .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE),
            ];
            raw.cmd_pipeline_barrier(cmd, vk::PipelineStageFlags::FRAGMENT_SHADER | vk::PipelineStageFlags::TOP_OF_PIPE, vk::PipelineStageFlags::TRANSFER, vk::DependencyFlags::empty(), &[], &[], &pre);
            let copy = |aspect, dst, cw, ch| {
                let region = vk::ImageCopy::default()
                    .src_subresource(vk::ImageSubresourceLayers { aspect_mask: aspect, mip_level: 0, base_array_layer: layer, layer_count: 1 })
                    .dst_subresource(vk::ImageSubresourceLayers { aspect_mask: vk::ImageAspectFlags::COLOR, mip_level: 0, base_array_layer: 0, layer_count: 1 })
                    .extent(vk::Extent3D { width: cw, height: ch, depth: 1 });
                raw.cmd_copy_image(cmd, src, vk::ImageLayout::TRANSFER_SRC_OPTIMAL, dst, vk::ImageLayout::TRANSFER_DST_OPTIMAL, &[region]);
            };
            copy(vk::ImageAspectFlags::PLANE_0, self.planes[0], w, h);
            copy(vk::ImageAspectFlags::PLANE_1, self.planes[1], w.div_ceil(2), h.div_ceil(2));
            let post = [
                image_barrier(src, plane_range(layer))
                    .old_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                    .new_layout(layout)
                    .src_access_mask(vk::AccessFlags::TRANSFER_READ)
                    .dst_access_mask(vk::AccessFlags::empty()),
                image_barrier(self.planes[0], color_range(0, 1))
                    .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                    .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                    .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                    .dst_access_mask(vk::AccessFlags::SHADER_READ),
                image_barrier(self.planes[1], color_range(0, 1))
                    .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                    .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                    .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                    .dst_access_mask(vk::AccessFlags::SHADER_READ),
            ];
            raw.cmd_pipeline_barrier(cmd, vk::PipelineStageFlags::TRANSFER, vk::PipelineStageFlags::FRAGMENT_SHADER | vk::PipelineStageFlags::BOTTOM_OF_PIPE, vk::DependencyFlags::empty(), &[], &[], &post);
            raw.end_command_buffer(cmd)?;
            let cmds = [cmd];
            let signals = [self.copied];
            let waits = [self.decoded];
            // The layout transitions at the start of this submission also touch the decoded
            // image, so everything in it waits, not just the copies.
            let stages = [vk::PipelineStageFlags::ALL_COMMANDS];
            let submit = vk::SubmitInfo::default().command_buffers(&cmds).wait_semaphores(&waits).wait_dst_stage_mask(&stages).signal_semaphores(&signals);
            // wgpu's queue, from the render thread that also submits wgpu's work: the draw that
            // samples the planes is submitted after this and so runs after it.
            raw.queue_submit(ctx.gfx_queue, &[submit], self.copy_fence)?;
        }
        self.copy_pending = true;
        self.planes_ready = true;
        Ok(())
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        let ctx = self.ctx.clone();
        let raw = &ctx.device;
        unsafe {
            let _ = raw.wait_for_fences(&[self.decode_fence, self.copy_fence], true, u64::MAX);
            if self.parameters != vk::VideoSessionParametersKHR::null() {
                (ctx.video_fns.fp().destroy_video_session_parameters_khr)(raw.handle(), self.parameters, ptr::null());
            }
            if self.session != vk::VideoSessionKHR::null() {
                (ctx.video_fns.fp().destroy_video_session_khr)(raw.handle(), self.session, ptr::null());
            }
            for m in self.session_memory.drain(..) {
                raw.free_memory(m, None);
            }
            let images = [(self.dpb, self.dpb_memory, self.dpb_view)].into_iter().chain(self.output);
            for (img, mem, view) in images {
                if view != vk::ImageView::null() {
                    raw.destroy_image_view(view, None);
                }
                if img != vk::Image::null() {
                    raw.destroy_image(img, None);
                }
                if mem != vk::DeviceMemory::null() {
                    raw.free_memory(mem, None);
                }
            }
            if self.bitstream != vk::Buffer::null() {
                raw.unmap_memory(self.bitstream_memory);
                raw.destroy_buffer(self.bitstream, None);
                raw.free_memory(self.bitstream_memory, None);
            }
            raw.destroy_semaphore(self.copied, None);
            raw.destroy_semaphore(self.decoded, None);
            raw.destroy_fence(self.decode_fence, None);
            raw.destroy_fence(self.copy_fence, None);
            raw.destroy_command_pool(self.decode_pool, None);
            raw.destroy_command_pool(self.copy_pool, None);
        }
        // `textures` own the plane images; wgpu destroys them once its last use completed.
    }
}

fn color_range(layer: u32, count: u32) -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange { aspect_mask: vk::ImageAspectFlags::COLOR, base_mip_level: 0, level_count: 1, base_array_layer: layer, layer_count: count }
}

fn plane_range(layer: u32) -> vk::ImageSubresourceRange {
    color_range(layer, 1)
}

/// The decode's own reads and writes have no legacy access bits (only synchronization2 names
/// them), so the layout transitions before the first decode make all memory access wait.
const DECODE_ACCESS: vk::AccessFlags = vk::AccessFlags::from_raw(vk::AccessFlags::MEMORY_READ.as_raw() | vk::AccessFlags::MEMORY_WRITE.as_raw());

fn image_barrier(image: vk::Image, range: vk::ImageSubresourceRange) -> vk::ImageMemoryBarrier<'static> {
    vk::ImageMemoryBarrier::default()
        .image(image)
        .subresource_range(range)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
}

/// A device-local image the copy writes and the shader samples, owned by a wgpu texture
/// (which destroys it). wgpu sees it only in the RESOURCE state; `copy_out` keeps it there.
fn plane_texture(ctx: &Context, device: &wgpu::Device, format: vk::Format, wformat: wgpu::TextureFormat, width: u32, height: u32) -> Result<(vk::Image, wgpu::Texture)> {
    let raw = ctx.device.clone();
    let info = vk::ImageCreateInfo::default()
        .image_type(vk::ImageType::TYPE_2D)
        .format(format)
        .extent(vk::Extent3D { width, height, depth: 1 })
        .mip_levels(1)
        .array_layers(1)
        .samples(vk::SampleCountFlags::TYPE_1)
        .tiling(vk::ImageTiling::OPTIMAL)
        .usage(vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::SAMPLED)
        .sharing_mode(vk::SharingMode::EXCLUSIVE)
        .initial_layout(vk::ImageLayout::UNDEFINED);
    let image = unsafe { raw.create_image(&info, None) }?;
    let memory = match unsafe { ctx.allocate(raw.get_image_memory_requirements(image), vk::MemoryPropertyFlags::DEVICE_LOCAL) } {
        Ok(m) => m,
        Err(e) => {
            unsafe { raw.destroy_image(image, None) };
            return Err(e);
        }
    };
    if let Err(e) = unsafe { raw.bind_image_memory(image, memory, 0) } {
        unsafe {
            raw.destroy_image(image, None);
            raw.free_memory(memory, None);
        }
        bail!("binding plane memory: {e}");
    }
    let size = wgpu::Extent3d { width, height, depth_or_array_layers: 1 };
    let hal_desc = wgpu::hal::TextureDescriptor {
        label: Some("h264 plane"),
        size,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wformat,
        usage: wgpu::TextureUses::RESOURCE,
        memory_flags: wgpu::hal::MemoryFlags::empty(),
        view_formats: vec![],
    };
    let destroy_raw = raw.clone();
    let destroy = Box::new(move || unsafe {
        destroy_raw.destroy_image(image, None);
        destroy_raw.free_memory(memory, None);
    });
    let hal = unsafe { device.as_hal::<Vulkan>() }.ok_or_else(|| anyhow!("not a Vulkan device"))?;
    let hal_tex = unsafe { hal.texture_from_raw(image, &hal_desc, Some(destroy), hal_vk::TextureMemory::External) };
    drop(hal);
    let desc = wgpu::TextureDescriptor {
        label: Some("h264 plane"),
        size,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wformat,
        usage: wgpu::TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    };
    let texture = unsafe { device.create_texture_from_hal::<Vulkan>(hal_tex, &desc, wgpu::TextureUses::RESOURCE) };
    Ok((image, texture))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job() -> DecodeJob {
        let data = include_bytes!("../../h264/testdata/videotoolbox_1080p.h264");
        let end = data.windows(4).skip(4).position(|w| w == [0, 0, 1, 0x21] || w == [0, 0, 1, 0x01]).unwrap() + 4;
        umac_h264::Parser::new().push(&data[..end]).unwrap()
    }

    #[test]
    fn std_structs_mirror_the_parsed_headers() {
        let j = job();
        let s = std_sps(&j.sps);
        assert_eq!((s.profile_idc, s.pic_width_in_mbs_minus1, s.pic_height_in_map_units_minus1), (100, 119, 67));
        assert_eq!(s.frame_crop_bottom_offset, 4);
        assert_eq!((s.flags.frame_mbs_only_flag(), s.flags.frame_cropping_flag()), (1, 1));
        assert_eq!(s.max_num_ref_frames, 4);
        let p = std_pps(&j.pps);
        assert_eq!(p.num_ref_idx_l0_default_active_minus1, 0);
        let pic = std_picture(&j);
        assert_eq!((pic.flags.IdrPicFlag(), pic.flags.is_intra(), pic.flags.is_reference()), (1, 1, 1));
        let r = std_reference(2, true, [4, 4]);
        assert_eq!((r.FrameNum, r.flags.used_for_long_term_reference()), (2, 1));
        assert_eq!(level(40), std_vk::StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_4_0);
    }
}
