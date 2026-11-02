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
    /// Fingerprint of the host the tray and commands without `--host` use.
    pub selected_host: Option<String>,
    /// Tray icon line color; `auto` follows the desktop color scheme.
    pub tray_icon_color: TrayIconColor,
    /// Open windows, dialogs and sheets that a streamed Mac app opens by itself.
    pub follow_new_windows: bool,
    /// Open the window the Mac focuses when it is not shown yet: an app started from a
    /// streamed window, a password prompt or alert that pops up, a Cmd+Tab switch.
    pub follow_focus: bool,
    /// Closing a window here closes it on the Mac too (its close button; unsaved documents ask
    /// first). Off: closing only stops streaming it.
    pub close_on_mac: bool,
    /// Log per-window latency stages every 2 s (also `--stats`).
    pub show_stats: bool,
    /// Pixels a Mac point becomes here. 1 shows Mac windows at their natural size whatever
    /// the Mac's display runs at (Retina, 4K); more makes them bigger, up to the Mac display's
    /// own scale.
    pub view_scale: f32,
    /// Keep every frame's stages for a summary when the session ends (`umac latency`).
    #[serde(skip)]
    pub latency_summary: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrayIconColor {
    #[default]
    Auto,
    Black,
    White,
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
            selected_host: None,
            tray_icon_color: TrayIconColor::Auto,
            follow_new_windows: true,
            follow_focus: true,
            close_on_mac: true,
            show_stats: false,
            view_scale: 1.0,
            latency_summary: false,
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

/// One paired agent. Every Mac user who ran setup has an agent of their own, so a Mac with
/// several users is several entries sharing `machine`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HostEntry {
    /// Short number for the command line (`umac hosts`, `--host`, `unpair`); kept when other
    /// entries go away.
    #[serde(default)]
    pub id: u32,
    pub name: String,
    /// `host:port` as given in the pairing link.
    pub addr: String,
    pub fingerprint: String,
    /// Mac user the agent runs for; learned on connect.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// Id shared by all agents on one Mac; learned on connect.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine: Option<String>,
}

impl HostEntry {
    pub fn fingerprint(&self) -> Result<Fingerprint> {
        self.fingerprint.parse().map_err(|_| anyhow!("corrupt fingerprint for host {}", self.name))
    }

    /// Whether `which` names this entry: its id, name or a fingerprint prefix.
    fn is(&self, which: &str) -> bool {
        self.name == which
            || which.parse::<u32>().is_ok_and(|id| id != 0 && id == self.id)
            || (which.len() >= FINGERPRINT_PREFIX && self.fingerprint.starts_with(&which.to_ascii_lowercase()))
    }

    /// The Mac's name without the `user@` that pairing puts in front.
    pub fn mac_name(&self) -> &str {
        self.user.as_deref().and_then(|u| self.name.strip_prefix(u)?.strip_prefix('@')).unwrap_or(&self.name)
    }

    /// Whether `other` is an agent on the same Mac (possibly this one).
    pub fn same_mac(&self, other: &HostEntry) -> bool {
        self.fingerprint == other.fingerprint || self.machine.as_ref().is_some_and(|m| !m.is_empty() && other.machine.as_ref() == Some(m))
    }
}

/// Shortest fingerprint prefix accepted in place of a host's id or name.
const FINGERPRINT_PREFIX: usize = 8;

/// Gives entries without an id the smallest free ones; returns whether any changed.
fn number(hosts: &mut [HostEntry]) -> bool {
    let mut changed = false;
    for i in 0..hosts.len() {
        if hosts[i].id == 0 {
            hosts[i].id = (1..).find(|n| hosts.iter().all(|h| h.id != *n)).unwrap();
            changed = true;
        }
    }
    changed
}

/// `entry` first, then the agents of the Mac's other users.
pub fn mac_group(hosts: &[HostEntry], entry: &HostEntry) -> Vec<HostEntry> {
    std::iter::once(entry.clone()).chain(hosts.iter().filter(|h| h.fingerprint != entry.fingerprint && entry.same_mac(h)).cloned()).collect()
}

