//! PyroWave transport policy: the bit budget and how packets map onto datagrams.
//! Intra-only frames have no reference chain, so loss only blurs blocks; the budget still
//! drops quickly on loss (a congested link loses more) and creeps back.

use std::{
    ops::Range,
    time::{Duration, Instant},
};

use umac_pyrowave_decode::select::Rect;

/// Most datagrams one encoder packet may be split over; more means a broken packet size.
const MAX_PIECES: usize = 8;

/// Splits encoder packets into datagram payloads of at most `room` bytes. Each piece is
/// paired with whether the packet continues in the next one. `None` if a packet needs more
/// than `MAX_PIECES` pieces or the frame more datagrams than a `u16` counts.
pub fn pieces(packets: &[Range<usize>], room: usize) -> Option<Vec<(Range<usize>, bool)>> {
    if room == 0 {
        return None;
    }
    let mut out = Vec::with_capacity(packets.len());
    for p in packets {
        let n = p.len().div_ceil(room).max(1);
        if n > MAX_PIECES {
            return None;
        }
        for k in 0..n {
            let start = p.start + k * room;
            out.push((start..(start + room).min(p.end), k + 1 < n));
        }
    }
    (out.len() <= u16::MAX as usize).then_some(out)
}

/// Loss above this fraction of datagrams counts as congestion.
const LOSS_THRESHOLD: f64 = 0.02;
/// Multiplicative cut per congestion event, at most once per `HOLD`.
const CUT: f64 = 0.75;
const HOLD: Duration = Duration::from_millis(500);
/// Recovery speed: fraction of the configured budget regained per second without loss.
const RECOVER_PER_SEC: f64 = 0.05;
/// Below this, small text is no longer legible (M1 benchmark: unusable at about 0.4 bpp).
pub const MIN_BPP: f64 = 0.5;

pub fn frame_bytes(width: u32, height: u32, bpp: f64) -> usize {
    (width as f64 * height as f64 * bpp / 8.0) as usize
}

/// Changed regions from ScreenCaptureKit (`[x, y, w, h]`, surface pixels) as rectangles
/// clipped to the surface. `None` (send everything) when a rect lies clearly outside the
/// surface, which would mean the coordinates are not what this assumes.
pub fn dirty_to_surface(raw: &[[f64; 4]], width: u32, height: u32) -> Option<Vec<Rect>> {
    let (w, h) = (width as f64, height as f64);
    let mut out = Vec::with_capacity(raw.len());
    for &[x, y, rw, rh] in raw {
        if !(x.is_finite() && y.is_finite() && rw.is_finite() && rh.is_finite()) || x < -2.0 || y < -2.0 || x + rw > w + 2.0 || y + rh > h + 2.0 {
            return None;
        }
        let r = Rect { x0: x.floor().max(0.0) as i64, y0: y.floor().max(0.0) as i64, x1: (x + rw).ceil().min(w) as i64, y1: (y + rh).ceil().min(h) as i64 };
        if r.x1 > r.x0 && r.y1 > r.y0 {
            out.push(r);
        }
    }
    Some(out)
}

/// Luma pixels per side of the tiles compared by [`changed_tiles`].
pub const TILE: usize = 16;

/// One NV12 frame as CPU readable planes: luma and interleaved CbCr, with their row strides.
pub struct Nv12<'a> {
    pub y: &'a [u8],
    pub y_stride: usize,
    pub uv: &'a [u8],
    pub uv_stride: usize,
}

