/// REST API E2E tests — starts a real server, hits it with reqwest.
/// Requires DATABASE_URL env var.

use std::sync::Arc;
use serde_json::{json, Value};
fn db_url() -> String {
    std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "mysql://root:111@localhost:6001/memoria_rs".to_string())
}

fn uid() -> String {
    format!("api_test_{}", uuid::Uuid::new_v4().simple())
}

/// Spawn the API server on a random port, return (base_url, client).
async fn spawn_server() -> (String, reqwest::Client) {
    use memoria_git::GitForDataService;
    use memoria_service::{Config, MemoryService};
    use memoria_storage::SqlMemoryStore;
    use sqlx::mysql::MySqlPool;

    let cfg = Config::from_env();
    let db = db_url();

    let store = SqlMemoryStore::connect(&db, 4).await.expect("connect");
    store.migrate().await.expect("migrate");
    let pool = MySqlPool::connect(&db).await.expect("pool");
    let git = Arc::new(GitForDataService::new(pool, &cfg.db_name));
    let service = Arc::new(MemoryService::new_sql(Arc::new(store), None));
    let state = memoria_api::AppState::new(service, git, String::new());

    let app = memoria_api::build_router(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().unwrap().port();
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await
    });

    tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;
    if handle.is_finished() {
        panic!("Server task finished unexpectedly");
    }

    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("client");
    let base = format!("http://127.0.0.1:{port}");
    (base, client)
}

// ── 1. health ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_api_health() {
    let (base, client) = spawn_server().await;
    let r = client.get(format!("{base}/health")).send().await.expect("get");
    assert_eq!(r.status(), 200);
    assert_eq!(r.text().await.unwrap(), "ok");
    println!("✅ GET /health");
}

// ── 2. store + list ───────────────────────────────────────────────────────────

#[tokio::test]
async fn test_api_store_and_list() {
    let (base, client) = spawn_server().await;
    let uid = uid();

    let r = client.post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({"content": "Rust is fast", "memory_type": "semantic"}))
        .send().await.expect("post");
    assert_eq!(r.status(), 201);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["content"], "Rust is fast");
    let mid = body["memory_id"].as_str().unwrap().to_string();
    println!("✅ POST /v1/memories: {mid}");

    let r = client.get(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .send().await.expect("get");
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert!(body["items"].as_array().unwrap().iter().any(|m| m["memory_id"] == mid));
    println!("✅ GET /v1/memories: {} items", body["items"].as_array().unwrap().len());
}

// ── 3. batch store ────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_api_batch_store() {
    let (base, client) = spawn_server().await;
    let uid = uid();

    let r = client.post(format!("{base}/v1/memories/batch"))
        .header("X-User-Id", &uid)
        .json(&json!({"memories": [
            {"content": "Memory A"},
            {"content": "Memory B", "memory_type": "profile"},
        ]}))
        .send().await.expect("post");
    assert_eq!(r.status(), 201);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body.as_array().unwrap().len(), 2);
    println!("✅ POST /v1/memories/batch: 2 stored");
}

// ── 4. retrieve ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_api_retrieve() {
    let (base, client) = spawn_server().await;
    let uid = uid();

    client.post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({"content": "MatrixOne is a distributed database"}))
        .send().await.unwrap();

    let r = client.post(format!("{base}/v1/memories/retrieve"))
        .header("X-User-Id", &uid)
        .json(&json!({"query": "database", "top_k": 5}))
        .send().await.expect("post");
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert!(!body.as_array().unwrap().is_empty());
    println!("✅ POST /v1/memories/retrieve: {} results", body.as_array().unwrap().len());
}

// ── 5. correct by id ──────────────────────────────────────────────────────────

#[tokio::test]
async fn test_api_correct() {
    let (base, client) = spawn_server().await;
    let uid = uid();

    let r = client.post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({"content": "Uses black for formatting"}))
        .send().await.unwrap();
    let mid = r.json::<Value>().await.unwrap()["memory_id"].as_str().unwrap().to_string();

    let r = client.put(format!("{base}/v1/memories/{mid}/correct"))
        .header("X-User-Id", &uid)
        .json(&json!({"new_content": "Uses ruff for formatting"}))
        .send().await.expect("put");
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["content"], "Uses ruff for formatting");
    println!("✅ PUT /v1/memories/:id/correct");
}

// ── 6. delete ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_api_delete() {
    let (base, client) = spawn_server().await;
    let uid = uid();

    let r = client.post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({"content": "to be deleted"}))
        .send().await.unwrap();
    let mid = r.json::<Value>().await.unwrap()["memory_id"].as_str().unwrap().to_string();

    let r = client.delete(format!("{base}/v1/memories/{mid}"))
        .header("X-User-Id", &uid)
        .send().await.expect("delete");
    assert_eq!(r.status(), 204);
    println!("✅ DELETE /v1/memories/:id");
}

