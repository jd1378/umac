use std::{sync::OnceLock, time::Instant};

static START: OnceLock<Instant> = OnceLock::new();

/// Microseconds on a process-wide monotonic clock.
pub fn now_us() -> u64 {
    START.get_or_init(Instant::now).elapsed().as_micros() as u64
}
