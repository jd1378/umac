//! Messages from the network and decode threads to whichever windowing shell is running.

use std::sync::Arc;

use umac_proto::{CursorShape, WindowId, WindowInfo};

use crate::decode::Nv12Frame;

pub enum UiEvent {
    Open(WindowInfo),
    Frame { window: WindowId, frame: Nv12Frame },
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
