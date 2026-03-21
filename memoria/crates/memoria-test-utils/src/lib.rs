//! Shared fake LLM server for E2E tests.
//!
//! Spawns a local HTTP server that responds to `/chat/completions` with
//! canned responses based on prompt content. Each test file can supply
//! extra `(prompt_contains, response_json)` pairs for domain-specific prompts.

use std::sync::Arc;

/// A `(needle, response_body)` pair: if the prompt contains `needle`,
/// the fake server returns `response_body` as the assistant message content.
pub type PromptRule = (&'static str, serde_json::Value);

/// Default rules shared by all test suites (reflect + entity extraction).
pub fn default_rules() -> Vec<PromptRule> {
    vec![
        (
            "OUTPUT FORMAT (JSON array, 0-2 items)",
            serde_json::json!([{
                "type": "semantic",
                "content": "Prefer deterministic validation for reflection flows.",
                "confidence": 0.66,
                "evidence_summary": "Stored sessions focused on stable tool validation."
            }]),
        ),
        (
            "Extract named entities from the following text",
            serde_json::json!([
                {"name": "AlphaMesh", "type": "tech"},
                {"name": "DeltaFabric", "type": "tech"}
            ]),
        ),
    ]
}

/// Spawn a fake OpenAI-compatible LLM server.
///
/// `extra_rules` are checked before `default_rules`, so they can override.
/// Returns the `LlmClient` (with proxy disabled) and a shutdown handle.
/// Drop or send on the handle to stop the server.
pub async fn spawn_fake_llm(
    extra_rules: Vec<PromptRule>,
) -> (
    Arc<memoria_embedding::LlmClient>,
    tokio::sync::oneshot::Sender<()>,
) {
    // Merge: extra first (higher priority), then defaults.
    let rules: Vec<(String, String)> = extra_rules
        .iter()
        .chain(default_rules().iter())
        .map(|(needle, val)| (needle.to_string(), val.to_string()))
        .collect();
    let rules = Arc::new(rules);

    // Fallback response when no rule matches.
    let fallback = default_rules().last().unwrap().1.to_string();
    let fallback = Arc::new(fallback);

    let rules_c = Arc::clone(&rules);
    let fallback_c = Arc::clone(&fallback);

    let app = axum::Router::new().route(
        "/chat/completions",
        axum::routing::post(
            move |axum::Json(payload): axum::Json<serde_json::Value>| {
                let rules = Arc::clone(&rules_c);
                let fallback = Arc::clone(&fallback_c);
                async move {
                    let prompt = payload["messages"]
                        .as_array()
                        .and_then(|msgs| msgs.last())
                        .and_then(|m| m["content"].as_str())
                        .unwrap_or("");
                    let content = rules
                        .iter()
                        .find(|(needle, _)| prompt.contains(needle.as_str()))
                        .map(|(_, resp)| resp.clone())
                        .unwrap_or_else(|| fallback.as_str().to_string());
                    axum::Json(serde_json::json!({
                        "choices": [{"message": {"content": content}}]
                    }))
                }
            },
        ),
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap();
    let port = listener.local_addr().unwrap().port();
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async { let _ = shutdown_rx.await; })
            .await
            .unwrap();
    });
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

    let client = Arc::new(memoria_embedding::LlmClient::new_no_proxy(
        "fake-key".into(),
        format!("http://127.0.0.1:{port}"),
        "fake-model".into(),
    ));
    (client, shutdown_tx)
}
