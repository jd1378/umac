//! Decode timing for Motion-JPEG frames: `cargo run --release --example mjpeg_bench -- <file.jpg>...`

use std::time::Instant;

use umac_client::decode::{Decoder, mjpeg::MjpegDecoder};

fn main() -> anyhow::Result<()> {
    for path in std::env::args().skip(1) {
        let data = std::fs::read(&path)?;
        let mut dec = MjpegDecoder;
        let mut times = Vec::new();
        for _ in 0..100 {
            let t = Instant::now();
            dec.decode(&data)?;
            times.push(t.elapsed().as_secs_f64() * 1e3);
        }
        times.sort_by(f64::total_cmp);
        println!("{path}: {} bytes, decode+convert p50 {:.2} ms p95 {:.2} ms", data.len(), times[50], times[95]);
    }
    Ok(())
}
