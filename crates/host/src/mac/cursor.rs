//! Reads the Mac system cursor (NSCursor has no C API) and watches it for changes, so the
//! client can show the same shape over streamed windows.

use std::{
    ffi::{CStr, c_void},
    sync::{Arc, Mutex, mpsc},
    time::Instant,
};

use core_graphics::{
    display::CGDisplay,
    geometry::{CGPoint, CGRect, CGSize},
};
use tokio::sync::watch;
use umac_proto::{Cursor, MAX_CURSOR_SIDE, NamedCursor};

use crate::{
    clipboard::FocusTracker,
    cursor::{self, Bitmap, Table},
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
unsafe extern "C" {}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGColorSpaceCreateDeviceRGB() -> *mut c_void;
    fn CGColorSpaceRelease(space: *mut c_void);
    fn CGBitmapContextCreate(data: *mut c_void, w: usize, h: usize, bpc: usize, bpr: usize, space: *mut c_void, info: u32) -> *mut c_void;
    fn CGContextDrawImage(ctx: *mut c_void, rect: CGRect, image: *const c_void);
    fn CGContextRelease(ctx: *mut c_void);
}

const ALPHA_PREMULTIPLIED_LAST: u32 = 1;
const BYTE_ORDER_32_BIG: u32 = 4 << 12;

fn sel(name: &CStr) -> Sel {
    unsafe { sel_registerName(name.as_ptr()) }
}

fn class(name: &CStr) -> Id {
    unsafe { objc_getClass(name.as_ptr()) }
}

unsafe fn send_id(obj: Id, s: &CStr) -> Id {
    let f: extern "C" fn(Id, Sel) -> Id = unsafe { std::mem::transmute(objc_msgSend as unsafe extern "C" fn()) };
    f(obj, sel(s))
}

unsafe fn send_usize(obj: Id, s: &CStr, a: usize) -> Id {
    let f: extern "C" fn(Id, Sel, usize) -> Id = unsafe { std::mem::transmute(objc_msgSend as unsafe extern "C" fn()) };
    f(obj, sel(s), a)
}

unsafe fn send_usize2(obj: Id, s: &CStr, a: usize, b: usize) -> Id {
    let f: extern "C" fn(Id, Sel, usize, usize) -> Id = unsafe { std::mem::transmute(objc_msgSend as unsafe extern "C" fn()) };
    f(obj, sel(s), a, b)
}

unsafe fn send_isize(obj: Id, s: &CStr) -> isize {
    let f: extern "C" fn(Id, Sel) -> isize = unsafe { std::mem::transmute(objc_msgSend as unsafe extern "C" fn()) };
    f(obj, sel(s))
}

unsafe fn responds(obj: Id, s: &CStr) -> bool {
    let f: extern "C" fn(Id, Sel, Sel) -> bool = unsafe { std::mem::transmute(objc_msgSend as unsafe extern "C" fn()) };
    !obj.is_null() && f(obj, sel(c"respondsToSelector:"), sel(s))
}

fn with_pool<T>(f: impl FnOnce() -> T) -> T {
    unsafe {
        let pool = objc_autoreleasePoolPush();
        let out = f();
        objc_autoreleasePoolPop(pool);
        out
    }
}

/// The class cursors only load their images once an NSApplication exists. The agent never
/// shows UI, so it is kept out of the Dock and app switcher.
pub fn init_appkit() {
    with_pool(|| unsafe {
        let app = send_id(class(c"NSApplication"), c"sharedApplication");
        const PROHIBITED: usize = 2;
        let f: extern "C" fn(Id, Sel, usize) -> bool = std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
        f(app, sel(c"setActivationPolicy:"), PROHIBITED);
    });
}

/// Pixels per point of the main display.
pub fn display_scale() -> f64 {
    let d = CGDisplay::main();
    let points = d.bounds().size.width;
    if points > 0.0 { (d.pixels_wide() as f64 / points).max(1.0) } else { 1.0 }
}

