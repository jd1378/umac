//! Native Wayland shell on smithay-client-toolkit. Normal windows are xdg toplevels with
//! server-side decorations; menus and tooltips are xdg popups placed relative to their parent
//! surface, so they can extend past it and the compositor keeps them on screen.
//!
//! Popups take no grab: a click outside lands on the parent, is forwarded to the Mac, and the
//! Mac closes the menu itself, which the host reports as `WindowClosed`.

use std::{
    collections::HashMap,
    io::{Read, Write},
    ptr::NonNull,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use raw_window_handle::{RawDisplayHandle, RawWindowHandle, WaylandDisplayHandle, WaylandWindowHandle};
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    data_device_manager::{
        DataDeviceManagerState, WritePipe,
        data_device::{DataDevice, DataDeviceHandler},
        data_offer::{DataOfferHandler, DragOffer},
        data_source::{CopyPasteSource, DataSourceHandler},
    },
    delegate_registry,
    output::{OutputHandler, OutputState},
    reexports::{
        calloop::{
            EventLoop, LoopHandle, channel,
            timer::{TimeoutAction, Timer},
        },
        calloop_wayland_source::WaylandSource,
        client::{
            Connection, Proxy, QueueHandle,
            backend::ObjectId,
            globals::registry_queue_init,
            protocol::{
                wl_data_device::WlDataDevice,
                wl_data_device_manager::DndAction,
                wl_data_source::WlDataSource,
                wl_keyboard, wl_output, wl_pointer, wl_seat, wl_shm, wl_surface,
            },
        },
        protocols::{
            wp::presentation_time::client::{
                wp_presentation::{self, WpPresentation},
                wp_presentation_feedback::{self, WpPresentationFeedback},
            },
            xdg::shell::client::xdg_positioner::{Anchor, ConstraintAdjustment, Gravity},
        },
    },
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        Capability, SeatHandler, SeatState,
        keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers, RawModifiers},
        pointer::{PointerEvent, PointerEventKind, PointerHandler, ThemeSpec, ThemedPointer},
    },
    shell::{
        WaylandSurface,
        xdg::{
            XdgPositioner, XdgShell, XdgSurface,
            popup::{Popup, PopupConfigure, PopupHandler},
            window::{Window, WindowConfigure, WindowDecorations, WindowHandler},
        },
    },
    shm::{
        Shm, ShmHandler,
        slot::{Buffer, SlotPool},
    },
};
use umac_proto::{MouseButton, WindowId, WindowInfo, WindowKind};

use crate::{
    clipboard::sendable,
    core::{self, Core, MAX_POPUPS, WHEEL_NOTCH_VALUE},
    cursor::{self as cursors, Shape as CursorShape},
    keymap,
    latency::{self, FrameTiming},
    ui::{UiEvent, UiSender},
};

/// CLOCK_MONOTONIC, the presentation clock unless the compositor says otherwise.
const DEFAULT_PRESENTATION_CLOCK: u32 = 1;

/// The presentation-time global; its only event names the clock of the timestamps.
struct PresentationData;

impl smithay_client_toolkit::dispatch2::Dispatch2<WpPresentation, State> for PresentationData {
    fn event(&self, state: &mut State, _: &WpPresentation, event: wp_presentation::Event, _: &Connection, _: &QueueHandle<State>) {
        if let wp_presentation::Event::ClockId { clk_id } = event {
            state.presentation_clock = clk_id;
        }
    }
}

/// One frame's presentation feedback; the timing is filled in once the frame was submitted.
struct FeedbackData {
    window: WindowId,
    frame: Arc<std::sync::Mutex<Option<(FrameTiming, Instant)>>>,
}

impl smithay_client_toolkit::dispatch2::Dispatch2<WpPresentationFeedback, State> for FeedbackData {
    fn event(&self, state: &mut State, _: &WpPresentationFeedback, event: wp_presentation_feedback::Event, _: &Connection, _: &QueueHandle<State>) {
        let wp_presentation_feedback::Event::Presented { tv_sec_hi, tv_sec_lo, tv_nsec, .. } = event else { return };
        let Some((timing, submit)) = self.frame.lock().ok().and_then(|mut f| f.take()) else { return };
        let sec = ((tv_sec_hi as u64) << 32) | tv_sec_lo as u64;
        let shown = latency::from_clock(state.presentation_clock, sec, tv_nsec);
        state.core.shown(self.window, timing, submit, shown);
    }
}

