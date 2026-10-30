//! Remote window client: receives streams of individual Mac windows and shows each as a
//! native local window.

#[cfg(not(any(feature = "pyrowave", feature = "h264", feature = "hevc", feature = "mjpeg")))]
compile_error!("umac-client needs at least one codec feature: pyrowave, h264, hevc or mjpeg");

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
#[cfg(target_os = "linux")]
pub mod ipc;
pub mod ids;
pub mod keymap;
pub mod latency;
pub mod manager;
pub mod net;
pub mod pyro;
pub mod render;
#[cfg(feature = "hevc")]
pub mod sample;
pub mod shortcuts;
pub mod shell;
pub mod tray;
pub mod ui;
pub mod viewport;
#[cfg(all(target_os = "linux", any(feature = "h264", feature = "hevc")))]
pub mod vkdevice;
#[cfg(all(target_os = "linux", feature = "h264"))]
pub mod vkvideo;
#[cfg(all(target_os = "linux", feature = "hevc"))]
pub mod zerocopy;
