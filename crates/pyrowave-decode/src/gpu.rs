//! wgpu decoder: dequantization, five inverse wavelet levels per component and conversion to
//! the R8 luma and RG8 CbCr textures the renderer samples. Everything stays on the GPU.

use std::num::NonZeroU64;

use crate::{
    bitstream::Frame,
    layout::{COMPONENTS, Chroma, LEVELS, Layout},
};

/// Uniform slot per dispatch (min uniform buffer offset alignment is at most 256).
const SLOT: u64 = 256;
/// Largest workgroup count per dispatch dimension.
const MAX_GROUPS: u32 = 65535;

enum Pass {
    Dequant { x: u32, y: u32 },
    Idwt { x: u32, y: u32 },
    Pack { words: u32, rows: u32 },
}

struct Plane {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    /// Start word and row pitch (bytes) of this plane in the texel buffer.
    start_bytes: u64,
    pitch: u32,
    size: (u32, u32),
}

pub struct GpuDecoder {
    layout: Layout,
    bind_layout: wgpu::BindGroupLayout,
    dequant: wgpu::ComputePipeline,
    idwt: wgpu::ComputePipeline,
    pack: wgpu::ComputePipeline,
    coef: wgpu::Buffer,
    params: wgpu::Buffer,
    payload: wgpu::Buffer,
    payload_capacity: u64,
    offsets: wgpu::Buffer,
    blocks: wgpu::Buffer,
    texels: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    passes: Vec<Pass>,
    luma: Plane,
    chroma: Plane,
}

#[derive(Debug, thiserror::Error)]
pub enum GpuError {
    #[error("the GPU limits are too small for a {0}x{1} stream")]
    Limits(u32, u32),
}

fn storage(binding: u32, read_only: bool) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::COMPUTE,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Storage { read_only },
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    }
}

fn buffer(device: &wgpu::Device, label: &str, size: u64, usage: wgpu::BufferUsages) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor { label: Some(label), size: size.max(16), usage, mapped_at_creation: false })
}

fn words(v: &[u32]) -> Vec<u8> {
    v.iter().flat_map(|w| w.to_le_bytes()).collect()
}

impl GpuDecoder {
    /// Whether this device can run the decoder (compute shaders and enough storage buffers).
    pub fn supported(device: &wgpu::Device) -> bool {
        device.limits().max_storage_buffers_per_shader_stage >= 6
    }

