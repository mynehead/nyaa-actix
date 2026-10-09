//! Upstream's COUNT_CACHE_SIZE / COUNT_CACHE_DURATION: listing totals are counted once and
//! reused for a few seconds, so paging through a big listing doesn't recount it every time.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Debug)]
pub struct CountCache {
    size: usize,
    duration: Duration,
    /// count, last used, expires
    entries: Mutex<HashMap<String, (i64, Instant, Instant)>>,
}

impl CountCache {
    /// None when `duration_secs` is 0, which turns the cache off as upstream.
    pub fn new(size: usize, duration_secs: u64) -> Option<CountCache> {
        (duration_secs > 0 && size > 0).then(|| CountCache {
            size,
            duration: Duration::from_secs(duration_secs),
            entries: Mutex::default(),
        })
    }

    pub fn from_env() -> Option<std::sync::Arc<CountCache>> {
        let num = |key: &str, default: u64| std::env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default);
        CountCache::new(num("COUNT_CACHE_SIZE", 256) as usize, num("COUNT_CACHE_DURATION", 30)).map(Into::into)
    }

    /// The cached count for `key`, or `count()`'s result, which is then cached.
    pub fn get_or<E>(&self, key: String, count: impl FnOnce() -> Result<i64, E>) -> Result<i64, E> {
        let now = Instant::now();
        {
            let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
            match entries.get_mut(&key) {
                Some((value, used, expires)) if *expires > now => {
                    *used = now;
                    return Ok(*value);
                }
                Some(_) => {
                    entries.remove(&key);
                }
                None => {}
            }
        }
        let value = count()?;
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        if entries.len() >= self.size && !entries.contains_key(&key) {
            // Drop the expired entries, or else the least recently used one
            entries.retain(|_, (_, _, expires)| *expires > now);
            if entries.len() >= self.size {
                if let Some(oldest) = entries.iter().min_by_key(|(_, (_, used, _))| *used).map(|(k, _)| k.clone()) {
                    entries.remove(&oldest);
                }
            }
        }
        entries.insert(key, (value, now, now + self.duration));
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caches_until_full_then_drops_the_least_recently_used() {
        assert!(CountCache::new(256, 0).is_none());
        let cache = CountCache::new(2, 60).unwrap();
        let ok = |n| move || Ok::<_, ()>(n);
        assert_eq!(cache.get_or("a".into(), ok(1)), Ok(1));
        assert_eq!(cache.get_or("a".into(), ok(9)), Ok(1));
        assert_eq!(cache.get_or("b".into(), ok(2)), Ok(2));
        assert_eq!(cache.get_or("a".into(), ok(9)), Ok(1));
        // "b" is now the least recently used
        assert_eq!(cache.get_or("c".into(), ok(3)), Ok(3));
        assert_eq!(cache.get_or("b".into(), ok(4)), Ok(4));
        assert_eq!(cache.get_or("x".into(), || Err::<i64, _>("db down")), Err("db down"));
    }
}