/// Renders an NSCursor at `scale` into premultiplied RGBA.
unsafe fn bitmap(cursor: Id, scale: f64) -> Option<Bitmap> {
    if cursor.is_null() {
        return None;
    }
    let image = unsafe { send_id(cursor, c"image") };
    if image.is_null() {
        return None;
    }
    let size: CGSize = unsafe {
        let f: extern "C" fn(Id, Sel) -> CGSize = std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
        f(image, sel(c"size"))
    };
    let hot: CGPoint = unsafe {
        let f: extern "C" fn(Id, Sel) -> CGPoint = std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
        f(cursor, sel(c"hotSpot"))
    };
    let (w, h) = ((size.width * scale).round() as usize, (size.height * scale).round() as usize);
    if w == 0 || h == 0 || w > MAX_CURSOR_SIDE as usize || h > MAX_CURSOR_SIDE as usize {
        return None;
    }
    // The representation drawn for this pixel size, else the largest one.
    let reps = unsafe { send_id(image, c"representations") };
    let count = unsafe { send_isize(reps, c"count") }.max(0) as usize;
    let mut best: Option<(Id, isize)> = None;
    for i in 0..count {
        let rep = unsafe { send_usize(reps, c"objectAtIndex:", i) };
        if !unsafe { responds(rep, c"CGImage") } {
            continue;
        }
        let px = unsafe { send_isize(rep, c"pixelsWide") };
        let exact = px == w as isize;
        if exact || best.is_none_or(|(_, b)| b != w as isize && px > b) {
            best = Some((rep, px));
        }
        if exact {
            break;
        }
    }
    let cg = unsafe {
        match best {
            Some((rep, _)) => send_id(rep, c"CGImage"),
            None => {
                let f: extern "C" fn(Id, Sel, *mut CGRect, Id, Id) -> Id = std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
                f(image, sel(c"CGImageForProposedRect:context:hints:"), std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut())
            }
        }
    };
    if cg.is_null() {
        return None;
    }
    let mut data = vec![0u8; w * h * 4];
    unsafe {
        let space = CGColorSpaceCreateDeviceRGB();
        let ctx = CGBitmapContextCreate(data.as_mut_ptr().cast(), w, h, 8, w * 4, space, ALPHA_PREMULTIPLIED_LAST | BYTE_ORDER_32_BIG);
        CGColorSpaceRelease(space);
        if ctx.is_null() {
            return None;
        }
        CGContextDrawImage(ctx, CGRect::new(&CGPoint::new(0.0, 0.0), &CGSize::new(w as f64, h as f64)), cg);
        CGContextRelease(ctx);
    }
    Some(Bitmap {
        width: w as u32,
        height: h as u32,
        hot_x: (hot.x * scale).round().max(0.0) as u32,
        hot_y: (hot.y * scale).round().max(0.0) as u32,
        premultiplied: data,
    })
}

/// The cursor the Mac shows right now, for whichever app is under the pointer.
pub fn current(scale: f64) -> Option<Bitmap> {
    with_pool(|| unsafe { bitmap(send_id(class(c"NSCursor"), c"currentSystemCursor"), scale) })
}

