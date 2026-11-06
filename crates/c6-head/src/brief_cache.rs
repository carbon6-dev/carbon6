//! Small, positive-only request caches. A generation prevents an in-flight
//! database read from repopulating a cache after a revocation committed.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const TTL: Duration = Duration::from_secs(2);
const CAPACITY: usize = 512;

pub(crate) struct BriefCache<V> {
    inner: Mutex<Entries<V>>,
}

struct Entries<V> {
    generation: u64,
    values: HashMap<String, (Instant, V)>,
}

impl<V> Default for BriefCache<V> {
    fn default() -> Self {
        Self { inner: Mutex::new(Entries { generation: 0, values: HashMap::new() }) }
    }
}

impl<V: Clone> BriefCache<V> {
    /// The generation must be captured before starting the database read.
    pub(crate) fn get(&self, key: &str) -> (u64, Option<V>) {
        let mut inner = self.inner.lock().unwrap();
        let value = match inner.values.get(key) {
            Some((at, value)) if at.elapsed() < TTL => Some(value.clone()),
            _ => {
                inner.values.remove(key);
                None
            }
        };
        (inner.generation, value)
    }

    pub(crate) fn insert(&self, generation: u64, key: String, value: V) {
        let mut inner = self.inner.lock().unwrap();
        if inner.generation != generation {
            return;
        }
        if inner.values.len() >= CAPACITY {
            inner.values.retain(|_, (at, _)| at.elapsed() < TTL);
            if inner.values.len() >= CAPACITY {
                inner.values.clear();
            }
        }
        inner.values.insert(key, (Instant::now(), value));
    }

    /// Call after the durable mutation, before returning success. Clear all
    /// entries so a token's id/prefix or realm need not be duplicated here.
    pub(crate) fn invalidate(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.generation = inner.generation.wrapping_add(1);
        inner.values.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expiry_capacity_and_in_flight_revocation() {
        let cache = BriefCache::default();
        let (generation, value) = cache.get("token");
        assert_eq!(value, None);
        cache.insert(generation, "token".into(), true);
        assert_eq!(cache.get("token").1, Some(true));
        cache.inner.lock().unwrap().values.get_mut("token").unwrap().0 = Instant::now() - TTL;
        assert_eq!(cache.get("token").1, None);
        cache.invalidate();
        cache.insert(generation, "token".into(), true);
        assert_eq!(cache.get("token").1, None, "a read started before revocation cannot refill the cache");
        let generation = cache.get("").0;
        for i in 0..=CAPACITY {
            cache.insert(generation, i.to_string(), true);
        }
        assert!(cache.inner.lock().unwrap().values.len() <= CAPACITY);
    }
}
