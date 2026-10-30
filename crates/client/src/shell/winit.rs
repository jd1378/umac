//! winit shell: fallback for X11 and non-Linux platforms. Popups become undecorated windows
//! placed at the parent's position plus the popup offset, which works where clients may
//! position windows (X11, Windows) and degrades to compositor placement elsewhere.

use std::{collections::HashMap, sync::Arc, time::Instant};

use anyhow::Context;
use umac_proto::{MouseButton, WindowId, WindowInfo, WindowKind};
use winit::{
    application::ApplicationHandler,
    dpi::{PhysicalPosition, PhysicalSize},
    event::{ElementState, MouseScrollDelta, WindowEvent},
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop},
    keyboard::{KeyCode, PhysicalKey},
    window::{Cursor, CustomCursor, Fullscreen, Window, WindowId as LocalId},
};

use crate::{
    core::{self, Core},
    cursor::Shape as CursorShape,
    keymap,
    ui::{UiEvent, UiSender},
};

pub struct Shell {
    event_loop: EventLoop<UiEvent>,
}

impl Shell {
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self { event_loop: EventLoop::<UiEvent>::with_user_event().build()? })
    }

    pub fn sender(&self) -> UiSender {
        let proxy = self.event_loop.create_proxy();
        UiSender::new(move |ev| proxy.send_event(ev).is_ok())
    }

    pub fn run(self, core: Core) -> anyhow::Result<Option<String>> {
        let mut app = App { core, windows: HashMap::new(), by_remote: HashMap::new(), clipboard_warned: false };
        self.event_loop.run_app(&mut app).context("event loop")?;
        Ok(app.core.error.take())
    }
}

struct App {
    core: Core,
    windows: HashMap<LocalId, (WindowId, Arc<Window>)>,
    by_remote: HashMap<WindowId, LocalId>,
    clipboard_warned: bool,
}

impl App {
    fn apply_titles(&mut self) {
        for (id, title) in self.core.take_titles() {
            if let Some(w) = self.window(id) {
                w.set_title(&title);
            }
        }
    }

    fn window(&self, remote: WindowId) -> Option<&Arc<Window>> {
        self.windows.get(self.by_remote.get(&remote)?).map(|(_, w)| w)
    }

    fn open(&mut self, el: &ActiveEventLoop, info: WindowInfo) -> anyhow::Result<()> {
        let output = el.primary_monitor().map(|m| (m.size().width, m.size().height));
        let (w, h) = core::initial_size(&info, output);
        let mut attrs = Window::default_attributes().with_title(core::title(&info)).with_inner_size(PhysicalSize::new(w, h));
        if info.kind == WindowKind::Popup {
            let parent = info.parent.and_then(|p| self.window(p)).context("popup parent is not shown")?;
            let origin = parent.inner_position().unwrap_or_default();
            let (dx, dy) = core::popup_offset(&info);
            attrs = attrs.with_decorations(false).with_position(PhysicalPosition::new(origin.x + dx, origin.y + dy));
        }
        // Dialogs open over their parent where the window manager allows placement (X11).
        if info.kind == WindowKind::Dialog
            && let Some(origin) = info.parent.and_then(|p| self.window(p)).and_then(|w| w.inner_position().ok())
        {
            let (dx, dy) = core::popup_offset(&info);
            attrs = attrs.with_position(PhysicalPosition::new(origin.x + dx, origin.y + dy));
        }
        let window = Arc::new(el.create_window(attrs)?);
        // A desktop stream has the Mac cursor drawn in.
        if info.kind == WindowKind::Desktop {
            window.set_cursor_visible(false);
        }
        let surface = self.core.instance().create_surface(window.clone())?;
        let size = window.inner_size();
        let id = info.id;
        self.core.attach(info, surface, (size.width, size.height))?;
        self.by_remote.insert(id, window.id());
        self.windows.insert(window.id(), (id, window));
        self.apply_cursor(el);
        Ok(())
    }

    /// Shows the Mac cursor shape on every streamed window; desktops draw their own.
    fn apply_cursor(&self, el: &ActiveEventLoop) {
        let cursor: Cursor = match self.core.cursor() {
            CursorShape::Named(icon) => (*icon).into(),
            CursorShape::Image(img) => {
                let source = CustomCursor::from_rgba(img.rgba.clone(), img.width as u16, img.height as u16, img.hot_x as u16, img.hot_y as u16);
                match source {
                    Ok(s) => el.create_custom_cursor(s).into(),
                    Err(e) => {
                        tracing::debug!("custom cursor: {e}");
                        Cursor::default()
                    }
                }
            }
        };
        for (remote, window) in self.windows.values() {
            if !self.core.is_desktop(*remote) {
                window.set_cursor(cursor.clone());
            }
        }
    }

    fn close(&mut self, el: &ActiveEventLoop, remote: WindowId) {
        for w in std::iter::once(remote).chain(self.core.descendants(remote)) {
            self.core.remove(w);
            if let Some(local) = self.by_remote.remove(&w) {
                self.windows.remove(&local);
            }
        }
        if self.core.should_exit() {
            el.exit();
        }
    }
}

