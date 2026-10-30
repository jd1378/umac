//! Injects client input as global CGEvents. Runs on its own thread because event sources
//! are not `Send`, and so slow AX calls never stall the network tasks.

use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicIsize, AtomicU64, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

use core_graphics::{
    event::{CGEvent, CGEventFlags, CGEventTapLocation, CGEventType, CGMouseButton, EventField, ScrollEventUnit},
    event_source::{CGEventSource, CGEventSourceStateID},
    geometry::CGPoint,
};
use tokio::sync::{Notify, mpsc::UnboundedSender};
use umac_proto::{HostMsg, InputEvent, MAX_CLIPBOARD_BYTES, WindowId, keys::is_modifier};

use super::{ax, ax::AxWindow, pasteboard};
use crate::{
    clipboard::{FocusTracker, PasteGate, is_paste_item},
    input_state::{ButtonKey, InputState, KeyAction, KeyRepeat, Placement, WheelLines},
};

/// Where each streamed window currently sits on the Mac; kept fresh by the session poller.
#[derive(Debug, Clone, Copy)]
pub struct Target {
    pub pid: i32,
    pub placement: Placement,
    pub popup: bool,
    /// A whole display: menus are part of the picture, so any click may hit one.
    pub desktop: bool,
    /// The parent window's app, for dialogs run by another process (the Open and Save panel
    /// service shows its panels while the parent app stays frontmost).
    pub parent_pid: Option<i32>,
}

/// How long a key press waits for the target app to come to the front after refocusing.
const FRONT_WAIT: Duration = Duration::from_millis(150);

pub type Targets = Arc<RwLock<HashMap<WindowId, Target>>>;

/// Control-stream messages back to the session that sent an input event.
pub type Reply = UnboundedSender<HostMsg>;

pub enum Command {
    Input(InputEvent, Reply),
    Focus(WindowId),
    ReleaseAll,
}

/// Shared with the pasteboard watcher: our own writes (not echoed back) and when input was
/// last injected (copies usually follow it).
#[derive(Clone, Default)]
pub struct ClipboardLink {
    pub own_write: Arc<AtomicIsize>,
    pub last_input: Arc<Mutex<Option<Instant>>>,
    pub focus: Arc<Mutex<FocusTracker>>,
    pub kick: Arc<Notify>,
}

impl ClipboardLink {
    /// Updates focus state and wakes the watcher so its fallback starts or stops.
    pub fn with_focus(&self, f: impl FnOnce(&mut FocusTracker)) {
        f(&mut self.focus.lock().unwrap());
        self.kick.notify_one();
    }
}

pub fn spawn(targets: Targets, clip: ClipboardLink, cursor: mpsc::Sender<()>) -> mpsc::Sender<Command> {
    let (tx, rx) = mpsc::channel::<Command>();
    std::thread::Builder::new()
        .name("umac-input".into())
        .spawn(move || {
            let (repeat_delay, repeat_interval) = system_key_repeat();
            tracing::debug!(?repeat_delay, ?repeat_interval, "key repeat timing");
            let Ok(source) = CGEventSource::new(CGEventSourceStateID::HIDSystemState) else {
                tracing::error!("cannot create CGEventSource; input disabled");
                return;
            };
            let mut inj = Injector {
                source,
                targets,
                state: InputState::default(),
                last_pos: HashMap::new(),
                gate: PasteGate::default(),
                clip,
                cursor,
                paste_titles: HashMap::new(),
                repeat: KeyRepeat::new(repeat_delay, repeat_interval),
                wheel: WheelLines::default(),
            };
            loop {
                let deadline = [inj.gate.deadline(), inj.repeat.deadline()].into_iter().flatten().min();
                let cmd = match deadline {
                    Some(d) => match rx.recv_timeout(d.saturating_duration_since(Instant::now())) {
                        Ok(c) => Some(c),
                        Err(mpsc::RecvTimeoutError::Timeout) => None,
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    },
                    None => match rx.recv() {
                        Ok(c) => Some(c),
                        Err(_) => break,
                    },
                };
                match cmd {
                    Some(Command::Input(ev, reply)) => inj.dispatch(ev, &reply),
                    Some(Command::Focus(w)) => inj.focus(w),
                    Some(Command::ReleaseAll) => inj.dispatch_local(InputEvent::ReleaseAll),
                    None => {
                        let now = Instant::now();
                        if let Some(held) = inj.gate.expire(now) {
                            tracing::warn!("client did not send its clipboard in time; pasting without it");
                            inj.replay(held);
                        }
                        if let Some(key) = inj.repeat.due(now) {
                            inj.post_key(key, true, None, inj.state.flags(), true);
                        }
                    }
                }
            }
            inj.release_all();
        })
        .expect("spawning input thread");
    tx
}

