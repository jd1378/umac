#![allow(non_upper_case_globals)]
//! Platform-free input bookkeeping: modifier flags, held keys/buttons, click counting and
//! surface-pixel to screen-point mapping. The macOS injector turns these into CGEvents.

use std::{
    collections::BTreeSet,
    time::{Duration, Instant},
};

use umac_proto::{MouseButton, keys::*};

/// `CGEventFlags` bits (IOKit `NX_*MASK`), device independent and device dependent.
pub mod flags {
    pub const ALPHA_SHIFT: u64 = 0x0001_0000;
    pub const SHIFT: u64 = 0x0002_0000;
    pub const CONTROL: u64 = 0x0004_0000;
    pub const ALTERNATE: u64 = 0x0008_0000;
    pub const COMMAND: u64 = 0x0010_0000;
    pub const SECONDARY_FN: u64 = 0x0080_0000;
    pub const NON_COALESCED: u64 = 0x0000_0100;
    pub const L_CONTROL: u64 = 0x0000_0001;
    pub const L_SHIFT: u64 = 0x0000_0002;
    pub const R_SHIFT: u64 = 0x0000_0004;
    pub const L_COMMAND: u64 = 0x0000_0008;
    pub const R_COMMAND: u64 = 0x0000_0010;
    pub const L_ALTERNATE: u64 = 0x0000_0020;
    pub const R_ALTERNATE: u64 = 0x0000_0040;
    pub const R_CONTROL: u64 = 0x0000_2000;
}

fn modifier_bits(keycode: u16) -> u64 {
    use flags::*;
    match keycode {
        kVK_Shift => SHIFT | L_SHIFT,
        kVK_RightShift => SHIFT | R_SHIFT,
        kVK_Control => CONTROL | L_CONTROL,
        kVK_RightControl => CONTROL | R_CONTROL,
        kVK_Option => ALTERNATE | L_ALTERNATE,
        kVK_RightOption => ALTERNATE | R_ALTERNATE,
        kVK_Command => COMMAND | L_COMMAND,
        kVK_RightCommand => COMMAND | R_COMMAND,
        kVK_Function => SECONDARY_FN,
        _ => 0,
    }
}

/// What the injector should do for one key event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyAction {
    /// Modifier changed: post a `FlagsChanged` event with these flags.
    FlagsChanged { keycode: u16, flags: u64 },
    /// Regular key: post key down/up with these flags.
    Key { keycode: u16, pressed: bool, flags: u64 },
    /// Repeated press or release of a key we never saw go down.
    Ignore,
}

#[derive(Debug, Default)]
pub struct InputState {
    modifiers: BTreeSet<u16>,
    keys: BTreeSet<u16>,
    buttons: BTreeSet<ButtonKey>,
    caps_lock: bool,
    clicks: ClickCounter,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ButtonKey(u8);

impl From<MouseButton> for ButtonKey {
    fn from(b: MouseButton) -> Self {
        Self(match b {
            MouseButton::Left => 0,
            MouseButton::Right => 1,
            MouseButton::Middle => 2,
            MouseButton::Other(n) => n.saturating_add(3).min(31),
        })
    }
}

impl ButtonKey {
    /// CoreGraphics button number (0 left, 1 right, 2 center, then others).
    pub fn cg_number(self) -> u32 {
        self.0 as u32
    }
}

impl InputState {
    pub fn flags(&self) -> u64 {
        let mut f = self.modifiers.iter().fold(0, |acc, k| acc | modifier_bits(*k));
        if self.caps_lock {
            f |= flags::ALPHA_SHIFT;
        }
        f | flags::NON_COALESCED
    }

    pub fn key(&mut self, keycode: u16, pressed: bool) -> KeyAction {
        if keycode == kVK_CapsLock {
            if !pressed {
                return KeyAction::Ignore;
            }
            self.caps_lock = !self.caps_lock;
            return KeyAction::FlagsChanged { keycode, flags: self.flags() };
        }
        if is_modifier(keycode) {
            let changed = if pressed { self.modifiers.insert(keycode) } else { self.modifiers.remove(&keycode) };
            return if changed { KeyAction::FlagsChanged { keycode, flags: self.flags() } } else { KeyAction::Ignore };
        }
        if !pressed && !self.keys.remove(&keycode) {
            return KeyAction::Ignore;
        }
        if pressed {
            // Auto-repeat from the client arrives as repeated presses; pass them through.
            self.keys.insert(keycode);
        }
        KeyAction::Key { keycode, pressed, flags: self.flags() }
    }

