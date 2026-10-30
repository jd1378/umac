//! Windowing-independent client state: one entry per shown remote window with its renderer,
//! input translation, resize debounce and size snapping. Shells own the native windows and
//! feed raw events in; everything that talks to the host goes through here.

use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use tokio::sync::mpsc::UnboundedSender;
use umac_proto::{ClientMsg, InputEvent, MouseButton, WindowId, WindowInfo, WindowKind};

use crate::{
    clipboard::PasteFlow,
    config::Config,
    cursor::{Cursors, Shape},
    decode::Picture,
    net::NetCmd,
    render::{Gpu, WindowRenderer},
    shortcuts::{Profile, Translator, is_terminal},
    viewport::Fit,
};

const RESIZE_DEBOUNCE: Duration = Duration::from_millis(150);
/// Wayland axis value of one wheel notch at the desktop's default scroll speed (libinput).
pub const WHEEL_NOTCH_VALUE: f64 = 15.0;
/// Popups shown at once; more are refused (a misbehaving host could otherwise flood windows).
pub const MAX_POPUPS: usize = 16;
/// Popup offsets beyond this are treated as garbage.
const MAX_OFFSET: i32 = 1 << 14;

struct Remote {
    info: WindowInfo,
    renderer: WindowRenderer,
    cursor: (f64, f64),
    size: (u32, u32),
    resize_due: Option<Instant>,
    presented: u32,
    /// Time from decode finished to present submitted, and the share spent on the UI thread.
    present_lag: Duration,
    ui_time: Duration,
    stats_since: Instant,
}

pub struct Core {
    cfg: Config,
    net: UnboundedSender<NetCmd>,
    instance: wgpu::Instance,
    gpu: Option<Gpu>,
    remotes: HashMap<WindowId, Remote>,
    opened_any: bool,
    paste: PasteFlow,
    keys: Translator,
    cursors: Cursors,
    pub error: Option<String>,
}

pub fn title(info: &WindowInfo) -> String {
    if info.title.is_empty() { info.app_name.clone() } else { format!("{}: {}", info.app_name, info.title) }
}

/// Popup offset as sent by the host, clamped to a sane range.
pub fn popup_offset(info: &WindowInfo) -> (i32, i32) {
    (info.offset.0.clamp(-MAX_OFFSET, MAX_OFFSET), info.offset.1.clamp(-MAX_OFFSET, MAX_OFFSET))
}

/// Top-level kinds the user opened; the session ends when none are left.
fn is_toplevel(kind: WindowKind) -> bool {
    matches!(kind, WindowKind::Normal | WindowKind::Desktop)
}

/// Size to open a window at. Windows match the remote surface exactly; a desktop is capped
/// to 90% of the local output so a large Mac display still fits on screen.
pub fn initial_size(info: &WindowInfo, output: Option<(u32, u32)>) -> (u32, u32) {
    let size = (info.width.max(1), info.height.max(1));
    let Some((ow, oh)) = output.filter(|_| info.kind == WindowKind::Desktop) else { return size };
    let (max_w, max_h) = (ow as f64 * 0.9, oh as f64 * 0.9);
    let scale = (max_w / size.0 as f64).min(max_h / size.1 as f64).min(1.0);
    (((size.0 as f64 * scale).round() as u32).max(1), ((size.1 as f64 * scale).round() as u32).max(1))
}

/// What a shell should do after the host changed a window.
pub struct Changed {
    pub title: String,
    /// The host settled on a different size and the user is not resizing: adopt it.
    pub snap: Option<(u32, u32)>,
    /// Popup moved or resized relative to its parent.
    pub reposition: bool,
}

