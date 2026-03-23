use memoria_core::interfaces::EmbeddingProvider;
use memoria_embedding::MockEmbedder;
use memoria_git::GitForDataService;
use memoria_service::MemoryService;
use memoria_storage::SqlMemoryStore;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::time::{sleep, Duration};
use uuid::Uuid;

fn isolated_db_url() -> String {
    let base = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "mysql://root:111@localhost:6001/memoria".to_string());
    let Some((prefix, db_name)) = base.rsplit_once('/') else {
        return base;
    };
    format!("{prefix}/{}_{}", db_name, Uuid::new_v4().simple())
}

fn db_name_from_url(db: &str) -> String {
    db.rsplit('/').next().unwrap_or("memoria").to_string()
}

fn is_unknown_database_error(message: &str) -> bool {
    message.contains("Unknown database")
        || message.contains("1049 (HY000)")
        || message.contains("number: 1049")
}

async fn migrate_store_with_retry(store: &SqlMemoryStore) {
    let mut last_error = None;
    for attempt in 0..5 {
        match store.migrate().await {
            Ok(()) => return,
            Err(err) if attempt < 4 && is_unknown_database_error(&err.to_string()) => {
                last_error = Some(err.to_string());
                sleep(Duration::from_millis(50 * (attempt as u64 + 1))).await;
            }
            Err(err) => panic!("migrate: {err:?}"),
        }
    }
    panic!(
        "migrate: {}",
        last_error.unwrap_or_else(|| "unknown migrate error".to_string())
    );
}

async fn connect_pool_with_retry(db: &str) -> sqlx::mysql::MySqlPool {
    let mut last_error = None;
    for attempt in 0..5 {
        match sqlx::mysql::MySqlPool::connect(db).await {
            Ok(pool) => return pool,
            Err(err) if attempt < 4 && is_unknown_database_error(&err.to_string()) => {
                last_error = Some(err.to_string());
                sleep(Duration::from_millis(50 * (attempt as u64 + 1))).await;
            }
            Err(err) => panic!("pool: {err:?}"),
        }
    }
    panic!(
        "pool: {}",
        last_error.unwrap_or_else(|| "unknown pool error".to_string())
    );
}

