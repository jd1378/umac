//! Platform-free cursor sync logic: when to look at the Mac cursor, how a cursor image is
//! recognised as a standard shape, and what each session still needs to be sent.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    hash::{Hash, Hasher},
    sync::Arc,
    time::{Duration, Instant},
};

use umac_proto::{Cursor, CursorShape, HostMsg, MAX_CURSOR_SIDE, NamedCursor};

/// Checks after the last injected pointer event: apps update the cursor on the next event
/// loop turn, a few take a frame or two longer.
pub const CHECK_AFTER_POINTER: [Duration; 2] = [Duration::from_millis(16), Duration::from_millis(80)];
/// Fallback for cursor changes not caused by our pointer events (a page finishing loading
/// under a still pointer), only while the pointer was recently over a focused window.
pub const FALLBACK_CHECK: Duration = Duration::from_millis(250);
pub const HOVER_WINDOW: Duration = Duration::from_secs(2);
/// Image cursors a session remembers the client has; matches the client's cache.
pub const KNOWN_IMAGES: usize = 32;

pub fn next_check(last_pointer: Option<Instant>, last_check: Instant, focused: bool, now: Instant) -> Option<Instant> {
    let last = last_pointer?;
    let fallback = (focused && now.duration_since(last) < HOVER_WINDOW).then_some(last_check + FALLBACK_CHECK);
    CHECK_AFTER_POINTER.iter().map(|d| last + *d).filter(|&t| t > last_check).chain(fallback).min()
}

/// A cursor bitmap as read from the Mac: premultiplied RGBA, hotspot in pixels.
#[derive(Debug, Clone, PartialEq)]
pub struct Bitmap {
    pub width: u32,
    pub height: u32,
    pub hot_x: u32,
    pub hot_y: u32,
    pub premultiplied: Vec<u8>,
}

impl Bitmap {
    pub fn fingerprint(&self) -> u64 {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        (self.width, self.height, self.hot_x, self.hot_y).hash(&mut h);
        self.premultiplied.hash(&mut h);
        h.finish()
    }

    fn to_image(&self) -> Option<CursorShape> {
        let (w, h) = (self.width, self.height);
        if w == 0 || h == 0 || w > MAX_CURSOR_SIDE || h > MAX_CURSOR_SIDE || self.premultiplied.len() != (w * h * 4) as usize {
            return None;
        }
        let rgba = self
            .premultiplied
            .as_chunks::<4>()
            .0
            .iter()
            .flat_map(|p| match p[3] {
                0 => [0, 0, 0, 0],
                a => {
                    let un = |c: u8| ((c as u32 * 255 + a as u32 / 2) / a as u32).min(255) as u8;
                    [un(p[0]), un(p[1]), un(p[2]), a]
                }
            })
            .collect();
        Some(CursorShape::Image { width: w, height: h, hot_x: self.hot_x.min(w - 1), hot_y: self.hot_y.min(h - 1), rgba })
    }
}

/// Fingerprints of the standard Mac cursors at one display scale.
#[derive(Default)]
pub struct Table(HashMap<u64, NamedCursor>);

