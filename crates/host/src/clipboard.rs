//! Platform-free clipboard logic: recognising a Paste menu item, holding input until the
//! client's clipboard arrives, and when to look at the Mac pasteboard.

use std::{
    collections::{HashMap, VecDeque},
    time::{Duration, Instant},
};

/// How long input is held waiting for the client's clipboard before giving up.
pub const PASTE_WAIT: Duration = Duration::from_millis(400);
/// Events held while waiting; beyond this the hold is released early.
const MAX_HELD: usize = 256;
/// Pasteboard checks after the last injected input: copies land shortly after the keystroke
/// or menu click that caused them, sometimes after the app does some work first.
pub const CHECK_AFTER_INPUT: [Duration; 3] =
    [Duration::from_millis(50), Duration::from_millis(300), Duration::from_millis(1000)];
/// Fallback for copies not caused by our input (scripts, timers, other devices), only while
/// a client has a streamed window focused.
pub const FALLBACK_CHECK: Duration = Duration::from_secs(3);

/// A menu item as Accessibility reports it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MenuItem {
    pub title: String,
    /// `AXMenuItemCmdChar`: the shortcut key, shown only in menu-bar menus.
    pub cmd_char: Option<String>,
    /// `AXMenuItemCmdModifiers`: kAXMenuItemModifier bits, 0 = Command alone, bit 3 = no Command.
    pub modifiers: Option<i64>,
}

impl MenuItem {
    /// Cmd+V, plus variants like Paste and Match Style (Cmd+Option+Shift+V).
    pub fn has_paste_shortcut(&self) -> bool {
        const NO_COMMAND: i64 = 1 << 3;
        self.cmd_char.as_deref().is_some_and(|c| c.eq_ignore_ascii_case("v")) && self.modifiers.unwrap_or(0) & NO_COMMAND == 0
    }
}

/// Whether clicking `item` pastes. Context menus show no shortcuts, so their items are
/// matched by title against the app's menu-bar items that have the Cmd+V shortcut. Both come
/// from the same app in the same language, so this works for any UI language.
pub fn is_paste_item(item: &MenuItem, paste_titles: &[String]) -> bool {
    item.has_paste_shortcut() || (!item.title.is_empty() && paste_titles.contains(&item.title))
}

/// Pasteboard change seen by the watcher: send it unless nothing changed or we wrote it.
pub fn should_send(count: isize, last_seen: isize, own_write: isize) -> bool {
    count != last_seen && count != own_write
}

/// When the watcher should next read the pasteboard change count; `None` means only new
/// input or a focus change can schedule a check.
pub fn next_check(last_input: Option<Instant>, last_check: Instant, focused: bool) -> Option<Instant> {
    let fallback = focused.then_some(last_check + FALLBACK_CHECK);
    last_input
        .into_iter()
        .flat_map(|t| CHECK_AFTER_INPUT.iter().map(move |d| t + *d))
        .filter(|&t| t > last_check)
        .chain(fallback)
        .min()
}

/// Which session currently has a streamed window focused on its client.
#[derive(Debug, Default)]
pub struct FocusTracker(HashMap<u64, u32>);

impl FocusTracker {
    pub fn focus(&mut self, session: u64, window: u32) {
        self.0.insert(session, window);
    }

    /// Focus left the client (ReleaseAll) or the session ended.
    pub fn unfocus(&mut self, session: u64) {
        self.0.remove(&session);
    }

    /// A window stopped streaming; it can no longer be the focused one.
    pub fn window_gone(&mut self, session: u64, window: u32) {
        if self.0.get(&session) == Some(&window) {
            self.0.remove(&session);
        }
    }

    pub fn any(&self) -> bool {
        !self.0.is_empty()
    }
}

/// Input held back while the client's clipboard is fetched for a paste.
pub struct PasteGate<T> {
    waiting: Option<(u64, Instant)>,
    held: VecDeque<T>,
}

impl<T> Default for PasteGate<T> {
    fn default() -> Self {
        Self { waiting: None, held: VecDeque::new() }
    }
}

impl<T> PasteGate<T> {
    /// Starts holding input until `resolve(nonce)` or the deadline.
    pub fn hold(&mut self, nonce: u64, trigger: T, now: Instant) {
        self.waiting = Some((nonce, now + PASTE_WAIT));
        self.held.push_back(trigger);
    }