async fn spawn_api_server() -> String {
    let db = isolated_db_url();
    let dim: usize = std::env::var("EMBEDDING_DIM")
        .unwrap_or_else(|_| "1024".to_string())
        .parse()
        .unwrap_or(1024);
    let store = SqlMemoryStore::connect(&db, dim, Uuid::new_v4().to_string())
        .await
        .expect("connect");
    migrate_store_with_retry(&store).await;
    let pool = connect_pool_with_retry(&db).await;
    let git = Arc::new(GitForDataService::new(pool, db_name_from_url(&db)));
    let embedder: Arc<dyn EmbeddingProvider> = Arc::new(MockEmbedder::new(dim));
    let service = Arc::new(MemoryService::new_sql_with_llm(
        Arc::new(store),
        Some(embedder),
        None,
    ).await);
    let state = memoria_api::AppState::new(service, git, String::new());
    let app = memoria_api::build_router(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let port = listener.local_addr().expect("local addr").port();
    let handle = tokio::spawn(async move { axum::serve(listener, app).await });

    sleep(Duration::from_millis(300)).await;
    assert!(!handle.is_finished(), "server exited unexpectedly");
    format!("http://127.0.0.1:{port}")
}

async fn call_remote(remote: &memoria_mcp::remote::RemoteClient, name: &str, args: Value) -> Value {
    remote.call(name, args).await.expect(name)
}

fn text(v: &Value) -> &str {
    v["content"][0]["text"].as_str().unwrap_or("")
}

fn json_text(v: &Value) -> Value {
    serde_json::from_str(text(v)).expect("json text")
}

async fn wait_for_remote_v2_recall_id(
    remote: &memoria_mcp::remote::RemoteClient,
    args: Value,
    expected_id: &str,
) -> Value {
    let mut last = json!({});
    for _ in 0..20 {
        let recalled = call_remote(remote, "memory_v2_recall", args.clone()).await;
        let body = json_text(&recalled);
        if body["memories"]
            .as_array()
            .is_some_and(|items| items.iter().any(|memory| memory["id"] == expected_id))
        {
            return body;
        }
        last = body;
        sleep(Duration::from_millis(250)).await;
    }
    panic!("timed out waiting for remote V2 recall result: {last}");
}

#[tokio::test]
async fn test_e2e_remote_v2_tools_keep_v1_v2_separate() {
    let base = spawn_api_server().await;
    let uid = format!("remote_v2_{}", Uuid::new_v4().simple());
    let remote = memoria_mcp::remote::RemoteClient::new(&base, None, uid, Some("codex"));

    let remembered = call_remote(
        &remote,
        "memory_v2_remember",
        json!({
            "content": "Rust platform guide for systems teams",
            "type": "semantic",
            "session_id": "sess-remote-v2",
            "tags": ["rust", "systems"]
        }),
    )
    .await;
    let remembered_body = json_text(&remembered);
    let semantic_id = remembered_body["memory_id"].as_str().unwrap().to_string();

    let _profile = call_remote(
        &remote,
        "memory_v2_remember",
        json!({
            "content": "Prefers Rust for infrastructure tooling",
            "type": "profile",
            "session_id": "sess-remote-v2"
        }),
    )
    .await;

    let legacy_list = call_remote(&remote, "memory_list", json!({"limit": 10})).await;
    assert_eq!(text(&legacy_list), "No memories found.");

    let legacy_profile = call_remote(&remote, "memory_profile", json!({})).await;
    assert_eq!(text(&legacy_profile), "No profile memories found.");

    let listed = call_remote(
        &remote,
        "memory_v2_list",
        json!({"type": "semantic", "session_id": "sess-remote-v2", "limit": 10}),
    )
    .await;
    let listed_body = json_text(&listed);
    let items = listed_body["items"].as_array().expect("list items");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["id"], semantic_id);
    assert_eq!(items[0]["type"], "semantic");
    assert!(items[0].get("memory_type").is_none());

    let profiled = call_remote(
        &remote,
        "memory_v2_profile",
        json!({"session_id": "sess-remote-v2", "limit": 10}),
    )
    .await;
    let profiled_body = json_text(&profiled);
    let profile_items = profiled_body["items"].as_array().expect("profile items");
    assert_eq!(profile_items.len(), 1);
    assert_eq!(
        profile_items[0]["content"],
        "Prefers Rust for infrastructure tooling"
    );

    let focused = call_remote(
        &remote,
        "memory_v2_focus",
        json!({"type": "session", "value": "sess-remote-v2", "ttl_secs": 600}),
    )
    .await;
    let focused_body = json_text(&focused);
    assert_eq!(focused_body["type"], "session");
    assert_eq!(focused_body["value"], "sess-remote-v2");

    let expanded = call_remote(
        &remote,
        "memory_v2_expand",
        json!({"memory_id": semantic_id, "level": "links"}),
    )
    .await;
    let expanded_body = json_text(&expanded);
    assert_eq!(expanded_body["memory_id"], semantic_id);
    assert_eq!(expanded_body["level"], "links");

    let recalled_body = wait_for_remote_v2_recall_id(
        &remote,
        json!({"query": "rust platform", "type": "semantic", "view": "compact", "top_k": 5}),
        &semantic_id,
    )
    .await;
    let memories = recalled_body["memories"]
        .as_array()
        .expect("recalled memories");
    assert!(memories.iter().any(|memory| memory["id"] == semantic_id));
    assert_eq!(memories[0]["type"], "semantic");

    let updated = call_remote(
        &remote,
        "memory_v2_update",
        json!({
            "memory_id": semantic_id,
            "content": "Updated rust platform guide for systems teams",
            "reason": "clarified"
        }),
    )
    .await;
    let updated_body = json_text(&updated);
    assert_eq!(updated_body["memory_id"], semantic_id);
    assert!(updated_body["updated_at"].is_string());

    let forgotten = call_remote(
        &remote,
        "memory_v2_forget",
        json!({"memory_id": semantic_id, "reason": "cleanup"}),
    )
    .await;
    let forgotten_body = json_text(&forgotten);
    assert_eq!(forgotten_body["memory_id"], semantic_id);
    assert_eq!(forgotten_body["forgotten"], true);

    let history = call_remote(
        &remote,
        "memory_v2_history",
        json!({"memory_id": semantic_id, "limit": 10}),
    )
    .await;
    let history_body = json_text(&history);
    let history_items = history_body["items"].as_array().expect("history items");
    assert_eq!(history_items.len(), 3);
    assert_eq!(history_items[0]["event_type"], "forgotten");
    assert_eq!(history_items[1]["event_type"], "updated");
    assert_eq!(history_items[2]["event_type"], "remembered");
    assert_eq!(history_items[2]["payload"]["type"], "semantic");
    assert!(history_items[2]["payload"].get("memory_type").is_none());

    let listed_after_forget = call_remote(
        &remote,
        "memory_v2_list",
        json!({"type": "semantic", "session_id": "sess-remote-v2", "limit": 10}),
    )
    .await;
    let listed_after_forget_body = json_text(&listed_after_forget);
    assert_eq!(
        listed_after_forget_body["items"]
            .as_array()
            .expect("post-forget items")
            .len(),
        0
    );
}

#[tokio::test]
async fn test_e2e_remote_v2_reflect_candidates() {
    let base = spawn_api_server().await;
    let uid = format!("remote_reflect_{}", Uuid::new_v4().simple());
    let remote = memoria_mcp::remote::RemoteClient::new(&base, None, uid, Some("codex"));

    let _ = call_remote(
        &remote,
        "memory_v2_remember",
        json!({
            "content": "Session alpha deployment handoff",
            "type": "semantic",
            "session_id": "sess-remote-reflect"
        }),
    )
    .await;
    let _ = call_remote(
        &remote,
        "memory_v2_remember",
        json!({
            "content": "Session beta deployment checklist",
            "type": "semantic",
            "session_id": "sess-remote-reflect"
        }),
    )
    .await;

    let reflected = call_remote(
        &remote,
        "memory_v2_reflect",
        json!({"mode": "candidates", "session_id": "sess-remote-reflect", "limit": 10}),
    )
    .await;
    let reflected_body = json_text(&reflected);
    assert_eq!(reflected_body["mode"], "candidates");
    assert_eq!(reflected_body["synthesized"], false);
    let candidates = reflected_body["candidates"].as_array().expect("candidates");
    assert!(!candidates.is_empty());
    assert!(candidates[0]["memory_count"].as_i64().unwrap_or_default() >= 2);

    let legacy_list = call_remote(&remote, "memory_list", json!({"limit": 10})).await;
    assert_eq!(text(&legacy_list), "No memories found.");
}