struct Injector {
    source: CGEventSource,
    targets: Targets,
    state: InputState,
    last_pos: HashMap<WindowId, (f64, f64)>,
    gate: PasteGate<InputEvent>,
    clip: ClipboardLink,
    /// Menu-bar Paste titles per app; walking the menu bar costs tens of milliseconds.
    paste_titles: HashMap<i32, (Instant, Vec<String>)>,
    repeat: KeyRepeat,
    wheel: WheelLines,
    /// Wakes the cursor watcher after pointer events over a streamed window.
    cursor: mpsc::Sender<()>,
}

/// The Mac's own key repeat settings (System Settings > Keyboard), via NSEvent.
fn system_key_repeat() -> (Duration, Duration) {
    let (delay, interval) = super::pasteboard::key_repeat_seconds();
    let valid = |s: f64, fallback: f64| Duration::from_secs_f64(if s.is_finite() && s > 0.0 && s < 5.0 { s } else { fallback });
    (valid(delay, 0.5), valid(interval, 0.083))
}

const PASTE_TITLES_TTL: std::time::Duration = std::time::Duration::from_secs(30);

static NEXT_NONCE: AtomicU64 = AtomicU64::new(1);

fn button_types(b: ButtonKey) -> (CGEventType, CGEventType, CGEventType, CGMouseButton) {
    match b.cg_number() {
        0 => (CGEventType::LeftMouseDown, CGEventType::LeftMouseUp, CGEventType::LeftMouseDragged, CGMouseButton::Left),
        1 => (CGEventType::RightMouseDown, CGEventType::RightMouseUp, CGEventType::RightMouseDragged, CGMouseButton::Right),
        _ => (CGEventType::OtherMouseDown, CGEventType::OtherMouseUp, CGEventType::OtherMouseDragged, CGMouseButton::Center),
    }
}

impl Injector {
    fn target(&self, w: WindowId) -> Option<Target> {
        self.targets.read().ok()?.get(&w).copied()
    }

    fn flags(&self) -> CGEventFlags {
        CGEventFlags::from_bits_retain(self.state.flags())
    }

    fn post_mouse(&self, kind: CGEventType, pos: (f64, f64), button: CGMouseButton, number: u32, clicks: Option<i64>) {
        let Ok(ev) = CGEvent::new_mouse_event(self.source.clone(), kind, CGPoint::new(pos.0, pos.1), button) else { return };
        ev.set_flags(self.flags());
        if number > 2 {
            ev.set_integer_value_field(EventField::MOUSE_EVENT_BUTTON_NUMBER, number as i64);
        }
        if let Some(c) = clicks {
            ev.set_integer_value_field(EventField::MOUSE_EVENT_CLICK_STATE, c);
        }
        ev.post(CGEventTapLocation::HID);
    }

    /// Routes one client event: holds it behind a pending paste, starts a paste hold when it
    /// clicks a Paste menu item, or injects it.
    fn dispatch(&mut self, ev: InputEvent, reply: &Reply) {
        if self.gate.is_waiting() {
            if let InputEvent::SetClipboard { nonce: Some(n), text } = &ev
                && self.gate.waits_for(*n)
            {
                self.set_clipboard(text.as_deref());
                let held = self.gate.release();
                self.replay(held);
            } else if let Some(held) = self.gate.push(ev) {
                self.replay(held);
            }
            return;
        }
        if self.clicks_paste_item(&ev) {
            let nonce = NEXT_NONCE.fetch_add(1, Ordering::Relaxed);
            if reply.send(HostMsg::ClipboardRequest { nonce }).is_ok() {
                self.gate.hold(nonce, ev, Instant::now());
                return;
            }
        }
        self.dispatch_local(ev);
    }

    fn replay(&mut self, events: Vec<InputEvent>) {
        for ev in events {
            self.dispatch_local(ev);
        }
    }

