//! winit application: one native window per remote window, input forwarding, resize sync.
//!
//! Popups and menus (`WindowKind::Popup` with a `parent`) are not shown yet. The planned seam is
//! `UiEvent::Open` for a popup: on Wayland it needs an xdg_popup anchored to the parent surface
//! (winit lacks this, so smithay-client-toolkit or compositing into the parent).

use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

use tokio::sync::mpsc::UnboundedSender;
use umac_proto::{ClientMsg, InputEvent, MouseButton, WindowId, WindowInfo};
use winit::{
    application::ApplicationHandler,
    dpi::PhysicalSize,
    event::{ElementState, MouseScrollDelta, WindowEvent},
    event_loop::{ActiveEventLoop, ControlFlow},
    keyboard::PhysicalKey,
    window::{Window, WindowId as LocalId},
};

use crate::{
    config::Config,
    keymap,
    net::{NetCmd, UiEvent},
    render::{Gpu, WindowRenderer},
};

const RESIZE_DEBOUNCE: Duration = Duration::from_millis(150);
/// Pixels per wheel notch sent as a non-precise scroll, roughly one macOS scroll line.
const LINE_PX: f32 = 10.0;

struct Remote {
    info: WindowInfo,
    window: Arc<Window>,
    renderer: WindowRenderer,
    cursor: (f64, f64),
    resize_due: Option<Instant>,
    presented: u32,
    stats_since: Instant,
}

pub struct App {
    cfg: Config,
    net: UnboundedSender<NetCmd>,
    gpu: Option<Gpu>,
    windows: HashMap<LocalId, Remote>,
    by_remote: HashMap<WindowId, LocalId>,
    opened_any: bool,
    pub error: Option<String>,
}

fn title(info: &WindowInfo) -> String {
    if info.title.is_empty() { info.app_name.clone() } else { format!("{}: {}", info.app_name, info.title) }
}

impl App {
    pub fn new(cfg: Config, net: UnboundedSender<NetCmd>) -> Self {
        Self { cfg, net, gpu: None, windows: HashMap::new(), by_remote: HashMap::new(), opened_any: false, error: None }
    }

    fn send(&self, cmd: NetCmd) {
        let _ = self.net.send(cmd);
    }

    fn open(&mut self, el: &ActiveEventLoop, info: WindowInfo) -> anyhow::Result<()> {
        let attrs = Window::default_attributes()
            .with_title(title(&info))
            .with_inner_size(PhysicalSize::new(info.width.max(1), info.height.max(1)));
        let window = Arc::new(el.create_window(attrs)?);
        let surface = match &self.gpu {
            Some(gpu) => gpu.create_surface(window.clone())?,
            None => {
                let (gpu, surface) = Gpu::new(window.clone())?;
                self.gpu = Some(gpu);
                surface
            }
        };
        let gpu = self.gpu.as_mut().unwrap();
        let size = window.inner_size();
        let mut renderer = WindowRenderer::new(gpu, surface, size.width, size.height);
        // Wayland maps a window only after its first buffer.
        renderer.render(gpu);
        self.by_remote.insert(info.id, window.id());
        self.windows.insert(
            window.id(),
            Remote {
                info,
                window,
                renderer,
                cursor: (0.0, 0.0),
                resize_due: None,
                presented: 0,
                stats_since: Instant::now(),
            },
        );
        self.opened_any = true;
        Ok(())
    }

    fn close(&mut self, el: &ActiveEventLoop, remote: WindowId) {
        if let Some(local) = self.by_remote.remove(&remote) {
            self.windows.remove(&local);
        }
        if self.opened_any && self.windows.is_empty() {
            el.exit();
        }
    }

    fn pointer(&self, r: &Remote) -> Option<(f32, f32)> {
        Some(r.renderer.viewport()?.to_surface(r.cursor.0, r.cursor.1))
    }
}

impl ApplicationHandler<UiEvent> for App {
    fn resumed(&mut self, _el: &ActiveEventLoop) {}