#[derive(Clone)]
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

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Writes the config file.
    pub fn save_config(&self, cfg: &Config) -> Result<()> {
        identity::write_private(&self.dir.join("config.json"), &serde_json::to_vec_pretty(cfg)?)?;
        Ok(())
    }

    /// Makes `fingerprint` the host used when no `--host` is given.
    pub fn select_host(&self, fingerprint: &str) -> Result<()> {
        let mut cfg = self.config()?;
        cfg.selected_host = Some(fingerprint.to_owned());
        self.save_config(&cfg)
    }

    pub fn hosts(&self) -> Result<Vec<HostEntry>> {
        let mut hosts: Vec<HostEntry> = read_json_or_default(&self.dir.join("hosts.json"))?;
        // Files from before ids existed get them once.
        if number(&mut hosts) {
            self.write_hosts(&hosts)?;
        }
        Ok(hosts)
    }

    fn write_hosts(&self, hosts: &[HostEntry]) -> Result<()> {
        identity::write_private(&self.dir.join("hosts.json"), &serde_json::to_vec_pretty(hosts)?)?;
        Ok(())
    }

    /// Adds or replaces a host, matching by name or address.
    pub fn save_host(&self, mut entry: HostEntry) -> Result<HostEntry> {
        let mut hosts = self.hosts()?;
        let replaced = |h: &HostEntry| h.name == entry.name || h.addr == entry.addr;
        if entry.id == 0
            && let Some(old) = hosts.iter().find(|h| replaced(h))
        {
            entry.id = old.id;
        }
        hosts.retain(|h| !replaced(h));
        hosts.push(entry);
        number(&mut hosts);
        self.write_hosts(&hosts)?;
        Ok(hosts.pop().unwrap())
    }

    /// Forgets a paired host, by name or id.
    pub fn remove_host(&self, which: &str) -> Result<HostEntry> {
        let mut hosts = self.hosts()?;
        let i = find(&hosts, which)?;
        let removed = hosts.remove(i);
        self.write_hosts(&hosts)?;
        let mut cfg = self.config()?;
        if cfg.selected_host.as_deref() == Some(removed.fingerprint.as_str()) {
            // Keep the same Mac selected through another of its users, if one is paired.
            cfg.selected_host = hosts.iter().find(|h| removed.same_mac(h)).map(|h| h.fingerprint.clone());
            self.save_config(&cfg)?;
        }
        Ok(removed)
    }

    /// Records which Mac user and machine an agent reported, if that changed.
    pub fn learn(&self, fingerprint: &str, user: &str, machine: &str) -> Result<()> {
        let mut hosts = self.hosts()?;
        let Some(h) = hosts.iter_mut().find(|h| h.fingerprint == fingerprint) else { return Ok(()) };
        let (user, machine) = (Some(user.to_owned()), (!machine.is_empty()).then(|| machine.to_owned()));
        if h.user == user && h.machine == machine {
            return Ok(());
        }
        (h.user, h.machine) = (user, machine);
        self.write_hosts(&hosts)?;
        Ok(())
    }

    /// Picks the named host; without a name the selected host, else the only paired host.
    pub fn host(&self, name: Option<&str>) -> Result<HostEntry> {
        let hosts = self.hosts()?;
        let selected = self.config()?.selected_host;
        match name {
            Some(n) => Ok(hosts[find(&hosts, n)?].clone()),
            None if let Some(h) = selected.and_then(|fp| hosts.iter().find(|h| h.fingerprint == fp)) => Ok(h.clone()),
            None if hosts.len() == 1 => Ok(hosts.into_iter().next().unwrap()),
            None if hosts.is_empty() => bail!("no paired host; run `umac pair <link>` first"),
            None => bail!("several hosts paired; pick one with --host"),
        }
    }
}

