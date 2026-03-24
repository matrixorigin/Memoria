use async_trait::async_trait;
use memoria_core::{interfaces::EmbeddingProvider, MemoriaError};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_RETRIES: u32 = 2;
const DEFAULT_MAX_CONCURRENT: usize = 32;
const DEFAULT_SEMAPHORE_TIMEOUT_SECS: u64 = 5;

/// HTTP-based embedding client — OpenAI-compatible API.
pub struct HttpEmbedder {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    model: String,
    dimension: usize,
    /// Semaphore to limit concurrent embedding requests
    semaphore: Arc<Semaphore>,
    /// Timeout for acquiring a semaphore permit
    semaphore_timeout: Duration,
}

#[derive(Serialize)]
struct EmbedRequest<'a> {
    input: EmbedInput<'a>,
    model: &'a str,
}

#[derive(Serialize)]
#[serde(untagged)]
enum EmbedInput<'a> {
    Single(&'a str),
    Batch(&'a [String]),
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
        let max_concurrent: usize = std::env::var("EMBED_MAX_CONCURRENT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_MAX_CONCURRENT)
            .clamp(1, 256);
        let semaphore_timeout = Duration::from_secs(
            std::env::var("EMBED_SEMAPHORE_TIMEOUT_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(DEFAULT_SEMAPHORE_TIMEOUT_SECS)
                .clamp(1, 120),
        );
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            client,
            base_url: base_url.into(),
            api_key: api_key.into(),
            model: model.into(),
            dimension,
            semaphore: Arc::new(Semaphore::new(max_concurrent)),
            semaphore_timeout,
        }
    }
}

impl HttpEmbedder {
    /// Send an embedding request with retry on transient errors.
    async fn post_embed(&self, body: &EmbedRequest<'_>) -> Result<EmbedResponse, MemoriaError> {
        let _permit = tokio::time::timeout(self.semaphore_timeout, self.semaphore.acquire())
            .await
            .map_err(|_| MemoriaError::Embedding("embedding concurrency limit timeout".to_string()))?
            .map_err(|_| MemoriaError::Embedding("embedding semaphore closed".to_string()))?;
        let url = format!("{}/embeddings", self.base_url.trim_end_matches('/'));
        let mut last_err = String::new();
        for attempt in 0..=MAX_RETRIES {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_millis(200 * (1 << (attempt - 1)))).await;
            }
            let resp = match self
                .client
                .post(&url)
                .bearer_auth(&self.api_key)
                .json(body)
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    last_err = e.to_string();
                    continue;
                }
            };
            if resp.status().is_server_error()
                || resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS
            {
                last_err = format!("HTTP {}", resp.status());
                continue;
            }
            if !resp.status().is_success() {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                return Err(MemoriaError::Embedding(format!("HTTP {status}: {body}")));
            }
            return resp
                .json::<EmbedResponse>()
                .await
                .map_err(|e| MemoriaError::Embedding(e.to_string()));
        }
        Err(MemoriaError::Embedding(format!(
            "failed after {} retries: {last_err}",
            MAX_RETRIES + 1
        )))
    }
}

#[async_trait]
impl EmbeddingProvider for HttpEmbedder {
    async fn embed(&self, text: &str) -> Result<Vec<f32>, MemoriaError> {
        let data = self
            .post_embed(&EmbedRequest {
                input: EmbedInput::Single(text),
                model: &self.model,
            })
            .await?;
        data.data
            .into_iter()
            .next()
            .map(|d| d.embedding)
            .ok_or_else(|| MemoriaError::Embedding("Empty embedding response".into()))
    }

    async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, MemoriaError> {
        if texts.is_empty() {
            return Ok(vec![]);
        }
        if texts.len() == 1 {
            return Ok(vec![self.embed(&texts[0]).await?]);
        }
        let data = self
            .post_embed(&EmbedRequest {
                input: EmbedInput::Batch(texts),
                model: &self.model,
            })
            .await?;
        Ok(data.data.into_iter().map(|d| d.embedding).collect())
    }

    fn dimension(&self) -> usize {
        self.dimension
    }
}
