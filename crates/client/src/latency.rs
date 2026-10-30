//! Latency measurement: clock sync with the host, per-frame timestamps and stage statistics.
//!
//! Frames carry their capture time on the host clock (truncated to u32 microseconds). With
//! the host clock offset known from `TimeProbe` round trips, each frame's capture moment is
//! placed on the local monotonic clock, so glass-to-glass latency is "shown here" minus
//! "composited on the Mac", give or take half the best round trip.

use std::{
    collections::VecDeque,
    sync::OnceLock,
    time::{Duration, Instant},
};

/// Round trips kept for the offset estimate; the fastest one wins (least queueing).
const SYNC_WINDOW: usize = 16;

static BASE: OnceLock<Instant> = OnceLock::new();

fn base() -> Instant {
    *BASE.get_or_init(Instant::now)
}

/// Microseconds on the local monotonic clock used for clock probes.
pub fn now_us() -> u64 {
    base().elapsed().as_micros() as u64
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Sample {
    rtt_us: u64,
    /// host clock minus the local clock at the probe's midpoint.
    offset_us: i64,
}

#[derive(Default)]
pub struct ClockSync {
    samples: VecDeque<Sample>,
}

impl ClockSync {
    /// One probe: sent at local `t0`, stamped by the host at `host_us`, answered at local `t3`.
    pub fn add(&mut self, t0: u64, host_us: u64, t3: u64) {
        if t3 < t0 {
            return;
        }
        let mid = t0 + (t3 - t0) / 2;
        self.samples.push_back(Sample { rtt_us: t3 - t0, offset_us: host_us as i64 - mid as i64 });
        if self.samples.len() > SYNC_WINDOW {
            self.samples.pop_front();
        }
    }

    /// Offset (host minus local) and its uncertainty (half the best round trip).
    pub fn estimate(&self) -> Option<(i64, u64)> {
        let best = self.samples.iter().min_by_key(|s| s.rtt_us)?;
        Some((best.offset_us, best.rtt_us / 2))
    }

    pub fn uncertainty_us(&self) -> Option<u64> {
        self.estimate().map(|e| e.1)
    }

    /// When a frame captured at host time `capture_u32` (wrapping microseconds) happened on
    /// the local clock, judged at local time `now`.
    pub fn capture_instant(&self, capture_u32: u32, now: Instant) -> Option<Instant> {
        let (offset, _) = self.estimate()?;
        let local_us = now.saturating_duration_since(base()).as_micros() as i64;
        let host_now = local_us + offset;
        // Frames are at most seconds old, so the low 32 bits pin down the full time.
        let age = (host_now as u32).wrapping_sub(capture_u32) as u64;
        if age > 60_000_000 {
            return None;
        }
        now.checked_sub(Duration::from_micros(age))
    }
}

/// Timestamps of one frame on the local clock.
#[derive(Debug, Clone, Copy)]
pub struct FrameTiming {
    /// Composited on the Mac, if the clocks are synced.
    pub capture: Option<Instant>,
    pub first_datagram: Instant,
    pub complete: Instant,
    pub decoded: Instant,
}

/// Host stages of one stream over the last report interval, microseconds.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct HostStages {
    pub encode_p50: u32,
    pub encode_p95: u32,
    pub send_p50: u32,
    pub uncertainty: u64,
}

/// Value at fraction `q` of `v` (sorted in place); `None` if empty.
pub fn percentile(v: &mut [u32], q: f64) -> Option<u32> {
    if v.is_empty() {
        return None;
    }
    v.sort_unstable();
    Some(v[((v.len() - 1) as f64 * q.clamp(0.0, 1.0)).round() as usize])
}

fn us(d: Duration) -> u32 {
    d.as_micros().min(u32::MAX as u128) as u32
}

