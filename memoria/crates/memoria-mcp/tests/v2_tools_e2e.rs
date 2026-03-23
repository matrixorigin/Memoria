use memoria_service::MemoryService;
use memoria_storage::SqlMemoryStore;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::time::{sleep, Duration};
use uuid::Uuid;

fn test_dim() -> usize {
    std::env::var("EMBEDDING_DIM")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1024)
}
fn db_url() -> String {
    std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "mysql://root:111@localhost:6001/memoria".to_string())
}
fn uid() -> String {
    format!("ct_{}", &Uuid::new_v4().simple().to_string()[..8])
}

async fn setup() -> (Arc<MemoryService>, String) {
    let store = SqlMemoryStore::connect(&db_url(), test_dim(), uuid::Uuid::new_v4().to_string())
        .await
        .expect("connect");
    store.migrate().await.expect("migrate");
    let svc = Arc::new(MemoryService::new_sql_with_llm(Arc::new(store), None, None).await);
    (svc, uid())
}

async fn call_v2(name: &str, args: Value, svc: &Arc<MemoryService>, uid: &str) -> Value {
    memoria_mcp::v2::tools::call(name, args, svc, uid)
        .await
        .expect(name)
}

fn text(v: &Value) -> &str {
    v["content"][0]["text"].as_str().unwrap_or("")
}

fn json_text(v: &Value) -> Value {
    serde_json::from_str(text(v)).expect("json text")
}

async fn wait_for_v2_recall_id(
    svc: &Arc<MemoryService>,
    uid: &str,
    args: Value,
    expected_id: &str,
) -> Value {
    let mut last = json!({});
    for _ in 0..20 {
        let recalled = call_v2("memory_v2_recall", args.clone(), svc, uid).await;
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
    panic!("timed out waiting for recall result: {last}");
}

#[tokio::test]
async fn test_v2_tools_remember_list_recall_profile_and_expand() {
    let (svc, uid) = setup().await;

    let remembered = call_v2(
        "memory_v2_remember",
        json!({
            "content": "Rust platform guide for systems teams",
            "type": "semantic",
            "session_id": "sess-v2-mcp",
            "tags": ["rust", "systems"]
        }),
        &svc,
        &uid,
    )
    .await;
    let remembered_body = json_text(&remembered);
    let semantic_id = remembered_body["memory_id"].as_str().unwrap().to_string();
    assert_eq!(
        remembered_body["abstract"],
        "Rust platform guide for systems teams"
    );

    let _profile = call_v2(
        "memory_v2_remember",
        json!({
            "content": "Prefers Rust for infrastructure tooling",
            "type": "profile",
            "session_id": "sess-v2-mcp"
        }),
        &svc,
        &uid,
    )
    .await;

    let listed = call_v2(
        "memory_v2_list",
        json!({"type": "semantic", "session_id": "sess-v2-mcp", "limit": 10}),
        &svc,
        &uid,
    )
    .await;
    let listed_body = json_text(&listed);
    let items = listed_body["items"].as_array().expect("list items");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["id"], semantic_id);
    assert_eq!(items[0]["type"], "semantic");
    assert!(items[0].get("memory_type").is_none());

    let profile = call_v2(
        "memory_v2_profile",
        json!({"session_id": "sess-v2-mcp", "limit": 10}),
        &svc,
        &uid,
    )
    .await;
    let profile_body = json_text(&profile);
    let profile_items = profile_body["items"].as_array().expect("profile items");
    assert_eq!(profile_items.len(), 1);
    assert_eq!(
        profile_items[0]["content"],
        "Prefers Rust for infrastructure tooling"
    );

    let recalled_body = wait_for_v2_recall_id(
        &svc,
        &uid,
        json!({"query": "rust platform", "type": "semantic", "view": "full", "top_k": 5}),
        &semantic_id,
    )
    .await;
    let memories = recalled_body["memories"]
        .as_array()
        .expect("recalled memories");
    assert!(!memories.is_empty());
    assert!(memories.iter().any(|memory| memory["id"] == semantic_id));
    assert!(
        recalled_body["summary"]["returned_count"]
            .as_i64()
            .unwrap_or_default()
            >= 1
    );

    let expanded = call_v2(
        "memory_v2_expand",
        json!({"memory_id": semantic_id, "level": "links"}),
        &svc,
        &uid,
    )
    .await;
    let expanded_body = json_text(&expanded);
    assert_eq!(expanded_body["memory_id"], semantic_id);
    assert_eq!(expanded_body["level"], "links");
    assert!(expanded_body["links"].is_array());
}

