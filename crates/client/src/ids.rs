//! Local window ids. Every Mac window shown here gets an id unique across all connected hosts
//! (two Macs can use the same `CGWindowID`); the UI only sees local ids and the router maps
//! them back to (host, Mac id) for messages going out.

use std::{collections::HashMap, sync::Arc};

use umac_proto::{ClientMsg, InputEvent, WindowId, WindowKind};

/// A paired host, by certificate fingerprint.
pub type HostKey = Arc<str>;

#[derive(Debug, Clone, PartialEq)]
pub struct Shown {
    pub host: HostKey,
    pub mac: WindowId,
    pub kind: WindowKind,
    pub title: String,
}

#[derive(Debug, Default)]
pub struct IdMap {
    next: WindowId,
    local: HashMap<WindowId, Shown>,
    mac: HashMap<(HostKey, WindowId), WindowId>,
    /// Clipboard requests in flight: which host asked.
    nonces: HashMap<u64, HostKey>,
    /// Host of the last window input went to: where a Cmd+V's clipboard belongs.
    last_input: Option<HostKey>,
}

impl IdMap {
    /// The local id for a Mac window, allocated on first use.
    pub fn local(&mut self, host: &HostKey, mac: WindowId, kind: WindowKind, title: &str) -> WindowId {
        if let Some(&id) = self.mac.get(&(host.clone(), mac)) {
            if let Some(s) = self.local.get_mut(&id) {
                s.title = title.to_owned();
            }
            return id;
        }
        loop {
            self.next = self.next.wrapping_add(1).max(1);
            if !self.local.contains_key(&self.next) {
                break;
            }
        }
        let id = self.next;
        self.local.insert(id, Shown { host: host.clone(), mac, kind, title: title.to_owned() });
        self.mac.insert((host.clone(), mac), id);
        id
    }

    pub fn find(&self, host: &HostKey, mac: WindowId) -> Option<WindowId> {
        self.mac.get(&(host.clone(), mac)).copied()
    }

    pub fn resolve(&self, local: WindowId) -> Option<&Shown> {
        self.local.get(&local)
    }

    pub fn set_title(&mut self, local: WindowId, title: &str) {
        if let Some(s) = self.local.get_mut(&local) {
            s.title = title.to_owned();
        }
    }

    pub fn release(&mut self, host: &HostKey, mac: WindowId) -> Option<WindowId> {
        let id = self.mac.remove(&(host.clone(), mac))?;
        self.local.remove(&id);
        Some(id)
    }

    /// Forgets every window of `host`; returns their local ids.
    pub fn release_host(&mut self, host: &HostKey) -> Vec<WindowId> {
        let ids: Vec<WindowId> = self.local.iter().filter(|(_, s)| &s.host == host).map(|(&id, _)| id).collect();
        for id in &ids {
            if let Some(s) = self.local.remove(id) {
                self.mac.remove(&(s.host, s.mac));
            }
        }
        self.nonces.retain(|_, h| h != host);
        if self.last_input.as_ref() == Some(host) {
            self.last_input = None;
        }
        ids
    }

    pub fn shown(&self) -> impl Iterator<Item = (WindowId, &Shown)> {
        self.local.iter().map(|(&id, s)| (id, s))
    }

    pub fn clipboard_request(&mut self, host: &HostKey, nonce: u64) {
        self.nonces.insert(nonce, host.clone());
    }

    /// Where a message from the UI goes, rewritten to the host's window id.
    pub fn route_control(&self, msg: ClientMsg) -> Option<(HostKey, ClientMsg)> {
        let window = control_window(&msg)?;
        let s = self.local.get(&window)?;
        Some((s.host.clone(), with_control_window(msg, s.mac)))
    }

    /// Where an input event goes. Window events go to their window's host; a release of
    /// everything goes to every host (`None`); a clipboard answer to whoever asked, or for
    /// a Cmd+V to the host the last input went to.
    pub fn route_input(&mut self, ev: InputEvent) -> Route {
        if let Some(window) = input_window(&ev) {
            let Some(s) = self.local.get(&window) else { return Route::Drop };
            self.last_input = Some(s.host.clone());
            return Route::To(s.host.clone(), with_input_window(ev, s.mac));
        }
        match ev {
            InputEvent::ReleaseAll => Route::All(ev),
            InputEvent::SetClipboard { nonce: Some(n), .. } => match self.nonces.remove(&n) {
                Some(host) => Route::To(host, ev),
                None => Route::Drop,
            },
            InputEvent::SetClipboard { nonce: None, .. } => match &self.last_input {
                Some(host) => Route::To(host.clone(), ev),
                None => Route::Drop,
            },
            _ => Route::Drop,
        }
    }
}

#[derive(Debug, PartialEq)]
pub enum Route {
    To(HostKey, InputEvent),
    All(InputEvent),
    Drop,
}