impl Core {
    pub fn new(cfg: Config, net: UnboundedSender<NetCmd>) -> Self {
        Self {
            cfg: cfg.clone(),
            net,
            instance: wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle()),
            gpu: None,
            remotes: HashMap::new(),
            opened_any: false,
            paste: PasteFlow::new(cfg.clipboard_to_mac),
            keys: Translator::new(cfg.swap_ctrl_cmd),
            cursors: Cursors::default(),
            error: None,
        }
    }

    /// Instance shells create their wgpu surfaces from.
    pub fn instance(&self) -> &wgpu::Instance {
        &self.instance
    }

    fn send(&self, cmd: NetCmd) {
        let _ = self.net.send(cmd);
    }

    /// All input goes through here so a pending paste keeps it in order.
    fn input(&mut self, ev: InputEvent) {
        for ev in self.paste.input(ev) {
            self.send(NetCmd::Input(ev));
        }
    }

    pub fn clipboard_to_pc(&self) -> bool {
        self.cfg.clipboard_to_pc
    }

    /// True while the shell should read the local clipboard and report it to `clipboard_read`.
    pub fn wants_clipboard(&self) -> bool {
        self.paste.wants_read()
    }

    pub fn clipboard_read(&mut self, text: Option<String>) {
        for ev in self.paste.read_done(text) {
            self.send(NetCmd::Input(ev));
        }
    }

    pub fn clipboard_request(&mut self, nonce: u64) {
        for ev in self.paste.request(nonce) {
            self.send(NetCmd::Input(ev));
        }
    }

    pub fn contains(&self, window: WindowId) -> bool {
        self.remotes.contains_key(&window)
    }

    pub fn info(&self, window: WindowId) -> Option<&WindowInfo> {
        self.remotes.get(&window).map(|r| &r.info)
    }

    pub fn popup_count(&self) -> usize {
        self.remotes.values().filter(|r| r.info.kind == WindowKind::Popup).count()
    }

    /// Starts presenting `info` on `surface`, which must be ready for a first buffer.
    pub fn attach(&mut self, info: WindowInfo, surface: wgpu::Surface<'static>, size: (u32, u32)) -> anyhow::Result<()> {
        if self.gpu.is_none() {
            self.gpu = Some(Gpu::with_instance(&self.instance, Some(&surface))?);
        }
        let gpu = self.gpu.as_mut().unwrap();
        let mut renderer = WindowRenderer::new(gpu, surface, size.0, size.1);
        if info.kind == WindowKind::Desktop {
            renderer.set_fit(Fit::Scale);
        }
        // Wayland maps a window only after its first buffer.
        renderer.render(gpu);
        if is_toplevel(info.kind) {
            self.opened_any = true;
        }
        self.remotes.insert(
            info.id,
            Remote { info, renderer, cursor: (0.0, 0.0), size, resize_due: None,
                presented: 0,
                present_lag: Duration::ZERO,
                ui_time: Duration::ZERO,
                stats_since: Instant::now(),
            },
        );
        Ok(())
    }

    /// Drops the window's renderer. Call before destroying the native surface.
    pub fn remove(&mut self, window: WindowId) {
        self.remotes.remove(&window);
    }

    /// Popups whose parent is `window`, directly or through other popups.
    pub fn descendants(&self, window: WindowId) -> Vec<WindowId> {
        let mut out = Vec::new();
        let mut frontier = vec![window];
        while let Some(p) = frontier.pop() {
            for r in self.remotes.values() {
                if r.info.parent == Some(p) && !out.contains(&r.info.id) {
                    out.push(r.info.id);
                    frontier.push(r.info.id);
                }
            }
        }
        out
    }

    /// True once every normal window that was opened is gone.
    pub fn should_exit(&self) -> bool {
        self.opened_any && !self.remotes.values().any(|r| is_toplevel(r.info.kind))
    }

    pub fn frame(&mut self, window: WindowId, frame: Picture, decoded: Instant) {
        let (Some(r), Some(gpu)) = (self.remotes.get_mut(&window), self.gpu.as_mut()) else { return };
        let start = Instant::now();
        r.renderer.upload(gpu, frame);
        r.renderer.render(gpu);
        let end = Instant::now();
        r.presented += 1;
        r.ui_time += end - start;
        r.present_lag += end - decoded;
        let secs = r.stats_since.elapsed().as_secs_f64();
        if secs >= 5.0 {
            let ms = |d: Duration| format_args!("{:.2}", d.as_secs_f64() * 1e3 / r.presented as f64).to_string();
            tracing::info!(
                window,
                fps = format_args!("{:.1}", r.presented as f64 / secs),
                ui_ms = ms(r.ui_time),
                lag_ms = ms(r.present_lag),
                "present"
            );
            (r.presented, r.ui_time, r.present_lag, r.stats_since) = (0, Duration::ZERO, Duration::ZERO, Instant::now());
        }
    }

    pub fn redraw(&mut self, window: WindowId) {
        if let (Some(r), Some(gpu)) = (self.remotes.get_mut(&window), self.gpu.as_mut()) {
            r.renderer.render(gpu);
        }
    }

    fn apply_size(&mut self, window: WindowId, size: (u32, u32)) -> Option<&mut Remote> {
        let (r, gpu) = (self.remotes.get_mut(&window)?, self.gpu.as_mut()?);
        r.size = size;
        r.renderer.resize(gpu, size.0, size.1);
        r.renderer.render(gpu);
        Some(r)
    }

    /// The local window was resized (by the user or compositor). Normal windows ask the host
    /// to follow once the size has been stable for a moment; desktops just rescale.
    pub fn resized(&mut self, window: WindowId, size: (u32, u32)) {
        let Some(r) = self.apply_size(window, size) else { return };
        if r.info.kind != WindowKind::Normal {
            return;
        }
        let (fw, fh) = r.renderer.frame_size().unwrap_or((r.info.width, r.info.height));
        let differs = fw.abs_diff(size.0) > 2 || fh.abs_diff(size.1) > 2;
        r.resize_due = differs.then(|| Instant::now() + RESIZE_DEBOUNCE);
    }

    /// Size the shell should use for the window right now.
    pub fn size(&self, window: WindowId) -> Option<(u32, u32)> {
        self.remotes.get(&window).map(|r| r.size)
    }

    pub fn changed(&mut self, info: WindowInfo) -> Option<Changed> {
        let r = self.remotes.get_mut(&info.id)?;
        let host_size = (info.width.max(1), info.height.max(1));
        // Apps snap to their own size steps (Terminal cells, minimum sizes), so adopt the size
        // the Mac settled on unless the user is still resizing. Desktops keep the user's size.
        let snap = (info.kind != WindowKind::Desktop && r.resize_due.is_none() && r.size != host_size).then_some(host_size);
        let reposition = info.kind == WindowKind::Popup
            && (info.offset != r.info.offset || (info.width, info.height) != (r.info.width, r.info.height));
        let (title, id) = (title(&info), info.id);
        r.info = info;
        if let Some(size) = snap {
            self.apply_size(id, size);
        }
        Some(Changed { title, snap, reposition })
    }

    /// Sends resize requests whose debounce expired; returns the next deadline, if any.
    pub fn flush_resizes(&mut self, now: Instant) -> Option<Instant> {
        let mut next: Option<Instant> = None;
        let mut due = Vec::new();
        for r in self.remotes.values_mut() {
            match r.resize_due {
                Some(t) if t <= now => {
                    r.resize_due = None;
                    due.push(ClientMsg::Resize { window: r.info.id, width: r.size.0, height: r.size.1 });
                }
                Some(t) => next = Some(next.map_or(t, |n| n.min(t))),
                None => {}
            }
        }
        for msg in due {
            self.send(NetCmd::Control(msg));
        }
        next
    }

    /// The user closed a local window: stop streaming it (the Mac window stays open).
    pub fn close_requested(&mut self, window: WindowId) {
        self.send(NetCmd::Control(ClientMsg::Unsubscribe { window }));
    }

    pub fn pointer_moved(&mut self, window: WindowId, x: f64, y: f64) {
        let Some(r) = self.remotes.get_mut(&window) else { return };
        r.cursor = (x, y);
        if let Some((x, y)) = surface_pos(r) {
            self.input(InputEvent::MouseMove { window, x, y });
        }
    }

    pub fn button(&mut self, window: WindowId, button: MouseButton, pressed: bool) {
        if let Some((x, y)) = self.remotes.get(&window).and_then(surface_pos) {
            self.input(InputEvent::MouseButton { window, x, y, button, pressed });
        }
    }

    /// Positive `dy` scrolls up (wheel away from the user). Precise deltas are pixels; wheel
    /// deltas are notches, sent as `scroll_lines` lines each.
    pub fn scroll(&mut self, window: WindowId, dx: f32, dy: f32, precise: bool) {
        if !self.contains(window) {
            return;
        }
        let (dx, dy) = if precise {
            (dx, dy)
        } else {
            let lines = self.cfg.scroll_lines.clamp(0.1, 50.0);
            (dx * lines, dy * lines)
        };
        self.input(InputEvent::Scroll { window, dx, dy, precise });
    }

    /// The normal window a popup belongs to (itself for normal windows).
    fn root(&self, mut window: WindowId) -> Option<WindowId> {
        for _ in 0..=MAX_POPUPS {
            let info = &self.remotes.get(&window)?.info;
            match (info.kind, info.parent) {
                (WindowKind::Popup, Some(p)) => window = p,
                _ => return Some(window),
            }
        }
        None
    }

    /// Keys always go to the normal window; the Mac routes them to an open menu itself.
    /// `keycode` is positional (Linux Ctrl is `kVK_Control`); see `shortcuts`.
    pub fn key(&mut self, window: WindowId, keycode: u16, pressed: bool) {
        let Some(window) = self.root(window) else { return };
        let profile = match self.info(window) {
            _ if !self.cfg.linux_shortcuts => Profile::Native,
            Some(i) if i.kind == WindowKind::Normal && is_terminal(i.bundle_id.as_deref()) => Profile::Terminal,
            _ => Profile::Apps,
        };
        for (keycode, pressed) in self.keys.key(keycode, pressed, profile) {
            self.input(InputEvent::Key { window, keycode, pressed });
        }
    }

    /// Applies a cursor change from the host; true if the shown shape should change.
    pub fn cursor_update(&mut self, id: u64, shape: Option<umac_proto::CursorShape>) -> bool {
        let changed = match shape {
            Some(shape) => self.cursors.set(id, shape),
            None => self.cursors.known(id),
        };
        if changed {
            tracing::debug!(cursor = ?self.cursor_name(), "cursor");
        }
        changed
    }

    pub fn cursor(&self) -> &Shape {
        self.cursors.current()
    }

    fn cursor_name(&self) -> String {
        match self.cursors.current() {
            Shape::Named(n) => n.name().to_owned(),
            Shape::Image(i) => format!("image {}x{}", i.width, i.height),
        }
    }

    pub fn is_desktop(&self, window: WindowId) -> bool {
        self.info(window).is_some_and(|i| i.kind == WindowKind::Desktop)
    }

    /// Focus changes of popups are ignored: raising a menu's owner would dismiss the menu.
    pub fn focus(&mut self, window: WindowId, focused: bool) {
        if self.info(window).is_none_or(|i| !is_toplevel(i.kind)) {
            return;
        }
        if focused {
            self.send(NetCmd::Control(ClientMsg::Focus { window }));
            self.paste.focused();
        } else {
            self.keys.reset();
            self.input(InputEvent::ReleaseAll);
        }
    }
}