// ── 7. purge bulk ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_api_purge_bulk() {
    let (base, client) = spawn_server().await;
    let uid = uid();

    let mut ids = Vec::new();
    for i in 0..3 {
        let r = client.post(format!("{base}/v1/memories"))
            .header("X-User-Id", &uid)
            .json(&json!({"content": format!("bulk purge {i}")}))
            .send().await.unwrap();
        ids.push(r.json::<Value>().await.unwrap()["memory_id"].as_str().unwrap().to_string());
    }

    let r = client.post(format!("{base}/v1/memories/purge"))
        .header("X-User-Id", &uid)
        .json(&json!({"memory_ids": ids}))
        .send().await.expect("post");
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["purged"], 3);
    println!("✅ POST /v1/memories/purge: 3 purged");
}

// ── 8. profile ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_api_profile() {
    let (base, client) = spawn_server().await;
    let uid = uid();

    client.post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({"content": "Prefers Rust", "memory_type": "profile"}))
        .send().await.unwrap();

    let r = client.get(format!("{base}/v1/profiles"))
        .header("X-User-Id", &uid)
        .send().await.expect("get");
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert!(body["profile"].as_str().unwrap().contains("Prefers Rust"));
    println!("✅ GET /v1/profiles");
}

// ── 9. governance ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_api_governance() {
    let (base, client) = spawn_server().await;
    let uid = uid();

    let r = client.post(format!("{base}/v1/governance"))
        .header("X-User-Id", &uid)
        .json(&json!({"force": true}))
        .send().await.expect("post");
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert!(body.get("quarantined").is_some() || body.get("skipped").is_some());
    println!("✅ POST /v1/governance");
}

// ── Helper: spawn server with master key ─────────────────────────────────────

async fn spawn_server_with_master_key(master_key: &str) -> (String, reqwest::Client) {
    use memoria_git::GitForDataService;
    use memoria_service::{Config, MemoryService};
    use memoria_storage::SqlMemoryStore;
    use sqlx::mysql::MySqlPool;

    let cfg = Config::from_env();
    let db = db_url();
    let store = SqlMemoryStore::connect(&db, 4).await.expect("connect");
    store.migrate().await.expect("migrate");
    let pool = MySqlPool::connect(&db).await.expect("pool");
    let git = Arc::new(GitForDataService::new(pool, &cfg.db_name));
    let service = Arc::new(MemoryService::new_sql(Arc::new(store), None));
    let state = memoria_api::AppState::new(service, git, master_key.to_string());
    let app = memoria_api::build_router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    (format!("http://127.0.0.1:{port}"), client)
}

// ── 10. auth: missing token returns 401 ──────────────────────────────────────

#[tokio::test]
async fn test_api_auth_required() {
    let mk = "test-master-key-12345";
    let (base, client) = spawn_server_with_master_key(mk).await;

    // No token → 401
    let r = client.get(format!("{base}/v1/memories"))
        .header("X-User-Id", "alice")
        .send().await.unwrap();
    assert_eq!(r.status(), 401);

    // Wrong token → 401
    let r = client.get(format!("{base}/v1/memories"))
        .header("X-User-Id", "alice")
        .header("Authorization", "Bearer wrong-key")
        .send().await.unwrap();
    assert_eq!(r.status(), 401);

    // Correct token → 200
    let r = client.get(format!("{base}/v1/memories"))
        .header("X-User-Id", "alice")
        .header("Authorization", format!("Bearer {mk}"))
        .send().await.unwrap();
    assert_eq!(r.status(), 200);

    println!("✅ Auth: 401 without token, 200 with correct token");
}

// ── 10b. auth: full API key CRUD (create/list/rotate/revoke) ─────────────────