const APP_ID: &str = "dev.umac.client";
/// Text types we offer and accept, most preferred first.
const TEXT_MIMES: [&str; 4] = ["text/plain;charset=utf-8", "UTF8_STRING", "text/plain", "STRING"];
/// How long a paste waits for another app to hand over the clipboard.
const READ_TIMEOUT: Duration = Duration::from_millis(300);

/// Clipboard (Wayland selection) state.
struct Clip {
    manager: Option<DataDeviceManagerState>,
    device: Option<DataDevice>,
    /// Latest keyboard serial (enter or key press). KWin rejects a selection set with a
    /// pointer serial from a popup, which never has keyboard focus, so pointer serials are
    /// not used.
    serial: Option<u32>,
    /// Our current selection and its text, while we own it.
    own: Option<(CopyPasteSource, Arc<str>)>,
    /// Mac copy that arrived without keyboard focus; set on the next focus.
    pending: Option<String>,
    /// Bumped per read so a late result after a timeout is ignored.
    read_gen: u64,
    reading: bool,
    results: channel::Sender<(u64, Option<String>)>,
}

pub struct Shell {
    conn: Connection,
    event_loop: EventLoop<'static, State>,
    tx: channel::Sender<UiEvent>,
    rx: channel::Channel<UiEvent>,
    globals: smithay_client_toolkit::reexports::client::globals::GlobalList,
    queue: smithay_client_toolkit::reexports::client::EventQueue<State>,
}

impl Shell {
    pub fn new() -> Result<Self> {
        let conn = Connection::connect_to_env().context("connecting to the Wayland compositor")?;
        let (globals, queue) = registry_queue_init(&conn).context("reading Wayland globals")?;
        let event_loop = EventLoop::try_new()?;
        let (tx, rx) = channel::channel();
        Ok(Self { conn, event_loop, tx, rx, globals, queue })
    }

    pub fn sender(&self) -> UiSender {
        let tx = std::sync::Mutex::new(self.tx.clone());
        UiSender::new(move |ev| tx.lock().is_ok_and(|t| t.send(ev).is_ok()))
    }

    pub fn run(self, core: Core) -> Result<Option<String>> {
        let Self { conn, mut event_loop, tx, rx, globals, queue } = self;
        drop(tx);
        let qh = queue.handle();
        let (results_tx, results_rx) = channel::channel::<(u64, Option<String>)>();
        let manager = DataDeviceManagerState::bind(&globals, &qh)
            .inspect_err(|e| tracing::warn!("no clipboard support from the compositor: {e}"))
            .ok();
        let mut state = State {
            core,
            loop_handle: event_loop.handle(),
            clip: Clip {
                manager,
                device: None,
                serial: None,
                own: None,
                pending: None,
                read_gen: 0,
                reading: false,
                results: results_tx,
            },
            conn: conn.clone(),
            qh: qh.clone(),
            registry: RegistryState::new(&globals),
            seats: SeatState::new(&globals, &qh),
            outputs: OutputState::new(&globals, &qh),
            compositor: CompositorState::bind(&globals, &qh).context("wl_compositor missing")?,
            xdg: XdgShell::bind(&globals, &qh).context("xdg_wm_base missing")?,
            shm: Shm::bind(&globals, &qh).context("wl_shm missing")?,
            surfaces: HashMap::new(),
            nodes: HashMap::new(),
            keyboard: None,
            pointer: None,
            hover: None,
            cursor_image: None,
            kb_focus: None,
            presentation: globals.bind::<WpPresentation, _, _>(&qh, 1..=2, PresentationData).ok(),
            presentation_clock: DEFAULT_PRESENTATION_CLOCK,
            exit: false,
        };
        WaylandSource::new(conn, queue).insert(event_loop.handle()).map_err(|e| anyhow::anyhow!("{e}"))?;
        event_loop
            .handle()
            .insert_source(rx, |ev, _, state: &mut State| {
                if let channel::Event::Msg(ev) = ev {
                    state.ui_event(ev);
                }
            })
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        event_loop
            .handle()
            .insert_source(results_rx, |ev, _, state: &mut State| {
                if let channel::Event::Msg((generation, text)) = ev {
                    state.read_finished(generation, text);
                }
            })
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        while !state.exit {
            let now = Instant::now();
            let timeout = state.core.flush_resizes(now).map(|t| t.saturating_duration_since(now));
            event_loop.dispatch(timeout, &mut state)?;
            state.service_clipboard();
        }
        state.close_all();
        Ok(state.core.error.take())
    }
}

enum Shape {
    Toplevel(Window),
    Popup(Popup),
}

