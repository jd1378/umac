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

/// Decodes frame A fully, then a changed frame as the sender would send it: only the blocks
/// touching the dirty rects. The result must match decoding the whole changed frame; without
/// the synthesis spread a change next to a block edge leaves a seam.
/// B: A with a text-line sized stripe pattern (96..176 x 60..72, chroma tinted under it) and an
/// inverted cursor sized patch (250..262 x 118..136). C: A with an 8x8 patch at 120..128, right
/// at a level 0 block edge. Both encoded by PyroWave's Metal encoder at 21600 bytes.
#[test]
fn changed_region_subset_matches_full_frame() {
    use umac_pyrowave_decode::select::{Rect, area, rebuild};
    let Some((device, queue)) = gpu() else { return };
    let layout = Layout::new(320, 180, Chroma::Yuv420).unwrap();
    let a = frames(&std::fs::read(testdata("320x180.420.b21600.pkts")).unwrap()).remove(0);
    let decode = |dec: &mut GpuDecoder, packets: &[Vec<u8>]| {
        let mut asm = Assembler::new(layout.clone(), default_frame_cap(&layout));
        for p in packets {
            asm.push_packet(p).unwrap();
        }
        dec.decode(&device, &queue, &asm.take_frame(false).expect("complete"));
        let (yt, ct) = dec.textures();
        [read_texture(&device, &queue, yt, 1), read_texture(&device, &queue, ct, 2)].concat()
    };
    let cases: [(&str, &[Rect]); 2] = [
        ("B", &[Rect { x0: 96, y0: 60, x1: 176, y1: 72 }, Rect { x0: 250, y0: 118, x1: 262, y1: 136 }]),
        ("C", &[Rect { x0: 120, y0: 120, x1: 128, y1: 128 }]),
    ];
    for (name, dirty) in cases {
        let changed = frames(&std::fs::read(testdata(&format!("320x180.420.b21600.{name}.pkts"))).unwrap()).remove(0);
        let subset = |spread: bool| -> Vec<Vec<u8>> {
            let keep: Vec<bool> = (0..layout.block_count).map(|i| dirty.iter().any(|d| d.intersects(&area(&layout, i, spread).unwrap()))).collect();
            let data = changed.concat();
            let mut ranges = Vec::new();
            let mut at = 0;
            for p in &changed {
                ranges.push(at..at + p.len());
                at += p.len();
            }
            let r = rebuild(&layout, &data, &ranges, &keep, 1400).unwrap();
            println!("{name} spread={spread}: {} of {} blocks, {} of {} bytes", r.kept, r.total, r.data.len(), data.len());
            r.packets.iter().map(|p| r.data[p.clone()].to_vec()).collect()
        };
        let full = decode(&mut GpuDecoder::new(&device, &queue, layout.clone()).unwrap(), &changed);
        let run = |spread: bool| {
            let mut dec = GpuDecoder::new(&device, &queue, layout.clone()).unwrap();
            decode(&mut dec, &a);
            decode(&mut dec, &subset(spread))
        };
        let (good, seams) = (run(true), run(false));
        let max = |x: &[u8]| x.iter().zip(&full).map(|(&p, &q)| p.abs_diff(q)).max().unwrap();
        println!(
            "{name}: with spread max diff {} psnr {:.1}; without max diff {} psnr {:.1}",
            max(&good),
            psnr(&good, &full),
            max(&seams),
            psnr(&seams, &full)
        );
        assert!(psnr(&good, &full) > 40.0, "{name}");
        if name == "C" {
            assert!(psnr(&seams, &full) < psnr(&good, &full) - 3.0, "the synthesis spread should matter");
        }
    }
}