fn control_window(msg: &ClientMsg) -> Option<WindowId> {
    match *msg {
        ClientMsg::Subscribe { window, .. }
        | ClientMsg::Unsubscribe { window }
        | ClientMsg::Resize { window, .. }
        | ClientMsg::Focus { window }
        | ClientMsg::RequestKeyframe { window }
        | ClientMsg::CloseWindow { window } => Some(window),
        _ => None,
    }
}

fn with_control_window(msg: ClientMsg, w: WindowId) -> ClientMsg {
    match msg {
        ClientMsg::Subscribe { max_fps, bitrate_kbps, .. } => ClientMsg::Subscribe { window: w, max_fps, bitrate_kbps },
        ClientMsg::Unsubscribe { .. } => ClientMsg::Unsubscribe { window: w },
        ClientMsg::Resize { width, height, .. } => ClientMsg::Resize { window: w, width, height },
        ClientMsg::Focus { .. } => ClientMsg::Focus { window: w },
        ClientMsg::RequestKeyframe { .. } => ClientMsg::RequestKeyframe { window: w },
        ClientMsg::CloseWindow { .. } => ClientMsg::CloseWindow { window: w },
        other => other,
    }
}

fn input_window(ev: &InputEvent) -> Option<WindowId> {
    match *ev {
        InputEvent::MouseMove { window, .. }
        | InputEvent::MouseButton { window, .. }
        | InputEvent::Scroll { window, .. }
        | InputEvent::Key { window, .. } => Some(window),
        _ => None,
    }
}

fn with_input_window(ev: InputEvent, w: WindowId) -> InputEvent {
    match ev {
        InputEvent::MouseMove { x, y, .. } => InputEvent::MouseMove { window: w, x, y },
        InputEvent::MouseButton { x, y, button, pressed, .. } => InputEvent::MouseButton { window: w, x, y, button, pressed },
        InputEvent::Scroll { dx, dy, precise, .. } => InputEvent::Scroll { window: w, dx, dy, precise },
        InputEvent::Key { keycode, pressed, .. } => InputEvent::Key { window: w, keycode, pressed },
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(s: &str) -> HostKey {
        HostKey::from(s)
    }

    #[test]
    fn same_mac_id_on_two_hosts_gets_two_local_ids() {
        let mut m = IdMap::default();
        let a = m.local(&key("a"), 7, WindowKind::Normal, "A");
        let b = m.local(&key("b"), 7, WindowKind::Normal, "B");
        assert_ne!(a, b);
        assert_eq!(m.local(&key("a"), 7, WindowKind::Normal, "A2"), a);
        assert_eq!(m.resolve(a).unwrap().title, "A2");
        assert_eq!(m.find(&key("b"), 7), Some(b));
        assert_eq!(m.release(&key("a"), 7), Some(a));
        assert!(m.resolve(a).is_none());
        assert_eq!(m.release_host(&key("b")), vec![b]);
        assert_eq!(m.shown().count(), 0);
    }

    #[test]
    fn routes_and_rewrites_control_messages() {
        let mut m = IdMap::default();
        let id = m.local(&key("a"), 42, WindowKind::Normal, "");
        let (host, msg) = m.route_control(ClientMsg::CloseWindow { window: id }).unwrap();
        assert_eq!((&*host, msg), ("a", ClientMsg::CloseWindow { window: 42 }));
        assert!(m.route_control(ClientMsg::Focus { window: 999 }).is_none());
        assert!(m.route_control(ClientMsg::ListWindows).is_none());
    }

    #[test]
    fn clipboard_answers_go_to_the_right_host() {
        let mut m = IdMap::default();
        let a = m.local(&key("a"), 1, WindowKind::Normal, "");
        m.local(&key("b"), 1, WindowKind::Normal, "");
        m.clipboard_request(&key("b"), 9);
        let answer = InputEvent::SetClipboard { nonce: Some(9), text: None };
        assert_eq!(m.route_input(answer.clone()), Route::To(key("b"), answer.clone()));
        assert_eq!(m.route_input(answer), Route::Drop, "answered once");
        // Cmd+V: the clipboard follows the keys' host.
        let paste = InputEvent::SetClipboard { nonce: None, text: Some("x".into()) };
        assert_eq!(m.route_input(paste.clone()), Route::Drop);
        let k = InputEvent::Key { window: a, keycode: 9, pressed: true };
        assert_eq!(m.route_input(k), Route::To(key("a"), InputEvent::Key { window: 1, keycode: 9, pressed: true }));
        assert_eq!(m.route_input(paste.clone()), Route::To(key("a"), paste));
        assert_eq!(m.route_input(InputEvent::ReleaseAll), Route::All(InputEvent::ReleaseAll));
    }
}
