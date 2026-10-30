//! PyroWave receive side: joins datagram pieces into encoder packets, validates them into
//! frames, decides when a frame is ready, and counts loss for `VideoFeedback`. No keyframes:
//! a frame is decoded with whatever arrived and missing blocks blur locally.

use std::time::{Duration, Instant};

use umac_proto::video::{PyroHeader, pyro_flags};
use umac_pyrowave_decode::{Assembler, Chroma, Frame, Layout, default_frame_cap};

/// How long after its latest datagram a frame waits for missing ones before it is decoded
/// without them. Counted from the latest, not the first: a 1080p frame takes several
/// milliseconds to arrive over Wi-Fi.
pub const FRAME_TIMEOUT: Duration = Duration::from_millis(6);
/// How often loss is reported to the host.
pub const FEEDBACK_EVERY: Duration = Duration::from_millis(500);
/// Datagrams one frame may announce; more is treated as garbage.
const MAX_DATAGRAMS: u16 = 16 * 1024;

/// A validated frame for the GPU decoder.
pub struct PyroPicture {
    pub layout: Layout,
    pub frame: Frame,
}

pub struct Ready {
    pub picture: PyroPicture,
    pub capture_us: u32,
    pub first_datagram: Instant,
}

struct Current {
    seq: u32,
    count: u16,
    capture_us: u32,
    first: Instant,
    last: Instant,
    /// Datagram payloads by index, with their CONTINUED flag.
    slots: Vec<Option<(bool, Vec<u8>)>>,
    received: u16,
    /// Next slot not yet turned into packets, and the pieces gathered so far.
    next: usize,
    joining: Vec<u8>,
}

pub struct PyroRx {
    asm: Option<Assembler>,
    key: Option<(u16, u16, bool)>,
    current: Option<Current>,
    last_seq: Option<u32>,
    received: u32,
    lost: u32,
    last_feedback: Instant,
    pub rejected: u32,
}

fn newer(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) > 0
}

impl PyroRx {
    pub fn new(now: Instant) -> Self {
        Self { asm: None, key: None, current: None, last_seq: None, received: 0, lost: 0, last_feedback: now, rejected: 0 }
    }

    /// One datagram. Returns a frame when one became ready (the previous one, superseded by
    /// this datagram's newer frame, or this one once all its datagrams arrived).
    pub fn push(&mut self, h: &PyroHeader, payload: &[u8], now: Instant) -> Option<Ready> {
        if h.packet_count > MAX_DATAGRAMS {
            return None;
        }
        let key = (h.width, h.height, h.flags & pyro_flags::CHROMA_444 != 0);
        if self.key != Some(key) {
            let chroma = if key.2 { Chroma::Yuv444 } else { Chroma::Yuv420 };
            let layout = Layout::new(h.width as u32, h.height as u32, chroma).ok()?;
            let cap = default_frame_cap(&layout);
            self.asm = Some(Assembler::new(layout, cap));
            self.key = Some(key);
            self.current = None;
        }
        if self.last_seq.is_some_and(|s| !newer(h.frame_seq, s)) && self.current.as_ref().is_none_or(|c| c.seq != h.frame_seq) {
            return None;
        }
        let mut ready = None;
        if self.current.as_ref().is_some_and(|c| c.seq != h.frame_seq) {
            if newer(h.frame_seq, self.current.as_ref().unwrap().seq) {
                ready = self.finish();
            } else {
                return None;
            }
        }
        let c = self.current.get_or_insert_with(|| Current {
            seq: h.frame_seq,
            count: h.packet_count,
            capture_us: h.capture_us,
            first: now,
            last: now,
            slots: vec![None; h.packet_count as usize],
            received: 0,
            next: 0,
            joining: Vec::new(),
        });
        if h.packet_count != c.count || c.slots[h.packet_index as usize].is_some() {
            return ready;
        }
        c.slots[h.packet_index as usize] = Some((h.flags & pyro_flags::CONTINUED != 0, payload.to_vec()));
        c.received += 1;
        c.last = now;
        self.feed(false);
        if self.current.as_ref().is_some_and(|c| c.received == c.count) {
            // Two ready frames at once is not possible: a newer datagram retired the old one.
            return ready.or_else(|| self.finish());
        }
        ready
    }

    /// Hands joined packets to the assembler in index order. `flush` skips missing slots,
    /// dropping the packet they belonged to.
    fn feed(&mut self, flush: bool) {
        let (Some(c), Some(asm)) = (self.current.as_mut(), self.asm.as_mut()) else { return };
        let mut lost_packet = false;
        while c.next < c.slots.len() {
            match c.slots[c.next].take() {
                Some((continued, piece)) => {
                    c.next += 1;
                    if lost_packet {
                        lost_packet = continued;
                        continue;
                    }
                    c.joining.extend_from_slice(&piece);
                    if !continued {
                        let packet = std::mem::take(&mut c.joining);
                        if asm.push_packet(&packet).is_err() {
                            self.rejected += 1;
                        }
                    }
                }
                None if flush => {
                    // The rest of this packet (up to its last piece) is useless now.
                    c.next += 1;
                    c.joining.clear();
                    lost_packet = true;
                }
                None => break,
            }
        }
    }

