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
    use axum::{routing::{delete, get, post, put}, Router};
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
    let state = memoria_api::AppState { service, git, master_key: String::new() };

    let app = Router::new()
        .route("/health", get(memoria_api::routes::memory::health))
        .route("/v1/memories", get(memoria_api::routes::memory::list_memories))
        .route("/v1/memories", post(memoria_api::routes::memory::store_memory))
        .route("/v1/memories/batch", post(memoria_api::routes::memory::batch_store))
        .route("/v1/memories/retrieve", post(memoria_api::routes::memory::retrieve))
        .route("/v1/memories/search", post(memoria_api::routes::memory::search))
        .route("/v1/memories/correct", post(memoria_api::routes::memory::correct_by_query))
        .route("/v1/memories/purge", post(memoria_api::routes::memory::purge_memories))
        .route("/v1/memories/:id", get(memoria_api::routes::memory::get_memory))
        .route("/v1/memories/:id/correct", put(memoria_api::routes::memory::correct_memory))
        .route("/v1/memories/:id", delete(memoria_api::routes::memory::delete_memory))
        .route("/v1/profiles", get(memoria_api::routes::memory::get_profile))
        .route("/v1/governance", post(memoria_api::routes::governance::governance))
        .route("/v1/consolidate", post(memoria_api::routes::governance::consolidate))
        .route("/v1/snapshots", get(memoria_api::routes::snapshots::list_snapshots))
        .route("/v1/snapshots", post(memoria_api::routes::snapshots::create_snapshot))
        .route("/v1/snapshots/:name", delete(memoria_api::routes::snapshots::delete_snapshot))
        .route("/v1/snapshots/:name/rollback", post(memoria_api::routes::snapshots::rollback))
        .route("/v1/branches", get(memoria_api::routes::snapshots::list_branches))
        .route("/v1/branches", post(memoria_api::routes::snapshots::create_branch))
        .route("/v1/branches/:name/checkout", post(memoria_api::routes::snapshots::checkout_branch))
        .route("/v1/branches/:name/merge", post(memoria_api::routes::snapshots::merge_branch))
        .route("/v1/branches/:name/diff", get(memoria_api::routes::snapshots::diff_branch))
        .route("/v1/branches/:name", delete(memoria_api::routes::snapshots::delete_branch))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().unwrap().port();
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await
    });

    // Give server time to start
    tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;

    // Check if server task panicked
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

// ── 10. auth: missing token returns 401 ──────────────────────────────────────

#[tokio::test]
async fn test_api_auth_required() {
    use axum::{routing::get, Router};
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
    // Set a master key to enable auth
    let state = memoria_api::AppState {
        service, git,
        master_key: "test-master-key-12345".to_string(),
    };

    let app = Router::new()
        .route("/v1/memories", get(memoria_api::routes::memory::list_memories))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let base = format!("http://127.0.0.1:{port}");

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
        .header("Authorization", "Bearer test-master-key-12345")
        .send().await.unwrap();
    assert_eq!(r.status(), 200);

    println!("✅ Auth: 401 without token, 200 with correct token");
}
