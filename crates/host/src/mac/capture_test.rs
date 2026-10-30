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
    let stats = Arc::new(Mutex::new((Vec::<u64>::new(), 0usize, 0usize, 0usize)));
    let (f, s) = (file.clone(), stats.clone());
    let stream = WindowStream::start(
        &window,
        StreamParams { window: id, codec, fps, bitrate_kbps, pyro_bpp_milli: (crate::config::Settings::load().pyrowave_bpp * 1000.0) as u32, max_datagram: 1400 },
        0.0,
        Arc::new(move |_, frame| {
            let lat = clock::now_us().saturating_sub(frame.capture_us);
            let mut st = s.lock().unwrap();
            st.0.push(lat);
            st.1 += frame.data.len();
            st.2 += frame.keyframe as usize;
            st.3 += frame.pyro.as_ref().map_or(0, |p| p.packets.len());
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
    let oversized = stream.knobs().oversized.load(std::sync::atomic::Ordering::Relaxed);
    drop(stream);
    let (mut lat, bytes, keyframes, packets) = stats.lock().unwrap().clone();
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
    if codec == Codec::PyroWave {
        println!("packets/frame {:.1}, oversized packets dropped {oversized}", packets as f64 / lat.len() as f64);
    }
    println!("wrote {}", out.display());
    Ok(())
}
