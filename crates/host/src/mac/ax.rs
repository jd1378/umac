//! Accessibility helpers: raise/focus and resize a window owned by another app.

use std::{ffi::c_void, ptr};

use anyhow::{anyhow, bail};
use core_foundation::{
    array::CFArray,
    base::{CFType, TCFType},
    boolean::CFBoolean,
    dictionary::CFDictionary,
    string::CFString,
};
use core_graphics::geometry::{CGPoint, CGSize};
use umac_proto::WindowId;

use super::ffi::*;
use crate::clipboard::MenuItem;

/// Owned AX element.
struct Element(AXUIElementRef);

impl Drop for Element {
    fn drop(&mut self) {
        unsafe { core_foundation::base::CFRelease(self.0) }
    }
}

fn attr(el: AXUIElementRef, name: &str) -> Option<CFType> {
    let mut out: CFTypeRef = ptr::null();
    let err = unsafe { AXUIElementCopyAttributeValue(el, CFString::new(name).as_concrete_TypeRef(), &mut out) };
    (err == 0 && !out.is_null()).then(|| unsafe { CFType::wrap_under_create_rule(out) })
}

fn set_attr(el: AXUIElementRef, name: &str, value: CFTypeRef) -> anyhow::Result<()> {
    let err = unsafe { AXUIElementSetAttributeValue(el, CFString::new(name).as_concrete_TypeRef(), value) };
    if err != 0 {
        bail!("setting {name}: AXError {err}");
    }
    Ok(())
}

fn point(el: AXUIElementRef) -> Option<(f64, f64)> {
    let v = attr(el, "AXPosition")?;
    let mut p = CGPoint::new(0.0, 0.0);
    unsafe { AXValueGetValue(v.as_CFTypeRef(), kAXValueCGPointType, (&mut p as *mut CGPoint).cast::<c_void>()) }
        .then_some((p.x, p.y))
}

fn role(el: AXUIElementRef) -> String {
    attr(el, "AXRole").and_then(|v| v.downcast::<CFString>()).map(|s| s.to_string()).unwrap_or_default()
}

fn menu_item(el: AXUIElementRef) -> MenuItem {
    MenuItem {
        title: attr(el, "AXTitle").and_then(|v| v.downcast::<CFString>()).map(|s| s.to_string()).unwrap_or_default(),
        cmd_char: attr(el, "AXMenuItemCmdChar").and_then(|v| v.downcast::<CFString>()).map(|s| s.to_string()),
        modifiers: attr(el, "AXMenuItemCmdModifiers")
            .and_then(|v| v.downcast::<core_foundation::number::CFNumber>())
            .and_then(|n| n.to_i64()),
    }
}

fn children(el: AXUIElementRef) -> Vec<CFType> {
    attr(el, "AXChildren")
        .map(|c| unsafe { CFArray::<CFType>::wrap_under_get_rule(c.as_CFTypeRef() as _) }.iter().map(|c| c.clone()).collect())
        .unwrap_or_default()
}

/// Titles of the app's menu-bar items whose shortcut is Cmd+V (Edit > Paste and friends).
pub fn paste_titles(pid: i32) -> Vec<String> {
    let app = Element(unsafe { AXUIElementCreateApplication(pid) });
    let Some(bar) = attr(app.0, "AXMenuBar") else { return Vec::new() };
    let mut out = Vec::new();
    // Menu bar > bar items > menus > items; submenus are not needed for Paste.
    for top in children(bar.as_CFTypeRef()) {
        for menu in children(top.as_CFTypeRef()) {
            for el in children(menu.as_CFTypeRef()) {
                let item = menu_item(el.as_CFTypeRef());
                if item.has_paste_shortcut() && !item.title.is_empty() {
                    out.push(item.title);
                }
            }
        }
    }
    tracing::debug!(pid, ?out, "paste titles");
    out
}

/// The menu item under a global point, or `None` when no menu item is there.
/// The menu item under a global point and the pid of the app that owns it.
/// Pid of the app that has keyboard focus, asked from the window server each time.
/// `NSWorkspace.frontmostApplication` only updates on a running main run loop, which this
/// agent does not have, so it goes stale.
pub fn focused_app_pid() -> Option<i32> {
    focused_window().map(|(pid, _)| pid)
}

/// The focused app and, when Accessibility knows it, its focused (or main) window.
pub fn focused_window() -> Option<(i32, Option<WindowId>)> {
    let system = Element(unsafe { AXUIElementCreateSystemWide() });
    let app = attr(system.0, "AXFocusedApplication")?;
    let mut pid = 0;
    if unsafe { AXUIElementGetPid(app.as_CFTypeRef(), &mut pid) } != 0 || pid <= 0 {
        return None;
    }
    let window = ["AXFocusedWindow", "AXMainWindow"].iter().find_map(|name| {
        let w = attr(app.as_CFTypeRef(), name)?;
        let mut id = 0u32;
        (unsafe { _AXUIElementGetWindow(w.as_CFTypeRef(), &mut id) } == 0 && id != 0).then_some(id)
    });
    Some((pid, window))
}