#[derive(Clone, Copy)]
enum Ctor {
    Class(&'static CStr),
    Directions(&'static CStr, usize),
    Frame(usize, usize),
}

/// Standard cursors in preference order: when two share an image, the first name wins.
fn standard_list() -> Vec<(NamedCursor, Ctor)> {
    use Ctor::*;
    use NamedCursor as N;
    const TOP: usize = 1;
    const LEFT: usize = 2;
    const BOTTOM: usize = 4;
    const RIGHT: usize = 8;
    const INWARD: usize = 1;
    const OUTWARD: usize = 2;
    const ALL: usize = 3;
    let mut list = vec![
        (N::Default, Class(c"arrowCursor")),
        (N::Text, Class(c"IBeamCursor")),
        (N::VerticalText, Class(c"IBeamCursorForVerticalLayout")),
        (N::Pointer, Class(c"pointingHandCursor")),
        (N::Grab, Class(c"openHandCursor")),
        (N::Grabbing, Class(c"closedHandCursor")),
        (N::Crosshair, Class(c"crosshairCursor")),
        (N::NotAllowed, Class(c"operationNotAllowedCursor")),
        (N::Copy, Class(c"dragCopyCursor")),
        (N::Alias, Class(c"dragLinkCursor")),
        (N::ContextMenu, Class(c"contextualMenuCursor")),
        (N::ZoomIn, Class(c"zoomInCursor")),
        (N::ZoomOut, Class(c"zoomOutCursor")),
        (N::ColResize, Class(c"columnResizeCursor")),
        (N::RowResize, Class(c"rowResizeCursor")),
        (N::WResize, Directions(c"columnResizeCursorInDirections:", 1)),
        (N::EResize, Directions(c"columnResizeCursorInDirections:", 2)),
        (N::NResize, Directions(c"rowResizeCursorInDirections:", 1)),
        (N::SResize, Directions(c"rowResizeCursorInDirections:", 2)),
        (N::NsResize, Frame(TOP, ALL)),
        (N::NsResize, Frame(BOTTOM, ALL)),
        (N::EwResize, Frame(LEFT, ALL)),
        (N::EwResize, Frame(RIGHT, ALL)),
        (N::NwseResize, Frame(TOP | LEFT, ALL)),
        (N::NwseResize, Frame(BOTTOM | RIGHT, ALL)),
        (N::NeswResize, Frame(TOP | RIGHT, ALL)),
        (N::NeswResize, Frame(BOTTOM | LEFT, ALL)),
    ];
    for (pos, out, inw) in [
        (TOP, N::NResize, N::SResize),
        (BOTTOM, N::SResize, N::NResize),
        (LEFT, N::WResize, N::EResize),
        (RIGHT, N::EResize, N::WResize),
        (TOP | LEFT, N::NwResize, N::SeResize),
        (TOP | RIGHT, N::NeResize, N::SwResize),
        (BOTTOM | LEFT, N::SwResize, N::NeResize),
        (BOTTOM | RIGHT, N::SeResize, N::NwResize),
    ] {
        list.push((out, Frame(pos, OUTWARD)));
        list.push((inw, Frame(pos, INWARD)));
    }
    // Legacy shapes last: older apps still use them.
    list.extend([
        (N::EwResize, Class(c"resizeLeftRightCursor")),
        (N::NsResize, Class(c"resizeUpDownCursor")),
        (N::WResize, Class(c"resizeLeftCursor")),
        (N::EResize, Class(c"resizeRightCursor")),
        (N::NResize, Class(c"resizeUpCursor")),
        (N::SResize, Class(c"resizeDownCursor")),
    ]);
    list
}

/// Fingerprints of every standard cursor this macOS version has, at `scale`.
pub fn standard(scale: f64) -> Table {
    let ns = class(c"NSCursor");
    let entries: Vec<_> = standard_list()
        .into_iter()
        .filter_map(|(name, ctor)| {
            with_pool(|| unsafe {
                let cursor = match ctor {
                    Ctor::Class(s) if responds(ns, s) => send_id(ns, s),
                    Ctor::Directions(s, d) if responds(ns, s) => send_usize(ns, s, d),
                    Ctor::Frame(p, d) if responds(ns, c"frameResizeCursorFromPosition:inDirections:") => {
                        send_usize2(ns, c"frameResizeCursorFromPosition:inDirections:", p, d)
                    }
                    _ => return None,
                };
                bitmap(cursor, scale).map(|b| (name, b))
            })
        })
        .collect();
    Table::new(entries)
}

/// Watches the cursor on its own thread (AppKit calls can block). Each message on the
/// returned channel is an injected pointer event over a streamed window; checks follow
/// those, plus a slow fallback while a streamed window is focused.
pub fn spawn(focus: Arc<Mutex<FocusTracker>>, out: watch::Sender<Option<Arc<Cursor>>>) -> mpsc::Sender<()> {
    let (tx, rx) = mpsc::channel::<()>();
    std::thread::Builder::new()
        .name("umac-cursor".into())
        .spawn(move || {
            init_appkit();
            let mut scale = display_scale();
            let mut table = standard(scale);
            tracing::debug!(scale, standard = table.len(), "cursor table ready");
            let (mut last_pointer, mut last_check, mut last_id) = (None::<Instant>, Instant::now(), None::<u64>);
            loop {
                let now = Instant::now();
                let focused = focus.lock().unwrap().any();
                let msg = match cursor::next_check(last_pointer, last_check, focused, now) {
                    Some(at) => rx.recv_timeout(at.saturating_duration_since(now)).map_err(|e| e == mpsc::RecvTimeoutError::Disconnected),
                    None => rx.recv().map_err(|_| true),
                };
                match msg {
                    Ok(()) => {
                        last_pointer = Some(Instant::now());
                        continue;
                    }
                    Err(true) => return,
                    Err(false) => {}
                }
                last_check = Instant::now();
                if out.receiver_count() == 0 {
                    continue;
                }
                let s = display_scale();
                if (s - scale).abs() > f64::EPSILON {
                    scale = s;
                    table = standard(scale);
                }
                let Some(bitmap) = current(scale) else { continue };
                let id = bitmap.fingerprint();
                if last_id == Some(id) {
                    continue;
                }
                last_id = Some(id);
                if let Some(c) = table.classify(&bitmap) {
                    tracing::debug!(shape = ?shape_name(&c), "cursor changed");
                    out.send_replace(Some(Arc::new(c)));
                }
            }
        })
        .expect("spawning cursor thread");
    tx
}

fn shape_name(c: &Cursor) -> String {
    match &c.shape {
        umac_proto::CursorShape::Named(n) => format!("{n:?}"),
        umac_proto::CursorShape::Image { width, height, .. } => format!("image {width}x{height}"),
    }
}
