//! Per-API-key rate limiting using a sliding window.
//!
//! Configure via env: MEMORIA_RATE_LIMIT_AUTH_KEYS=1000,60 (max_requests,window_seconds)
//! Default: 1000 requests per 60 seconds.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::warn;

const SHARD_COUNT: usize = 64;

/// Hard upper bounds to prevent misconfiguration from exhausting memory.
const MAX_REQUESTS_UPPER: u32 = 100_000;
const WINDOW_SECS_UPPER: u64 = 86_400; // 1 day

#[derive(Default)]
struct Bucket {
    entries: Vec<(u64, u32)>,
    last_seen: u64,
}

#[derive(Default)]
struct BucketShard {
    buckets: HashMap<Arc<str>, Bucket>,
    next_cleanup_at: u64,
}

#[derive(Clone)]
pub struct RateLimiter {
    max_requests: u32,
    window_secs: u64,
    inactivity_ttl_secs: u64,
    cleanup_interval_secs: u64,
    shards: Arc<Vec<Mutex<BucketShard>>>,
}

impl RateLimiter {
    pub fn new(max_requests: u32, window_secs: u64) -> Self {
        let max_requests = max_requests.clamp(1, MAX_REQUESTS_UPPER);
        let window_secs = window_secs.clamp(1, WINDOW_SECS_UPPER);
        let inactivity_ttl_secs = window_secs.saturating_mul(2);
        let cleanup_interval_secs = window_secs;

        Self {
            max_requests,
            window_secs,
            inactivity_ttl_secs,
            cleanup_interval_secs,
            shards: Arc::new(
                (0..SHARD_COUNT)
                    .map(|_| Mutex::new(BucketShard::default()))
                    .collect(),
            ),
        }
    }

    fn shard_index(&self, key_hash: &str) -> usize {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        key_hash.hash(&mut hasher);
        (hasher.finish() as usize) % self.shards.len()
    }

    fn cleanup_if_due(&self, shard: &mut BucketShard, now: u64) {
        if now < shard.next_cleanup_at {
            return;
        }

        shard.buckets.retain(|_, bucket| {
            bucket
                .entries
                .retain(|(ts, _)| now.saturating_sub(*ts) < self.window_secs);
            !bucket.entries.is_empty()
                || now.saturating_sub(bucket.last_seen) < self.inactivity_ttl_secs
        });
        shard.next_cleanup_at = now.saturating_add(self.cleanup_interval_secs);
    }

    /// Check if request is allowed. Returns true if within limit.
    pub async fn allow(&self, key_hash: &str) -> bool {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let mut shard = self.shards[self.shard_index(key_hash)].lock().await;
        self.cleanup_if_due(&mut shard, now);

        let key: Arc<str> = Arc::from(key_hash);
        let bucket = shard.buckets.entry(key).or_default();
        bucket
            .entries
            .retain(|(ts, _)| now.saturating_sub(*ts) < self.window_secs);
        bucket.last_seen = now;

        let count: u32 = bucket.entries.iter().map(|(_, c)| *c).sum();
        if count >= self.max_requests {
            warn!(
                key_prefix = &key_hash[..key_hash.len().min(8)],
                count,
                limit = self.max_requests,
                window_secs = self.window_secs,
                "Rate limit exceeded"
            );
            return false;
        }

        match bucket.entries.last_mut() {
            Some((ts, c)) if *ts == now => {
                if let Some(next) = c.checked_add(1) {
                    *c = next;
                } else {
                    bucket.entries.push((now, 1));
                }
            }
            _ => bucket.entries.push((now, 1)),
        }
        true
    }

    #[cfg(test)]
    fn shard_index_for_test(&self, key_hash: &str) -> usize {
        self.shard_index(key_hash)
    }

    #[cfg(test)]
    async fn tracked_key_count(&self) -> usize {
        let mut total = 0;
        for shard in self.shards.iter() {
            total += shard.lock().await.buckets.len();
        }
        total
    }
}

pub fn from_env() -> RateLimiter {
    let config = std::env::var("MEMORIA_RATE_LIMIT_AUTH_KEYS")
        .unwrap_or_else(|_| "1000,60".to_string());

    let parts: Vec<&str> = config.split(',').collect();
    let raw_max: u32 = parts
        .first()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1000);
    let raw_window: u64 = parts
        .get(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(60);

    if raw_max > MAX_REQUESTS_UPPER || raw_window > WINDOW_SECS_UPPER {
        warn!(
            raw_max_requests = raw_max,
            raw_window_secs = raw_window,
            clamped_max = raw_max.min(MAX_REQUESTS_UPPER),
            clamped_window = raw_window.min(WINDOW_SECS_UPPER),
            "Rate limit config exceeds upper bounds, clamping"
        );
    }

    RateLimiter::new(raw_max, raw_window)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_rate_limit_allows_within_limit() {
        let limiter = RateLimiter::new(3, 60);
        assert!(limiter.allow("key1").await);
        assert!(limiter.allow("key1").await);
        assert!(limiter.allow("key1").await);
        assert!(!limiter.allow("key1").await);
    }

    #[tokio::test]
    async fn test_rate_limit_per_key() {
        let limiter = RateLimiter::new(2, 60);
        assert!(limiter.allow("key1").await);
        assert!(limiter.allow("key2").await);
        assert!(limiter.allow("key1").await);
        assert!(!limiter.allow("key1").await);
        assert!(limiter.allow("key2").await);
        assert!(!limiter.allow("key2").await);
    }

    #[tokio::test]
    async fn test_rate_limit_resets_after_window() {
        let limiter = RateLimiter::new(1, 1);
        assert!(limiter.allow("key1").await);
        assert!(!limiter.allow("key1").await);
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        assert!(limiter.allow("key1").await);
    }

    #[test]
    fn test_rate_limit_shard_distribution() {
        let limiter = RateLimiter::new(100, 60);
        let keys: Vec<String> = (0..1000).map(|i| format!("key-hash-{i:064x}")).collect();

        let mut counts = vec![0usize; SHARD_COUNT];
        for k in &keys {
            counts[limiter.shard_index(k)] += 1;
        }

        // Each shard should get roughly 1000/64 ≈ 15 keys.
        // Allow generous slack (3x mean) to avoid flakiness.
        let mean = 1000 / SHARD_COUNT;
        for (i, &c) in counts.iter().enumerate() {
            assert!(c <= mean * 3, "shard {i} has {c} keys, expected ≤ {}", mean * 3);
        }
        // All shards should be non-empty with 1000 keys across 64 shards.
        assert!(counts.iter().all(|&c| c > 0), "some shards are empty");
    }

    #[tokio::test]
    async fn test_rate_limit_evicts_inactive_keys() {
        let limiter = RateLimiter::new(1, 1);
        assert!(limiter.allow("stale-key").await);

        let stale_shard = limiter.shard_index_for_test("stale-key");
        let cleanup_key = (0..10_000)
            .map(|i| format!("cleanup-key-{i}"))
            .find(|candidate| {
                limiter.shard_index_for_test(candidate) == stale_shard && candidate != "stale-key"
            })
            .expect("find cleanup key in same shard");

        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        assert!(limiter.allow(&cleanup_key).await);
        assert_eq!(limiter.tracked_key_count().await, 1);
    }
}