/// Index of the host `which` names (name, id or fingerprint prefix).
fn find(hosts: &[HostEntry], which: &str) -> Result<usize> {
    let found: Vec<usize> = hosts.iter().enumerate().filter(|(_, h)| h.is(which)).map(|(i, _)| i).collect();
    match found.as_slice() {
        [i] => Ok(*i),
        [] => bail!("no paired host {which} (see `umac hosts`)"),
        _ => bail!("{which} matches several hosts; use its id from `umac hosts`"),
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

    fn agent(name: &str, fp: &str, user: Option<&str>, machine: Option<&str>) -> HostEntry {
        HostEntry {
            id: 0,
            name: name.into(),
            addr: "192.0.2.10:47810".into(),
            fingerprint: fp.into(),
            user: user.map(Into::into),
            machine: machine.map(Into::into),
        }
    }

    #[test]
    fn users_of_one_mac_form_a_group() {
        let ana = agent("ana@Mac mini", "aa", Some("ana"), Some("m1"));
        let bo = agent("bo@Mac mini", "bb", Some("bo"), Some("m1"));
        let other = agent("studio", "cc", Some("ana"), Some("m2"));
        let old = agent("Mac mini", "dd", None, None);
        let hosts = [ana.clone(), other.clone(), bo.clone(), old.clone()];
        assert_eq!(mac_group(&hosts, &bo), vec![bo.clone(), ana.clone()]);
        assert_eq!(mac_group(&hosts, &other), vec![other.clone()]);
        // Not learned yet: alone.
        assert_eq!(mac_group(&hosts, &old), vec![old.clone()]);
        assert!(!agent("x", "x", None, Some("")).same_mac(&agent("y", "y", None, Some(""))));
        assert_eq!(ana.mac_name(), "Mac mini");
        assert_eq!(other.mac_name(), "studio");
        assert_eq!(old.mac_name(), "Mac mini");
    }

    #[test]
    fn learning_the_user_keeps_the_entry() {
        let dir = std::env::temp_dir().join(format!("umac-client-learn-{}", std::process::id()));
        let store = Store::new(Some(dir.clone())).unwrap();
        let e = store.save_host(HostEntry { name: "mac-a".into(), addr: "1.2.3.4:5".into(), fingerprint: FP.into(), ..Default::default() }).unwrap();
        store.learn(FP, "ana", "m1").unwrap();
        let got = store.host(None).unwrap();
        assert_eq!((got.user.as_deref(), got.machine.as_deref()), (Some("ana"), Some("m1")));
        assert_eq!(HostEntry { user: None, machine: None, ..got }, e);
        // Old files without the new fields still parse.
        fs::write(dir.join("hosts.json"), format!(r#"[{{"name":"m","addr":"a:1","fingerprint":"{FP}"}}]"#)).unwrap();
        assert_eq!(store.host(None).unwrap().user, None);
        assert_eq!(store.host(None).unwrap().id, 1);
        assert!(fs::read_to_string(dir.join("hosts.json")).unwrap().contains("\"id\": 1"));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn unpairing_by_name_or_fingerprint() {
        let dir = std::env::temp_dir().join(format!("umac-client-unpair-{}", std::process::id()));
        let store = Store::new(Some(dir.clone())).unwrap();
        let fp2 = FP.replace("00", "11");
        let ana = HostEntry { name: "ana@mini".into(), fingerprint: FP.into(), machine: Some("m".into()), ..Default::default() };
        let bo = HostEntry { name: "bo@mini".into(), addr: "x:2".into(), fingerprint: fp2.clone(), machine: Some("m".into()), ..Default::default() };
        let ana = store.save_host(ana).unwrap();
        let bo = store.save_host(bo).unwrap();
        assert_eq!((ana.id, bo.id), (1, 2));
        store.select_host(FP).unwrap();
        assert!(store.remove_host("nobody").is_err());
        assert!(store.remove_host("3").is_err());
        assert!(store.remove_host(&FP[..4]).is_err(), "too short a prefix");
        assert_eq!(store.host(Some("2")).unwrap(), bo);
        assert_eq!(store.remove_host("ana@mini").unwrap(), ana);
        // Ids stay put when others go.
        assert_eq!(store.host(Some("2")).unwrap(), bo);
        // The Mac stays selected through its other user.
        assert_eq!(store.config().unwrap().selected_host.as_deref(), Some(fp2.as_str()));
        assert_eq!(store.remove_host(&fp2[..12]).unwrap(), bo);
        // A new pairing takes the smallest free id.
        assert_eq!(store.save_host(HostEntry { name: "c".into(), fingerprint: FP.into(), ..Default::default() }).unwrap().id, 1);
        store.remove_host("1").unwrap();
        assert!(store.hosts().unwrap().is_empty());
        assert_eq!(store.config().unwrap().selected_host, None);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn store_roundtrip() {
        let dir = std::env::temp_dir().join(format!("umac-client-store-{}", std::process::id()));
        let store = Store::new(Some(dir.clone())).unwrap();
        assert!(store.host(None).is_err());
        let e = HostEntry { name: "mac-a".into(), addr: "1.2.3.4:5".into(), fingerprint: FP.into(), ..Default::default() };
        let saved = store.save_host(e.clone()).unwrap();
        assert_eq!(saved, HostEntry { id: 1, ..e.clone() });
        // Pairing the same host again keeps its id.
        assert_eq!(store.save_host(e.clone()).unwrap().id, 1);
        assert_eq!(store.host(None).unwrap(), saved);
        assert_eq!(store.config().unwrap().bitrate_kbps, 25_000);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn selected_host_is_used_and_persisted() {
        let dir = std::env::temp_dir().join(format!("umac-client-select-{}", std::process::id()));
        let store = Store::new(Some(dir.clone())).unwrap();
        let fp2 = FP.replace("00", "11");
        let a = HostEntry { name: "a".into(), addr: "1.1.1.1:1".into(), fingerprint: FP.into(), ..Default::default() };
        let b = HostEntry { name: "b".into(), addr: "2.2.2.2:2".into(), fingerprint: fp2.clone(), ..Default::default() };
        let a = store.save_host(a).unwrap();
        let b = store.save_host(b).unwrap();
        assert!(store.host(None).is_err());
        store.select_host(&fp2).unwrap();
        assert_eq!(store.host(None).unwrap(), b);
        assert_eq!(store.host(Some("a")).unwrap(), a);
        // Survives a reload and does not clobber other settings.
        assert_eq!(Store::new(Some(dir.clone())).unwrap().config().unwrap().selected_host.as_deref(), Some(fp2.as_str()));
        assert_eq!(store.config().unwrap().max_fps, 60);
        // A stale selection falls back to the normal rules.
        store.select_host("ff").unwrap();
        assert!(store.host(None).is_err());
        fs::remove_dir_all(dir).unwrap();
    }
}