impl ApplicationHandler<UiEvent> for App {
    fn resumed(&mut self, _el: &ActiveEventLoop) {}

    fn user_event(&mut self, el: &ActiveEventLoop, event: UiEvent) {
        match event {
            UiEvent::Open(info) => {
                let (id, popup) = (info.id, info.kind == WindowKind::Popup);
                if let Err(e) = self.open(el, info) {
                    tracing::error!("opening window: {e:#}");
                    if !popup {
                        self.core.close_requested(id);
                    }
                }
            }
            UiEvent::Frame { window, slot } => {
                // No presentation feedback here: the submit time stands in for on screen.
                if let Some((timing, submit)) = self.core.frame(window, &slot) {
                    self.core.shown(window, timing, submit, None);
                }
                self.apply_titles();
            }
            UiEvent::HostStats { window, host } => self.core.host_stats(window, host),
            UiEvent::Changed(info) => {
                let (id, kind, offset, parent) = (info.id, info.kind, core::popup_offset(&info), info.parent);
                let Some(change) = self.core.changed(info) else { return };
                let Some(window) = self.window(id).cloned() else { return };
                window.set_title(&change.title);
                if let Some((w, h)) = change.snap {
                    let _ = window.request_inner_size(PhysicalSize::new(w, h));
                }
                if kind == WindowKind::Popup
                    && change.reposition
                    && let Some(origin) = parent.and_then(|p| self.window(p)).and_then(|p| p.inner_position().ok())
                {
                    window.set_outer_position(PhysicalPosition::new(origin.x + offset.0, origin.y + offset.1));
                }
            }
            UiEvent::Closed(window) => self.close(el, window),
            UiEvent::Shutdown(reason) => {
                if !reason.is_empty() {
                    self.core.error = Some(reason);
                }
                el.exit();
            }
            UiEvent::Clipboard(_) => {
                if !self.clipboard_warned {
                    self.clipboard_warned = true;
                    tracing::info!("clipboard sync needs the Wayland backend; ignoring Mac copies");
                }
            }
            UiEvent::ClipboardRequest(nonce) => self.core.clipboard_request(nonce),
            UiEvent::Cursor { id, shape } => {
                if self.core.cursor_update(id, shape) {
                    self.apply_cursor(el);
                }
            }
        }
    }

    fn window_event(&mut self, el: &ActiveEventLoop, id: LocalId, event: WindowEvent) {
        let Some(&(remote, _)) = self.windows.get(&id) else { return };
        let core = &mut self.core;
        match event {
            WindowEvent::CloseRequested => {
                core.close_requested(remote);
                self.close(el, remote);
            }
            WindowEvent::Resized(size) => core.resized(remote, (size.width, size.height)),
            WindowEvent::RedrawRequested => core.redraw(remote),
            WindowEvent::CursorMoved { position, .. } => core.pointer_moved(remote, position.x, position.y),
            WindowEvent::MouseInput { state, button, .. } => {
                let button = match button {
                    winit::event::MouseButton::Left => MouseButton::Left,
                    winit::event::MouseButton::Right => MouseButton::Right,
                    winit::event::MouseButton::Middle => MouseButton::Middle,
                    winit::event::MouseButton::Back => MouseButton::Other(3),
                    winit::event::MouseButton::Forward => MouseButton::Other(4),
                    winit::event::MouseButton::Other(n) => MouseButton::Other(n.min(255) as u8),
                };
                core.button(remote, button, state == ElementState::Pressed);
            }
            WindowEvent::MouseWheel { delta, .. } => {
                // Positive dy scrolls up (wheel away from the user), as in winit and CGEvent.
                let (dx, dy, precise) = match delta {
                    MouseScrollDelta::LineDelta(x, y) => (x, y, false),
                    MouseScrollDelta::PixelDelta(p) => (p.x as f32, p.y as f32, true),
                };
                core.scroll(remote, dx, dy, precise);
            }
            WindowEvent::KeyboardInput { event, .. }
                if event.physical_key == PhysicalKey::Code(KeyCode::F11) && core.is_desktop(remote) =>
            {
                if event.state == ElementState::Pressed
                    && !event.repeat
                    && let Some((_, window)) = self.windows.get(&id)
                {
                    let full = window.fullscreen().is_none().then_some(Fullscreen::Borderless(None));
                    window.set_fullscreen(full);
                }
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if let PhysicalKey::Code(code) = event.physical_key
                    && !event.repeat
                    && let Some(keycode) = keymap::to_mac(code, false)
                {
                    core.key(remote, keycode, event.state == ElementState::Pressed);
                }
                self.apply_titles();
            }
            WindowEvent::Focused(focused) => core.focus(remote, focused),
            _ => {}
        }
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        // No clipboard access in this shell: answer pending pastes with nothing.
        if self.core.wants_clipboard() {
            self.core.clipboard_read(None);
        }
        let next = self.core.flush_resizes(Instant::now());
        el.set_control_flow(next.map_or(ControlFlow::Wait, ControlFlow::WaitUntil));
    }
}
