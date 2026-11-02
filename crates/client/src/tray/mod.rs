//! System tray: a platform-neutral model of the menu and one backend per platform. Linux uses
//! StatusNotifierItem over D-Bus (`ksni`); another platform adds a backend with the same
//! `Tray` surface.

pub mod icon;
#[cfg(target_os = "linux")]
mod ksni;

use anyhow::Result;
use tokio::sync::mpsc::UnboundedSender;

use std::collections::HashMap;

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

/// One paired Mac. Its users' agents share one item: actions go to whichever is on screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostItem {
    pub label: String,
    /// The selected agent of this Mac if it has the selection, else its first one.
    pub fingerprint: String,
}

/// What the menu shows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Menu {
    pub hosts: Vec<HostItem>,
    /// Index into `hosts` of the Mac actions go to.
    pub selected: Option<usize>,
    /// Who is on the selected Mac's screen, when known.
    pub screen: Option<String>,
    /// The selected Mac's paired users, when it has several; actions prefer the selected one.
    pub users: Vec<HostItem>,
    pub selected_user: Option<usize>,
}

impl Menu {
    /// Menu for the paired Macs; `selected` is the host commands without `--host` use,
    /// `screen` the line saying who is on its screen and `streamable` which of its agents can
    /// stream now (by fingerprint; unknown ones are missing).
    pub fn new(hosts: &[HostEntry], selected: Option<&HostEntry>, screen: Option<String>, streamable: &HashMap<String, bool>) -> Self {
        let mut macs: Vec<Vec<&HostEntry>> = Vec::new();
        for h in hosts {
            match macs.iter_mut().find(|users| users[0].same_mac(h)) {
                Some(users) => users.push(h),
                None => macs.push(vec![h]),
            }
        }
        let mut items = Vec::with_capacity(macs.len());
        let mut selected_item = None;
        for (i, users) in macs.iter().enumerate() {
            let chosen = users.iter().find(|h| selected.is_some_and(|s| s.fingerprint == h.fingerprint));
            if chosen.is_some() {
                selected_item = Some(i);
            }
            let first = users[0];
            let ip = first.addr.rsplit_once(':').map_or(first.addr.as_str(), |(ip, _)| ip);
            items.push(HostItem { label: format!("{} ({ip})", first.mac_name()), fingerprint: chosen.unwrap_or(&first).fingerprint.clone() });
        }
        let mut users = Vec::new();
        let mut selected_user = None;
        if let Some(sel) = selected {
            let group: Vec<&HostEntry> = hosts.iter().filter(|h| sel.same_mac(h)).collect();
            if group.len() > 1 {
                for h in group {
                    let user = h.user.as_deref().unwrap_or(&h.name);
                    let label = match streamable.get(&h.fingerprint) {
                        Some(true) => format!("{user} (on screen)"),
                        Some(false) => format!("{user} (not on screen)"),
                        None => user.to_owned(),
                    };
                    if h.fingerprint == sel.fingerprint {
                        selected_user = Some(users.len());
                    }
                    users.push(HostItem { label, fingerprint: h.fingerprint.clone() });
                }
            }
        }
        Self { hosts: items, selected: selected_item, screen, users, selected_user }
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
        HostEntry { name: name.into(), addr: addr.into(), fingerprint: fp.into(), ..Default::default() }
    }

    fn user_of(mac: &str, user: &str, port: u16, fp: &str) -> HostEntry {
        HostEntry {
            id: 0,
            name: format!("{user}@Mac mini"),
            addr: format!("192.0.2.10:{port}"),
            fingerprint: fp.into(),
            user: Some(user.into()),
            machine: Some(mac.into()),
        }
    }

    #[test]
    fn menu_labels_and_selection() {
        let hosts = [host("mac-a", "192.0.2.10:47810", "aa"), host("studio", "[fe80::1]:47810", "bb")];
        let m = Menu::new(&hosts, Some(&hosts[1]), None, &HashMap::new());
        assert_eq!(m.hosts[0].label, "mac-a (192.0.2.10)");
        assert_eq!(m.hosts[1].label, "studio ([fe80::1])");
        assert_eq!(m.selected, Some(1));
        assert_eq!(m.selected_label(), Some("studio ([fe80::1])"));
        assert_eq!(Menu::new(&hosts, None, None, &HashMap::new()).selected, None);
        assert!(m.users.is_empty(), "one user: no user choice");
    }

    #[test]
    fn users_of_one_mac_share_an_item() {
        let hosts = [user_of("m1", "ana", 47810, "aa"), host("studio", "192.0.2.20:47810", "cc"), user_of("m1", "bo", 47811, "bb")];
        let streamable = HashMap::from([("aa".to_owned(), true), ("bb".to_owned(), false)]);
        let m = Menu::new(&hosts, Some(&hosts[2]), Some("On screen: bo".into()), &streamable);
        assert_eq!(m.hosts.len(), 2);
        let labels: Vec<&str> = m.users.iter().map(|u| u.label.as_str()).collect();
        assert_eq!(labels, ["ana (on screen)", "bo (not on screen)"]);
        assert_eq!(m.selected_user, Some(1));
        assert_eq!(m.hosts[0], HostItem { label: "Mac mini (192.0.2.10)".into(), fingerprint: "bb".into() });
        assert_eq!(m.selected, Some(0));
        assert_eq!(m.screen.as_deref(), Some("On screen: bo"));
        let m = Menu::new(&hosts, Some(&hosts[1]), None, &HashMap::new());
        assert_eq!(m.hosts[0].fingerprint, "aa");
        assert!(m.users.is_empty());
        assert_eq!(m.selected, Some(1));
    }
}
