//! Minimal OpenAI-compatible LLM client.
//! Reads LLM_API_KEY, LLM_BASE_URL, LLM_MODEL from environment.

use serde::{Deserialize, Serialize};

#[derive(Clone)]
pub struct LlmClient {
    api_key: String,
    base_url: String,
    model: String,
    client: reqwest::Client,
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: &'a [ChatMessage],
    temperature: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
}

#[derive(Deserialize)]
struct Choice {
    message: MessageContent,
}

#[derive(Deserialize)]
struct MessageContent {
    content: Option<String>,
}

impl LlmClient {
    /// Create from environment variables. Returns None if LLM_API_KEY not set.
    pub fn from_env() -> Option<Self> {
        let api_key = std::env::var("LLM_API_KEY").ok().filter(|s| !s.is_empty())?;
        let base_url = std::env::var("LLM_BASE_URL")
            .unwrap_or_else(|_| "https://api.openai.com/v1".to_string());
        let model = std::env::var("LLM_MODEL")
            .unwrap_or_else(|_| "gpt-4o-mini".to_string());
        Some(Self::new(api_key, base_url, model))
    }

    /// Create from explicit config values.
    pub fn new(api_key: String, base_url: String, model: String) -> Self {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self { api_key, base_url, model, client }
    }

    pub fn model(&self) -> &str { &self.model }

    /// Send a chat completion request. Returns the assistant message content.
    pub async fn chat(
        &self,
        messages: &[ChatMessage],
        temperature: f32,
        max_tokens: Option<u32>,
    ) -> anyhow::Result<String> {
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let req = ChatRequest {
            model: &self.model,
            messages,
            temperature,
            max_tokens,
        };
        let resp = self.client
            .post(&url)
            .bearer_auth(&self.api_key)
            .json(&req)
            .send()
            .await?
            .error_for_status()?
            .json::<ChatResponse>()
            .await?;

        Ok(resp.choices.into_iter()
            .next()
            .and_then(|c| c.message.content)
            .unwrap_or_default())
    }
}
