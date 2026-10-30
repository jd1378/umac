use std::{sync::OnceLock, time::Instant};

static START: OnceLock<Instant> = OnceLock::new();

/// Microseconds on a process-wide monotonic clock.
pub fn now_us() -> u64 {
    START.get_or_init(Instant::now).elapsed().as_micros() as u64
}

#[cfg(target_os = "macos")]
mod mach {
    #[repr(C)]
    #[derive(Default)]
    pub struct Timebase {
        pub numer: u32,
        pub denom: u32,
    }

    unsafe extern "C" {
        pub fn mach_absolute_time() -> u64;
        pub fn mach_timebase_info(info: *mut Timebase) -> i32;
    }
}

/// A mach absolute time (as in `SCStreamFrameInfo.displayTime`) on the `now_us` clock, or
/// `None` if it is implausibly far from now. ScreenCaptureKit stamps frames with the refresh
/// they are shown at, which can be a few milliseconds ahead of delivery.
#[cfg(target_os = "macos")]
pub fn from_mach(ticks: u64) -> Option<u64> {
    static TIMEBASE: OnceLock<(i128, i128)> = OnceLock::new();
    let &(numer, denom) = TIMEBASE.get_or_init(|| {
        let mut tb = mach::Timebase::default();
        unsafe { mach::mach_timebase_info(&mut tb) };
        (tb.numer.max(1) as i128, tb.denom.max(1) as i128)
    });
    let (now, now_ticks) = (now_us() as i128, unsafe { mach::mach_absolute_time() } as i128);
    let ahead_us = (ticks as i128 - now_ticks) * numer / denom / 1000;
    (-1_000_000..100_000).contains(&ahead_us).then(|| (now + ahead_us).max(0) as u64)
}