fn surface_pos(r: &Remote) -> Option<(f32, f32)> {
    Some(r.renderer.viewport()?.to_surface(r.cursor.0, r.cursor.1))
}

/// Placement of a popup inside its parent for an xdg_positioner: the anchor point must lie
/// within the parent's geometry, so the offset is split into a clamped anchor plus a
/// remainder passed as the positioner offset.
pub fn popup_anchor(offset: (i32, i32), parent: (u32, u32)) -> ((i32, i32), (i32, i32)) {
    let clamp = |v: i32, len: u32| v.clamp(0, (len.max(1) - 1) as i32);
    let anchor = (clamp(offset.0, parent.0), clamp(offset.1, parent.1));
    (anchor, (offset.0 - anchor.0, offset.1 - anchor.1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(kind: WindowKind, width: u32, height: u32) -> WindowInfo {
        WindowInfo {
            id: 1,
            pid: 0,
            app_name: String::new(),
            bundle_id: None,
            title: String::new(),
            width,
            height,
            scale: 1.0,
            parent: None,
            offset: (0, 0),
            kind,
        }
    }

    #[test]
    fn initial_size_caps_only_desktops() {
        let out = Some((2560, 1440));
        assert_eq!(initial_size(&info(WindowKind::Desktop, 3840, 2160), out), (2304, 1296));
        assert_eq!(initial_size(&info(WindowKind::Desktop, 1920, 1080), out), (1920, 1080));
        assert_eq!(initial_size(&info(WindowKind::Desktop, 1920, 1080), None), (1920, 1080));
        assert_eq!(initial_size(&info(WindowKind::Normal, 3000, 2000), out), (3000, 2000));
    }

    #[test]
    fn popup_anchor_inside_parent() {
        assert_eq!(popup_anchor((100, 80), (800, 600)), ((100, 80), (0, 0)));
    }

    #[test]
    fn popup_anchor_outside_parent_moves_remainder_to_offset() {
        assert_eq!(popup_anchor((-30, 20), (800, 600)), ((0, 20), (-30, 0)));
        assert_eq!(popup_anchor((900, 650), (800, 600)), ((799, 599), (101, 51)));
        assert_eq!(popup_anchor((5, 5), (0, 0)), ((0, 0), (5, 5)));
    }
}