    fn finish(&mut self) -> Option<Ready> {
        self.feed(true);
        let c = self.current.take()?;
        self.last_seq = Some(c.seq);
        self.received += c.received as u32;
        self.lost += (c.count - c.received) as u32;
        let asm = self.asm.as_mut()?;
        let frame = asm.take_frame(true);
        asm.reset();
        Some(Ready { picture: PyroPicture { layout: asm.layout().clone(), frame: frame? }, capture_us: c.capture_us, first_datagram: c.first })
    }

    /// A frame that waited too long for missing datagrams, decoded as is.
    pub fn poll(&mut self, now: Instant) -> Option<Ready> {
        if self.current.as_ref().is_some_and(|c| now.duration_since(c.last) >= FRAME_TIMEOUT) { self.finish() } else { None }
    }

    /// Datagrams received and lost since the last report, every [`FEEDBACK_EVERY`].
    pub fn feedback(&mut self, now: Instant) -> Option<(u32, u32)> {
        if now.duration_since(self.last_feedback) < FEEDBACK_EVERY {
            return None;
        }
        self.last_feedback = now;
        let report = (self.received, self.lost);
        (self.received, self.lost) = (0, 0);
        (report != (0, 0)).then_some(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(seq: u32, index: u16, count: u16, flags: u8) -> PyroHeader {
        PyroHeader { flags, window: 1, frame_seq: seq, capture_us: 7, packet_index: index, packet_count: count, width: 256, height: 144 }
    }

    /// The golden 256x144 4:2:0 frame's encoder packets.
    fn packets() -> Vec<Vec<u8>> {
        let pkts = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/../pyrowave-decode/testdata/256x144.420.b13824.pkts")).unwrap();
        let u32_at = |i: usize| u32::from_le_bytes(pkts[i..i + 4].try_into().unwrap()) as usize;
        let (count, mut at, mut out) = (u32_at(0), 4, Vec::new());
        for _ in 0..count {
            let size = u32_at(at);
            out.push(pkts[at + 4..at + 4 + size].to_vec());
            at += 4 + size;
        }
        out
    }

    /// Splits every packet into two datagrams (CONTINUED on the first half).
    fn datagrams(seq: u32) -> Vec<(PyroHeader, Vec<u8>)> {
        let pieces: Vec<(bool, Vec<u8>)> =
            packets().into_iter().flat_map(|p| [(true, p[..p.len() / 2].to_vec()), (false, p[p.len() / 2..].to_vec())]).collect();
        let n = pieces.len() as u16;
        pieces
            .into_iter()
            .enumerate()
            .map(|(i, (cont, d))| (header(seq, i as u16, n, if cont { pyro_flags::CONTINUED } else { 0 }), d))
            .collect()
    }

    #[test]
    fn joins_pieces_and_completes() {
        let mut rx = PyroRx::new(Instant::now());
        let d = datagrams(5);
        let mut ready = None;
        for (h, p) in &d {
            ready = ready.or(rx.push(h, p, Instant::now()));
        }
        let r = ready.expect("complete frame");
        assert!(r.picture.frame.is_complete());
        assert_eq!(rx.rejected, 0);
        assert_eq!(rx.feedback(Instant::now() + FEEDBACK_EVERY), Some((d.len() as u32, 0)));
    }

    #[test]
    fn loss_drops_one_packet_and_times_out() {
        let t = Instant::now();
        let mut rx = PyroRx::new(t);
        let d = datagrams(1);
        // Lose the first piece of packet 2: packet 2 is lost, the rest decodes.
        for (h, p) in d.iter().filter(|(h, _)| h.packet_index != 4) {
            assert!(rx.push(h, p, t).is_none());
        }
        let r = rx.poll(t + FRAME_TIMEOUT).expect("timed out frame");
        assert!(!r.picture.frame.is_complete());
        assert!(r.picture.frame.received > 0);
        assert_eq!(rx.rejected, 0);
        assert_eq!(rx.feedback(t + FEEDBACK_EVERY), Some((d.len() as u32 - 1, 1)));
    }

    #[test]
    fn newer_frame_retires_old_and_late_datagrams_are_dropped() {
        let t = Instant::now();
        let mut rx = PyroRx::new(t);
        let old = datagrams(1);
        let new = datagrams(2);
        rx.push(&old[0].0, &old[0].1, t);
        let r = rx.push(&new[0].0, &new[0].1, t);
        // The old frame had only half a packet: nothing decodable, so nothing ready.
        assert!(r.is_none());
        assert!(rx.push(&old[1].0, &old[1].1, t).is_none());
        let mut ready = None;
        for (h, p) in &new[1..] {
            ready = ready.or(rx.push(h, p, t));
        }
        assert!(ready.unwrap().picture.frame.is_complete());
    }
}
