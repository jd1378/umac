//! On-disk locations and agent settings.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

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
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            port: umac_proto::DEFAULT_PORT,
            // The Mac is expected on Wi-Fi, where single datagram losses are common.
            fec_percent: 15,
            default_bitrate_kbps: 25_000,
            max_bitrate_kbps: 100_000,
        }
    }
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