impl Table {
    /// Earlier entries win, so list the preferred meaning of a shared image first.
    pub fn new(entries: impl IntoIterator<Item = (NamedCursor, Bitmap)>) -> Self {
        let mut map = HashMap::new();
        for (name, bitmap) in entries {
            map.entry(bitmap.fingerprint()).or_insert(name);
        }
        Self(map)
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Standard shape if recognised, else the image itself. `None` for an unusable bitmap.
    pub fn classify(&self, bitmap: &Bitmap) -> Option<Cursor> {
        let id = bitmap.fingerprint();
        let shape = match self.0.get(&id) {
            Some(&name) => CursorShape::Named(name),
            None => bitmap.to_image()?,
        };
        Some(Cursor { id, shape })
    }
}

/// What one session's client has: the current cursor and which images it cached.
#[derive(Default)]
pub struct SessionCursor {
    current: Option<u64>,
    known: HashSet<u64>,
    order: VecDeque<u64>,
}

impl SessionCursor {
    pub fn message(&mut self, cursor: &Arc<Cursor>) -> Option<HostMsg> {
        if self.current == Some(cursor.id) {
            return None;
        }
        self.current = Some(cursor.id);
        if !matches!(cursor.shape, CursorShape::Image { .. }) {
            return Some(HostMsg::Cursor((**cursor).clone()));
        }
        if self.known.contains(&cursor.id) {
            return Some(HostMsg::CursorKnown { id: cursor.id });
        }
        if self.order.len() >= KNOWN_IMAGES
            && let Some(old) = self.order.pop_front()
        {
            self.known.remove(&old);
        }
        self.known.insert(cursor.id);
        self.order.push_back(cursor.id);
        Some(HostMsg::Cursor((**cursor).clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bitmap(seed: u8, hot: u32) -> Bitmap {
        Bitmap { width: 2, height: 2, hot_x: hot, hot_y: hot, premultiplied: vec![seed, seed, seed, 255, 0, 0, 0, 0, 64, 32, 0, 128, 1, 2, 3, 255] }
    }

    #[test]
    fn classifies_standard_shapes_and_falls_back_to_image() {
        let table = Table::new([(NamedCursor::Default, bitmap(1, 0)), (NamedCursor::NotAllowed, bitmap(2, 0)), (NamedCursor::Text, bitmap(1, 0))]);
        assert_eq!(table.len(), 2);
        assert_eq!(table.classify(&bitmap(1, 0)).unwrap().shape, CursorShape::Named(NamedCursor::Default));
        assert_eq!(table.classify(&bitmap(2, 0)).unwrap().shape, CursorShape::Named(NamedCursor::NotAllowed));
        // Same pixels, different hotspot: not the same cursor.
        let Some(Cursor { shape: CursorShape::Image { rgba, hot_x, .. }, .. }) = table.classify(&bitmap(1, 1)) else { panic!() };
        assert_eq!(hot_x, 1);
        // Unpremultiplied: half-transparent (64, 32, 0) at alpha 128 becomes (128, 64, 0).
        assert_eq!(&rgba[8..12], &[128, 64, 0, 128]);
        assert_eq!(&rgba[4..8], &[0, 0, 0, 0]);
    }

    #[test]
    fn rejects_bad_bitmaps() {
        let mut b = bitmap(1, 0);
        b.premultiplied.pop();
        assert!(Table::default().classify(&b).is_none());
        let big = Bitmap { width: MAX_CURSOR_SIDE + 1, height: 1, hot_x: 0, hot_y: 0, premultiplied: vec![0; (MAX_CURSOR_SIDE as usize + 1) * 4] };
        assert!(Table::default().classify(&big).is_none());
    }

    #[test]
    fn session_sends_changes_and_reuses_cached_images() {
        let table = Table::new([(NamedCursor::Default, bitmap(1, 0))]);
        let arrow = Arc::new(table.classify(&bitmap(1, 0)).unwrap());
        let image = Arc::new(table.classify(&bitmap(9, 0)).unwrap());
        let mut s = SessionCursor::default();
        assert!(matches!(s.message(&arrow), Some(HostMsg::Cursor(_))));
        assert!(s.message(&arrow).is_none());
        assert!(matches!(s.message(&image), Some(HostMsg::Cursor(_))));
        s.message(&arrow);
        assert_eq!(s.message(&image), Some(HostMsg::CursorKnown { id: image.id }));
    }

    #[test]
    fn schedule_follows_pointer_and_gates_fallback() {
        let t = Instant::now();
        let ms = Duration::from_millis;
        assert_eq!(next_check(None, t, true, t), None);
        assert_eq!(next_check(Some(t), t, false, t), Some(t + ms(16)));
        assert_eq!(next_check(Some(t), t + ms(16), false, t + ms(16)), Some(t + ms(80)));
        assert_eq!(next_check(Some(t), t + ms(80), false, t + ms(80)), None);
        assert_eq!(next_check(Some(t), t + ms(80), true, t + ms(80)), Some(t + ms(330)));
        // Pointer idle for longer than the hover window: no fallback.
        assert_eq!(next_check(Some(t), t + ms(2500), true, t + ms(2500)), None);
    }
}
