//! Client settings, paired hosts and pairing links. Everything lives in one private directory.

use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use umac_transport::{Fingerprint, Identity, identity};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub bitrate_kbps: u32,
    pub max_fps: u16,
    /// Ctrl acts as Cmd and Super acts as Control, matching Linux/Windows shortcut habits.
    pub swap_ctrl_cmd: bool,
    /// Lines scrolled per mouse wheel notch, before the desktop's own scroll speed factor.
    pub scroll_lines: f32,
    /// Rewrite Linux text editing chords (Ctrl+Backspace, Ctrl+Arrows, Home/End) to their Mac
    /// equivalents, and use Linux terminal keys in Mac terminals (Ctrl+C interrupts,
    /// Ctrl+Shift+C/V copy and paste).
    pub linux_shortcuts: bool,
    /// Copies made on the Mac land on this computer's clipboard.
    pub clipboard_to_pc: bool,
    /// When this computer's clipboard is sent to the Mac.
    pub clipboard_to_mac: ClipboardToMac,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bitrate_kbps: 25_000,
            max_fps: 60,
            swap_ctrl_cmd: true,
            linux_shortcuts: true,
            scroll_lines: 3.0,
            clipboard_to_pc: true,
            clipboard_to_mac: ClipboardToMac::OnPaste,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClipboardToMac {
    Off,
    /// Only when pasting on the Mac (Cmd+V or a Paste menu item): shares just what you paste.
    OnPaste,
    /// Also whenever a streamed window gains focus.
    Always,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostEntry {
    pub name: String,
    /// `host:port` as given in the pairing link.
    pub addr: String,
    pub fingerprint: String,
}

impl HostEntry {
    pub fn fingerprint(&self) -> Result<Fingerprint> {
        self.fingerprint.parse().map_err(|_| anyhow!("corrupt fingerprint for host {}", self.name))
    }
}

pub struct Store {
    dir: PathBuf,
}

impl Store {
    pub fn new(dir: Option<PathBuf>) -> Result<Self> {
        let dir = match dir {
            Some(d) => d,
            None => dirs::config_dir().context("no config directory")?.join("umac"),
        };
        identity::create_private_dir(&dir)?;
        Ok(Self { dir })
    }

    pub fn identity(&self) -> Result<Identity> {
        Ok(Identity::load_or_create(&self.dir.join("identity"))?)
    }

    pub fn config(&self) -> Result<Config> {
        read_json_or_default(&self.dir.join("config.json"))
    }

    pub fn hosts(&self) -> Result<Vec<HostEntry>> {
        read_json_or_default(&self.dir.join("hosts.json"))
    }

    /// Adds or replaces a host, matching by name or address.
    pub fn save_host(&self, entry: HostEntry) -> Result<()> {
        let mut hosts = self.hosts()?;
        hosts.retain(|h| h.name != entry.name && h.addr != entry.addr);
        hosts.push(entry);
        identity::write_private(&self.dir.join("hosts.json"), &serde_json::to_vec_pretty(&hosts)?)?;
        Ok(())
    }

    /// Picks the named host, or the only paired host when no name is given.
    pub fn host(&self, name: Option<&str>) -> Result<HostEntry> {
        let hosts = self.hosts()?;
        match name {
            Some(n) => hosts.into_iter().find(|h| h.name == n).ok_or_else(|| anyhow!("unknown host {n}")),
            None if hosts.len() == 1 => Ok(hosts.into_iter().next().unwrap()),
            None if hosts.is_empty() => bail!("no paired host; run `umac pair <link>` first"),
            None => bail!("several hosts paired; pick one with --host"),
        }
    }
}

fn read_json_or_default<T: Default + for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(e) => Err(e.into()),
    }
}

/// Parsed `umac://host:port?fp=<sha256 hex>&token=<hex>` link printed by `umac-host pair`.
#[derive(Debug, PartialEq)]
pub struct PairingLink {
    pub addr: String,
    pub fingerprint: Fingerprint,
    pub token: String,
}

impl std::str::FromStr for PairingLink {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        let url = url::Url::parse(s.trim()).context("invalid pairing link")?;
        if url.scheme() != "umac" {
            bail!("pairing link must start with umac://");
        }
        let host = url.host_str().context("pairing link has no host")?;
        let port = url.port().unwrap_or(umac_proto::DEFAULT_PORT);
        let (mut fp, mut token) = (None, None);
        for (k, v) in url.query_pairs() {
            match &*k {
                "fp" => fp = Some(v.parse::<Fingerprint>().map_err(|_| anyhow!("bad fingerprint in link"))?),
                "token" => token = Some(v.into_owned()),
                _ => {}
            }
        }
        let token = token.context("pairing link has no token")?;
        if token.is_empty() || token.len() > 128 || !token.bytes().all(|b| b.is_ascii_alphanumeric()) {
            bail!("bad token in pairing link");
        }
        // IPv6 literals come back bracketed from host_str, which is what socket parsing wants.
        Ok(Self { addr: format!("{host}:{port}"), fingerprint: fp.context("pairing link has no fp")?, token })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FP: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

    #[test]
    fn parses_link() {
        let l: PairingLink = format!("umac://192.0.2.10:4000?fp={FP}&token=abc123").parse().unwrap();
        assert_eq!(l.addr, "192.0.2.10:4000");
        assert_eq!(l.fingerprint.to_string(), FP);
        assert_eq!(l.token, "abc123");
        let l: PairingLink = format!("umac://mac.local?token=t&fp={FP}").parse().unwrap();
        assert_eq!(l.addr, format!("mac.local:{}", umac_proto::DEFAULT_PORT));
        let l: PairingLink = format!("umac://[fe80::1]:5?fp={FP}&token=t").parse().unwrap();
        assert_eq!(l.addr, "[fe80::1]:5");
    }

    #[test]
    fn rejects_bad_links() {
        for bad in [
            format!("http://h:1?fp={FP}&token=t"),
            "umac://h:1?token=t".to_owned(),
            format!("umac://h:1?fp={FP}"),
            "umac://h:1?fp=zz&token=t".to_owned(),
            format!("umac://h:1?fp={FP}&token=a%20b"),
        ] {
            assert!(bad.parse::<PairingLink>().is_err(), "{bad}");
        }
    }

    #[test]
    fn store_roundtrip() {
        let dir = std::env::temp_dir().join(format!("umac-client-store-{}", std::process::id()));
        let store = Store::new(Some(dir.clone())).unwrap();
        assert!(store.host(None).is_err());
        let e = HostEntry { name: "mac-a".into(), addr: "1.2.3.4:5".into(), fingerprint: FP.into() };
        store.save_host(e.clone()).unwrap();
        store.save_host(e.clone()).unwrap();
        assert_eq!(store.host(None).unwrap(), e);
        assert_eq!(store.config().unwrap().bitrate_kbps, 25_000);
        fs::remove_dir_all(dir).unwrap();
    }
}