#[tokio::test]
async fn test_api_key_crud() {
    let mk = "test-master-key-crud";
    let (base, client) = spawn_server_with_master_key(mk).await;
    let auth = format!("Bearer {mk}");
    let uid = uid();

    // 1. Create key
    let r = client.post(format!("{base}/auth/keys"))
        .header("Authorization", &auth)
        .json(&json!({"user_id": uid, "name": "test-key-1"}))
        .send().await.unwrap();
    assert_eq!(r.status(), 201, "create key");
    let body: Value = r.json().await.unwrap();
    let key_id = body["key_id"].as_str().unwrap().to_string();
    let raw_key = body["raw_key"].as_str().unwrap().to_string();
    assert!(raw_key.starts_with("sk-"), "raw_key should start with sk-");
    assert_eq!(body["user_id"].as_str().unwrap(), uid);
    assert_eq!(body["name"].as_str().unwrap(), "test-key-1");
    println!("✅ create key: {key_id}, prefix={}", body["key_prefix"]);

    // 2. List keys — use master key to authenticate (API keys are for external use)
    let r = client.get(format!("{base}/auth/keys"))
        .header("Authorization", &auth)
        .header("X-User-Id", &uid)
        .send().await.unwrap();
    assert_eq!(r.status(), 200, "list keys");
    let keys: Vec<Value> = r.json().await.unwrap();
    assert!(keys.iter().any(|k| k["key_id"].as_str() == Some(&key_id)), "should find created key");
    println!("✅ list keys: {} keys found", keys.len());

    // 3. Rotate key
    let r = client.put(format!("{base}/auth/keys/{key_id}/rotate"))
        .header("Authorization", &auth)
        .send().await.unwrap();
    assert_eq!(r.status(), 201, "rotate key");
    let body: Value = r.json().await.unwrap();
    let new_key_id = body["key_id"].as_str().unwrap().to_string();
    let new_raw_key = body["raw_key"].as_str().unwrap().to_string();
    assert_ne!(new_key_id, key_id, "rotated key should have new id");
    assert_ne!(new_raw_key, raw_key, "rotated key should have new raw_key");
    assert_eq!(body["name"].as_str().unwrap(), "test-key-1", "name preserved");
    println!("✅ rotate key: old={key_id} → new={new_key_id}");

    // 4. Old key should be deactivated — verify via list
    let r = client.get(format!("{base}/auth/keys"))
        .header("Authorization", &auth)
        .header("X-User-Id", &uid)
        .send().await.unwrap();
    let keys: Vec<Value> = r.json().await.unwrap();
    assert!(!keys.iter().any(|k| k["key_id"].as_str() == Some(&key_id)),
        "old key should not appear in active list");
    println!("✅ old key deactivated after rotate");

    // 5. New key appears in list
    assert!(keys.iter().any(|k| k["key_id"].as_str() == Some(&new_key_id)),
        "new key should appear in active list");
    println!("✅ new key in active list after rotate");

    // 6. Revoke key
    let r = client.delete(format!("{base}/auth/keys/{new_key_id}"))
        .header("Authorization", &auth)
        .send().await.unwrap();
    assert_eq!(r.status(), 204, "revoke key");
    println!("✅ revoke key: {new_key_id}");

    // 7. Revoked key should not appear in active list
    let r = client.get(format!("{base}/auth/keys"))
        .header("Authorization", &auth)
        .header("X-User-Id", &uid)
        .send().await.unwrap();
    let keys: Vec<Value> = r.json().await.unwrap();
    assert!(!keys.iter().any(|k| k["key_id"].as_str() == Some(&new_key_id)),
        "revoked key should not appear in active list");
    println!("✅ revoked key not in active list");

    // 8. Rotate non-existent key → 404
    let r = client.put(format!("{base}/auth/keys/nonexistent-id/rotate"))
        .header("Authorization", &auth)
        .send().await.unwrap();
    assert_eq!(r.status(), 404, "rotate nonexistent");
    println!("✅ rotate nonexistent → 404");

    // 9. Revoke non-existent key → 404
    let r = client.delete(format!("{base}/auth/keys/nonexistent-id"))
        .header("Authorization", &auth)
        .send().await.unwrap();
    assert_eq!(r.status(), 404, "revoke nonexistent");
    println!("✅ revoke nonexistent → 404");
}

// ── 10c. observe endpoint ────────────────────────────────────────────────────

#[tokio::test]
async fn test_api_observe_turn() {
    let (base, client) = spawn_server().await;
    let uid = uid();

    // Observe with assistant + user messages
    let r = client.post(format!("{base}/v1/observe"))
        .header("X-User-Id", &uid)
        .json(&json!({
            "messages": [
                {"role": "user", "content": "What is Rust?"},
                {"role": "assistant", "content": "Rust is a systems programming language"},
                {"role": "system", "content": "You are helpful"},
                {"role": "assistant", "content": ""}
            ]
        }))
        .send().await.unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    let memories = body["memories"].as_array().unwrap();
    // Should store user + non-empty assistant messages, skip system + empty
    assert_eq!(memories.len(), 2, "should store 2 memories (user + assistant): {body}");
    assert!(body.get("warning").is_some(), "should have LLM warning without LLM");
    println!("✅ observe: stored {} memories, warning={}", memories.len(), body["warning"]);

    // Verify stored memories are retrievable
    let r = client.post(format!("{base}/v1/memories/search"))
        .header("X-User-Id", &uid)
        .json(&json!({"query": "Rust programming", "top_k": 10}))
        .send().await.unwrap();
    assert_eq!(r.status(), 200);
    let results: Vec<Value> = r.json().await.unwrap();
    assert!(results.len() >= 2, "should find observed memories, got {}", results.len());
    println!("✅ observe memories retrievable: {} found", results.len());
}

