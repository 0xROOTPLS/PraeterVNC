//! Bottleneck model for pacing: rate from ack spacing of queued updates, propagation from
//! min RTT, predicted drain time of our bytes.
use std::collections::VecDeque;
use std::time::{Duration, Instant};

struct Out {
    t: Instant,
    bytes: f64,
    qwait: f64,
}

pub struct Pipe {
    out: VecDeque<Out>,
    /// (ack time, send time) of the last acked update.
    last: Option<(Instant, Instant)>,
    /// (time, bytes/ms) samples where acks spread wider than sends: link-limited.
    rs: VecDeque<(Instant, f64)>,
    /// (time, rtt - predicted queue wait, bytes).
    ps: VecDeque<(Instant, f64, f64)>,
    /// (time, bytes/rtt) of large updates: a lower bound on rate.
    lb: VecDeque<(Instant, f64)>,
    /// Bytes per ms; 0 while unknown.
    pub rate: f64,
    pub prop: f64,
    drain: Instant,
    sent: u64,
    /// Link time per update, ms (EMA).
    pub tx: f64,
}

const RATE_WIN: Duration = Duration::from_secs(3);
const PROP_WIN: Duration = Duration::from_secs(10);

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1e3
}

fn dur(ms: f64) -> Duration {
    Duration::from_secs_f64(ms.max(0.0) / 1e3)
}

impl Default for Pipe {
    fn default() -> Pipe {
        Pipe::new()
    }
}

impl Pipe {
    pub fn new() -> Pipe {
        Pipe { out: VecDeque::new(), last: None, rs: VecDeque::new(), ps: VecDeque::new(), lb: VecDeque::new(), rate: 0.0, prop: 0.0, drain: Instant::now(), sent: 0, tx: 0.0 }
    }

    pub fn inflight(&self) -> usize {
        self.out.len()
    }

    pub fn clear(&mut self) {
        self.out.clear();
        self.last = None;
    }

    pub fn on_send(&mut self, bytes: usize, now: Instant) {
        let mut qwait = 0.0;
        if self.rate > 0.0 {
            let start = self.drain.max(now);
            qwait = ms(start - now);
            let tx = bytes as f64 / self.rate;
            self.drain = start + dur(tx);
            self.tx = self.tx * 0.75 + tx * 0.25;
        }
        self.out.push_back(Out { t: now, bytes: bytes as f64, qwait });
        self.sent += 1;
        if self.out.len() > 64 {
            self.out.pop_front();
        }
    }

    /// Consumes the oldest outstanding update; returns its RTT in ms.
    pub fn on_ack(&mut self, now: Instant) -> Option<f64> {
        let o = self.out.pop_front()?;
        let rtt = ms(now - o.t);
        if let Some((la, lt)) = self.last {
            let (da, ds) = (ms(now - la), ms(o.t.saturating_duration_since(lt)));
            if o.bytes >= 8192.0 && da >= 2.0 && da > ds * 1.2 + 1.0 {
                self.rs.push_back((now, o.bytes / da));
            }
        }
        self.last = Some((now, o.t));
        if o.bytes >= 32768.0 {
            self.lb.push_back((now, o.bytes / rtt.max(0.1)));
        }
        while self.lb.front().is_some_and(|s| now - s.0 > RATE_WIN) && self.lb.len() > 1 {
            self.lb.pop_front();
        }
        while self.rs.front().is_some_and(|s| now - s.0 > RATE_WIN) {
            self.rs.pop_front();
        }
        self.rate = self.rs.iter().map(|s| s.1).fold(0.0, f64::max);
        if self.rate > 0.0 {
            self.rate = self.rate.max(self.lb.iter().map(|s| s.1).fold(0.0, f64::max));
        }
        self.ps.push_back((now, rtt - o.qwait, o.bytes));
        while self.ps.front().is_some_and(|s| now - s.0 > PROP_WIN) || self.ps.len() > 256 {
            self.ps.pop_front();
        }
        // Prefer propagation-dominated samples.
        let r = self.rate;
        let est = |s: &(Instant, f64, f64)| if r > 0.0 { s.1 - s.2 / r } else { s.1 };
        let good = self.ps.iter().filter(|s| r <= 0.0 || s.2 / r <= 0.5 * s.1).map(est).fold(f64::MAX, f64::min);
        self.prop = if good < f64::MAX { good } else { self.ps.iter().map(est).fold(f64::MAX, f64::min) }.max(0.0);
        if r > 0.0 {
            let mut d = now.checked_sub(dur(self.prop)).unwrap_or(now);
            for x in &self.out {
                d = d.max(x.t) + dur(x.bytes / r);
            }
            self.drain = d;
        }
        Some(rtt)
    }

