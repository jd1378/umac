//! Keyframe gating. After any loss the decoder's references are broken, so inter frames are
//! dropped until a keyframe arrives, and keyframe requests are rate limited.

use std::time::{Duration, Instant};

const REQUEST_INTERVAL: Duration = Duration::from_millis(250);

pub struct KeyframeGate {
    waiting: bool,
    last_request: Option<Instant>,
}

impl KeyframeGate {
    /// Starts waiting. The host sends a keyframe first after subscribe, so the first explicit
    /// request only goes out if that one does not arrive within the request interval.
    pub fn new(now: Instant) -> Self {
        Self { waiting: true, last_request: Some(now) }
    }

    /// Whether a frame may go to the decoder.
    pub fn accept(&mut self, keyframe: bool) -> bool {
        if keyframe {
            self.waiting = false;
        }
        !self.waiting
    }

    /// Records a loss. Returns true when a keyframe request should be sent now.
    pub fn on_loss(&mut self, now: Instant) -> bool {
        self.waiting = true;
        self.poll(now)
    }

    /// Returns true when still waiting and the previous request is old enough to repeat.
    pub fn poll(&mut self, now: Instant) -> bool {
        if !self.waiting || self.last_request.is_some_and(|t| now.duration_since(t) < REQUEST_INTERVAL) {
            return false;
        }
        self.last_request = Some(now);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drops_until_keyframe() {
        let mut g = KeyframeGate::new(Instant::now());
        assert!(!g.accept(false));
        assert!(g.accept(true));
        assert!(g.accept(false));
        let t = Instant::now();
        g.on_loss(t);
        assert!(!g.accept(false));
        assert!(g.accept(true));
    }

    #[test]
    fn rate_limits_requests() {
        let t = Instant::now();
        let mut g = KeyframeGate::new(t - REQUEST_INTERVAL);
        assert!(g.accept(true));
        assert!(g.on_loss(t));
        assert!(!g.on_loss(t + Duration::from_millis(10)));
        assert!(!g.poll(t + Duration::from_millis(100)));
        assert!(g.poll(t + REQUEST_INTERVAL));
        g.accept(true);
        assert!(!g.poll(t + 4 * REQUEST_INTERVAL));
    }

    #[test]
    fn initial_wait_requests_after_grace() {
        let t = Instant::now();
        let mut g = KeyframeGate::new(t);
        assert!(!g.poll(t));
        assert!(g.poll(t + REQUEST_INTERVAL));
    }
}
