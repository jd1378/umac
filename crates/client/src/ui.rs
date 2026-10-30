//! Messages from the network and decode threads to whichever windowing shell is running.

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicU32, Ordering},
    },
    time::Instant,
};

use umac_proto::{CursorShape, WindowId, WindowInfo};

use crate::decode::Picture;

pub enum UiEvent {
    Open(WindowInfo),
    /// A decoded picture is waiting in the window's slot.
    Frame { window: WindowId, slot: Arc<FrameSlot> },
    Changed(WindowInfo),
    Closed(WindowId),
    Shutdown(String),
    /// The Mac clipboard changed: put this text on the local clipboard.
    Clipboard(String),
    /// The host needs our clipboard for a paste; answer through `Core::clipboard_read`.
    ClipboardRequest(u64),
    /// The Mac cursor changed; `shape` is `None` for a cached image (`CursorKnown`).
    Cursor { id: u64, shape: Option<CursorShape> },
}

/// The newest decoded picture of one window, waiting for the UI thread. Holding at most one
/// keeps a stalled UI from piling up pictures: a zero-copy picture pins a decoder surface,
/// and the decoder's pool is small.
#[derive(Default)]
pub struct FrameSlot {
    pending: Mutex<Option<(Picture, Instant)>>,
    dropped: AtomicU32,
}

impl FrameSlot {
    /// Stores a picture, replacing one the UI has not taken yet. Returns true if the slot was
    /// empty, i.e. the UI needs a `Frame` event to come and take it.
    pub fn put(&self, picture: Picture, decoded: Instant) -> bool {
        let old = self.pending.lock().unwrap().replace((picture, decoded));
        if old.is_some() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
        old.is_none()
    }

    /// The waiting picture and when it was decoded.
    pub fn take(&self) -> Option<(Picture, Instant)> {
        self.pending.lock().unwrap().take()
    }

    /// Pictures replaced before the UI took them, since the last call.
    pub fn take_dropped(&self) -> u32 {
        self.dropped.swap(0, Ordering::Relaxed)
    }
}

/// Wakes the shell's event loop with an event. `send` returns false once the loop is gone.
#[derive(Clone)]
pub struct UiSender(Arc<dyn Fn(UiEvent) -> bool + Send + Sync>);

impl UiSender {
    pub fn new(send: impl Fn(UiEvent) -> bool + Send + Sync + 'static) -> Self {
        Self(Arc::new(send))
    }

    pub fn send(&self, event: UiEvent) -> bool {
        (self.0)(event)
    }
}