/// Per-frame client stages of one window, microseconds.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Stages {
    /// Capture to frame complete here: host encode + host send + network + reassembly.
    pub to_complete: Option<u32>,
    /// First datagram to frame complete (transfer time of the frame itself).
    pub transfer: u32,
    pub decode: u32,
    /// Decoded to submitted for presentation (queueing on the UI thread and render).
    pub render: u32,
    /// Submitted to on screen, if the compositor reported it.
    pub display: Option<u32>,
    /// Capture to on screen (or to submit when the compositor gives no feedback).
    pub glass: Option<u32>,
}

impl Stages {
    pub fn of(t: &FrameTiming, submit: Instant, shown: Option<Instant>) -> Self {
        let end = shown.unwrap_or(submit);
        Self {
            to_complete: t.capture.map(|c| us(t.complete.saturating_duration_since(c))),
            transfer: us(t.complete.saturating_duration_since(t.first_datagram)),
            decode: us(t.decoded.saturating_duration_since(t.complete)),
            render: us(submit.saturating_duration_since(t.decoded)),
            display: shown.map(|s| us(s.saturating_duration_since(submit))),
            glass: t.capture.map(|c| us(end.saturating_duration_since(c))),
        }
    }
}

/// Stage samples of one window between reports, plus totals for a final summary.
#[derive(Default)]
pub struct Recorder {
    window: Vec<Stages>,
    run: Vec<Stages>,
    pub host: Option<HostStages>,
    keep_run: bool,
}

fn column(v: &[Stages], f: impl Fn(&Stages) -> Option<u32>) -> Vec<u32> {
    v.iter().filter_map(f).collect()
}

/// p50/p95 of each stage over `v`, in milliseconds, with the host stages for the network split.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Report {
    pub frames: usize,
    pub glass: Option<(f64, f64)>,
    pub encode: Option<(f64, f64)>,
    pub send: Option<f64>,
    /// Capture to complete minus the host stages: time on the wire and in reassembly.
    pub network: Option<f64>,
    pub transfer: Option<(f64, f64)>,
    pub decode: Option<(f64, f64)>,
    pub render: Option<(f64, f64)>,
    pub display: Option<(f64, f64)>,
    pub uncertainty: Option<f64>,
}

fn p(v: &mut [u32]) -> Option<(f64, f64)> {
    Some((percentile(v, 0.5)? as f64 / 1e3, percentile(v, 0.95)? as f64 / 1e3))
}

pub fn report(v: &[Stages], host: Option<HostStages>) -> Report {
    let to_complete = p(&mut column(v, |s| s.to_complete)).map(|x| x.0);
    let network = match (to_complete, host) {
        (Some(c), Some(h)) => Some((c - (h.encode_p50 as f64 + h.send_p50 as f64) / 1e3).max(0.0)),
        _ => None,
    };
    Report {
        frames: v.len(),
        glass: p(&mut column(v, |s| s.glass)),
        encode: host.map(|h| (h.encode_p50 as f64 / 1e3, h.encode_p95 as f64 / 1e3)),
        send: host.map(|h| h.send_p50 as f64 / 1e3),
        network,
        transfer: p(&mut column(v, |s| Some(s.transfer))),
        decode: p(&mut column(v, |s| Some(s.decode))),
        render: p(&mut column(v, |s| Some(s.render))),
        display: p(&mut column(v, |s| s.display)),
        uncertainty: host.map(|h| h.uncertainty as f64 / 1e3),
    }
}

impl Report {
    /// One compact log line: `p50/p95` milliseconds per stage.
    pub fn line(&self) -> String {
        let pair = |x: Option<(f64, f64)>| x.map_or("-".into(), |(a, b)| format!("{a:.1}/{b:.1}"));
        let one = |x: Option<f64>| x.map_or("-".into(), |a| format!("{a:.1}"));
        format!(
            "glass {} | encode {} send {} net {} | transfer {} decode {} render {} display {} | clock ±{} ms, {} frames",
            pair(self.glass),
            pair(self.encode),
            one(self.send),
            one(self.network),
            pair(self.transfer),
            pair(self.decode),
            pair(self.render),
            pair(self.display),
            one(self.uncertainty),
            self.frames
        )
    }
}

impl Recorder {
    pub fn new(keep_run: bool) -> Self {
        Self { keep_run, ..Self::default() }
    }