    pub fn deadline(&self) -> Option<Instant> {
        self.waiting.map(|(_, d)| d)
    }

    pub fn is_waiting(&self) -> bool {
        self.waiting.is_some()
    }

    pub fn waits_for(&self, nonce: u64) -> bool {
        self.waiting.is_some_and(|(n, _)| n == nonce)
    }

    /// Queues an event behind the held paste. Returns everything held if the queue is full,
    /// so a stuck client can never swallow input.
    pub fn push(&mut self, ev: T) -> Option<Vec<T>> {
        self.held.push_back(ev);
        (self.held.len() >= MAX_HELD).then(|| self.release())
    }

    /// Ends the hold; the caller replays the returned events in order.
    pub fn release(&mut self) -> Vec<T> {
        self.waiting = None;
        self.held.drain(..).collect()
    }

    /// Releases the hold if its deadline passed.
    pub fn expire(&mut self, now: Instant) -> Option<Vec<T>> {
        self.deadline().is_some_and(|d| now >= d).then(|| self.release())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(title: &str, ch: Option<&str>, mods: Option<i64>) -> MenuItem {
        MenuItem { title: title.into(), cmd_char: ch.map(Into::into), modifiers: mods }
    }

    #[test]
    fn paste_items() {
        let titles = vec!["Paste".to_owned(), "Paste and Match Style".to_owned()];
        // Menu-bar items carry the shortcut.
        assert!(is_paste_item(&item("Einsetzen", Some("V"), Some(0)), &[]));
        assert!(is_paste_item(&item("x", Some("v"), Some(1 | 2)), &[]));
        assert!(!is_paste_item(&item("x", Some("V"), Some(8)), &[]));
        assert!(!is_paste_item(&item("Copy", Some("C"), Some(0)), &titles));
        // Context-menu items (as Terminal reports them) match by title.
        assert!(is_paste_item(&item("Paste", None, Some(0)), &titles));
        assert!(!is_paste_item(&item("Copy", None, Some(0)), &titles));
        assert!(!is_paste_item(&item("", None, Some(0)), &["".to_owned()]));
    }

    #[test]
    fn echo_suppression() {
        assert!(should_send(5, 4, 3));
        assert!(!should_send(5, 5, 3));
        assert!(!should_send(5, 4, 5));
    }

    #[test]
    fn check_schedule_after_input_then_fallback() {
        let t0 = Instant::now();
        assert_eq!(next_check(None, t0, true), Some(t0 + FALLBACK_CHECK));
        assert_eq!(next_check(None, t0, false), None);
        let input = t0 + Duration::from_millis(10);
        assert_eq!(next_check(Some(input), t0, false), Some(input + CHECK_AFTER_INPUT[0]));
        assert_eq!(next_check(Some(input), input + CHECK_AFTER_INPUT[0], true), Some(input + CHECK_AFTER_INPUT[1]));
        let after_all = input + CHECK_AFTER_INPUT[2];
        assert_eq!(next_check(Some(input), after_all, true), Some(after_all + FALLBACK_CHECK));
        assert_eq!(next_check(Some(input), after_all, false), None);
    }

    #[test]
    fn focus_tracking() {
        let mut f = FocusTracker::default();
        assert!(!f.any());
        f.focus(1, 10);
        f.window_gone(1, 11);
        assert!(f.any());
        f.window_gone(1, 10);
        assert!(!f.any());
        f.focus(1, 10);
        f.focus(2, 20);
        f.unfocus(1);
        assert!(f.any());
        f.unfocus(2);
        assert!(!f.any());
    }

    #[test]
    fn gate_holds_in_order_and_expires() {
        let t0 = Instant::now();
        let mut g = PasteGate::default();
        g.hold(7, 1, t0);
        assert!(g.waits_for(7) && !g.waits_for(8));
        assert!(g.push(2).is_none());
        assert_eq!(g.expire(t0), None);
        assert_eq!(g.expire(t0 + PASTE_WAIT), Some(vec![1, 2]));
        assert!(!g.is_waiting());
    }

    #[test]
    fn gate_releases_when_full() {
        let mut g = PasteGate::default();
        g.hold(1, 0, Instant::now());
        let released = (1..MAX_HELD).find_map(|i| g.push(i)).unwrap();
        assert_eq!(released.len(), MAX_HELD);
        assert!(!g.is_waiting());
    }
}
