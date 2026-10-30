//! PyroWave encoder knobs, shared between a stream and its session so rate control applies
//! without a restart. Lives apart from the encoder so builds without PyroWave keep the session.

use std::sync::{
    Mutex,
    atomic::{AtomicBool, AtomicU32, AtomicU64},
};

/// Live encoder knobs, shared with the session so rate control applies without a restart.
#[derive(Debug, Default)]
pub struct Knobs {
    /// Bits per pixel times 1000 (current, lowered on loss).
    pub bpp_milli: AtomicU32,
    /// Configured bits per pixel times 1000.
    pub target_milli: AtomicU32,
    /// Rate cap in bytes per frame for a changing window.
    pub cap_bytes: AtomicU32,
    /// The next encode is the one-off high quality frame of an idle window.
    pub refine: AtomicBool,
    /// Largest packet that fits a datagram after the PyroWave header.
    pub packet_boundary: AtomicU32,
    /// Packets larger than the boundary (a single big block); sent split over datagrams.
    pub oversized: AtomicU64,
    /// Encode call durations in microseconds (submit to packets ready), drained by the session.
    pub encode_us: Mutex<Vec<u32>>,
    /// Send only blocks touching changed regions.
    pub changed_only: AtomicBool,
    /// Changed regions of the frame being submitted, as ScreenCaptureKit reported them.
    pub dirty: Mutex<Option<Vec<[f64; 4]>>>,
    /// Blocks sent and blocks in the frames, and bytes sent, since the session last drained them.
    pub kept_blocks: AtomicU64,
    pub total_blocks: AtomicU64,
    pub sent_bytes: AtomicU64,
}