impl Shape {
    fn wl_surface(&self) -> &wl_surface::WlSurface {
        match self {
            Shape::Toplevel(w) => w.wl_surface(),
            Shape::Popup(p) => p.wl_surface(),
        }
    }

    fn xdg_surface(&self) -> &smithay_client_toolkit::reexports::protocols::xdg::shell::client::xdg_surface::XdgSurface {
        match self {
            Shape::Toplevel(w) => w.xdg_surface(),
            Shape::Popup(p) => p.xdg_surface(),
        }
    }
}

struct Node {
    shape: Shape,
    fullscreen: bool,
    /// Waiting for the first configure before the first buffer may be attached.
    pending: Option<(WindowInfo, wgpu::Surface<'static>)>,
}

/// Field order matters: `core` drops first so every wgpu surface is gone before the
/// connection its raw display pointer came from.
struct State {
    core: Core,
    loop_handle: LoopHandle<'static, State>,
    clip: Clip,
    conn: Connection,
    qh: QueueHandle<State>,
    registry: RegistryState,
    seats: SeatState,
    outputs: OutputState,
    compositor: CompositorState,
    xdg: XdgShell,
    shm: Shm,
    surfaces: HashMap<ObjectId, WindowId>,
    nodes: HashMap<WindowId, Node>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer: Option<ThemedPointer>,
    /// Streamed window under the pointer and the serial of its enter event.
    hover: Option<(WindowId, u32)>,
    cursor_image: Option<ImageCursor>,
    kb_focus: Option<WindowId>,
    /// Reports when frames actually reach the screen; absent on some compositors.
    presentation: Option<WpPresentation>,
    presentation_clock: u32,
    exit: bool,
}

/// A Mac cursor that is not a standard shape, drawn from the host's image.
struct ImageCursor {
    id: u64,
    hot: (i32, i32),
    surface: wl_surface::WlSurface,
    _pool: SlotPool,
    _buffer: Buffer,
}

/// Wheel or touchpad scroll on one axis, in Wayland's sign (positive = down/right): pixels for
/// touchpads, wheel notches otherwise. Notches are scaled by the desktop's scroll speed, which
/// the compositor applies to `absolute` (15 per notch at the default speed).
pub fn axis_delta(absolute: f64, value120: i32, discrete: i32, continuous: bool) -> Option<(f32, bool)> {
    if continuous || (value120 == 0 && discrete == 0) {
        return (absolute != 0.0).then_some((absolute as f32, true));
    }
    let steps = if value120 != 0 { value120 as f64 / 120.0 } else { discrete as f64 };
    let speed = if absolute != 0.0 { (absolute.abs() / (steps.abs() * WHEEL_NOTCH_VALUE)).clamp(0.1, 20.0) } else { 1.0 };
    Some(((steps * speed) as f32, false))
}

fn mouse_button(code: u32) -> MouseButton {
    // linux/input-event-codes.h: BTN_LEFT = 0x110 and up.
    match code {
        0x110 => MouseButton::Left,
        0x111 => MouseButton::Right,
        0x112 => MouseButton::Middle,
        0x113 => MouseButton::Other(3),
        0x114 => MouseButton::Other(4),
        c => MouseButton::Other(c.saturating_sub(0x110).min(255) as u8),
    }
}

impl State {
    fn id_of(&self, surface: &wl_surface::WlSurface) -> Option<WindowId> {
        self.surfaces.get(&surface.id()).copied()
    }

    fn wgpu_surface(&self, wl: &wl_surface::WlSurface) -> Result<wgpu::Surface<'static>> {
        let display = NonNull::new(self.conn.backend().display_ptr().cast()).context("no wl_display")?;
        let surface = NonNull::new(wl.id().as_ptr().cast()).context("no wl_surface")?;
        // Safety: the wl_surface and connection outlive the wgpu surface, which `Core` drops in
        // `close` before the node owning the wl_surface.
        Ok(unsafe {
            self.core.instance().create_surface_unsafe(wgpu::SurfaceTargetUnsafe::RawHandle {
                raw_display_handle: Some(RawDisplayHandle::Wayland(WaylandDisplayHandle::new(display))),
                raw_window_handle: RawWindowHandle::Wayland(WaylandWindowHandle::new(surface)),
            })?
        })
    }

