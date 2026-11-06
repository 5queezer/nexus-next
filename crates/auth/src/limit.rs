//! In-memory rate limits. They reset on restart, which only hands an attacker a few free attempts.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Entries kept per limiter, so spraying source addresses cannot grow memory without bound.
const MAX_ENTRIES: usize = 4096;

pub(crate) const GLOBAL_KEY: &str = "global";

/// The limiter key for an address. An IPv6 client usually controls a whole /64, so that is the key.
pub(crate) fn address_key(ip: IpAddr) -> String {
    match ip.to_canonical() {
        IpAddr::V4(v4) => v4.to_string(),
        IpAddr::V6(v6) => {
            let mut octets = v6.octets();
            octets[8..].fill(0);
            std::net::Ipv6Addr::from(octets).to_string()
        }
    }
}

fn retry_secs(wait: Duration) -> u64 {
    wait.as_secs_f64().ceil().max(1.0) as u64
}

/// A port of `checkRateLimit`: `max` requests per key per fixed window.
pub(crate) struct FixedWindow {
    max: u32,
    window: Duration,
    entries: Mutex<HashMap<String, (u32, Instant)>>,
}

impl FixedWindow {
    pub(crate) fn new(max: u32, window: Duration) -> Self {
        Self {
            max,
            window,
            entries: Mutex::default(),
        }
    }

    /// Counts a request. `Err` holds the seconds until the key may try again.
    pub(crate) fn check(&self, key: &str, now: Instant) -> Result<(), u64> {
        let mut entries = self.entries.lock().unwrap();
        if entries.len() >= MAX_ENTRIES && !entries.contains_key(key) {
            entries.retain(|_, (_, reset)| now < *reset);
            if entries.len() >= MAX_ENTRIES {
                entries.clear();
            }
        }
        let entry = entries
            .entry(key.to_owned())
            .or_insert((0, now + self.window));
        if now >= entry.1 {
            *entry = (0, now + self.window);
        }
        if entry.0 >= self.max {
            return Err(retry_secs(entry.1 - now));
        }
        entry.0 += 1;
        Ok(())
    }
}

#[derive(Clone, Copy)]
pub(crate) struct BackoffPolicy {
    /// Attempts before any delay applies.
    pub(crate) free: u32,
    /// The first delay; each later attempt doubles it.
    pub(crate) base: Duration,
    pub(crate) max: Duration,
    /// A key with no attempts for this long starts over. Longer than `max`, or the cap never bites.
    pub(crate) forget_after: Duration,
}

struct BackoffEntry {
    attempts: u32,
    next_allowed: Instant,
    last: Instant,
}

/// Exponential backoff on failed attempts. An attempt is counted before it is verified, in one
/// atomic step across all its keys, so parallel guesses cannot all slip through before the first
/// failure lands. Success clears the source and refunds its attempt from the global count, which it
/// must not clear: the owner logging in during an attack would hand the attacker fresh free attempts.
pub(crate) struct Backoff {
    source: BackoffPolicy,
    global: BackoffPolicy,
    entries: Mutex<HashMap<String, BackoffEntry>>,
}

impl Backoff {
    /// `global` applies to `GLOBAL_KEY`, `source` to every other key.
    pub(crate) fn new(source: BackoffPolicy, global: BackoffPolicy) -> Self {
        Self {
            source,
            global,
            entries: Mutex::default(),
        }
    }

    fn policy(&self, key: &str) -> BackoffPolicy {
        if key == GLOBAL_KEY {
            self.global
        } else {
            self.source
        }
    }

    /// Reserves an attempt on all `keys`, or none of them if any is waiting.
    pub(crate) fn reserve(&self, keys: &[&str], now: Instant) -> Result<(), u64> {
        let mut entries = self.entries.lock().unwrap();
        for key in keys {
            let forget = self.policy(key).forget_after;
            if entries
                .get(*key)
                .is_some_and(|e| now.duration_since(e.last) >= forget)
            {
                entries.remove(*key);
            }
        }
        let wait = keys
            .iter()
            .filter_map(|key| entries.get(*key))
            .filter(|e| now < e.next_allowed)
            .map(|e| e.next_allowed - now)
            .max();
        if let Some(wait) = wait {
            return Err(retry_secs(wait));
        }
        if entries.len() + keys.len() > MAX_ENTRIES {
            let oldest = entries
                .iter()
                .filter(|(key, _)| key.as_str() != GLOBAL_KEY)
                .min_by_key(|(_, e)| e.last)
                .map(|(key, _)| key.clone());
            if let Some(oldest) = oldest {
                entries.remove(&oldest);
            }
        }
        for key in keys {
            let policy = self.policy(key);
            let entry = entries.entry((*key).to_owned()).or_insert(BackoffEntry {
                attempts: 0,
                next_allowed: now,
                last: now,
            });
            entry.attempts = entry.attempts.saturating_add(1);
            entry.last = now;
            if entry.attempts > policy.free {
                let doublings = entry.attempts - policy.free - 1;
                let delay = policy
                    .base
                    .checked_mul(2u32.saturating_pow(doublings))
                    .map_or(policy.max, |delay| delay.min(policy.max));
                entry.next_allowed = now + delay;
            }
        }
        Ok(())
    }

