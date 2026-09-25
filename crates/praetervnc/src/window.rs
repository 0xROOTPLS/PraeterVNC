//! In-flight update window, TCP-Vegas style: queued ≈ W·(1 − baseRTT/sRTT) updates.
//! Grow by one per RTT while fewer than ALPHA updates queue at the bottleneck (client decode
//! or link), shrink by one above BETA.
use std::collections::VecDeque;
use std::time::{Duration, Instant};

pub struct Window {
    pub depth: u32,
    max: u32,
    hist: VecDeque<(Instant, f64)>,
    last_adjust: Instant,
    pub srtt: f64,
}

const HIST: Duration = Duration::from_secs(10);
const ALPHA: f64 = 1.0;
const BETA: f64 = 2.0;

impl Window {
    pub fn new(max: u32) -> Window {
        Window { depth: 1, max, hist: VecDeque::new(), last_adjust: Instant::now(), srtt: 0.0 }
    }

    pub fn base(&self) -> f64 {
        self.hist.iter().map(|h| h.1).fold(f64::MAX, f64::min)
    }

    pub fn sample(&mut self, rtt_ms: f64) {
        let now = Instant::now();
        self.srtt = if self.srtt == 0.0 { rtt_ms } else { self.srtt * 0.875 + rtt_ms * 0.125 };
        self.hist.push_back((now, rtt_ms));
        while self.hist.front().is_some_and(|h| now - h.0 > HIST) {
            self.hist.pop_front();
        }
        if now - self.last_adjust < Duration::from_secs_f64(self.srtt / 1e3) {
            return;
        }
        self.last_adjust = now;
        let queued = self.depth as f64 * (1.0 - self.base() / self.srtt.max(1e-3));
        if queued < ALPHA && self.depth < self.max {
            self.depth += 1;
        } else if queued > BETA && self.depth > 1 {
            self.depth -= 1;
        }
    }
}