    /// Records a button change and returns the click count to stamp on the event.
    pub fn button(&mut self, button: MouseButton, pressed: bool, pos: (f64, f64), now: Instant) -> i64 {
        let b = ButtonKey::from(button);
        if pressed {
            self.buttons.insert(b);
            self.clicks.press(b, pos, now)
        } else {
            self.buttons.remove(&b);
            self.clicks.current()
        }
    }

    /// Button whose drag event a mouse move should become, if any is held.
    pub fn dragging(&self) -> Option<ButtonKey> {
        self.buttons.first().copied()
    }

    /// Everything still held, so it can be released (modifiers last).
    pub fn take_held(&mut self) -> (Vec<u16>, Vec<ButtonKey>, Vec<u16>) {
        let keys = std::mem::take(&mut self.keys).into_iter().collect();
        let buttons = std::mem::take(&mut self.buttons).into_iter().collect();
        let mods = std::mem::take(&mut self.modifiers).into_iter().collect();
        (keys, buttons, mods)
    }
}

/// Double/triple click detection, matching the AppKit default interval.
#[derive(Debug, Default)]
struct ClickCounter {
    last: Option<(ButtonKey, (f64, f64), Instant)>,
    count: i64,
}

const DOUBLE_CLICK: Duration = Duration::from_millis(500);

/// Repeats the held key like a physical keyboard: events posted by software never
/// auto-repeat on macOS, and client-side repeat would add network jitter to the rhythm.
#[derive(Debug)]
pub struct KeyRepeat {
    delay: Duration,
    interval: Duration,
    held: Option<(u16, Instant)>,
}

impl KeyRepeat {
    pub fn new(delay: Duration, interval: Duration) -> Self {
        Self { delay, interval: interval.max(Duration::from_millis(5)), held: None }
    }

    pub fn press(&mut self, keycode: u16, now: Instant) {
        if !is_modifier(keycode) {
            self.held = Some((keycode, now + self.delay));
        }
    }

    pub fn release(&mut self, keycode: u16) {
        if self.held.is_some_and(|(k, _)| k == keycode) {
            self.held = None;
        }
    }

    pub fn clear(&mut self) {
        self.held = None;
    }

    pub fn deadline(&self) -> Option<Instant> {
        self.held.map(|(_, at)| at)
    }

    /// The key to repeat now, if due. A late wakeup repeats once instead of bursting.
    pub fn due(&mut self, now: Instant) -> Option<u16> {
        let (key, at) = self.held?;
        if now < at {
            return None;
        }
        self.held = Some((key, (at + self.interval).max(now)));
        Some(key)
    }
}
const CLICK_SLOP_POINTS: f64 = 4.0;

impl ClickCounter {
    fn press(&mut self, b: ButtonKey, pos: (f64, f64), now: Instant) -> i64 {
        let repeat = self.last.is_some_and(|(lb, lp, lt)| {
            lb == b
                && now.duration_since(lt) <= DOUBLE_CLICK
                && (lp.0 - pos.0).abs() <= CLICK_SLOP_POINTS
                && (lp.1 - pos.1).abs() <= CLICK_SLOP_POINTS
        });
        self.count = if repeat { self.count + 1 } else { 1 };
        self.last = Some((b, pos, now));
        self.count
    }

    fn current(&self) -> i64 {
        self.count.max(1)
    }
}

/// Window placement on the Mac in global screen points (top-left origin).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Placement {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    /// Surface pixels per point.
    pub scale: f64,
}

impl Placement {
    pub fn contains(self, p: (f64, f64)) -> bool {
        p.0 >= self.x && p.1 >= self.y && p.0 < self.x + self.width && p.1 < self.y + self.height
    }

