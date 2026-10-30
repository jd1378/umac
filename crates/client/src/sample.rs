//! Test and demo helper: encoded frames from a media file, one access unit per packet,
//! in the Annex-B layout the host sends.

use std::path::Path;

use anyhow::{Context, Result};

pub struct SamplePacket {
    pub data: Vec<u8>,
    pub keyframe: bool,
}

/// Reads all video packets from a file with Annex-B framing (e.g. MPEG-TS from the ffmpeg CLI).
pub fn read_packets(path: &Path) -> Result<Vec<SamplePacket>> {
    ffmpeg_next::init()?;
    let mut input = ffmpeg_next::format::input(path).with_context(|| format!("opening {}", path.display()))?;
    let stream = input.streams().best(ffmpeg_next::media::Type::Video).context("no video stream")?.index();
    Ok(input
        .packets()
        .filter(|(s, _)| s.index() == stream)
        .filter_map(|(_, p)| Some(SamplePacket { keyframe: p.is_key(), data: p.data()?.to_vec() }))
        .collect())
}

/// Generates a short HEVC test clip with the ffmpeg CLI (no B-frames, headers on every keyframe).
pub fn generate_hevc(path: &Path, width: u32, height: u32, seconds: u32) -> Result<()> {
    let status = std::process::Command::new("ffmpeg")
        .args(["-loglevel", "error", "-y", "-f", "lavfi", "-i"])
        .arg(format!("testsrc2=size={width}x{height}:rate=60"))
        .args(["-t", &seconds.to_string(), "-pix_fmt", "yuv420p", "-c:v", "libx265"])
        .args(["-x265-params", "bframes=0:repeat-headers=1:keyint=120:log-level=error"])
        .args(["-f", "mpegts"])
        .arg(path)
        .status()
        .context("running ffmpeg")?;
    anyhow::ensure!(status.success(), "ffmpeg failed: {status}");
    Ok(())
}
