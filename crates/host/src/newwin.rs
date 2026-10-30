//! Rules for windows a streamed app opens by itself (new documents, dialogs, sheets, file
//! panels): which ones to follow and as what kind.

use std::collections::HashSet;

use umac_proto::{WindowId, WindowKind};

/// Smaller windows are transient helpers (drag images, progress slivers), not worth a window.
pub const MIN_SIZE: f64 = 40.0;
/// Windows followed per session; a misbehaving app must not flood the client.
pub const MAX_AUTO: usize = 16;
/// How long a new window may stay unclassifiable (AX not ready yet) before it counts as a
/// dialog of the app (popovers have no AX window at all).
pub const CLASSIFY_GRACE_MS: u64 = 500;

/// Processes that show file Open/Save panels for sandboxed apps. Their windows belong to
/// whichever streamed window the user was working in.
const PANEL_SERVICES: &[&str] = &["com.apple.appkit.xpc.openAndSavePanelService", "Open and Save Panel Service"];

pub fn is_panel_service(owner: &str) -> bool {
    PANEL_SERVICES.iter().any(|s| owner.contains(s))
}

/// What Accessibility says about a window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AxKind {
    pub role: String,
    pub subrole: String,
    /// Found as a sheet attached to another window rather than in the app's window list.
    pub sheet: bool,
    /// `AXModal`: blocks its app until dismissed (file panels, alerts).
    pub modal: bool,
    /// Has an enabled minimize button; settings and utility windows usually do not.
    pub minimizable: bool,
}

/// Normal for standard windows, Dialog for dialogs, sheets and floating panels.
pub fn classify(ax: &AxKind) -> WindowKind {
    if ax.sheet || ax.role == "AXSheet" {
        return WindowKind::Dialog;
    }
    match ax.subrole.as_str() {
        "AXDialog" | "AXSystemDialog" | "AXFloatingWindow" | "AXSystemFloatingWindow" => WindowKind::Dialog,
        // On macOS 27 file panels and Settings windows report AXStandardWindow and are not
        // modal; what sets them apart from documents is the disabled minimize button.
        _ if ax.modal || !ax.minimizable => WindowKind::Dialog,
        _ => WindowKind::Normal,
    }
}

/// Layer 0 windows that appeared since the last poll. `known` is updated to the current set,
/// so each window is offered once; the first poll only records what is already open.
pub fn fresh(known: &mut Option<HashSet<WindowId>>, now: &[(WindowId, i32)]) -> Vec<WindowId> {
    let current: HashSet<WindowId> = now.iter().filter(|(_, layer)| *layer == 0).map(|(id, _)| *id).collect();
    let fresh = match known {
        Some(k) => now.iter().filter(|(id, layer)| *layer == 0 && !k.contains(id)).map(|(id, _)| *id).collect(),
        None => Vec::new(),
    };
    *known = Some(current);
    fresh
}

/// Window levels the focused window may have to be followed: normal (0), floating panels (3)
/// and modal panels such as password prompts (8). Menus (101) are popups; overlays like the
/// Siri panel (23), the menu bar (24, 25) and the Dock sit above and are left alone.
pub fn focus_eligible(layer: i32, width: f64, height: f64) -> bool {
    (0..=8).contains(&layer) && width >= MIN_SIZE && height >= MIN_SIZE
}

/// System UI processes whose windows are never followed even when they take focus.
const SYSTEM_UI: &[&str] = &["Dock", "SystemUIServer", "Siri AI", "Control Center", "Notification Center", "Spotlight", "WindowManager", "Window Server"];

pub fn is_system_ui(owner: &str) -> bool {
    SYSTEM_UI.contains(&owner)
}

/// Follows the Mac's focused window: reports it when focus moves to a window this session
/// does not show. The window focused when following starts, one the client closed while it
/// stays focused, and one that could not be started are skipped until focus leaves them.
#[derive(Debug, Default)]
pub struct FocusFollower {
    started: bool,
    focused: Option<WindowId>,
    skip: Option<WindowId>,
}

impl FocusFollower {
    pub fn update(
        &mut self,
        focused: Option<WindowId>,
        streamed: impl Fn(WindowId) -> bool,
        eligible: impl Fn(WindowId) -> bool,
    ) -> Option<WindowId> {
        if !self.started {
            (self.started, self.focused, self.skip) = (true, focused, focused);
            return None;
        }
        if focused != self.focused {
            self.focused = focused;
            if self.skip != focused {
                self.skip = None;
            }
        }
        focused.filter(|&w| self.skip != Some(w) && !streamed(w) && eligible(w))
    }

    /// The client closed `window`: do not bring it back while it keeps focus.
    pub fn dismissed(&mut self, window: WindowId) {
        if self.focused == Some(window) {
            self.skip = Some(window);
        }
    }

    /// Following the focused window failed or was refused: wait until focus moves.
    pub fn failed(&mut self) {
        self.skip = self.focused;
    }
}