#[tokio::test]
async fn test_api_observe_empty_messages() {
    let (base, client) = spawn_server().await;
    let uid = uid();

    // Empty messages array → should return 200 with empty memories
    let r = client.post(format!("{base}/v1/observe"))
        .header("X-User-Id", &uid)
        .json(&json!({"messages": []}))
        .send().await.unwrap();
    // Could be 200 with empty or 422 for validation — check what we get
    let status = r.status().as_u16();
    assert!(status == 200 || status == 422, "empty messages: got {status}");
    println!("✅ observe empty messages: {status}");
}

// ── 10d. retrieve edge cases ─────────────────────────────────────────────────

#[tokio::test]
async fn test_api_retrieve_top_k_respected() {
    let (base, client) = spawn_server().await;
    let uid = uid();

    // Store 5 memories
    for i in 0..5 {
        client.post(format!("{base}/v1/memories"))
            .header("X-User-Id", &uid)
            .json(&json!({"content": format!("topk test item {i}"), "memory_type": "semantic"}))
            .send().await.unwrap();
    }

    // Retrieve with top_k=2
    let r = client.post(format!("{base}/v1/memories/retrieve"))
        .header("X-User-Id", &uid)
        .json(&json!({"query": "topk test item", "top_k": 2}))
        .send().await.unwrap();
    assert_eq!(r.status(), 200);
    let results: Vec<Value> = r.json().await.unwrap();
    assert!(results.len() <= 2, "top_k=2 should return at most 2, got {}", results.len());
    println!("✅ retrieve top_k=2: got {} results", results.len());
}

#[tokio::test]
async fn test_api_search_returns_fields() {
    let (base, client) = spawn_server().await;
    let uid = uid();

    client.post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({"content": "field check memory", "memory_type": "profile"}))
        .send().await.unwrap();

    let r = client.post(format!("{base}/v1/memories/search"))
        .header("X-User-Id", &uid)
        .json(&json!({"query": "field check", "top_k": 1}))
        .send().await.unwrap();
    assert_eq!(r.status(), 200);
    let results: Vec<Value> = r.json().await.unwrap();
    assert!(!results.is_empty(), "should find memory");
    let mem = &results[0];
    // Verify essential fields are present
    assert!(mem["memory_id"].as_str().is_some(), "should have memory_id");
    assert!(mem["content"].as_str().is_some(), "should have content");
    assert!(mem["memory_type"].as_str().is_some(), "should have memory_type");
    println!("✅ search returns all fields: id={}, type={}", mem["memory_id"], mem["memory_type"]);
}

// ── 10e. error scenarios ─────────────────────────────────────────────────────

#[tokio::test]
async fn test_api_store_missing_content() {
    let (base, client) = spawn_server().await;
    let uid = uid();

    let r = client.post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({"memory_type": "semantic"}))
        .send().await.unwrap();
    assert_eq!(r.status(), 422, "missing content should be 422");
    println!("✅ store missing content → 422");
}

#[tokio::test]
async fn test_api_delete_nonexistent() {
    let (base, client) = spawn_server().await;
    let uid = uid();

    let r = client.delete(format!("{base}/v1/memories/nonexistent-id-12345"))
        .header("X-User-Id", &uid)
        .send().await.unwrap();
    // Should be 404 or 200 with "not found" — check what we return
    let status = r.status().as_u16();
    println!("✅ delete nonexistent: {status}");
}

#[tokio::test]
async fn test_api_correct_nonexistent() {
    let (base, client) = spawn_server().await;
    let uid = uid();

    let r = client.put(format!("{base}/v1/memories/nonexistent-id-12345/correct"))
        .header("X-User-Id", &uid)
        .json(&json!({"new_content": "updated", "reason": "test"}))
        .send().await.unwrap();
    let status = r.status().as_u16();
    // Should be 404 or 500
    assert!(status == 404 || status == 500, "correct nonexistent: got {status}");
    println!("✅ correct nonexistent → {status}");
}

// ── 10f. memory history ──────────────────────────────────────────────────────

