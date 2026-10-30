//! When the local clipboard goes to the Mac. With `on_paste` it only leaves this computer for
//! a paste: Cmd+V typed into a streamed window, or a host request when a Paste menu item is
//! clicked. Input after Cmd+V waits until the clipboard was read so the Mac sees it in order.
#![allow(non_upper_case_globals)]

use umac_proto::{
    InputEvent, MAX_CLIPBOARD_BYTES,
    keys::{kVK_ANSI_V, kVK_Command, kVK_RightCommand},
};

use crate::config::ClipboardToMac;

/// Clipboard text to send, or `None` when it is empty, not text, or too large.
pub fn sendable(text: Option<String>) -> Option<String> {
    let text = text.filter(|t| !t.is_empty())?;
    if text.len() > MAX_CLIPBOARD_BYTES {
        tracing::info!(bytes = text.len(), "clipboard too large to send to the Mac");
        return None;
    }
    Some(text)
}

pub struct PasteFlow {
    mode: ClipboardToMac,
    command: [bool; 2],
    /// A paste (or focus push) is waiting for the clipboard; `queued` holds input behind it.
    pending: bool,
    queued: Vec<InputEvent>,
    requests: Vec<u64>,
}

impl PasteFlow {
    pub fn new(mode: ClipboardToMac) -> Self {
        Self { mode, command: [false; 2], pending: false, queued: Vec::new(), requests: Vec::new() }
    }

    /// Takes one outgoing input event and returns what to send now.
    pub fn input(&mut self, ev: InputEvent) -> Vec<InputEvent> {
        if let InputEvent::Key { keycode, pressed, .. } = ev {
            match keycode {
                kVK_Command => self.command[0] = pressed,
                kVK_RightCommand => self.command[1] = pressed,
                _ => {}
            }
        }
        if matches!(ev, InputEvent::ReleaseAll) {
            self.command = [false; 2];
        }
        if self.pending {
            self.queued.push(ev);
            return Vec::new();
        }
        let paste = matches!(ev, InputEvent::Key { keycode: kVK_ANSI_V, pressed: true, .. })
            && self.command.iter().any(|&c| c)
            && self.mode != ClipboardToMac::Off;
        if paste {
            self.pending = true;
            self.queued.push(ev);
            return Vec::new();
        }
        vec![ev]
    }

    /// A streamed window gained focus.
    pub fn focused(&mut self) {
        if self.mode == ClipboardToMac::Always {
            self.pending = true;
        }
    }

    /// The host asks for the clipboard; with sharing off it is declined right away.
    pub fn request(&mut self, nonce: u64) -> Vec<InputEvent> {
        if self.mode == ClipboardToMac::Off {
            return vec![InputEvent::SetClipboard { nonce: Some(nonce), text: None }];
        }
        self.requests.push(nonce);
        Vec::new()
    }

    /// True while the shell should read the local clipboard and call `read_done`.
    pub fn wants_read(&self) -> bool {
        self.pending || !self.requests.is_empty()
    }

    /// The clipboard was read (`None` if empty or unavailable): answers every waiting request
    /// and then releases the queued input, clipboard first.
    pub fn read_done(&mut self, text: Option<String>) -> Vec<InputEvent> {
        let text = sendable(text);
        let mut out: Vec<InputEvent> = self
            .requests
            .drain(..)
            .map(|n| InputEvent::SetClipboard { nonce: Some(n), text: text.clone() })
            .collect();
        if std::mem::take(&mut self.pending) {
            if text.is_some() {
                out.push(InputEvent::SetClipboard { nonce: None, text });
            }
            out.append(&mut self.queued);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use umac_proto::keys::{kVK_ANSI_A, kVK_Control};

    use super::*;

    fn key(keycode: u16, pressed: bool) -> InputEvent {
        InputEvent::Key { window: 1, keycode, pressed }
    }

    #[test]
    fn cmd_v_waits_for_clipboard_and_keeps_order() {
        let mut f = PasteFlow::new(ClipboardToMac::OnPaste);
        assert_eq!(f.input(key(kVK_Command, true)), vec![key(kVK_Command, true)]);
        assert!(f.input(key(kVK_ANSI_V, true)).is_empty());
        assert!(f.wants_read());
        assert!(f.input(key(kVK_ANSI_V, false)).is_empty());
        let out = f.read_done(Some("hi".into()));
        assert_eq!(
            out,
            vec![
                InputEvent::SetClipboard { nonce: None, text: Some("hi".into()) },
                key(kVK_ANSI_V, true),
                key(kVK_ANSI_V, false),
            ]
        );
        assert!(!f.wants_read());
        assert_eq!(f.input(key(kVK_ANSI_A, true)), vec![key(kVK_ANSI_A, true)]);
    }

    #[test]
    fn plain_v_and_control_v_are_not_pastes() {
        let mut f = PasteFlow::new(ClipboardToMac::OnPaste);
        assert_eq!(f.input(key(kVK_ANSI_V, true)).len(), 1);
        f.input(key(kVK_Control, true));
        assert_eq!(f.input(key(kVK_ANSI_V, true)).len(), 1);
        // Command released before V.
        f.input(key(kVK_RightCommand, true));
        f.input(key(kVK_RightCommand, false));
        assert_eq!(f.input(key(kVK_ANSI_V, true)).len(), 1);
        // Focus loss clears held modifiers.
        f.input(key(kVK_Command, true));
        f.input(InputEvent::ReleaseAll);
        assert_eq!(f.input(key(kVK_ANSI_V, true)).len(), 1);
    }

    #[test]
    fn off_mode_never_reads_and_declines_requests() {
        let mut f = PasteFlow::new(ClipboardToMac::Off);
        f.input(key(kVK_Command, true));
        assert_eq!(f.input(key(kVK_ANSI_V, true)).len(), 1);
        f.focused();
        assert!(!f.wants_read());
        assert_eq!(f.request(9), vec![InputEvent::SetClipboard { nonce: Some(9), text: None }]);
    }

    #[test]
    fn requests_and_always_mode() {
        let mut f = PasteFlow::new(ClipboardToMac::OnPaste);
        f.focused();
        assert!(!f.wants_read());
        assert!(f.request(4).is_empty());
        assert_eq!(f.read_done(None), vec![InputEvent::SetClipboard { nonce: Some(4), text: None }]);

        let mut f = PasteFlow::new(ClipboardToMac::Always);
        f.focused();
        assert!(f.wants_read());
        assert!(f.input(key(kVK_ANSI_A, true)).is_empty());
        let out = f.read_done(Some("x".into()));
        assert_eq!(out, vec![InputEvent::SetClipboard { nonce: None, text: Some("x".into()) }, key(kVK_ANSI_A, true)]);
    }

    #[test]
    fn oversized_or_empty_text_is_not_sent() {
        assert_eq!(sendable(Some(String::new())), None);
        assert_eq!(sendable(Some("a".repeat(MAX_CLIPBOARD_BYTES + 1))), None);
        assert_eq!(sendable(Some("ok".into())), Some("ok".into()));
    }
}