/// Regions that differ between two frames of `width` x `height`, as `TILE` sized tiles merged
/// into runs along each tile row. Captured pixels of unchanged content are byte identical, so
/// this is exact; it replaces ScreenCaptureKit's dirty rects where those cover the whole frame
/// (window captures report every frame as fully dirty).
pub fn changed_tiles(prev: &Nv12, cur: &Nv12, width: usize, height: usize) -> Vec<Rect> {
    let mut out = Vec::new();
    let differs = |tx: usize, ty: usize| {
        let (x0, x1) = (tx * TILE, ((tx + 1) * TILE).min(width));
        let (y0, y1) = (ty * TILE, ((ty + 1) * TILE).min(height));
        let luma = (y0..y1).any(|y| prev.y[y * prev.y_stride + x0..y * prev.y_stride + x1] != cur.y[y * cur.y_stride + x0..y * cur.y_stride + x1]);
        luma || (y0 / 2..y1.div_ceil(2)).any(|y| {
            let (a, b) = (x0 & !1, x1.next_multiple_of(2).min(width.next_multiple_of(2)));
            prev.uv[y * prev.uv_stride + a..y * prev.uv_stride + b] != cur.uv[y * cur.uv_stride + a..y * cur.uv_stride + b]
        })
    };
    for ty in 0..height.div_ceil(TILE) {
        let mut run: Option<usize> = None;
        for tx in 0..=width.div_ceil(TILE) {
            let changed = tx < width.div_ceil(TILE) && differs(tx, ty);
            match (changed, run) {
                (true, None) => run = Some(tx),
                (false, Some(start)) => {
                    let y1 = ((ty + 1) * TILE).min(height);
                    out.push(Rect { x0: (start * TILE) as i64, y0: (ty * TILE) as i64, x1: ((tx * TILE).min(width)) as i64, y1: y1 as i64 });
                    run = None;
                }
                _ => {}
            }
        }
    }
    out
}

/// Share of the frame the rects cover (overlaps counted twice, which only overestimates).
pub fn coverage(rects: &[Rect], width: u32, height: u32) -> f64 {
    let area: i64 = rects.iter().map(|r| (r.x1 - r.x0) * (r.y1 - r.y0)).sum();
    area as f64 / (width as f64 * height as f64).max(1.0)
}

/// Quality of the one-off frame sent when a window stops changing; static content then looks
/// like the source (the M1 benchmark was effectively lossless from 2.4 bpp).
pub const REFINE_BPP: f64 = 3.0;

/// Bytes for one frame. Intra-only frames never refine on their own, so the budget is per
/// pixel up to `target_bpp`, capped by the per-stream rate (`cap_bytes`) for large windows.
/// Both shrink together under loss (`bpp` below `target_bpp`). A refine frame ignores the cap:
/// it is sent once, after the window went idle.
pub fn budget(width: u32, height: u32, bpp: f64, target_bpp: f64, cap_bytes: usize, refine: bool) -> usize {
    let floor = frame_bytes(width, height, MIN_BPP).max(4096);
    if refine {
        return frame_bytes(width, height, REFINE_BPP.max(target_bpp)).max(floor);
    }
    let scale = if target_bpp > 0.0 { (bpp / target_bpp).clamp(0.0, 1.0) } else { 1.0 };
    frame_bytes(width, height, bpp).min((cap_bytes as f64 * scale) as usize).max(floor)
}

#[derive(Debug, Clone)]
pub struct Rate {
    target: f64,
    current: f64,
    last_cut: Option<Instant>,
    last_update: Option<Instant>,
}

impl Rate {
    pub fn new(target_bpp: f64) -> Self {
        let target = target_bpp.max(MIN_BPP);
        Self { target, current: target, last_cut: None, last_update: None }
    }

    pub fn bpp(&self) -> f64 {
        self.current
    }

