//! Per-API-key rate limiting using sliding window.
//!
//! Configure via env: MEMORIA_RATE_LIMIT_AUTH_KEYS=1000,60 (max_requests,window_seconds)
//! Default: 1000 requests per 60 seconds.

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

type BucketMap = HashMap<String, Vec<(u64, u32)>>;

#[derive(Clone)]
pub struct RateLimiter {
    max_requests: u32,
    window_secs: u64,
    // key_hash -> Vec<(timestamp_secs, count)>
    buckets: Arc<RwLock<BucketMap>>,
}

impl RateLimiter {
    pub fn new(max_requests: u32, window_secs: u64) -> Self {
        Self {
            max_requests,
            window_secs,
            buckets: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Check if request is allowed. Returns true if within limit.
    pub async fn allow(&self, key_hash: &str) -> bool {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let mut buckets = self.buckets.write().await;
        let bucket = buckets.entry(key_hash.to_string()).or_insert_with(Vec::new);

        // Remove old entries outside window
        bucket.retain(|(ts, _)| now - ts < self.window_secs);

        // Count requests in window
        let count: u32 = bucket.iter().map(|(_, c)| c).sum();

        if count < self.max_requests {
            // Add current request
            if let Some((_, c)) = bucket.last_mut() {
                if c.checked_add(1).is_some() {
                    *c += 1;
                } else {
                    bucket.push((now, 1));
                }
            } else {
                bucket.push((now, 1));
            }
            true
        } else {
            false
        }
    }
}

pub fn from_env() -> RateLimiter {
    let config = std::env::var("MEMORIA_RATE_LIMIT_AUTH_KEYS")
        .unwrap_or_else(|_| "1000,60".to_string());

    let parts: Vec<&str> = config.split(',').collect();
    let max_requests = parts
        .first()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1000);
    let window_secs = parts
        .get(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(60);

    RateLimiter::new(max_requests, window_secs)
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
}