    fn dispatch_local(&mut self, ev: InputEvent) {
        match ev {
            // A late answer to a paste that already went ahead: too late to matter.
            InputEvent::SetClipboard { nonce: Some(_), .. } => {}
            InputEvent::SetClipboard { nonce: None, text } => self.set_clipboard(text.as_deref()),
            ev => {
                // Copies are caused by keys and clicks; pointer motion and scrolling are not.
                let may_copy = matches!(ev, InputEvent::Key { .. } | InputEvent::MouseButton { .. });
                // A desktop stream has the Mac cursor drawn in, so only windows need its shape.
                let pointer_window = match ev {
                    InputEvent::MouseMove { window, .. } | InputEvent::MouseButton { window, .. } | InputEvent::Scroll { window, .. } => Some(window),
                    _ => None,
                };
                self.handle(ev);
                if pointer_window.and_then(|w| self.target(w)).is_some_and(|t| !t.desktop) {
                    let _ = self.cursor.send(());
                }
                if may_copy {
                    *self.clip.last_input.lock().unwrap() = Some(Instant::now());
                    self.clip.kick.notify_one();
                }
            }
        }
    }

    fn set_clipboard(&mut self, text: Option<&str>) {
        let Some(text) = text else { return };
        if text.len() > MAX_CLIPBOARD_BYTES {
            tracing::warn!(bytes = text.len(), "client clipboard too large; ignored");
            return;
        }
        let count = pasteboard::write_text(text);
        self.clip.own_write.store(count, Ordering::Relaxed);
        tracing::debug!(bytes = text.len(), "pasteboard set from client");
    }

    /// A button release over a popup on a menu item whose shortcut is Cmd+V. Menu items act
    /// on release, so this is the moment to fetch the client's clipboard first.
    fn clicks_paste_item(&mut self, ev: &InputEvent) -> bool {
        let InputEvent::MouseButton { window, x, y, pressed: false, .. } = *ev else { return false };
        let Some(t) = self.target(window) else { return false };
        let pos = t.placement.to_global(x, y);
        let popup_pid = self.targets.read().ok().and_then(|ts| ts.values().find(|t| t.popup && t.placement.contains(pos)).map(|t| t.pid));
        if popup_pid.is_none() && !t.desktop {
            return false;
        }
        let now = Instant::now();
        let Some((item, owner)) = ax::menu_item_at(pos.0, pos.1) else { return false };
        let pid = popup_pid.unwrap_or(owner);
        let titles = match self.paste_titles.get(&pid) {
            Some((at, t)) if now.duration_since(*at) < PASTE_TITLES_TTL => t,
            _ => {
                let t = ax::paste_titles(pid);
                &self.paste_titles.entry(pid).insert_entry((now, t)).into_mut().1
            }
        };
        let paste = is_paste_item(&item, titles);
        tracing::debug!(paste, ms = now.elapsed().as_millis() as u64, "classified menu click");
        if paste {
            tracing::info!(window, "Paste menu item clicked; fetching the client clipboard first");
        }
        paste
    }

    fn handle(&mut self, ev: InputEvent) {
        match ev {
            InputEvent::MouseMove { window, x, y } => {
                let Some(t) = self.target(window) else { return };
                let pos = t.placement.to_global(x, y);
                self.last_pos.insert(window, pos);
                match self.state.dragging() {
                    Some(b) => {
                        let (_, _, drag, cgb) = button_types(b);
                        self.post_mouse(drag, pos, cgb, b.cg_number(), None);
                    }
                    None => self.post_mouse(CGEventType::MouseMoved, pos, CGMouseButton::Left, 0, None),
                }
            }
            InputEvent::MouseButton { window, x, y, button, pressed } => {
                // A release must never be dropped, even if its window (a menu) just closed,
                // or the Mac keeps the button held and later clicks turn into drags.
                let pos = match self.target(window) {
                    Some(t) => t.placement.to_global(x, y),
                    None if !pressed => match self.last_pos.get(&window).or_else(|| self.last_pos.values().next()) {
                        Some(&p) => p,
                        None => return,
                    },
                    None => return,
                };
                self.last_pos.insert(window, pos);
                let clicks = self.state.button(button, pressed, pos, Instant::now());
                let b = ButtonKey::from(button);
                let (down, up, _, cgb) = button_types(b);
                self.post_mouse(if pressed { down } else { up }, pos, cgb, b.cg_number(), Some(clicks));
            }
            InputEvent::Scroll { window, dx, dy, precise } => {
                let Some(t) = self.target(window) else { return };
                let pos = self.last_pos.get(&window).copied().unwrap_or_else(|| {
                    let p = t.placement;
                    (p.x + p.width / 2.0, p.y + p.height / 2.0)
                });
                // Same sign convention on both ends: positive dy is wheel up (away from the user).
                // Wheels arrive in lines and become line scrolls, so the Mac scrolls like it does
                // for its own mouse; fractions from high-resolution wheels carry over.
                let (unit, sx, sy) = if precise {
                    (ScrollEventUnit::PIXEL, (dx as f64 / t.placement.scale) as i32, (dy as f64 / t.placement.scale) as i32)
                } else {
                    let (sx, sy) = self.wheel.add(dx as f64, dy as f64);
                    (ScrollEventUnit::LINE, sx, sy)
                };
                if sx == 0 && sy == 0 {
                    return;
                }
                let Ok(ev) = CGEvent::new_scroll_event(self.source.clone(), unit, 2, sy, sx, 0) else { return };
                ev.set_location(CGPoint::new(pos.0, pos.1));
                ev.set_flags(self.flags());
                if precise {
                    ev.set_integer_value_field(EventField::SCROLL_WHEEL_EVENT_IS_CONTINUOUS, 1);
                }
                ev.post(CGEventTapLocation::HID);
            }
            InputEvent::Key { window, keycode, pressed } => {
                // Releases and modifiers always pass so nothing stays held; a press only goes
                // out when the target app is in front, never to whatever app happens to be.
                if pressed && !is_modifier(keycode) && !self.key_reaches(window) {
                    tracing::warn!(window, "dropped a key press: the window's app is not frontmost and could not be focused");
                    return;
                }
                self.key(keycode, pressed)
            }
            InputEvent::ReleaseAll => self.release_all(),
            InputEvent::SetClipboard { .. } => {}
        }
    }

