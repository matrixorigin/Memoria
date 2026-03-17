use async_trait::async_trait;
use memoria_core::{interfaces::EmbeddingProvider, MemoriaError};
use serde::{Deserialize, Serialize};

/// HTTP-based embedding client — OpenAI-compatible API.
pub struct HttpEmbedder {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    model: String,
    dimension: usize,
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
                input: EmbedInput::Single(text),
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

    async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, MemoriaError> {
        if texts.is_empty() {
            return Ok(vec![]);
        }
        if texts.len() == 1 {
            return Ok(vec![self.embed(&texts[0]).await?]);
        }

        let url = format!("{}/embeddings", self.base_url.trim_end_matches('/'));
        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.api_key)
            .json(&EmbedRequest {
                input: EmbedInput::Batch(texts),
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

        Ok(data.data.into_iter().map(|d| d.embedding).collect())
    }

    fn dimension(&self) -> usize {
        self.dimension
    }
}