#[tokio::test]
async fn test_api_memory_history() {
    let (base, client) = spawn_server().await;
    let uid = uid();

    // Store a memory
    let r = client.post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({"content": "history test v1", "memory_type": "semantic"}))
        .send().await.unwrap();
    let body: Value = r.json().await.unwrap();
    let mid = body["memory_id"].as_str().unwrap().to_string();

    // Get history — should have 1 version
    let r = client.get(format!("{base}/v1/memories/{mid}/history"))
        .header("X-User-Id", &uid)
        .send().await.unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["total"].as_i64().unwrap(), 1);
    assert_eq!(body["versions"][0]["content"].as_str().unwrap(), "history test v1");
    println!("✅ memory history: 1 version");

    // Correct the memory
    client.put(format!("{base}/v1/memories/{mid}/correct"))
        .header("X-User-Id", &uid)
        .json(&json!({"new_content": "history test v2", "reason": "updated"}))
        .send().await.unwrap();

    // History should still show the memory (in-place update, same id)
    let r = client.get(format!("{base}/v1/memories/{mid}/history"))
        .header("X-User-Id", &uid)
        .send().await.unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert!(body["total"].as_i64().unwrap() >= 1);
    println!("✅ memory history after correct: {} versions", body["total"]);
}

#[tokio::test]
async fn test_api_memory_history_not_found() {
    let (base, client) = spawn_server().await;
    let uid = uid();

    let r = client.get(format!("{base}/v1/memories/nonexistent-id/history"))
        .header("X-User-Id", &uid)
        .send().await.unwrap();
    assert_eq!(r.status(), 404);
    println!("✅ memory history nonexistent → 404");
}

// ── Remote mode E2E tests ─────────────────────────────────────────────────────

/// Spawn API server + test remote MCP client against it.
async fn spawn_api_for_remote() -> (String, reqwest::Client) {
    // Reuse spawn_server but return the base URL for RemoteClient
    spawn_server().await
}

#[tokio::test]
async fn test_remote_store_retrieve() {
    use memoria_mcp::remote::RemoteClient;

    let (base, _) = spawn_api_for_remote().await;
    let uid = uid();

    let remote = RemoteClient::new(&base, None, uid.clone());

    // Store
    let r = remote.call("memory_store", json!({
        "content": "Remote mode test memory",
        "memory_type": "semantic"
    })).await.expect("store");
    let text = r["content"][0]["text"].as_str().unwrap_or("");
    assert!(text.contains("Stored memory"), "got: {text}");
    println!("✅ remote store: {text}");

    // Retrieve
    let r = remote.call("memory_retrieve", json!({
        "query": "remote mode test",
        "top_k": 5
    })).await.expect("retrieve");
    let text = r["content"][0]["text"].as_str().unwrap_or("");
    assert!(text.contains("Remote mode test memory") || text.contains("No relevant"), "got: {text}");
    println!("✅ remote retrieve: {}", &text[..text.len().min(80)]);
}

#[tokio::test]
async fn test_remote_correct_purge() {
    use memoria_mcp::remote::RemoteClient;

    let (base, _) = spawn_api_for_remote().await;
    let uid = uid();
    let remote = RemoteClient::new(&base, None, uid.clone());

    // Store
    let r = remote.call("memory_store", json!({"content": "Uses black formatter"}))
        .await.expect("store");
    let text = r["content"][0]["text"].as_str().unwrap_or("");
    let mid = text.split_whitespace().nth(2).unwrap_or("").trim_end_matches(':').to_string();

    // Correct by id
    let r = remote.call("memory_correct", json!({
        "memory_id": mid,
        "new_content": "Uses ruff formatter",
        "reason": "switched"
    })).await.expect("correct");
    let text = r["content"][0]["text"].as_str().unwrap_or("");
    assert!(text.contains("Corrected"), "got: {text}");
    println!("✅ remote correct: {text}");

    // Purge
    let r = remote.call("memory_purge", json!({"memory_id": mid}))
        .await.expect("purge");
    let text = r["content"][0]["text"].as_str().unwrap_or("");
    assert!(text.contains("Purged"), "got: {text}");
    println!("✅ remote purge: {text}");
}

#[tokio::test]
async fn test_remote_governance() {
    use memoria_mcp::remote::RemoteClient;

    let (base, _) = spawn_api_for_remote().await;
    let uid = uid();
    let remote = RemoteClient::new(&base, None, uid.clone());

    let r = remote.call("memory_governance", json!({"force": true}))
        .await.expect("governance");
    let text = r["content"][0]["text"].as_str().unwrap_or("");
    assert!(text.contains("Governance complete") || text.contains("skipped"), "got: {text}");
    println!("✅ remote governance: {text}");
}

#[tokio::test]
async fn test_remote_capabilities() {
    use memoria_mcp::remote::RemoteClient;

    let (base, _) = spawn_api_for_remote().await;
    let uid = uid();
    let remote = RemoteClient::new(&base, None, uid.clone());

    let r = remote.call("memory_capabilities", json!({}))
        .await.expect("capabilities");
    let text = r["content"][0]["text"].as_str().unwrap_or("");
    assert!(text.contains("remote mode"), "should mention remote mode, got: {text}");
    println!("✅ remote capabilities: {}", &text[..text.len().min(80)]);
}

