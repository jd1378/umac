//! The Mac user this agent runs for, and whether their desktop is the one on screen. With
//! fast user switching every logged-in user's agent keeps running, but only the user on
//! screen can be captured and controlled.

use std::{
    process::Command,
    sync::atomic::{AtomicBool, Ordering},
};

use core_foundation::{
    base::{CFType, TCFType},
    boolean::CFBoolean,
    dictionary::{CFDictionary, CFDictionaryRef},
    string::CFString,
};
use core_graphics::event::CGEventTapLocation;
use umac_proto::MacSession;

use crate::config;

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGSessionCopyCurrentDictionary() -> CFDictionaryRef;
}

/// Whether this user's desktop is being drawn somewhere it can be captured: the Mac's own
/// screen, or a Screen Sharing session logged in as this user while someone else has the
/// screen.
pub fn on_screen() -> bool {
    let console = on_console();
    CONSOLE.store(console, Ordering::Relaxed);
    console || screen_shared()
}

/// Whether Screen Sharing shows this session. macOS has no API for it, and a switched-out
/// session keeps its (virtual) display, so displays cannot tell. Screen Sharing runs its menu
/// agent in exactly the sessions it shows.
fn screen_shared() -> bool {
    Command::new("pgrep")
        .args(["-qx", "-U", &user(), "SSMenuAgent"])
        .status()
        .is_ok_and(|s| s.success())
}

/// Whether this session had the Mac's own screen at the last `on_screen` check.
static CONSOLE: AtomicBool = AtomicBool::new(true);

/// Where injected input goes. The HID stream reaches whichever session has the Mac's own
/// screen, so a Screen Sharing session gets its input through its session tap instead.
pub fn event_tap() -> CGEventTapLocation {
    if CONSOLE.load(Ordering::Relaxed) { CGEventTapLocation::HID } else { CGEventTapLocation::Session }
}

/// Whether this process's login session owns the Mac's own screen.
fn on_console() -> bool {
    let dict = unsafe { CGSessionCopyCurrentDictionary() };
    if dict.is_null() {
        return false;
    }
    let dict: CFDictionary<CFString, CFType> = unsafe { CFDictionary::wrap_under_create_rule(dict) };
    dict.find(CFString::from_static_string("kCGSSessionOnConsoleKey"))
        .and_then(|v| v.downcast::<CFBoolean>())
        .is_some_and(bool::from)
}

/// The window server's description of this login session, for diagnosing on-screen state.
pub fn details() -> String {
    let dict = unsafe { CGSessionCopyCurrentDictionary() };
    if dict.is_null() {
        return "no window server session".into();
    }
    let dict: CFDictionary<CFString, CFType> = unsafe { CFDictionary::wrap_under_create_rule(dict) };
    let (keys, values) = dict.get_keys_and_values();
    keys.iter()
        .zip(values)
        .map(|(&k, v)| {
            let k = unsafe { CFString::wrap_under_get_rule(k as _) };
            let v = unsafe { CFType::wrap_under_get_rule(v as _) };
            format!("{k}={v:?}")
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn user() -> String {
    std::env::var("USER")
        .ok()
        .filter(|u| !u.is_empty())
        .or_else(|| config::home().file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_default()
}

fn hardware_uuid() -> Option<String> {
    let out = Command::new("ioreg").args(["-rd1", "-c", "IOPlatformExpertDevice"]).output().ok()?;
    let text = String::from_utf8(out.stdout).ok()?;
    let line = text.lines().find(|l| l.contains("\"IOPlatformUUID\""))?;
    let value = line.rsplit('=').next()?.trim().trim_matches('"');
    (!value.is_empty()).then(|| value.to_owned())
}

/// Who this agent is, with the screen state as of now.
pub fn describe() -> MacSession {
    let machine = hardware_uuid().map(|u| config::machine_id(&u)).unwrap_or_else(|| {
        tracing::warn!("no hardware UUID; clients cannot group this Mac's users");
        String::new()
    });
    MacSession { user: user(), machine, on_screen: on_screen() }
}