    fn ui_event(&mut self, ev: UiEvent) {
        match ev {
            UiEvent::Open(info) => {
                let (id, kind) = (info.id, info.kind);
                if let Err(e) = self.open(info) {
                    tracing::warn!(window = id, "opening window: {e:#}");
                    if kind != WindowKind::Popup {
                        self.core.close_requested(id);
                    }
                }
            }
            UiEvent::Frame { window, slot } => self.present(window, &slot),
            UiEvent::HostStats { window, host } => self.core.host_stats(window, host),
            UiEvent::Changed(info) => self.changed(info),
            UiEvent::Closed(window) => self.close(window),
            UiEvent::Shutdown(reason) => {
                if !reason.is_empty() {
                    self.core.error = Some(reason);
                }
                self.exit = true;
            }
            UiEvent::Clipboard(text) => self.set_selection(text),
            UiEvent::ClipboardRequest(nonce) => self.core.clipboard_request(nonce),
            UiEvent::Cursor { id, shape } => {
                if self.core.cursor_update(id, shape) {
                    self.apply_cursor();
                }
            }
        }
    }

    /// Shows the Mac cursor shape while the pointer is over a streamed window. A desktop
    /// stream has the Mac cursor drawn in, so ours is hidden there.
    fn apply_cursor(&mut self) {
        let (Some((id, serial)), Some(p)) = (self.hover, &self.pointer) else { return };
        if self.core.is_desktop(id) {
            let _ = p.hide_cursor();
            return;
        }
        match self.core.cursor().clone() {
            CursorShape::Named(icon) => {
                if let Err(e) = p.set_cursor(&self.conn, icon) {
                    tracing::debug!("cursor {icon:?}: {e}");
                }
            }
            CursorShape::Image(img) => {
                if self.cursor_image.as_ref().is_none_or(|c| c.id != img.id) {
                    self.cursor_image = self.image_cursor(&img);
                }
                if let Some(c) = &self.cursor_image {
                    p.pointer().set_cursor(serial, Some(&c.surface), c.hot.0, c.hot.1);
                }
            }
        }
    }

    fn image_cursor(&self, img: &cursors::Image) -> Option<ImageCursor> {
        let (w, h) = (img.width as i32, img.height as i32);
        let mut pool = SlotPool::new((w * h * 4) as usize, &self.shm).inspect_err(|e| tracing::debug!("cursor pool: {e}")).ok()?;
        let (buffer, canvas) = pool.create_buffer(w, h, w * 4, wl_shm::Format::Argb8888).ok()?;
        // The pool rounds slots up (to 64 bytes), so the canvas can be longer than the image.
        let pixels = img.bgra_premultiplied();
        canvas.get_mut(..pixels.len())?.copy_from_slice(&pixels);
        let surface = self.compositor.create_surface(&self.qh);
        buffer.attach_to(&surface).ok()?;
        surface.damage_buffer(0, 0, w, h);
        surface.commit();
        Some(ImageCursor { id: img.id, hot: (img.hot_x as i32, img.hot_y as i32), surface, _pool: pool, _buffer: buffer })
    }

    fn open(&mut self, info: WindowInfo) -> Result<()> {
        anyhow::ensure!(!self.nodes.contains_key(&info.id), "window {} already shown", info.id);
        let shape = match info.kind {
            WindowKind::Normal | WindowKind::Desktop | WindowKind::Dialog => {
                let surface = self.compositor.create_surface(&self.qh);
                let window = self.xdg.create_window(surface, WindowDecorations::RequestServer, &self.qh);
                window.set_title(core::title(&info));
                window.set_app_id(APP_ID);
                // Dialogs stay above their parent, grouped with it like native dialogs.
                if info.kind == WindowKind::Dialog
                    && let Some(Shape::Toplevel(parent)) = info.parent.and_then(|p| self.nodes.get(&p)).map(|n| &n.shape)
                {
                    window.set_parent(Some(parent));
                }
                window.commit();
                Shape::Toplevel(window)
            }
            WindowKind::Popup => {
                anyhow::ensure!(self.core.popup_count() < MAX_POPUPS, "too many popups");
                let positioner = self.positioner(&info)?;
                let parent = info.parent.and_then(|p| self.nodes.get(&p)).context("popup parent is not shown")?;
                let popup = Popup::new(parent.shape.xdg_surface(), &positioner, &self.qh, &self.compositor, &self.xdg)?;
                Shape::Popup(popup)
            }
        };
        let surface = self.wgpu_surface(shape.wl_surface())?;
        self.surfaces.insert(shape.wl_surface().id(), info.id);
        self.nodes.insert(info.id, Node { shape, fullscreen: false, pending: Some((info, surface)) });
        Ok(())
    }