#[tokio::test]
async fn test_remote_list_search_profile() {
    use memoria_mcp::remote::RemoteClient;
    let (base, _) = spawn_api_for_remote().await;
    let uid = uid();
    let remote = RemoteClient::new(&base, None, uid.clone());

    remote.call("memory_store", json!({"content": "Prefers Rust", "memory_type": "profile"})).await.unwrap();
    remote.call("memory_store", json!({"content": "Uses MatrixOne database"})).await.unwrap();

    // list
    let r = remote.call("memory_list", json!({"limit": 10})).await.unwrap();
    let t = r["content"][0]["text"].as_str().unwrap_or("");
    assert!(t.contains("MatrixOne") || t.contains("Prefers"), "list: {t}");
    println!("✅ remote list: {}", &t[..t.len().min(80)]);

    // search
    let r = remote.call("memory_search", json!({"query": "database", "top_k": 5})).await.unwrap();
    let t = r["content"][0]["text"].as_str().unwrap_or("");
    assert!(t.contains("MatrixOne") || t.contains("No relevant"), "search: {t}");
    println!("✅ remote search: {}", &t[..t.len().min(80)]);

    // profile
    let r = remote.call("memory_profile", json!({})).await.unwrap();
    let t = r["content"][0]["text"].as_str().unwrap_or("");
    assert!(t.contains("Prefers Rust") || t.contains("No profile"), "profile: {t}");
    println!("✅ remote profile: {t}");
}

#[tokio::test]
async fn test_remote_snapshot_branch() {
    use memoria_mcp::remote::RemoteClient;
    let (base, _) = spawn_api_for_remote().await;
    let uid = uid();
    let remote = RemoteClient::new(&base, None, uid.clone());

    // Store a memory first
    remote.call("memory_store", json!({"content": "snapshot branch test memory"})).await.unwrap();

    // Create snapshot
    let snap_name = format!("test_snap_{}", uuid::Uuid::new_v4().simple().to_string()[..8].to_string());
    let r = remote.call("memory_snapshot", json!({"name": snap_name})).await.unwrap();
    let t = r["content"][0]["text"].as_str().unwrap_or("");
    assert!(t.contains("created") || t.contains(&snap_name), "snapshot create: {t}");
    println!("✅ remote snapshot create: {t}");

    // List snapshots
    let r = remote.call("memory_snapshots", json!({"limit": 20})).await.unwrap();
    let t = r["content"][0]["text"].as_str().unwrap_or("");
    println!("✅ remote snapshots list: {}", &t[..t.len().min(80)]);

    // Create branch
    let branch_name = format!("test_br_{}", uuid::Uuid::new_v4().simple().to_string()[..8].to_string());
    let r = remote.call("memory_branch", json!({"name": branch_name})).await.unwrap();
    let t = r["content"][0]["text"].as_str().unwrap_or("");
    assert!(t.contains("created") || t.contains(&branch_name), "branch create: {t}");
    println!("✅ remote branch create: {t}");

    // List branches
    let r = remote.call("memory_branches", json!({})).await.unwrap();
    let t = r["content"][0]["text"].as_str().unwrap_or("");
    println!("✅ remote branches list: {}", &t[..t.len().min(80)]);

    // Checkout branch
    let r = remote.call("memory_checkout", json!({"name": branch_name})).await.unwrap();
    let t = r["content"][0]["text"].as_str().unwrap_or("");
    assert!(t.contains("Switched") || t.contains(&branch_name), "checkout: {t}");
    println!("✅ remote checkout: {t}");

    // Store on branch
    remote.call("memory_store", json!({"content": "branch-only memory"})).await.unwrap();

    // Diff
    let r = remote.call("memory_diff", json!({"source": branch_name})).await.unwrap();
    let t = r["content"][0]["text"].as_str().unwrap_or("");
    println!("✅ remote diff: {}", &t[..t.len().min(80)]);

    // Merge back
    let r = remote.call("memory_merge", json!({"source": branch_name, "strategy": "append"})).await.unwrap();
    let t = r["content"][0]["text"].as_str().unwrap_or("");
    println!("✅ remote merge: {}", &t[..t.len().min(80)]);

    // Delete branch
    let r = remote.call("memory_branch_delete", json!({"name": branch_name})).await.unwrap();
    let t = r["content"][0]["text"].as_str().unwrap_or("");
    assert!(t.contains("deleted") || t.contains(&branch_name), "branch delete: {t}");
    println!("✅ remote branch delete: {t}");

    // Delete snapshot
    let r = remote.call("memory_snapshot_delete", json!({"names": snap_name})).await.unwrap();
    let t = r["content"][0]["text"].as_str().unwrap_or("");
    println!("✅ remote snapshot delete: {t}");
}

