use super::{ApiClient, Stats};

pub(super) async fn remember(
    client: &ApiClient,
    content: &str,
    memory_type: &str,
    stats: &Stats,
) -> Option<String> {
    client
        .post(
            "/v1/memories",
            serde_json::json!({
                "content": content,
                "memory_type": memory_type,
            }),
            stats,
            &[201],
        )
        .await
        .1
        .and_then(|data| data["memory_id"].as_str().map(ToOwned::to_owned))
}

pub(super) async fn recall(
    client: &ApiClient,
    query: &str,
    top_k: i64,
    stats: &Stats,
    expected: &[u16],
) -> (u16, Option<serde_json::Value>) {
    client
        .post(
            "/v1/memories/retrieve",
            serde_json::json!({"query": query, "top_k": top_k}),
            stats,
            expected,
        )
        .await
}

pub(super) async fn search(
    client: &ApiClient,
    query: &str,
    top_k: i64,
    stats: &Stats,
    expected: &[u16],
) -> u16 {
    client
        .post(
            "/v1/memories/search",
            serde_json::json!({"query": query, "top_k": top_k}),
            stats,
            expected,
        )
        .await
        .0
}

pub(super) async fn list_memories(client: &ApiClient, stats: &Stats, expected: &[u16]) -> u16 {
    client.get("/v1/memories", stats, expected).await.0
}

pub(super) async fn profile(client: &ApiClient, stats: &Stats, expected: &[u16]) -> u16 {
    client
        .get(&format!("/v1/profiles/{}", client.user_id), stats, expected)
        .await
        .0
}

pub(super) async fn correct(
    client: &ApiClient,
    query: &str,
    new_content: &str,
    reason: &str,
    stats: &Stats,
    expected: &[u16],
) -> u16 {
    client
        .post(
            "/v1/memories/correct",
            serde_json::json!({
                "query": query,
                "new_content": new_content,
                "reason": reason,
            }),
            stats,
            expected,
        )
        .await
        .0
}

pub(super) async fn purge(
    client: &ApiClient,
    query: &str,
    reason: &str,
    stats: &Stats,
    expected: &[u16],
) -> u16 {
    client
        .post(
            "/v1/memories/purge",
            serde_json::json!({"topic": query, "reason": reason}),
            stats,
            expected,
        )
        .await
        .0
}
