//! In-memory attempt counters for login and registration. Each key (an IP or an account
//! name) gets `max` attempts per fixed window. State lives in the process, so it resets
//! on restart and isn't shared between servers, which is enough to stop online password
//! guessing against one instance.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Past this many keys, expired windows are dropped on the next hit so the map can't grow
/// without bound.
const PRUNE_AT: usize = 10_000;

pub struct Throttle {
    max: u32,
    window: Duration,
    hits: Mutex<HashMap<String, (Instant, u32)>>,
}

impl Throttle {
    pub fn new(max: u32, window: Duration) -> Self {
        Throttle { max, window, hits: Mutex::new(HashMap::new()) }
    }

    /// Whether `key` has used up its attempts in the current window.
    #[cfg(test)]
    pub fn is_blocked(&self, key: &str) -> bool {
        self.is_blocked_at(key, Instant::now())
    }

    /// Counts one attempt for `key`.
    pub fn hit(&self, key: &str) {
        self.hit_at(key, Instant::now())
    }

    /// Counts one attempt for `key` unless it has used up its attempts; false when it has.
    /// Checking and counting in one step, so requests sent at the same moment can't all
    /// get past the check before any of them is counted.
    pub fn try_hit(&self, key: &str) -> bool {
        self.try_hit_at(key, Instant::now())
    }

    /// Takes back one attempt counted by [`Throttle::try_hit`], as when it succeeded.
    pub fn undo(&self, key: &str) {
        if let Some(entry) = self.hits.lock().unwrap().get_mut(key) {
            entry.1 = entry.1.saturating_sub(1);
        }
    }

    /// Forgets `key`, as after a successful login.
    pub fn clear(&self, key: &str) {
        self.hits.lock().unwrap().remove(key);
    }

    #[cfg(test)]
    fn is_blocked_at(&self, key: &str, now: Instant) -> bool {
        match self.hits.lock().unwrap().get(key) {
            Some(&(start, count)) => now.duration_since(start) < self.window && count >= self.max,
            None => false,
        }
    }

    fn hit_at(&self, key: &str, now: Instant) {
        self.count_at(key, now, u32::MAX);
    }

    fn try_hit_at(&self, key: &str, now: Instant) -> bool {
        self.count_at(key, now, self.max)
    }

    /// Counts one attempt for `key` if it has fewer than `limit` in the current window.
    fn count_at(&self, key: &str, now: Instant, limit: u32) -> bool {
        let mut hits = self.hits.lock().unwrap();
        if hits.len() >= PRUNE_AT {
            hits.retain(|_, (start, _)| now.duration_since(*start) < self.window);
        }
        let entry = hits.entry(key.to_string()).or_insert((now, 0));
        if now.duration_since(entry.0) >= self.window {
            *entry = (now, 0);
        }
        if entry.1 >= limit {
            return false;
        }
        entry.1 += 1;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_after_max_hits_until_the_window_ends() {
        let t = Throttle::new(3, Duration::from_secs(60));
        let start = Instant::now();
        for _ in 0..2 {
            t.hit_at("a", start);
        }
        assert!(!t.is_blocked_at("a", start));
        t.hit_at("a", start);
        assert!(t.is_blocked_at("a", start));
        assert!(!t.is_blocked_at("b", start), "keys are counted separately");
        let later = start + Duration::from_secs(61);
        assert!(!t.is_blocked_at("a", later));
        t.hit_at("a", later);
        assert!(!t.is_blocked_at("a", later), "a new window starts from zero");
    }

    #[test]
    fn try_hit_counts_until_blocked_and_undo_gives_one_back() {
        let t = Throttle::new(2, Duration::from_secs(60));
        let start = Instant::now();
        assert!(t.try_hit_at("a", start));
        assert!(t.try_hit_at("a", start));
        assert!(!t.try_hit_at("a", start), "the third attempt is refused");
        assert!(t.is_blocked_at("a", start));
        t.undo("a");
        assert!(t.try_hit_at("a", start));
        assert!(t.try_hit_at("a", start + Duration::from_secs(61)), "a new window starts from zero");
    }

    #[test]
    fn clear_resets_a_key() {
        let t = Throttle::new(1, Duration::from_secs(60));
        t.hit("a");
        assert!(t.is_blocked("a"));
        t.clear("a");
        assert!(!t.is_blocked("a"));
    }

    #[test]
    fn expired_keys_are_pruned() {
        let t = Throttle::new(1, Duration::from_secs(60));
        let start = Instant::now();
        for i in 0..PRUNE_AT {
            t.hit_at(&i.to_string(), start);
        }
        t.hit_at("new", start + Duration::from_secs(61));
        assert_eq!(t.hits.lock().unwrap().len(), 1);
    }
}
