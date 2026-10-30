//! Physical key (winit, layout independent) to macOS virtual keycode. Positional mapping lets
//! the Mac apply its own keyboard layout.

use umac_proto::keys::*;
use winit::keyboard::KeyCode;

pub fn to_mac(code: KeyCode, swap_ctrl_cmd: bool) -> Option<u16> {
    use KeyCode as K;
    let (ctrl_l, ctrl_r, super_l, super_r) = if swap_ctrl_cmd {
        (kVK_Command, kVK_RightCommand, kVK_Control, kVK_RightControl)
    } else {
        (kVK_Control, kVK_RightControl, kVK_Command, kVK_RightCommand)
    };
    Some(match code {
        K::KeyA => kVK_ANSI_A,
        K::KeyB => kVK_ANSI_B,
        K::KeyC => kVK_ANSI_C,
        K::KeyD => kVK_ANSI_D,
        K::KeyE => kVK_ANSI_E,
        K::KeyF => kVK_ANSI_F,
        K::KeyG => kVK_ANSI_G,
        K::KeyH => kVK_ANSI_H,
        K::KeyI => kVK_ANSI_I,
        K::KeyJ => kVK_ANSI_J,
        K::KeyK => kVK_ANSI_K,
        K::KeyL => kVK_ANSI_L,
        K::KeyM => kVK_ANSI_M,
        K::KeyN => kVK_ANSI_N,
        K::KeyO => kVK_ANSI_O,
        K::KeyP => kVK_ANSI_P,
        K::KeyQ => kVK_ANSI_Q,
        K::KeyR => kVK_ANSI_R,
        K::KeyS => kVK_ANSI_S,
        K::KeyT => kVK_ANSI_T,
        K::KeyU => kVK_ANSI_U,
        K::KeyV => kVK_ANSI_V,
        K::KeyW => kVK_ANSI_W,
        K::KeyX => kVK_ANSI_X,
        K::KeyY => kVK_ANSI_Y,
        K::KeyZ => kVK_ANSI_Z,
        K::Digit0 => kVK_ANSI_0,
        K::Digit1 => kVK_ANSI_1,
        K::Digit2 => kVK_ANSI_2,
        K::Digit3 => kVK_ANSI_3,
        K::Digit4 => kVK_ANSI_4,
        K::Digit5 => kVK_ANSI_5,
        K::Digit6 => kVK_ANSI_6,
        K::Digit7 => kVK_ANSI_7,
        K::Digit8 => kVK_ANSI_8,
        K::Digit9 => kVK_ANSI_9,
        K::Minus => kVK_ANSI_Minus,
        K::Equal => kVK_ANSI_Equal,
        K::BracketLeft => kVK_ANSI_LeftBracket,
        K::BracketRight => kVK_ANSI_RightBracket,
        K::Backslash => kVK_ANSI_Backslash,
        K::Semicolon => kVK_ANSI_Semicolon,
        K::Quote => kVK_ANSI_Quote,
        K::Backquote => kVK_ANSI_Grave,
        K::Comma => kVK_ANSI_Comma,
        K::Period => kVK_ANSI_Period,
        K::Slash => kVK_ANSI_Slash,
        K::IntlBackslash => kVK_ISO_Section,
        K::Enter => kVK_Return,
        K::Tab => kVK_Tab,
        K::Space => kVK_Space,
        K::Backspace => kVK_Delete,
        K::Delete => kVK_ForwardDelete,
        K::Escape => kVK_Escape,
        K::Insert | K::Help => kVK_Help,
        K::Home => kVK_Home,
        K::End => kVK_End,
        K::PageUp => kVK_PageUp,
        K::PageDown => kVK_PageDown,
        K::ArrowLeft => kVK_LeftArrow,
        K::ArrowRight => kVK_RightArrow,
        K::ArrowUp => kVK_UpArrow,
        K::ArrowDown => kVK_DownArrow,
        K::ShiftLeft => kVK_Shift,
        K::ShiftRight => kVK_RightShift,
        K::AltLeft => kVK_Option,
        K::AltRight => kVK_RightOption,
        K::ControlLeft => ctrl_l,
        K::ControlRight => ctrl_r,
        K::SuperLeft => super_l,
        K::SuperRight => super_r,
        K::CapsLock => kVK_CapsLock,
        K::Fn => kVK_Function,
        K::F1 => kVK_F1,
        K::F2 => kVK_F2,
        K::F3 => kVK_F3,
        K::F4 => kVK_F4,
        K::F5 => kVK_F5,
        K::F6 => kVK_F6,
        K::F7 => kVK_F7,
        K::F8 => kVK_F8,
        K::F9 => kVK_F9,
        K::F10 => kVK_F10,
        K::F11 => kVK_F11,
        K::F12 => kVK_F12,
        K::F13 => kVK_F13,
        K::F14 => kVK_F14,
        K::F15 => kVK_F15,
        K::F16 => kVK_F16,
        K::F17 => kVK_F17,
        K::F18 => kVK_F18,
        K::F19 => kVK_F19,
        K::F20 => kVK_F20,
        K::NumLock => kVK_ANSI_KeypadClear,
        K::Numpad0 => kVK_ANSI_Keypad0,
        K::Numpad1 => kVK_ANSI_Keypad1,
        K::Numpad2 => kVK_ANSI_Keypad2,
        K::Numpad3 => kVK_ANSI_Keypad3,
        K::Numpad4 => kVK_ANSI_Keypad4,
        K::Numpad5 => kVK_ANSI_Keypad5,
        K::Numpad6 => kVK_ANSI_Keypad6,
        K::Numpad7 => kVK_ANSI_Keypad7,
        K::Numpad8 => kVK_ANSI_Keypad8,
        K::Numpad9 => kVK_ANSI_Keypad9,
        K::NumpadDecimal => kVK_ANSI_KeypadDecimal,
        K::NumpadMultiply => kVK_ANSI_KeypadMultiply,
        K::NumpadAdd => kVK_ANSI_KeypadPlus,
        K::NumpadSubtract => kVK_ANSI_KeypadMinus,
        K::NumpadDivide => kVK_ANSI_KeypadDivide,
        K::NumpadEnter => kVK_ANSI_KeypadEnter,
        K::NumpadEqual => kVK_ANSI_KeypadEquals,
        K::AudioVolumeUp => kVK_VolumeUp,
        K::AudioVolumeDown => kVK_VolumeDown,
        K::AudioVolumeMute => kVK_Mute,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letters_and_specials() {
        assert_eq!(to_mac(KeyCode::KeyA, false), Some(kVK_ANSI_A));
        assert_eq!(to_mac(KeyCode::KeyQ, false), Some(kVK_ANSI_Q));
        assert_eq!(to_mac(KeyCode::Backspace, false), Some(kVK_Delete));
        assert_eq!(to_mac(KeyCode::Delete, false), Some(kVK_ForwardDelete));
        assert_eq!(to_mac(KeyCode::NumpadEnter, false), Some(kVK_ANSI_KeypadEnter));
        assert_eq!(to_mac(KeyCode::F12, false), Some(kVK_F12));
        assert_eq!(to_mac(KeyCode::PrintScreen, false), None);
    }

    #[test]
    fn ctrl_cmd_swap() {
        assert_eq!(to_mac(KeyCode::ControlLeft, true), Some(kVK_Command));
        assert_eq!(to_mac(KeyCode::SuperLeft, true), Some(kVK_Control));
        assert_eq!(to_mac(KeyCode::ControlLeft, false), Some(kVK_Control));
        assert_eq!(to_mac(KeyCode::SuperRight, false), Some(kVK_RightCommand));
    }

    #[test]
    fn mapping_is_injective() {
        use std::collections::HashMap;
        let all = [
            KeyCode::KeyA, KeyCode::KeyZ, KeyCode::Digit0, KeyCode::Digit9, KeyCode::Minus, KeyCode::Equal,
            KeyCode::BracketLeft, KeyCode::BracketRight, KeyCode::Backslash, KeyCode::Semicolon, KeyCode::Quote,
            KeyCode::Backquote, KeyCode::Comma, KeyCode::Period, KeyCode::Slash, KeyCode::Enter, KeyCode::Tab,
            KeyCode::Space, KeyCode::Backspace, KeyCode::Delete, KeyCode::Escape, KeyCode::Home, KeyCode::End,
            KeyCode::PageUp, KeyCode::PageDown, KeyCode::ArrowLeft, KeyCode::ArrowRight, KeyCode::ArrowUp,
            KeyCode::ArrowDown, KeyCode::ShiftLeft, KeyCode::ShiftRight, KeyCode::AltLeft, KeyCode::AltRight,
            KeyCode::ControlLeft, KeyCode::ControlRight, KeyCode::SuperLeft, KeyCode::SuperRight,
            KeyCode::Numpad0, KeyCode::Numpad9, KeyCode::NumpadEnter, KeyCode::F1, KeyCode::F20,
        ];
        let mut seen = HashMap::new();
        for k in all {
            let v = to_mac(k, true).unwrap();
            assert!(seen.insert(v, k).is_none(), "{k:?} collides with {:?}", seen[&v]);
        }
    }
}