    fn user_event(&mut self, el: &ActiveEventLoop, event: UiEvent) {
        match event {
            UiEvent::Open(info) => {
                let id = info.id;
                if let Err(e) = self.open(el, info) {
                    tracing::error!("opening window: {e:#}");
                    self.send(NetCmd::Control(ClientMsg::Unsubscribe { window: id }));
                }
            }
            UiEvent::Frame { window, frame } => {
                let (Some(local), Some(gpu)) = (self.by_remote.get(&window), self.gpu.as_mut()) else { return };
                let r = self.windows.get_mut(local).unwrap();
                r.renderer.upload(gpu, &frame);
                r.renderer.render(gpu);
                r.presented += 1;
                let secs = r.stats_since.elapsed().as_secs_f64();
                if secs >= 5.0 {
                    tracing::info!(window, fps = format_args!("{:.1}", r.presented as f64 / secs), "present");
                    (r.presented, r.stats_since) = (0, Instant::now());
                }
            }
            UiEvent::Changed(info) => {
                if let Some(r) = self.by_remote.get(&info.id).and_then(|l| self.windows.get_mut(l)) {
                    r.window.set_title(&title(&info));
                    // Apps snap to their own size steps (Terminal cells, minimum sizes), so adopt
                    // the size the Mac settled on unless the user is still resizing.
                    let size = PhysicalSize::new(info.width.max(1), info.height.max(1));
                    if r.resize_due.is_none() && r.window.inner_size() != size {
                        let _ = r.window.request_inner_size(size);
                    }
                    r.info = info;
                }
            }
            UiEvent::Closed(window) => self.close(el, window),
            UiEvent::Shutdown(reason) => {
                if !reason.is_empty() {
                    self.error = Some(reason);
                }
                el.exit();
            }
        }
    }

    fn window_event(&mut self, el: &ActiveEventLoop, id: LocalId, event: WindowEvent) {
        let Some(r) = self.windows.get_mut(&id) else { return };
        let remote = r.info.id;
        match event {
            WindowEvent::CloseRequested => {
                self.send(NetCmd::Control(ClientMsg::Unsubscribe { window: remote }));
                self.close(el, remote);
            }
            WindowEvent::Resized(size) => {
                if let Some(gpu) = self.gpu.as_mut() {
                    r.renderer.resize(gpu, size.width, size.height);
                    r.renderer.render(gpu);
                }
                let (fw, fh) = r.renderer.frame_size().unwrap_or((r.info.width, r.info.height));
                let differs = fw.abs_diff(size.width) > 2 || fh.abs_diff(size.height) > 2;
                r.resize_due = differs.then(|| Instant::now() + RESIZE_DEBOUNCE);
            }
            WindowEvent::RedrawRequested => {
                if let Some(gpu) = self.gpu.as_mut() {
                    r.renderer.render(gpu);
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                r.cursor = (position.x, position.y);
                if let Some((x, y)) = self.pointer(&self.windows[&id]) {
                    self.send(NetCmd::Input(InputEvent::MouseMove { window: remote, x, y }));
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let button = match button {
                    winit::event::MouseButton::Left => MouseButton::Left,
                    winit::event::MouseButton::Right => MouseButton::Right,
                    winit::event::MouseButton::Middle => MouseButton::Middle,
                    winit::event::MouseButton::Back => MouseButton::Other(3),
                    winit::event::MouseButton::Forward => MouseButton::Other(4),
                    winit::event::MouseButton::Other(n) => MouseButton::Other(n.min(255) as u8),
                };
                if let Some((x, y)) = self.pointer(&self.windows[&id]) {
                    let pressed = state == ElementState::Pressed;
                    self.send(NetCmd::Input(InputEvent::MouseButton { window: remote, x, y, button, pressed }));
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                // Positive dy scrolls up (wheel away from the user), as in winit and CGEvent.
                let (dx, dy, precise) = match delta {
                    MouseScrollDelta::LineDelta(x, y) => (x * LINE_PX, y * LINE_PX, false),
                    MouseScrollDelta::PixelDelta(p) => (p.x as f32, p.y as f32, true),
                };
                self.send(NetCmd::Input(InputEvent::Scroll { window: remote, dx, dy, precise }));
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if let PhysicalKey::Code(code) = event.physical_key
                    && let Some(keycode) = keymap::to_mac(code, self.cfg.swap_ctrl_cmd)
                {
                    let pressed = event.state == ElementState::Pressed;
                    self.send(NetCmd::Input(InputEvent::Key { window: remote, keycode, pressed }));
                }
            }
            WindowEvent::Focused(true) => self.send(NetCmd::Control(ClientMsg::Focus { window: remote })),
            WindowEvent::Focused(false) => self.send(NetCmd::Input(InputEvent::ReleaseAll)),
            _ => {}
        }
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        let now = Instant::now();
        let mut next: Option<Instant> = None;
        let mut resizes = Vec::new();
        for r in self.windows.values_mut() {
            match r.resize_due {
                Some(due) if due <= now => {
                    r.resize_due = None;
                    let size = r.window.inner_size();
                    resizes.push(ClientMsg::Resize { window: r.info.id, width: size.width, height: size.height });
                }
                Some(due) => next = Some(next.map_or(due, |n| n.min(due))),
                None => {}
            }
        }
        for msg in resizes {
            self.send(NetCmd::Control(msg));
        }
        el.set_control_flow(next.map_or(ControlFlow::Wait, ControlFlow::WaitUntil));
    }
}
