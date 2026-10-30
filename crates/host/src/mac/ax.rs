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

/// Tallest title bar we are willing to crop, in points. Anything deeper is real content.
const MAX_TITLEBAR: f64 = 64.0;

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