/// The app's front window when Accessibility names none: its first window, in front-to-back
/// order, that focus following accepts.
pub fn front_window(pid: i32, front_to_back: &[(WindowId, i32, i32, f64, f64)]) -> Option<WindowId> {
    front_to_back.iter().find(|&&(_, owner, layer, w, h)| owner == pid && focus_eligible(layer, w, h)).map(|&(id, ..)| id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ax(role: &str, subrole: &str, sheet: bool) -> AxKind {
        AxKind { role: role.into(), subrole: subrole.into(), sheet, modal: false, minimizable: true }
    }

    #[test]
    fn follows_focus_changes_but_not_the_starting_window() {
        let mut f = FocusFollower::default();
        let none = |_| false;
        let all = |_| true;
        // Whatever is focused when following starts stays where it is.
        assert_eq!(f.update(Some(1), none, all), None);
        assert_eq!(f.update(Some(1), none, all), None);
        // Calculator (2) takes focus: streamed until it shows up as streamed.
        assert_eq!(f.update(Some(2), none, all), Some(2));
        assert_eq!(f.update(Some(2), |w| w == 2, all), None);
        // Back to the starting window: it is followed now that focus left and returned.
        assert_eq!(f.update(Some(1), none, all), Some(1));
        // Not eligible (overlay, tiny, system UI).
        assert_eq!(f.update(Some(3), none, |_| false), None);
        assert_eq!(f.update(None, none, all), None);
    }

    #[test]
    fn closed_window_returns_only_after_focus_left() {
        let mut f = FocusFollower::default();
        let none = |_| false;
        let all = |_| true;
        f.update(Some(1), none, all);
        assert_eq!(f.update(Some(2), none, all), Some(2));
        f.dismissed(2);
        assert_eq!(f.update(Some(2), none, all), None);
        assert_eq!(f.update(Some(1), none, all), Some(1));
        assert_eq!(f.update(Some(2), none, all), Some(2));
        // A failed start waits for the next focus change too.
        f.failed();
        assert_eq!(f.update(Some(2), none, all), None);
        // Dismissing a window that is not focused changes nothing.
        f.dismissed(7);
        assert_eq!(f.update(Some(4), none, all), Some(4));
    }

    #[test]
    fn eligibility_and_front_window() {
        assert!(focus_eligible(0, 400.0, 300.0));
        assert!(focus_eligible(8, 400.0, 200.0));
        assert!(!focus_eligible(23, 800.0, 600.0));
        assert!(!focus_eligible(101, 200.0, 300.0));
        assert!(!focus_eligible(0, 20.0, 300.0));
        assert!(is_system_ui("Siri AI") && is_system_ui("Dock") && !is_system_ui("SecurityAgent"));
        let list = [(5, 20, 101, 100.0, 100.0), (6, 20, 0, 20.0, 20.0), (7, 30, 0, 300.0, 200.0), (8, 20, 0, 300.0, 200.0)];
        assert_eq!(front_window(20, &list), Some(8));
        assert_eq!(front_window(40, &list), None);
    }

    #[test]
    fn classifies_by_role_and_subrole() {
        assert_eq!(classify(&ax("AXWindow", "AXStandardWindow", false)), WindowKind::Normal);
        assert_eq!(classify(&ax("AXWindow", "AXDialog", false)), WindowKind::Dialog);
        assert_eq!(classify(&ax("AXWindow", "AXSystemDialog", false)), WindowKind::Dialog);
        assert_eq!(classify(&ax("AXWindow", "AXFloatingWindow", false)), WindowKind::Dialog);
        assert_eq!(classify(&ax("AXSheet", "", false)), WindowKind::Dialog);
        assert_eq!(classify(&ax("AXWindow", "AXStandardWindow", true)), WindowKind::Dialog);
        assert_eq!(classify(&ax("AXWindow", "", false)), WindowKind::Normal);
        // TextEdit's Open panel and Settings on macOS 27: standard, not minimizable.
        let panel = AxKind { minimizable: false, ..ax("AXWindow", "AXStandardWindow", false) };
        assert_eq!(classify(&panel), WindowKind::Dialog);
        let modal = AxKind { modal: true, ..ax("AXWindow", "AXStandardWindow", false) };
        assert_eq!(classify(&modal), WindowKind::Dialog);
    }

    #[test]
    fn panel_service_names() {
        assert!(is_panel_service("com.apple.appkit.xpc.openAndSavePanelService"));
        assert!(is_panel_service("Open and Save Panel Service (TextEdit)"));
        assert!(!is_panel_service("TextEdit"));
    }

    #[test]
    fn fresh_skips_first_poll_and_non_zero_layers() {
        let mut known = None;
        assert!(fresh(&mut known, &[(1, 0), (2, 101)]).is_empty());
        assert_eq!(fresh(&mut known, &[(1, 0), (3, 0), (4, 101)]), vec![3]);
        assert!(fresh(&mut known, &[(1, 0), (3, 0)]).is_empty());
        // A closed window that reopens with the same id counts again.
        fresh(&mut known, &[(1, 0)]);
        assert_eq!(fresh(&mut known, &[(1, 0), (3, 0)]), vec![3]);
    }
}
