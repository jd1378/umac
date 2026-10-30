//! Windowing shells. Each owns the native windows and raw input and drives [`Core`].

#[cfg(target_os = "linux")]
pub mod wayland;
pub mod winit;

use crate::{core::Core, ui::UiSender};

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Backend {
    /// Wayland when available, else winit.
    Auto,
    /// Native Wayland (supports menus and popups placed by the compositor).
    Wayland,
    /// winit (X11, other platforms).
    Winit,
}

impl Backend {
    /// Resolves `Auto` from the session's `WAYLAND_DISPLAY`.
    pub fn resolve(self, wayland_display: Option<&str>) -> Backend {
        match self {
            Backend::Auto if cfg!(target_os = "linux") && wayland_display.is_some_and(|d| !d.is_empty()) => Backend::Wayland,
            Backend::Auto => Backend::Winit,
            b => b,
        }
    }
}

pub enum Shell {
    #[cfg(target_os = "linux")]
    Wayland(Box<wayland::Shell>),
    Winit(Box<winit::Shell>),
}

impl Shell {
    pub fn new(backend: Backend) -> anyhow::Result<Self> {
        let wayland_display = std::env::var("WAYLAND_DISPLAY").ok();
        match backend.resolve(wayland_display.as_deref()) {
            #[cfg(target_os = "linux")]
            Backend::Wayland => Ok(Shell::Wayland(Box::new(wayland::Shell::new()?))),
            #[cfg(not(target_os = "linux"))]
            Backend::Wayland => anyhow::bail!("the Wayland backend is only available on Linux"),
            _ => Ok(Shell::Winit(Box::new(winit::Shell::new()?))),
        }
    }

    pub fn sender(&self) -> UiSender {
        match self {
            #[cfg(target_os = "linux")]
            Shell::Wayland(s) => s.sender(),
            Shell::Winit(s) => s.sender(),
        }
    }

    /// Runs until every window is closed; returns the error that ended the session, if any.
    pub fn run(self, core: Core) -> anyhow::Result<Option<String>> {
        match self {
            #[cfg(target_os = "linux")]
            Shell::Wayland(s) => s.run(core),
            Shell::Winit(s) => s.run(core),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_picks_wayland_only_with_a_display() {
        let wayland = if cfg!(target_os = "linux") { Backend::Wayland } else { Backend::Winit };
        assert_eq!(Backend::Auto.resolve(Some("wayland-0")), wayland);
        assert_eq!(Backend::Auto.resolve(Some("")), Backend::Winit);
        assert_eq!(Backend::Auto.resolve(None), Backend::Winit);
        assert_eq!(Backend::Winit.resolve(Some("wayland-0")), Backend::Winit);
        assert_eq!(Backend::Wayland.resolve(None), Backend::Wayland);
    }
}