    /// Places a popup at its offset inside the parent's surface; the compositor may slide or
    /// flip it to keep it on screen. Input stays surface-local, so that never skews clicks.
    fn positioner(&self, info: &WindowInfo) -> Result<XdgPositioner> {
        let parent = info.parent.context("popup without parent")?;
        let parent_size = self.core.size(parent).context("popup parent not configured yet")?;
        let (anchor, offset) = core::popup_anchor(core::popup_offset(info), parent_size);
        let p = XdgPositioner::new(&self.xdg)?;
        p.set_size(info.width.clamp(1, i32::MAX as u32) as i32, info.height.clamp(1, i32::MAX as u32) as i32);
        p.set_anchor_rect(anchor.0, anchor.1, 1, 1);
        p.set_anchor(Anchor::TopLeft);
        p.set_gravity(Gravity::BottomRight);
        p.set_offset(offset.0, offset.1);
        p.set_constraint_adjustment(
            ConstraintAdjustment::SlideX | ConstraintAdjustment::SlideY | ConstraintAdjustment::FlipX | ConstraintAdjustment::FlipY,
        );
        Ok(p)
    }

    /// Draws a decoded frame and asks the compositor when it reaches the screen. The feedback
    /// request applies to the next commit, which wgpu makes while presenting.
    fn present(&mut self, window: WindowId, slot: &crate::ui::FrameSlot) {
        let feedback = match (&self.presentation, self.nodes.get(&window)) {
            (Some(p), Some(node)) => {
                let frame = Arc::new(std::sync::Mutex::new(None));
                p.feedback(node.shape.wl_surface(), &self.qh, FeedbackData { window, frame: frame.clone() });
                Some(frame)
            }
            _ => None,
        };
        let Some(frame) = self.core.frame(window, slot) else { return };
        match feedback {
            Some(slot) => *slot.lock().unwrap() = Some(frame),
            None => self.core.shown(window, frame.0, frame.1, None),
        }
        self.apply_titles();
    }

    fn apply_titles(&mut self) {
        for (id, title) in self.core.take_titles() {
            if let Some(Shape::Toplevel(w)) = self.nodes.get(&id).map(|n| &n.shape) {
                w.set_title(title);
            }
        }
    }

    fn changed(&mut self, info: WindowInfo) {
        let id = info.id;
        let Some(change) = self.core.changed(info.clone()) else { return };
        match self.nodes.get(&id).map(|n| &n.shape) {
            Some(Shape::Toplevel(w)) => {
                // A snapped size was already applied to the renderer; the next commit carries it.
                w.set_title(change.title);
            }
            Some(Shape::Popup(p)) if change.reposition => match self.positioner(&info) {
                Ok(pos) if p.xdg_popup().version() >= 3 => p.reposition(&pos, 0),
                _ => {
                    // No reposition support: show it again at the new place.
                    self.close(id);
                    if let Err(e) = self.open(info) {
                        tracing::debug!(window = id, "reopening popup: {e:#}");
                    }
                }
            },
            _ => {}
        }
    }

    /// Closes a window and the popups above it, topmost first as xdg-shell requires.
    fn close(&mut self, window: WindowId) {
        let mut order = self.core.descendants(window);
        order.reverse();
        order.push(window);
        for w in order {
            self.core.remove(w);
            if let Some(node) = self.nodes.remove(&w) {
                self.surfaces.remove(&node.shape.wl_surface().id());
            }
            if self.kb_focus == Some(w) {
                self.kb_focus = None;
            }
        }
        if self.core.should_exit() {
            self.exit = true;
        }
    }

    fn close_all(&mut self) {
        let roots: Vec<WindowId> = self
            .nodes
            .keys()
            .copied()
            .filter(|&id| self.core.info(id).is_none_or(|i| i.parent.is_none_or(|p| !self.nodes.contains_key(&p))))
            .collect();
        for id in roots {
            self.close(id);
        }
        self.nodes.clear();
    }

    /// Logical size of the biggest output, to keep a desktop window on screen.
    fn largest_output(&self) -> Option<(u32, u32)> {
        self.outputs
            .outputs()
            .filter_map(|o| self.outputs.info(&o)?.logical_size)
            .map(|(w, h)| (w.max(0) as u32, h.max(0) as u32))
            .max_by_key(|&(w, h)| w as u64 * h as u64)
    }

    /// F11 on a desktop window toggles fullscreen.
    fn toggle_fullscreen(&mut self, id: WindowId) {
        let Some(node) = self.nodes.get_mut(&id) else { return };
        let Shape::Toplevel(w) = &node.shape else { return };
        if node.fullscreen {
            w.unset_fullscreen();
        } else {
            w.set_fullscreen(None);
        }
        node.fullscreen = !node.fullscreen;
    }

