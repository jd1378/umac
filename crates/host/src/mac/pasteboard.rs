//! The general pasteboard through the Objective-C runtime (NSPasteboard has no C API).
//! Only plain UTF-8 text is read or written.

use std::ffi::{CStr, c_void};

use core_foundation::{
    base::TCFType,
    string::{CFString, CFStringRef},
};

type Id = *mut c_void;
type Sel = *mut c_void;

#[link(name = "objc")]
unsafe extern "C" {
    fn objc_getClass(name: *const std::ffi::c_char) -> Id;
    fn sel_registerName(name: *const std::ffi::c_char) -> Sel;
    fn objc_msgSend();
    fn objc_autoreleasePoolPush() -> *mut c_void;
    fn objc_autoreleasePoolPop(pool: *mut c_void);
}

#[link(name = "AppKit", kind = "framework")]
unsafe extern "C" {
    /// "public.utf8-plain-text"; an NSString, toll-free bridged to CFString.
    static NSPasteboardTypeString: CFStringRef;
}

/// `NSEvent.keyRepeatDelay` and `keyRepeatInterval` in seconds.
pub fn key_repeat_seconds() -> (f64, f64) {
    unsafe {
        let send: extern "C" fn(Id, Sel) -> f64 = std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
        let class = objc_getClass(c"NSEvent".as_ptr());
        (send(class, sel(c"keyRepeatDelay")), send(class, sel(c"keyRepeatInterval")))
    }
}

fn sel(name: &CStr) -> Sel {
    unsafe { sel_registerName(name.as_ptr()) }
}

/// Runs `f` inside an autorelease pool: the calls return autoreleased objects and our
/// threads have no run loop draining a pool.
fn with_pool<T>(f: impl FnOnce(Id) -> T) -> T {
    unsafe {
        let pool = objc_autoreleasePoolPush();
        let send: extern "C" fn(Id, Sel) -> Id = std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
        let pb = send(objc_getClass(c"NSPasteboard".as_ptr()), sel(c"generalPasteboard"));
        let out = f(pb);
        objc_autoreleasePoolPop(pool);
        out
    }
}

/// Increments on every pasteboard write by anyone. Reading it does not touch the contents,
/// so it never triggers the pasteboard privacy prompt.
pub fn change_count() -> isize {
    with_pool(|pb| unsafe {
        let send: extern "C" fn(Id, Sel) -> isize = std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
        send(pb, sel(c"changeCount"))
    })
}

pub fn read_text() -> Option<String> {
    with_pool(|pb| unsafe {
        let send: extern "C" fn(Id, Sel, Id) -> Id = std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
        let s = send(pb, sel(c"stringForType:"), NSPasteboardTypeString as Id);
        (!s.is_null()).then(|| CFString::wrap_under_get_rule(s as CFStringRef).to_string())
    })
}

/// Replaces the pasteboard contents with `text`; returns the resulting change count.
pub fn write_text(text: &str) -> isize {
    let value = CFString::new(text);
    with_pool(|pb| unsafe {
        let clear: extern "C" fn(Id, Sel) -> isize = std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
        clear(pb, sel(c"clearContents"));
        let set: extern "C" fn(Id, Sel, Id, Id) -> bool = std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
        if !set(pb, sel(c"setString:forType:"), value.as_concrete_TypeRef() as Id, NSPasteboardTypeString as Id) {
            tracing::warn!("writing the pasteboard failed");
        }
        let count: extern "C" fn(Id, Sel) -> isize = std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
        count(pb, sel(c"changeCount"))
    })
}
