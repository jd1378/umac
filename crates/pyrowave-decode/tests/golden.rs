//! Decodes bitstreams produced by PyroWave's own Metal encoder on a Mac and compares the
//! result with PyroWave's Metal decoder output (`testdata/*.dec.yuv`) and the source frames.

use std::path::PathBuf;

use umac_pyrowave_decode::{Assembler, Chroma, GpuDecoder, Layout, default_frame_cap};

fn testdata(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata").join(name)
}

fn frames(pkts: &[u8]) -> Vec<Vec<Vec<u8>>> {
    let mut out = Vec::new();
    let mut at = 0;
    let u32_at = |i: usize| u32::from_le_bytes(pkts[i..i + 4].try_into().unwrap()) as usize;
    while at < pkts.len() {
        let count = u32_at(at);
        at += 4;
        let mut frame = Vec::new();
        for _ in 0..count {
            let size = u32_at(at);
            frame.push(pkts[at + 4..at + 4 + size].to_vec());
            at += 4 + size;
        }
        out.push(frame);
    }
    out
}

fn gpu() -> Option<(wgpu::Device, wgpu::Queue)> {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default())).ok()?;
    pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default())).ok()
}

fn read_texture(device: &wgpu::Device, queue: &wgpu::Queue, texture: &wgpu::Texture, bpp: u32) -> Vec<u8> {
    let (w, h) = (texture.width(), texture.height());
    let pitch = (w * bpp).next_multiple_of(256);
    let buf = device.create_buffer(&wgpu::BufferDescriptor {
        label: None,
        size: (pitch * h) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut enc = device.create_command_encoder(&Default::default());
    enc.copy_texture_to_buffer(
        texture.as_image_copy(),
        wgpu::TexelCopyBufferInfo {
            buffer: &buf,
            layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(pitch), rows_per_image: None },
        },
        wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
    );
    queue.submit([enc.finish()]);
    buf.slice(..).map_async(wgpu::MapMode::Read, |r| r.unwrap());
    device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
    let data = buf.slice(..).get_mapped_range().unwrap();
    let mut out = Vec::with_capacity((w * h * bpp) as usize);
    for y in 0..h {
        let row = (y * pitch) as usize;
        out.extend_from_slice(&data[row..row + (w * bpp) as usize]);
    }
    out
}

fn psnr(a: &[u8], b: &[u8]) -> f64 {
    let mse = a.iter().zip(b).map(|(&x, &y)| (x as f64 - y as f64).powi(2)).sum::<f64>() / a.len() as f64;
    if mse == 0.0 { 99.0 } else { 10.0 * (255.0f64 * 255.0 / mse).log10() }
}

struct Case {
    name: &'static str,
    width: u32,
    height: u32,
    chroma: Chroma,
}

const CASES: &[Case] = &[
    Case { name: "256x144.420.b13824", width: 256, height: 144, chroma: Chroma::Yuv420 },
    Case { name: "256x144.420.b2764", width: 256, height: 144, chroma: Chroma::Yuv420 },
    Case { name: "256x144.444.b13824", width: 256, height: 144, chroma: Chroma::Yuv444 },
    Case { name: "256x144.444.b2764", width: 256, height: 144, chroma: Chroma::Yuv444 },
    Case { name: "320x180.420.b21600", width: 320, height: 180, chroma: Chroma::Yuv420 },
    Case { name: "320x180.420.b4320", width: 320, height: 180, chroma: Chroma::Yuv420 },
    Case { name: "320x180.444.b21600", width: 320, height: 180, chroma: Chroma::Yuv444 },
    Case { name: "320x180.444.b4320", width: 320, height: 180, chroma: Chroma::Yuv444 },
    Case { name: "200x120.444.b9000", width: 200, height: 120, chroma: Chroma::Yuv444 },
    Case { name: "200x120.444.b1800", width: 200, height: 120, chroma: Chroma::Yuv444 },
];

#[test]
fn matches_pyrowave_metal_decoder() {
    let Some((device, queue)) = gpu() else {
        eprintln!("no GPU, skipping");
        return;
    };
    for case in CASES {
        let layout = Layout::new(case.width, case.height, case.chroma).unwrap();
        let mut asm = Assembler::new(layout.clone(), default_frame_cap(&layout));
        let mut dec = GpuDecoder::new(&device, &queue, layout.clone()).unwrap();
        let pkts = std::fs::read(testdata(&format!("{}.pkts", case.name))).unwrap();
        for frame in frames(&pkts) {
            for p in &frame {
                asm.push_packet(p).unwrap();
            }
            let f = asm.take_frame(false).expect("complete frame");
            assert_eq!(Some(f.received), f.total_blocks, "{}", case.name);
            dec.decode(&device, &queue, &f);
        }
        let (yt, ct) = dec.textures();
        let y = read_texture(&device, &queue, yt, 1);
        let cbcr = read_texture(&device, &queue, ct, 2);
        let cb: Vec<u8> = cbcr.iter().step_by(2).copied().collect();
        let cr: Vec<u8> = cbcr.iter().skip(1).step_by(2).copied().collect();
        let ours = [y, cb, cr].concat();

        let reference = std::fs::read(testdata(&format!("{}.dec.yuv", case.name))).unwrap();
        let chroma_tag = if case.chroma == Chroma::Yuv420 { "420" } else { "444" };
        let source = std::fs::read(testdata(&format!("src_{}x{}.{chroma_tag}.yuv", case.width, case.height))).unwrap();
        assert_eq!(ours.len(), reference.len(), "{}", case.name);
        let diffs: Vec<u8> = ours.iter().zip(&reference).map(|(&a, &b)| a.abs_diff(b)).collect();
        let max = *diffs.iter().max().unwrap();
        let off = diffs.iter().filter(|&&d| d > 0).count();
        let (vs_ref, vs_src, ref_src) = (psnr(&ours, &reference), psnr(&ours, &source), psnr(&reference, &source));
        println!(
            "{:22} max_diff={max} differing={:.2}% psnr_vs_metal={vs_ref:.1} psnr_vs_source={vs_src:.2} (metal {ref_src:.2})",
            case.name,
            100.0 * off as f64 / diffs.len() as f64
        );
        assert!(max <= 2, "{}: max diff {max}", case.name);
        assert!((vs_src - ref_src).abs() < 0.5, "{}: quality differs from the reference decoder", case.name);
    }
}

#[test]
fn missing_blocks_still_decode() {
    let Some((device, queue)) = gpu() else { return };
    let layout = Layout::new(320, 180, Chroma::Yuv420).unwrap();
    let mut asm = Assembler::new(layout.clone(), default_frame_cap(&layout));
    let mut dec = GpuDecoder::new(&device, &queue, layout).unwrap();
    let pkts = std::fs::read(testdata("320x180.420.b21600.pkts")).unwrap();
    let frame = &frames(&pkts)[0];
    // Drop every third packet: decode must still work, just softer.
    for p in frame.iter().enumerate().filter(|(i, _)| i % 3 != 1).map(|(_, p)| p) {
        asm.push_packet(p).unwrap();
    }
    assert!(asm.take_frame(false).is_none());
    let f = asm.take_frame(true).unwrap();
    assert!(f.received < f.total_blocks.unwrap());
    dec.decode(&device, &queue, &f);
    let y = read_texture(&device, &queue, dec.textures().0, 1);
    let source = std::fs::read(testdata("src_320x180.420.yuv")).unwrap();
    let p = psnr(&y, &source[..y.len()]);
    println!("partial frame luma psnr {p:.1}");
    assert!(p > 15.0);
}
