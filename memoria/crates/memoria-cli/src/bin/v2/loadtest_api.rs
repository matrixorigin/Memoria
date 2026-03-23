use super::{ApiClient, ApiVersion, Stats};
use reqwest::Client;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn first_recall_match(data: Option<&serde_json::Value>) -> Option<(String, String)> {
    let data = data?;
    let item = data["memories"].as_array()?.first()?;
    Some((
        item["id"].as_str()?.to_string(),
        item["text"]
            .as_str()
            .or_else(|| item["overview"].as_str())
            .or_else(|| item["abstract"].as_str())
            .unwrap_or_default()
            .to_string(),
    ))
}

pub(super) async fn remember(
    client: &ApiClient,
    content: &str,
    memory_type: &str,
    stats: &Stats,
) -> Option<String> {
    client
        .post(
            "/v2/memory/remember",
            serde_json::json!({
                "content": content,
                "type": memory_type,
                "session_id": client.session_id(),
                "source": {"kind": "loadtest"},
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
            "/v2/memory/recall",
            serde_json::json!({
                "query": query,
                "top_k": top_k,
                "scope": "all",
                "session_id": client.session_id(),
                "view": "compact",
            }),
            stats,
            expected,
        )
        .await
}

pub(super) async fn list_memories(client: &ApiClient, stats: &Stats, expected: &[u16]) -> u16 {
    client
        .get("/v2/memory/list?limit=50", stats, expected)
        .await
        .0
}

pub(super) async fn profile(client: &ApiClient, stats: &Stats, expected: &[u16]) -> u16 {
    client
        .get("/v2/memory/profile?limit=10", stats, expected)
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
    let t0 = Instant::now();
    let (recall_status, recall_data) = client
        .post_raw(
            "/v2/memory/recall",
            serde_json::json!({
                "query": query,
                "top_k": 1,
                "scope": "all",
                "session_id": client.session_id(),
                "view": "compact",
            }),
        )
        .await;
    let status = if recall_status != 200 {
        recall_status
    } else if let Some((memory_id, _)) = first_recall_match(recall_data.as_ref()) {
        client
            .patch_raw(
                "/v2/memory/update",
                serde_json::json!({
                    "memory_id": memory_id,
                    "content": new_content,
                    "reason": reason,
                }),
            )
            .await
            .0
    } else {
        404
    };
    client.record_compound(stats, expected, t0, status).await
}

pub(super) async fn purge(
    client: &ApiClient,
    query: &str,
    reason: &str,
    stats: &Stats,
    expected: &[u16],
) -> u16 {
    let t0 = Instant::now();
    let (recall_status, recall_data) = client
        .post_raw(
            "/v2/memory/recall",
            serde_json::json!({
                "query": query,
                "top_k": 10,
                "scope": "all",
                "session_id": client.session_id(),
                "view": "compact",
            }),
        )
        .await;
    let status = if recall_status != 200 {
        recall_status
    } else {
        let query_lower = query.to_ascii_lowercase();
        let ids: Vec<String> = recall_data
            .as_ref()
            .and_then(|data| data["memories"].as_array())
            .map(|items| {
                items
                    .iter()
                    .filter(|item| {
                        item["text"]
                            .as_str()
                            .map(|text| text.to_ascii_lowercase().contains(&query_lower))
                            .unwrap_or(false)
                    })
                    .filter_map(|item| item["id"].as_str().map(ToOwned::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        if ids.is_empty() {
            200
        } else if ids.len() == 1 {
            client
                .post_raw(
                    "/v2/memory/forget",
                    serde_json::json!({"memory_id": ids[0], "reason": reason}),
                )
                .await
                .0
        } else {
            client
                .post_raw(
                    "/v2/memory/batch-forget",
                    serde_json::json!({"memory_ids": ids, "reason": reason}),
                )
                .await
                .0
        }
    };
    client.record_compound(stats, expected, t0, status).await
}

pub(super) async fn maintenance_loop(client: &ApiClient, duration: Duration) -> Vec<Arc<Stats>> {
    let entities_extract = Arc::new(Stats::new("entities_extract"));
    let stats = Arc::new(Stats::new("stats"));
    let reflect = Arc::new(Stats::new("reflect"));
    let metrics = Arc::new(Stats::new("metrics"));
    let profile_stats = Arc::new(Stats::new("profile"));
    let jobs = Arc::new(Stats::new("jobs"));
    let tags = Arc::new(Stats::new("tags"));

    let deadline = Instant::now() + duration;
    while Instant::now() < deadline {
        match fastrand::u32(0..12) {
            0..=2 => {
                client
                    .post(
                        "/v2/memory/entities/extract",
                        serde_json::json!({"limit": 100}),
                        &entities_extract,
                        &[200],
                    )
                    .await;
            }
            3..=4 => {
                client.get("/v2/memory/stats", &stats, &[200]).await;
            }
            5 => {
                client
                    .post(
                        "/v2/memory/reflect",
                        serde_json::json!({"mode": "candidates", "limit": 20}),
                        &reflect,
                        &[200],
                    )
                    .await;
            }
            6..=8 => {
                client.get("/metrics", &metrics, &[200]).await;
            }
            9 => {
                client.get("/v2/memory/jobs?limit=20", &jobs, &[200]).await;
            }
            10 => {
                client.get("/v2/memory/tags?limit=20", &tags, &[200]).await;
            }
            _ => {
                profile(client, &profile_stats, &[200]).await;
            }
        }
        tokio::time::sleep(Duration::from_millis(fastrand::u64(1000..3000))).await;
    }

    vec![
        entities_extract,
        stats,
        reflect,
        metrics,
        profile_stats,
        jobs,
        tags,
    ]
}

pub(super) async fn preflight_extra(base: &str, token: &str, shared: Arc<Client>) -> u16 {
    let client = ApiClient::new(base, token, "lt-preflight", ApiVersion::V2, shared);
    let stats = Stats::new("_");
    client.get("/v2/memory/stats", &stats, &[200]).await.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        extract::State,
        http::StatusCode,
        routing::{get, patch, post},
        Json, Router,
    };
    use serde_json::Value;
    use std::net::SocketAddr;
    use tokio::sync::Mutex;

    #[derive(Clone, Default)]
    struct MockState {
        memory: Arc<Mutex<Option<(String, String)>>>,
    }

    async fn spawn_mock_v2_loadtest_server() -> String {
        async fn health() -> &'static str {
            "ok"
        }

        async fn remember(
            State(state): State<MockState>,
            Json(body): Json<Value>,
        ) -> (StatusCode, Json<Value>) {
            let content = body["content"].as_str().unwrap_or_default().to_string();
            *state.memory.lock().await = Some(("mem-1".to_string(), content.clone()));
            (
                StatusCode::CREATED,
                Json(serde_json::json!({
                    "memory_id": "mem-1",
                    "abstract": content,
                    "has_overview": true,
                    "has_detail": true
                })),
            )
        }

        async fn recall(State(state): State<MockState>, Json(_body): Json<Value>) -> Json<Value> {
            let memories = state
                .memory
                .lock()
                .await
                .clone()
                .map(|(id, text)| {
                    vec![serde_json::json!({
                        "id": id,
                        "text": text,
                        "type": "semantic",
                        "score": 1.0,
                        "related": false
                    })]
                })
                .unwrap_or_default();
            Json(serde_json::json!({
                "summary": {
                    "discovered_count": memories.len(),
                    "returned_count": memories.len(),
                    "truncated": false,
                    "by_retrieval_path": []
                },
                "memories": memories,
                "token_used": 1,
                "has_more": false
            }))
        }

        async fn update(State(state): State<MockState>, Json(body): Json<Value>) -> Json<Value> {
            let content = body["content"].as_str().unwrap_or_default().to_string();
            *state.memory.lock().await = Some(("mem-1".to_string(), content.clone()));
            Json(serde_json::json!({
                "memory_id": "mem-1",
                "abstract": content,
                "updated_at": "2026-01-01T00:00:00Z",
                "has_overview": true,
                "has_detail": true
            }))
        }

        async fn forget(State(state): State<MockState>, Json(_body): Json<Value>) -> Json<Value> {
            *state.memory.lock().await = None;
            Json(serde_json::json!({"memory_id": "mem-1", "forgotten": true}))
        }

        async fn list() -> Json<Value> {
            Json(serde_json::json!({"items": [], "next_cursor": null}))
        }

        async fn profile() -> Json<Value> {
            Json(serde_json::json!({"items": [], "next_cursor": null}))
        }

        async fn stats() -> Json<Value> {
            Json(
                serde_json::json!({"total_count": 1, "active_count": 1, "forgotten_count": 0, "by_type": []}),
            )
        }

        async fn create_key() -> (StatusCode, Json<Value>) {
            (StatusCode::CREATED, Json(serde_json::json!({"token": "k"})))
        }

        async fn list_keys() -> Json<Value> {
            Json(serde_json::json!({"items": []}))
        }

        async fn metrics() -> &'static str {
            "# mock metrics"
        }

        let state = MockState::default();
        let app = Router::new()
            .route("/health", get(health))
            .route("/v2/memory/remember", post(remember))
            .route("/v2/memory/recall", post(recall))
            .route("/v2/memory/update", patch(update))
            .route("/v2/memory/forget", post(forget))
            .route("/v2/memory/list", get(list))
            .route("/v2/memory/profile", get(profile))
            .route("/v2/memory/stats", get(stats))
            .route("/auth/keys", post(create_key).get(list_keys))
            .route("/metrics", get(metrics))
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind listener");
        let addr: SocketAddr = listener.local_addr().expect("listener addr");
        tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve mock v2 loadtest");
        });
        format!("http://{}", addr)
    }

    #[tokio::test]
    async fn v2_preflight_passes_against_mock_server() {
        let base = spawn_mock_v2_loadtest_server().await;
        assert!(super::super::loadtest_runtime::preflight(&base, "token", ApiVersion::V2).await);
    }
}