    /// First configure attaches the renderer; later ones follow the compositor's size.
    fn configured(&mut self, id: WindowId, size: Option<(u32, u32)>) {
        let output = self.largest_output();
        let Some(node) = self.nodes.get_mut(&id) else { return };
        if let Some((info, surface)) = node.pending.take() {
            let size = size.unwrap_or_else(|| core::initial_size(&info, output));
            if let Err(e) = self.core.attach(info, surface, size) {
                tracing::error!(window = id, "showing window: {e:#}");
                self.core.close_requested(id);
                self.close(id);
            }
            return;
        }
        if let Some(size) = size
            && self.core.size(id) != Some(size)
        {
            self.core.resized(id, size);
        }
    }
}

impl WindowHandler for State {
    fn request_close(&mut self, _: &Connection, _: &QueueHandle<Self>, window: &Window) {
        if let Some(id) = self.id_of(window.wl_surface()) {
            self.core.close_requested(id);
            self.close(id);
        }
    }

    fn configure(&mut self, _: &Connection, _: &QueueHandle<Self>, window: &Window, configure: WindowConfigure, _: u32) {
        let Some(id) = self.id_of(window.wl_surface()) else { return };
        let size = match configure.new_size {
            (Some(w), Some(h)) => Some((w.get(), h.get())),
            _ => None,
        };
        self.configured(id, size);
    }
}

impl PopupHandler for State {
    fn configure(&mut self, _: &Connection, _: &QueueHandle<Self>, popup: &Popup, config: PopupConfigure) {
        let Some(id) = self.id_of(popup.wl_surface()) else { return };
        let size = (config.width > 0 && config.height > 0).then_some((config.width as u32, config.height as u32));
        self.configured(id, size);
    }

    fn done(&mut self, _: &Connection, _: &QueueHandle<Self>, popup: &Popup) {
        // The compositor dismissed it; the host will report the Mac menu closing.
        if let Some(id) = self.id_of(popup.wl_surface()) {
            self.close(id);
        }
    }
}

impl PointerHandler for State {
    fn pointer_frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_pointer::WlPointer, events: &[PointerEvent]) {
        for e in events {
            let Some(id) = self.id_of(&e.surface) else { continue };
            let (x, y) = e.position;
            match e.kind {
                PointerEventKind::Enter { serial } => {
                    self.hover = Some((id, serial));
                    self.apply_cursor();
                    self.core.pointer_moved(id, x, y);
                }
                PointerEventKind::Leave { .. } => {
                    if self.hover.is_some_and(|(h, _)| h == id) {
                        self.hover = None;
                    }
                }
                PointerEventKind::Motion { .. } => self.core.pointer_moved(id, x, y),
                PointerEventKind::Press { button, .. } => self.core.button(id, mouse_button(button), true),
                PointerEventKind::Release { button, .. } => self.core.button(id, mouse_button(button), false),
                PointerEventKind::Axis { horizontal, vertical, source, .. } => {
                    let continuous = matches!(
                        source,
                        Some(wl_pointer::AxisSource::Finger) | Some(wl_pointer::AxisSource::Continuous)
                    );
                    let h = axis_delta(horizontal.absolute, horizontal.value120, horizontal.discrete, continuous);
                    let v = axis_delta(vertical.absolute, vertical.value120, vertical.discrete, continuous);
                    if h.is_none() && v.is_none() {
                        continue;
                    }
                    let precise = h.or(v).is_some_and(|(_, p)| p);
                    // Wayland: positive is down/right. Ours: positive dy scrolls up.
                    let (dx, dy) = (-h.map_or(0.0, |d| d.0), -v.map_or(0.0, |d| d.0));
                    self.core.scroll(id, dx, dy, precise);
                }
            }
        }
    }
}

impl KeyboardHandler for State {
    fn enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        surface: &wl_surface::WlSurface,
        serial: u32,
        _: &[u32],
        _: &[Keysym],
    ) {
        if let Some(id) = self.id_of(surface) {
            self.kb_focus = Some(id);
            self.clip.serial = Some(serial);
            self.core.focus(id, true);
            if let Some(text) = self.clip.pending.take() {
                self.set_selection(text);
            }
        }
    }

    fn leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, surface: &wl_surface::WlSurface, _: u32) {
        if let Some(id) = self.id_of(surface) {
            self.core.focus(id, false);
            if self.kb_focus == Some(id) {
                self.kb_focus = None;
            }
        }
    }

    fn press_key(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, serial: u32, event: KeyEvent) {
        self.clip.serial = Some(serial);
        self.key(event.raw_code, true);
        self.apply_titles();
    }

    fn release_key(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, event: KeyEvent) {
        self.key(event.raw_code, false);
    }

    // macOS generates its own key repeat from the held key.
    fn repeat_key(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, _: KeyEvent) {}

    fn update_modifiers(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        _: Modifiers,
        _: RawModifiers,
        _: u32,
    ) {
    }
}

