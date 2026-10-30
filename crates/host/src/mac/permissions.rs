//! Screen Recording and Accessibility (TCC) checks. Prompts only work from the GUI session.

use super::{ax, ffi};

pub fn screen_recording() -> bool {
    unsafe { ffi::CGPreflightScreenCaptureAccess() }
}

pub fn accessibility() -> bool {
    ax::is_trusted()
}

/// Asks for whatever is missing. The system shows its own dialogs and adds this binary to
/// System Settings > Privacy & Security; the user still has to flip the switches.
pub fn request_missing() {
    if !screen_recording() {
        unsafe { ffi::CGRequestScreenCaptureAccess() };
    }
    if !accessibility() {
        ax::prompt_trust();
    }
}

pub const SCREEN_RECORDING_PANE: &str = "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture";
pub const ACCESSIBILITY_PANE: &str = "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility";
