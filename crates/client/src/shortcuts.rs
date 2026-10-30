//! Translates Linux keyboard habits into their macOS equivalents.
//!
//! Input is positional (Linux Ctrl arrives as `kVK_Control`, Super as `kVK_Command`). Outside
//! terminals Ctrl acts as Cmd when `swap_ctrl_cmd` is on, and a few text editing chords are
//! rewritten (Ctrl+Backspace deletes a word with Option on the Mac, Home/End move within the
//! line with Cmd). Terminals keep Ctrl as Control so Ctrl+C interrupts, and use Ctrl+Shift+C/V
//! for copy and paste like Linux terminals do.
#![allow(non_upper_case_globals)]

use std::collections::{BTreeSet, HashMap};

use umac_proto::keys::*;

/// Bundle ids of terminal emulators, which get Linux terminal key rules.
const TERMINALS: &[&str] = &[
    "com.apple.Terminal",
    "com.googlecode.iterm2",
    "net.kovidgoyal.kitty",
    "org.alacritty",
    "io.alacritty",
    "com.github.wez.wezterm",
    "com.mitchellh.ghostty",
    "dev.warp.Warp-Stable",
];

pub fn is_terminal(bundle_id: Option<&str>) -> bool {
    bundle_id.is_some_and(|b| TERMINALS.contains(&b))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    /// Ctrl/Cmd swap per config, Linux text editing chords.
    Apps,
    /// Ctrl stays Control; Ctrl+Shift+C/V copy and paste.
    Terminal,
    /// No rewriting at all: positional keys with the configured swap.
    Native,
}

#[derive(Debug, Clone, Copy, Default)]
struct Mods {
    ctrl: bool,
    shift: bool,
    alt: bool,
    super_: bool,
}

pub struct Translator {
    swap_ctrl_cmd: bool,
    /// Physical modifier keys held, positional codes.
    held: BTreeSet<u16>,
    /// Modifier keys currently down on the Mac.
    sent: BTreeSet<u16>,
    /// Keys whose press was rewritten, mapped to the key that was sent instead.
    active: HashMap<u16, u16>,
}

fn is_mod(k: u16) -> bool {
    is_modifier(k) && k != kVK_CapsLock && k != kVK_Function
}

impl Translator {
    pub fn new(swap_ctrl_cmd: bool) -> Self {
        Self { swap_ctrl_cmd, held: BTreeSet::new(), sent: BTreeSet::new(), active: HashMap::new() }
    }

    fn mods(&self) -> Mods {
        let has = |a: u16, b: u16| self.held.contains(&a) || self.held.contains(&b);
        Mods {
            ctrl: has(kVK_Control, kVK_RightControl),
            shift: has(kVK_Shift, kVK_RightShift),
            alt: has(kVK_Option, kVK_RightOption),
            super_: has(kVK_Command, kVK_RightCommand),
        }
    }

    /// Mac modifier a physical modifier stands for under `profile`.
    fn map_mod(&self, k: u16, profile: Profile) -> u16 {
        if !self.swap_ctrl_cmd || profile == Profile::Terminal {
            return k;
        }
        match k {
            kVK_Control => kVK_Command,
            kVK_RightControl => kVK_RightCommand,
            kVK_Command => kVK_Control,
            kVK_RightCommand => kVK_RightControl,
            k => k,
        }
    }

    /// Mac modifiers matching the physical ones held.
    fn natural(&self, profile: Profile) -> BTreeSet<u16> {
        self.held.iter().map(|&k| self.map_mod(k, profile)).collect()
    }

    /// Key events that move the Mac's modifiers from what was sent to `want`.
    fn set_mods(&mut self, want: BTreeSet<u16>, out: &mut Vec<(u16, bool)>) {
        for &k in self.sent.difference(&want) {
            out.push((k, false));
        }
        for &k in want.difference(&self.sent) {
            out.push((k, true));
        }
        self.sent = want;
    }

    fn rule(&self, key: u16, m: Mods, profile: Profile) -> Option<(u16, BTreeSet<u16>)> {
        let shift = |set: &mut BTreeSet<u16>| {
            if m.shift {
                set.insert(kVK_Shift);
            }
        };
        let with = |mods: &[u16], keep_shift: bool| {
            let mut s: BTreeSet<u16> = mods.iter().copied().collect();
            if keep_shift {
                shift(&mut s);
            }
            s
        };
        if profile == Profile::Native || m.alt || m.super_ {
            return None;
        }
        let editing = match (key, m.ctrl) {
            (kVK_Delete, true) => Some((kVK_Delete, with(&[kVK_Option], false))),
            (kVK_ForwardDelete, true) => Some((kVK_ForwardDelete, with(&[kVK_Option], false))),
            (kVK_LeftArrow | kVK_RightArrow, true) => Some((key, with(&[kVK_Option], true))),
            _ => None,
        };
        if editing.is_some() {
            return editing;
        }
        match profile {
            Profile::Terminal => match (key, m.ctrl, m.shift) {
                (kVK_ANSI_C | kVK_ANSI_V, true, true) => Some((key, with(&[kVK_Command], false))),
                (kVK_Home, false, _) => Some((kVK_ANSI_A, with(&[kVK_Control], false))),
                (kVK_End, false, _) => Some((kVK_ANSI_E, with(&[kVK_Control], false))),
                _ => None,
            },
            Profile::Apps => match (key, m.ctrl) {
                (kVK_Home, false) => Some((kVK_LeftArrow, with(&[kVK_Command], true))),
                (kVK_End, false) => Some((kVK_RightArrow, with(&[kVK_Command], true))),
                (kVK_Home, true) => Some((kVK_UpArrow, with(&[kVK_Command], true))),
                (kVK_End, true) => Some((kVK_DownArrow, with(&[kVK_Command], true))),
                _ => None,
            },
            Profile::Native => None,
        }
    }

