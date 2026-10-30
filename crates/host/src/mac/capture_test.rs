//! `umac-host capture-test`: capture + encode one window to an Annex-B file and report
//! encode latency (capture callback to encoded output).

use std::{
    io::Write,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, anyhow};
use umac_proto::Codec;

use super::{
    capture::find_window,
    stream::{StreamParams, WindowStream},
    windows,
};
use crate::clock;

pub fn resolve_window(spec: &str) -> anyhow::Result<u32> {
    if let Ok(id) = spec.parse::<u32>() {
        return Ok(id);
    }
    let needle = spec.to_lowercase();
    windows::list()?
        .into_iter()
        .find(|w| w.app_name.to_lowercase().contains(&needle) || w.title.to_lowercase().contains(&needle))
        .map(|w| w.id)
        .ok_or_else(|| anyhow!("no on-screen window matches {spec:?}"))
}

pub fn run(spec: &str, seconds: u64, out: &Path, codec: Codec, bitrate_kbps: u32, fps: u32) -> anyhow::Result<()> {
    let id = resolve_window(spec)?;
    let window = find_window(id)?;
    let file = Arc::new(Mutex::new(std::fs::File::create(out).with_context(|| format!("creating {}", out.display()))?));
    let stats = Arc::new(Mutex::new((Vec::<u64>::new(), 0usize, 0usize)));
    let (f, s) = (file.clone(), stats.clone());
    let stream = WindowStream::start(
        &window,
        StreamParams { window: id, codec, fps, bitrate_kbps },
        Arc::new(move |_, frame| {
            let lat = clock::now_us().saturating_sub(frame.capture_us);
            let mut st = s.lock().unwrap();
            st.0.push(lat);
            st.1 += frame.data.len();
            st.2 += frame.keyframe as usize;
            f.lock().unwrap().write_all(&frame.data).is_ok()
        }),
    )?;
    println!("capturing window {id} at {}x{} (scale {}) for {seconds}s", stream.size.0, stream.size.1, stream.scale());
    for _ in 0..seconds * 4 {
        std::thread::sleep(Duration::from_millis(250));
        // Exercise the keyframe path once per second.
        stream.request_keyframe();
        stream.serve_idle_keyframe();
    }
    drop(stream);
    let (mut lat, bytes, keyframes) = stats.lock().unwrap().clone();
    if lat.is_empty() {
        println!("no frames captured (is Screen Recording allowed? is the window visible?)");
        return Ok(());
    }
    lat.sort_unstable();
    let pct = |p: f64| lat[((lat.len() - 1) as f64 * p) as usize] as f64 / 1000.0;
    println!(
        "frames {} (keyframes {keyframes}), {:.1} KB avg, encode latency ms: p50 {:.2} p95 {:.2} max {:.2}",
        lat.len(),
        bytes as f64 / lat.len() as f64 / 1024.0,
        pct(0.5),
        pct(0.95),
        pct(1.0)
    );
    println!("wrote {}", out.display());
    Ok(())
}
