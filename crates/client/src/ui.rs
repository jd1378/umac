//! Messages from the network and decode threads to whichever windowing shell is running.

use std::sync::Arc;

use umac_proto::{WindowId, WindowInfo};

use crate::decode::Nv12Frame;

pub enum UiEvent {
    Open(WindowInfo),
    Frame { window: WindowId, frame: Nv12Frame },
    Changed(WindowInfo),
    Closed(WindowId),
    Shutdown(String),
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