impl State {
    fn key(&mut self, evdev: u32, pressed: bool) {
        if let Some(id) = self.kb_focus
            && evdev == keymap::EVDEV_F11
            && self.core.is_desktop(id)
        {
            if pressed {
                self.toggle_fullscreen(id);
            }
            return;
        }
        if let Some(id) = self.kb_focus
            && let Some(keycode) = keymap::evdev_to_mac(evdev, false)
        {
            self.core.key(id, keycode, pressed);
        }
    }
}

impl SeatHandler for State {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seats
    }

    fn new_seat(&mut self, _: &Connection, qh: &QueueHandle<Self>, seat: wl_seat::WlSeat) {
        self.ensure_data_device(qh, &seat);
    }

    fn new_capability(&mut self, _: &Connection, qh: &QueueHandle<Self>, seat: wl_seat::WlSeat, capability: Capability) {
        // Seats present at startup are bound by SeatState::new without a new_seat callback,
        // so the first capability is the earliest reliable point to attach the data device.
        self.ensure_data_device(qh, &seat);
        match capability {
            Capability::Keyboard if self.keyboard.is_none() => match self.seats.get_keyboard(qh, &seat, None) {
                Ok(k) => self.keyboard = Some(k),
                Err(e) => tracing::warn!("keyboard: {e}"),
            },
            Capability::Pointer if self.pointer.is_none() => {
                let cursor_surface = self.compositor.create_surface(qh);
                match self.seats.get_pointer_with_theme::<_, ()>(qh, &seat, self.shm.wl_shm(), cursor_surface, ThemeSpec::default()) {
                    Ok(p) => self.pointer = Some(p),
                    Err(e) => tracing::warn!("pointer: {e}"),
                }
            }
            _ => {}
        }
    }

    fn remove_capability(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat, capability: Capability) {
        match capability {
            Capability::Keyboard => {
                if let Some(k) = self.keyboard.take() {
                    k.release();
                }
            }
            Capability::Pointer => self.pointer = None,
            _ => {}
        }
    }

    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl State {
    fn ensure_data_device(&mut self, qh: &QueueHandle<Self>, seat: &wl_seat::WlSeat) {
        if self.clip.device.is_none()
            && let Some(m) = &self.clip.manager
        {
            self.clip.device = Some(m.get_data_device(qh, seat));
            tracing::debug!("clipboard data device ready");
        }
    }

    /// Puts Mac clipboard text on the local clipboard. Wayland only lets a client with
    /// keyboard focus set the selection, so without focus it waits for the next one.
    fn set_selection(&mut self, text: String) {
        let (Some(manager), Some(device)) = (&self.clip.manager, &self.clip.device) else {
            tracing::warn!("no Wayland data device; cannot set the local clipboard");
            return;
        };
        let (Some(serial), Some(_)) = (self.clip.serial, self.kb_focus) else {
            self.clip.pending = Some(text);
            return;
        };
        let source = manager.create_copy_paste_source(&self.qh, TEXT_MIMES);
        source.set_selection(device, serial);
        tracing::debug!(bytes = text.len(), "local clipboard set from the Mac");
        self.clip.own = Some((source, text.into()));
    }

    /// Starts a clipboard read when the core needs one (a paste is waiting).
    fn service_clipboard(&mut self) {
        if !self.core.wants_clipboard() || self.clip.reading {
            return;
        }
        if let Some((_, text)) = &self.clip.own {
            let text = text.to_string();
            self.core.clipboard_read(Some(text));
            return;
        }
        let offer = self.clip.device.as_ref().and_then(|d| d.data().selection_offer());
        let Some(offer) = offer else {
            self.core.clipboard_read(None);
            return;
        };
        let mime = offer.with_mime_types(|types| TEXT_MIMES.iter().find(|m| types.iter().any(|t| t == *m)).map(|m| m.to_string()));
        let pipe = match mime.map(|m| offer.receive(m)) {
            Some(Ok(pipe)) => pipe,
            _ => {
                self.core.clipboard_read(None);
                return;
            }
        };
        self.clip.read_gen += 1;
        self.clip.reading = true;
        let (generation, results) = (self.clip.read_gen, self.clip.results.clone());
        // The source app writes at its own pace; a blocking read on a helper thread keeps the
        // event loop free. The timeout below gives up without it.
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let limit = umac_proto::MAX_CLIPBOARD_BYTES as u64 + 1;
            let text = std::fs::File::from(std::os::fd::OwnedFd::from(pipe))
                .take(limit)
                .read_to_end(&mut buf)
                .ok()
                .and_then(|_| String::from_utf8(buf).ok());
            let _ = results.send((generation, text));
        });
        let timer = Timer::from_duration(READ_TIMEOUT);
        let _ = self.loop_handle.insert_source(timer, move |_, _, state| {
            if state.clip.reading && state.clip.read_gen == generation {
                tracing::info!("clipboard owner did not answer in time; pasting without it");
                state.read_finished(generation, None);
            }
            TimeoutAction::Drop
        });
    }

    fn read_finished(&mut self, generation: u64, text: Option<String>) {
        if !self.clip.reading || generation != self.clip.read_gen {
            return;
        }
        self.clip.reading = false;
        self.core.clipboard_read(sendable(text));
    }
}

