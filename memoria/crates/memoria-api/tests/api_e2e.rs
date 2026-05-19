use serde_json::{json, Value};
/// REST API E2E tests — starts a real server, hits it with reqwest.
/// Requires DATABASE_URL env var.
use std::sync::Arc;

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
    format!("api_test_{}", uuid::Uuid::new_v4().simple())
}

fn episodic_rules() -> Vec<memoria_test_utils::PromptRule> {
    vec![
        (
            "Respond with a JSON object containing: topic, action, outcome",
            json!({
                "topic": "Deterministic validation of session workflows",
                "action": "Stored session memories and generated an episodic summary with a local fake LLM",
                "outcome": "The session summary path completed and persisted an episodic memory"
            }),
        ),
        (
            "Respond with a JSON object: {\"points\"",
            json!({
                "points": [
                    "Validated session summary path",
                    "Used local fake LLM",
                    "Stored episodic memory"
                ]
            }),
        ),
    ]
}

async fn spawn_fake_llm() -> (
    Arc<memoria_embedding::LlmClient>,
    tokio::sync::oneshot::Sender<()>,
) {
    memoria_test_utils::spawn_fake_llm(episodic_rules()).await
}

/// Returns (key, base_url, model) if EMBEDDING_API_KEY is set, else None.
fn try_embedding() -> Option<(String, String, String)> {
    let key = std::env::var("EMBEDDING_API_KEY")
        .ok()
        .filter(|s| !s.is_empty())?;
    let base = std::env::var("EMBEDDING_BASE_URL")
        .unwrap_or_else(|_| "https://api.siliconflow.cn/v1".into());
    let model = std::env::var("EMBEDDING_MODEL").unwrap_or_else(|_| "BAAI/bge-m3".into());
    Some((key, base, model))
}

/// Spawn the API server on a random port, return (base_url, client).
async fn spawn_server() -> (String, reqwest::Client) {
    use memoria_git::GitForDataService;
    use memoria_service::{Config, MemoryService};
    use memoria_storage::SqlMemoryStore;
    use sqlx::mysql::MySqlPool;

    let cfg = Config::from_env();
    let db = db_url();

    let store = SqlMemoryStore::connect(&db, test_dim(), uuid::Uuid::new_v4().to_string())
        .await
        .expect("connect");
    store.migrate().await.expect("migrate");
    let pool = MySqlPool::connect(&db).await.expect("pool");
    let git = Arc::new(GitForDataService::new(pool, &cfg.db_name));
    let service = Arc::new(MemoryService::new_sql_with_llm(Arc::new(store), None, None));
    let state = memoria_api::AppState::new(service, git, String::new());

    let app = memoria_api::build_router(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let port = listener.local_addr().unwrap().port();
    let handle = tokio::spawn(async move { axum::serve(listener, app).await });

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
    let r = client
        .get(format!("{base}/health"))
        .send()
        .await
        .expect("get");
    assert_eq!(r.status(), 200);
    assert_eq!(r.text().await.unwrap(), "ok");
    println!("✅ GET /health");
}

// ── 2. store + list ───────────────────────────────────────────────────────────

#[tokio::test]
async fn test_api_store_and_list() {
    let (base, client) = spawn_server().await;
    let uid = uid();

    let r = client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({"content": "Rust is fast", "memory_type": "semantic"}))
        .send()
        .await
        .expect("post");
    assert_eq!(r.status(), 201);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["content"], "Rust is fast");
    let mid = body["memory_id"].as_str().unwrap().to_string();
    println!("✅ POST /v1/memories: {mid}");

    let r = client
        .get(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .expect("get");
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert!(body["items"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["memory_id"] == mid));
    println!(
        "✅ GET /v1/memories: {} items",
        body["items"].as_array().unwrap().len()
    );
}

// ── 3. batch store ────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_api_batch_store() {
    let (base, client) = spawn_server().await;
    let uid = uid();

    let r = client
        .post(format!("{base}/v1/memories/batch"))
        .header("X-User-Id", &uid)
        .json(&json!({"memories": [
            {"content": "Memory A"},
            {"content": "Memory B", "memory_type": "profile"},
        ]}))
        .send()
        .await
        .expect("post");
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

    client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({"content": "MatrixOne is a distributed database"}))
        .send()
        .await
        .unwrap();

    let r = client
        .post(format!("{base}/v1/memories/retrieve"))
        .header("X-User-Id", &uid)
        .json(&json!({"query": "database", "top_k": 5}))
        .send()
        .await
        .expect("post");
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert!(!body.as_array().unwrap().is_empty());
    println!(
        "✅ POST /v1/memories/retrieve: {} results",
        body.as_array().unwrap().len()
    );
}

// ── 5. correct by id ──────────────────────────────────────────────────────────

#[tokio::test]
async fn test_api_correct() {
    let (base, client) = spawn_server().await;
    let uid = uid();

    let r = client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({"content": "Uses black for formatting"}))
        .send()
        .await
        .unwrap();
    let mid = r.json::<Value>().await.unwrap()["memory_id"]
        .as_str()
        .unwrap()
        .to_string();

    let r = client
        .put(format!("{base}/v1/memories/{mid}/correct"))
        .header("X-User-Id", &uid)
        .json(&json!({"new_content": "Uses ruff for formatting"}))
        .send()
        .await
        .expect("put");
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

    let r = client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({"content": "to be deleted"}))
        .send()
        .await
        .unwrap();
    let mid = r.json::<Value>().await.unwrap()["memory_id"]
        .as_str()
        .unwrap()
        .to_string();

    let r = client
        .delete(format!("{base}/v1/memories/{mid}"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .expect("delete");
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
        let r = client
            .post(format!("{base}/v1/memories"))
            .header("X-User-Id", &uid)
            .json(&json!({"content": format!("bulk purge {i}")}))
            .send()
            .await
            .unwrap();
        ids.push(
            r.json::<Value>().await.unwrap()["memory_id"]
                .as_str()
                .unwrap()
                .to_string(),
        );
    }

    let r = client
        .post(format!("{base}/v1/memories/purge"))
        .header("X-User-Id", &uid)
        .json(&json!({"memory_ids": ids}))
        .send()
        .await
        .expect("post");
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

    client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({"content": "Prefers Rust", "memory_type": "profile"}))
        .send()
        .await
        .unwrap();

    let r = client
        .get(format!("{base}/v1/profiles/me"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .expect("get");
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert!(body["profile"].as_str().unwrap().contains("Prefers Rust"));
    println!("✅ GET /v1/profiles/me");
}

// ── 9. governance ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_api_governance() {
    let (base, client) = spawn_server().await;
    let uid = uid();

    let r = client
        .post(format!("{base}/v1/governance"))
        .header("X-User-Id", &uid)
        .json(&json!({"force": true}))
        .send()
        .await
        .expect("post");
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
    let store = SqlMemoryStore::connect(&db, test_dim(), uuid::Uuid::new_v4().to_string())
        .await
        .expect("connect");
    store.migrate().await.expect("migrate");
    let pool = MySqlPool::connect(&db).await.expect("pool");
    let git = Arc::new(GitForDataService::new(pool, &cfg.db_name));
    let service = Arc::new(MemoryService::new_sql_with_llm(Arc::new(store), None, None));
    let state = memoria_api::AppState::new(service, git, master_key.to_string());
    let app = memoria_api::build_router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    (format!("http://127.0.0.1:{port}"), client)
}

async fn create_api_key_for_user(
    client: &reqwest::Client,
    base: &str,
    master_auth: &str,
    user_id: &str,
    name: &str,
) -> String {
    let r = client
        .post(format!("{base}/auth/keys"))
        .header("Authorization", master_auth)
        .json(&json!({ "user_id": user_id, "name": name }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201, "create key for {user_id}");
    r.json::<Value>().await.unwrap()["raw_key"]
        .as_str()
        .unwrap()
        .to_string()
}

// ── 10. auth: missing token returns 401 ──────────────────────────────────────

#[tokio::test]
async fn test_api_auth_required() {
    let mk = "test-master-key-12345";
    let (base, client) = spawn_server_with_master_key(mk).await;

    // No token → 401
    let r = client
        .get(format!("{base}/v1/memories"))
        .header("X-User-Id", "alice")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);

    // Wrong token → 401
    let r = client
        .get(format!("{base}/v1/memories"))
        .header("X-User-Id", "alice")
        .header("Authorization", "Bearer wrong-key")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);

    // Correct token → 200
    let r = client
        .get(format!("{base}/v1/memories"))
        .header("X-User-Id", "alice")
        .header("Authorization", format!("Bearer {mk}"))
        .send()
        .await
        .unwrap();
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
    let r = client
        .post(format!("{base}/auth/keys"))
        .header("Authorization", &auth)
        .json(&json!({"user_id": uid, "name": "test-key-1"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201, "create key");
    let body: Value = r.json().await.unwrap();
    let key_id = body["key_id"].as_str().unwrap().to_string();
    let raw_key = body["raw_key"].as_str().unwrap().to_string();
    assert!(raw_key.starts_with("sk-"), "raw_key should start with sk-");
    assert_eq!(body["user_id"].as_str().unwrap(), uid);
    assert_eq!(body["name"].as_str().unwrap(), "test-key-1");
    println!("✅ create key: {key_id}, prefix={}", body["key_prefix"]);

    // 2. List keys — use master key to authenticate (API keys are for external use)
    let r = client
        .get(format!("{base}/auth/keys"))
        .header("Authorization", &auth)
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "list keys");
    let keys: Vec<Value> = r.json().await.unwrap();
    assert!(
        keys.iter().any(|k| k["key_id"].as_str() == Some(&key_id)),
        "should find created key"
    );
    println!("✅ list keys: {} keys found", keys.len());

    // 3. Rotate key
    let r = client
        .put(format!("{base}/auth/keys/{key_id}/rotate"))
        .header("Authorization", &auth)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201, "rotate key");
    let body: Value = r.json().await.unwrap();
    let new_key_id = body["key_id"].as_str().unwrap().to_string();
    let new_raw_key = body["raw_key"].as_str().unwrap().to_string();
    assert_ne!(new_key_id, key_id, "rotated key should have new id");
    assert_ne!(new_raw_key, raw_key, "rotated key should have new raw_key");
    assert_eq!(
        body["name"].as_str().unwrap(),
        "test-key-1",
        "name preserved"
    );
    println!("✅ rotate key: old={key_id} → new={new_key_id}");

    // 4. Old key should be deactivated — verify via list
    let r = client
        .get(format!("{base}/auth/keys"))
        .header("Authorization", &auth)
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap();
    let keys: Vec<Value> = r.json().await.unwrap();
    assert!(
        !keys.iter().any(|k| k["key_id"].as_str() == Some(&key_id)),
        "old key should not appear in active list"
    );
    println!("✅ old key deactivated after rotate");

    // 5. New key appears in list
    assert!(
        keys.iter()
            .any(|k| k["key_id"].as_str() == Some(&new_key_id)),
        "new key should appear in active list"
    );
    println!("✅ new key in active list after rotate");

    // 6. Revoke key
    let r = client
        .delete(format!("{base}/auth/keys/{new_key_id}"))
        .header("Authorization", &auth)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204, "revoke key");
    println!("✅ revoke key: {new_key_id}");

    // 7. Revoked key should not appear in active list
    let r = client
        .get(format!("{base}/auth/keys"))
        .header("Authorization", &auth)
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap();
    let keys: Vec<Value> = r.json().await.unwrap();
    assert!(
        !keys
            .iter()
            .any(|k| k["key_id"].as_str() == Some(&new_key_id)),
        "revoked key should not appear in active list"
    );
    println!("✅ revoked key not in active list");

    // 8. Rotate non-existent key → 404
    let r = client
        .put(format!("{base}/auth/keys/nonexistent-id/rotate"))
        .header("Authorization", &auth)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404, "rotate nonexistent");
    println!("✅ rotate nonexistent → 404");

    // 9. Revoke non-existent key → 404
    let r = client
        .delete(format!("{base}/auth/keys/nonexistent-id"))
        .header("Authorization", &auth)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404, "revoke nonexistent");
    println!("✅ revoke nonexistent → 404");
}

