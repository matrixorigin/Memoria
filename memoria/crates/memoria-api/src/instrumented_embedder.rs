//! Instrumented embedding provider wrapper.
//!
//! Wraps any [`EmbeddingProvider`] and transparently records latency and error
//! metrics to the global [`crate::metrics::Registry`].  Apply this wrapper
//! once at startup (in `build_embedder`) so all embedding calls are covered
//! regardless of which backend is in use.

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use memoria_core::{interfaces::EmbeddingProvider, MemoriaError};

use crate::metrics::registry;

/// Wraps any [`EmbeddingProvider`] and records call latency + error counts.
///
/// The `provider` label (e.g. `"http"`, `"mock"`, `"round-robin"`) is set
/// once at construction time and attached to every metric observation.
pub struct InstrumentedEmbedder {
    inner: Arc<dyn EmbeddingProvider>,
    provider: String,
}

impl InstrumentedEmbedder {
    pub fn new(inner: Arc<dyn EmbeddingProvider>, provider: impl Into<String>) -> Self {
        Self {
            inner,
            provider: provider.into(),
        }
    }
}

#[async_trait]
impl EmbeddingProvider for InstrumentedEmbedder {
    async fn embed(&self, text: &str) -> Result<Vec<f32>, MemoriaError> {
        let key = format!("{}|single", self.provider);
        let t = Instant::now();
        let result = self.inner.embed(text).await;
        let elapsed = t.elapsed().as_secs_f64();
        let reg = registry();
        reg.embedding.duration_seconds.observe(&key, elapsed);
        if result.is_err() {
            reg.embedding.errors_total.inc(&key);
        }
        result
    }

    async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, MemoriaError> {
        let key = format!("{}|batch", self.provider);
        let t = Instant::now();
        let result = self.inner.embed_batch(texts).await;
        let elapsed = t.elapsed().as_secs_f64();
        let reg = registry();
        reg.embedding.duration_seconds.observe(&key, elapsed);
        if result.is_err() {
            reg.embedding.errors_total.inc(&key);
        }
        result
    }

    fn dimension(&self) -> usize {
        self.inner.dimension()
    }
}
