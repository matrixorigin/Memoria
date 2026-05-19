use async_trait::async_trait;
use memoria_core::{interfaces::EmbeddingProvider, MemoriaError};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

/// Mock embedder for benchmarking. Returns deterministic vectors based on content hash.
pub struct MockEmbedder {
    dim: usize,
}

impl MockEmbedder {
    pub fn new(dim: usize) -> Self {
        Self { dim }
    }
}

#[async_trait]
impl EmbeddingProvider for MockEmbedder {
    async fn embed(&self, text: &str) -> Result<Vec<f32>, MemoriaError> {
        let mut hasher = DefaultHasher::new();
        text.hash(&mut hasher);
        let seed = hasher.finish();
        // Generate deterministic pseudo-random unit vector from seed
        let mut v: Vec<f32> = (0..self.dim)
            .map(|i| {
                let x = ((seed.wrapping_mul(6364136223846793005).wrapping_add(i as u64)) as f32)
                    / u64::MAX as f32;
                x * 2.0 - 1.0
            })
            .collect();
        // Normalize to unit vector
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 0.0 {
            v.iter_mut().for_each(|x| *x /= norm);
        }
        Ok(v)
    }

    fn dimension(&self) -> usize {
        self.dim
    }
}