    fn key(&mut self, keycode: u16, pressed: bool) {
        let (keycode, down, kind, flags) = match self.state.key(keycode, pressed) {
            KeyAction::Ignore => return,
            KeyAction::FlagsChanged { keycode, flags } => (keycode, pressed, Some(CGEventType::FlagsChanged), flags),
            KeyAction::Key { keycode, pressed, flags } => (keycode, pressed, None, flags),
        };
        if kind.is_none() {
            if down {
                self.repeat.press(keycode, Instant::now());
            } else {
                self.repeat.release(keycode);
            }
        }
        self.post_key(keycode, down, kind, flags, false);
    }

    fn post_key(&self, keycode: u16, down: bool, kind: Option<CGEventType>, flags: u64, autorepeat: bool) {
        let Ok(ev) = CGEvent::new_keyboard_event(self.source.clone(), keycode, down) else { return };
        if let Some(k) = kind {
            ev.set_type(k);
        }
        if autorepeat {
            ev.set_integer_value_field(EventField::KEYBOARD_EVENT_AUTOREPEAT, 1);
        }
        ev.set_flags(CGEventFlags::from_bits_retain(flags));
        ev.post(CGEventTapLocation::HID);
    }

    fn release_all(&mut self) {
        self.repeat.clear();
        let (keys, buttons, mods) = self.state.take_held();
        for k in keys {
            if let Ok(ev) = CGEvent::new_keyboard_event(self.source.clone(), k, false) {
                ev.set_flags(self.flags());
                ev.post(CGEventTapLocation::HID);
            }
        }
        let pos = self.last_pos.values().next().copied().unwrap_or((0.0, 0.0));
        for b in buttons {
            let (_, up, _, cgb) = button_types(b);
            self.post_mouse(up, pos, cgb, b.cg_number(), None);
        }
        for m in mods {
            if let Ok(ev) = CGEvent::new_keyboard_event(self.source.clone(), m, false) {
                ev.set_type(CGEventType::FlagsChanged);
                ev.set_flags(self.flags());
                ev.post(CGEventTapLocation::HID);
            }
        }
    }

    fn key_reaches(&mut self, w: WindowId) -> bool {
        let Some(t) = self.target(w) else { return false };
        if t.desktop {
            return true;
        }
        let in_front = || pasteboard::frontmost_pid().is_some_and(|p| p == t.pid || Some(p) == t.parent_pid);
        if in_front() {
            return true;
        }
        self.focus(w);
        let deadline = Instant::now() + FRONT_WAIT;
        while Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
            if in_front() {
                return true;
            }
        }
        false
    }

    fn focus(&mut self, w: WindowId) {
        let Some(t) = self.target(w) else { return };
        if let Err(e) = AxWindow::find(t.pid, w).and_then(|ax| ax.focus()) {
            tracing::warn!(window = w, "focus failed: {e}");
        }
    }
}