    pub(crate) fn succeed(&self, source: &str) {
        let mut entries = self.entries.lock().unwrap();
        entries.remove(source);
        if let Some(global) = entries.get_mut(GLOBAL_KEY) {
            global.attempts = global.attempts.saturating_sub(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEC: Duration = Duration::from_secs(1);

    #[test]
    fn fixed_window_allows_max_then_resets() {
        let limit = FixedWindow::new(2, SEC * 60);
        let start = Instant::now();
        assert_eq!(limit.check("a", start), Ok(()));
        assert_eq!(limit.check("a", start), Ok(()));
        assert_eq!(limit.check("a", start), Err(60));
        assert_eq!(limit.check("b", start), Ok(()));
        assert_eq!(limit.check("a", start + SEC * 60), Ok(()));
    }

    #[test]
    fn fixed_window_stays_bounded_when_keys_are_sprayed() {
        let limit = FixedWindow::new(1, SEC * 60);
        let now = Instant::now();
        for i in 0..(MAX_ENTRIES + 10) {
            limit.check(&i.to_string(), now).unwrap();
        }
        assert!(limit.entries.lock().unwrap().len() <= MAX_ENTRIES);
    }

    fn backoff() -> Backoff {
        let ip = BackoffPolicy {
            free: 2,
            base: SEC * 2,
            max: SEC * 8,
            forget_after: SEC * 100,
        };
        let global = BackoffPolicy {
            free: 4,
            base: SEC,
            max: SEC * 4,
            forget_after: SEC * 100,
        };
        Backoff::new(ip, global)
    }

    #[test]
    fn backoff_doubles_up_to_the_cap_after_free_attempts() {
        let limit = backoff();
        let mut now = Instant::now();
        assert!(limit.reserve(&["a"], now).is_ok());
        assert!(limit.reserve(&["a"], now).is_ok());
        for delay in [2u32, 4, 8, 8, 8] {
            assert!(limit.reserve(&["a"], now).is_ok());
            assert_eq!(limit.reserve(&["a"], now), Err(u64::from(delay)));
            assert_eq!(limit.reserve(&["a"], now + SEC * delay - SEC), Err(1));
            now += SEC * delay;
        }
    }

    #[test]
    fn a_blocked_attempt_counts_on_no_key() {
        let limit = backoff();
        let now = Instant::now();
        for _ in 0..3 {
            assert!(limit.reserve(&["a", GLOBAL_KEY], now).is_ok());
        }
        assert_eq!(limit.reserve(&["a", GLOBAL_KEY], now), Err(2));
        let entries = limit.entries.lock().unwrap();
        assert_eq!(entries["a"].attempts, 3);
        assert_eq!(entries[GLOBAL_KEY].attempts, 3);
    }

    #[test]
    fn global_key_blocks_other_sources() {
        let limit = backoff();
        let now = Instant::now();
        for source in ["a", "b", "c", "d", "e"] {
            assert!(limit.reserve(&[source, GLOBAL_KEY], now).is_ok());
        }
        assert_eq!(limit.reserve(&["f", GLOBAL_KEY], now), Err(1));
    }

    #[test]
    fn success_and_forgetting_restart_the_source_count() {
        let limit = backoff();
        let now = Instant::now();
        for _ in 0..3 {
            limit.reserve(&["a"], now).unwrap();
        }
        assert!(limit.reserve(&["a"], now).is_err());
        limit.succeed("a");
        assert!(limit.reserve(&["a"], now).is_ok());

        for _ in 0..2 {
            limit.reserve(&["a"], now).unwrap();
        }
        assert!(limit.reserve(&["a"], now).is_err());
        assert!(limit.reserve(&["a"], now + SEC * 100).is_ok());
    }

    #[test]
    fn success_refunds_one_global_attempt_and_keeps_the_rest() {
        let limit = backoff();
        let now = Instant::now();
        for source in ["a", "b", "c"] {
            limit.reserve(&[source, GLOBAL_KEY], now).unwrap();
        }
        limit.succeed("c");
        assert_eq!(limit.entries.lock().unwrap()[GLOBAL_KEY].attempts, 2);
    }

    #[test]
    fn backoff_stays_bounded_and_keeps_the_global_key() {
        let limit = backoff();
        let now = Instant::now();
        limit.reserve(&[GLOBAL_KEY], now).unwrap();
        for i in 0..(MAX_ENTRIES + 10) {
            limit
                .reserve(&[&i.to_string()], now + SEC * (i as u32))
                .unwrap();
        }
        let entries = limit.entries.lock().unwrap();
        assert!(entries.len() <= MAX_ENTRIES);
        assert!(entries.contains_key(GLOBAL_KEY));
    }

    #[test]
    fn huge_attempt_counts_do_not_overflow_the_delay() {
        let limit = backoff();
        let now = Instant::now();
        limit.reserve(&["a"], now).unwrap();
        limit.entries.lock().unwrap().get_mut("a").unwrap().attempts = u32::MAX - 1;
        limit.reserve(&["a"], now).unwrap();
        assert_eq!(limit.reserve(&["a"], now), Err(8));
    }

    #[test]
    fn address_keys_group_ipv6_by_64_and_unmap_ipv4() {
        let key = |s: &str| address_key(s.parse().unwrap());
        assert_eq!(key("192.0.2.1"), "192.0.2.1");
        assert_eq!(key("::ffff:192.0.2.1"), "192.0.2.1");
        assert_eq!(key("2001:db8:1:2:aaaa::1"), key("2001:db8:1:2:bbbb::2"));
        assert_ne!(key("2001:db8:1:2::1"), key("2001:db8:1:3::1"));
    }
}
