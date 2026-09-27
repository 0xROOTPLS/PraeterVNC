//! Per-address failed-login limit.
use parking_lot::Mutex;
use std::collections::HashMap;
use std::net::IpAddr;
use std::time::{Duration, Instant};

struct Entry {
    fails: u32,
    last: Instant,
    until: Option<Instant>,
}

#[derive(Default)]
pub struct Guard {
    m: Mutex<HashMap<IpAddr, Entry>>,
}

impl Guard {
    /// Remaining block time for `ip`.
    pub fn blocked(&self, ip: IpAddr) -> Option<Duration> {
        let now = Instant::now();
        self.m.lock().get(&ip)?.until.filter(|&u| u > now).map(|u| u - now)
    }

    /// Records a failure; true if `ip` is now blocked.
    pub fn failed(&self, ip: IpAddr, max: u32, block: Duration) -> bool {
        let now = Instant::now();
        let mut m = self.m.lock();
        if m.len() > 4096 {
            m.retain(|_, e| now - e.last < block.max(Duration::from_secs(600)));
        }
        let e = m.entry(ip).or_insert(Entry { fails: 0, last: now, until: None });
        // Failures older than the block window are forgotten.
        if e.until.is_some_and(|u| u <= now) || now - e.last > block.max(Duration::from_secs(60)) {
            e.fails = 0;
            e.until = None;
        }
        e.fails += 1;
        e.last = now;
        if max > 0 && e.fails >= max && !block.is_zero() {
            e.until = Some(now + block);
            return true;
        }
        false
    }

    /// Blocked addresses and time left.
    pub fn list(&self) -> Vec<(IpAddr, Duration)> {
        let now = Instant::now();
        self.m.lock().iter().filter_map(|(ip, e)| e.until.filter(|&u| u > now).map(|u| (*ip, u - now))).collect()
    }

    pub fn ok(&self, ip: IpAddr) {
        self.m.lock().remove(&ip);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_after_max() {
        let g = Guard::default();
        let ip: IpAddr = "10.0.0.1".parse().unwrap();
        let b = Duration::from_secs(60);
        assert!(!g.failed(ip, 3, b));
        assert!(!g.failed(ip, 3, b));
        assert!(g.blocked(ip).is_none());
        assert!(g.failed(ip, 3, b));
        assert!(g.blocked(ip).is_some());
        assert!(g.blocked("10.0.0.2".parse().unwrap()).is_none());
        let u: IpAddr = "10.0.0.3".parse().unwrap();
        for _ in 0..10 {
            assert!(!g.failed(u, 0, b));
        }
        assert_eq!(g.list().iter().map(|b| b.0).collect::<Vec<_>>(), [ip]);
        g.ok(ip);
        assert!(g.blocked(ip).is_none());
        assert!(g.list().is_empty());
    }
}
