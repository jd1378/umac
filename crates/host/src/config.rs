//! On-disk locations and agent settings.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use umac_proto::Codec;

pub const LABEL: &str = "dev.umac.host";

pub fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

/// `~/Library/Application Support/umac`: identity, paired clients, pairing tokens, status.
pub fn support_dir() -> PathBuf {
    home().join("Library/Application Support/umac")
}

pub fn identity_dir() -> PathBuf {
    support_dir().join("identity")
}

pub fn install_path() -> PathBuf {
    support_dir().join("bin/umac-host")
}

pub fn log_dir() -> PathBuf {
    home().join("Library/Logs/umac")
}

pub fn launch_agent_path() -> PathBuf {
    home().join(format!("Library/LaunchAgents/{LABEL}.plist"))
}

pub fn settings_path() -> PathBuf {
    support_dir().join("settings.json")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub port: u16,
    /// Reed-Solomon recovery shards as a percentage of data shards.
    pub fec_percent: u8,
    pub default_bitrate_kbps: u32,
    pub max_bitrate_kbps: u32,
    /// Log filter (tracing `EnvFilter` syntax) when `RUST_LOG` is not set, e.g. "umac_host=debug".
    pub log: String,
    /// PyroWave quality ceiling in bits per pixel at 4:2:0 (the M1 benchmark was visually
    /// lossless from 1.2 and effectively lossless at 2.4). Small windows always get it; large
    /// ones are held to `pyrowave_max_mbps` while they change. Lowered on loss.
    pub pyrowave_bpp: f64,
    /// Rate cap per changing PyroWave stream in Mbit/s (about 1.6 bpp at 1080p60 for 200).
    pub pyrowave_max_mbps: f64,
    /// Send only the PyroWave blocks touching changed regions (off: every block, for A/B).
    pub pyrowave_changed_only: bool,
    /// Motion-JPEG quality, 0 to 1 (VideoToolbox rounds it to a few steps: 0.65 and 0.75
    /// encode the same, as do 0.85 and 0.92). 0.75 keeps small text clean at 1080p in about
    /// 170 to 290 KB per frame; 0.5 shows ringing around text.
    pub mjpeg_quality: f64,
    /// Codec preference, most preferred first; the first one the client also supports wins.
    pub codecs: Vec<Codec>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            port: umac_proto::DEFAULT_PORT,
            // The Mac is expected on Wi-Fi, where single datagram losses are common.
            fec_percent: 15,
            default_bitrate_kbps: 25_000,
            max_bitrate_kbps: 100_000,
            log: "info".into(),
            pyrowave_bpp: 3.0,
            pyrowave_max_mbps: 200.0,
            pyrowave_changed_only: true,
            mjpeg_quality: 0.75,
            codecs: vec![Codec::PyroWave, Codec::H264, Codec::Hevc, Codec::Mjpeg],
        }
    }
}

/// The first codec in `prefs` that this build and the client both support, if any.
pub fn choose_codec(prefs: &[Codec], supported: &[Codec], client: &[Codec]) -> Option<Codec> {
    prefs.iter().chain(supported).copied().find(|c| supported.contains(c) && client.contains(c))
}

impl Settings {
    pub fn load() -> Self {
        std::fs::read(settings_path())
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    pub fn clamp_bitrate(&self, requested_kbps: u32) -> u32 {
        if requested_kbps == 0 {
            self.default_bitrate_kbps
        } else {
            requested_kbps.clamp(1_000, self.max_bitrate_kbps)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codec_choice_follows_host_preference_within_client_support() {
        let supported = [Codec::PyroWave, Codec::Hevc, Codec::H264];
        let prefs = Settings::default().codecs;
        assert_eq!(choose_codec(&prefs, &supported, &[Codec::Hevc, Codec::PyroWave]), Some(Codec::PyroWave));
        assert_eq!(choose_codec(&prefs, &supported, &[Codec::Hevc, Codec::H264]), Some(Codec::H264));
        assert_eq!(choose_codec(&[Codec::Hevc], &supported, &[Codec::PyroWave, Codec::Hevc]), Some(Codec::Hevc));
        // A preference the build lacks is skipped; an empty list falls back to support order.
        assert_eq!(choose_codec(&[], &supported, &[Codec::H264, Codec::PyroWave]), Some(Codec::PyroWave));
        assert_eq!(choose_codec(&prefs, &supported, &[]), None);
        assert_eq!(choose_codec(&prefs, &[Codec::PyroWave], &[Codec::Hevc]), None);
    }

    #[test]
    fn mjpeg_is_the_last_resort() {
        let supported = [Codec::PyroWave, Codec::Hevc, Codec::H264, Codec::Mjpeg];
        let prefs = Settings::default().codecs;
        assert_eq!(choose_codec(&prefs, &supported, &[Codec::Mjpeg, Codec::H264]), Some(Codec::H264));
        assert_eq!(choose_codec(&prefs, &supported, &[Codec::Mjpeg]), Some(Codec::Mjpeg));
    }
}
