//! Launch rules: what each `LaunchAction` runs and which window answers it. The commands are
//! fixed; nothing from the client reaches `open` except the choice of action.

use std::{collections::HashSet, time::Duration};

use umac_proto::{LaunchAction, WindowId};

/// How long a launch may take to show its window.
pub const TIMEOUT: Duration = Duration::from_secs(5);
/// How long to wait for a new window before settling for an existing one, where allowed.
pub const REUSE_AFTER: Duration = Duration::from_millis(1500);
/// Smaller layer-0 windows are utility panels, not app windows.
const MIN_SIZE: f64 = 40.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reuse {
    /// Only a window that did not exist before counts.
    Never,
    /// Fall back to the app's frontmost existing window.
    Any,
    /// Fall back to an existing window with this title (Finder already showing the folder).
    Titled(&'static str),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// Arguments to `/usr/bin/open`. Bundle ids, not names, so localization does not matter.
    pub open_args: Vec<String>,
    pub bundle_id: &'static str,
    pub reuse: Reuse,
}

pub fn plan(action: LaunchAction, home: &str) -> Plan {
    let (open_args, bundle_id, reuse) = match action {
        LaunchAction::Terminal => (vec!["-b", "com.apple.Terminal", home], "com.apple.Terminal", Reuse::Never),
        LaunchAction::ActivityMonitor => (vec!["-b", "com.apple.ActivityMonitor"], "com.apple.ActivityMonitor", Reuse::Any),
        LaunchAction::Apps => (vec!["-b", "com.apple.apps.launcher"], "com.apple.apps.launcher", Reuse::Any),
        LaunchAction::FinderApplications => (vec!["/Applications"], "com.apple.finder", Reuse::Titled("Applications")),
    };
    Plan { open_args: open_args.into_iter().map(str::to_owned).collect(), bundle_id, reuse }
}

/// An on-screen window as far as launch detection cares.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Seen {
    pub id: WindowId,
    pub layer: i32,
    pub width: f64,
    pub height: f64,
}

/// Normal-size layer-0 windows that were not on screen before, front to back.
pub fn new_windows(before: &HashSet<WindowId>, now: &[Seen]) -> Vec<WindowId> {
    now.iter()
        .filter(|w| w.layer == 0 && w.width >= MIN_SIZE && w.height >= MIN_SIZE && !before.contains(&w.id))
        .map(|w| w.id)
        .collect()
}

/// New surfaces at other layers (overlays, panels), for explaining a launch that showed no
/// streamable window.
pub fn new_overlays(before: &HashSet<WindowId>, now: &[Seen]) -> Vec<Seen> {
    now.iter().filter(|w| w.layer != 0 && !before.contains(&w.id)).copied().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seen(id: WindowId, layer: i32, size: f64) -> Seen {
        Seen { id, layer, width: size, height: size }
    }

    #[test]
    fn plans_use_fixed_bundle_ids() {
        let p = plan(LaunchAction::Terminal, "/Users/me");
        assert_eq!(p.open_args, ["-b", "com.apple.Terminal", "/Users/me"]);
        assert_eq!(p.reuse, Reuse::Never);
        assert_eq!(plan(LaunchAction::FinderApplications, "/x").open_args, ["/Applications"]);
        assert_eq!(plan(LaunchAction::Apps, "/x").bundle_id, "com.apple.apps.launcher");
    }

    #[test]
    fn detects_only_new_normal_windows_in_order() {
        let before = HashSet::from([1, 2]);
        let now = [seen(9, 0, 500.0), seen(1, 0, 500.0), seen(7, 0, 20.0), seen(8, 101, 300.0), seen(5, 0, 300.0)];
        assert_eq!(new_windows(&before, &now), vec![9, 5]);
        assert_eq!(new_overlays(&before, &now), vec![seen(8, 101, 300.0)]);
    }
}
