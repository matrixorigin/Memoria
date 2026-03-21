/// Local embedding via fastembed (ONNX Runtime).
/// Supports all-MiniLM-L6-v2 (384d) and BAAI/bge-m3 (1024d).
/// Enabled with `local-embedding` feature.

#[cfg(feature = "local-embedding")]
mod inner {
    use async_trait::async_trait;
    use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};
    use memoria_core::{interfaces::EmbeddingProvider, MemoriaError};
    use std::sync::Mutex;

    pub struct LocalEmbedder {
        model: Mutex<TextEmbedding>,
        dimension: usize,
    }

    impl LocalEmbedder {
        pub fn new(model_name: &str) -> Result<Self, MemoriaError> {
            let (model_enum, dim) = match model_name {
                "all-MiniLM-L6-v2" | "sentence-transformers/all-MiniLM-L6-v2" => {
                    (EmbeddingModel::AllMiniLML6V2, 384)
                }
                "BAAI/bge-m3" | "bge-m3" => (EmbeddingModel::BGEM3, 1024),
                "BAAI/bge-small-en-v1.5" | "bge-small-en-v1.5" => {
                    (EmbeddingModel::BGESmallENV15, 384)
                }
                "BAAI/bge-base-en-v1.5" | "bge-base-en-v1.5" => (EmbeddingModel::BGEBaseENV15, 384),
                "BAAI/bge-large-en-v1.5" | "bge-large-en-v1.5" => {
                    (EmbeddingModel::BGELargeENV15, 1024)
                }
                "all-MiniLM-L12-v2" | "sentence-transformers/all-MiniLM-L12-v2" => {
                    (EmbeddingModel::AllMiniLML12V2, 384)
                }
                _ => {
                    return Err(MemoriaError::Embedding(format!(
                        "Unsupported local model: {model_name}. Supported: all-MiniLM-L6-v2, BAAI/bge-m3, BAAI/bge-small-en-v1.5, BAAI/bge-base-en-v1.5, BAAI/bge-large-en-v1.5"
                    )));
                }
            };

            let opts = InitOptions::new(model_enum).with_show_download_progress(true);
            let model = TextEmbedding::try_new(opts)
                .map_err(|e| MemoriaError::Embedding(format!("Failed to load local model: {e}")))?;

            Ok(Self {
                model: Mutex::new(model),
                dimension: dim,
            })
        }
    }

    #[async_trait]
    impl EmbeddingProvider for LocalEmbedder {
        async fn embed(&self, text: &str) -> Result<Vec<f32>, MemoriaError> {
            let model = self
                .model
                .lock()
                .map_err(|e| MemoriaError::Embedding(format!("Lock poisoned: {e}")))?;
            let results = model
                .embed(vec![text], None)
                .map_err(|e| MemoriaError::Embedding(e.to_string()))?;
            results
                .into_iter()
                .next()
                .ok_or_else(|| MemoriaError::Embedding("Empty embedding result".into()))
        }

        async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, MemoriaError> {
            if texts.is_empty() {
                return Ok(vec![]);
            }
            let model = self
                .model
                .lock()
                .map_err(|e| MemoriaError::Embedding(format!("Lock poisoned: {e}")))?;
            let refs: Vec<&str> = texts.iter().map(|s| s.as_str()).collect();
            model
                .embed(refs, None)
                .map_err(|e| MemoriaError::Embedding(e.to_string()))
        }

        fn dimension(&self) -> usize {
            self.dimension
        }
    }
}

#[cfg(feature = "local-embedding")]
pub use inner::LocalEmbedder;
