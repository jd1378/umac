//! Which session streams each Mac window, per paired client. Every tray action or CLI command
//! is its own client process and session, so without this two of them could show the same Mac
//! window twice. A window belongs to at most one session of a client: an explicit request moves
//! it, automatic following skips it, and only the client's most recently active session follows
//! the Mac's focus. Different clients stay independent.

use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use umac_proto::WindowId;

/// How long a window returned by `Launch` is kept for the session that will subscribe to it.
/// `umac launch` asks on one connection and streams on a new one; in between no other session
/// of the client may follow the window.
pub const LAUNCH_HOLD: Duration = Duration::from_secs(5);

/// A paired client, by certificate fingerprint.
pub type ClientId = [u8; 32];
pub type SessionId = u64;

#[derive(Default)]
pub struct Owners {
    windows: HashMap<(ClientId, WindowId), SessionId>,
    /// Sessions per client, least recently active first.
    sessions: HashMap<ClientId, Vec<SessionId>>,
    /// Windows handed out by a launch, until the subscribing session claims them.
    held: HashMap<(ClientId, WindowId), Instant>,
}

impl Owners {
    /// A new session starts as the client's active one: it was opened to show something.
    pub fn open(&mut self, client: ClientId, session: SessionId) {
        self.touch(client, session);
    }

    /// The session received input or a request, so it is the one the user works in.
    pub fn touch(&mut self, client: ClientId, session: SessionId) {
        let list = self.sessions.entry(client).or_default();
        list.retain(|&s| s != session);
        list.push(session);
    }

    pub fn close(&mut self, client: ClientId, session: SessionId) {
        self.windows.retain(|&(c, _), s| c != client || *s != session);
        if let Some(list) = self.sessions.get_mut(&client) {
            list.retain(|&s| s != session);
            if list.is_empty() {
                self.sessions.remove(&client);
            }
        }
    }

    /// Claims `window` for `session`; returns the other session that must let go of it.
    pub fn claim(&mut self, client: ClientId, window: WindowId, session: SessionId) -> Option<SessionId> {
        self.held.remove(&(client, window));
        self.windows.insert((client, window), session).filter(|&s| s != session)
    }

    pub fn release(&mut self, client: ClientId, window: WindowId, session: SessionId) {
        if self.windows.get(&(client, window)) == Some(&session) {
            self.windows.remove(&(client, window));
        }
    }

    /// Keeps `window` from automatic following until `until`, for the session a launch serves.
    pub fn hold(&mut self, client: ClientId, window: WindowId, until: Instant) {
        let now = Instant::now();
        self.held.retain(|_, t| *t > now);
        self.held.insert((client, window), until);
    }

    /// Another session of the same client already shows `window`, or a launch reserved it;
    /// automatic following skips it.
    pub fn taken_elsewhere(&self, client: ClientId, window: WindowId, session: SessionId) -> bool {
        self.windows.get(&(client, window)).is_some_and(|&s| s != session)
            || self.held.get(&(client, window)).is_some_and(|&t| t > Instant::now())
    }

    pub fn follows_focus(&self, client: ClientId, session: SessionId) -> bool {
        self.sessions.get(&client).and_then(|l| l.last()) == Some(&session)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: ClientId = [1; 32];
    const B: ClientId = [2; 32];

    #[test]
    fn explicit_claim_moves_the_window() {
        let mut o = Owners::default();
        assert_eq!(o.claim(A, 7, 1), None);
        assert_eq!(o.claim(A, 7, 1), None);
        assert_eq!(o.claim(A, 7, 2), Some(1));
        assert!(o.taken_elsewhere(A, 7, 1));
        assert!(!o.taken_elsewhere(A, 7, 2));
        // The old session's late release must not drop the new owner.
        o.release(A, 7, 1);
        assert!(o.taken_elsewhere(A, 7, 1));
        o.release(A, 7, 2);
        assert!(!o.taken_elsewhere(A, 7, 1));
    }

    #[test]
    fn clients_are_independent() {
        let mut o = Owners::default();
        o.claim(A, 7, 1);
        assert_eq!(o.claim(B, 7, 2), None);
        assert!(!o.taken_elsewhere(A, 7, 1));
        assert!(o.taken_elsewhere(B, 7, 1));
    }

    #[test]
    fn focus_follows_only_the_active_session() {
        let mut o = Owners::default();
        o.open(A, 1);
        o.open(A, 2);
        o.open(B, 3);
        assert!(o.follows_focus(A, 2) && !o.follows_focus(A, 1));
        assert!(o.follows_focus(B, 3));
        o.touch(A, 1);
        assert!(o.follows_focus(A, 1) && !o.follows_focus(A, 2));
        // When the active one ends, the previous one takes over.
        o.close(A, 1);
        assert!(o.follows_focus(A, 2));
    }

    #[test]
    fn launched_window_is_held_until_claimed() {
        let mut o = Owners::default();
        o.hold(A, 9, Instant::now() + LAUNCH_HOLD);
        assert!(o.taken_elsewhere(A, 9, 1) && o.taken_elsewhere(A, 9, 2));
        assert!(!o.taken_elsewhere(B, 9, 1));
        assert_eq!(o.claim(A, 9, 2), None);
        assert!(o.taken_elsewhere(A, 9, 1) && !o.taken_elsewhere(A, 9, 2));
        // An expired hold no longer blocks.
        o.hold(A, 10, Instant::now());
        assert!(!o.taken_elsewhere(A, 10, 1));
    }

    #[test]
    fn closing_a_session_frees_its_windows() {
        let mut o = Owners::default();
        o.open(A, 1);
        o.claim(A, 7, 1);
        o.claim(A, 8, 1);
        o.close(A, 1);
        assert!(!o.taken_elsewhere(A, 7, 2) && !o.taken_elsewhere(A, 8, 2));
        assert!(!o.follows_focus(A, 1));
    }
}