    /// Maps a surface pixel position to a global point, clamped inside the window so a
    /// stray coordinate never lands on another window.
    pub fn to_global(self, px: f32, py: f32) -> (f64, f64) {
        let scale = if self.scale > 0.0 { self.scale } else { 1.0 };
        let x = (px as f64 / scale).clamp(0.0, (self.width - 1.0).max(0.0));
        let y = (py as f64 / scale).clamp(0.0, (self.height - 1.0).max(0.0));
        (self.x + x, self.y + y)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modifier_flags_track_left_and_right() {
        let mut s = InputState::default();
        assert_eq!(
            s.key(kVK_Command, true),
            KeyAction::FlagsChanged { keycode: kVK_Command, flags: flags::COMMAND | flags::L_COMMAND | flags::NON_COALESCED }
        );
        s.key(kVK_RightShift, true);
        let KeyAction::Key { flags: f, .. } = s.key(kVK_ANSI_C, true) else { panic!() };
        assert_eq!(f, flags::COMMAND | flags::L_COMMAND | flags::SHIFT | flags::R_SHIFT | flags::NON_COALESCED);
        s.key(kVK_Command, false);
        s.key(kVK_RightShift, false);
        assert_eq!(s.flags(), flags::NON_COALESCED);
        assert_eq!(s.key(kVK_Command, false), KeyAction::Ignore);
    }

    #[test]
    fn caps_lock_toggles() {
        let mut s = InputState::default();
        s.key(kVK_CapsLock, true);
        assert_eq!(s.key(kVK_CapsLock, false), KeyAction::Ignore);
        assert_ne!(s.flags() & flags::ALPHA_SHIFT, 0);
        s.key(kVK_CapsLock, true);
        assert_eq!(s.flags() & flags::ALPHA_SHIFT, 0);
    }

    #[test]
    fn release_of_unknown_key_ignored_and_release_all() {
        let mut s = InputState::default();
        assert_eq!(s.key(kVK_ANSI_A, false), KeyAction::Ignore);
        s.key(kVK_ANSI_A, true);
        s.key(kVK_Shift, true);
        s.button(MouseButton::Left, true, (0.0, 0.0), Instant::now());
        let (keys, buttons, mods) = s.take_held();
        assert_eq!((keys, buttons.len(), mods), (vec![kVK_ANSI_A], 1, vec![kVK_Shift]));
        assert_eq!(s.dragging(), None);
    }

    #[test]
    fn click_counting() {
        let mut s = InputState::default();
        let t = Instant::now();
        assert_eq!(s.button(MouseButton::Left, true, (10.0, 10.0), t), 1);
        assert_eq!(s.button(MouseButton::Left, false, (10.0, 10.0), t), 1);
        assert_eq!(s.button(MouseButton::Left, true, (11.0, 12.0), t + Duration::from_millis(200)), 2);
        assert_eq!(s.button(MouseButton::Left, false, (11.0, 12.0), t), 2);
        assert_eq!(s.button(MouseButton::Left, true, (11.0, 12.0), t + Duration::from_millis(1000)), 1);
        assert_eq!(s.button(MouseButton::Right, true, (11.0, 12.0), t + Duration::from_millis(1100)), 1);
        assert_eq!(s.dragging(), Some(ButtonKey(0)));
    }

    #[test]
    fn key_repeat_waits_delay_then_repeats_held_key_only() {
        let t = Instant::now();
        let ms = Duration::from_millis;
        let mut r = KeyRepeat::new(ms(300), ms(30));
        r.press(kVK_Shift, t);
        assert_eq!(r.deadline(), None);
        r.press(kVK_Delete, t);
        assert_eq!(r.due(t + ms(299)), None);
        assert_eq!(r.due(t + ms(300)), Some(kVK_Delete));
        assert_eq!(r.due(t + ms(310)), None);
        assert_eq!(r.due(t + ms(330)), Some(kVK_Delete));
        // A long stall repeats once, then resumes the normal rhythm.
        assert_eq!(r.due(t + ms(1000)), Some(kVK_Delete));
        assert_eq!(r.deadline(), Some(t + ms(1000)));
        // Pressing another key takes over; releasing the old one does nothing.
        r.press(kVK_ANSI_A, t + ms(1000));
        r.release(kVK_Delete);
        assert!(r.deadline().is_some());
        r.release(kVK_ANSI_A);
        assert_eq!(r.deadline(), None);
    }

    #[test]
    fn coordinate_mapping_scales_and_clamps() {
        let p = Placement { x: 100.0, y: 50.0, width: 800.0, height: 600.0, scale: 2.0 };
        assert_eq!(p.to_global(200.0, 100.0), (200.0, 100.0));
        assert_eq!(p.to_global(-5.0, 99_999.0), (100.0, 649.0));
    }
}