    pub fn record(&mut self, s: Stages) {
        if self.window.len() < 4096 {
            self.window.push(s);
        }
        if self.keep_run && self.run.len() < 1 << 20 {
            self.run.push(s);
        }
    }

    /// Report for the interval since the last call, then starts a new one.
    pub fn take(&mut self) -> Report {
        let r = report(&self.window, self.host);
        self.window.clear();
        r
    }

    pub fn run_report(&self) -> Report {
        report(&self.run, self.host)
    }
}

/// Converts a presentation-time timestamp (on `clock_id`, usually CLOCK_MONOTONIC) to an
/// `Instant`, which is CLOCK_MONOTONIC on Linux.
#[cfg(target_os = "linux")]
pub fn from_clock(clock_id: u32, sec: u64, nsec: u32) -> Option<Instant> {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    let now = Instant::now();
    if unsafe { libc::clock_gettime(clock_id as libc::clockid_t, &mut ts) } != 0 {
        return None;
    }
    let now_ns = ts.tv_sec as i128 * 1_000_000_000 + ts.tv_nsec as i128;
    let then_ns = sec as i128 * 1_000_000_000 + nsec as i128;
    let ago = now_ns - then_ns;
    if ago < 0 {
        return now.checked_add(Duration::from_nanos((-ago) as u64));
    }
    now.checked_sub(Duration::from_nanos(ago as u64))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offset_uses_fastest_round_trip() {
        let mut c = ClockSync::default();
        assert_eq!(c.estimate(), None);
        // True offset 1_000_000; a slow probe whose reply was delayed on the way back.
        c.add(100, 1_000_150, 400);
        c.add(1000, 1_001_010, 1020);
        assert_eq!(c.estimate(), Some((1_000_000, 10)));
        for i in 0..SYNC_WINDOW as u64 {
            c.add(5000 + i * 100, 1_005_000 + i * 100 + 50, 5000 + i * 100 + 100);
        }
        // The fast sample aged out of the window.
        assert_eq!(c.estimate(), Some((1_000_000, 50)));
    }

    #[test]
    fn capture_time_unwraps_u32() {
        let mut c = ClockSync::default();
        let now = Instant::now();
        let local = now.duration_since(base()).as_micros() as u64;
        // Host clock far past 2^32 microseconds: capture_us has wrapped.
        let offset: u64 = (1u64 << 32) * 3 + 123;
        c.add(local, local + offset, local);
        let host_now = local + offset;
        let capture = host_now - 20_000;
        let got = c.capture_instant(capture as u32, now).unwrap();
        assert_eq!(now.duration_since(got), Duration::from_micros(20_000));
    }

    #[test]
    fn stages_and_report() {
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        let t = FrameTiming { capture: Some(t0), first_datagram: t0 + ms(5), complete: t0 + ms(8), decoded: t0 + ms(12) };
        let s = Stages::of(&t, t0 + ms(13), Some(t0 + ms(20)));
        assert_eq!(s.to_complete, Some(8000));
        assert_eq!((s.transfer, s.decode, s.render, s.display, s.glass), (3000, 4000, 1000, Some(7000), Some(20000)));
        let host = HostStages { encode_p50: 4000, encode_p95: 5000, send_p50: 1000, uncertainty: 500 };
        let r = report(&[s; 3], Some(host));
        assert_eq!(r.glass, Some((20.0, 20.0)));
        assert_eq!(r.network, Some(3.0));
        assert!(r.line().starts_with("glass 20.0/20.0 | encode 4.0/5.0 send 1.0 net 3.0"));
        // Without feedback the glass time ends at submit.
        assert_eq!(Stages::of(&t, t0 + ms(13), None).glass, Some(13000));
    }

    #[test]
    fn percentile_edges() {
        assert_eq!(percentile(&mut [], 0.5), None);
        assert_eq!(percentile(&mut [3, 1, 2], 0.5), Some(2));
        assert_eq!(percentile(&mut [3, 1, 2], 1.0), Some(3));
    }
}
