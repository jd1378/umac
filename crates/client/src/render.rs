//! wgpu presentation: one surface per window, NV12 as R8 + RG8 textures (uploaded from CPU
//! memory, or the decoder's own surface imported zero-copy) converted to RGB in the shader.

use std::{collections::HashMap, sync::Arc};

use anyhow::{Context, Result};

use crate::{
    decode::{self, Nv12Frame, Picture},
    viewport::{Fit, Viewport},
};

const SHADER: &str = r#"
struct VsOut { @builtin(position) pos: vec4<f32>, @location(0) uv: vec2<f32> };

@vertex
fn vs(@builtin(vertex_index) i: u32) -> VsOut {
    let p = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
    // Built in one expression: a local `var` of an IO struct makes naga emit Offset
    // decorations on a Function variable, which Vulkan validation rejects.
    return VsOut(vec4<f32>(p * 2.0 - 1.0, 0.0, 1.0), vec2<f32>(p.x, 1.0 - p.y));
}

@group(0) @binding(0) var y_tex: texture_2d<f32>;
@group(0) @binding(1) var uv_tex: texture_2d<f32>;
@group(0) @binding(2) var samp: sampler;

// BT.709, limited range.
fn yuv_to_rgb(uv: vec2<f32>) -> vec3<f32> {
    let y = (textureSample(y_tex, samp, uv).r - 16.0 / 255.0) * (255.0 / 219.0);
    let c = (textureSample(uv_tex, samp, uv).rg - 128.0 / 255.0) * (255.0 / 224.0);
    let rgb = vec3<f32>(
        y + 1.5748 * c.y,
        y - 0.1873 * c.x - 0.4681 * c.y,
        y + 1.8556 * c.x,
    );
    return clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0));
}

@fragment
fn fs_unorm(v: VsOut) -> @location(0) vec4<f32> {
    return vec4<f32>(yuv_to_rgb(v.uv), 1.0);
}

// For sRGB surfaces the hardware re-encodes on write, so hand it linear values.
@fragment
fn fs_srgb(v: VsOut) -> @location(0) vec4<f32> {
    let c = yuv_to_rgb(v.uv);
    let lin = select(pow((c + 0.055) / 1.055, vec3<f32>(2.4)), c / 12.92, c <= vec3<f32>(0.04045));
    return vec4<f32>(lin, 1.0);
}
"#;

/// GPU objects shared by every window.
pub struct Gpu {
    /// Declared first so it drops first: it destroys Vulkan objects on `device`.
    #[cfg(target_os = "linux")]
    importer: Option<crate::zerocopy::Importer>,
    adapter: wgpu::Adapter,
    device: wgpu::Device,
    queue: wgpu::Queue,
    layout: wgpu::BindGroupLayout,
    pipeline_layout: wgpu::PipelineLayout,
    shader: wgpu::ShaderModule,
    sampler: wgpu::Sampler,
    pipelines: HashMap<wgpu::TextureFormat, wgpu::RenderPipeline>,
    /// Consecutive zero-copy import or acquire failures.
    #[cfg(target_os = "linux")]
    zc_failures: u32,
}

