//! System tray: a platform-neutral model of the menu and one backend per platform. Linux uses
//! StatusNotifierItem over D-Bus (`ksni`); another platform adds a backend with the same
//! `Tray` surface.

pub mod icon;
#[cfg(target_os = "linux")]
mod ksni;

use anyhow::Result;
use tokio::sync::mpsc::UnboundedSender;

use crate::config::HostEntry;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Apps,
    Terminal,
    Desktop,
    TaskManager,
    Settings,
    /// Fingerprint of the host to use from now on.
    SelectHost(String),
    Exit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostItem {
    pub label: String,
    pub fingerprint: String,
}

/// What the menu shows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Menu {
    pub hosts: Vec<HostItem>,
    /// Index into `hosts` of the host actions go to.
    pub selected: Option<usize>,
}

impl Menu {
    /// Menu for the paired hosts; `selected` is the host commands without `--host` use.
    pub fn new(hosts: &[HostEntry], selected: Option<&HostEntry>) -> Self {
        let items: Vec<HostItem> = hosts
            .iter()
            .map(|h| {
                let ip = h.addr.rsplit_once(':').map_or(h.addr.as_str(), |(ip, _)| ip);
                HostItem { label: format!("{} ({ip})", h.name), fingerprint: h.fingerprint.clone() }
            })
            .collect();
        let selected = selected.and_then(|s| items.iter().position(|i| i.fingerprint == s.fingerprint));
        Self { hosts: items, selected }
    }

    pub fn selected_label(&self) -> Option<&str> {
        self.selected.and_then(|i| self.hosts.get(i)).map(|h| h.label.as_str())
    }
}

/// A running tray icon. Menu clicks arrive on the `actions` channel given to `spawn`.
pub struct Tray {
    #[cfg(target_os = "linux")]
    inner: ksni::Handle,
}

impl Tray {
    pub async fn spawn(icon: Vec<icon::Pixmap>, menu: Menu, actions: UnboundedSender<Action>) -> Result<Self> {
        #[cfg(target_os = "linux")]
        return Ok(Self { inner: ksni::spawn(icon, menu, actions).await? });
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (icon, menu, actions);
            anyhow::bail!("the tray is not supported on this platform yet")
        }
    }

    pub async fn set_menu(&self, menu: Menu) {
        #[cfg(target_os = "linux")]
        self.inner.set_menu(menu).await;
        #[cfg(not(target_os = "linux"))]
        let _ = menu;
    }

    pub async fn shutdown(self) {
        #[cfg(target_os = "linux")]
        self.inner.shutdown().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(name: &str, addr: &str, fp: &str) -> HostEntry {
        HostEntry { name: name.into(), addr: addr.into(), fingerprint: fp.into() }
    }

    #[test]
    fn menu_labels_and_selection() {
        let hosts = [host("mac-a", "192.0.2.10:47810", "aa"), host("studio", "[fe80::1]:47810", "bb")];
        let m = Menu::new(&hosts, Some(&hosts[1]));
        assert_eq!(m.hosts[0].label, "mac-a (192.0.2.10)");
        assert_eq!(m.hosts[1].label, "studio ([fe80::1])");
        assert_eq!(m.selected, Some(1));
        assert_eq!(m.selected_label(), Some("studio ([fe80::1])"));
        assert_eq!(Menu::new(&hosts, None).selected, None);
    }
}
