use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use memoria_core::{interfaces::EmbeddingProvider, MemoriaError};

use crate::HttpEmbedder;

/// A load-balancing embedding provider that distributes requests across
/// multiple OpenAI-compatible backends using round-robin selection.
///
/// All backends must serve the **same model**. When a backend returns an
/// error (e.g. HTTP 429 rate-limit or transient 5xx), the next backend in
/// the ring is tried automatically, so a single rate-limited key does not
/// block the entire system.
///
/// # Construction
/// - [`RoundRobinEmbedder::new`] — production path, creates [`HttpEmbedder`]
///   instances from `(url, api_key)` pairs.
/// - [`RoundRobinEmbedder::from_providers`] — inject arbitrary
///   [`EmbeddingProvider`] implementations; used in tests.
pub struct RoundRobinEmbedder {
    backends: Vec<Arc<dyn EmbeddingProvider>>,
    /// Monotonically-increasing counter; the starting backend index for each
    /// call is `counter % backends.len()`.
    counter: AtomicUsize,
}

impl RoundRobinEmbedder {
    /// Production constructor: builds [`HttpEmbedder`] backends from a list of
    /// `(base_url, api_key)` pairs. All endpoints must serve the same `model`
    /// at the given `dimension`. Panics if `endpoints` is empty.
    pub fn new(
        endpoints: Vec<(String, String)>,
        model: impl Into<String> + Clone,
        dimension: usize,
    ) -> Self {
        assert!(
            !endpoints.is_empty(),
            "RoundRobinEmbedder requires at least one endpoint"
        );
        let model_str = model.into();
        let backends: Vec<Arc<dyn EmbeddingProvider>> = endpoints
            .into_iter()
            .map(|(url, key)| {
                Arc::new(HttpEmbedder::new(url, key, model_str.clone(), dimension))
                    as Arc<dyn EmbeddingProvider>
            })
            .collect();
        Self {
            backends,
            counter: AtomicUsize::new(0),
        }
    }

    /// Injection constructor: wraps arbitrary [`EmbeddingProvider`] instances.
    /// Intended for testing and custom integrations. Panics if `backends` is empty.
    pub fn from_providers(backends: Vec<Arc<dyn EmbeddingProvider>>) -> Self {
        assert!(
            !backends.is_empty(),
            "RoundRobinEmbedder requires at least one backend"
        );
        Self {
            backends,
            counter: AtomicUsize::new(0),
        }
    }

    /// Returns the index of the next starting backend using atomic round-robin.
    fn next_start(&self) -> usize {
        self.counter.fetch_add(1, Ordering::Relaxed) % self.backends.len()
    }

    /// Returns `true` for HTTP client errors (4xx except 429) that will fail
    /// on every backend — retrying is pointless (e.g. 401 bad key, 403 forbidden).
    /// `HttpEmbedder` formats these as `"HTTP 4xx: …"`.
    fn is_non_retryable(err: &MemoriaError) -> bool {
        match err {
            MemoriaError::Embedding(msg) => {
                msg.starts_with("HTTP 4") && !msg.starts_with("HTTP 429")
            }
            _ => false,
        }
    }
}

#[async_trait]
impl EmbeddingProvider for RoundRobinEmbedder {
    async fn embed(&self, text: &str) -> Result<Vec<f32>, MemoriaError> {
        let n = self.backends.len();
        let start = self.next_start();
        let mut last_err = MemoriaError::Embedding("no backends".into());
        for i in 0..n {
            let idx = (start + i) % n;
            match self.backends[idx].embed(text).await {
                Ok(v) => return Ok(v),
                Err(e) => {
                    if Self::is_non_retryable(&e) {
                        tracing::warn!(
                            backend = idx,
                            error = %e,
                            "embedding backend returned non-retryable error, aborting failover"
                        );
                        return Err(e);
                    }
                    if i < n - 1 {
                        tracing::warn!(
                            backend = idx,
                            error = %e,
                            "embedding backend failed, rotating to next"
                        );
                    }
                    last_err = e;
                }
            }
        }
        Err(last_err)
    }

