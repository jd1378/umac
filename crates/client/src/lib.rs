//! Remote window client: receives streams of individual Mac windows and shows each as a
//! native local window.

#[cfg(target_os = "linux")]
pub mod autostart;
pub mod clipboard;
pub mod config;
pub mod core;
pub mod cursor;
#[cfg(target_os = "linux")]
pub mod daemon;
pub mod decode;
pub mod gate;
pub mod keymap;
pub mod net;
pub mod render;
pub mod sample;
pub mod shortcuts;
pub mod shell;
pub mod tray;
pub mod ui;
pub mod viewport;
#[cfg(target_os = "linux")]
pub mod zerocopy;
