//! Host side latency samples per stream, reported to clients that ask for them.

use std::collections::HashMap;

use umac_proto::{HostMsg, WindowId};

/// Samples kept per stream between reports; older ones are dropped.
const MAX_SAMPLES: usize = 2048;

/// Value at fraction `q` (0..=1) of `v`, which this sorts. `None` for an empty slice.
pub fn percentile(v: &mut [u32], q: f64) -> Option<u32> {
    if v.is_empty() {
        return None;
    }
    v.sort_unstable();
    let i = ((v.len() - 1) as f64 * q.clamp(0.0, 1.0)).round() as usize;
    Some(v[i])
}

#[derive(Default)]
pub struct StageStats {
    /// Per stream: (capture to encoded, encoded to last datagram queued), microseconds.
    samples: HashMap<WindowId, Vec<(u32, u32)>>,
}

impl StageStats {
    pub fn record(&mut self, window: WindowId, encode_us: u64, send_us: u64) {
        let v = self.samples.entry(window).or_default();
        if v.len() < MAX_SAMPLES {
            v.push((encode_us.min(u32::MAX as u64) as u32, send_us.min(u32::MAX as u64) as u32));
        }
    }

    /// One `StreamStats` per stream with samples since the last call, then starts over.
    pub fn drain(&mut self) -> Vec<HostMsg> {
        self.samples
            .drain()
            .filter(|(_, v)| !v.is_empty())
            .map(|(window, v)| {
                let mut enc: Vec<u32> = v.iter().map(|s| s.0).collect();
                let mut snd: Vec<u32> = v.iter().map(|s| s.1).collect();
                HostMsg::StreamStats {
                    window,
                    encode_p50_us: percentile(&mut enc, 0.5).unwrap_or(0),
                    encode_p95_us: percentile(&mut enc, 0.95).unwrap_or(0),
                    send_p50_us: percentile(&mut snd, 0.5).unwrap_or(0),
                    frames: v.len() as u32,
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles() {
        assert_eq!(percentile(&mut [], 0.5), None);
        assert_eq!(percentile(&mut [7], 0.95), Some(7));
        let mut v: Vec<u32> = (1..=100).rev().collect();
        assert_eq!(percentile(&mut v, 0.5), Some(51));
        assert_eq!(percentile(&mut v, 0.95), Some(95));
        assert_eq!(percentile(&mut v, 0.0), Some(1));
    }

    #[test]
    fn drain_reports_and_resets() {
        let mut s = StageStats::default();
        for i in 0..10 {
            s.record(3, 1000 + i, 200);
        }
        let out = s.drain();
        assert!(matches!(out[..], [HostMsg::StreamStats { window: 3, frames: 10, send_p50_us: 200, .. }]));
        assert!(s.drain().is_empty());
    }
}