    /// One positional key change in, the Mac key events to send out.
    pub fn key(&mut self, key: u16, pressed: bool, profile: Profile) -> Vec<(u16, bool)> {
        let mut out = Vec::new();
        if is_mod(key) {
            if pressed {
                self.held.insert(key);
            } else {
                self.held.remove(&key);
            }
            // While a rewritten key is down its modifiers stay as the rule set them.
            if self.active.is_empty() {
                let want = self.natural(profile);
                self.set_mods(want, &mut out);
            }
            return out;
        }
        if pressed {
            if let Some((sent_key, mods)) = self.rule(key, self.mods(), profile) {
                self.set_mods(mods, &mut out);
                self.active.insert(key, sent_key);
                out.push((sent_key, true));
                return out;
            }
            let want = self.natural(profile);
            self.set_mods(want, &mut out);
            out.push((key, true));
            return out;
        }
        match self.active.remove(&key) {
            Some(sent_key) => {
                out.push((sent_key, false));
                if self.active.is_empty() {
                    let want = self.natural(profile);
                    self.set_mods(want, &mut out);
                }
            }
            None => out.push((key, false)),
        }
        out
    }

    /// Focus left: the host releases everything itself, so just forget the state.
    pub fn reset(&mut self) {
        self.held.clear();
        self.sent.clear();
        self.active.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(t: &mut Translator, keys: &[(u16, bool)], p: Profile) -> Vec<(u16, bool)> {
        keys.iter().flat_map(|&(k, d)| t.key(k, d, p)).collect()
    }

    #[test]
    fn ctrl_is_cmd_in_apps() {
        let mut t = Translator::new(true);
        let out = run(&mut t, &[(kVK_Control, true), (kVK_ANSI_C, true), (kVK_ANSI_C, false), (kVK_Control, false)], Profile::Apps);
        assert_eq!(out, vec![(kVK_Command, true), (kVK_ANSI_C, true), (kVK_ANSI_C, false), (kVK_Command, false)]);
    }

    #[test]
    fn ctrl_backspace_deletes_word_and_restores_cmd() {
        let mut t = Translator::new(true);
        let out = run(&mut t, &[(kVK_Control, true), (kVK_Delete, true), (kVK_Delete, false)], Profile::Apps);
        assert_eq!(
            out,
            vec![(kVK_Command, true), (kVK_Command, false), (kVK_Option, true), (kVK_Delete, true), (kVK_Delete, false), (kVK_Option, false), (kVK_Command, true)]
        );
    }

    #[test]
    fn modifier_released_during_rewrite_is_not_restored() {
        let mut t = Translator::new(true);
        run(&mut t, &[(kVK_Control, true), (kVK_Delete, true)], Profile::Apps);
        let out = run(&mut t, &[(kVK_Control, false), (kVK_Delete, false)], Profile::Apps);
        assert_eq!(out, vec![(kVK_Delete, false), (kVK_Option, false)]);
    }

    #[test]
    fn shift_ctrl_arrow_selects_words() {
        let mut t = Translator::new(true);
        let out = run(&mut t, &[(kVK_Shift, true), (kVK_Control, true), (kVK_LeftArrow, true)], Profile::Apps);
        assert_eq!(out.last(), Some(&(kVK_LeftArrow, true)));
        assert_eq!(t.sent, BTreeSet::from([kVK_Shift, kVK_Option]));
    }

    #[test]
    fn home_end_in_apps_and_terminal() {
        let mut t = Translator::new(true);
        assert_eq!(run(&mut t, &[(kVK_Home, true)], Profile::Apps), vec![(kVK_Command, true), (kVK_LeftArrow, true)]);
        t.reset();
        assert_eq!(run(&mut t, &[(kVK_End, true)], Profile::Terminal), vec![(kVK_Control, true), (kVK_ANSI_E, true)]);
    }

    #[test]
    fn terminal_keeps_ctrl_and_uses_ctrl_shift_for_clipboard() {
        let mut t = Translator::new(true);
        let out = run(&mut t, &[(kVK_Control, true), (kVK_ANSI_C, true), (kVK_ANSI_C, false)], Profile::Terminal);
        assert_eq!(out, vec![(kVK_Control, true), (kVK_ANSI_C, true), (kVK_ANSI_C, false)]);
        let out = run(&mut t, &[(kVK_Shift, true), (kVK_ANSI_V, true)], Profile::Terminal);
        assert_eq!(out, vec![(kVK_Shift, true), (kVK_Shift, false), (kVK_Control, false), (kVK_Command, true), (kVK_ANSI_V, true)]);
    }

    #[test]
    fn native_profile_only_swaps() {
        let mut t = Translator::new(true);
        let out = run(&mut t, &[(kVK_Control, true), (kVK_Delete, true)], Profile::Native);
        assert_eq!(out, vec![(kVK_Command, true), (kVK_Delete, true)]);
    }

    #[test]
    fn terminal_detection() {
        assert!(is_terminal(Some("com.apple.Terminal")));
        assert!(!is_terminal(Some("com.apple.Safari")));
        assert!(!is_terminal(None));
    }
}
