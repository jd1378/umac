//! Popup rules: which windows count as popups (menus, dropdown lists, tooltips, popovers),
//! where they sit relative to their parent, and which streamed window owns them.

use umac_proto::WindowId;

/// `kCGMainMenuWindowLevel` and `kCGStatusWindowLevel`: the menu bar and its extras.
const MENU_BAR_LAYERS: [i32; 2] = [24, 25];
/// Above this are system overlays (screen saver, assistive UI, cursor), never app popups.
const MAX_LAYER: i32 = 1000;
const MIN_SIZE: f64 = 4.0;

/// Whether an app-owned on-screen window at `layer` is a popup. Layer 0 windows are normal
/// windows, sheets and dialogs; negative layers are desktop elements.
pub fn is_popup(layer: i32, width: f64, height: f64) -> bool {
    layer > 0 && layer < MAX_LAYER && !MENU_BAR_LAYERS.contains(&layer) && width >= MIN_SIZE && height >= MIN_SIZE
}

/// Popup top-left relative to the parent's captured surface origin, in parent surface pixels.
pub fn offset(popup: (f64, f64), parent_origin: (f64, f64), scale: f64) -> (i32, i32) {
    (((popup.0 - parent_origin.0) * scale).round() as i32, ((popup.1 - parent_origin.1) * scale).round() as i32)
}

/// A streamed window as seen by parent selection.
#[derive(Debug, Clone, Copy)]
pub struct Candidate {
    pub id: WindowId,
    pub pid: i32,
    /// Set for popups: the window they belong to.
    pub popup_of: Option<WindowId>,
}

/// Picks the window a new popup of `pid` belongs to: the window that last got input or focus
/// (a popup resolves to its own parent, so submenus stay on the same parent), else the
/// lowest-id normal window of that app.
pub fn choose_parent(pid: i32, active: Option<WindowId>, streams: &[Candidate]) -> Option<WindowId> {
    let find = |id| streams.iter().find(|c| c.id == id);
    let active = active.and_then(find).map(|c| c.popup_of.and_then(find).unwrap_or(c));
    if let Some(c) = active.filter(|c| c.pid == pid && c.popup_of.is_none()) {
        return Some(c.id);
    }
    streams.iter().filter(|c| c.pid == pid && c.popup_of.is_none()).map(|c| c.id).min()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_layers() {
        assert!(is_popup(101, 200.0, 300.0)); // menus
        assert!(is_popup(3, 50.0, 20.0)); // floating panels, tooltips
        assert!(!is_popup(0, 800.0, 600.0)); // normal windows and sheets
        assert!(!is_popup(24, 1920.0, 30.0)); // menu bar
        assert!(!is_popup(25, 30.0, 30.0)); // menu bar extras
        assert!(!is_popup(-2147483601, 180.0, 180.0)); // desktop widgets
        assert!(!is_popup(2147483630, 28.0, 28.0)); // system overlays
        assert!(!is_popup(101, 3.0, 300.0)); // slivers
    }

    #[test]
    fn offset_scales_and_allows_negative() {
        assert_eq!(offset((150.0, 260.0), (100.0, 200.0), 2.0), (100, 120));
        assert_eq!(offset((90.0, 190.5), (100.0, 200.0), 1.0), (-10, -10));
    }

    #[test]
    fn parent_prefers_active_window_of_same_app() {
        let s = [
            Candidate { id: 10, pid: 1, popup_of: None },
            Candidate { id: 11, pid: 1, popup_of: None },
            Candidate { id: 20, pid: 2, popup_of: None },
            Candidate { id: 30, pid: 1, popup_of: Some(11) },
        ];
        assert_eq!(choose_parent(1, Some(11), &s), Some(11));
        // Input on a popup (submenu opening) maps to that popup's parent.
        assert_eq!(choose_parent(1, Some(30), &s), Some(11));
        // Active window belongs to another app: fall back to the lowest id of this app.
        assert_eq!(choose_parent(1, Some(20), &s), Some(10));
        assert_eq!(choose_parent(1, None, &s), Some(10));
        assert_eq!(choose_parent(3, Some(10), &s), None);
    }
}