    /// Feeds one loss report (datagrams received and lost since the previous report).
    pub fn report(&mut self, received: u64, lost: u64, now: Instant) {
        let elapsed = self.last_update.map_or(Duration::ZERO, |t| now.saturating_duration_since(t));
        self.last_update = Some(now);
        let total = received + lost;
        let lossy = total > 0 && lost as f64 / total as f64 > LOSS_THRESHOLD;
        if lossy {
            if self.last_cut.is_none_or(|t| now.saturating_duration_since(t) >= HOLD) {
                self.current = (self.current * CUT).max(MIN_BPP);
                self.last_cut = Some(now);
            }
        } else {
            self.current = (self.current + self.target * RECOVER_PER_SEC * elapsed.as_secs_f64()).min(self.target);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cuts_on_loss_once_per_hold_and_recovers_slowly() {
        let t = Instant::now();
        let mut r = Rate::new(1.2);
        r.report(1000, 0, t);
        assert_eq!(r.bpp(), 1.2);
        r.report(900, 100, t + Duration::from_millis(100));
        assert!((r.bpp() - 0.9).abs() < 1e-9);
        // Within the hold: no second cut.
        r.report(900, 100, t + Duration::from_millis(300));
        assert!((r.bpp() - 0.9).abs() < 1e-9);
        // Two clean seconds regain 10% of the target.
        r.report(1000, 0, t + Duration::from_millis(2300));
        assert!((r.bpp() - 1.02).abs() < 1e-9);
        // Never above the target.
        r.report(1000, 0, t + Duration::from_secs(60));
        assert_eq!(r.bpp(), 1.2);
    }

    #[test]
    fn floor_and_budget() {
        let t = Instant::now();
        let mut r = Rate::new(0.1);
        assert_eq!(r.bpp(), MIN_BPP);
        for i in 0..20 {
            r.report(0, 100, t + HOLD * i);
        }
        assert_eq!(r.bpp(), MIN_BPP);
        assert_eq!(frame_bytes(1920, 1080, 1.2), 311_040);
    }

    #[test]
    #[allow(clippy::single_range_in_vec_init)]
    fn pieces_split_only_oversized_packets() {
        let p = pieces(&[0..100, 100..350, 350..351], 100).unwrap();
        assert_eq!(
            p,
            vec![(0..100, false), (100..200, true), (200..300, true), (300..350, false), (350..351, false)]
        );
        assert!(pieces(&[0..1000], 100).is_none());
        assert!(pieces(&[0..10], 0).is_none());
    }

    #[test]
    fn small_loss_is_noise() {
        let t = Instant::now();
        let mut r = Rate::new(1.2);
        r.report(990, 10, t);
        assert_eq!(r.bpp(), 1.2);
    }

    #[test]
    fn budget_favors_small_windows_and_caps_large_ones() {
        let cap = (200e6 / 8.0 / 60.0) as usize;
        // Calculator sized: the full per-pixel budget.
        assert_eq!(budget(230, 408, 3.0, 3.0, cap, false), frame_bytes(230, 408, 3.0));
        // 1080p: held to the rate cap while it changes, about 1.6 bpp.
        assert_eq!(budget(1920, 1080, 3.0, 3.0, cap, false), cap);
        // Loss halves both.
        assert_eq!(budget(1920, 1080, 1.5, 3.0, cap, false), cap / 2);
        // Refinement ignores the cap.
        assert_eq!(budget(1920, 1080, 1.5, 3.0, cap, true), frame_bytes(1920, 1080, REFINE_BPP));
        // Tiny windows never drop below the floor.
        assert_eq!(budget(40, 40, 0.5, 3.0, cap, false), 4096);
    }

    #[test]
    fn dirty_rects_clip_and_reject_outliers() {
        let r = dirty_to_surface(&[[10.2, 5.0, 20.0, 3.5], [0.0, 0.0, 0.0, 4.0], [1910.0, 1070.0, 11.0, 11.0]], 1920, 1080).unwrap();
        assert_eq!(r, vec![Rect { x0: 10, y0: 5, x1: 31, y1: 9 }, Rect { x0: 1910, y0: 1070, x1: 1920, y1: 1080 }]);
        assert!(dirty_to_surface(&[[0.0, 0.0, 3840.0, 2160.0]], 1920, 1080).is_none());
        assert_eq!(dirty_to_surface(&[], 10, 10), Some(vec![]));
    }

    #[test]
    fn changed_tiles_finds_exactly_the_edited_tiles() {
        let (w, h) = (100usize, 40usize);
        let (ys, uvs) = (112usize, 112usize);
        let prev_y = vec![10u8; ys * h];
        let prev_uv = vec![128u8; uvs * h / 2];
        let mut cur_y = prev_y.clone();
        let mut cur_uv = prev_uv.clone();
        // Two luma pixels in tiles (1, 0) and (2, 0), and one chroma sample in tile (5, 2).
        cur_y[3 * ys + 20] = 11;
        cur_y[5 * ys + 40] = 11;
        cur_uv[17 * uvs + 82] = 1;
        let frame = |y: &'static [u8], uv: &'static [u8]| Nv12 { y, y_stride: ys, uv, uv_stride: uvs };
        let (py, puv, cy, cuv) = (prev_y.leak(), prev_uv.leak(), cur_y.leak(), cur_uv.leak());
        let r = changed_tiles(&frame(py, puv), &frame(cy, cuv), w, h);
        assert_eq!(r, vec![Rect { x0: 16, y0: 0, x1: 48, y1: 16 }, Rect { x0: 80, y0: 32, x1: 96, y1: 40 }]);
        assert!(changed_tiles(&frame(py, puv), &frame(py, puv), w, h).is_empty());
        assert!((coverage(&r, 100, 40) - (32.0 * 16.0 + 16.0 * 8.0) / 4000.0).abs() < 1e-9);
    }
}
