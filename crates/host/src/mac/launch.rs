//! Runs a `LaunchAction` with `/usr/bin/open` and waits for the window it shows.

use std::{collections::HashSet, process::Command, time::Instant};

use anyhow::{bail, Context};
use umac_proto::{LaunchAction, WindowInfo};

use super::windows;
use crate::{
    config,
    launch::{self, Reuse, Seen},
};

const POLL: std::time::Duration = std::time::Duration::from_millis(50);

fn seen() -> Vec<Seen> {
    windows::on_screen().into_iter().map(|w| Seen { id: w.id, layer: w.layer, width: w.width, height: w.height }).collect()
}

/// Blocking: launches and returns the window to stream. Safe to run concurrently; each call
/// only looks for windows that appeared after its own snapshot.
pub fn run(action: LaunchAction) -> anyhow::Result<WindowInfo> {
    let home = config::home();
    let plan = launch::plan(action, &home.to_string_lossy());
    let started = Instant::now();
    let before: HashSet<_> = seen().iter().map(|w| w.id).collect();
    let status = Command::new("/usr/bin/open").args(&plan.open_args).status().context("running open")?;
    if !status.success() {
        bail!("open {:?} failed: {status}", plan.open_args);
    }
    let opened_ms = started.elapsed().as_millis() as u64;
    loop {
        let now = seen();
        let fresh = launch::new_windows(&before, &now);
        let elapsed = started.elapsed();
        let reuse = plan.reuse != Reuse::Never && elapsed >= launch::REUSE_AFTER;
        if !fresh.is_empty() || reuse {
            let list = windows::list()?;
            let of_app = |id| list.iter().find(|w| w.id == id && w.bundle_id.as_deref() == Some(plan.bundle_id));
            if let Some(info) = fresh.iter().find_map(|&id| of_app(id)) {
                tracing::info!(?action, window = info.id, opened_ms, ms = elapsed.as_millis() as u64, "launched: new window");
                return Ok(info.clone());
            }
            if reuse {
                let existing = now.iter().filter(|w| w.layer == 0).filter_map(|w| of_app(w.id)).find(|w| match plan.reuse {
                    Reuse::Titled(t) => w.title == t,
                    _ => true,
                });
                if let Some(info) = existing {
                    tracing::info!(?action, window = info.id, ms = elapsed.as_millis() as u64, "launched: existing window");
                    return Ok(info.clone());
                }
            }
        }
        // On macOS 27 the Apps launcher is a system overlay, not a window. It cannot be
        // streamed and it holds the keyboard, so close it and show /Applications instead.
        if action == LaunchAction::Apps && !launch::new_overlays(&before, &now).is_empty() {
            dismiss_overlay();
            tracing::info!(ms = elapsed.as_millis() as u64, "Apps opened as an overlay; dismissed it, opening Finder on Applications");
            return run(LaunchAction::FinderApplications);
        }
        if elapsed >= launch::TIMEOUT {
            let overlays = launch::new_overlays(&before, &now);
            tracing::info!(?action, ?overlays, "launch showed no streamable window");
            if let Some(o) = overlays.first() {
                bail!(
                    "{action:?} opened as an overlay (layer {}, {}x{} points), not a window that can be streamed",
                    o.layer,
                    o.width,
                    o.height
                );
            }
            bail!("{action:?} showed no window within {} s", launch::TIMEOUT.as_secs());
        }
        std::thread::sleep(POLL);
    }
}

/// Escape closes the launcher overlay, which has the keyboard while it is open.
fn dismiss_overlay() {
    use core_graphics::{
        event::CGEvent,
        event_source::{CGEventSource, CGEventSourceStateID},
    };
    let Ok(source) = CGEventSource::new(CGEventSourceStateID::HIDSystemState) else { return };
    for down in [true, false] {
        if let Ok(ev) = CGEvent::new_keyboard_event(source.clone(), umac_proto::keys::kVK_Escape, down) {
            ev.post(super::session::event_tap());
        }
    }
}
