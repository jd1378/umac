//! Decode timing: `cargo run --release --example bench -- <file.pkts> <width> <height> <420|444> [iterations]`
//! (packet files as written by the golden vector tool: per frame a u32 count, then u32 size + bytes).

use std::time::Instant;

use umac_pyrowave_decode::{Assembler, Chroma, GpuDecoder, Layout, default_frame_cap};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let pkts = std::fs::read(&args[0]).expect("packet file");
    let (w, h): (u32, u32) = (args[1].parse().unwrap(), args[2].parse().unwrap());
    let chroma = if args[3] == "444" { Chroma::Yuv444 } else { Chroma::Yuv420 };
    let iterations: usize = args.get(4).map_or(200, |s| s.parse().unwrap());

    let mut packets = Vec::new();
    let mut at = 0;
    let u32_at = |i: usize| u32::from_le_bytes(pkts[i..i + 4].try_into().unwrap()) as usize;
    let count = u32_at(at);
    at += 4;
    for _ in 0..count {
        let size = u32_at(at);
        packets.push(&pkts[at + 4..at + 4 + size]);
        at += 4 + size;
    }

    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
    let adapter = pollster::block_on(instance.request_adapter(&Default::default())).expect("adapter");
    println!("GPU: {}", adapter.get_info().name);
    let (device, queue) = pollster::block_on(adapter.request_device(&Default::default())).expect("device");
    let layout = Layout::new(w, h, chroma).unwrap();
    let mut asm = Assembler::new(layout.clone(), default_frame_cap(&layout));
    let mut dec = GpuDecoder::new(&device, &queue, layout).unwrap();

    let (mut parse, mut gpu) = (Vec::new(), Vec::new());
    for i in 0..iterations {
        let t0 = Instant::now();
        // Rewrite the 3-bit sequence so every iteration is a new frame.
        for p in &packets {
            let mut p = p.to_vec();
            let mut off = 0;
            while off + 8 <= p.len() {
                let w0 = u32::from_le_bytes(p[off..off + 4].try_into().unwrap());
                let w0 = (w0 & !(7 << 28)) | (((i as u32) & 7) << 28);
                p[off..off + 4].copy_from_slice(&w0.to_le_bytes());
                off += if w0 >> 31 != 0 { 8 } else { (((w0 >> 16) & 0xfff) as usize * 4).max(8) };
            }
            asm.push_packet(&p).unwrap();
        }
        let frame = asm.take_frame(false).expect("complete");
        let t1 = Instant::now();
        dec.decode(&device, &queue, &frame);
        device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
        let t2 = Instant::now();
        if i >= 10 {
            parse.push((t1 - t0).as_secs_f64() * 1e3);
            gpu.push((t2 - t1).as_secs_f64() * 1e3);
        }
    }
    let pct = |v: &mut Vec<f64>, p: f64| {
        v.sort_by(f64::total_cmp);
        v[((v.len() - 1) as f64 * p) as usize]
    };
    println!(
        "{w}x{h} {:?} {} bytes/frame: parse+validate p50 {:.3} ms p95 {:.3} ms, upload+decode (wall, to GPU idle) p50 {:.3} ms p95 {:.3} ms",
        chroma,
        packets.iter().map(|p| p.len()).sum::<usize>(),
        pct(&mut parse, 0.5),
        pct(&mut parse, 0.95),
        pct(&mut gpu, 0.5),
        pct(&mut gpu, 0.95)
    );
}