impl DataDeviceHandler for State {
    fn enter(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice, _: f64, _: f64, _: &wl_surface::WlSurface) {}
    fn leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice) {}
    fn motion(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice, _: f64, _: f64) {}
    fn selection(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice) {}
    fn drop_performed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice) {}
}

impl DataOfferHandler for State {
    fn source_actions(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &mut DragOffer, _: DndAction) {}
    fn selected_action(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &mut DragOffer, _: DndAction) {}
}

impl DataSourceHandler for State {
    fn accept_mime(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource, _: Option<String>) {}

    fn send_request(&mut self, _: &Connection, _: &QueueHandle<Self>, source: &WlDataSource, mime: String, fd: WritePipe) {
        let Some((own, text)) = &self.clip.own else { return };
        if own.inner() != source || !TEXT_MIMES.contains(&mime.as_str()) {
            return;
        }
        // A helper thread, so a slow reader never blocks the event loop.
        let text = text.clone();
        std::thread::spawn(move || {
            let mut file = std::fs::File::from(std::os::fd::OwnedFd::from(fd));
            let _ = file.write_all(text.as_bytes());
        });
    }

    fn cancelled(&mut self, _: &Connection, _: &QueueHandle<Self>, source: &WlDataSource) {
        // Another app took the clipboard.
        if self.clip.own.as_ref().is_some_and(|(s, _)| s.inner() == source) {
            tracing::debug!("local clipboard replaced by another app (or the compositor refused ours)");
            self.clip.own = None;
        }
    }

    fn dnd_dropped(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource) {}
    fn dnd_finished(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource) {}
    fn action(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource, _: DndAction) {}
}

impl CompositorHandler for State {
    fn scale_factor_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: i32) {}
    fn transform_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: wl_output::Transform) {}
    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {}
    fn surface_enter(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}
    fn surface_leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}
}

impl OutputHandler for State {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.outputs
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl ShmHandler for State {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

delegate_registry!(State);

impl ProvidesRegistryState for State {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }
    registry_handlers![OutputState, SeatState];
}

smithay_client_toolkit::delegate_dispatch2!(State);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wheel_and_touchpad_axis() {
        // One wheel notch down, hi-res and legacy.
        assert_eq!(axis_delta(15.0, 120, 1, false), Some((1.0, false)));
        assert_eq!(axis_delta(15.0, 0, 1, false), Some((1.0, false)));
        assert_eq!(axis_delta(7.5, 60, 0, false), Some((0.5, false)));
        // Desktop scroll speed doubled: twice the notches.
        assert_eq!(axis_delta(-30.0, -120, -1, false), Some((-2.0, false)));
        // Touchpad: pixels, precise.
        assert_eq!(axis_delta(-3.5, 0, 0, true), Some((-3.5, true)));
        assert_eq!(axis_delta(0.0, 0, 0, true), None);
    }

    #[test]
    fn buttons() {
        assert_eq!(mouse_button(0x110), MouseButton::Left);
        assert_eq!(mouse_button(0x111), MouseButton::Right);
        assert_eq!(mouse_button(0x112), MouseButton::Middle);
        assert_eq!(mouse_button(0x114), MouseButton::Other(4));
    }
}