impl Gpu {
    /// Bind group sampling a luma (R8) and CbCr (RG8) plane pair.
    fn bind_planes(&self, y: &wgpu::TextureView, cbcr: &wgpu::TextureView) -> wgpu::BindGroup {
        self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("nv12"),
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(y) },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(cbcr) },
                wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::Sampler(&self.sampler) },
            ],
        })
    }

    /// Creates the device, picking an adapter that can present to `surface` (the first window's).
    pub fn with_instance(instance: &wgpu::Instance, surface: Option<&wgpu::Surface<'_>>) -> Result<Self> {
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: surface,
            ..Default::default()
        }))
        .context("no GPU adapter")?;
        tracing::info!(adapter = ?adapter.get_info().name, backend = ?adapter.get_info().backend, "GPU");
        #[cfg(target_os = "linux")]
        let zero_copy = decode::zero_copy_allowed().then(|| crate::zerocopy::open_device(&adapter)).flatten();
        #[cfg(not(target_os = "linux"))]
        let zero_copy = None;
        let zero_copy_device = zero_copy.is_some();
        let (device, queue) = match zero_copy {
            Some(dq) => dq,
            None => pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))?,
        };
        #[cfg(target_os = "linux")]
        let importer = zero_copy_device
            .then(|| crate::zerocopy::Importer::new(&device).inspect_err(|e| tracing::warn!("zero-copy importer: {e:#}")).ok())
            .flatten();
        #[cfg(target_os = "linux")]
        let zero_copy_device = importer.is_some();
        decode::set_zero_copy(zero_copy_device);
        tracing::info!(path = if zero_copy_device { "zero-copy dmabuf" } else { "copy" }, "decoded frames");
        let tex_entry = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("nv12"),
            entries: &[
                tex_entry(0),
                tex_entry(1),
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("nv12"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("nv12"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        Ok(Self {
            adapter,
            device,
            queue,
            layout,
            pipeline_layout,
            shader,
            sampler,
            pipelines: HashMap::new(),
            #[cfg(target_os = "linux")]
            importer,
            #[cfg(target_os = "linux")]
            zc_failures: 0,
        })
    }

    /// Drops the zero-copy state of a closed window.
    pub fn forget(&mut self, owner: u64) {
        #[cfg(target_os = "linux")]
        if let Some(i) = self.importer.as_mut() {
            i.forget(owner);
        }
        #[cfg(not(target_os = "linux"))]
        let _ = owner;
    }

    /// Counts a zero-copy failure; too many in a row switch every decoder to copying.
    #[cfg(target_os = "linux")]
    fn zero_copy_failed(&mut self, what: &str, e: &anyhow::Error) {
        self.zc_failures += 1;
        tracing::warn!(failures = self.zc_failures, "zero-copy {what} failed ({e:#}); copying this frame");
        if self.zc_failures >= decode::MAX_ZERO_COPY_FAILURES {
            tracing::warn!("zero-copy keeps failing; copying frames from now on");
            decode::set_zero_copy(false);
        }
    }

    /// Draws `frame` into `target`. An imported frame is acquired from the decoder before the
    /// submission and released back after it; if the acquire fails nothing samples it.
    fn draw(&mut self, target: &wgpu::TextureView, format: wgpu::TextureFormat, frame: Option<(&FrameTextures, Viewport)>) {
        #[cfg(target_os = "linux")]
        let imported = match frame {
            Some((FrameTextures { source: Source::Imported { frame: f, images }, .. }, _)) => {
                match self.importer.as_mut().map(|i| i.acquire(*images)) {
                    Some(Ok(())) => Some((f.clone(), *images)),
                    Some(Err(e)) => {
                        self.zero_copy_failed("acquire", &e);
                        return self.draw(target, format, None);
                    }
                    None => return self.draw(target, format, None),
                }
            }
            _ => None,
        };
        let pipeline = self.pipeline(format).clone();
        let mut enc = self.device.create_command_encoder(&Default::default());
        {
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("present"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            if let Some((frame, v)) = frame {
                pass.set_pipeline(&pipeline);
                pass.set_bind_group(0, &frame.bind_group, &[]);
                pass.set_viewport(v.x, v.y, v.w, v.h, 0.0, 1.0);
                pass.draw(0..3, 0..1);
            }
        }
        self.queue.submit([enc.finish()]);
        #[cfg(target_os = "linux")]
        if let (Some((f, images)), Some(importer)) = (imported, self.importer.as_mut())
            && let Err(e) = importer.release(images, f)
        {
            tracing::warn!("zero-copy release failed: {e:#}");
        }
    }

    fn pipeline(&mut self, format: wgpu::TextureFormat) -> &wgpu::RenderPipeline {
        let Self { device, pipeline_layout, shader, .. } = self;
        self.pipelines.entry(format).or_insert_with(|| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("nv12"),
                layout: Some(pipeline_layout),
                vertex: wgpu::VertexState {
                    module: shader,
                    entry_point: Some("vs"),
                    buffers: &[],
                    compilation_options: Default::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module: shader,
                    entry_point: Some(if format.is_srgb() { "fs_srgb" } else { "fs_unorm" }),
                    targets: &[Some(format.into())],
                    compilation_options: Default::default(),
                }),
                primitive: Default::default(),
                depth_stencil: None,
                multisample: Default::default(),
                multiview_mask: None,
                cache: None,
            })
        })
    }
}

