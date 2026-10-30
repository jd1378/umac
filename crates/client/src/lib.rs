//! Remote window client: receives streams of individual Mac windows and shows each as a
//! native local window.

pub mod app;
pub mod config;
pub mod decode;
pub mod gate;
pub mod keymap;
pub mod net;
pub mod render;
pub mod sample;
pub mod viewport;
