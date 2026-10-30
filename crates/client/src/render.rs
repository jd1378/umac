//! wgpu presentation: one surface per window, NV12 uploaded as R8 + RG8 textures and
//! converted to RGB in the fragment shader.

use std::{collections::HashMap, sync::Arc};

use anyhow::{Context, Result};
use winit::window::Window;

use crate::{decode::Nv12Frame, viewport::Viewport};

const SHADER: &str = r#"
struct VsOut { @builtin(position) pos: vec4<f32>, @location(0) uv: vec2<f32> };

@vertex
fn vs(@builtin(vertex_index) i: u32) -> VsOut {
    let p = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
    var o: VsOut;
    o.pos = vec4<f32>(p * 2.0 - 1.0, 0.0, 1.0);
    o.uv = vec2<f32>(p.x, 1.0 - p.y);
    return o;
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
    instance: wgpu::Instance,
    adapter: wgpu::Adapter,
    device: wgpu::Device,
    queue: wgpu::Queue,
    layout: wgpu::BindGroupLayout,
    pipeline_layout: wgpu::PipelineLayout,
    shader: wgpu::ShaderModule,
    sampler: wgpu::Sampler,
    pipelines: HashMap<wgpu::TextureFormat, wgpu::RenderPipeline>,
}

impl Gpu {
    /// Creates the device using the first window's surface to pick a compatible adapter.
    pub fn new(window: Arc<Window>) -> Result<(Self, wgpu::Surface<'static>)> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let surface = instance.create_surface(window).context("creating surface")?;
        let gpu = Self::with_instance(instance, Some(&surface))?;
        Ok((gpu, surface))
    }

    fn with_instance(instance: wgpu::Instance, surface: Option<&wgpu::Surface<'_>>) -> Result<Self> {
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: surface,
            ..Default::default()
        }))
        .context("no GPU adapter")?;
        tracing::info!(adapter = ?adapter.get_info().name, backend = ?adapter.get_info().backend, "GPU");
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))?;
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
        Ok(Self { instance, adapter, device, queue, layout, pipeline_layout, shader, sampler, pipelines: HashMap::new() })
    }

    pub fn create_surface(&self, window: Arc<Window>) -> Result<wgpu::Surface<'static>> {
        Ok(self.instance.create_surface(window)?)
    }

    fn draw(&mut self, target: &wgpu::TextureView, format: wgpu::TextureFormat, frame: Option<(&FrameTextures, Viewport)>) {
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
    y: wgpu::Texture,
    uv: wgpu::Texture,
    bind_group: wgpu::BindGroup,
    width: u32,
    height: u32,
}

/// Per-window presentation state.
pub struct WindowRenderer {
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    frame: Option<FrameTextures>,
}

impl WindowRenderer {
    pub fn new(gpu: &Gpu, surface: wgpu::Surface<'static>, width: u32, height: u32) -> Self {
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
        Self { surface, config, frame: None }
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
        Some(Viewport::fit(w, h, self.config.width, self.config.height))
    }

    pub fn upload(&mut self, gpu: &Gpu, f: &Nv12Frame) {
        if self.frame_size() != Some((f.width, f.height)) {
            self.frame = Some(FrameTextures::new(gpu, f.width, f.height));
        }
        self.frame.as_ref().unwrap().write(gpu, f);
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
        let t = self;
        let (cw, ch) = (f.width.div_ceil(2), f.height.div_ceil(2));
        let write = |tex: &wgpu::Texture, data: &[u8], stride: usize, w: u32, h: u32| {
            gpu.queue.write_texture(
                tex.as_image_copy(),
                data,
                wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(stride as u32), rows_per_image: Some(h) },
                wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            );
        };
        write(&t.y, &f.y, f.y_stride, f.width, f.height);
        write(&t.uv, &f.uv, f.uv_stride, cw, ch);
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
        let bind_group = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("nv12"),
            layout: &gpu.layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&y.create_view(&Default::default())),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&uv.create_view(&Default::default())),
                },
                wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::Sampler(&gpu.sampler) },
            ],
        });
        FrameTextures { y, uv, bind_group, width, height }
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
        let (w, h) = (64u32, 64u32);
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
        let textures = FrameTextures::new(gpu, f.width, f.height);
        textures.write(gpu, f);
        let view = target.create_view(&Default::default());
        gpu.draw(&view, format, Some((&textures, Viewport::fit(f.width, f.height, w, h))));
        let buf = gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: (256 * h) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut enc = gpu.device.create_command_encoder(&Default::default());
        enc.copy_texture_to_buffer(
            target.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &buf,
                layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(256), rows_per_image: Some(h) },
            },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );
        gpu.queue.submit([enc.finish()]);
        buf.slice(..).map_async(wgpu::MapMode::Read, |r| r.unwrap());
        gpu.device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
        let data = buf.slice(..).get_mapped_range().unwrap();
        // Frames draw 1:1 from the top-left, so sample inside the frame.
        let (x, y) = (f.width / 2, f.height / 2);
        let o = (y * 256 + x * 4) as usize;
        [data[o], data[o + 1], data[o + 2], data[o + 3]]
    }

    #[test]
    fn bt709_limited_conversion() {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let Ok(mut gpu) = Gpu::with_instance(instance, None) else {
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