#[tokio::test]
async fn test_v2_tools_focus_update_forget_and_history() {
    let (svc, uid) = setup().await;

    let remembered = call_v2(
        "memory_v2_remember",
        json!({
            "content": "Legacy rust handbook for MCP V2 history",
            "type": "semantic",
            "session_id": "sess-v2-history"
        }),
        &svc,
        &uid,
    )
    .await;
    let remembered_body = json_text(&remembered);
    let memory_id = remembered_body["memory_id"].as_str().unwrap().to_string();

    let focused = call_v2(
        "memory_v2_focus",
        json!({"type": "session", "value": "sess-v2-history", "ttl_secs": 600}),
        &svc,
        &uid,
    )
    .await;
    let focused_body = json_text(&focused);
    assert_eq!(focused_body["type"], "session");
    assert_eq!(focused_body["value"], "sess-v2-history");
    assert!(focused_body["active_until"].is_string());

    let updated = call_v2(
        "memory_v2_update",
        json!({
            "memory_id": memory_id,
            "content": "Updated rust handbook for MCP V2 history",
            "reason": "clarified",
            "tags_add": ["shared"]
        }),
        &svc,
        &uid,
    )
    .await;
    let updated_body = json_text(&updated);
    assert_eq!(updated_body["memory_id"], memory_id);
    assert!(updated_body["updated_at"].is_string());

    let forgotten = call_v2(
        "memory_v2_forget",
        json!({"memory_id": memory_id, "reason": "cleanup"}),
        &svc,
        &uid,
    )
    .await;
    let forgotten_body = json_text(&forgotten);
    assert_eq!(forgotten_body["memory_id"], memory_id);
    assert_eq!(forgotten_body["forgotten"], true);

    let history = call_v2(
        "memory_v2_history",
        json!({"memory_id": memory_id, "limit": 10}),
        &svc,
        &uid,
    )
    .await;
    let history_body = json_text(&history);
    let items = history_body["items"].as_array().expect("history items");
    assert_eq!(items.len(), 3);
    assert_eq!(items[0]["event_type"], "forgotten");
    assert_eq!(items[1]["event_type"], "updated");
    assert_eq!(items[2]["event_type"], "remembered");
    assert_eq!(items[2]["payload"]["type"], "semantic");
    assert!(items[2]["payload"].get("memory_type").is_none());
}

#[tokio::test]
async fn test_v2_tools_reflect_internal_creates_synthesized_memory() {
    let (svc, uid) = setup().await;

    let _ = call_v2(
        "memory_v2_remember",
        json!({
            "content": "Session alpha deployment note",
            "type": "semantic",
            "session_id": "sess-v2-reflect"
        }),
        &svc,
        &uid,
    )
    .await;
    let _ = call_v2(
        "memory_v2_remember",
        json!({
            "content": "Session beta deployment handoff",
            "type": "semantic",
            "session_id": "sess-v2-reflect"
        }),
        &svc,
        &uid,
    )
    .await;

    let reflected = call_v2(
        "memory_v2_reflect",
        json!({"mode": "internal", "session_id": "sess-v2-reflect", "limit": 10}),
        &svc,
        &uid,
    )
    .await;
    let reflected_body = json_text(&reflected);
    assert_eq!(reflected_body["mode"], "internal");
    assert_eq!(reflected_body["synthesized"], true);
    assert_eq!(reflected_body["scenes_created"], 1);
    let candidates = reflected_body["candidates"].as_array().expect("candidates");
    assert!(!candidates.is_empty());
    assert!(candidates[0]["memories"].as_array().unwrap().len() >= 2);

    let listed = call_v2("memory_v2_list", json!({"limit": 10}), &svc, &uid).await;
    let listed_body = json_text(&listed);
    let items = listed_body["items"].as_array().expect("list items");
    assert_eq!(items.len(), 3);
}
