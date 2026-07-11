use std::collections::HashMap;
use std::net::IpAddr;

/// Per-IP login lockout: after 8 consecutive bad passwords, lock that IP for 60s
/// (LOGIN_MAX_FAILS=8, LOGIN_LOCK_MS=60_000). `now_ms` is injected for deterministic tests.
#[derive(Default)]
pub struct LoginThrottle {
    fails: HashMap<IpAddr, (u32, u64)>, // ip -> (consecutive fails, locked_until_ms)
}
const MAX_FAILS: u32 = 8;
const LOCK_MS: u64 = 60_000;
impl LoginThrottle {
    /// true = this IP may attempt a login now.
    pub fn check(&self, ip: IpAddr, now_ms: u64) -> bool {
        match self.fails.get(&ip) {
            Some(&(_, until)) => until <= now_ms,
            None => true,
        }
    }
    pub fn record_fail(&mut self, ip: IpAddr, now_ms: u64) {
        if self.fails.len() > 1000 {
            self.fails.clear();
        } // bound the map
        let e = self.fails.entry(ip).or_insert((0, 0));
        e.0 += 1;
        if e.0 >= MAX_FAILS {
            *e = (0, now_ms + LOCK_MS);
        }
    }
    pub fn record_success(&mut self, ip: IpAddr) {
        self.fails.remove(&ip);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn ip() -> std::net::IpAddr {
        "1.2.3.4".parse().unwrap()
    }

    #[test]
    fn locks_after_8_fails_then_clears_on_success() {
        let mut t = LoginThrottle::default();
        assert!(t.check(ip(), 0)); // allowed
        for _ in 0..8 {
            t.record_fail(ip(), 0);
        }
        assert!(!t.check(ip(), 0)); // locked
        assert!(t.check(ip(), 60_001)); // lock expired after 60s
        t.record_success(ip());
        for _ in 0..7 {
            t.record_fail(ip(), 0);
        }
        assert!(t.check(ip(), 0)); // <8 since success → still allowed
    }
}
