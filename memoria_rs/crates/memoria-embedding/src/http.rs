use async_trait::async_trait;
use memoria_core::{interfaces::EmbeddingProvider, MemoriaError};
use serde::{Deserialize, Serialize};

/// HTTP-based embedding client — OpenAI-compatible API.
/// Phase 2 implementation; Phase 4 will add Candle local embedding.
pub struct HttpEmbedder {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    model: String,
    dimension: usize,
}

#[derive(Serialize)]
struct EmbedRequest<'a> {
    input: &'a str,
    model: &'a str,
}

#[derive(Deserialize)]
struct EmbedResponse {
    data: Vec<EmbedData>,
}

#[derive(Deserialize)]
struct EmbedData {
    embedding: Vec<f32>,
}

impl HttpEmbedder {
    pub fn new(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
        dimension: usize,
    ) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: base_url.into(),
            api_key: api_key.into(),
            model: model.into(),
            dimension,
        }
    }
}

#[async_trait]
impl EmbeddingProvider for HttpEmbedder {
    async fn embed(&self, text: &str) -> Result<Vec<f32>, MemoriaError> {
        let url = format!("{}/embeddings", self.base_url.trim_end_matches('/'));
        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.api_key)
            .json(&EmbedRequest {
                input: text,
                model: &self.model,
            })
            .send()
            .await
            .map_err(|e| MemoriaError::Embedding(e.to_string()))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(MemoriaError::Embedding(format!(
                "HTTP {status}: {body}"
            )));
        }

        let data: EmbedResponse = resp
            .json()
            .await
            .map_err(|e| MemoriaError::Embedding(e.to_string()))?;

        data.data
            .into_iter()
            .next()
            .map(|d| d.embedding)
            .ok_or_else(|| MemoriaError::Embedding("Empty embedding response".into()))
    }

    fn dimension(&self) -> usize {
        self.dimension
    }
}