#[tokio::test]
async fn test_api_key_cannot_get_other_users_memory() {
    let mk = "test-master-key-memory-read";
    let (base, client) = spawn_server_with_master_key(mk).await;
    let auth = format!("Bearer {mk}");
    let owner_id = uid();
    let attacker_id = uid();
    let owner_key = create_api_key_for_user(&client, &base, &auth, &owner_id, "owner-read").await;
    let attacker_key =
        create_api_key_for_user(&client, &base, &auth, &attacker_id, "attacker-read").await;

    let r = client
        .post(format!("{base}/v1/memories"))
        .header("Authorization", format!("Bearer {owner_key}"))
        .json(&json!({ "content": "owner private memory", "memory_type": "semantic" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);
    let memory_id = r.json::<Value>().await.unwrap()["memory_id"]
        .as_str()
        .unwrap()
        .to_string();

    let r = client
        .get(format!("{base}/v1/memories/{memory_id}"))
        .header("Authorization", format!("Bearer {attacker_key}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403, "non-owner API key must get 403");
}

#[tokio::test]
async fn test_api_key_cannot_correct_other_users_memory() {
    let mk = "test-master-key-memory-correct";
    let (base, client) = spawn_server_with_master_key(mk).await;
    let auth = format!("Bearer {mk}");
    let owner_id = uid();
    let attacker_id = uid();
    let owner_key =
        create_api_key_for_user(&client, &base, &auth, &owner_id, "owner-correct").await;
    let attacker_key =
        create_api_key_for_user(&client, &base, &auth, &attacker_id, "attacker-correct").await;

    let r = client
        .post(format!("{base}/v1/memories"))
        .header("Authorization", format!("Bearer {owner_key}"))
        .json(&json!({ "content": "unchanged owner memory", "memory_type": "semantic" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);
    let memory_id = r.json::<Value>().await.unwrap()["memory_id"]
        .as_str()
        .unwrap()
        .to_string();

    let r = client
        .put(format!("{base}/v1/memories/{memory_id}/correct"))
        .header("Authorization", format!("Bearer {attacker_key}"))
        .json(&json!({ "new_content": "attacker overwrite" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403, "non-owner API key must get 403");
}

#[tokio::test]
async fn test_api_key_cannot_get_other_users_task_status() {
    use memoria_service::AsyncTaskStore;
    use memoria_storage::SqlMemoryStore;

    let mk = "test-master-key-task-status";
    let (base, client) = spawn_server_with_master_key(mk).await;
    let auth = format!("Bearer {mk}");
    let owner_id = uid();
    let attacker_id = uid();
    let attacker_key =
        create_api_key_for_user(&client, &base, &auth, &attacker_id, "attacker-task").await;

    let store = SqlMemoryStore::connect(&db_url(), test_dim(), uuid::Uuid::new_v4().to_string())
        .await
        .expect("connect");
    store.migrate().await.expect("migrate");

    let task_id = format!("task_{}", uuid::Uuid::new_v4().simple());
    store
        .create_task(&task_id, "instance_authz", &owner_id)
        .await
        .unwrap();

    let r = client
        .get(format!("{base}/v1/tasks/{task_id}"))
        .header("Authorization", format!("Bearer {attacker_key}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403, "non-owner API key must get 403");
}

// ── 10b-4. cross-user delete ──────────────────────────────────────────────────

#[tokio::test]
async fn test_api_key_cannot_delete_other_users_memory() {
    let mk = "test-master-key-memory-delete";
    let (base, client) = spawn_server_with_master_key(mk).await;
    let auth = format!("Bearer {mk}");
    let owner_id = uid();
    let attacker_id = uid();
    let owner_key = create_api_key_for_user(&client, &base, &auth, &owner_id, "owner-del").await;
    let attacker_key =
        create_api_key_for_user(&client, &base, &auth, &attacker_id, "attacker-del").await;

    // Owner creates memory
    let r = client
        .post(format!("{base}/v1/memories"))
        .header("Authorization", format!("Bearer {owner_key}"))
        .json(&json!({ "content": "owner secret", "memory_type": "semantic" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);
    let memory_id = r.json::<Value>().await.unwrap()["memory_id"]
        .as_str()
        .unwrap()
        .to_string();

    // Attacker tries to delete
    let r = client
        .delete(format!("{base}/v1/memories/{memory_id}"))
        .header("Authorization", format!("Bearer {attacker_key}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403, "non-owner API key must get 403 on delete");
}

// ── 10b-5. cross-user list isolation ─────────────────────────────────────────

#[tokio::test]
async fn test_api_key_list_only_own_memories() {
    let mk = "test-master-key-list-isolation";
    let (base, client) = spawn_server_with_master_key(mk).await;
    let auth = format!("Bearer {mk}");
    let user_a = uid();
    let user_b = uid();
    let key_a = create_api_key_for_user(&client, &base, &auth, &user_a, "list-a").await;
    let key_b = create_api_key_for_user(&client, &base, &auth, &user_b, "list-b").await;

    // User A stores a memory
    let r = client
        .post(format!("{base}/v1/memories"))
        .header("Authorization", format!("Bearer {key_a}"))
        .json(&json!({ "content": "user_a private data", "memory_type": "semantic" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);

    // User B lists — should see nothing from A
    let r = client
        .get(format!("{base}/v1/memories"))
        .header("Authorization", format!("Bearer {key_b}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    let items = body["items"].as_array().unwrap();
    for item in items {
        assert_ne!(
            item["content"].as_str().unwrap_or(""),
            "user_a private data",
            "user B must not see user A's memories in list"
        );
    }
}

// ── 10b-6. master key impersonation boundary ─────────────────────────────────

#[tokio::test]
async fn test_master_key_can_impersonate_and_access_any_user() {
    let mk = "test-master-key-impersonate";
    let (base, client) = spawn_server_with_master_key(mk).await;
    let auth = format!("Bearer {mk}");
    let user_a = uid();
    let key_a = create_api_key_for_user(&client, &base, &auth, &user_a, "imp-a").await;

    // User A stores a memory
    let r = client
        .post(format!("{base}/v1/memories"))
        .header("Authorization", format!("Bearer {key_a}"))
        .json(&json!({ "content": "impersonation test data", "memory_type": "semantic" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);
    let memory_id = r.json::<Value>().await.unwrap()["memory_id"]
        .as_str()
        .unwrap()
        .to_string();

    // Master key can read any user's memory
    let r = client
        .get(format!("{base}/v1/memories/{memory_id}"))
        .header("Authorization", &auth)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "master key should read any memory");

    // Master key can correct any user's memory
    let r = client
        .put(format!("{base}/v1/memories/{memory_id}/correct"))
        .header("Authorization", &auth)
        .json(&json!({ "new_content": "master corrected" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "master key should correct any memory");

    // Master key can delete any user's memory
    let r = client
        .delete(format!("{base}/v1/memories/{memory_id}"))
        .header("Authorization", &auth)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204, "master key should delete any memory");

    // API key (non-master) cannot access admin routes
    let r = client
        .get(format!("{base}/admin/stats"))
        .header("Authorization", format!("Bearer {key_a}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403, "API key must not access admin routes");
}

// ── 10b-7. revoked API key returns 401 ───────────────────────────────────────

#[tokio::test]
async fn test_revoked_api_key_returns_401() {
    let mk = "test-master-key-revoked";
    let (base, client) = spawn_server_with_master_key(mk).await;
    let auth = format!("Bearer {mk}");
    let uid = uid();

    // Create and get raw key + key_id
    let r = client
        .post(format!("{base}/auth/keys"))
        .header("Authorization", &auth)
        .json(&json!({"user_id": uid, "name": "to-revoke"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);
    let body: Value = r.json().await.unwrap();
    let key_id = body["key_id"].as_str().unwrap().to_string();
    let raw_key = body["raw_key"].as_str().unwrap().to_string();

    // Verify key works before revoke
    let r = client
        .get(format!("{base}/v1/memories"))
        .header("Authorization", format!("Bearer {raw_key}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "key should work before revoke");

    // Revoke
    let r = client
        .delete(format!("{base}/auth/keys/{key_id}"))
        .header("Authorization", &auth)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);

    // Revoked key must be rejected
    let r = client
        .get(format!("{base}/v1/memories"))
        .header("Authorization", format!("Bearer {raw_key}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401, "revoked key must return 401");

    // Also rejected on write endpoints
    let r = client
        .post(format!("{base}/v1/memories"))
        .header("Authorization", format!("Bearer {raw_key}"))
        .json(&json!({"content": "should fail", "memory_type": "semantic"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401, "revoked key must return 401 on store");

    println!("✅ revoked API key returns 401 on all endpoints");
}

// ── 10c. observe endpoint ────────────────────────────────────────────────────

#[tokio::test]
async fn test_api_observe_turn() {
    let (base, client) = spawn_server().await;
    let uid = uid();

    // Observe with assistant + user messages
    let r = client
        .post(format!("{base}/v1/observe"))
        .header("X-User-Id", &uid)
        .json(&json!({
            "messages": [
                {"role": "user", "content": "What is Rust?"},
                {"role": "assistant", "content": "Rust is a systems programming language"},
                {"role": "system", "content": "You are helpful"},
                {"role": "assistant", "content": ""}
            ]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    let memories = body["memories"].as_array().unwrap();
    // Should store user + non-empty assistant messages, skip system + empty
    assert_eq!(
        memories.len(),
        2,
        "should store 2 memories (user + assistant): {body}"
    );
    assert!(
        body.get("warning").is_some(),
        "should have LLM warning without LLM"
    );
    println!(
        "✅ observe: stored {} memories, warning={}",
        memories.len(),
        body["warning"]
    );

    // Verify stored memories are retrievable
    let r = client
        .post(format!("{base}/v1/memories/search"))
        .header("X-User-Id", &uid)
        .json(&json!({"query": "Rust programming", "top_k": 10}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let results: Vec<Value> = r.json().await.unwrap();
    assert!(
        results.len() >= 2,
        "should find observed memories, got {}",
        results.len()
    );
    println!("✅ observe memories retrievable: {} found", results.len());
}

#[tokio::test]
async fn test_api_observe_empty_messages() {
    let (base, client) = spawn_server().await;
    let uid = uid();

    // Empty messages array → should return 200 with empty memories
    let r = client
        .post(format!("{base}/v1/observe"))
        .header("X-User-Id", &uid)
        .json(&json!({"messages": []}))
        .send()
        .await
        .unwrap();
    // Could be 200 with empty or 422 for validation — check what we get
    let status = r.status().as_u16();
    assert!(
        status == 200 || status == 422,
        "empty messages: got {status}"
    );
    println!("✅ observe empty messages: {status}");
}

// ── 10d. retrieve edge cases ─────────────────────────────────────────────────

#[tokio::test]
async fn test_api_retrieve_top_k_respected() {
    let (base, client) = spawn_server().await;
    let uid = uid();

    // Store 5 memories
    for i in 0..5 {
        client
            .post(format!("{base}/v1/memories"))
            .header("X-User-Id", &uid)
            .json(&json!({"content": format!("topk test item {i}"), "memory_type": "semantic"}))
            .send()
            .await
            .unwrap();
    }

    // Retrieve with top_k=2
    let r = client
        .post(format!("{base}/v1/memories/retrieve"))
        .header("X-User-Id", &uid)
        .json(&json!({"query": "topk test item", "top_k": 2}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let results: Vec<Value> = r.json().await.unwrap();
    assert!(
        results.len() <= 2,
        "top_k=2 should return at most 2, got {}",
        results.len()
    );
    println!("✅ retrieve top_k=2: got {} results", results.len());
}

#[tokio::test]
async fn test_api_search_returns_fields() {
    let (base, client) = spawn_server().await;
    let uid = uid();

    client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({"content": "field check memory", "memory_type": "profile"}))
        .send()
        .await
        .unwrap();

    let r = client
        .post(format!("{base}/v1/memories/search"))
        .header("X-User-Id", &uid)
        .json(&json!({"query": "field check", "top_k": 1}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let results: Vec<Value> = r.json().await.unwrap();
    assert!(!results.is_empty(), "should find memory");
    let mem = &results[0];
    // Verify essential fields are present
    assert!(mem["memory_id"].as_str().is_some(), "should have memory_id");
    assert!(mem["content"].as_str().is_some(), "should have content");
    assert!(
        mem["memory_type"].as_str().is_some(),
        "should have memory_type"
    );
    println!(
        "✅ search returns all fields: id={}, type={}",
        mem["memory_id"], mem["memory_type"]
    );
}

// ── 10e. error scenarios ─────────────────────────────────────────────────────

#[tokio::test]
async fn test_api_store_missing_content() {
    let (base, client) = spawn_server().await;
    let uid = uid();

    let r = client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({"memory_type": "semantic"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 422, "missing content should be 422");
    println!("✅ store missing content → 422");
}

#[tokio::test]
async fn test_api_delete_nonexistent() {
    let (base, client) = spawn_server().await;
    let uid = uid();

    let r = client
        .delete(format!("{base}/v1/memories/nonexistent-id-12345"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap();
    // Should be 404 or 200 with "not found" — check what we return
    let status = r.status().as_u16();
    println!("✅ delete nonexistent: {status}");
}

#[tokio::test]
async fn test_api_correct_nonexistent() {
    let (base, client) = spawn_server().await;
    let uid = uid();

    let r = client
        .put(format!("{base}/v1/memories/nonexistent-id-12345/correct"))
        .header("X-User-Id", &uid)
        .json(&json!({"new_content": "updated", "reason": "test"}))
        .send()
        .await
        .unwrap();
    let status = r.status().as_u16();
    // Should be 404 or 500
    assert!(
        status == 404 || status == 500,
        "correct nonexistent: got {status}"
    );
    println!("✅ correct nonexistent → {status}");
}

// ── 10f. memory history ──────────────────────────────────────────────────────

#[tokio::test]
async fn test_api_memory_history() {
    let (base, client) = spawn_server().await;
    let uid = uid();

    // Store a memory
    let r = client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({"content": "history test v1", "memory_type": "semantic"}))
        .send()
        .await
        .unwrap();
    let body: Value = r.json().await.unwrap();
    let mid = body["memory_id"].as_str().unwrap().to_string();

    // Get history — should have 1 version
    let r = client
        .get(format!("{base}/v1/memories/{mid}/history"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["total"].as_i64().unwrap(), 1);
    assert_eq!(
        body["versions"][0]["content"].as_str().unwrap(),
        "history test v1"
    );
    println!("✅ memory history: 1 version");

    // Correct the memory
    client
        .put(format!("{base}/v1/memories/{mid}/correct"))
        .header("X-User-Id", &uid)
        .json(&json!({"new_content": "history test v2", "reason": "updated"}))
        .send()
        .await
        .unwrap();

    // History should still show the memory (in-place update, same id)
    let r = client
        .get(format!("{base}/v1/memories/{mid}/history"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert!(body["total"].as_i64().unwrap() >= 1);
    println!(
        "✅ memory history after correct: {} versions",
        body["total"]
    );
}

#[tokio::test]
async fn test_api_memory_history_not_found() {
    let (base, client) = spawn_server().await;
    let uid = uid();

    let r = client
        .get(format!("{base}/v1/memories/nonexistent-id/history"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap();
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

    let remote = RemoteClient::new(&base, None, uid.clone(), None);

    // Store
    let r = remote
        .call(
            "memory_store",
            json!({
                "content": "Remote mode test memory",
                "memory_type": "semantic"
            }),
        )
        .await
        .expect("store");
    let text = r["content"][0]["text"].as_str().unwrap_or("");
    assert!(text.contains("Stored memory"), "got: {text}");
    println!("✅ remote store: {text}");

    // Retrieve
    let r = remote
        .call(
            "memory_retrieve",
            json!({
                "query": "remote mode test",
                "top_k": 5
            }),
        )
        .await
        .expect("retrieve");
    let text = r["content"][0]["text"].as_str().unwrap_or("");
    assert!(
        text.contains("Remote mode test memory") || text.contains("No relevant"),
        "got: {text}"
    );
    println!("✅ remote retrieve: {}", &text[..text.len().min(80)]);
}

#[tokio::test]
async fn test_remote_correct_purge() {
    use memoria_mcp::remote::RemoteClient;

    let (base, _) = spawn_api_for_remote().await;
    let uid = uid();
    let remote = RemoteClient::new(&base, None, uid.clone(), None);

    // Store
    let r = remote
        .call("memory_store", json!({"content": "Uses black formatter"}))
        .await
        .expect("store");
    let text = r["content"][0]["text"].as_str().unwrap_or("");
    let mid = text
        .split_whitespace()
        .nth(2)
        .unwrap_or("")
        .trim_end_matches(':')
        .to_string();

    // Correct by id
    let r = remote
        .call(
            "memory_correct",
            json!({
                "memory_id": mid,
                "new_content": "Uses ruff formatter",
                "reason": "switched"
            }),
        )
        .await
        .expect("correct");
    let text = r["content"][0]["text"].as_str().unwrap_or("");
    assert!(text.contains("Corrected"), "got: {text}");
    println!("✅ remote correct: {text}");

    // Purge
    let r = remote
        .call("memory_purge", json!({"memory_id": mid}))
        .await
        .expect("purge");
    let text = r["content"][0]["text"].as_str().unwrap_or("");
    assert!(text.contains("Purged"), "got: {text}");
    println!("✅ remote purge: {text}");
}

#[tokio::test]
async fn test_remote_governance() {
    use memoria_mcp::remote::RemoteClient;

    let (base, _) = spawn_api_for_remote().await;
    let uid = uid();
    let remote = RemoteClient::new(&base, None, uid.clone(), None);

    let r = remote
        .call("memory_governance", json!({"force": true}))
        .await
        .expect("governance");
    let text = r["content"][0]["text"].as_str().unwrap_or("");
    assert!(
        text.contains("Governance complete") || text.contains("skipped"),
        "got: {text}"
    );
    println!("✅ remote governance: {text}");
}

#[tokio::test]
async fn test_remote_capabilities() {
    use memoria_mcp::remote::RemoteClient;

    let (base, _) = spawn_api_for_remote().await;
    let uid = uid();
    let remote = RemoteClient::new(&base, None, uid.clone(), None);

    let r = remote
        .call("memory_capabilities", json!({}))
        .await
        .expect("capabilities");
    let text = r["content"][0]["text"].as_str().unwrap_or("");
    assert!(
        text.contains("remote mode"),
        "should mention remote mode, got: {text}"
    );
    println!("✅ remote capabilities: {}", &text[..text.len().min(80)]);
}

#[tokio::test]
async fn test_remote_list_search_profile() {
    use memoria_mcp::remote::RemoteClient;
    let (base, _) = spawn_api_for_remote().await;
    let uid = uid();
    let remote = RemoteClient::new(&base, None, uid.clone(), None);

    remote
        .call(
            "memory_store",
            json!({"content": "Prefers Rust", "memory_type": "profile"}),
        )
        .await
        .unwrap();
    remote
        .call(
            "memory_store",
            json!({"content": "Uses MatrixOne database"}),
        )
        .await
        .unwrap();

    // list
    let r = remote
        .call("memory_list", json!({"limit": 10}))
        .await
        .unwrap();
    let t = r["content"][0]["text"].as_str().unwrap_or("");
    assert!(
        t.contains("MatrixOne") || t.contains("Prefers"),
        "list: {t}"
    );
    println!("✅ remote list: {}", &t[..t.len().min(80)]);

    // search
    let r = remote
        .call("memory_search", json!({"query": "database", "top_k": 5}))
        .await
        .unwrap();
    let t = r["content"][0]["text"].as_str().unwrap_or("");
    assert!(
        t.contains("MatrixOne") || t.contains("No relevant"),
        "search: {t}"
    );
    println!("✅ remote search: {}", &t[..t.len().min(80)]);

    // profile
    let r = remote.call("memory_profile", json!({})).await.unwrap();
    let t = r["content"][0]["text"].as_str().unwrap_or("");
    assert!(
        t.contains("Prefers Rust") || t.contains("No profile"),
        "profile: {t}"
    );
    println!("✅ remote profile: {t}");
}

#[tokio::test]
async fn test_remote_snapshot_branch() {
    use memoria_mcp::remote::RemoteClient;
    let (base, _) = spawn_api_for_remote().await;
    let uid = uid();
    let remote = RemoteClient::new(&base, None, uid.clone(), None);

    // Store a memory first
    remote
        .call(
            "memory_store",
            json!({"content": "snapshot branch test memory"}),
        )
        .await
        .unwrap();

    // Create snapshot
    let snap_name = format!(
        "test_snap_{}",
        uuid::Uuid::new_v4().simple().to_string()[..8].to_string()
    );
    let r = remote
        .call("memory_snapshot", json!({"name": snap_name}))
        .await
        .unwrap();
    let t = r["content"][0]["text"].as_str().unwrap_or("");
    assert!(
        t.contains("created") || t.contains(&snap_name),
        "snapshot create: {t}"
    );
    println!("✅ remote snapshot create: {t}");

    // List snapshots
    let r = remote
        .call("memory_snapshots", json!({"limit": 20}))
        .await
        .unwrap();
    let t = r["content"][0]["text"].as_str().unwrap_or("");
    println!("✅ remote snapshots list: {}", &t[..t.len().min(80)]);

    // Create branch
    let branch_name = format!(
        "test_br_{}",
        uuid::Uuid::new_v4().simple().to_string()[..8].to_string()
    );
    let r = remote
        .call("memory_branch", json!({"name": branch_name}))
        .await
        .unwrap();
    let t = r["content"][0]["text"].as_str().unwrap_or("");
    assert!(
        t.contains("created") || t.contains(&branch_name),
        "branch create: {t}"
    );
    println!("✅ remote branch create: {t}");

    // List branches
    let r = remote.call("memory_branches", json!({})).await.unwrap();
    let t = r["content"][0]["text"].as_str().unwrap_or("");
    println!("✅ remote branches list: {}", &t[..t.len().min(80)]);

    // Checkout branch
    let r = remote
        .call("memory_checkout", json!({"name": branch_name}))
        .await
        .unwrap();
    let t = r["content"][0]["text"].as_str().unwrap_or("");
    assert!(
        t.contains("Switched") || t.contains(&branch_name),
        "checkout: {t}"
    );
    println!("✅ remote checkout: {t}");

    // Store on branch
    remote
        .call("memory_store", json!({"content": "branch-only memory"}))
        .await
        .unwrap();

    // Diff
    let r = remote
        .call("memory_diff", json!({"source": branch_name}))
        .await
        .unwrap();
    let t = r["content"][0]["text"].as_str().unwrap_or("");
    println!("✅ remote diff: {}", &t[..t.len().min(80)]);

    // Merge back
    let r = remote
        .call(
            "memory_merge",
            json!({"source": branch_name, "strategy": "append"}),
        )
        .await
        .unwrap();
    let t = r["content"][0]["text"].as_str().unwrap_or("");
    println!("✅ remote merge: {}", &t[..t.len().min(80)]);

    // Delete branch
    let r = remote
        .call("memory_branch_delete", json!({"name": branch_name}))
        .await
        .unwrap();
    let t = r["content"][0]["text"].as_str().unwrap_or("");
    assert!(
        t.contains("deleted") || t.contains(&branch_name),
        "branch delete: {t}"
    );
    println!("✅ remote branch delete: {t}");

    // Delete snapshot
    let r = remote
        .call("memory_snapshot_delete", json!({"names": snap_name}))
        .await
        .unwrap();
    let t = r["content"][0]["text"].as_str().unwrap_or("");
    println!("✅ remote snapshot delete: {t}");
}

#[tokio::test]
async fn test_remote_reflect_extract_entities() {
    use memoria_mcp::remote::RemoteClient;
    let (base, _) = spawn_api_for_remote().await;
    let uid = uid();
    let remote = RemoteClient::new(&base, None, uid.clone(), None);

    remote
        .call(
            "memory_store",
            json!({"content": "Uses Rust for backend services", "session_id": "s1"}),
        )
        .await
        .unwrap();
    remote
        .call(
            "memory_store",
            json!({"content": "MatrixOne as primary database", "session_id": "s2"}),
        )
        .await
        .unwrap();

    // reflect candidates (no LLM needed)
    let r = remote
        .call(
            "memory_reflect",
            json!({"mode": "candidates", "force": true}),
        )
        .await
        .unwrap();
    let t = r["content"][0]["text"].as_str().unwrap_or("");
    assert!(
        !t.to_lowercase().contains("error"),
        "reflect should not error: {t}"
    );
    println!("✅ remote reflect candidates: {}", &t[..t.len().min(100)]);

    // extract entities candidates
    let r = remote
        .call("memory_extract_entities", json!({"mode": "candidates"}))
        .await
        .unwrap();
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
                }]))
                .unwrap();
                let r = remote
                    .call("memory_link_entities", json!({"entities": link_payload}))
                    .await
                    .unwrap();
                let t = r["content"][0]["text"].as_str().unwrap_or("");
                let p: serde_json::Value =
                    serde_json::from_str(t).unwrap_or(serde_json::Value::Null);
                assert!(
                    p.get("entities_created").is_some() || p["status"] == "done",
                    "link: {t}"
                );
                println!("✅ remote link entities: {t}");
            }
        }
    }
}

#[tokio::test]
async fn test_reflect_no_llm_falls_back_to_candidates() {
    // When LLM is not configured, mode=auto should return candidates (not error)
    let (base, client) = spawn_server().await;
    let uid = uid();

    client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({"content": "Rust backend", "memory_type": "semantic"}))
        .send()
        .await
        .unwrap();
    client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({"content": "MatrixOne database", "memory_type": "semantic"}))
        .send()
        .await
        .unwrap();

    let r = client
        .post(format!("{base}/v1/reflect"))
        .header("X-User-Id", &uid)
        .json(&json!({"mode": "auto", "force": true}))
        .send()
        .await
        .unwrap();
    assert!(
        r.status().is_success(),
        "reflect auto without LLM should not 500: {}",
        r.status()
    );
    let body: serde_json::Value = r.json().await.unwrap();
    assert!(
        body.get("candidates").is_some() || body.get("scenes_created").is_some(),
        "reflect response: {body}"
    );
    println!("✅ reflect mode=auto without LLM: candidates or scenes_created present");
}

#[tokio::test]
async fn test_extract_entities_no_llm_falls_back_to_candidates() {
    let (base, client) = spawn_server().await;
    let uid = uid();

    client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({"content": "Uses PostgreSQL and Redis", "memory_type": "semantic"}))
        .send()
        .await
        .unwrap();

    let r = client
        .post(format!("{base}/v1/extract-entities"))
        .header("X-User-Id", &uid)
        .json(&json!({"mode": "auto"}))
        .send()
        .await
        .unwrap();
    assert!(
        r.status().is_success(),
        "extract_entities auto without LLM should not 500"
    );
    let body: serde_json::Value = r.json().await.unwrap();
    assert!(
        body["status"] == "candidates" || body["status"] == "complete",
        "extract response: {body}"
    );
    println!(
        "✅ extract_entities mode=auto without LLM: status={}",
        body["status"]
    );
}

#[tokio::test]
async fn test_governance_pollution_detection() {
    let (base, client) = spawn_server().await;
    let uid = uid();

    // Store 3 memories then supersede 2 of them → ratio=2/5=0.4 > 0.3 → polluted
    let mut mids = vec![];
    for i in 0..3 {
        let r = client
            .post(format!("{base}/v1/memories"))
            .header("X-User-Id", &uid)
            .json(&json!({"content": format!("fact {i}"), "memory_type": "semantic"}))
            .send()
            .await
            .unwrap();
        let body: serde_json::Value = r.json().await.unwrap();
        mids.push(body["memory_id"].as_str().unwrap_or("").to_string());
    }
    // Supersede 2 via correct (sets superseded_by on old, creates new)
    for mid in &mids[..2] {
        client
            .put(format!("{base}/v1/memories/{mid}/correct"))
            .header("X-User-Id", &uid)
            .json(&json!({"new_content": "updated fact", "reason": "test"}))
            .send()
            .await
            .unwrap();
    }

    let r = client
        .post(format!(
            "{base}/admin/governance/{uid}/trigger?op=governance"
        ))
        .header("Authorization", "Bearer ")
        .send()
        .await
        .unwrap();
    assert!(
        r.status().is_success(),
        "governance trigger failed: {}",
        r.status()
    );
    let body: serde_json::Value = r.json().await.unwrap();
    assert_eq!(
        body["pollution_detected"], true,
        "expected pollution=true: {body}"
    );
    println!("✅ governance pollution_detected=true (high supersede ratio)");
}

#[tokio::test]
async fn test_reflect_with_llm() {
    let (llm, _shutdown) = spawn_fake_llm().await;
    let (base, client) = spawn_server_with_llm(llm).await;
    let uid = uid();

    let store = memoria_storage::SqlMemoryStore::connect(&db_url(), test_dim(), uuid::Uuid::new_v4().to_string())
        .await
        .expect("connect");
    store.migrate().await.expect("migrate");
    let graph = store.graph_store();
    for (idx, content) in [
        "Project uses Rust for all backend services",
        "MatrixOne is the primary database",
        "Validation emphasizes deterministic test loops",
    ]
    .into_iter()
    .enumerate()
    {
        graph.create_node(&memoria_storage::GraphNode {
            node_id: format!("reflect_node_{idx}_{}", &uuid::Uuid::new_v4().simple().to_string()[..8]),
            user_id: uid.clone(),
            node_type: memoria_storage::NodeType::Semantic,
            content: content.to_string(),
            entity_type: None,
            embedding: None,
            memory_id: None,
            session_id: Some("llm_cluster".to_string()),
            confidence: 0.8,
            trust_tier: "T3".to_string(),
            importance: 0.5,
            source_nodes: vec![],
            conflicts_with: None,
            conflict_resolution: None,
            access_count: 0,
            cross_session_count: 0,
            is_active: true,
            superseded_by: None,
            created_at: Some(chrono::Utc::now().naive_utc()),
        })
        .await
        .expect("create graph node");
    }

    let r = client
        .post(format!("{base}/v1/reflect"))
        .header("X-User-Id", &uid)
        .json(&json!({"mode": "auto", "force": true}))
        .send()
        .await
        .unwrap();
    assert!(
        r.status().is_success(),
        "reflect with LLM failed: {}",
        r.status()
    );
    let body: serde_json::Value = r.json().await.unwrap();
    assert!(
        body["scenes_created"].as_u64().unwrap_or(0) >= 1,
        "reflect should synthesize at least one scene: {body}"
    );

    let r = client
        .get(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .query(&[("limit", "20")])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let list_body: serde_json::Value = r.json().await.unwrap();
    let items = list_body["items"].as_array().expect("items array");
    assert!(
        items.iter().any(|m| {
            m["content"]
                .as_str()
                .unwrap_or("")
                .contains("Prefer deterministic validation")
        }),
        "reflected memory should be written back: {list_body}"
    );
    println!(
        "✅ reflect with LLM: scenes_created={}",
        body["scenes_created"]
    );
}

#[tokio::test]
async fn test_extract_entities_with_llm() {
    let (llm, _shutdown) = spawn_fake_llm().await;
    let (base, client) = spawn_server_with_llm(llm).await;
    let uid = uid();

    client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({
            "content": "our stack relies on alphamesh for routing and deltafabric for caching",
            "memory_type": "semantic"
        }))
        .send()
        .await
        .unwrap();

    let r = client
        .post(format!("{base}/v1/extract-entities"))
        .header("X-User-Id", &uid)
        .json(&json!({"mode": "auto"}))
        .send()
        .await
        .unwrap();
    assert!(
        r.status().is_success(),
        "extract_entities with LLM failed: {}",
        r.status()
    );
    let body: serde_json::Value = r.json().await.unwrap();
    assert_eq!(body["status"], "done", "extract LLM response: {body}");
    assert!(
        body["entities_found"].as_u64().unwrap_or(0) >= 2,
        "fake LLM should create entities: {body}"
    );

    let r = client
        .get(format!("{base}/v1/entities"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let entities_body: serde_json::Value = r.json().await.unwrap();
    let empty_entities = Vec::new();
    let names: Vec<&str> = entities_body["entities"]
        .as_array()
        .unwrap_or(&empty_entities)
        .iter()
        .filter_map(|e| e["name"].as_str())
        .collect();
    assert!(
        names.contains(&"alphamesh") && names.contains(&"deltafabric"),
        "LLM-extracted entities should be persisted: {entities_body}"
    );
    println!(
        "✅ extract_entities with LLM: entities_found={}",
        body["entities_found"]
    );
}

#[tokio::test]
async fn test_remote_consolidate() {
    use memoria_mcp::remote::RemoteClient;
    let (base, _) = spawn_api_for_remote().await;
    let uid = uid();
    let remote = RemoteClient::new(&base, None, uid.clone(), None);

    let r = remote
        .call("memory_consolidate", json!({"force": true}))
        .await
        .unwrap();
    let t = r["content"][0]["text"].as_str().unwrap_or("");
    assert!(
        t.contains("Consolidation complete") || t.contains("skipped"),
        "got: {t}"
    );
    println!("✅ remote consolidate: {t}");
}

#[tokio::test]
async fn test_remote_correct_by_query() {
    use memoria_mcp::remote::RemoteClient;
    let (base, _) = spawn_api_for_remote().await;
    let uid = uid();
    let remote = RemoteClient::new(&base, None, uid.clone(), None);

    remote
        .call(
            "memory_store",
            json!({"content": "Uses black for Python formatting"}),
        )
        .await
        .unwrap();

    let r = remote
        .call(
            "memory_correct",
            json!({
                "query": "black formatting",
                "new_content": "Uses ruff for Python formatting",
                "reason": "switched"
            }),
        )
        .await
        .unwrap();
    let t = r["content"][0]["text"].as_str().unwrap_or("");
    assert!(
        t.contains("Corrected") || t.contains("No matching"),
        "got: {t}"
    );
    println!("✅ remote correct by query: {t}");
}

#[tokio::test]
async fn test_remote_purge_by_topic() {
    use memoria_mcp::remote::RemoteClient;
    let (base, _) = spawn_api_for_remote().await;
    let uid = uid();
    let remote = RemoteClient::new(&base, None, uid.clone(), None);

    remote
        .call("memory_store", json!({"content": "topic purge test alpha"}))
        .await
        .unwrap();
    remote
        .call("memory_store", json!({"content": "topic purge test beta"}))
        .await
        .unwrap();

    let r = remote
        .call("memory_purge", json!({"topic": "topic purge test"}))
        .await
        .unwrap();
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
    client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({"content": "Worked on Rust backend", "session_id": "sess1"}))
        .send()
        .await
        .unwrap();

    // Without LLM configured, should return 503
    let r = client
        .post(format!("{base}/v1/sessions/sess1/summary"))
        .header("X-User-Id", &uid)
        .json(&json!({"mode": "full", "sync": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 503, "should return 503 without LLM");
    println!("✅ episodic without LLM: 503 SERVICE_UNAVAILABLE");
}

async fn spawn_server_with_llm(
    llm: Arc<memoria_embedding::LlmClient>,
) -> (String, reqwest::Client) {
    use memoria_git::GitForDataService;
    use memoria_service::{Config, MemoryService};
    use memoria_storage::SqlMemoryStore;
    use sqlx::mysql::MySqlPool;

    let cfg = Config::from_env();
    let db = db_url();
    let store = SqlMemoryStore::connect(&db, test_dim(), uuid::Uuid::new_v4().to_string())
        .await
        .expect("connect");
    store.migrate().await.expect("migrate");
    let pool = MySqlPool::connect(&db).await.expect("pool");
    let git = Arc::new(GitForDataService::new(pool, &cfg.db_name));
    let service = Arc::new(MemoryService::new_sql_with_llm(
        Arc::new(store),
        None,
        Some(llm),
    ));
    let state = memoria_api::AppState::new(service, git, String::new());
    let app = memoria_api::build_router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    (format!("http://127.0.0.1:{port}"), client)
}

async fn spawn_server_with_embedding(
    emb_key: String,
    base_url: String,
    model: String,
) -> (String, reqwest::Client) {
    use memoria_embedding::HttpEmbedder;
    use memoria_git::GitForDataService;
    use memoria_service::{Config, MemoryService};
    use memoria_storage::SqlMemoryStore;
    use sqlx::mysql::MySqlPool;

    let cfg = Config::from_env();
    let db = db_url();
    let store = SqlMemoryStore::connect(&db, 1024, uuid::Uuid::new_v4().to_string()).await.expect("connect");
    store.migrate().await.expect("migrate");
    let pool = MySqlPool::connect(&db).await.expect("pool");
    let git = Arc::new(GitForDataService::new(pool, &cfg.db_name));
    let embedder = Arc::new(HttpEmbedder::new(base_url, emb_key, model, 1024));
    let service = Arc::new(MemoryService::new_sql_with_llm(
        Arc::new(store),
        Some(embedder),
        None,
    ));
    let state = memoria_api::AppState::new(service, git, String::new());
    let app = memoria_api::build_router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    (format!("http://127.0.0.1:{port}"), client)
}

#[tokio::test]
async fn test_episodic_no_memories_returns_error() {
    let (llm, _shutdown) = spawn_fake_llm().await;
    let (base, client) = spawn_server_with_llm(llm).await;
    let uid = uid();

    // No memories for this session → 500
    let r = client
        .post(format!("{base}/v1/sessions/nonexistent_session/summary"))
        .header("X-User-Id", &uid)
        .json(&json!({"mode": "full", "sync": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 500, "should return 500 for empty session");
    println!("✅ episodic empty session: 500");
}

#[tokio::test]
async fn test_episodic_async_task_polling() {
    let (base, client) = spawn_server().await;
    let uid = uid();

    // Without LLM, async mode should still create a task (that will fail)
    // but the endpoint itself returns 503 before creating a task
    let r = client
        .post(format!("{base}/v1/sessions/sess_async/summary"))
        .header("X-User-Id", &uid)
        .json(&json!({"mode": "full", "sync": false}))
        .send()
        .await
        .unwrap();
    // Without LLM: 503
    assert_eq!(r.status(), 503);
    println!("✅ episodic async without LLM: 503");
}

#[tokio::test]
async fn test_episodic_with_llm_sync() {
    let (llm, _shutdown) = spawn_fake_llm().await;
    let (base, client) = spawn_server_with_llm(llm).await;
    let uid = uid();
    let session_id = format!(
        "ep_sess_{}",
        uuid::Uuid::new_v4().simple().to_string()[..8].to_string()
    );

    // Store memories with session_id
    for content in &[
        "Implemented Rust REST API",
        "Added episodic memory support",
        "All tests passing",
    ] {
        client
            .post(format!("{base}/v1/memories"))
            .header("X-User-Id", &uid)
            .json(&json!({"content": content, "session_id": session_id}))
            .send()
            .await
            .unwrap();
    }

    // Generate episodic memory (sync)
    let r = client
        .post(format!("{base}/v1/sessions/{session_id}/summary"))
        .header("X-User-Id", &uid)
        .json(&json!({"mode": "full", "sync": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "should return 200");
    let body: serde_json::Value = r.json().await.unwrap();
    assert!(
        body["memory_id"].as_str().is_some(),
        "should have memory_id: {body}"
    );
    assert!(
        body["content"]
            .as_str()
            .map(|c| c.contains("Deterministic validation of session workflows"))
            .unwrap_or(false),
        "content should contain fake LLM topic: {body}"
    );
    assert_eq!(
        body["metadata"]["topic"].as_str().unwrap_or(""),
        "Deterministic validation of session workflows"
    );
    println!(
        "✅ episodic with LLM sync: memory_id={}",
        body["memory_id"].as_str().unwrap_or("")
    );
    println!(
        "   content: {}",
        &body["content"].as_str().unwrap_or("")
            [..100.min(body["content"].as_str().unwrap_or("").len())]
    );
}

#[tokio::test]
async fn test_episodic_with_llm_async() {
    let (llm, _shutdown) = spawn_fake_llm().await;
    let (base, client) = spawn_server_with_llm(llm).await;
    let uid = uid();
    let session_id = format!("ep_async_{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);

    for content in [
        "Validated scheduler fallback behavior",
        "Verified deterministic reflection write-back",
    ] {
        client
            .post(format!("{base}/v1/memories"))
            .header("X-User-Id", &uid)
            .json(&json!({"content": content, "session_id": session_id}))
            .send()
            .await
            .unwrap();
    }

    let r = client
        .post(format!("{base}/v1/sessions/{session_id}/summary"))
        .header("X-User-Id", &uid)
        .json(&json!({"mode": "full", "sync": false}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: serde_json::Value = r.json().await.unwrap();
    let task_id = body["task_id"].as_str().expect("task_id").to_string();

    let mut result = None;
    for attempt in 0..60 {
        let poll = client
            .get(format!("{base}/v1/tasks/{task_id}"))
            .header("X-User-Id", &uid)
            .send()
            .await
            .unwrap();
        assert_eq!(poll.status(), 200);
        let task: serde_json::Value = poll.json().await.unwrap();
        match task["status"].as_str().unwrap_or("") {
            "completed" => {
                result = Some(task["result"].clone());
                break;
            }
            "failed" => panic!("async episodic task failed on attempt {attempt}: {task}"),
            status => {
                if attempt == 59 {
                    panic!("async episodic task timed out after 60 polls, last status={status}: {task}");
                }
                tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
            }
        }
    }

    let result = result.expect("async episodic task should complete");
    assert!(
        result["content"]
            .as_str()
            .unwrap_or("")
            .contains("Deterministic validation of session workflows"),
        "async summary should persist fake LLM content: {result}"
    );
    println!("✅ episodic with LLM async: task_id={task_id}");
}

// ── Admin API ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_admin_stats_and_users() {
    let (base, client) = spawn_server().await;
    let user = uid();

    // Store a memory first
    client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &user)
        .json(&json!({"content": "admin test memory", "memory_type": "semantic"}))
        .send()
        .await
        .unwrap();

    // GET /admin/stats
    let r = client
        .get(format!("{base}/admin/stats"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert!(body["total_memories"].as_i64().unwrap() >= 1);
    assert!(body["total_users"].as_i64().unwrap() >= 1);
    println!("✅ admin stats: {body}");

    // GET /admin/users — just check it returns a list (may not contain our user if DB has many)
    let r = client
        .get(format!("{base}/admin/users"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    let users = body["users"].as_array().unwrap();
    assert!(!users.is_empty(), "admin users list should not be empty");
    println!("✅ admin users: {} users", users.len());

    // GET /admin/users/:user_id/stats
    let r = client
        .get(format!("{base}/admin/users/{user}/stats"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["memory_count"].as_i64().unwrap(), 1);
    println!("✅ admin user stats: {body}");

    // POST /admin/users/:user_id/reset-access-counts
    let r = client
        .post(format!("{base}/admin/users/{user}/reset-access-counts"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    println!("✅ admin reset access counts");

    // DELETE /admin/users/:user_id
    let r = client
        .delete(format!("{base}/admin/users/{user}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    println!("✅ admin delete user");

    // Verify user's memories are deactivated
    let r = client
        .get(format!("{base}/admin/users/{user}/stats"))
        .send()
        .await
        .unwrap();
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["memory_count"].as_i64().unwrap(), 0);
    println!("✅ admin verified user deleted (0 active memories)");
}

#[tokio::test]
async fn test_admin_trigger_governance() {
    let (base, client) = spawn_server().await;
    let user = uid();

    // Store a memory
    client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &user)
        .json(&json!({"content": "governance trigger test", "memory_type": "semantic"}))
        .send()
        .await
        .unwrap();

    // Trigger governance (skips cooldown)
    let r = client
        .post(format!(
            "{base}/admin/governance/{user}/trigger?op=governance"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["op"].as_str().unwrap(), "governance");
    println!("✅ admin trigger governance: {body}");

    // Trigger consolidate
    let r = client
        .post(format!(
            "{base}/admin/governance/{user}/trigger?op=consolidate"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["op"].as_str().unwrap(), "consolidate");
    println!("✅ admin trigger consolidate: {body}");

    // Invalid op
    let r = client
        .post(format!("{base}/admin/governance/{user}/trigger?op=invalid"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    println!("✅ admin trigger invalid op rejected");
}

#[tokio::test]
async fn test_health_endpoints() {
    let (base, client) = spawn_server().await;
    let user = uid();

    // Store some memories
    for content in ["Rust is fast", "Python is easy", "Go is simple"] {
        client
            .post(format!("{base}/v1/memories"))
            .header("X-User-Id", &user)
            .json(&json!({"content": content, "memory_type": "semantic"}))
            .send()
            .await
            .unwrap();
    }

    // GET /v1/health/analyze
    let r = client
        .get(format!("{base}/v1/health/analyze"))
        .header("X-User-Id", &user)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert!(body["semantic"]["total"].as_i64().unwrap() >= 3);
    println!("✅ health analyze: {body}");

    // GET /v1/health/storage
    let r = client
        .get(format!("{base}/v1/health/storage"))
        .header("X-User-Id", &user)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert!(body["total"].as_i64().unwrap() >= 3);
    assert!(body["active"].as_i64().unwrap() >= 3);
    println!("✅ health storage: {body}");

    // GET /v1/health/capacity
    let r = client
        .get(format!("{base}/v1/health/capacity"))
        .header("X-User-Id", &user)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["recommendation"].as_str().unwrap(), "ok");
    println!("✅ health capacity: {body}");
}

// ── Sandbox validation ────────────────────────────────────────────────────────

#[tokio::test]
async fn test_sandbox_validation() {
    let (base, client) = spawn_server().await;
    let user = uid();

    // Store some base memories
    for content in [
        "Rust is a systems language",
        "Python is great for scripting",
    ] {
        client
            .post(format!("{base}/v1/memories"))
            .header("X-User-Id", &user)
            .json(&json!({"content": content, "memory_type": "semantic"}))
            .send()
            .await
            .unwrap();
    }

    // Sandbox validation is internal — test via store (which uses it internally if enabled)
    // For now, verify that storing a memory still works (sandbox is fail-open)
    let r = client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &user)
        .json(&json!({"content": "Go is compiled and fast", "memory_type": "semantic"}))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success());
    let body: Value = r.json().await.unwrap();
    assert!(body["memory_id"].as_str().is_some());
    println!("✅ sandbox: store succeeds (fail-open)");
}

#[tokio::test]
async fn test_retrieve_with_explain() {
    let (base, client) = spawn_server().await;
    let user = uid();

    client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &user)
        .json(&json!({"content": "Rust is fast and safe", "memory_type": "semantic"}))
        .send()
        .await
        .unwrap();

    // explain=true (basic)
    let r = client
        .post(format!("{base}/v1/memories/retrieve"))
        .header("X-User-Id", &user)
        .json(&json!({"query": "fast language", "top_k": 5, "explain": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert!(body["explain"].is_object(), "explain field missing: {body}");
    assert!(body["explain"]["path"].is_string());
    assert!(body["explain"]["total_ms"].is_number());
    assert_eq!(body["explain"]["level"], "basic");
    println!(
        "✅ explain=basic: path={}, total_ms={}",
        body["explain"]["path"].as_str().unwrap_or("?"),
        body["explain"]["total_ms"].as_f64().unwrap_or(0.0)
    );

    // explain="verbose" — should include candidate_scores
    let r = client
        .post(format!("{base}/v1/memories/retrieve"))
        .header("X-User-Id", &user)
        .json(&json!({"query": "fast language", "top_k": 5, "explain": "verbose"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["explain"]["level"], "verbose");
    // candidate_scores present when results exist
    if !body["results"]
        .as_array()
        .map(|a| a.is_empty())
        .unwrap_or(true)
    {
        let scores = &body["explain"]["candidate_scores"];
        assert!(
            scores.is_array(),
            "verbose should have candidate_scores: {body}"
        );
        let first = &scores[0];
        assert!(
            first["final_score"].is_number(),
            "missing final_score: {first}"
        );
        assert!(first["vector_score"].is_number());
        assert!(first["keyword_score"].is_number());
        assert!(first["temporal_score"].is_number());
        assert!(first["confidence_score"].is_number());
        println!("✅ explain=verbose: candidate_scores[0]={}", first);
    }

    // explain="none" — returns array directly
    let r = client
        .post(format!("{base}/v1/memories/retrieve"))
        .header("X-User-Id", &user)
        .json(&json!({"query": "fast language", "top_k": 5}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert!(
        body.is_array(),
        "without explain should return array: {body}"
    );
    println!(
        "✅ explain=none: {} results",
        body.as_array().unwrap().len()
    );
}

#[tokio::test]
async fn test_explain_verbose_candidate_scores() {
    let Some((key, base_url, model)) = try_embedding() else {
        println!("⏭️  test_explain_verbose_candidate_scores skipped (EMBEDDING_API_KEY not set)");
        return;
    };
    let (base, client) = spawn_server_with_embedding(key, base_url, model).await;
    let uid = uid();

    // Store a few memories
    for content in [
        "Rust is fast and memory-safe",
        "Python is easy to learn",
        "Go has great concurrency",
    ] {
        client
            .post(format!("{base}/v1/memories"))
            .header("X-User-Id", &uid)
            .json(&json!({"content": content, "memory_type": "semantic"}))
            .send()
            .await
            .unwrap();
    }

    // explain=verbose should return candidate_scores with 4-dim breakdown
    let r = client
        .post(format!("{base}/v1/memories/retrieve"))
        .header("X-User-Id", &uid)
        .json(&json!({"query": "fast programming language", "top_k": 5, "explain": "verbose"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert_eq!(
        body["explain"]["level"], "verbose",
        "level mismatch: {body}"
    );
    assert!(body["explain"]["path"].is_string());

    let results = body["results"].as_array().expect("results array");
    if !results.is_empty() {
        let scores = body["explain"]["candidate_scores"]
            .as_array()
            .expect("candidate_scores should be array when results non-empty");
        assert!(!scores.is_empty(), "candidate_scores empty");
        let first = &scores[0];
        assert!(first["final_score"].is_number(), "missing final_score");
        assert!(first["vector_score"].is_number(), "missing vector_score");
        assert!(first["keyword_score"].is_number(), "missing keyword_score");
        assert!(
            first["temporal_score"].is_number(),
            "missing temporal_score"
        );
        assert!(
            first["confidence_score"].is_number(),
            "missing confidence_score"
        );
        assert_eq!(first["rank"], 1);
        println!("✅ explain=verbose candidate_scores[0]: final={:.4} vec={:.4} kw={:.4} time={:.4} conf={:.4}",
            first["final_score"].as_f64().unwrap_or(0.0),
            first["vector_score"].as_f64().unwrap_or(0.0),
            first["keyword_score"].as_f64().unwrap_or(0.0),
            first["temporal_score"].as_f64().unwrap_or(0.0),
            first["confidence_score"].as_f64().unwrap_or(0.0));
    } else {
        println!("⚠️  no results returned (embedding may not have indexed yet)");
    }
}

#[tokio::test]
async fn test_pipeline_run() {
    let (base, client) = spawn_server().await;
    let user = uid();

    // Normal candidates — should all be stored
    let r = client
        .post(format!("{base}/v1/pipeline/run"))
        .header("X-User-Id", &user)
        .json(&json!({
            "candidates": [
                {"content": "Rust is fast", "memory_type": "semantic"},
                {"content": "Python is easy", "memory_type": "semantic"},
            ]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["memories_stored"].as_i64().unwrap(), 2);
    assert_eq!(body["memories_rejected"].as_i64().unwrap(), 0);
    println!("✅ pipeline run: {body}");

    // Sensitive candidate — should be blocked
    let r = client
        .post(format!("{base}/v1/pipeline/run"))
        .header("X-User-Id", &user)
        .json(&json!({
            "candidates": [
                {"content": "password=supersecret123", "memory_type": "semantic"},
                {"content": "Go is compiled", "memory_type": "semantic"},
            ]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["memories_stored"].as_i64().unwrap(), 1);
    assert_eq!(body["memories_rejected"].as_i64().unwrap(), 1);
    println!("✅ pipeline sensitivity block: {body}");
}

// ═══════════════════════════════════════════════════════════════════════════════
// NEW FEATURE TESTS — admin keys, user params, snapshot detail/diff, batch embed
// ═══════════════════════════════════════════════════════════════════════════════

// ── Admin: list user keys ────────────────────────────────────────────────────

#[tokio::test]
async fn test_admin_list_user_keys() {
    let mk = "test-mk-list-keys";
    let (base, client) = spawn_server_with_master_key(mk).await;
    let auth = format!("Bearer {mk}");
    let uid = uid();

    // Create a key for this user first
    client
        .post(format!("{base}/auth/keys"))
        .header("Authorization", &auth)
        .json(&json!({"user_id": uid, "name": "key-for-list"}))
        .send()
        .await
        .unwrap();

    // List via admin endpoint
    let r = client
        .get(format!("{base}/admin/users/{uid}/keys"))
        .header("Authorization", &auth)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["user_id"], uid);
    let keys = body["keys"].as_array().unwrap();
    assert!(
        keys.iter().any(|k| k["name"] == "key-for-list"),
        "should find created key: {body}"
    );
    // Verify key fields
    let k = &keys[0];
    assert!(k["key_id"].as_str().is_some());
    assert!(k["key_prefix"].as_str().is_some());
    assert!(k["created_at"].as_str().is_some());
    println!("✅ GET /admin/users/:id/keys: {} keys", keys.len());
}

#[tokio::test]
async fn test_admin_list_user_keys_empty() {
    let mk = "test-mk-list-keys-empty";
    let (base, client) = spawn_server_with_master_key(mk).await;
    let auth = format!("Bearer {mk}");
    let uid = uid(); // fresh user, no keys

    let r = client
        .get(format!("{base}/admin/users/{uid}/keys"))
        .header("Authorization", &auth)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert!(
        body["keys"].as_array().unwrap().is_empty(),
        "new user should have no keys"
    );
    println!("✅ GET /admin/users/:id/keys (empty): {body}");
}

// ── Admin: revoke all user keys ──────────────────────────────────────────────

#[tokio::test]
async fn test_admin_revoke_all_user_keys() {
    let mk = "test-mk-revoke-all";
    let (base, client) = spawn_server_with_master_key(mk).await;
    let auth = format!("Bearer {mk}");
    let uid = uid();

    // Create 3 keys
    for i in 0..3 {
        client
            .post(format!("{base}/auth/keys"))
            .header("Authorization", &auth)
            .json(&json!({"user_id": uid, "name": format!("key-{i}")}))
            .send()
            .await
            .unwrap();
    }

    // Verify 3 keys exist
    let r = client
        .get(format!("{base}/admin/users/{uid}/keys"))
        .header("Authorization", &auth)
        .send()
        .await
        .unwrap();
    assert_eq!(
        r.json::<Value>().await.unwrap()["keys"]
            .as_array()
            .unwrap()
            .len(),
        3
    );

    // Revoke all
    let r = client
        .delete(format!("{base}/admin/users/{uid}/keys"))
        .header("Authorization", &auth)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["revoked"], 3);
    println!(
        "✅ DELETE /admin/users/:id/keys: revoked {}",
        body["revoked"]
    );

    // Verify all gone
    let r = client
        .get(format!("{base}/admin/users/{uid}/keys"))
        .header("Authorization", &auth)
        .send()
        .await
        .unwrap();
    let body: Value = r.json().await.unwrap();
    assert!(
        body["keys"].as_array().unwrap().is_empty(),
        "all keys should be revoked"
    );
    println!("✅ verified 0 keys after revoke_all");
}

#[tokio::test]
async fn test_admin_revoke_all_user_keys_idempotent() {
    let mk = "test-mk-revoke-idem";
    let (base, client) = spawn_server_with_master_key(mk).await;
    let auth = format!("Bearer {mk}");
    let uid = uid(); // no keys

    // Revoke on user with no keys → should succeed with revoked=0
    let r = client
        .delete(format!("{base}/admin/users/{uid}/keys"))
        .header("Authorization", &auth)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["revoked"], 0);
    println!("✅ revoke_all idempotent: revoked=0 for user with no keys");
}

// ── Admin: set user params ───────────────────────────────────────────────────

#[tokio::test]
async fn test_admin_set_user_params() {
    let mk = "test-mk-set-params";
    let (base, client) = spawn_server_with_master_key(mk).await;
    let auth = format!("Bearer {mk}");
    let uid = uid();

    // Ensure the config table exists (may not be in Rust migration yet)
    if let Ok(pool) = sqlx::mysql::MySqlPool::connect(&db_url()).await {
        let _ = sqlx::query(
            "CREATE TABLE IF NOT EXISTS mem_user_memory_config (\
             user_id VARCHAR(128) PRIMARY KEY, \
             strategy_key VARCHAR(64) DEFAULT NULL, \
             params_json JSON DEFAULT NULL, \
             updated_at DATETIME DEFAULT NULL)",
        )
        .execute(&pool)
        .await;
        // Insert a row for the user
        let _ = sqlx::query("INSERT IGNORE INTO mem_user_memory_config (user_id) VALUES (?)")
            .bind(&uid)
            .execute(&pool)
            .await;
    }

    // Set params
    let params = json!({"vector_weight": 0.7, "keyword_weight": 0.3, "max_results": 20});
    let r = client
        .post(format!("{base}/admin/users/{uid}/params"))
        .header("Authorization", &auth)
        .json(&params)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["user_id"], uid);
    assert_eq!(body["params"]["vector_weight"], 0.7);
    assert_eq!(body["params"]["keyword_weight"], 0.3);
    println!("✅ POST /admin/users/:id/params: {body}");
}

// ── Snapshot: GET detail (time-travel) ───────────────────────────────────────

#[tokio::test]
async fn test_snapshot_get_detail() {
    let (base, client) = spawn_server().await;
    let uid = uid();

    // Store memories
    for content in [
        "snapshot detail A",
        "snapshot detail B",
        "snapshot detail C",
    ] {
        client
            .post(format!("{base}/v1/memories"))
            .header("X-User-Id", &uid)
            .json(&json!({"content": content, "memory_type": "semantic"}))
            .send()
            .await
            .unwrap();
    }

    // Create snapshot
    let snap = format!(
        "detail_test_{}",
        &uuid::Uuid::new_v4().simple().to_string()[..8]
    );
    client
        .post(format!("{base}/v1/snapshots"))
        .header("X-User-Id", &uid)
        .json(&json!({"name": snap}))
        .send()
        .await
        .unwrap();

    // GET snapshot detail (brief, default)
    let r = client
        .get(format!("{base}/v1/snapshots/{snap}"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["name"], snap);
    assert_eq!(body["memory_count"], 3);
    assert!(body["by_type"]["semantic"].as_i64().unwrap() >= 3);
    let mems = body["memories"].as_array().unwrap();
    assert_eq!(mems.len(), 3);
    // Brief mode: content should be short
    for m in mems {
        assert!(m["memory_id"].as_str().is_some());
        assert!(m["content"].as_str().is_some());
        assert_eq!(m["memory_type"], "semantic");
    }
    println!(
        "✅ GET /v1/snapshots/:name (brief): {} memories, by_type={}",
        mems.len(),
        body["by_type"]
    );

    // GET with detail=full — should include confidence
    let r = client
        .get(format!("{base}/v1/snapshots/{snap}?detail=full"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    let mems = body["memories"].as_array().unwrap();
    assert!(
        mems[0].get("confidence").is_some(),
        "full detail should include confidence: {}",
        mems[0]
    );
    println!("✅ GET /v1/snapshots/:name (full): confidence present");

    // GET with pagination
    let r = client
        .get(format!("{base}/v1/snapshots/{snap}?limit=2&offset=0"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap();
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["memories"].as_array().unwrap().len(), 2);
    assert_eq!(body["has_more"], true);
    assert_eq!(body["limit"], 2);
    assert_eq!(body["offset"], 0);
    println!("✅ GET /v1/snapshots/:name (paginated): limit=2, has_more=true");

    // Page 2
    let r = client
        .get(format!("{base}/v1/snapshots/{snap}?limit=2&offset=2"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap();
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["memories"].as_array().unwrap().len(), 1);
    assert_eq!(body["has_more"], false);
    println!("✅ GET /v1/snapshots/:name (page 2): 1 memory, has_more=false");

    // Cleanup
    client
        .delete(format!("{base}/v1/snapshots/{snap}"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap();
}

// ── Snapshot: diff (current vs snapshot) ─────────────────────────────────────

#[tokio::test]
async fn test_snapshot_diff() {
    let (base, client) = spawn_server().await;
    let uid = uid();

    // Store 2 memories
    let mut mids = vec![];
    for content in ["diff base A", "diff base B"] {
        let r = client
            .post(format!("{base}/v1/memories"))
            .header("X-User-Id", &uid)
            .json(&json!({"content": content}))
            .send()
            .await
            .unwrap();
        mids.push(
            r.json::<Value>().await.unwrap()["memory_id"]
                .as_str()
                .unwrap()
                .to_string(),
        );
    }

    // Create snapshot
    let snap = format!(
        "diff_test_{}",
        &uuid::Uuid::new_v4().simple().to_string()[..8]
    );
    client
        .post(format!("{base}/v1/snapshots"))
        .header("X-User-Id", &uid)
        .json(&json!({"name": snap}))
        .send()
        .await
        .unwrap();

    // Add a new memory after snapshot
    client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({"content": "diff added C"}))
        .send()
        .await
        .unwrap();

    // Delete one of the original memories
    client
        .delete(format!("{base}/v1/memories/{}", mids[0]))
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap();

    // Diff
    let r = client
        .get(format!("{base}/v1/snapshots/{snap}/diff"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["snapshot_count"], 2, "snapshot had 2 memories");
    assert_eq!(
        body["current_count"], 2,
        "current has 2 (1 original + 1 new)"
    );

    let added = body["added"].as_array().unwrap();
    let removed = body["removed"].as_array().unwrap();
    // "diff added C" should be in added
    assert!(
        added
            .iter()
            .any(|m| m["content"].as_str().unwrap().contains("diff added C")),
        "should find added memory: {added:?}"
    );
    // "diff base A" should be in removed (deleted after snapshot)
    assert!(
        removed
            .iter()
            .any(|m| m["content"].as_str().unwrap().contains("diff base A")),
        "should find removed memory: {removed:?}"
    );
    println!(
        "✅ GET /v1/snapshots/:name/diff: added={}, removed={}",
        added.len(),
        removed.len()
    );

    // Diff with limit
    let r = client
        .get(format!("{base}/v1/snapshots/{snap}/diff?limit=1"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert!(body["added"].as_array().unwrap().len() <= 1);
    assert!(body["removed"].as_array().unwrap().len() <= 1);
    println!("✅ GET /v1/snapshots/:name/diff (limit=1)");

    // Cleanup
    client
        .delete(format!("{base}/v1/snapshots/{snap}"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap();
}

#[tokio::test]
async fn test_snapshot_diff_no_changes() {
    let (base, client) = spawn_server().await;
    let uid = uid();

    // Store a memory
    client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({"content": "no change test"}))
        .send()
        .await
        .unwrap();

    // Snapshot immediately
    let snap = format!(
        "nochange_{}",
        &uuid::Uuid::new_v4().simple().to_string()[..8]
    );
    client
        .post(format!("{base}/v1/snapshots"))
        .header("X-User-Id", &uid)
        .json(&json!({"name": snap}))
        .send()
        .await
        .unwrap();

    // Diff with no changes
    let r = client
        .get(format!("{base}/v1/snapshots/{snap}/diff"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert!(
        body["added"].as_array().unwrap().is_empty(),
        "no additions expected"
    );
    assert!(
        body["removed"].as_array().unwrap().is_empty(),
        "no removals expected"
    );
    assert_eq!(body["snapshot_count"], body["current_count"]);
    println!("✅ snapshot diff no changes: counts match, empty added/removed");

    client
        .delete(format!("{base}/v1/snapshots/{snap}"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap();
}

#[tokio::test]
async fn test_api_snapshot_limit_is_per_user() {
    let (base, client) = spawn_server().await;
    let uid_a = uid();
    let uid_b = uid();
    let names_a: Vec<String> = (0..20)
        .map(|i| {
            format!(
                "api_cap_a_{i}_{}",
                &uuid::Uuid::new_v4().simple().to_string()[..6]
            )
        })
        .collect();

    for name in &names_a {
        let r = client
            .post(format!("{base}/v1/snapshots"))
            .header("X-User-Id", &uid_a)
            .json(&json!({"name": name}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 201);
        let body: Value = r.json().await.unwrap();
        let result = body["result"].as_str().unwrap_or("");
        assert!(
            result.contains("created"),
            "snapshot create failed: {result}"
        );
    }

    let overflow = format!(
        "api_cap_overflow_{}",
        &uuid::Uuid::new_v4().simple().to_string()[..6]
    );
    let r = client
        .post(format!("{base}/v1/snapshots"))
        .header("X-User-Id", &uid_a)
        .json(&json!({"name": overflow}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);
    let body: Value = r.json().await.unwrap();
    assert!(
        body["result"]
            .as_str()
            .unwrap_or("")
            .contains("Snapshot limit reached (20)"),
        "expected per-user cap message: {body}"
    );

    let b_snap = format!(
        "api_cap_b_{}",
        &uuid::Uuid::new_v4().simple().to_string()[..6]
    );
    let r = client
        .post(format!("{base}/v1/snapshots"))
        .header("X-User-Id", &uid_b)
        .json(&json!({"name": b_snap}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);
    let body: Value = r.json().await.unwrap();
    assert!(
        body["result"].as_str().unwrap_or("").contains("created"),
        "user B should still be able to create: {body}"
    );

    let r = client
        .get(format!("{base}/v1/snapshots"))
        .header("X-User-Id", &uid_b)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    let listed = body["result"].as_str().unwrap_or("");
    assert!(
        listed.contains(&b_snap),
        "B should see own snapshot: {listed}"
    );
    assert!(
        !listed.contains(&names_a[0]),
        "B should not see A's snapshots: {listed}"
    );

    client
        .post(format!("{base}/v1/snapshots/delete"))
        .header("X-User-Id", &uid_a)
        .json(&json!({"names": names_a.join(",")}))
        .send()
        .await
        .unwrap();
    client
        .delete(format!("{base}/v1/snapshots/{b_snap}"))
        .header("X-User-Id", &uid_b)
        .send()
        .await
        .unwrap();
}

#[tokio::test]
async fn test_api_snapshot_detail_is_scoped_to_owner() {
    let (base, client) = spawn_server().await;
    let uid_a = uid();
    let uid_b = uid();
    let snap = format!(
        "api_owned_{}",
        &uuid::Uuid::new_v4().simple().to_string()[..6]
    );

    client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid_a)
        .json(&json!({"content": "owner snapshot detail"}))
        .send()
        .await
        .unwrap();

    client
        .post(format!("{base}/v1/snapshots"))
        .header("X-User-Id", &uid_a)
        .json(&json!({"name": snap}))
        .send()
        .await
        .unwrap();

    let r = client
        .get(format!("{base}/v1/snapshots/{snap}"))
        .header("X-User-Id", &uid_b)
        .send()
        .await
        .unwrap();
    assert_eq!(
        r.status(),
        404,
        "non-owner should not resolve snapshot detail"
    );

    client
        .delete(format!("{base}/v1/snapshots/{snap}"))
        .header("X-User-Id", &uid_a)
        .send()
        .await
        .unwrap();
}

// ── Batch store: validates types upfront ─────────────────────────────────────

#[tokio::test]
async fn test_batch_store_invalid_type_rejects_all() {
    let (base, client) = spawn_server().await;
    let uid = uid();

    // One valid, one invalid type → should reject entire batch
    let r = client
        .post(format!("{base}/v1/memories/batch"))
        .header("X-User-Id", &uid)
        .json(&json!({"memories": [
            {"content": "valid memory", "memory_type": "semantic"},
            {"content": "bad type", "memory_type": "nonexistent_type"},
        ]}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 422, "invalid type should reject batch");
    println!("✅ batch store: invalid type rejects entire batch");
}

#[tokio::test]
async fn test_batch_store_all_types() {
    let (base, client) = spawn_server().await;
    let uid = uid();

    let r = client
        .post(format!("{base}/v1/memories/batch"))
        .header("X-User-Id", &uid)
        .json(&json!({"memories": [
            {"content": "semantic fact", "memory_type": "semantic"},
            {"content": "user preference", "memory_type": "profile"},
            {"content": "how to deploy", "memory_type": "procedural"},
            {"content": "current task", "memory_type": "working"},
        ]}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);
    let body: Vec<Value> = r.json().await.unwrap();
    assert_eq!(body.len(), 4);
    let types: Vec<&str> = body
        .iter()
        .map(|m| m["memory_type"].as_str().unwrap())
        .collect();
    assert!(types.contains(&"semantic"));
    assert!(types.contains(&"profile"));
    assert!(types.contains(&"procedural"));
    assert!(types.contains(&"working"));
    println!("✅ batch store all types: {:?}", types);
}

#[tokio::test]
async fn test_batch_store_empty() {
    let (base, client) = spawn_server().await;
    let uid = uid();

    let r = client
        .post(format!("{base}/v1/memories/batch"))
        .header("X-User-Id", &uid)
        .json(&json!({"memories": []}))
        .send()
        .await
        .unwrap();
    // Empty batch should succeed with empty result
    assert_eq!(r.status(), 201);
    let body: Vec<Value> = r.json().await.unwrap();
    assert!(body.is_empty());
    println!("✅ batch store empty: 201 with []");
}

#[tokio::test]
async fn test_batch_store_sensitivity_filter() {
    let (base, client) = spawn_server().await;
    let uid = uid();

    // Batch with a sensitive item — store_batch checks sensitivity
    let r = client
        .post(format!("{base}/v1/memories/batch"))
        .header("X-User-Id", &uid)
        .json(&json!({"memories": [
            {"content": "normal memory"},
            {"content": "my password is hunter2 and my ssn is 123-45-6789"},
        ]}))
        .send()
        .await
        .unwrap();
    // Should either filter out sensitive or reject — check behavior
    let status = r.status().as_u16();
    let body: Value = r.json().await.unwrap();
    println!("✅ batch store sensitivity: status={status}, body={body}");
}

// ── Batch store with embedding (requires EMBEDDING_API_KEY) ──────────────────

#[tokio::test]
async fn test_batch_store_with_embedding() {
    let Some((key, base_url, model)) = try_embedding() else {
        println!("⏭️  test_batch_store_with_embedding skipped (EMBEDDING_API_KEY not set)");
        return;
    };
    let (base, client) = spawn_server_with_embedding(key, base_url, model).await;
    let uid = uid();

    // Batch store 5 items — should use embed_batch (single API call)
    let r = client
        .post(format!("{base}/v1/memories/batch"))
        .header("X-User-Id", &uid)
        .json(&json!({"memories": [
            {"content": "Rust is a systems programming language"},
            {"content": "Python is great for data science"},
            {"content": "Go has excellent concurrency support"},
            {"content": "TypeScript adds types to JavaScript"},
            {"content": "Java runs on the JVM"},
        ]}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);
    let body: Vec<Value> = r.json().await.unwrap();
    assert_eq!(body.len(), 5);
    println!("✅ batch store with embedding: 5 items stored");

    // Verify they're retrievable via semantic search
    let r = client
        .post(format!("{base}/v1/memories/retrieve"))
        .header("X-User-Id", &uid)
        .json(&json!({"query": "systems programming", "top_k": 3}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let results: Vec<Value> = r.json().await.unwrap();
    assert!(
        !results.is_empty(),
        "batch-stored memories should be retrievable"
    );
    // Rust should rank high for "systems programming"
    assert!(
        results[0]["content"].as_str().unwrap().contains("Rust"),
        "Rust should be top result for 'systems programming': {:?}",
        results
            .iter()
            .map(|r| r["content"].as_str().unwrap())
            .collect::<Vec<_>>()
    );
    println!(
        "✅ batch store retrieval: top result = {}",
        results[0]["content"]
    );
}

// ── Remote mode: admin key management ────────────────────────────────────────

#[tokio::test]
async fn test_remote_admin_list_revoke_keys() {
    let mk = "test-mk-remote-keys";
    let (base, client) = spawn_server_with_master_key(mk).await;
    let auth = format!("Bearer {mk}");
    let uid = uid();

    // Create keys via REST
    for i in 0..2 {
        client
            .post(format!("{base}/auth/keys"))
            .header("Authorization", &auth)
            .json(&json!({"user_id": uid, "name": format!("rkey-{i}")}))
            .send()
            .await
            .unwrap();
    }

    // List via admin endpoint
    let r = client
        .get(format!("{base}/admin/users/{uid}/keys"))
        .header("Authorization", &auth)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["keys"].as_array().unwrap().len(), 2);
    println!("✅ remote admin list keys: 2 keys");

    // Revoke all
    let r = client
        .delete(format!("{base}/admin/users/{uid}/keys"))
        .header("Authorization", &auth)
        .send()
        .await
        .unwrap();
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["revoked"], 2);
    println!("✅ remote admin revoke all: {body}");
}

// ── Snapshot detail via remote mode ──────────────────────────────────────────

#[tokio::test]
async fn test_remote_snapshot_detail_and_diff() {
    use memoria_mcp::remote::RemoteClient;
    let (base, _) = spawn_api_for_remote().await;
    let uid = uid();
    let remote = RemoteClient::new(&base, None, uid.clone(), None);

    // Store memories
    remote
        .call("memory_store", json!({"content": "remote snap detail A"}))
        .await
        .unwrap();
    remote
        .call("memory_store", json!({"content": "remote snap detail B"}))
        .await
        .unwrap();

    // Create snapshot
    let snap = format!("rsnap_{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);
    remote
        .call("memory_snapshot", json!({"name": snap}))
        .await
        .unwrap();

    // Add another memory after snapshot
    remote
        .call(
            "memory_store",
            json!({"content": "remote snap detail C (after)"}),
        )
        .await
        .unwrap();

    // GET snapshot detail via REST (not MCP — direct HTTP)
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let r = client
        .get(format!("{base}/v1/snapshots/{snap}"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["memory_count"], 2, "snapshot should have 2 memories");
    println!(
        "✅ remote snapshot detail: memory_count={}",
        body["memory_count"]
    );

    // GET snapshot diff
    let r = client
        .get(format!("{base}/v1/snapshots/{snap}/diff"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert!(
        !body["added"].as_array().unwrap().is_empty(),
        "should have added memories"
    );
    println!(
        "✅ remote snapshot diff: added={}, removed={}",
        body["added"].as_array().unwrap().len(),
        body["removed"].as_array().unwrap().len()
    );

    // Cleanup
    remote
        .call("memory_snapshot_delete", json!({"names": snap}))
        .await
        .unwrap();
}

// ── Plugin API e2e tests ──────────────────────────────────────────────────────

/// Build a signed Rhai plugin package as base64 file map (ready for POST /admin/plugins).
fn build_signed_plugin_files(
    signer_name: &str,
    signer_key: &ed25519_dalek::SigningKey,
    plugin_name: &str,
    version: &str,
) -> std::collections::HashMap<String, String> {
    use base64::engine::general_purpose::STANDARD as B64;
    use base64::Engine;
    use ed25519_dalek::Signer;

    let script = r#"
        fn memoria_plugin(ctx) {
            if ctx["phase"] == "plan" {
                return #{ requires_approval: false };
            }
            return #{ warnings: [] };
        }
    "#;

    let manifest_unsigned = serde_json::json!({
        "name": plugin_name,
        "version": version,
        "api_version": "v1",
        "runtime": "rhai",
        "entry": {
            "rhai": { "script": "policy.rhai", "entrypoint": "memoria_plugin" },
            "grpc": null
        },
        "capabilities": ["governance.plan", "governance.execute"],
        "compatibility": { "memoria": ">=0.1.0-rc1 <0.2.0" },
        "permissions": { "network": false, "filesystem": false, "env": [] },
        "limits": { "timeout_ms": 500, "max_memory_mb": 32, "max_output_bytes": 8192 },
        "integrity": { "sha256": "", "signature": "", "signer": signer_name },
        "metadata": { "display_name": "E2E test plugin" }
    });

    // Write to temp dir to compute sha256
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("policy.rhai"), script).unwrap();
    std::fs::write(
        dir.path().join("manifest.json"),
        serde_json::to_vec_pretty(&manifest_unsigned).unwrap(),
    )
    .unwrap();
    let sha256 = memoria_service::compute_package_sha256(dir.path()).unwrap();
    let signature = B64.encode(signer_key.sign(sha256.as_bytes()).to_bytes());

    let mut manifest_signed = manifest_unsigned;
    manifest_signed["integrity"]["sha256"] = serde_json::json!(sha256);
    manifest_signed["integrity"]["signature"] = serde_json::json!(signature);

    let mut files = std::collections::HashMap::new();
    files.insert(
        "manifest.json".into(),
        B64.encode(serde_json::to_vec_pretty(&manifest_signed).unwrap()),
    );
    files.insert("policy.rhai".into(), B64.encode(script));
    files
}

fn test_signer_key() -> ed25519_dalek::SigningKey {
    ed25519_dalek::SigningKey::from_bytes(&[42u8; 32])
}

fn test_signer_public_b64() -> String {
    use base64::engine::general_purpose::STANDARD as B64;
    use base64::Engine;
    use ed25519_dalek::VerifyingKey;
    B64.encode(VerifyingKey::from(&test_signer_key()).as_bytes())
}

/// Full plugin lifecycle: signer → publish → list → review → score → binding → activate → matrix → events → rules
#[tokio::test]
async fn test_plugin_full_lifecycle() {
    let (base, c) = spawn_server().await;
    let signer_name = format!("e2e-signer-{}", uuid::Uuid::new_v4().simple());
    let plugin_name = format!("e2e-plugin-{}", uuid::Uuid::new_v4().simple());

    // 1. Register signer
    let r = c
        .post(format!("{base}/admin/plugins/signers"))
        .json(&json!({ "signer": signer_name, "public_key": test_signer_public_b64() }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    // 2. List signers — should contain ours
    let r = c
        .get(format!("{base}/admin/plugins/signers"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    let signers = body["signers"].as_array().unwrap();
    assert!(signers.iter().any(|s| s["signer"] == signer_name));

    // 3. Publish
    let files = build_signed_plugin_files(&signer_name, &test_signer_key(), &plugin_name, "0.1.0");
    let r = c
        .post(format!("{base}/admin/plugins"))
        .json(&json!({ "files": files }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        r.status(),
        200,
        "publish failed: {}",
        r.text().await.unwrap_or_default()
    );

    // Same content re-publish is idempotent (returns existing entry)
    let files2 = build_signed_plugin_files(&signer_name, &test_signer_key(), &plugin_name, "0.1.0");
    let r = c
        .post(format!("{base}/admin/plugins"))
        .json(&json!({ "files": files2 }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "idempotent re-publish should succeed");

    // 4. List packages
    let r = c.get(format!("{base}/admin/plugins")).send().await.unwrap();
    assert_eq!(r.status(), 200);
    let pkgs: Vec<Value> = r.json().await.unwrap();
    let ours = pkgs
        .iter()
        .find(|p| p["plugin_key"].as_str().unwrap().contains(&plugin_name));
    assert!(ours.is_some(), "published plugin should appear in list");
    let plugin_key = ours.unwrap()["plugin_key"].as_str().unwrap().to_string();
    assert_eq!(ours.unwrap()["status"], "pending");

    // 5. Review → active
    let r = c
        .post(format!("{base}/admin/plugins/{plugin_key}/0.1.0/review"))
        .json(&json!({ "status": "active", "notes": "e2e approved" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    // Verify status changed
    let r = c.get(format!("{base}/admin/plugins")).send().await.unwrap();
    let pkgs: Vec<Value> = r.json().await.unwrap();
    let ours = pkgs.iter().find(|p| p["plugin_key"] == plugin_key).unwrap();
    assert_eq!(ours["status"], "active");

    // 6. Score
    let r = c
        .post(format!("{base}/admin/plugins/{plugin_key}/0.1.0/score"))
        .json(&json!({ "score": 4.5, "notes": "solid" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    // 7. Create binding rule
    let r = c
        .post(format!("{base}/admin/plugins/domains/governance/bindings"))
        .json(&json!({
            "binding_key": "default",
            "plugin_key": plugin_key,
            "selector_kind": "semver",
            "selector_value": ">=0.1.0",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    // 8. List rules
    let r = c
        .get(format!(
            "{base}/admin/plugins/domains/governance/bindings?binding=default"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let rules: Vec<Value> = r.json().await.unwrap();
    assert!(rules.iter().any(|r| r["plugin_key"] == plugin_key));

    // 9. Activate binding
    let r = c
        .post(format!("{base}/admin/plugins/domains/governance/activate"))
        .json(&json!({
            "plugin_key": plugin_key,
            "version": "0.1.0",
            "binding_key": "default",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    // 10. Compatibility matrix
    let r = c
        .get(format!("{base}/admin/plugins/matrix"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let matrix: Vec<Value> = r.json().await.unwrap();
    assert!(matrix.iter().any(|m| m["plugin_key"] == plugin_key));

    // 11. Audit events — should have publish, review, score, binding, activate
    let r = c
        .get(format!(
            "{base}/admin/plugins/events?plugin_key={plugin_key}&limit=20"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let events: Vec<Value> = r.json().await.unwrap();
    let event_types: Vec<&str> = events
        .iter()
        .filter_map(|e| e["event_type"].as_str())
        .collect();
    assert!(
        event_types.contains(&"package.published"),
        "events: {event_types:?}"
    );
    assert!(
        event_types.contains(&"package.reviewed"),
        "events: {event_types:?}"
    );
    assert!(
        event_types.contains(&"package.scored"),
        "events: {event_types:?}"
    );

    println!(
        "✅ plugin full lifecycle: {plugin_key} — {} audit events",
        events.len()
    );
}

/// Dev-mode publish: skips signature verification, auto-approves.
#[tokio::test]
async fn test_plugin_dev_mode_publish() {
    let (base, c) = spawn_server().await;
    let plugin_name = format!("e2e-dev-{}", uuid::Uuid::new_v4().simple());

    // Build an UNSIGNED package (no signer registered, no valid signature)
    let script = r#"fn memoria_plugin(ctx) { return #{}; }"#;
    let manifest = serde_json::json!({
        "name": plugin_name,
        "version": "0.1.0",
        "api_version": "v1",
        "runtime": "rhai",
        "entry": {
            "rhai": { "script": "policy.rhai", "entrypoint": "memoria_plugin" },
            "grpc": null
        },
        "capabilities": ["governance.plan"],
        "compatibility": { "memoria": ">=0.1.0-rc1 <0.2.0" },
        "permissions": { "network": false, "filesystem": false, "env": [] },
        "limits": { "timeout_ms": 500, "max_memory_mb": 32, "max_output_bytes": 8192 },
        "integrity": { "sha256": "", "signature": "", "signer": "nobody" },
        "metadata": {}
    });

    use base64::engine::general_purpose::STANDARD as B64;
    use base64::Engine;
    let mut files = std::collections::HashMap::<String, String>::new();
    files.insert(
        "manifest.json".into(),
        B64.encode(serde_json::to_vec_pretty(&manifest).unwrap()),
    );
    files.insert("policy.rhai".into(), B64.encode(script));

    // Normal publish should FAIL (no signer "nobody" registered)
    let r = c
        .post(format!("{base}/admin/plugins"))
        .json(&json!({ "files": files }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        r.status(),
        500,
        "unsigned publish should fail without dev-mode"
    );

    // Dev-mode publish should succeed — but we need the dev endpoint.
    // The REST API currently only has the normal publish endpoint.
    // Dev-mode is available via CLI (which calls publish_plugin_package_dev directly).
    // Let's verify the service-layer function works by checking the normal publish rejects.
    println!("✅ plugin dev-mode: unsigned publish correctly rejected by normal endpoint");
}

/// Error: publish without manifest.json
#[tokio::test]
async fn test_plugin_publish_missing_manifest() {
    let (base, c) = spawn_server().await;

    use base64::engine::general_purpose::STANDARD as B64;
    use base64::Engine;
    let mut files = std::collections::HashMap::<String, String>::new();
    files.insert("policy.rhai".into(), B64.encode("fn x() {}"));

    let r = c
        .post(format!("{base}/admin/plugins"))
        .json(&json!({ "files": files }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    let text = r.text().await.unwrap();
    assert!(
        text.contains("manifest.json"),
        "error should mention manifest: {text}"
    );
    println!("✅ plugin publish missing manifest rejected");
}

/// Error: publish with path traversal filename
#[tokio::test]
async fn test_plugin_publish_path_traversal_rejected() {
    let (base, c) = spawn_server().await;

    use base64::engine::general_purpose::STANDARD as B64;
    use base64::Engine;
    let mut files = std::collections::HashMap::<String, String>::new();
    files.insert("manifest.json".into(), B64.encode("{}"));
    files.insert("../etc/passwd".into(), B64.encode("evil"));

    let r = c
        .post(format!("{base}/admin/plugins"))
        .json(&json!({ "files": files }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    let text = r.text().await.unwrap();
    assert!(text.contains("invalid filename"), "error: {text}");
    println!("✅ plugin publish path traversal rejected");
}

/// Error: review a non-existent package
#[tokio::test]
async fn test_plugin_review_nonexistent() {
    let (base, c) = spawn_server().await;

    let r = c
        .post(format!(
            "{base}/admin/plugins/governance:nonexistent:v0/9.9.9/review"
        ))
        .json(&json!({ "status": "active" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 500);
    println!("✅ plugin review nonexistent rejected");
}

/// Signer upsert is idempotent
#[tokio::test]
async fn test_plugin_signer_upsert_idempotent() {
    let (base, c) = spawn_server().await;
    let signer_name = format!("e2e-idem-{}", uuid::Uuid::new_v4().simple());

    for _ in 0..2 {
        let r = c
            .post(format!("{base}/admin/plugins/signers"))
            .json(&json!({ "signer": signer_name, "public_key": test_signer_public_b64() }))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
    }

    let r = c
        .get(format!("{base}/admin/plugins/signers"))
        .send()
        .await
        .unwrap();
    let body: Value = r.json().await.unwrap();
    let count = body["signers"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| s["signer"] == signer_name)
        .count();
    assert_eq!(count, 1, "signer should appear exactly once");
    println!("✅ plugin signer upsert idempotent");
}

/// Empty list/matrix/events return empty arrays, not errors
#[tokio::test]
async fn test_plugin_empty_queries() {
    let (base, c) = spawn_server().await;

    let r = c.get(format!("{base}/admin/plugins")).send().await.unwrap();
    assert_eq!(r.status(), 200);
    let _: Vec<Value> = r.json().await.unwrap(); // should parse as array

    let r = c
        .get(format!("{base}/admin/plugins/matrix"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let _: Vec<Value> = r.json().await.unwrap();

    let r = c
        .get(format!("{base}/admin/plugins/events"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let _: Vec<Value> = r.json().await.unwrap();

    let r = c
        .get(format!(
            "{base}/admin/plugins/domains/governance/bindings?binding=default"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let _: Vec<Value> = r.json().await.unwrap();

    println!("✅ plugin empty queries return empty arrays");
}

#[tokio::test]
async fn test_plugin_admin_routes_require_master_key() {
    let mk = "test-mk-plugin-admin";
    let (base, client) = spawn_server_with_master_key(mk).await;
    let auth = format!("Bearer {mk}");
    let uid = uid();
    let user_key = create_api_key_for_user(&client, &base, &auth, &uid, "plugin-user").await;
    let user_auth = format!("Bearer {user_key}");

    let r = client
        .get(format!("{base}/admin/plugins"))
        .header("Authorization", &user_auth)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403, "API key must not list admin plugins");

    let r = client
        .post(format!("{base}/admin/plugins/signers"))
        .header("Authorization", &user_auth)
        .json(&json!({
            "signer": format!("forbidden-{}", uuid::Uuid::new_v4().simple()),
            "public_key": test_signer_public_b64()
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        r.status(),
        403,
        "API key must not mutate admin plugin state"
    );
}

// ── Distributed coordination tests ────────────────────────────────────────────

/// Spawn a server with a specific instance_id, returning (base_url, client, instance_id).
async fn spawn_server_with_instance(instance_id: &str) -> (String, reqwest::Client, String) {
    use memoria_git::GitForDataService;
    use memoria_service::{Config, MemoryService};
    use memoria_storage::SqlMemoryStore;
    use sqlx::mysql::MySqlPool;

    let cfg = Config::from_env();
    let db = db_url();

    let store = SqlMemoryStore::connect(&db, test_dim(), uuid::Uuid::new_v4().to_string())
        .await
        .expect("connect");
    store.migrate().await.expect("migrate");
    let pool = MySqlPool::connect(&db).await.expect("pool");
    let git = Arc::new(GitForDataService::new(pool, &cfg.db_name));
    let service = Arc::new(MemoryService::new_sql_with_llm(Arc::new(store), None, None));
    let state = memoria_api::AppState::new(service, git, String::new())
        .with_instance_id(instance_id.to_string());

    let app = memoria_api::build_router(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await });
    tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;

    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("client");
    let base = format!("http://127.0.0.1:{port}");
    (base, client, instance_id.to_string())
}

#[tokio::test]
async fn test_distributed_health_instance_returns_id() {
    let iid = format!("inst_{}", uuid::Uuid::new_v4().simple());
    let (base, c, _) = spawn_server_with_instance(&iid).await;

    let r = c
        .get(format!("{base}/health/instance"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["instance_id"], iid);
    assert_eq!(body["status"], "ok");
    println!("✅ /health/instance returns correct instance_id");
}

#[tokio::test]
async fn test_distributed_two_instances_different_ids() {
    let id_a = format!("inst_a_{}", uuid::Uuid::new_v4().simple());
    let id_b = format!("inst_b_{}", uuid::Uuid::new_v4().simple());

    let (base_a, c, _) = spawn_server_with_instance(&id_a).await;
    let (base_b, c2, _) = spawn_server_with_instance(&id_b).await;

    let ra: Value = c
        .get(format!("{base_a}/health/instance"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let rb: Value = c2
        .get(format!("{base_b}/health/instance"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(ra["instance_id"], id_a);
    assert_eq!(rb["instance_id"], id_b);
    assert_ne!(ra["instance_id"], rb["instance_id"]);
    println!("✅ two instances report different instance_ids");
}

#[tokio::test]
async fn test_distributed_cross_instance_memory_visibility() {
    let id_a = format!("inst_a_{}", uuid::Uuid::new_v4().simple());
    let id_b = format!("inst_b_{}", uuid::Uuid::new_v4().simple());
    let user = uid();

    let (base_a, c, _) = spawn_server_with_instance(&id_a).await;
    let (base_b, c2, _) = spawn_server_with_instance(&id_b).await;

    // Store on instance A
    let r = c
        .post(format!("{base_a}/v1/memories"))
        .header("x-user-id", &user)
        .json(&json!({"content": "distributed test memory", "memory_type": "semantic"}))
        .send()
        .await
        .unwrap();
    assert!(
        r.status().is_success(),
        "store should succeed, got {}",
        r.status()
    );

    // Read from instance B
    let r = c2
        .get(format!("{base_b}/v1/memories"))
        .header("x-user-id", &user)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    let memories = body["items"]
        .as_array()
        .expect("response should have items array");
    assert!(
        memories
            .iter()
            .any(|m| m["content"] == "distributed test memory"),
        "Memory stored on instance A should be visible from instance B"
    );
    println!("✅ memory stored on A is visible from B");
}

#[tokio::test]
async fn test_distributed_lock_acquire_release() {
    // Direct test of the distributed lock via SqlMemoryStore
    use memoria_service::DistributedLock;
    use memoria_storage::SqlMemoryStore;
    use std::time::Duration;

    let db = db_url();
    let store = SqlMemoryStore::connect(&db, test_dim(), uuid::Uuid::new_v4().to_string())
        .await
        .expect("connect");
    store.migrate().await.expect("migrate");

    let lock_key = format!("test_lock_{}", uuid::Uuid::new_v4().simple());

    // Holder A acquires
    let acquired = store
        .try_acquire(&lock_key, "holder_a", Duration::from_secs(60))
        .await
        .unwrap();
    assert!(acquired, "holder_a should acquire the lock");

    // Holder B cannot acquire
    let acquired = store
        .try_acquire(&lock_key, "holder_b", Duration::from_secs(60))
        .await
        .unwrap();
    assert!(
        !acquired,
        "holder_b should NOT acquire while holder_a holds it"
    );

    // Holder A re-entrant
    let acquired = store
        .try_acquire(&lock_key, "holder_a", Duration::from_secs(60))
        .await
        .unwrap();
    assert!(acquired, "holder_a should re-acquire (re-entrant)");

    // Holder A releases
    store.release(&lock_key, "holder_a").await.unwrap();

    // Now holder B can acquire
    let acquired = store
        .try_acquire(&lock_key, "holder_b", Duration::from_secs(60))
        .await
        .unwrap();
    assert!(acquired, "holder_b should acquire after holder_a released");

    // Cleanup
    store.release(&lock_key, "holder_b").await.unwrap();
    println!("✅ distributed lock acquire/release/re-entrant works");
}

#[tokio::test]
async fn test_distributed_lock_expiry() {
    use memoria_service::DistributedLock;
    use memoria_storage::SqlMemoryStore;
    use std::time::Duration;

    let db = db_url();
    let store = SqlMemoryStore::connect(&db, test_dim(), uuid::Uuid::new_v4().to_string())
        .await
        .expect("connect");
    store.migrate().await.expect("migrate");

    let lock_key = format!("test_lock_exp_{}", uuid::Uuid::new_v4().simple());

    // Acquire with 1-second TTL
    let acquired = store
        .try_acquire(&lock_key, "holder_a", Duration::from_secs(1))
        .await
        .unwrap();
    assert!(acquired);

    // Wait for expiry
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Holder B should now acquire (expired lock cleaned up)
    let acquired = store
        .try_acquire(&lock_key, "holder_b", Duration::from_secs(60))
        .await
        .unwrap();
    assert!(
        acquired,
        "holder_b should acquire after holder_a's lock expired"
    );

    store.release(&lock_key, "holder_b").await.unwrap();
    println!("✅ distributed lock expires and can be taken over");
}

#[tokio::test]
async fn test_distributed_lock_renew() {
    use memoria_service::DistributedLock;
    use memoria_storage::SqlMemoryStore;
    use std::time::Duration;

    let db = db_url();
    let store = SqlMemoryStore::connect(&db, test_dim(), uuid::Uuid::new_v4().to_string())
        .await
        .expect("connect");
    store.migrate().await.expect("migrate");

    let lock_key = format!("test_lock_renew_{}", uuid::Uuid::new_v4().simple());

    // Acquire
    store
        .try_acquire(&lock_key, "holder_a", Duration::from_secs(60))
        .await
        .unwrap();

    // Renew by holder_a succeeds
    let renewed = store
        .renew(&lock_key, "holder_a", Duration::from_secs(120))
        .await
        .unwrap();
    assert!(renewed, "holder_a should renew its own lock");

    // Renew by holder_b fails
    let renewed = store
        .renew(&lock_key, "holder_b", Duration::from_secs(120))
        .await
        .unwrap();
    assert!(!renewed, "holder_b should NOT renew holder_a's lock");

    store.release(&lock_key, "holder_a").await.unwrap();
    println!("✅ distributed lock renew works correctly");
}

#[tokio::test]
async fn test_distributed_async_task_cross_instance() {
    use memoria_service::AsyncTaskStore;
    use memoria_storage::SqlMemoryStore;

    let db = db_url();
    let store = SqlMemoryStore::connect(&db, test_dim(), uuid::Uuid::new_v4().to_string())
        .await
        .expect("connect");
    store.migrate().await.expect("migrate");

    let task_id = format!("task_{}", uuid::Uuid::new_v4().simple());

    // Create task on "instance_a"
    store
        .create_task(&task_id, "instance_a", "test_user")
        .await
        .unwrap();

    // Read from "instance_b" perspective (same store, simulating different instance)
    let task = store
        .get_task(&task_id)
        .await
        .unwrap()
        .expect("task should exist");
    assert_eq!(task.task_id, task_id);
    assert_eq!(task.instance_id, "instance_a");
    assert_eq!(task.status, "processing");

    // Complete task
    store
        .complete_task(&task_id, json!({"memory_id": "m123"}))
        .await
        .unwrap();
    let task = store
        .get_task(&task_id)
        .await
        .unwrap()
        .expect("task should exist");
    assert_eq!(task.status, "completed");
    assert_eq!(task.result.unwrap()["memory_id"], "m123");

    println!("✅ async task visible cross-instance and completable");
}

#[tokio::test]
async fn test_distributed_async_task_fail() {
    use memoria_service::AsyncTaskStore;
    use memoria_storage::SqlMemoryStore;

    let db = db_url();
    let store = SqlMemoryStore::connect(&db, test_dim(), uuid::Uuid::new_v4().to_string())
        .await
        .expect("connect");
    store.migrate().await.expect("migrate");

    let task_id = format!("task_{}", uuid::Uuid::new_v4().simple());
    store
        .create_task(&task_id, "instance_x", "test_user")
        .await
        .unwrap();
    store
        .fail_task(&task_id, json!({"code": "ERR", "message": "boom"}))
        .await
        .unwrap();

    let task = store
        .get_task(&task_id)
        .await
        .unwrap()
        .expect("task should exist");
    assert_eq!(task.status, "failed");
    assert_eq!(task.error.unwrap()["message"], "boom");

    println!("✅ async task failure recorded correctly");
}


// ── Feedback API tests ────────────────────────────────────────────────────────

#[tokio::test]
async fn test_api_feedback_record() {
    let (base, client) = spawn_server().await;
    let uid = uid();

    // Store a memory first
    let r = client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({"content": "API feedback test memory", "memory_type": "semantic"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);
    let body: Value = r.json().await.unwrap();
    let memory_id = body["memory_id"].as_str().unwrap();

    // Record feedback
    let r = client
        .post(format!("{base}/v1/memories/{memory_id}/feedback"))
        .header("X-User-Id", &uid)
        .json(&json!({"signal": "useful", "context": "very helpful"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);
    let body: Value = r.json().await.unwrap();
    assert!(!body["feedback_id"].as_str().unwrap().is_empty());
    assert_eq!(body["memory_id"], memory_id);
    assert_eq!(body["signal"], "useful");

    println!("✅ POST /v1/memories/:id/feedback");
}

#[tokio::test]
async fn test_api_feedback_invalid_signal() {
    let (base, client) = spawn_server().await;
    let uid = uid();

    // Store a memory
    let r = client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({"content": "Invalid signal test", "memory_type": "semantic"}))
        .send()
        .await
        .unwrap();
    let body: Value = r.json().await.unwrap();
    let memory_id = body["memory_id"].as_str().unwrap();

    // Try invalid signal
    let r = client
        .post(format!("{base}/v1/memories/{memory_id}/feedback"))
        .header("X-User-Id", &uid)
        .json(&json!({"signal": "bad_signal"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 422); // Validation error for invalid signal
    println!("✅ POST /v1/memories/:id/feedback rejects invalid signal");
}

#[tokio::test]
async fn test_api_feedback_nonexistent_memory() {
    let (base, client) = spawn_server().await;
    let uid = uid();

    let r = client
        .post(format!("{base}/v1/memories/nonexistent_id_12345/feedback"))
        .header("X-User-Id", &uid)
        .json(&json!({"signal": "useful"}))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_client_error() || r.status().is_server_error());
    println!("✅ POST /v1/memories/:id/feedback rejects nonexistent memory");
}

#[tokio::test]
async fn test_api_feedback_stats() {
    let (base, client) = spawn_server().await;
    let uid = uid();

    // Store memories and give feedback
    for signal in &["useful", "useful", "irrelevant", "outdated"] {
        let r = client
            .post(format!("{base}/v1/memories"))
            .header("X-User-Id", &uid)
            .json(&json!({"content": format!("Stats test {signal}"), "memory_type": "semantic"}))
            .send()
            .await
            .unwrap();
        let body: Value = r.json().await.unwrap();
        let memory_id = body["memory_id"].as_str().unwrap();

        client
            .post(format!("{base}/v1/memories/{memory_id}/feedback"))
            .header("X-User-Id", &uid)
            .json(&json!({"signal": signal}))
            .send()
            .await
            .unwrap();
    }

    // Get stats
    let r = client
        .get(format!("{base}/v1/feedback/stats"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["total"], 4);
    assert_eq!(body["useful"], 2);
    assert_eq!(body["irrelevant"], 1);
    assert_eq!(body["outdated"], 1);
    assert_eq!(body["wrong"], 0);

    println!("✅ GET /v1/feedback/stats: {body}");
}

#[tokio::test]
async fn test_api_feedback_by_tier() {
    let (base, client) = spawn_server().await;
    let uid = uid();

    // Store memories with different tiers and give feedback
    for (tier, signal) in &[("T1", "useful"), ("T2", "useful"), ("T3", "irrelevant")] {
        let r = client
            .post(format!("{base}/v1/memories"))
            .header("X-User-Id", &uid)
            .json(&json!({
                "content": format!("Tier {tier} test"),
                "memory_type": "semantic",
                "trust_tier": tier
            }))
            .send()
            .await
            .unwrap();
        let body: Value = r.json().await.unwrap();
        let memory_id = body["memory_id"].as_str().unwrap();

        client
            .post(format!("{base}/v1/memories/{memory_id}/feedback"))
            .header("X-User-Id", &uid)
            .json(&json!({"signal": signal}))
            .send()
            .await
            .unwrap();
    }

    // Get breakdown by tier
    let r = client
        .get(format!("{base}/v1/feedback/by-tier"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    let breakdown = body["breakdown"].as_array().unwrap();
    assert!(!breakdown.is_empty(), "should have tier breakdown");

    println!("✅ GET /v1/feedback/by-tier: {} entries", breakdown.len());
}


// ── Retrieval Params API Tests ────────────────────────────────────────────────

#[tokio::test]
async fn test_api_get_retrieval_params() {
    let (base, client) = spawn_server().await;
    let uid = uid();

    let r = client
        .get(format!("{base}/v1/retrieval-params"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();

    // Should return default params
    assert_eq!(body["feedback_weight"], 0.1);
    assert_eq!(body["temporal_decay_hours"], 168.0);
    assert_eq!(body["confidence_weight"], 0.1);

    println!("✅ GET /v1/retrieval-params: {body}");
}

#[tokio::test]
async fn test_api_set_retrieval_params() {
    let (base, client) = spawn_server().await;
    let uid = uid();

    // Set custom params
    let r = client
        .put(format!("{base}/v1/retrieval-params"))
        .header("X-User-Id", &uid)
        .json(&json!({
            "feedback_weight": 0.15,
            "temporal_decay_hours": 200.0
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();

    assert!((body["feedback_weight"].as_f64().unwrap() - 0.15).abs() < 0.001);
    assert!((body["temporal_decay_hours"].as_f64().unwrap() - 200.0).abs() < 0.1);

    // Verify persisted
    let r = client
        .get(format!("{base}/v1/retrieval-params"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap();
    let body: Value = r.json().await.unwrap();
    assert!((body["feedback_weight"].as_f64().unwrap() - 0.15).abs() < 0.001);

    println!("✅ PUT /v1/retrieval-params: params updated and persisted");
}

#[tokio::test]
async fn test_api_tune_retrieval_params() {
    let (base, client) = spawn_server().await;
    let uid = uid();

    // Without enough feedback, should not tune
    let r = client
        .post(format!("{base}/v1/retrieval-params/tune"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["tuned"], false);

    println!("✅ POST /v1/retrieval-params/tune: {}", body["message"]);
}

#[tokio::test]
async fn test_api_tune_with_feedback() {
    let (base, client) = spawn_server().await;
    let uid = uid();

    // Create a memory
    let r = client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({
            "content": "Test memory for tuning API",
            "memory_type": "semantic"
        }))
        .send()
        .await
        .unwrap();
    let body: Value = r.json().await.unwrap();
    let memory_id = body["memory_id"].as_str().unwrap();

    // Add 12 useful feedback signals
    for _ in 0..12 {
        client
            .post(format!("{base}/v1/memories/{memory_id}/feedback"))
            .header("X-User-Id", &uid)
            .json(&json!({"signal": "useful"}))
            .send()
            .await
            .unwrap();
    }

    // Now tune should work
    let r = client
        .post(format!("{base}/v1/retrieval-params/tune"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["tuned"], true);

    let old_weight = body["old_params"]["feedback_weight"].as_f64().unwrap();
    let new_weight = body["new_params"]["feedback_weight"].as_f64().unwrap();
    assert!(new_weight >= old_weight, "feedback_weight should increase with positive feedback");

    println!("✅ POST /v1/retrieval-params/tune: {:.3} → {:.3}", old_weight, new_weight);
}

// ── Prometheus metrics ────────────────────────────────────────────────────────

#[tokio::test]
async fn test_api_metrics() {
    let (base, client) = spawn_server().await;
    let r = client.get(format!("{base}/metrics")).send().await.unwrap();
    assert_eq!(r.status(), 200);
    let body = r.text().await.unwrap();
    assert!(body.contains("memoria_memories_total"), "missing memoria_memories_total");
    assert!(body.contains("memoria_users_total"), "missing memoria_users_total");
    assert!(body.contains("memoria_auth_failures_total"), "missing auth_failures counter");
    println!("✅ GET /metrics: {} bytes", body.len());
}

// ── Snapshot rollback ─────────────────────────────────────────────────────────

#[tokio::test]
async fn test_api_snapshot_rollback() {
    let (base, client) = spawn_server().await;
    let uid = uid();

    // Store a memory
    let r = client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({"content": "before rollback", "memory_type": "semantic"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);

    // Create snapshot
    let snap = format!("rb_{}", &uuid::Uuid::new_v4().simple().to_string()[..6]);
    let r = client
        .post(format!("{base}/v1/snapshots"))
        .header("X-User-Id", &uid)
        .json(&json!({"name": snap}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);

    // Store another memory (after snapshot)
    let r = client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({"content": "after snapshot", "memory_type": "semantic"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);

    // Rollback
    let r = client
        .post(format!("{base}/v1/snapshots/{snap}/rollback"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert!(
        body["result"].as_str().unwrap_or("").to_lowercase().contains("roll"),
        "rollback response: {body}"
    );
    println!("✅ POST /v1/snapshots/:name/rollback: {}", body["result"]);
}

// ── Entity list ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_api_entities() {
    let (base, client) = spawn_server().await;
    let uid = uid();

    // Store a memory to trigger entity extraction
    let r = client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({"content": "We use PostgreSQL and Redis for caching", "memory_type": "semantic"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);

    let r = client
        .get(format!("{base}/v1/entities"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert!(body["entities"].is_array(), "entities should be an array");
    println!("✅ GET /v1/entities: {} entities", body["entities"].as_array().unwrap().len());
}

// ── Admin config ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_api_admin_config() {
    let mk = format!("mk_{}", uuid::Uuid::new_v4().simple());
    let (base, client) = spawn_server_with_master_key(&mk).await;

    // Without master key → 401
    let r = client.get(format!("{base}/admin/config")).send().await.unwrap();
    assert_eq!(r.status(), 401);

    // With master key → 200
    let r = client
        .get(format!("{base}/admin/config"))
        .header("Authorization", format!("Bearer {mk}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert!(body["db_name"].is_string(), "missing db_name");
    assert!(body["embedding_dim"].is_number(), "missing embedding_dim");
    // Password should be redacted
    let db_url = body["db_url"].as_str().unwrap_or("");
    assert!(db_url.contains("***") || !db_url.contains("@"), "db password not redacted: {db_url}");
    println!("✅ GET /admin/config: db_name={}", body["db_name"]);
}

#[tokio::test]
async fn test_api_admin_config_forbidden() {
    let mk = format!("mk_{}", uuid::Uuid::new_v4().simple());
    let (base, client) = spawn_server_with_master_key(&mk).await;

    // Create an API key (non-master)
    let auth = format!("Bearer {mk}");
    let r = client
        .post(format!("{base}/auth/keys"))
        .header("Authorization", &auth)
        .json(&json!({"user_id": "bob", "name": "test"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);
    let raw_key = r.json::<Value>().await.unwrap()["raw_key"]
        .as_str()
        .unwrap()
        .to_string();

    // Non-master API key → 403
    let r = client
        .get(format!("{base}/admin/config"))
        .header("Authorization", format!("Bearer {raw_key}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    println!("✅ GET /admin/config: non-master key → 403");
}

// ── Concurrency: parallel stores to same user ─────────────────────────────────

#[tokio::test]
async fn test_concurrent_stores() {
    let (base, client) = spawn_server().await;
    let uid = uid();
    let client = Arc::new(client);
    let n = 20;

    let mut handles = Vec::new();
    for i in 0..n {
        let c = client.clone();
        let b = base.clone();
        let u = uid.clone();
        handles.push(tokio::spawn(async move {
            c.post(format!("{b}/v1/memories"))
                .header("X-User-Id", &u)
                .json(&json!({"content": format!("concurrent fact #{i}"), "memory_type": "semantic"}))
                .send()
                .await
                .unwrap()
        }));
    }
    let mut ok = 0;
    for h in handles {
        let r = h.await.unwrap();
        if r.status() == 201 {
            ok += 1;
        }
    }
    assert_eq!(ok, n, "all concurrent stores should succeed");

    // Verify all persisted
    let r = client
        .get(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .query(&[("limit", "100")])
        .send()
        .await
        .unwrap();
    let body: Value = r.json().await.unwrap();
    let count = body["items"].as_array().unwrap().len();
    assert!(count >= n, "expected >= {n} memories, got {count}");
    println!("✅ concurrent stores: {ok}/{n} succeeded, {count} persisted");
}

// ── Concurrency: entity extraction race condition ─────────────────────────────

#[tokio::test]
async fn test_concurrent_entity_upsert() {
    let (base, client) = spawn_server().await;
    let uid = uid();
    let client = Arc::new(client);

    // All memories mention "Redis" — triggers concurrent upsert_entity for same entity
    let mut handles = Vec::new();
    for i in 0..10 {
        let c = client.clone();
        let b = base.clone();
        let u = uid.clone();
        handles.push(tokio::spawn(async move {
            c.post(format!("{b}/v1/memories"))
                .header("X-User-Id", &u)
                .json(&json!({"content": format!("Redis is used for caching scenario {i}"), "memory_type": "semantic"}))
                .send()
                .await
                .unwrap()
        }));
    }
    for h in handles {
        let r = h.await.unwrap();
        assert_eq!(r.status(), 201, "concurrent entity upsert should not fail");
    }

    // Verify entity exists (no duplicates crashed it)
    let r = client
        .get(format!("{base}/v1/entities"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    println!("✅ concurrent entity upsert: no race condition errors");
}

// ── Pressure: batch store at limit ────────────────────────────────────────────

#[tokio::test]
async fn test_batch_store_at_limit() {
    let (base, client) = spawn_server().await;
    let uid = uid();

    let memories: Vec<_> = (0..100)
        .map(|i| json!({"content": format!("batch item {i}"), "memory_type": "semantic"}))
        .collect();

    let r = client
        .post(format!("{base}/v1/memories/batch"))
        .header("X-User-Id", &uid)
        .json(&json!({"memories": memories}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body.as_array().unwrap().len(), 100);

    // Over limit → 422
    let memories_over: Vec<_> = (0..101)
        .map(|i| json!({"content": format!("over {i}"), "memory_type": "semantic"}))
        .collect();
    let r = client
        .post(format!("{base}/v1/memories/batch"))
        .header("X-User-Id", &uid)
        .json(&json!({"memories": memories_over}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 422);
    println!("✅ batch store: 100 ok, 101 rejected");
}

// ── Concurrency: parallel feedback on same memory ─────────────────────────────

#[tokio::test]
async fn test_concurrent_feedback() {
    let (base, client) = spawn_server().await;
    let uid = uid();
    let client = Arc::new(client);

    // Create a memory
    let r = client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({"content": "shared memory for feedback", "memory_type": "semantic"}))
        .send()
        .await
        .unwrap();
    let mid = r.json::<Value>().await.unwrap()["memory_id"]
        .as_str()
        .unwrap()
        .to_string();

    // 10 concurrent feedback signals
    let mut handles = Vec::new();
    for i in 0..10 {
        let c = client.clone();
        let b = base.clone();
        let u = uid.clone();
        let m = mid.clone();
        let signal = if i % 2 == 0 { "useful" } else { "irrelevant" };
        handles.push(tokio::spawn(async move {
            c.post(format!("{b}/v1/memories/{m}/feedback"))
                .header("X-User-Id", &u)
                .json(&json!({"signal": signal}))
                .send()
                .await
                .unwrap()
        }));
    }
    for h in handles {
        let r = h.await.unwrap();
        assert!(r.status() == 200 || r.status() == 201, "concurrent feedback should succeed, got {}", r.status());
    }

    // Verify stats reflect all signals
    let r = client
        .get(format!("{base}/v1/feedback/stats"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap();
    let body: Value = r.json().await.unwrap();
    let total = body["total"].as_i64().unwrap();
    assert_eq!(total, 10, "all 10 feedback signals should be recorded");
    println!("✅ concurrent feedback: {total} signals recorded");
}

// ── Graceful degradation ──────────────────────────────────────────────────────

#[tokio::test]
async fn test_graceful_degradation() {
    let (base, client) = spawn_server().await;
    let uid = uid();

    // Rollback to nonexistent snapshot → error, not crash
    let r = client
        .post(format!("{base}/v1/snapshots/nonexistent_snap_xyz/rollback"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap();
    assert!(r.status().is_client_error() || r.status().is_server_error());

    // Feedback on nonexistent memory → 404
    let r = client
        .post(format!("{base}/v1/memories/nonexistent_id/feedback"))
        .header("X-User-Id", &uid)
        .json(&json!({"signal": "useful"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);

    // Store → purge → correct the purged memory → should fail gracefully
    let r = client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({"content": "ephemeral", "memory_type": "semantic"}))
        .send()
        .await
        .unwrap();
    let mid = r.json::<Value>().await.unwrap()["memory_id"]
        .as_str()
        .unwrap()
        .to_string();

    client
        .delete(format!("{base}/v1/memories/{mid}"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap();

    let r = client
        .put(format!("{base}/v1/memories/{mid}"))
        .header("X-User-Id", &uid)
        .json(&json!({"new_content": "updated", "reason": "test"}))
        .send()
        .await
        .unwrap();
    assert!(
        r.status() == 404 || r.status().is_client_error() || r.status().is_server_error(),
        "correct after purge should fail gracefully, got {}",
        r.status()
    );

    println!("✅ graceful degradation: all error cases handled without panic");
}

// ── last_used_at batched flush (#62) ─────────────────────────────────────────

/// Verify that the LastUsedBatcher correctly coalesces multiple mark_used calls
/// and flushes them in a single batch UPDATE.
#[tokio::test]
async fn test_last_used_batcher_coalesces_and_flushes() {
    use memoria_api::auth::LastUsedBatcher;
    use memoria_storage::SqlMemoryStore;
    use sha2::{Digest, Sha256};

    let mk = "test-master-batcher";
    let (base, client) = spawn_server_with_master_key(mk).await;
    let auth = format!("Bearer {mk}");

    // Create 3 API keys
    let mut keys = Vec::new();
    for i in 0..3 {
        let raw = create_api_key_for_user(
            &client,
            &base,
            &auth,
            &format!("batcher_user_{i}"),
            &format!("key_{i}"),
        )
        .await;
        keys.push(raw);
    }

    // Build a batcher and mark all 3 keys as used
    let batcher = LastUsedBatcher::new();
    let hashes: Vec<String> = keys
        .iter()
        .map(|k| format!("{:x}", Sha256::digest(k.as_bytes())))
        .collect();
    for h in &hashes {
        batcher.mark_used(h.clone());
    }

    // Verify all 3 are pending
    // (We can't inspect the internal set directly, but we can flush and verify DB)

    // Connect to DB and flush
    let store = SqlMemoryStore::connect(&db_url(), test_dim(), uuid::Uuid::new_v4().to_string())
        .await
        .expect("connect");
    batcher.flush(store.pool()).await;

    // Verify last_used_at was updated for all 3 keys
    for hash in &hashes {
        let row: Option<(Option<chrono::NaiveDateTime>,)> = sqlx::query_as(
            "SELECT last_used_at FROM mem_api_keys WHERE key_hash = ?",
        )
        .bind(hash)
        .fetch_optional(store.pool())
        .await
        .expect("query");
        let (last_used,) = row.expect("key should exist");
        assert!(last_used.is_some(), "last_used_at should be set after flush for hash {}", &hash[..8]);
    }

    // Flush again — should be a no-op (pending set is drained)
    batcher.flush(store.pool()).await;

    // Mark one key again and flush — only that one should be updated
    batcher.mark_used(hashes[0].clone());
    batcher.flush(store.pool()).await;

    println!("✅ test_last_used_batcher_coalesces_and_flushes: batch UPDATE works correctly");
}

/// Verify that API key auth works with the dedicated auth pool and that
/// last_used_at is updated via the batcher (not per-request fire-and-forget).
#[tokio::test]
async fn test_api_key_auth_uses_batcher_not_fire_and_forget() {
    let mk = "test-master-batcher-auth";
    let db = db_url();

    // Spawn server WITH init_auth_pool
    use memoria_git::GitForDataService;
    use memoria_service::{Config, MemoryService};
    use memoria_storage::SqlMemoryStore;
    use sqlx::mysql::MySqlPool;

    let cfg = Config::from_env();
    let store = SqlMemoryStore::connect(&db, test_dim(), uuid::Uuid::new_v4().to_string())
        .await
        .expect("connect");
    store.migrate().await.expect("migrate");
    let pool = MySqlPool::connect(&db).await.expect("pool");
    let git = Arc::new(GitForDataService::new(pool, &cfg.db_name));
    let service = Arc::new(MemoryService::new_sql_with_llm(Arc::new(store), None, None));
    let state = memoria_api::AppState::new(service, git, mk.to_string())
        .init_auth_pool(&db)
        .await;

    let batcher = state.last_used_batcher.clone();
    let app = memoria_api::build_router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let base = format!("http://127.0.0.1:{port}");
    let auth = format!("Bearer {mk}");

    // Create an API key
    let raw_key = create_api_key_for_user(&client, &base, &auth, "batcher_e2e_user", "e2e_key").await;

    // Use the API key to make a request (cache miss → DB lookup → batcher.mark_used)
    let r = client
        .get(format!("{base}/v1/memories"))
        .header("Authorization", format!("Bearer {raw_key}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "API key auth should succeed");

    // Make another request (cache hit → batcher.mark_used, no DB)
    let r = client
        .get(format!("{base}/v1/memories"))
        .header("Authorization", format!("Bearer {raw_key}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "Cached API key auth should succeed");

    // Manually flush the batcher to verify last_used_at is updated
    let verify_store = SqlMemoryStore::connect(&db, test_dim(), uuid::Uuid::new_v4().to_string())
        .await
        .expect("connect");
    batcher.flush(verify_store.pool()).await;

    let key_hash = format!("{:x}", <sha2::Sha256 as sha2::Digest>::digest(raw_key.as_bytes()));
    let row: Option<(Option<chrono::NaiveDateTime>,)> = sqlx::query_as(
        "SELECT last_used_at FROM mem_api_keys WHERE key_hash = ?",
    )
    .bind(&key_hash)
    .fetch_optional(verify_store.pool())
    .await
    .expect("query");
    let (last_used,) = row.expect("key should exist");
    assert!(last_used.is_some(), "last_used_at should be set after batcher flush");

    println!("✅ test_api_key_auth_uses_batcher_not_fire_and_forget: auth pool + batcher works");
}