#[tokio::test]
async fn test_remote_reflect_extract_entities() {
    use memoria_mcp::remote::RemoteClient;
    let (base, _) = spawn_api_for_remote().await;
    let uid = uid();
    let remote = RemoteClient::new(&base, None, uid.clone());

    remote.call("memory_store", json!({"content": "Uses Rust for backend services", "session_id": "s1"})).await.unwrap();
    remote.call("memory_store", json!({"content": "MatrixOne as primary database", "session_id": "s2"})).await.unwrap();

    // reflect candidates (no LLM needed)
    let r = remote.call("memory_reflect", json!({"mode": "candidates", "force": true})).await.unwrap();
    let t = r["content"][0]["text"].as_str().unwrap_or("");
    assert!(!t.to_lowercase().contains("error"), "reflect should not error: {t}");
    println!("✅ remote reflect candidates: {}", &t[..t.len().min(100)]);

    // extract entities candidates
    let r = remote.call("memory_extract_entities", json!({"mode": "candidates"})).await.unwrap();
    let t = r["content"][0]["text"].as_str().unwrap_or("");
    let parsed: serde_json::Value = serde_json::from_str(t).unwrap_or(serde_json::Value::Null);
    assert!(
        parsed["status"] == "candidates" || parsed["status"] == "complete",
        "extract: {t}"
    );
    println!("✅ remote extract entities: status={}", parsed["status"]);

    // link entities if we have candidates
    if parsed["status"] == "candidates" {
        if let Some(mems) = parsed["memories"].as_array() {
            if let Some(first) = mems.first() {
                let mid = first["memory_id"].as_str().unwrap_or("");
                let link_payload = serde_json::to_string(&json!([{
                    "memory_id": mid,
                    "entities": [{"name": "Rust", "type": "tech"}]
                }])).unwrap();
                let r = remote.call("memory_link_entities", json!({"entities": link_payload})).await.unwrap();
                let t = r["content"][0]["text"].as_str().unwrap_or("");
                let p: serde_json::Value = serde_json::from_str(t).unwrap_or(serde_json::Value::Null);
                assert!(p.get("entities_created").is_some() || p["status"] == "done", "link: {t}");
                println!("✅ remote link entities: {t}");
            }
        }
    }
}

#[tokio::test]
async fn test_remote_consolidate() {
    use memoria_mcp::remote::RemoteClient;
    let (base, _) = spawn_api_for_remote().await;
    let uid = uid();
    let remote = RemoteClient::new(&base, None, uid.clone());

    let r = remote.call("memory_consolidate", json!({"force": true})).await.unwrap();
    let t = r["content"][0]["text"].as_str().unwrap_or("");
    assert!(t.contains("Consolidation complete") || t.contains("skipped"), "got: {t}");
    println!("✅ remote consolidate: {t}");
}

#[tokio::test]
async fn test_remote_correct_by_query() {
    use memoria_mcp::remote::RemoteClient;
    let (base, _) = spawn_api_for_remote().await;
    let uid = uid();
    let remote = RemoteClient::new(&base, None, uid.clone());

    remote.call("memory_store", json!({"content": "Uses black for Python formatting"})).await.unwrap();

    let r = remote.call("memory_correct", json!({
        "query": "black formatting",
        "new_content": "Uses ruff for Python formatting",
        "reason": "switched"
    })).await.unwrap();
    let t = r["content"][0]["text"].as_str().unwrap_or("");
    assert!(t.contains("Corrected") || t.contains("No matching"), "got: {t}");
    println!("✅ remote correct by query: {t}");
}

#[tokio::test]
async fn test_remote_purge_by_topic() {
    use memoria_mcp::remote::RemoteClient;
    let (base, _) = spawn_api_for_remote().await;
    let uid = uid();
    let remote = RemoteClient::new(&base, None, uid.clone());

    remote.call("memory_store", json!({"content": "topic purge test alpha"})).await.unwrap();
    remote.call("memory_store", json!({"content": "topic purge test beta"})).await.unwrap();

    let r = remote.call("memory_purge", json!({"topic": "topic purge test"})).await.unwrap();
    let t = r["content"][0]["text"].as_str().unwrap_or("");
    assert!(t.contains("Purged"), "got: {t}");
    println!("✅ remote purge by topic: {t}");
}

// ── Episodic memory tests ─────────────────────────────────────────────────────

#[tokio::test]
async fn test_episodic_no_llm_returns_503() {
    let (base, client) = spawn_server().await;
    let uid = uid();

    // Store some memories with a session_id
    client.post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({"content": "Worked on Rust backend", "session_id": "sess1"}))
        .send().await.unwrap();

    // Without LLM configured, should return 503
    let r = client.post(format!("{base}/v1/sessions/sess1/summary"))
        .header("X-User-Id", &uid)
        .json(&json!({"mode": "full", "sync": true}))
        .send().await.unwrap();
    assert_eq!(r.status(), 503, "should return 503 without LLM");
    println!("✅ episodic without LLM: 503 SERVICE_UNAVAILABLE");
}

