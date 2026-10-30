//! Agent status file, written by `run` and read by `setup` / `status`.

use serde::{Deserialize, Serialize};
use umac_transport::identity::write_private;

use crate::config;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Status {
    pub pid: u32,
    pub version: String,
    pub screen_recording: bool,
    pub accessibility: bool,
    pub listening: Option<u16>,
    pub error: Option<String>,
    pub updated_at: u64,
}

impl Status {
    pub fn read() -> Option<Self> {
        serde_json::from_slice(&std::fs::read(config::support_dir().join("status.json")).ok()?).ok()
    }

    pub fn write(&self) {
        let mut s = self.clone();
        s.updated_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let path = config::support_dir().join("status.json");
        let res = umac_transport::identity::create_private_dir(&config::support_dir())
            .and_then(|_| write_private(&path, &serde_json::to_vec_pretty(&s).unwrap_or_default()));
        if let Err(e) = res {
            tracing::warn!("writing status: {e}");
        }
    }
}