    /// Best known rate, falling back to a lower bound; 0 if nothing is known.
    pub fn rate_or_bound(&self) -> f64 {
        if self.rate > 0.0 {
            self.rate
        } else {
            self.lb.iter().map(|s| s.1).fold(0.0, f64::max)
        }
    }

    /// Time until the next update may go, given `lead` ms to produce it. Every 8th update
    /// (4th while the estimate is stale) leaves early as a rate probe.
    pub fn wait(&self, lead: f64, now: Instant) -> Option<Duration> {
        if self.rate <= 0.0 || self.out.is_empty() {
            return None;
        }
        let stale = self.rs.back().is_none_or(|s| now - s.0 > Duration::from_millis(500));
        let gain = if self.sent % if stale { 4 } else { 8 } == 3 { 0.6 } else { 0.1 };
        let open = self.drain.checked_sub(dur(lead + gain * self.tx))?;
        (open > now).then(|| open - now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Link of `rate` bytes/ms and `prop` ms RTT; the sender offers `size`-byte updates
    /// whenever the pipe allows. Returns (rate estimate, mean queue wait ms, updates/s).
    fn sim(rate: f64, prop: f64, size: usize) -> (f64, f64, f64) {
        let t0 = Instant::now();
        let at = |ms: f64| t0 + dur(ms);
        let mut p = Pipe::new();
        let (mut now, mut free, mut sent) = (0.0f64, 0.0f64, 0);
        let mut acks: VecDeque<f64> = VecDeque::new();
        let (mut qsum, mut n) = (0.0, 0);
        while now < 5000.0 {
            while acks.front().is_some_and(|&a| a <= now) {
                p.on_ack(at(acks.pop_front().unwrap()));
            }
            let blocked = p.inflight() >= 32 || p.wait(1.0, at(now)).is_some();
            if !blocked {
                let start = now.max(free);
                if now > 2000.0 {
                    qsum += start - now;
                    n += 1;
                }
                free = start + size as f64 / rate;
                acks.push_back(free + prop);
                p.on_send(size, at(now));
                sent += 1;
            }
            now += 0.25;
        }
        let _ = sent;
        (p.rate, qsum / n.max(1) as f64, n as f64 / 3.0)
    }

    /// Small app-paced updates (typing) on a slow link must never be held back.
    #[test]
    fn app_limited_not_paced() {
        let t0 = Instant::now();
        let at = |ms: f64| t0 + dur(ms);
        let mut p = Pipe::new();
        let (rate, prop) = (625.0, 80.0);
        let mut free = 0.0f64;
        let mut acks: VecDeque<f64> = VecDeque::new();
        for i in 0..500 {
            let now = i as f64 * 10.0;
            while acks.front().is_some_and(|&a| a <= now) {
                p.on_ack(at(acks.pop_front().unwrap()));
            }
            assert!(p.wait(0.5, at(now)).is_none_or(|w| w < Duration::from_millis(2)), "held at {now}");
            let size = 1500 + (i * 7919 % 3000);
            free = now.max(free) + size as f64 / rate;
            acks.push_back(free + prop);
            p.on_send(size, at(now));
        }
    }

    #[test]
    fn converges_without_queue() {
        for (rate, prop, size) in [(2500.0, 40.0, 100_000), (625.0, 80.0, 40_000), (12_500.0, 20.0, 60_000)] {
            let (r, q, ups) = sim(rate, prop, size);
            let ideal = rate / size as f64 * 1000.0;
            eprintln!("rate {rate} prop {prop} size {size}: est {r:.0} queue {q:.2} ms {ups:.1} upd/s ideal {ideal:.1}");
            assert!((r / rate - 1.0).abs() < 0.1, "rate {r} vs {rate}");
            assert!(q < 0.2 * size as f64 / rate + 2.0, "queue {q} ms");
            assert!(ups > 0.8 * ideal, "{ups} upd/s vs ideal {ideal}");
        }
    }
}
