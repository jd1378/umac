//! Window enumeration (ScreenCaptureKit) and cheap per-window geometry polling (CGWindowList).

use core_foundation::{
    base::{CFType, TCFType},
    dictionary::CFDictionary,
    number::CFNumber,
    string::CFString,
};
use core_graphics::window::{
    copy_window_info, kCGWindowBounds, kCGWindowIsOnscreen, kCGWindowListOptionIncludingWindow, kCGWindowName,
};
use screencapturekit::prelude::*;
use umac_proto::{WindowId, WindowInfo, WindowKind};

use crate::input_state::Placement;

/// Ordinary app windows on screen, excluding our own process and tiny utility windows.
pub fn list() -> anyhow::Result<Vec<WindowInfo>> {
    let content = SCShareableContent::create()
        .with_on_screen_windows_only(true)
        .with_exclude_desktop_windows(true)
        .get()
        .map_err(|e| anyhow::anyhow!("listing windows: {e}"))?;
    let scale = main_display_scale();
    let me = std::process::id() as i32;
    Ok(content
        .windows()
        .into_iter()
        .filter(|w| w.window_layer() == 0)
        .filter_map(|w| {
            let app = w.owning_application()?;
            let f = w.frame();
            if app.process_id() == me || f.size.width < 40.0 || f.size.height < 40.0 {
                return None;
            }
            Some(WindowInfo {
                id: w.window_id(),
                pid: app.process_id(),
                app_name: app.application_name(),
                bundle_id: Some(app.bundle_identifier()).filter(|b| !b.is_empty()),
                title: w.title().unwrap_or_default(),
                width: (f.size.width * scale).round() as u32,
                height: (f.size.height * scale).round() as u32,
                scale: scale as f32,
                parent: None,
                kind: WindowKind::Normal,
            })
        })
        .collect())
}

fn main_display_scale() -> f64 {
    let d = core_graphics::display::CGDisplay::main();
    match d.display_mode() {
        Some(m) if m.width() > 0 => m.pixel_width() as f64 / m.width() as f64,
        _ => 1.0,
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Geometry {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    pub title: String,
    pub on_screen: bool,
}

impl Geometry {
    pub fn placement(&self, scale: f64) -> Placement {
        Placement { x: self.x, y: self.y, width: self.width, height: self.height, scale }
    }
}

fn number(d: &CFDictionary<CFString, CFType>, k: &str) -> Option<f64> {
    d.find(CFString::new(k))?.downcast::<CFNumber>()?.to_f64()
}

/// Current bounds (global points) and title of a window, or `None` once it is gone.
pub fn geometry(id: WindowId) -> Option<Geometry> {
    let list = copy_window_info(kCGWindowListOptionIncludingWindow, id)?;
    let raw = *list.iter().next()?;
    let info: CFDictionary<CFString, CFType> = unsafe { CFDictionary::wrap_under_get_rule(raw as _) };
    let key = |k| unsafe { CFString::wrap_under_get_rule(k) };
    let bounds = info.find(key(unsafe { kCGWindowBounds }))?.downcast::<CFDictionary>()?;
    let bounds: CFDictionary<CFString, CFType> = unsafe { CFDictionary::wrap_under_get_rule(bounds.as_concrete_TypeRef()) };
    let title = info
        .find(key(unsafe { kCGWindowName }))
        .and_then(|v| v.downcast::<CFString>())
        .map(|s| s.to_string())
        .unwrap_or_default();
    let on_screen = info
        .find(key(unsafe { kCGWindowIsOnscreen }))
        .and_then(|v| v.downcast::<core_foundation::boolean::CFBoolean>())
        .is_some_and(bool::from);
    Some(Geometry {
        x: number(&bounds, "X")?,
        y: number(&bounds, "Y")?,
        width: number(&bounds, "Width")?,
        height: number(&bounds, "Height")?,
        title,
        on_screen,
    })
}
