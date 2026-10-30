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

#[cfg(test)]
mod tests {
    use super::*;

    fn ax(role: &str, subrole: &str, sheet: bool) -> AxKind {
        AxKind { role: role.into(), subrole: subrole.into(), sheet, modal: false, minimizable: true }
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