    async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, MemoriaError> {
        if texts.is_empty() {
            return Ok(vec![]);
        }
        let n = self.backends.len();
        let start = self.next_start();
        let mut last_err = MemoriaError::Embedding("no backends".into());
        for i in 0..n {
            let idx = (start + i) % n;
            match self.backends[idx].embed_batch(texts).await {
                Ok(v) => return Ok(v),
                Err(e) => {
                    if Self::is_non_retryable(&e) {
                        tracing::warn!(
                            backend = idx,
                            error = %e,
                            "embedding backend returned non-retryable error on batch, aborting failover"
                        );
                        return Err(e);
                    }
                    if i < n - 1 {
                        tracing::warn!(
                            backend = idx,
                            error = %e,
                            "embedding backend failed on batch, rotating to next"
                        );
                    }
                    last_err = e;
                }
            }
        }
        Err(last_err)
    }

    fn dimension(&self) -> usize {
        self.backends[0].dimension()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use memoria_core::MemoriaError;

    // ── Mock backend ──────────────────────────────────────────────────────────

    const DIM: usize = 4;

    /// A fake [`EmbeddingProvider`] that records which backend index was invoked
    /// into a shared `call_log`. Optionally always returns an error to simulate
    /// rate-limiting or server failures.
    struct MockProvider {
        id: usize,
        call_log: Arc<Mutex<Vec<usize>>>,
        fail_msg: Option<String>,
    }

    impl MockProvider {
        fn ok(id: usize, call_log: Arc<Mutex<Vec<usize>>>) -> Arc<Self> {
            Arc::new(Self { id, call_log, fail_msg: None })
        }

        /// Retryable failure (e.g. rate-limit, server error).
        fn fail(id: usize, call_log: Arc<Mutex<Vec<usize>>>) -> Arc<Self> {
            Arc::new(Self {
                id,
                call_log,
                fail_msg: Some(format!("backend {} simulated rate limit", id)),
            })
        }

        /// Non-retryable failure (e.g. 401 auth error) — should abort failover.
        fn fail_auth(id: usize, call_log: Arc<Mutex<Vec<usize>>>) -> Arc<Self> {
            Arc::new(Self {
                id,
                call_log,
                fail_msg: Some(format!("HTTP 401: invalid api key on backend {}", id)),
            })
        }
    }

    #[async_trait]
    impl EmbeddingProvider for MockProvider {
        async fn embed(&self, _text: &str) -> Result<Vec<f32>, MemoriaError> {
            self.call_log.lock().unwrap().push(self.id);
            if let Some(msg) = &self.fail_msg {
                return Err(MemoriaError::Embedding(msg.clone()));
            }
            Ok(vec![self.id as f32; DIM])
        }

        async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, MemoriaError> {
            self.call_log.lock().unwrap().push(self.id);
            if let Some(msg) = &self.fail_msg {
                return Err(MemoriaError::Embedding(msg.clone()));
            }
            Ok(texts.iter().map(|_| vec![self.id as f32; DIM]).collect())
        }

        fn dimension(&self) -> usize {
            DIM
        }
    }

    fn new_log() -> Arc<Mutex<Vec<usize>>> {
        Arc::new(Mutex::new(vec![]))
    }

    // ── embed() tests ─────────────────────────────────────────────────────────

    #[tokio::test]
    async fn round_robin_cycles_through_all_backends() {
        let log = new_log();
        let rr = RoundRobinEmbedder::from_providers(vec![
            MockProvider::ok(0, log.clone()),
            MockProvider::ok(1, log.clone()),
            MockProvider::ok(2, log.clone()),
        ]);

        for _ in 0..6 {
            rr.embed("text").await.unwrap();
        }
        // Two full cycles: 0,1,2 then 0,1,2
        assert_eq!(*log.lock().unwrap(), vec![0, 1, 2, 0, 1, 2]);
    }

    #[tokio::test]
    async fn failover_skips_failing_backend() {
        let log = new_log();
        // Backend 0 always fails; backend 1 succeeds.
        let rr = RoundRobinEmbedder::from_providers(vec![
            MockProvider::fail(0, log.clone()),
            MockProvider::ok(1, log.clone()),
        ]);

        let result = rr.embed("hello").await.unwrap();
        // Result vector is identified by backend id = 1
        assert_eq!(result, vec![1.0_f32; DIM]);
        // Both were attempted in order: 0 failed, 1 succeeded
        assert_eq!(*log.lock().unwrap(), vec![0, 1]);
    }

    #[tokio::test]
    async fn failover_wraps_around_ring() {
        let log = new_log();
        // Backend 0 ok, backend 1 fails.
        let rr = RoundRobinEmbedder::from_providers(vec![
            MockProvider::ok(0, log.clone()),
            MockProvider::fail(1, log.clone()),
        ]);

        // First call starts at 0 → succeeds immediately.
        rr.embed("a").await.unwrap();
        log.lock().unwrap().clear();

        // Second call starts at 1 → fails, wraps back to 0 → succeeds.
        let result = rr.embed("b").await.unwrap();
        assert_eq!(result, vec![0.0_f32; DIM]);
        assert_eq!(*log.lock().unwrap(), vec![1, 0]);
    }

    #[tokio::test]
    async fn all_backends_fail_returns_error() {
        let log = new_log();
        let rr = RoundRobinEmbedder::from_providers(vec![
            MockProvider::fail(0, log.clone()),
            MockProvider::fail(1, log.clone()),
        ]);

        let err = rr.embed("text").await.unwrap_err();
        assert!(matches!(err, MemoriaError::Embedding(_)));
        // Every backend was tried exactly once.
        assert_eq!(log.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn single_backend_succeeds() {
        let log = new_log();
        let rr = RoundRobinEmbedder::from_providers(vec![MockProvider::ok(0, log.clone())]);

        let result = rr.embed("hi").await.unwrap();
        assert_eq!(result, vec![0.0_f32; DIM]);
    }

    #[tokio::test]
    async fn single_failing_backend_returns_error() {
        let log = new_log();
        let rr = RoundRobinEmbedder::from_providers(vec![MockProvider::fail(0, log.clone())]);

        assert!(rr.embed("hi").await.is_err());
        assert_eq!(log.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn dimension_reflects_backend() {
        let log = new_log();
        let rr = RoundRobinEmbedder::from_providers(vec![MockProvider::ok(0, log.clone())]);
        assert_eq!(rr.dimension(), DIM);
    }

    // ── embed_batch() tests ───────────────────────────────────────────────────

    #[tokio::test]
    async fn batch_round_robin_cycles_backends() {
        let log = new_log();
        let rr = RoundRobinEmbedder::from_providers(vec![
            MockProvider::ok(0, log.clone()),
            MockProvider::ok(1, log.clone()),
        ]);

        let texts = vec!["a".to_string(), "b".to_string()];
        rr.embed_batch(&texts).await.unwrap();
        rr.embed_batch(&texts).await.unwrap();
        assert_eq!(*log.lock().unwrap(), vec![0, 1]);
    }

    #[tokio::test]
    async fn batch_failover_uses_next_backend() {
        let log = new_log();
        let rr = RoundRobinEmbedder::from_providers(vec![
            MockProvider::fail(0, log.clone()),
            MockProvider::ok(1, log.clone()),
        ]);

        let texts = vec!["x".to_string()];
        let result = rr.embed_batch(&texts).await.unwrap();
        assert_eq!(result, vec![vec![1.0_f32; DIM]]);
        assert_eq!(*log.lock().unwrap(), vec![0, 1]);
    }

    #[tokio::test]
    async fn batch_all_fail_returns_error() {
        let log = new_log();
        let rr = RoundRobinEmbedder::from_providers(vec![
            MockProvider::fail(0, log.clone()),
            MockProvider::fail(1, log.clone()),
        ]);

        assert!(rr.embed_batch(&["x".to_string()]).await.is_err());
        assert_eq!(log.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn empty_batch_returns_empty_without_calling_backends() {
        let log = new_log();
        let rr = RoundRobinEmbedder::from_providers(vec![MockProvider::ok(0, log.clone())]);

        let result = rr.embed_batch(&[]).await.unwrap();
        assert!(result.is_empty());
        // No backend should be called for an empty input.
        assert!(log.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn batch_result_contains_embedding_per_text() {
        let log = new_log();
        let rr = RoundRobinEmbedder::from_providers(vec![MockProvider::ok(7, log.clone())]);

        let texts = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let result = rr.embed_batch(&texts).await.unwrap();
        assert_eq!(result.len(), 3);
        assert!(result.iter().all(|v| *v == vec![7.0_f32; DIM]));
    }

    // ── additional coverage (from code review) ────────────────────────────────

    #[tokio::test]
    async fn failover_skips_middle_failing_backend_in_three_backend_ring() {
        // Pattern: [ok(0), fail(1), ok(2)] — round-robin starts at 1.
        // Expected: tries 1 (fails) → 2 (succeeds); backend 0 never touched.
        let log = new_log();
        let rr = RoundRobinEmbedder::from_providers(vec![
            MockProvider::ok(0, log.clone()),
            MockProvider::fail(1, log.clone()),
            MockProvider::ok(2, log.clone()),
        ]);

        // Advance counter so the first call starts at backend 1.
        rr.embed("prime").await.unwrap(); // starts at 0, succeeds immediately
        log.lock().unwrap().clear();

        let result = rr.embed("next").await.unwrap(); // starts at 1, fails → 2
        assert_eq!(result, vec![2.0_f32; DIM]);
        // Only backends 1 and 2 were tried; 0 was not reached.
        assert_eq!(*log.lock().unwrap(), vec![1, 2]);
    }

    #[tokio::test]
    async fn embed_result_has_correct_dimension() {
        let log = new_log();
        let rr = RoundRobinEmbedder::from_providers(vec![
            MockProvider::ok(0, log.clone()),
            MockProvider::ok(1, log.clone()),
        ]);

        for _ in 0..4 {
            let v = rr.embed("check").await.unwrap();
            assert_eq!(v.len(), DIM, "embed() must return a vector of length DIM");
        }
    }

    #[tokio::test]
    async fn embed_batch_each_result_has_correct_dimension() {
        let log = new_log();
        let rr = RoundRobinEmbedder::from_providers(vec![MockProvider::ok(3, log.clone())]);

        let texts: Vec<String> = (0..5).map(|i| format!("text-{i}")).collect();
        let results = rr.embed_batch(&texts).await.unwrap();
        assert_eq!(results.len(), texts.len());
        for v in &results {
            assert_eq!(v.len(), DIM, "each batch embedding must be length DIM");
        }
    }

    // ── non-retryable error tests ─────────────────────────────────────────────

    #[tokio::test]
    async fn auth_error_aborts_failover_immediately() {
        let log = new_log();
        // Backend 0 returns 401; backend 1 is healthy but should never be tried.
        let rr = RoundRobinEmbedder::from_providers(vec![
            MockProvider::fail_auth(0, log.clone()),
            MockProvider::ok(1, log.clone()),
        ]);

        let err = rr.embed("text").await.unwrap_err();
        assert!(matches!(err, MemoriaError::Embedding(ref m) if m.contains("401")));
        // Only backend 0 was called — failover was aborted.
        assert_eq!(*log.lock().unwrap(), vec![0]);
    }

    #[tokio::test]
    async fn auth_error_aborts_batch_failover_immediately() {
        let log = new_log();
        let rr = RoundRobinEmbedder::from_providers(vec![
            MockProvider::fail_auth(0, log.clone()),
            MockProvider::ok(1, log.clone()),
        ]);

        let err = rr.embed_batch(&["x".to_string()]).await.unwrap_err();
        assert!(matches!(err, MemoriaError::Embedding(ref m) if m.contains("401")));
        assert_eq!(*log.lock().unwrap(), vec![0]);
    }

    #[tokio::test]
    async fn retryable_error_still_fails_over_past_auth_ok_backend() {
        let log = new_log();
        // Backend 0 has retryable error (rate limit), backend 1 is ok.
        // Should still failover normally.
        let rr = RoundRobinEmbedder::from_providers(vec![
            MockProvider::fail(0, log.clone()),
            MockProvider::ok(1, log.clone()),
        ]);

        let result = rr.embed("text").await.unwrap();
        assert_eq!(result, vec![1.0_f32; DIM]);
        assert_eq!(*log.lock().unwrap(), vec![0, 1]);
    }

    #[tokio::test]
    async fn is_non_retryable_classification() {
        // 401, 403, 400 → non-retryable
        assert!(RoundRobinEmbedder::is_non_retryable(
            &MemoriaError::Embedding("HTTP 401: bad key".into())
        ));
        assert!(RoundRobinEmbedder::is_non_retryable(
            &MemoriaError::Embedding("HTTP 403: forbidden".into())
        ));
        assert!(RoundRobinEmbedder::is_non_retryable(
            &MemoriaError::Embedding("HTTP 400: bad request".into())
        ));
        // 429 → retryable (rate limit)
        assert!(!RoundRobinEmbedder::is_non_retryable(
            &MemoriaError::Embedding("HTTP 429: too many requests".into())
        ));
        // 500, generic → retryable
        assert!(!RoundRobinEmbedder::is_non_retryable(
            &MemoriaError::Embedding("HTTP 500: server error".into())
        ));
        assert!(!RoundRobinEmbedder::is_non_retryable(
            &MemoriaError::Embedding("connection timeout".into())
        ));
        // Non-Embedding variant → retryable
        assert!(!RoundRobinEmbedder::is_non_retryable(
            &MemoriaError::Internal("something".into())
        ));
    }
}