struct FrameTextures {
    source: Source,
    bind_group: wgpu::BindGroup,
    width: u32,
    height: u32,
}

enum Source {
    Upload { y: wgpu::Texture, uv: wgpu::Texture },
    /// The window's PyroWave decoder output planes (owned by `WindowRenderer::pyro`).
    Pyro,
    /// Sampled in place; each draw hands the frame to the importer until the GPU is done.
    #[cfg(target_os = "linux")]
    Imported { frame: Arc<decode::dmabuf::DmabufFrame>, images: [ash::vk::Image; 2] },
}

/// Per-window presentation state.
pub struct WindowRenderer {
    /// Window id, so the importer can tell this window's decoder surfaces apart.
    owner: u64,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    frame: Option<FrameTextures>,
    fit: Fit,
    /// PyroWave decoder for this window and the bind group sampling its output planes.
    pyro: Option<(umac_pyrowave_decode::GpuDecoder, wgpu::BindGroup)>,
}

impl WindowRenderer {
    pub fn new(gpu: &Gpu, owner: u64, surface: wgpu::Surface<'static>, width: u32, height: u32) -> Self {
        let caps = surface.get_capabilities(&gpu.adapter);
        let format = caps.formats.iter().copied().find(|f| !f.is_srgb()).unwrap_or(caps.formats[0]);
        let present_mode = [wgpu::PresentMode::Mailbox, wgpu::PresentMode::Immediate]
            .into_iter()
            .find(|m| caps.present_modes.contains(m))
            .unwrap_or(wgpu::PresentMode::Fifo);
        tracing::debug!(?format, ?present_mode, "surface");
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: width.max(1),
            height: height.max(1),
            present_mode,
            desired_maximum_frame_latency: 1,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
            color_space: Default::default(),
        };
        surface.configure(&gpu.device, &config);
        Self { owner, surface, config, frame: None, fit: Fit::Exact, pyro: None }
    }

    pub fn set_fit(&mut self, fit: Fit) {
        self.fit = fit;
    }

    pub fn resize(&mut self, gpu: &Gpu, width: u32, height: u32) {
        if width == 0 || height == 0 || (width, height) == (self.config.width, self.config.height) {
            return;
        }
        self.config.width = width;
        self.config.height = height;
        self.surface.configure(&gpu.device, &self.config);
    }

    /// Size of the last uploaded frame.
    pub fn frame_size(&self) -> Option<(u32, u32)> {
        self.frame.as_ref().map(|f| (f.width, f.height))
    }

    pub fn viewport(&self) -> Option<Viewport> {
        let (w, h) = self.frame_size()?;
        Some(Viewport::new(self.fit, w, h, self.config.width, self.config.height))
    }

    pub fn upload(&mut self, gpu: &mut Gpu, picture: Picture) {
        match picture {
            Picture::Pyro(p) => {
                if self.pyro.as_ref().is_none_or(|(d, _)| *d.layout() != p.layout) {
                    match umac_pyrowave_decode::GpuDecoder::new(&gpu.device, &gpu.queue, p.layout.clone()) {
                        Ok(d) => {
                            let (y, c) = d.views();
                            let bind_group = gpu.bind_planes(y, c);
                            self.pyro = Some((d, bind_group));
                        }
                        Err(e) => {
                            tracing::warn!("PyroWave decoder: {e}");
                            return;
                        }
                    }
                }
                let (decoder, bind_group) = self.pyro.as_mut().unwrap();
                // Same queue as the draw that follows, so the decode is ordered before it.
                decoder.decode(&gpu.device, &gpu.queue, &p.frame);
                let (width, height) = (p.layout.width, p.layout.height);
                self.frame = Some(FrameTextures { source: Source::Pyro, bind_group: bind_group.clone(), width, height });
            }
            Picture::Cpu(f) => {
                let reusable = matches!(&self.frame, Some(t) if matches!(t.source, Source::Upload { .. }) && (t.width, t.height) == (f.width, f.height));
                if !reusable {
                    self.frame = Some(FrameTextures::new(gpu, f.width, f.height));
                }
                self.frame.as_ref().unwrap().write(gpu, &f);
            }
            #[cfg(target_os = "linux")]
            Picture::Dmabuf(f) => {
                let Gpu { device, layout, sampler, importer, .. } = gpu;
                let res = match importer.as_mut() {
                    Some(i) => i.import(device, layout, sampler, self.owner, &f),
                    None => Err(anyhow::anyhow!("no importer")),
                };
                match res {
                    Ok(imported) => {
                        gpu.zc_failures = 0;
                        let (width, height) = (f.width, f.height);
                        let source = Source::Imported { frame: Arc::new(f), images: imported.images };
                        self.frame = Some(FrameTextures { source, bind_group: imported.bind_group, width, height });
                    }
                    Err(e) => {
                        gpu.zero_copy_failed("import", &e);
                        match f.download() {
                            Ok(cpu) => self.upload(gpu, Picture::Cpu(cpu)),
                            Err(e) => tracing::warn!("copying the frame failed too: {e:#}"),
                        }
                    }
                }
            }
        }
    }

    /// Draws the current frame (black until the first one arrives) and presents.
    pub fn render(&mut self, gpu: &mut Gpu) {
        let texture = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t) | wgpu::CurrentSurfaceTexture::Suboptimal(t) => t,
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                self.surface.configure(&gpu.device, &self.config);
                return;
            }
            other => {
                tracing::debug!(status = ?std::mem::discriminant(&other), "surface not ready");
                return;
            }
        };
        let view = texture.texture.create_view(&Default::default());
        let frame = self.frame.as_ref().zip(self.viewport());
        gpu.draw(&view, self.config.format, frame);
        gpu.queue.present(texture);
    }
}