pub fn menu_item_at(x: f64, y: f64) -> Option<(MenuItem, i32)> {
    let system = Element(unsafe { AXUIElementCreateSystemWide() });
    let mut hit: AXUIElementRef = ptr::null();
    if unsafe { AXUIElementCopyElementAtPosition(system.0, x as f32, y as f32, &mut hit) } != 0 || hit.is_null() {
        return None;
    }
    let mut el = unsafe { CFType::wrap_under_create_rule(hit) };
    // The hit may be the item's title text or image; menu items are a level or two up.
    for _ in 0..4 {
        if role(el.as_CFTypeRef()) == "AXMenuItem" {
            let item = menu_item(el.as_CFTypeRef());
            let mut pid = 0;
            unsafe { AXUIElementGetPid(el.as_CFTypeRef(), &mut pid) };
            tracing::debug!(?item, pid, "menu item under pointer");
            return Some((item, pid));
        }
        el = attr(el.as_CFTypeRef(), "AXParent")?;
    }
    None
}

fn size_of(el: AXUIElementRef) -> Option<(f64, f64)> {
    let v = attr(el, "AXSize")?;
    let mut s = CGSize::new(0.0, 0.0);
    unsafe { AXValueGetValue(v.as_CFTypeRef(), kAXValueCGSizeType, (&mut s as *mut CGSize).cast::<c_void>()) }
        .then_some((s.width, s.height))
}

/// Logs every item of the menu under a global point with its shortcut and frame, for
/// diagnosing which items count as Paste. Debug level only.
pub fn log_menu_items(x: f64, y: f64) {
    let system = Element(unsafe { AXUIElementCreateSystemWide() });
    let mut hit: AXUIElementRef = ptr::null();
    if unsafe { AXUIElementCopyElementAtPosition(system.0, x as f32, y as f32, &mut hit) } != 0 || hit.is_null() {
        return;
    }
    let mut el = unsafe { CFType::wrap_under_create_rule(hit) };
    for _ in 0..5 {
        if role(el.as_CFTypeRef()) == "AXMenu" {
            break;
        }
        let Some(parent) = attr(el.as_CFTypeRef(), "AXParent") else { return };
        el = parent;
    }
    for c in children(el.as_CFTypeRef()) {
        let r = c.as_CFTypeRef();
        tracing::debug!(item = ?menu_item(r), pos = ?point(r), size = ?size_of(r), "menu item");
    }
}

/// Tallest title bar we are willing to crop, in points. Anything deeper is real content.
const MAX_TITLEBAR: f64 = 64.0;

/// Role, subrole and whether `id` is a sheet attached to another window of `pid`. Sheets are
/// not in the app's window list, so their parents' children are searched too.
pub fn window_kind(pid: i32, id: WindowId) -> Option<crate::newwin::AxKind> {
    let app = Element(unsafe { AXUIElementCreateApplication(pid) });
    let windows = attr(app.0, "AXWindows")?;
    let windows: CFArray<CFType> = unsafe { CFArray::wrap_under_get_rule(windows.as_CFTypeRef() as _) };
    let window_id = |el: AXUIElementRef| {
        let mut wid = 0u32;
        (unsafe { _AXUIElementGetWindow(el, &mut wid) } == 0).then_some(wid)
    };
    let flag = |el: AXUIElementRef, name: &str| attr(el, name).and_then(|v| v.downcast::<CFBoolean>()).is_some_and(bool::from);
    let kind = |el: AXUIElementRef, sheet: bool| crate::newwin::AxKind {
        role: role(el),
        subrole: attr(el, "AXSubrole").and_then(|v| v.downcast::<CFString>()).map(|s| s.to_string()).unwrap_or_default(),
        sheet,
        modal: flag(el, "AXModal"),
        minimizable: attr(el, "AXMinimizeButton").is_some_and(|b| flag(b.as_CFTypeRef(), "AXEnabled")),
    };
    for w in windows.iter() {
        if window_id(w.as_CFTypeRef()) == Some(id) {
            return Some(kind(w.as_CFTypeRef(), false));
        }
    }
    for w in windows.iter() {
        for c in children(w.as_CFTypeRef()) {
            if role(c.as_CFTypeRef()) == "AXSheet" && window_id(c.as_CFTypeRef()) == Some(id) {
                return Some(kind(c.as_CFTypeRef(), true));
            }
        }
    }
    None
}