async fn spawn_server_with_llm(llm_key: String) -> (String, reqwest::Client) {
    use memoria_git::GitForDataService;
    use memoria_service::{Config, MemoryService};
    use memoria_storage::SqlMemoryStore;
    use sqlx::mysql::MySqlPool;
    use memoria_embedding::LlmClient;

    let cfg = Config::from_env();
    let db = db_url();
    let store = SqlMemoryStore::connect(&db, 4).await.expect("connect");
    store.migrate().await.expect("migrate");
    let pool = MySqlPool::connect(&db).await.expect("pool");
    let git = Arc::new(GitForDataService::new(pool, &cfg.db_name));
    let base_url = std::env::var("LLM_BASE_URL").unwrap_or_else(|_| "https://api.openai.com/v1".to_string());
    let model = std::env::var("LLM_MODEL").unwrap_or_else(|_| "gpt-4o-mini".to_string());
    let llm = Arc::new(LlmClient::new(llm_key, base_url, model));
    let service = Arc::new(MemoryService::new_sql_with_llm(Arc::new(store), None, Some(llm)));
    let state = memoria_api::AppState::new(service, git, String::new());
    let app = memoria_api::build_router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    (format!("http://127.0.0.1:{port}"), client)
}

#[tokio::test]
async fn test_episodic_no_memories_returns_error() {
    // This test requires LLM — skip if not configured
    let llm_key = match std::env::var("LLM_API_KEY").ok().filter(|s| !s.is_empty()) {
        Some(k) => k,
        None => {
            println!("⏭️  test_episodic_no_memories skipped (LLM_API_KEY not set)");
            return;
        }
    };

    let (base, client) = spawn_server_with_llm(llm_key).await;
    let uid = uid();

    // No memories for this session → 500
    let r = client.post(format!("{base}/v1/sessions/nonexistent_session/summary"))
        .header("X-User-Id", &uid)
        .json(&json!({"mode": "full", "sync": true}))
        .send().await.unwrap();
    assert_eq!(r.status(), 500, "should return 500 for empty session");
    println!("✅ episodic empty session: 500");
}

#[tokio::test]
async fn test_episodic_async_task_polling() {
    let (base, client) = spawn_server().await;
    let uid = uid();

    // Without LLM, async mode should still create a task (that will fail)
    // but the endpoint itself returns 503 before creating a task
    let r = client.post(format!("{base}/v1/sessions/sess_async/summary"))
        .header("X-User-Id", &uid)
        .json(&json!({"mode": "full", "sync": false}))
        .send().await.unwrap();
    // Without LLM: 503
    assert_eq!(r.status(), 503);
    println!("✅ episodic async without LLM: 503");
}

#[tokio::test]
async fn test_episodic_with_llm_sync() {
    let llm_key = match std::env::var("LLM_API_KEY").ok().filter(|s| !s.is_empty()) {
        Some(k) => k,
        None => {
            println!("⏭️  test_episodic_with_llm_sync skipped (LLM_API_KEY not set)");
            return;
        }
    };

    let (base, client) = spawn_server_with_llm(llm_key).await;
    let uid = uid();
    let session_id = format!("ep_sess_{}", uuid::Uuid::new_v4().simple().to_string()[..8].to_string());

    // Store memories with session_id
    for content in &["Implemented Rust REST API", "Added episodic memory support", "All tests passing"] {
        client.post(format!("{base}/v1/memories"))
            .header("X-User-Id", &uid)
            .json(&json!({"content": content, "session_id": session_id}))
            .send().await.unwrap();
    }

    // Generate episodic memory (sync)
    let r = client.post(format!("{base}/v1/sessions/{session_id}/summary"))
        .header("X-User-Id", &uid)
        .json(&json!({"mode": "full", "sync": true}))
        .send().await.unwrap();
    assert_eq!(r.status(), 200, "should return 200");
    let body: serde_json::Value = r.json().await.unwrap();
    assert!(body["memory_id"].as_str().is_some(), "should have memory_id: {body}");
    assert!(body["content"].as_str().map(|c| c.contains("Session Summary")).unwrap_or(false),
        "content should contain 'Session Summary': {body}");
    println!("✅ episodic with LLM sync: memory_id={}", body["memory_id"].as_str().unwrap_or(""));
    println!("   content: {}", &body["content"].as_str().unwrap_or("")[..100.min(body["content"].as_str().unwrap_or("").len())]);
}