    pub fn new(device: &wgpu::Device, queue: &wgpu::Queue, layout: Layout) -> Result<Self, GpuError> {
        let limits = device.limits();
        let plane_floats = |c: usize| {
            let lowest = layout.lowest_level(c);
            (layout.aligned_width >> lowest) as u64 * (layout.aligned_height >> lowest) as u64
        };
        let finals: Vec<u64> = (0..COMPONENTS).map(plane_floats).collect();
        let coef_len = layout.coef_len as u64 + finals.iter().sum::<u64>();
        if coef_len * 4 > limits.max_storage_buffer_binding_size {
            return Err(GpuError::Limits(layout.width, layout.height));
        }
        let entries = [
            storage(0, false),
            wgpu::BindGroupLayoutEntry {
                binding: 2,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: true,
                    min_binding_size: NonZeroU64::new(32),
                },
                count: None,
            },
            storage(3, true),
            storage(4, true),
            storage(5, true),
            storage(6, false),
        ];
        let bind_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor { label: Some("pyrowave"), entries: &entries });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("pyrowave"),
            bind_group_layouts: &[Some(&bind_layout)],
            immediate_size: 0,
        });
        let pipeline = |label: &str, source: &str, entry: &str| {
            let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some(label),
                source: wgpu::ShaderSource::Wgsl(source.into()),
            });
            device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some(label),
                layout: Some(&pipeline_layout),
                module: &module,
                entry_point: Some(entry),
                compilation_options: Default::default(),
                cache: None,
            })
        };
        let dequant = pipeline("pyrowave dequant", include_str!("shaders/dequant.wgsl"), "main");
        let idwt = pipeline("pyrowave idwt", include_str!("shaders/idwt.wgsl"), "main");
        let pack = pipeline("pyrowave pack", include_str!("shaders/pack.wgsl"), "main");

        let rw = wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST;
        let coef = buffer(device, "pyrowave coef", coef_len * 4, wgpu::BufferUsages::STORAGE);
        let offsets = buffer(device, "pyrowave offsets", layout.block_count as u64 * 4, rw);
        let payload_capacity = 64 * 1024;
        let payload = buffer(device, "pyrowave payload", payload_capacity, rw);

        // Per block: where its band lives and which 32x32 tile it is.
        let mut block_info = vec![0u32; layout.block_count as usize * 4];
        for band in &layout.bands {
            for by in 0..band.blocks_y {
                for bx in 0..band.blocks_x {
                    let i = (band.first_block + by * band.blocks_x + bx) as usize * 4;
                    block_info[i..i + 4].copy_from_slice(&[band.coef_offset, band.width, band.height, bx | (by << 16)]);
                }
            }
        }
        let blocks = buffer(device, "pyrowave blocks", block_info.len() as u64 * 4, rw);
        queue.write_buffer(&blocks, 0, &words(&block_info));

        // Final planes sit after the bands in `coef`.
        let mut final_offset = [0u32; COMPONENTS];
        let mut at = layout.coef_len;
        for (offset, len) in final_offset.iter_mut().zip(&finals) {
            *offset = at;
            at += *len as u32;
        }

        let texture = |label: &str, size: (u32, u32), format: wgpu::TextureFormat| {
            let texture = device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d { width: size.0, height: size.1, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            });
            let view = texture.create_view(&Default::default());
            (texture, view)
        };
        let pitch = |bytes: u32| bytes.next_multiple_of(256);
        let luma_size = layout.plane_size(0);
        let chroma_size = layout.plane_size(1);
        let (lt, lv) = texture("pyrowave y", luma_size, wgpu::TextureFormat::R8Unorm);
        let (ct, cv) = texture("pyrowave cbcr", chroma_size, wgpu::TextureFormat::Rg8Unorm);
        let luma = Plane { texture: lt, view: lv, start_bytes: 0, pitch: pitch(luma_size.0), size: luma_size };
        let luma_bytes = luma.pitch as u64 * luma_size.1 as u64;
        let chroma = Plane { texture: ct, view: cv, start_bytes: luma_bytes, pitch: pitch(chroma_size.0 * 2), size: chroma_size };
        let texel_bytes = luma_bytes + chroma.pitch as u64 * chroma_size.1 as u64;
        let texels = buffer(device, "pyrowave texels", texel_bytes, wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC);

        // Every dispatch with its parameters, fixed for this layout.
        let mut passes = Vec::new();
        let mut params: Vec<[u32; 8]> = Vec::new();
        let gx = layout.block_count.clamp(1, MAX_GROUPS);
        passes.push(Pass::Dequant { x: gx, y: layout.block_count.div_ceil(gx) });
        params.push([layout.block_count, gx, 0, 0, 0, 0, 0, 0]);
        for (c, &final_dst) in final_offset.iter().enumerate() {
            let lowest = layout.lowest_level(c);
            for level in (lowest..LEVELS).rev() {
                let band = |b| layout.band(c, level, b).expect("band in layout");
                let ll = band(0);
                let (w, h) = (ll.width, ll.height);
                let dst = if level == lowest { final_dst } else { layout.band(c, level - 1, 0).expect("derived LL").coef_offset };
                let p = [ll.coef_offset, band(1).coef_offset, band(2).coef_offset, band(3).coef_offset, w, h, dst, 0];
                passes.push(Pass::Idwt { x: (2 * w).div_ceil(32), y: (2 * h).div_ceil(32) });
                params.push(p);
            }
        }
        let stride = |c: usize| layout.aligned_width >> layout.lowest_level(c);
        let luma_words = luma.pitch / 4;
        passes.push(Pass::Pack { words: luma_words, rows: luma_size.1 });
        params.push([final_offset[0], 0, stride(0), luma_size.0, luma_size.1, luma_words, 0, 0]);
        let chroma_words = chroma.pitch / 4;
        passes.push(Pass::Pack { words: chroma_words, rows: chroma_size.1 });
        params.push([final_offset[1], final_offset[2], stride(1), chroma_size.0, chroma_size.1, chroma_words, (chroma.start_bytes / 4) as u32, 1]);
        let mut param_bytes = vec![0u8; params.len() * SLOT as usize];
        for (i, p) in params.iter().enumerate() {
            param_bytes[i * SLOT as usize..i * SLOT as usize + 32].copy_from_slice(&words(p));
        }
        let params_buffer = buffer(device, "pyrowave params", param_bytes.len() as u64, wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST);
        queue.write_buffer(&params_buffer, 0, &param_bytes);

        let bind_group = Self::bind(device, &bind_layout, [&coef, &params_buffer, &payload, &offsets, &blocks, &texels]);
        Ok(Self {
            layout,
            bind_layout,
            dequant,
            idwt,
            pack,
            coef,
            params: params_buffer,
            payload,
            payload_capacity,
            offsets,
            blocks,
            texels,
            bind_group,
            passes,
            luma,
            chroma,
        })
    }

    fn bind(device: &wgpu::Device, layout: &wgpu::BindGroupLayout, b: [&wgpu::Buffer; 6]) -> wgpu::BindGroup {
        const BINDINGS: [u32; 6] = [0, 2, 3, 4, 5, 6];
        let entries: Vec<_> = b
            .iter()
            .zip(BINDINGS)
            .map(|(buf, binding)| wgpu::BindGroupEntry {
                binding,
                resource: if binding == 2 {
                    wgpu::BindingResource::Buffer(wgpu::BufferBinding { buffer: buf, offset: 0, size: NonZeroU64::new(32) })
                } else {
                    buf.as_entire_binding()
                },
            })
            .collect();
        device.create_bind_group(&wgpu::BindGroupDescriptor { label: Some("pyrowave"), layout, entries: &entries })
    }

    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    pub fn chroma(&self) -> Chroma {
        self.layout.chroma
    }

    /// Luma (R8) and CbCr (RG8) planes holding the last decoded frame.
    pub fn views(&self) -> (&wgpu::TextureView, &wgpu::TextureView) {
        (&self.luma.view, &self.chroma.view)
    }

    pub fn textures(&self) -> (&wgpu::Texture, &wgpu::Texture) {
        (&self.luma.texture, &self.chroma.texture)
    }

    /// Records the decode of `frame` into `encoder`. Missing blocks decode as zeros.
    pub fn encode(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, encoder: &mut wgpu::CommandEncoder, frame: &Frame) {
        // A few spare words: the shader's bounds checks never read them, but a binding must be non-empty.
        let needed = (frame.words.len() as u64 + 4) * 4;
        if needed > self.payload_capacity {
            self.payload_capacity = needed.next_power_of_two();
            self.payload = buffer(device, "pyrowave payload", self.payload_capacity, wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST);
            self.bind_group = Self::bind(
                device,
                &self.bind_layout,
                [&self.coef, &self.params, &self.payload, &self.offsets, &self.blocks, &self.texels],
            );
        }
        let mut payload = words(&frame.words);
        payload.resize(payload.len() + 16, 0);
        queue.write_buffer(&self.payload, 0, &payload);
        queue.write_buffer(&self.offsets, 0, &words(&frame.offsets));
        // The dequant shader needs the payload length to bound reads.
        let mut dequant = [0u8; 16];
        dequant[..4].copy_from_slice(&self.layout.block_count.to_le_bytes());
        dequant[4..8].copy_from_slice(&self.layout.block_count.clamp(1, MAX_GROUPS).to_le_bytes());
        dequant[8..12].copy_from_slice(&((payload.len() / 4) as u32).to_le_bytes());
        queue.write_buffer(&self.params, 0, &dequant);

        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor { label: Some("pyrowave decode"), timestamp_writes: None });
            for (i, p) in self.passes.iter().enumerate() {
                pass.set_bind_group(0, &self.bind_group, &[(i as u64 * SLOT) as u32]);
                match *p {
                    Pass::Dequant { x, y } => {
                        pass.set_pipeline(&self.dequant);
                        pass.dispatch_workgroups(x, y, 1);
                    }
                    Pass::Idwt { x, y } => {
                        pass.set_pipeline(&self.idwt);
                        pass.dispatch_workgroups(x, y, 1);
                    }
                    Pass::Pack { words, rows } => {
                        pass.set_pipeline(&self.pack);
                        pass.dispatch_workgroups(words.div_ceil(64), rows, 1);
                    }
                }
            }
        }
        for plane in [&self.luma, &self.chroma] {
            encoder.copy_buffer_to_texture(
                wgpu::TexelCopyBufferInfo {
                    buffer: &self.texels,
                    layout: wgpu::TexelCopyBufferLayout { offset: plane.start_bytes, bytes_per_row: Some(plane.pitch), rows_per_image: None },
                },
                plane.texture.as_image_copy(),
                wgpu::Extent3d { width: plane.size.0, height: plane.size.1, depth_or_array_layers: 1 },
            );
        }
    }

    /// Decodes `frame` and submits it.
    pub fn decode(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, frame: &Frame) {
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("pyrowave") });
        self.encode(device, queue, &mut encoder, frame);
        queue.submit([encoder.finish()]);
    }
}