impl FrameTextures {
    fn write(&self, gpu: &Gpu, f: &Nv12Frame) {
        let Source::Upload { y, uv } = &self.source else { return };
        let (cw, ch) = (f.width.div_ceil(2), f.height.div_ceil(2));
        let write = |tex: &wgpu::Texture, data: &[u8], stride: usize, w: u32, h: u32| {
            gpu.queue.write_texture(
                tex.as_image_copy(),
                data,
                wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(stride as u32), rows_per_image: Some(h) },
                wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            );
        };
        write(y, &f.y, f.y_stride, f.width, f.height);
        write(uv, &f.uv, f.uv_stride, cw, ch);
    }

    fn new(gpu: &Gpu, width: u32, height: u32) -> Self {
        let tex = |label, format, w, h| {
            gpu.device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            })
        };
        let y = tex("y", wgpu::TextureFormat::R8Unorm, width, height);
        let uv = tex("uv", wgpu::TextureFormat::Rg8Unorm, width.div_ceil(2), height.div_ceil(2));
        let bind_group = gpu.bind_planes(&y.create_view(&Default::default()), &uv.create_view(&Default::default()));
        FrameTextures { source: Source::Upload { y, uv }, bind_group, width, height }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(w: u32, h: u32, y: u8, u: u8, v: u8) -> Nv12Frame {
        let (cw, ch) = (w.div_ceil(2) as usize, h.div_ceil(2) as usize);
        Nv12Frame {
            width: w,
            height: h,
            y: vec![y; (w * h) as usize],
            y_stride: w as usize,
            uv: [u, v].repeat(cw * ch),
            uv_stride: cw * 2,
        }
    }

    /// Renders an NV12 frame offscreen and returns the center pixel.
    fn render_pixel(gpu: &mut Gpu, f: &Nv12Frame) -> [u8; 4] {
        let textures = FrameTextures::new(gpu, f.width, f.height);
        textures.write(gpu, f);
        let data = render_rgba(gpu, &textures, 64, 64);
        // Frames draw 1:1 from the top-left, so sample inside the frame.
        let (x, y) = (f.width as usize / 2, f.height as usize / 2);
        let o = (y * 64 + x) * 4;
        [data[o], data[o + 1], data[o + 2], data[o + 3]]
    }

    /// Draws `textures` into a `w`x`h` RGBA target and reads it back, tightly packed.
    fn render_rgba(gpu: &mut Gpu, textures: &FrameTextures, w: u32, h: u32) -> Vec<u8> {
        let row = (w * 4).next_multiple_of(256);
        let format = wgpu::TextureFormat::Rgba8Unorm;
        let target = gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: None,
            size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = target.create_view(&Default::default());
        gpu.draw(&view, format, Some((textures, Viewport::new(Fit::Exact, textures.width, textures.height, w, h))));
        let buf = gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: (row * h) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut enc = gpu.device.create_command_encoder(&Default::default());
        enc.copy_texture_to_buffer(
            target.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &buf,
                layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(row), rows_per_image: Some(h) },
            },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );
        gpu.queue.submit([enc.finish()]);
        buf.slice(..).map_async(wgpu::MapMode::Read, |r| r.unwrap());
        gpu.device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
        let data = buf.slice(..).get_mapped_range().unwrap();
        data.chunks(row as usize).flat_map(|r| r[..(w * 4) as usize].to_vec()).collect()
    }

    /// The zero-copy path must show exactly what the copy path shows.
    #[cfg(target_os = "linux")]
    #[test]
    fn dmabuf_import_matches_copy() {
        use umac_proto::Codec;
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let Ok(mut gpu) = Gpu::with_instance(&instance, None) else { return eprintln!("no GPU, skipping") };
        if gpu.importer.is_none() {
            return eprintln!("no dmabuf import support, skipping");
        }
        let path = std::env::temp_dir().join(format!("umac-zc-test-{}.ts", std::process::id()));
        crate::sample::generate_hevc(&path, 320, 180, 1).unwrap();
        let packets = crate::sample::read_packets(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        let (Ok(mut copy), Ok(mut zc)) = (decode::create_with_export(Codec::Hevc, false), decode::create_with_export(Codec::Hevc, true))
        else {
            return eprintln!("no decoder, skipping");
        };
        if !zc.describe().contains("vaapi") {
            return eprintln!("no VA-API, skipping");
        }
        // Every frame of the clip: the decoder cycles through its whole surface pool, so cached
        // images are re-acquired many times. Each frame is drawn twice, as a redraw would.
        assert!(packets.len() >= 50);
        for (i, p) in packets.iter().enumerate() {
            let Some(Picture::Cpu(cpu)) = copy.decode(&p.data).unwrap() else { panic!("copy path gave no CPU picture") };
            let Some(Picture::Dmabuf(f)) = zc.decode(&p.data).unwrap() else { panic!("zero-copy path gave no dmabuf") };
            let a = FrameTextures::new(&gpu, cpu.width, cpu.height);
            a.write(&gpu, &cpu);
            let Gpu { device, layout, sampler, importer, .. } = &mut gpu;
            let imported = importer.as_mut().unwrap().import(device, layout, sampler, 1, &f).unwrap();
            let source = Source::Imported { frame: Arc::new(f), images: imported.images };
            let b = FrameTextures { source, bind_group: imported.bind_group, width: cpu.width, height: cpu.height };
            let ra = render_rgba(&mut gpu, &a, 320, 180);
            for _ in 0..2 {
                let rb = render_rgba(&mut gpu, &b, 320, 180);
                let worst = ra.iter().zip(&rb).map(|(x, y)| x.abs_diff(*y)).max().unwrap();
                assert!(worst <= 1, "frame {i}: zero-copy differs from copy by up to {worst}");
            }
        }
        assert_eq!(gpu.zc_failures, 0);
        gpu.forget(1);
    }

    #[test]
    fn bt709_limited_conversion() {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let Ok(mut gpu) = Gpu::with_instance(&instance, None) else {
            eprintln!("no GPU, skipping");
            return;
        };
        let close = |a: [u8; 4], b: [u8; 3]| a.iter().zip(b).all(|(x, y)| x.abs_diff(y) <= 3);
        let cases = [
            ((235, 128, 128), [255, 255, 255]),
            ((16, 128, 128), [0, 0, 0]),
            ((126, 128, 128), [128, 128, 128]),
            // BT.709 limited red, green, blue.
            ((63, 102, 240), [255, 0, 0]),
            ((173, 42, 26), [0, 255, 0]),
            ((32, 240, 118), [0, 0, 255]),
        ];
        for ((y, u, v), rgb) in cases {
            let px = render_pixel(&mut gpu, &solid(16, 16, y, u, v));
            assert!(close(px, rgb), "yuv {y},{u},{v} gave {px:?}, want {rgb:?}");
        }
    }
}