pub struct AxWindow {
    app: Element,
    window: CFType,
}

impl AxWindow {
    pub fn find(pid: i32, id: WindowId) -> anyhow::Result<Self> {
        let app = Element(unsafe { AXUIElementCreateApplication(pid) });
        let windows = attr(app.0, "AXWindows").ok_or_else(|| anyhow!("no AX windows for pid {pid} (Accessibility allowed?)"))?;
        let windows: CFArray<CFType> = unsafe { CFArray::wrap_under_get_rule(windows.as_CFTypeRef() as _) };
        for w in windows.iter() {
            let mut wid = 0u32;
            if unsafe { _AXUIElementGetWindow(w.as_CFTypeRef(), &mut wid) } == 0 && wid == id {
                return Ok(Self { app, window: w.clone() });
            }
        }
        bail!("window {id} not found via Accessibility")
    }

    fn el(&self) -> AXUIElementRef {
        self.window.as_CFTypeRef()
    }

    /// Makes the app frontmost and the window key, so injected events land in it.
    pub fn focus(&self) -> anyhow::Result<()> {
        let t = CFBoolean::true_value();
        set_attr(self.app.0, "AXFrontmost", t.as_CFTypeRef())?;
        let _ = set_attr(self.el(), "AXMain", t.as_CFTypeRef());
        let err = unsafe { AXUIElementPerformAction(self.el(), CFString::new("AXRaise").as_concrete_TypeRef()) };
        if err != 0 {
            bail!("AXRaise: AXError {err}");
        }
        Ok(())
    }

    /// Presses the window's close button, as a click on the red traffic light would.
    pub fn close(&self) -> anyhow::Result<()> {
        let button = attr(self.el(), "AXCloseButton").ok_or_else(|| anyhow!("window has no close button"))?;
        let err = unsafe { AXUIElementPerformAction(button.as_CFTypeRef(), CFString::new("AXPress").as_concrete_TypeRef()) };
        if err != 0 {
            bail!("pressing the close button: AXError {err}");
        }
        Ok(())
    }

    /// Height in points of a plain title bar above the window content, or 0 when the top of
    /// the window is interactive (unified toolbar, full-size content view) or unknown.
    /// The client draws its own title bar, so a plain macOS one would just be duplicated.
    pub fn titlebar_inset(&self) -> f64 {
        let Some((_, top)) = point(self.el()) else { return 0.0 };
        let Some(children) = attr(self.el(), "AXChildren") else { return 0.0 };
        let children: CFArray<CFType> = unsafe { CFArray::wrap_under_get_rule(children.as_CFTypeRef() as _) };
        let mut content_top: Option<f64> = None;
        for child in children.iter() {
            let el = child.as_CFTypeRef();
            let role = role(el);
            let y = point(el).map(|(_, y)| y - top);
            tracing::debug!(%role, ?y, "window child");
            match role.as_str() {
                "AXToolbar" => return 0.0,
                // Traffic lights, title text and proxy icon live inside the title bar.
                "AXButton" | "AXStaticText" | "AXImage" => {}
                _ => {
                    if let Some(y) = y {
                        content_top = Some(content_top.map_or(y, |c: f64| c.min(y)));
                    }
                }
            }
        }
        content_top.filter(|&y| y > 0.0 && y <= MAX_TITLEBAR).unwrap_or(0.0)
    }

    pub fn size(&self) -> Option<(f64, f64)> {
        let v = attr(self.el(), "AXSize")?;
        let mut s = CGSize::new(0.0, 0.0);
        unsafe { AXValueGetValue(v.as_CFTypeRef(), kAXValueCGSizeType, (&mut s as *mut CGSize).cast::<c_void>()) }
            .then_some((s.width, s.height))
    }

    /// Requests a new size in points and returns the size the app actually accepted.
    pub fn resize(&self, width: f64, height: f64) -> anyhow::Result<(f64, f64)> {
        let s = CGSize::new(width, height);
        let v = unsafe { CFType::wrap_under_create_rule(AXValueCreate(kAXValueCGSizeType, (&s as *const CGSize).cast())) };
        set_attr(self.el(), "AXSize", v.as_CFTypeRef())?;
        self.size().ok_or_else(|| anyhow!("reading back AXSize"))
    }
}

pub fn is_trusted() -> bool {
    unsafe { AXIsProcessTrusted() }
}

/// Shows the system "allow Accessibility" prompt if not yet trusted.
pub fn prompt_trust() -> bool {
    let key = unsafe { CFString::wrap_under_get_rule(kAXTrustedCheckOptionPrompt) };
    let opts = CFDictionary::from_CFType_pairs(&[(key, CFBoolean::true_value())]);
    unsafe { AXIsProcessTrustedWithOptions(opts.as_concrete_TypeRef()) }
}
