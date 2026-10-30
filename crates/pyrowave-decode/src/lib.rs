//! PyroWave (bitstream v1) decoder in Rust on wgpu.
//!
//! [`bitstream::Assembler`] validates packets and collects them into frames on the CPU;
//! [`gpu::GpuDecoder`] turns a frame into R8 luma and RG8 CbCr textures with compute shaders.
//! The shaders are ported from PyroWave by Hans-Kristian Arntzen (MIT), see `NOTICE`.

pub mod bitstream;
pub mod gpu;
pub mod layout;

pub use bitstream::{Assembler, Error, Frame};
pub use gpu::GpuDecoder;
pub use layout::{Chroma, Layout};

/// Default cap on one coded frame: 24 bits per pixel, far above any useful rate.
pub fn default_frame_cap(layout: &Layout) -> usize {
    layout.aligned_width as usize * layout.aligned_height as usize * 3 + 4096
}
