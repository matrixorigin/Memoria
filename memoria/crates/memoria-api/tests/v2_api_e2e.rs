use chrono::{Duration, Utc};
use memoria_core::{MemoryType, TrustTier};
use memoria_storage::{MemoryV2RememberInput, SqlMemoryStore};
use serde_json::{json, Value};
use std::sync::Arc;
use std::sync::OnceLock;
use tokio::sync::Mutex;

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

fn isolated_db_url() -> String {
    let base = db_url();
    let Some((prefix, db_name)) = base.rsplit_once('/') else {
        return base;
    };
    format!("{prefix}/{}_{}", db_name, uuid::Uuid::new_v4().simple())
}

fn db_name_from_url(db: &str) -> String {
    db.rsplit('/').next().unwrap_or("memoria").to_string()
}

fn uid() -> String {
    format!("api_v2_test_{}", uuid::Uuid::new_v4().simple())
}

const V2_WAIT_ATTEMPTS: usize = 500;
const V2_WAIT_SLEEP_MS: u64 = 300;

fn heavy_flow_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
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
                tokio::time::sleep(tokio::time::Duration::from_millis(
                    50 * (attempt as u64 + 1),
                ))
                .await;
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
                tokio::time::sleep(tokio::time::Duration::from_millis(
                    50 * (attempt as u64 + 1),
                ))
                .await;
            }
            Err(err) => panic!("pool: {err:?}"),
        }
    }
    panic!(
        "pool: {}",
        last_error.unwrap_or_else(|| "unknown pool error".to_string())
    );
}

async fn wait_for_views(
    base: &str,
    client: &reqwest::Client,
    user_id: &str,
    memory_id: &str,
) -> Value {
    for _ in 0..V2_WAIT_ATTEMPTS {
        let response = client
            .post(format!("{base}/v2/memory/expand"))
            .header("X-User-Id", user_id)
            .json(&json!({
                "memory_id": memory_id,
                "level": "links"
            }))
            .send()
            .await
            .expect("expand views");
        assert_eq!(response.status(), 200);
        let body: Value = response.json().await.expect("expand views json");
        if !body["overview"].is_null() && !body["detail"].is_null() {
            return body;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(V2_WAIT_SLEEP_MS)).await;
    }
    panic!("timed out waiting for V2 derived views");
}

async fn wait_for_link_count(
    base: &str,
    client: &reqwest::Client,
    user_id: &str,
    memory_id: &str,
    direction: &str,
    min_count: usize,
) -> Value {
    wait_for_filtered_link_count(base, client, user_id, memory_id, direction, None, min_count).await
}

async fn wait_for_filtered_link_count(
    base: &str,
    client: &reqwest::Client,
    user_id: &str,
    memory_id: &str,
    direction: &str,
    link_type: Option<&str>,
    min_count: usize,
) -> Value {
    for _ in 0..V2_WAIT_ATTEMPTS {
        let mut query = vec![
            ("memory_id", memory_id.to_string()),
            ("direction", direction.to_string()),
            ("limit", "10".to_string()),
        ];
        if let Some(link_type) = link_type {
            query.push(("link_type", link_type.to_string()));
        }
        let response = client
            .get(format!("{base}/v2/memory/links"))
            .header("X-User-Id", user_id)
            .query(&query)
            .send()
            .await
            .expect("list links");
        assert_eq!(response.status(), 200);
        let body: Value = response.json().await.expect("links json");
        if body["items"]
            .as_array()
            .map(|items| items.len() >= min_count)
            .unwrap_or(false)
        {
            return body;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(V2_WAIT_SLEEP_MS)).await;
    }
    panic!("timed out waiting for V2 links");
}

async fn wait_for_jobs_done(
    base: &str,
    client: &reqwest::Client,
    user_id: &str,
    memory_id: &str,
) -> Value {
    for _ in 0..V2_WAIT_ATTEMPTS {
        let response = client
            .get(format!("{base}/v2/memory/jobs"))
            .header("X-User-Id", user_id)
            .query(&[("memory_id", memory_id), ("limit", "10")])
            .send()
            .await
            .expect("jobs");
        assert_eq!(response.status(), 200);
        let body: Value = response.json().await.expect("jobs json");
        if body["pending_count"].as_u64().unwrap_or(1) == 0
            && body["in_progress_count"].as_u64().unwrap_or(1) == 0
            && body["failed_count"].as_u64().unwrap_or_default() == 0
            && body["derivation_state"] == "complete"
        {
            return body;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(V2_WAIT_SLEEP_MS)).await;
    }
    panic!("timed out waiting for V2 jobs");
}

async fn wait_for_recall_memory(
    base: &str,
    client: &reqwest::Client,
    user_id: &str,
    query: &str,
    session_id: &str,
    memory_id: &str,
) -> Value {
    for _ in 0..V2_WAIT_ATTEMPTS {
        let response = client
            .post(format!("{base}/v2/memory/recall"))
            .header("X-User-Id", user_id)
            .json(&json!({
                "query": query,
                "top_k": 5,
                "scope": "session",
                "session_id": session_id,
                "view": "full"
            }))
            .send()
            .await
            .expect("recall with links");
        assert_eq!(response.status(), 200);
        let body: Value = response.json().await.expect("recall with links json");
        if body["memories"]
            .as_array()
            .and_then(|memories| memories.iter().find(|memory| memory["id"] == memory_id))
            .map(|memory| !memory["overview"].is_null())
            .unwrap_or(false)
        {
            return body;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(V2_WAIT_SLEEP_MS)).await;
    }
    panic!("timed out waiting for V2 recall memory");
}

async fn spawn_server_with_llm(
    llm: Option<Arc<memoria_embedding::LlmClient>>,
) -> (String, reqwest::Client) {
    use memoria_core::interfaces::EmbeddingProvider;
    use memoria_embedding::MockEmbedder;
    use memoria_git::GitForDataService;
    use memoria_service::MemoryService;
    let db = isolated_db_url();

    let store = SqlMemoryStore::connect(&db, test_dim(), uuid::Uuid::new_v4().to_string())
        .await
        .expect("connect");
    migrate_store_with_retry(&store).await;
    let pool = connect_pool_with_retry(&db).await;
    let git = Arc::new(GitForDataService::new(pool, db_name_from_url(&db)));
    let embedder: Arc<dyn EmbeddingProvider> = Arc::new(MockEmbedder::new(test_dim()));
    let service = Arc::new(MemoryService::new_sql_with_llm(
        Arc::new(store),
        Some(embedder),
        llm,
    ).await);
    let state = memoria_api::AppState::new(service, git, String::new());
    let app = memoria_api::build_router(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let port = listener.local_addr().expect("local addr").port();
    let handle = tokio::spawn(async move { axum::serve(listener, app).await });

    tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;
    assert!(!handle.is_finished(), "server exited unexpectedly");

    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("client");
    (format!("http://127.0.0.1:{port}"), client)
}

async fn spawn_server_for_store_with_llm(
    db: &str,
    store: Arc<SqlMemoryStore>,
    llm: Option<Arc<memoria_embedding::LlmClient>>,
) -> (String, reqwest::Client) {
    use memoria_core::interfaces::EmbeddingProvider;
    use memoria_embedding::MockEmbedder;
    use memoria_git::GitForDataService;
    use memoria_service::MemoryService;
    let pool = connect_pool_with_retry(db).await;
    let git = Arc::new(GitForDataService::new(pool, db_name_from_url(db)));
    let embedder: Arc<dyn EmbeddingProvider> = Arc::new(MockEmbedder::new(test_dim()));
    let service = Arc::new(MemoryService::new_sql_with_llm(store, Some(embedder), llm).await);
    let state = memoria_api::AppState::new(service, git, String::new());
    let app = memoria_api::build_router(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let port = listener.local_addr().expect("local addr").port();
    let handle = tokio::spawn(async move { axum::serve(listener, app).await });

    tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;
    assert!(!handle.is_finished(), "server exited unexpectedly");

    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("client");
    (format!("http://127.0.0.1:{port}"), client)
}

async fn spawn_server() -> (String, reqwest::Client) {
    spawn_server_with_llm(None).await
}

#[tokio::test]
async fn test_api_v2_memory_flow() {
    let _guard = heavy_flow_lock().lock().await;
    let (base, client) = spawn_server().await;
    let user_id = uid();

    let remember = client
        .post(format!("{base}/v2/memory/remember"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "content": "Rust platform guide for systems teams working on shared infrastructure services. This content is long enough to trigger view derivation so that overview and detail text are populated. Platform engineers use Rust ownership and borrowing to build safe and reliable distributed systems.",
            "type": "semantic",
            "session_id": "sess-v2",
            "importance": 0.6,
            "trust_tier": "T2",
            "tags": ["rust", "systems"],
            "source": {"kind": "chat", "app": "copilot"}
        }))
        .send()
        .await
        .expect("remember");
    assert_eq!(remember.status(), 201);
    let remembered: Value = remember.json().await.expect("remember json");
    let memory_id = remembered["memory_id"]
        .as_str()
        .expect("memory_id")
        .to_string();
    assert!(remembered["abstract"]
        .as_str()
        .unwrap_or("")
        .starts_with("Rust platform guide for systems teams"));

    let second_remember = client
        .post(format!("{base}/v2/memory/remember"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "content": "Rust service handbook for infrastructure teams building shared platform services. This content is long enough to trigger derive_views so overview and detail are populated after job processing. Infrastructure engineers depend on Rust memory safety for critical systems work. Teams also document reliability playbooks for incident response.",
            "type": "semantic",
            "session_id": "sess-v2",
            "tags": ["rust", "infra"]
        }))
        .send()
        .await
        .expect("remember second");
    assert_eq!(second_remember.status(), 201);
    let second_remembered: Value = second_remember.json().await.expect("remember second json");
    let second_memory_id = second_remembered["memory_id"]
        .as_str()
        .expect("second memory_id")
        .to_string();

    let _ = wait_for_views(&base, &client, &user_id, &memory_id).await;
    let _ = wait_for_jobs_done(&base, &client, &user_id, &memory_id).await;
    let _ = wait_for_views(&base, &client, &user_id, &second_memory_id).await;
    let _ = wait_for_jobs_done(&base, &client, &user_id, &second_memory_id).await;
    let _ = wait_for_link_count(&base, &client, &user_id, &memory_id, "both", 1).await;
    let enriched = client
        .post(format!("{base}/v2/memory/expand"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "memory_id": memory_id,
            "level": "links"
        }))
        .send()
        .await
        .expect("expand enriched");
    assert_eq!(enriched.status(), 200);
    let enriched: Value = enriched.json().await.expect("expand enriched json");
    assert!(enriched["overview"].as_str().unwrap_or("").contains("Rust"));
    assert!(enriched["detail"]
        .as_str()
        .unwrap_or("")
        .contains("systems teams"));
    let direct_links = client
        .get(format!("{base}/v2/memory/links"))
        .header("X-User-Id", &user_id)
        .query(&[
            ("memory_id", memory_id.as_str()),
            ("direction", "both"),
            ("limit", "10"),
        ])
        .send()
        .await
        .expect("direct links");
    assert_eq!(direct_links.status(), 200);
    let direct_links_body: Value = direct_links.json().await.expect("direct links json");
    let direct_links_items = direct_links_body["items"]
        .as_array()
        .expect("direct link items");
    assert!(!direct_links_items.is_empty());
    assert!(direct_links_items
        .iter()
        .all(|link| !link["provenance"].is_null()));
    assert!(direct_links_items
        .iter()
        .all(|link| !link["provenance"]["primary_evidence_type"].is_null()));
    assert!(direct_links_items
        .iter()
        .all(|link| !link["provenance"]["primary_evidence_strength"].is_null()));
    assert!(direct_links_items
        .iter()
        .all(|link| link["provenance"]["refined"] == false));
    assert!(direct_links_items.iter().all(|link| {
        link["provenance"]["evidence"]
            .as_array()
            .map(|items| !items.is_empty())
            .unwrap_or(false)
    }));
    assert!(direct_links_items.iter().all(|link| {
        link["provenance"]["extraction_trace"]["content_version_id"].is_string()
            && link["provenance"]["extraction_trace"]["derivation_state"] == "complete"
            && link["provenance"]["extraction_trace"]["latest_job_status"] == "done"
            && !link["provenance"]["extraction_trace"]["latest_job_updated_at"].is_null()
    }));
    assert!(direct_links_items.iter().any(|link| {
        link["provenance"]["evidence"]
            .as_array()
            .map(|items| {
                items.iter().any(|detail| {
                    detail["type"] == "tag_overlap"
                        && detail["overlap_count"] == 1
                        && detail["source_tag_count"] == 2
                        && detail["target_tag_count"] == 2
                })
            })
            .unwrap_or(false)
    }));
    if let Some(enriched_links) = enriched["links"].as_array() {
        assert!(enriched_links
            .iter()
            .all(|link| !link["provenance"].is_null()));
    }

    let list = client
        .get(format!("{base}/v2/memory/list"))
        .header("X-User-Id", &user_id)
        .query(&[("session_id", "sess-v2"), ("limit", "10")])
        .send()
        .await
        .expect("list");
    assert_eq!(list.status(), 200);
    let listed: Value = list.json().await.expect("list json");
    let items = listed["items"].as_array().expect("items");
    assert_eq!(items.len(), 2);
    assert!(items.iter().any(|item| item["id"] == memory_id));
    assert!(items.iter().all(|item| item["type"].is_string()));
    assert!(items.iter().all(|item| item.get("memory_type").is_none()));

    let recalled =
        wait_for_recall_memory(&base, &client, &user_id, "platform", "sess-v2", &memory_id).await;
    let memories = recalled["memories"].as_array().expect("memories");
    assert_eq!(memories.len(), 2);
    assert!(recalled["token_used"].as_u64().unwrap_or_default() > 0);
    let recalled_memory = memories
        .iter()
        .find(|memory| memory["id"] == memory_id)
        .expect("recalled target memory");
    assert_eq!(recalled_memory["type"], "semantic");
    assert!(recalled_memory.get("memory_type").is_none());
    assert!(!recalled_memory["overview"].is_null());
    let recall_links = recalled_memory["links"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if !recall_links.is_empty() {
        assert!(recall_links
            .iter()
            .all(|link| !link["provenance"].is_null()));
        assert!(recall_links
            .iter()
            .all(|link| !link["provenance"]["primary_evidence_type"].is_null()));
        assert!(recall_links
            .iter()
            .all(|link| link["provenance"]["refined"] == false));
        assert!(recall_links.iter().all(|link| {
            link["provenance"]["evidence"]
                .as_array()
                .map(|items| !items.is_empty())
                .unwrap_or(false)
        }));
    }

    let expand = client
        .post(format!("{base}/v2/memory/expand"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "memory_id": memory_id,
            "level": "detail"
        }))
        .send()
        .await
        .expect("expand");
    assert_eq!(expand.status(), 200);
    let expanded: Value = expand.json().await.expect("expand json");
    assert!(expanded["abstract"]
        .as_str()
        .unwrap_or("")
        .starts_with("Rust platform guide for systems teams"));
    assert!(!expanded["overview"].is_null());
    assert!(!expanded["detail"].is_null());

    let focus = client
        .post(format!("{base}/v2/memory/focus"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "type": "topic",
            "value": "rust",
            "boost": 2.0,
            "ttl_secs": 300
        }))
        .send()
        .await
        .expect("focus");
    assert_eq!(focus.status(), 201);
    let focused: Value = focus.json().await.expect("focus json");
    assert_eq!(focused["type"], "topic");
    assert_eq!(focused["value"], "rust");

    let forget = client
        .post(format!("{base}/v2/memory/forget"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "memory_id": memory_id,
            "reason": "cleanup"
        }))
        .send()
        .await
        .expect("forget");
    assert_eq!(forget.status(), 200);
    let forgotten: Value = forget.json().await.expect("forget json");
    assert_eq!(forgotten["memory_id"], memory_id);
    assert_eq!(forgotten["forgotten"], true);

    let list_after_forget = client
        .get(format!("{base}/v2/memory/list"))
        .header("X-User-Id", &user_id)
        .query(&[("session_id", "sess-v2"), ("limit", "10")])
        .send()
        .await
        .expect("list after forget");
    assert_eq!(list_after_forget.status(), 200);
    let listed_after: Value = list_after_forget.json().await.expect("list after json");
    let remaining = listed_after["items"].as_array().expect("items");
    assert_eq!(remaining.len(), 1);
    assert!(remaining.iter().all(|item| item["id"] != memory_id));
}

#[tokio::test]
async fn test_api_v2_batch_remember_and_forget() {
    let (base, client) = spawn_server().await;
    let user_id = uid();

    let remember = client
        .post(format!("{base}/v2/memory/batch-remember"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "memories": [
                {
                    "content": "Rust systems handbook",
                    "session_id": "sess-batch-api",
                    "tags": ["Rust", "systems", "rust"]
                },
                {
                    "content": "Python data handbook",
                    "session_id": "sess-batch-api",
                    "tags": ["python", "data"]
                },
                {
                    "content": "Infra runbook for deployments",
                    "type": "procedural",
                    "session_id": "sess-batch-api",
                    "tags": ["infra"]
                }
            ]
        }))
        .send()
        .await
        .expect("batch remember");
    assert_eq!(remember.status(), 201);
    let remember_body: Value = remember.json().await.expect("batch remember json");
    let remembered = remember_body["memories"]
        .as_array()
        .expect("remembered memories");
    assert_eq!(remembered.len(), 3);
    let first_id = remembered[0]["memory_id"]
        .as_str()
        .expect("first memory id");
    let second_id = remembered[1]["memory_id"]
        .as_str()
        .expect("second memory id");
    let third_id = remembered[2]["memory_id"]
        .as_str()
        .expect("third memory id");

    let list = client
        .get(format!("{base}/v2/memory/list"))
        .header("X-User-Id", &user_id)
        .query(&[("session_id", "sess-batch-api"), ("limit", "10")])
        .send()
        .await
        .expect("batch list");
    assert_eq!(list.status(), 200);
    let list_body: Value = list.json().await.expect("batch list json");
    assert_eq!(list_body["items"].as_array().expect("batch items").len(), 3);

    let tags = client
        .get(format!("{base}/v2/memory/tags"))
        .header("X-User-Id", &user_id)
        .query(&[("limit", "10")])
        .send()
        .await
        .expect("batch tags");
    assert_eq!(tags.status(), 200);
    let tags_body: Value = tags.json().await.expect("batch tags json");
    let tag_items = tags_body["items"].as_array().expect("tag items");
    assert!(tag_items
        .iter()
        .any(|item| item["tag"] == "rust" && item["memory_count"] == 1));

    let forget = client
        .post(format!("{base}/v2/memory/batch-forget"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "memory_ids": [first_id, third_id, first_id],
            "reason": "cleanup"
        }))
        .send()
        .await
        .expect("batch forget");
    assert_eq!(forget.status(), 200);
    let forget_body: Value = forget.json().await.expect("batch forget json");
    let forgotten = forget_body["memories"]
        .as_array()
        .expect("forgotten memories");
    assert_eq!(forgotten.len(), 2);
    assert!(forgotten.iter().any(|item| item["memory_id"] == first_id));
    assert!(forgotten.iter().any(|item| item["memory_id"] == third_id));

    let listed_after_forget = client
        .get(format!("{base}/v2/memory/list"))
        .header("X-User-Id", &user_id)
        .query(&[("session_id", "sess-batch-api"), ("limit", "10")])
        .send()
        .await
        .expect("list after batch forget");
    assert_eq!(listed_after_forget.status(), 200);
    let listed_after_forget_body: Value = listed_after_forget
        .json()
        .await
        .expect("list after batch forget json");
    let remaining = listed_after_forget_body["items"]
        .as_array()
        .expect("remaining memories");
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0]["id"], second_id);
}

#[tokio::test]
async fn test_api_v2_batch_remember_rejects_large_batch() {
    let (base, client) = spawn_server().await;
    let user_id = uid();
    let memories = (0..101)
        .map(|idx| json!({ "content": format!("memory {idx}") }))
        .collect::<Vec<_>>();

    let response = client
        .post(format!("{base}/v2/memory/batch-remember"))
        .header("X-User-Id", &user_id)
        .json(&json!({ "memories": memories }))
        .send()
        .await
        .expect("oversized batch remember");
    assert_eq!(response.status(), 422);
    assert!(response
        .text()
        .await
        .expect("oversized batch remember text")
        .contains("batch exceeds 100 items"));
}

#[tokio::test]
async fn test_api_v2_profile_lists_only_v2_profile_memories() {
    let (base, client) = spawn_server().await;
    let user_id = uid();

    let v1_profile = client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "content": "Legacy V1 profile that must stay outside V2 profile surface",
            "type": "profile"
        }))
        .send()
        .await
        .expect("remember v1 profile");
    assert_eq!(v1_profile.status(), 201);
    let v1_profile_body: Value = v1_profile.json().await.expect("v1 profile json");
    let v1_profile_id = v1_profile_body["memory_id"]
        .as_str()
        .expect("v1 profile id")
        .to_string();

    let first_profile = client
        .post(format!("{base}/v2/memory/remember"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "content": "Prefers short commit messages",
            "type": "profile",
            "session_id": "sess-profile-a",
            "trust_tier": "T1"
        }))
        .send()
        .await
        .expect("remember first v2 profile");
    assert_eq!(first_profile.status(), 201);
    let first_profile_body: Value = first_profile.json().await.expect("first profile json");
    let first_profile_id = first_profile_body["memory_id"]
        .as_str()
        .expect("first profile id")
        .to_string();

    let semantic = client
        .post(format!("{base}/v2/memory/remember"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "content": "Semantic V2 memory that should not appear in profile listing",
            "type": "semantic",
            "session_id": "sess-profile-a"
        }))
        .send()
        .await
        .expect("remember semantic");
    assert_eq!(semantic.status(), 201);
    let semantic_body: Value = semantic.json().await.expect("semantic json");
    let semantic_id = semantic_body["memory_id"]
        .as_str()
        .expect("semantic id")
        .to_string();

    let second_profile = client
        .post(format!("{base}/v2/memory/remember"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "content": "Uses focused test runs before full validation",
            "type": "profile",
            "session_id": "sess-profile-b",
            "trust_tier": "T2",
            "importance": 0.7
        }))
        .send()
        .await
        .expect("remember second v2 profile");
    assert_eq!(second_profile.status(), 201);
    let second_profile_body: Value = second_profile.json().await.expect("second profile json");
    let second_profile_id = second_profile_body["memory_id"]
        .as_str()
        .expect("second profile id")
        .to_string();

    let listed = client
        .get(format!("{base}/v2/memory/profile"))
        .header("X-User-Id", &user_id)
        .query(&[("limit", "10")])
        .send()
        .await
        .expect("list v2 profiles");
    assert_eq!(listed.status(), 200);
    let listed_body: Value = listed.json().await.expect("v2 profile list json");
    let items = listed_body["items"].as_array().expect("profile items");
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["id"], second_profile_id);
    assert_eq!(
        items[0]["content"],
        "Uses focused test runs before full validation"
    );
    assert_eq!(items[0]["trust_tier"], "T2");
    assert_eq!(items[1]["id"], first_profile_id);
    assert!(items.iter().all(|item| item["id"] != v1_profile_id));
    assert!(items.iter().all(|item| item["id"] != semantic_id));

    let filtered = client
        .get(format!("{base}/v2/memory/profile"))
        .header("X-User-Id", &user_id)
        .query(&[("session_id", "sess-profile-a"), ("limit", "10")])
        .send()
        .await
        .expect("list filtered v2 profiles");
    assert_eq!(filtered.status(), 200);
    let filtered_body: Value = filtered.json().await.expect("filtered profile list json");
    let filtered_items = filtered_body["items"]
        .as_array()
        .expect("filtered profile items");
    assert_eq!(filtered_items.len(), 1);
    assert_eq!(filtered_items[0]["id"], first_profile_id);
    assert_eq!(filtered_items[0]["session_id"], "sess-profile-a");
    assert!(filtered_body["next_cursor"].is_null());
}

#[tokio::test]
async fn test_api_v2_entities_extract_list_refresh_and_isolate_from_v1() {
    let (base, client) = spawn_server().await;
    let user_id = uid();

    let v1_memory = client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "content": "Legacy zebra-gateway operations note",
            "memory_type": "semantic"
        }))
        .send()
        .await
        .expect("remember v1 memory");
    assert_eq!(v1_memory.status(), 201);

    let remembered = client
        .post(format!("{base}/v2/memory/remember"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "content": "Rust and Docker keep the auth-service deployable",
            "type": "semantic",
            "session_id": "sess-entities"
        }))
        .send()
        .await
        .expect("remember v2 entity memory");
    assert_eq!(remembered.status(), 201);
    let remembered_body: Value = remembered.json().await.expect("remembered body");
    let memory_id = remembered_body["memory_id"]
        .as_str()
        .expect("v2 memory id")
        .to_string();

    let _ = wait_for_jobs_done(&base, &client, &user_id, &memory_id).await;

    let listed = client
        .get(format!("{base}/v2/memory/entities"))
        .header("X-User-Id", &user_id)
        .query(&[("limit", "20")])
        .send()
        .await
        .expect("list v2 entities");
    assert_eq!(listed.status(), 200);
    let listed_body: Value = listed.json().await.expect("list entities body");
    let items = listed_body["items"].as_array().expect("entity items");
    let names = items
        .iter()
        .filter_map(|item| item["name"].as_str())
        .collect::<Vec<_>>();
    assert!(names.contains(&"rust"));
    assert!(names.contains(&"docker"));
    assert!(names.contains(&"auth-service"));
    assert!(!names.contains(&"zebra-gateway"));

    let filtered = client
        .get(format!("{base}/v2/memory/entities"))
        .header("X-User-Id", &user_id)
        .query(&[
            ("memory_id", memory_id.as_str()),
            ("query", "dock"),
            ("entity_type", "tech"),
            ("limit", "20"),
        ])
        .send()
        .await
        .expect("filter v2 entities");
    assert_eq!(filtered.status(), 200);
    let filtered_body: Value = filtered.json().await.expect("filtered entities body");
    let filtered_items = filtered_body["items"].as_array().expect("filtered items");
    assert_eq!(filtered_items.len(), 1);
    assert_eq!(filtered_items[0]["name"], "docker");

    let updated = client
        .patch(format!("{base}/v2/memory/update"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "memory_id": memory_id,
            "content": "Python powers the billing-gateway"
        }))
        .send()
        .await
        .expect("update v2 memory");
    assert_eq!(updated.status(), 200);

    let _ = wait_for_jobs_done(&base, &client, &user_id, &memory_id).await;

    let relisted = client
        .get(format!("{base}/v2/memory/entities"))
        .header("X-User-Id", &user_id)
        .query(&[("limit", "20")])
        .send()
        .await
        .expect("relist v2 entities");
    assert_eq!(relisted.status(), 200);
    let relisted_body: Value = relisted.json().await.expect("relisted entities body");
    let relisted_items = relisted_body["items"].as_array().expect("relisted items");
    let relisted_names = relisted_items
        .iter()
        .filter_map(|item| item["name"].as_str())
        .collect::<Vec<_>>();
    assert!(relisted_names.contains(&"python"));
    assert!(relisted_names.contains(&"billing-gateway"));
    assert!(!relisted_names.contains(&"rust"));
    assert!(!relisted_names.contains(&"docker"));

    let forgotten = client
        .post(format!("{base}/v2/memory/forget"))
        .header("X-User-Id", &user_id)
        .json(&json!({ "memory_id": memory_id }))
        .send()
        .await
        .expect("forget v2 memory");
    assert_eq!(forgotten.status(), 200);

    let after_forget = client
        .get(format!("{base}/v2/memory/entities"))
        .header("X-User-Id", &user_id)
        .query(&[("limit", "20")])
        .send()
        .await
        .expect("list entities after forget");
    assert_eq!(after_forget.status(), 200);
    let after_forget_body: Value = after_forget.json().await.expect("after forget body");
    let after_forget_items = after_forget_body["items"]
        .as_array()
        .expect("after forget items");
    assert!(after_forget_items.is_empty());
}

#[tokio::test]
async fn test_api_v2_reflect_returns_v2_only_linked_candidates() {
    let (base, client) = spawn_server().await;
    let user_id = uid();

    let v1_memory = client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "content": "Legacy V1 reflect note that must stay outside V2",
            "type": "semantic"
        }))
        .send()
        .await
        .expect("remember v1 reflect memory");
    assert_eq!(v1_memory.status(), 201);
    let v1_body: Value = v1_memory.json().await.expect("v1 reflect json");
    let v1_memory_id = v1_body["memory_id"]
        .as_str()
        .expect("v1 memory id")
        .to_string();

    let first = client
        .post(format!("{base}/v2/memory/remember"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "content": "Alpha platform memory connected through shared operations and cross-team workflows. This content is intentionally long enough to trigger derive_views so the reflect internal test waits on fully processed memories before asserting synthesized reflection links. Shared alpha tags connect this memory to related platform operations across sessions.",
            "session_id": "sess-reflect-a",
            "tags": ["shared", "alpha"],
            "importance": 0.6
        }))
        .send()
        .await
        .expect("remember reflect first");
    assert_eq!(first.status(), 201);
    let first_body: Value = first.json().await.expect("reflect first json");
    let first_id = first_body["memory_id"]
        .as_str()
        .expect("reflect first id")
        .to_string();

    let second = client
        .post(format!("{base}/v2/memory/remember"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "content": "Bridge operations memory joining alpha and beta service clusters across the platform. This content is also long enough to trigger derive_views so the reflect internal test runs after links and views are fully processed. Shared beta tags connect this bridge memory with both alpha and beta operational contexts.",
            "session_id": "sess-reflect-b",
            "tags": ["shared", "beta"],
            "importance": 0.8
        }))
        .send()
        .await
        .expect("remember reflect second");
    assert_eq!(second.status(), 201);
    let second_body: Value = second.json().await.expect("reflect second json");
    let second_id = second_body["memory_id"]
        .as_str()
        .expect("reflect second id")
        .to_string();

    let third = client
        .post(format!("{base}/v2/memory/remember"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "content": "Beta deployment memory linked through the bridge operations cluster in the platform. This content is long enough to trigger derive_views and keep the test aligned with other V2 suites that wait for fully processed memories. Beta and gamma tags connect this memory to shared deployment and incident workflows.",
            "session_id": "sess-reflect-b",
            "tags": ["beta", "gamma"],
            "importance": 0.5
        }))
        .send()
        .await
        .expect("remember reflect third");
    assert_eq!(third.status(), 201);
    let third_body: Value = third.json().await.expect("reflect third json");
    let third_id = third_body["memory_id"]
        .as_str()
        .expect("reflect third id")
        .to_string();

    let mut reflected_body = None;
    let _ = wait_for_jobs_done(&base, &client, &user_id, &first_id).await;
    let _ = wait_for_jobs_done(&base, &client, &user_id, &second_id).await;
    let _ = wait_for_jobs_done(&base, &client, &user_id, &third_id).await;
    for _ in 0..V2_WAIT_ATTEMPTS {
        let reflected = client
            .post(format!("{base}/v2/memory/reflect"))
            .header("X-User-Id", &user_id)
            .json(&json!({
                "mode": "auto",
                "limit": 10
            }))
            .send()
            .await
            .expect("reflect candidates");
        assert_eq!(reflected.status(), 200);
        let body: Value = reflected.json().await.expect("reflect body");
        let ready = body["candidates"]
            .as_array()
            .map(|candidates| {
                candidates.iter().any(|candidate| {
                    candidate["signal"] == "cross_session_linked_cluster"
                        && candidate["memory_count"] == 3
                })
            })
            .unwrap_or(false);
        if ready {
            reflected_body = Some(body);
            break;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(V2_WAIT_SLEEP_MS)).await;
    }
    let reflected_body = reflected_body.expect("reflect candidate body");
    assert_eq!(reflected_body["mode"], "auto");
    assert_eq!(reflected_body["synthesized"], false);
    assert_eq!(reflected_body["scenes_created"], 0);
    let candidates = reflected_body["candidates"]
        .as_array()
        .expect("reflect candidates array");
    assert!(!candidates.is_empty());
    let candidate = candidates
        .iter()
        .find(|candidate| candidate["signal"] == "cross_session_linked_cluster")
        .expect("cross session candidate");
    assert_eq!(candidate["memory_count"], 3);
    assert_eq!(candidate["session_count"], 2);
    assert!(candidate["link_count"].as_i64().unwrap_or_default() >= 2);
    let memories = candidate["memories"]
        .as_array()
        .expect("candidate memories");
    assert!(memories.iter().all(|memory| memory["type"].is_string()));
    assert!(memories
        .iter()
        .all(|memory| memory.get("memory_type").is_none()));
    assert!(memories.iter().any(|memory| memory["id"] == first_id));
    assert!(memories.iter().any(|memory| memory["id"] == second_id));
    assert!(memories.iter().any(|memory| memory["id"] == third_id));
    assert!(memories.iter().all(|memory| memory["id"] != v1_memory_id));
}

#[tokio::test]
async fn test_api_v2_reflect_falls_back_to_session_cluster() {
    let (base, client) = spawn_server().await;
    let user_id = uid();

    let first = client
        .post(format!("{base}/v2/memory/remember"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "content": "Notebook summary for a quiet retrospective",
            "session_id": "sess-reflect-fallback",
            "tags": ["notebook"],
            "importance": 0.4
        }))
        .send()
        .await
        .expect("remember fallback first");
    assert_eq!(first.status(), 201);
    let first_body: Value = first.json().await.expect("fallback first json");
    let first_id = first_body["memory_id"]
        .as_str()
        .expect("fallback first id")
        .to_string();

    let second = client
        .post(format!("{base}/v2/memory/remember"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "content": "Calendar reminder for the same session",
            "session_id": "sess-reflect-fallback",
            "tags": ["calendar"],
            "importance": 0.6
        }))
        .send()
        .await
        .expect("remember fallback second");
    assert_eq!(second.status(), 201);
    let second_body: Value = second.json().await.expect("fallback second json");
    let second_id = second_body["memory_id"]
        .as_str()
        .expect("fallback second id")
        .to_string();

    let reflected = client
        .post(format!("{base}/v2/memory/reflect"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "mode": "candidates",
            "session_id": "sess-reflect-fallback",
            "min_cluster_size": 2,
            "min_link_strength": 0.95
        }))
        .send()
        .await
        .expect("reflect fallback");
    assert_eq!(reflected.status(), 200);
    let reflected_body: Value = reflected.json().await.expect("reflect fallback body");
    let candidates = reflected_body["candidates"]
        .as_array()
        .expect("fallback candidates");
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0]["signal"], "session_cluster");
    assert_eq!(candidates[0]["memory_count"], 2);
    assert_eq!(candidates[0]["session_count"], 1);
    assert_eq!(candidates[0]["link_count"], 0);
    let memories = candidates[0]["memories"]
        .as_array()
        .expect("fallback memories");
    assert!(memories.iter().any(|memory| memory["id"] == first_id));
    assert!(memories.iter().any(|memory| memory["id"] == second_id));
}

#[tokio::test]
async fn test_api_v2_reflect_internal_writes_synthesized_memory_and_dedupes() {
    let (base, client) = spawn_server().await;
    let user_id = uid();

    let first = client
        .post(format!("{base}/v2/memory/remember"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "content": "Alpha platform memory connected through shared operations",
            "session_id": "sess-reflect-a",
            "tags": ["shared", "alpha"],
            "importance": 0.6
        }))
        .send()
        .await
        .expect("remember internal first");
    assert_eq!(first.status(), 201);
    let first_body: Value = first.json().await.expect("internal first json");
    let first_id = first_body["memory_id"]
        .as_str()
        .expect("internal first id")
        .to_string();

    let second = client
        .post(format!("{base}/v2/memory/remember"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "content": "Bridge operations memory joining alpha and beta",
            "session_id": "sess-reflect-b",
            "tags": ["shared", "beta"],
            "importance": 0.8
        }))
        .send()
        .await
        .expect("remember internal second");
    assert_eq!(second.status(), 201);
    let second_body: Value = second.json().await.expect("internal second json");
    let second_id = second_body["memory_id"]
        .as_str()
        .expect("internal second id")
        .to_string();

    let third = client
        .post(format!("{base}/v2/memory/remember"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "content": "Beta deployment memory linked through the bridge",
            "session_id": "sess-reflect-b",
            "tags": ["beta", "gamma"],
            "importance": 0.5
        }))
        .send()
        .await
        .expect("remember internal third");
    assert_eq!(third.status(), 201);
    let third_body: Value = third.json().await.expect("internal third json");
    let third_id = third_body["memory_id"]
        .as_str()
        .expect("internal third id")
        .to_string();

    let _ = wait_for_jobs_done(&base, &client, &user_id, &first_id).await;
    let _ = wait_for_jobs_done(&base, &client, &user_id, &second_id).await;
    let _ = wait_for_jobs_done(&base, &client, &user_id, &third_id).await;

    let mut reflect_ready = false;
    for _ in 0..V2_WAIT_ATTEMPTS {
        let reflected = client
            .post(format!("{base}/v2/memory/reflect"))
            .header("X-User-Id", &user_id)
            .json(&json!({
                "mode": "auto",
                "limit": 10
            }))
            .send()
            .await
            .expect("reflect readiness");
        assert_eq!(reflected.status(), 200);
        let body: Value = reflected.json().await.expect("reflect readiness body");
        let candidate_ready = body["candidates"]
            .as_array()
            .map(|candidates| {
                candidates.iter().any(|candidate| {
                    candidate["signal"] == "cross_session_linked_cluster"
                        && candidate["memory_count"] == 3
                })
            })
            .unwrap_or(false);
        if candidate_ready {
            reflect_ready = true;
            break;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(V2_WAIT_SLEEP_MS)).await;
    }
    assert!(
        reflect_ready,
        "timed out waiting for reflect candidate readiness"
    );

    let reflected = client
        .post(format!("{base}/v2/memory/reflect"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "mode": "internal",
            "limit": 10
        }))
        .send()
        .await
        .expect("reflect internal");
    assert_eq!(reflected.status(), 200);
    let reflected_body: Value = reflected.json().await.expect("reflect internal body");
    assert_eq!(reflected_body["mode"], "internal");
    assert_eq!(reflected_body["synthesized"], true);
    assert_eq!(reflected_body["scenes_created"], 1);
    let candidates = reflected_body["candidates"]
        .as_array()
        .expect("internal candidates");
    let candidate = candidates
        .iter()
        .find(|candidate| candidate["signal"] == "cross_session_linked_cluster")
        .expect("cross session internal candidate");
    let memories = candidate["memories"].as_array().expect("internal memories");
    assert!(memories.iter().any(|memory| memory["id"] == first_id));
    assert!(memories.iter().any(|memory| memory["id"] == second_id));
    assert!(memories.iter().any(|memory| memory["id"] == third_id));

    let listed = client
        .get(format!("{base}/v2/memory/list"))
        .header("X-User-Id", &user_id)
        .query(&[("limit", "10")])
        .send()
        .await
        .expect("list after internal reflect");
    assert_eq!(listed.status(), 200);
    let listed_body: Value = listed.json().await.expect("list body");
    let items = listed_body["items"].as_array().expect("list items");
    assert_eq!(items.len(), 4);
    let synthesized_id = items
        .iter()
        .map(|item| item["id"].as_str().unwrap_or_default())
        .find(|id| *id != first_id && *id != second_id && *id != third_id)
        .expect("synthesized id")
        .to_string();

    let reflection_links_body = wait_for_filtered_link_count(
        &base,
        &client,
        &user_id,
        &synthesized_id,
        "both",
        Some("reflection"),
        3,
    )
    .await;
    let link_items = reflection_links_body["items"]
        .as_array()
        .expect("reflection link items");
    assert_eq!(link_items.len(), 3);
    assert!(link_items.iter().all(|item| item["direction"] == "inbound"));
    assert!(link_items.iter().any(|item| item["id"] == first_id));
    assert!(link_items.iter().any(|item| item["id"] == second_id));
    assert!(link_items.iter().any(|item| item["id"] == third_id));

    let rerun = client
        .post(format!("{base}/v2/memory/reflect"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "mode": "internal",
            "limit": 10
        }))
        .send()
        .await
        .expect("reflect internal rerun");
    assert_eq!(rerun.status(), 200);
    let rerun_body: Value = rerun.json().await.expect("reflect rerun body");
    assert_eq!(rerun_body["scenes_created"], 0);

    let candidates_after = client
        .post(format!("{base}/v2/memory/reflect"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "mode": "candidates",
            "limit": 10
        }))
        .send()
        .await
        .expect("reflect candidates after synth");
    assert_eq!(candidates_after.status(), 200);
    let candidates_after_body: Value = candidates_after
        .json()
        .await
        .expect("reflect candidates after synth body");
    let candidates_after_items = candidates_after_body["candidates"]
        .as_array()
        .expect("candidates after synth array");
    assert!(candidates_after_items.iter().all(|candidate| {
        candidate["memories"]
            .as_array()
            .expect("candidate memories after synth")
            .iter()
            .all(|memory| memory["id"] != synthesized_id)
    }));
}

#[tokio::test]
async fn test_api_v2_feedback_records() {
    let (base, client) = spawn_server().await;
    let user_id = uid();

    let remembered = client
        .post(format!("{base}/v2/memory/remember"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "content": "Platform guide candidate positive",
            "session_id": "sess-feedback",
            "tags": ["shared"]
        }))
        .send()
        .await
        .expect("remember");
    assert_eq!(remembered.status(), 201);
    let remembered_body: Value = remembered.json().await.expect("remember json");
    let memory_id = remembered_body["memory_id"]
        .as_str()
        .expect("memory id")
        .to_string();

    let feedback = client
        .post(format!("{base}/v2/memory/{memory_id}/feedback"))
        .header("X-User-Id", &user_id)
        .json(&json!({ "signal": "useful", "context": "good match" }))
        .send()
        .await
        .expect("feedback");
    assert_eq!(feedback.status(), 201);
    let feedback_body: Value = feedback.json().await.expect("feedback json");
    assert_eq!(feedback_body["memory_id"], memory_id);
    assert_eq!(feedback_body["signal"], "useful");
    assert!(feedback_body["feedback_id"].as_str().unwrap_or("").len() > 8);
}

#[tokio::test]
async fn test_api_v2_feedback_rejects_invalid_input() {
    let (base, client) = spawn_server().await;
    let user_id = uid();

    let remembered = client
        .post(format!("{base}/v2/memory/remember"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "content": "Feedback validation target",
            "session_id": "sess-feedback"
        }))
        .send()
        .await
        .expect("remember");
    assert_eq!(remembered.status(), 201);
    let remembered_body: Value = remembered.json().await.expect("remember json");
    let memory_id = remembered_body["memory_id"].as_str().expect("memory id");

    let invalid = client
        .post(format!("{base}/v2/memory/{memory_id}/feedback"))
        .header("X-User-Id", &user_id)
        .json(&json!({ "signal": "bad_signal" }))
        .send()
        .await
        .expect("invalid feedback");
    assert_eq!(invalid.status(), 422);

    let missing = client
        .post(format!("{base}/v2/memory/nonexistent/feedback"))
        .header("X-User-Id", &user_id)
        .json(&json!({ "signal": "useful" }))
        .send()
        .await
        .expect("missing feedback");
    assert_eq!(missing.status(), 404);
}

#[tokio::test]
async fn test_api_v2_feedback_stats_and_breakdown() {
    let (base, client) = spawn_server().await;
    let user_id = uid();

    let trusted = client
        .post(format!("{base}/v2/memory/remember"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "content": "Trusted feedback target",
            "session_id": "sess-feedback-stats",
            "trust_tier": "T1",
            "tags": ["stats"]
        }))
        .send()
        .await
        .expect("remember trusted");
    assert_eq!(trusted.status(), 201);
    let trusted_body: Value = trusted.json().await.expect("trusted json");
    let trusted_id = trusted_body["memory_id"]
        .as_str()
        .expect("trusted id")
        .to_string();

    let curated = client
        .post(format!("{base}/v2/memory/remember"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "content": "Curated feedback target",
            "session_id": "sess-feedback-stats",
            "trust_tier": "T2",
            "tags": ["stats"]
        }))
        .send()
        .await
        .expect("remember curated");
    assert_eq!(curated.status(), 201);
    let curated_body: Value = curated.json().await.expect("curated json");
    let curated_id = curated_body["memory_id"]
        .as_str()
        .expect("curated id")
        .to_string();

    for (memory_id, signal) in [
        (&trusted_id, "useful"),
        (&curated_id, "irrelevant"),
        (&curated_id, "outdated"),
    ] {
        let feedback = client
            .post(format!("{base}/v2/memory/{memory_id}/feedback"))
            .header("X-User-Id", &user_id)
            .json(&json!({ "signal": signal }))
            .send()
            .await
            .expect("record feedback");
        assert_eq!(feedback.status(), 201);
    }

    let stats = client
        .get(format!("{base}/v2/feedback/stats"))
        .header("X-User-Id", &user_id)
        .send()
        .await
        .expect("feedback stats");
    assert_eq!(stats.status(), 200);
    let stats_body: Value = stats.json().await.expect("stats json");
    assert_eq!(stats_body["total"], 3);
    assert_eq!(stats_body["useful"], 1);
    assert_eq!(stats_body["irrelevant"], 1);
    assert_eq!(stats_body["outdated"], 1);
    assert_eq!(stats_body["wrong"], 0);

    let breakdown = client
        .get(format!("{base}/v2/feedback/by-tier"))
        .header("X-User-Id", &user_id)
        .send()
        .await
        .expect("feedback by tier");
    assert_eq!(breakdown.status(), 200);
    let breakdown_body: Value = breakdown.json().await.expect("breakdown json");
    let items = breakdown_body["breakdown"]
        .as_array()
        .expect("breakdown items");
    assert!(items
        .iter()
        .any(|item| item["tier"] == "T1" && item["signal"] == "useful" && item["count"] == 1));
    assert!(items.iter().any(|item| {
        item["tier"] == "T2" && item["signal"] == "irrelevant" && item["count"] == 1
    }));
    assert!(items
        .iter()
        .any(|item| item["tier"] == "T2" && item["signal"] == "outdated" && item["count"] == 1));
}

#[tokio::test]
async fn test_api_v2_memory_feedback_summary() {
    let (base, client) = spawn_server().await;
    let user_id = uid();

    let remembered = client
        .post(format!("{base}/v2/memory/remember"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "content": "Memory feedback summary target",
            "session_id": "sess-feedback-read",
            "tags": ["feedback"]
        }))
        .send()
        .await
        .expect("remember");
    assert_eq!(remembered.status(), 201);
    let remembered_body: Value = remembered.json().await.expect("remember json");
    let memory_id = remembered_body["memory_id"]
        .as_str()
        .expect("memory id")
        .to_string();

    let empty = client
        .get(format!("{base}/v2/memory/{memory_id}/feedback"))
        .header("X-User-Id", &user_id)
        .send()
        .await
        .expect("get empty feedback");
    assert_eq!(empty.status(), 200);
    let empty_body: Value = empty.json().await.expect("empty feedback json");
    assert_eq!(empty_body["memory_id"], memory_id);
    assert_eq!(empty_body["feedback"]["useful"], 0);
    assert_eq!(empty_body["feedback"]["irrelevant"], 0);
    assert_eq!(empty_body["feedback"]["outdated"], 0);
    assert_eq!(empty_body["feedback"]["wrong"], 0);
    assert!(empty_body["last_feedback_at"].is_null());

    for signal in ["useful", "wrong"] {
        let feedback = client
            .post(format!("{base}/v2/memory/{memory_id}/feedback"))
            .header("X-User-Id", &user_id)
            .json(&json!({ "signal": signal }))
            .send()
            .await
            .expect("record feedback");
        assert_eq!(feedback.status(), 201);
    }

    let summary = client
        .get(format!("{base}/v2/memory/{memory_id}/feedback"))
        .header("X-User-Id", &user_id)
        .send()
        .await
        .expect("get feedback summary");
    assert_eq!(summary.status(), 200);
    let summary_body: Value = summary.json().await.expect("summary json");
    assert_eq!(summary_body["memory_id"], memory_id);
    assert_eq!(summary_body["feedback"]["useful"], 1);
    assert_eq!(summary_body["feedback"]["irrelevant"], 0);
    assert_eq!(summary_body["feedback"]["outdated"], 0);
    assert_eq!(summary_body["feedback"]["wrong"], 1);
    assert!(summary_body["last_feedback_at"].is_string());

    let missing = client
        .get(format!("{base}/v2/memory/missing-memory/feedback"))
        .header("X-User-Id", &user_id)
        .send()
        .await
        .expect("missing summary");
    assert_eq!(missing.status(), 404);
}

#[tokio::test]
async fn test_api_v2_memory_feedback_history() {
    let (base, client) = spawn_server().await;
    let user_id = uid();

    let remembered = client
        .post(format!("{base}/v2/memory/remember"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "content": "Memory feedback history target",
            "session_id": "sess-feedback-history",
            "tags": ["feedback"]
        }))
        .send()
        .await
        .expect("remember");
    assert_eq!(remembered.status(), 201);
    let remembered_body: Value = remembered.json().await.expect("remember json");
    let memory_id = remembered_body["memory_id"]
        .as_str()
        .expect("memory id")
        .to_string();

    let first = client
        .post(format!("{base}/v2/memory/{memory_id}/feedback"))
        .header("X-User-Id", &user_id)
        .json(&json!({ "signal": "useful", "context": "first" }))
        .send()
        .await
        .expect("first feedback");
    assert_eq!(first.status(), 201);
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;

    let second = client
        .post(format!("{base}/v2/memory/{memory_id}/feedback"))
        .header("X-User-Id", &user_id)
        .json(&json!({ "signal": "wrong", "context": "second" }))
        .send()
        .await
        .expect("second feedback");
    assert_eq!(second.status(), 201);
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;

    let third = client
        .post(format!("{base}/v2/memory/{memory_id}/feedback"))
        .header("X-User-Id", &user_id)
        .json(&json!({ "signal": "outdated" }))
        .send()
        .await
        .expect("third feedback");
    assert_eq!(third.status(), 201);

    let history = client
        .get(format!("{base}/v2/memory/{memory_id}/feedback/history"))
        .header("X-User-Id", &user_id)
        .query(&[("limit", "2")])
        .send()
        .await
        .expect("feedback history");
    assert_eq!(history.status(), 200);
    let history_body: Value = history.json().await.expect("history json");
    let items = history_body["items"].as_array().expect("history items");
    assert_eq!(history_body["memory_id"], memory_id);
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["signal"], "outdated");
    assert!(items[0]["context"].is_null());
    assert_eq!(items[1]["signal"], "wrong");
    assert_eq!(items[1]["context"], "second");
    assert!(items[0]["feedback_id"].as_str().unwrap_or("").len() > 8);
    assert!(items[0]["created_at"].is_string());

    let full_history = client
        .get(format!("{base}/v2/memory/{memory_id}/feedback/history"))
        .header("X-User-Id", &user_id)
        .query(&[("limit", "10")])
        .send()
        .await
        .expect("full feedback history");
    assert_eq!(full_history.status(), 200);
    let full_history_body: Value = full_history.json().await.expect("full history json");
    let full_items = full_history_body["items"]
        .as_array()
        .expect("full history items");
    assert_eq!(full_items.len(), 3);
    assert!(full_items
        .iter()
        .any(|item| item["signal"] == "useful" && item["context"] == "first"));

    let missing = client
        .get(format!("{base}/v2/memory/missing-memory/feedback/history"))
        .header("X-User-Id", &user_id)
        .send()
        .await
        .expect("missing history");
    assert_eq!(missing.status(), 404);
}

#[tokio::test]
async fn test_api_v2_feedback_history_feed() {
    let (base, client) = spawn_server().await;
    let user_id = uid();

    let first = client
        .post(format!("{base}/v2/memory/remember"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "content": "Alpha feedback feed target",
            "session_id": "sess-feedback-feed",
            "tags": ["feedback"]
        }))
        .send()
        .await
        .expect("remember first");
    assert_eq!(first.status(), 201);
    let first_body: Value = first.json().await.expect("first remember json");
    let first_memory_id = first_body["memory_id"]
        .as_str()
        .expect("first memory id")
        .to_string();
    let first_abstract = first_body["abstract"]
        .as_str()
        .expect("first abstract")
        .to_string();

    let second = client
        .post(format!("{base}/v2/memory/remember"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "content": "Beta feedback feed target",
            "session_id": "sess-feedback-feed",
            "tags": ["feedback"]
        }))
        .send()
        .await
        .expect("remember second");
    assert_eq!(second.status(), 201);
    let second_body: Value = second.json().await.expect("second remember json");
    let second_memory_id = second_body["memory_id"]
        .as_str()
        .expect("second memory id")
        .to_string();
    let second_abstract = second_body["abstract"]
        .as_str()
        .expect("second abstract")
        .to_string();

    let first_feedback = client
        .post(format!("{base}/v2/memory/{first_memory_id}/feedback"))
        .header("X-User-Id", &user_id)
        .json(&json!({ "signal": "useful", "context": "alpha useful" }))
        .send()
        .await
        .expect("first feedback");
    assert_eq!(first_feedback.status(), 201);
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;

    let second_feedback = client
        .post(format!("{base}/v2/memory/{second_memory_id}/feedback"))
        .header("X-User-Id", &user_id)
        .json(&json!({ "signal": "wrong", "context": "beta wrong" }))
        .send()
        .await
        .expect("second feedback");
    assert_eq!(second_feedback.status(), 201);
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;

    let third_feedback = client
        .post(format!("{base}/v2/memory/{first_memory_id}/feedback"))
        .header("X-User-Id", &user_id)
        .json(&json!({ "signal": "outdated" }))
        .send()
        .await
        .expect("third feedback");
    assert_eq!(third_feedback.status(), 201);

    let history = client
        .get(format!("{base}/v2/feedback/history"))
        .header("X-User-Id", &user_id)
        .query(&[("limit", "2")])
        .send()
        .await
        .expect("feedback history feed");
    assert_eq!(history.status(), 200);
    let history_body: Value = history.json().await.expect("feedback history feed json");
    let items = history_body["items"].as_array().expect("feed items");
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["signal"], "outdated");
    assert_eq!(items[0]["memory_id"], first_memory_id);
    assert_eq!(items[0]["abstract"], first_abstract);
    assert!(items[0]["context"].is_null());
    assert_eq!(items[1]["signal"], "wrong");
    assert_eq!(items[1]["memory_id"], second_memory_id);
    assert_eq!(items[1]["abstract"], second_abstract);
    assert_eq!(items[1]["context"], "beta wrong");

    let filtered = client
        .get(format!("{base}/v2/feedback/history"))
        .header("X-User-Id", &user_id)
        .query(&[
            ("memory_id", first_memory_id.as_str()),
            ("signal", "useful"),
            ("limit", "10"),
        ])
        .send()
        .await
        .expect("filtered feedback history feed");
    assert_eq!(filtered.status(), 200);
    let filtered_body: Value = filtered.json().await.expect("filtered feed json");
    let filtered_items = filtered_body["items"]
        .as_array()
        .expect("filtered feed items");
    assert_eq!(filtered_items.len(), 1);
    assert_eq!(filtered_items[0]["memory_id"], first_memory_id);
    assert_eq!(filtered_items[0]["signal"], "useful");
    assert_eq!(filtered_items[0]["context"], "alpha useful");

    let invalid = client
        .get(format!("{base}/v2/feedback/history"))
        .header("X-User-Id", &user_id)
        .query(&[("signal", "bad_signal")])
        .send()
        .await
        .expect("invalid feed filter");
    assert_eq!(invalid.status(), 422);
}

#[tokio::test]
async fn test_api_v2_recall_requires_session_id_for_session_scope() {
    let (base, client) = spawn_server().await;
    let user_id = uid();

    let response = client
        .post(format!("{base}/v2/memory/recall"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "query": "platform",
            "scope": "session"
        }))
        .send()
        .await
        .expect("recall");
    assert_eq!(response.status(), 422);
}

#[tokio::test]
async fn test_api_v2_recall_exposes_has_related_true_for_linked_memory() {
    let (base, client) = spawn_server().await;
    let user_id = uid();

    let first = client
        .post(format!("{base}/v2/memory/remember"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "content": "Rust deployment automation guide for platform teams",
            "type": "semantic",
            "session_id": "sess-related",
            "tags": ["rust", "shared"]
        }))
        .send()
        .await
        .expect("remember first");
    assert_eq!(first.status(), 201);
    let first_body: Value = first.json().await.expect("remember first json");
    let first_id = first_body["memory_id"]
        .as_str()
        .expect("first memory id")
        .to_string();

    let second = client
        .post(format!("{base}/v2/memory/remember"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "content": "Rust deployment checklist for shared automation work",
            "type": "semantic",
            "session_id": "sess-related",
            "tags": ["rust", "shared"]
        }))
        .send()
        .await
        .expect("remember second");
    assert_eq!(second.status(), 201);

    let _ = wait_for_link_count(&base, &client, &user_id, &first_id, "both", 1).await;

    let recall = client
        .post(format!("{base}/v2/memory/recall"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "query": "rust deployment automation",
            "top_k": 5,
            "type": "semantic"
        }))
        .send()
        .await
        .expect("recall linked memory");
    assert_eq!(recall.status(), 200);
    let body: Value = recall.json().await.expect("recall linked memory json");
    let memories = body["memories"].as_array().expect("recall memories");
    assert!(memories.iter().any(|memory| memory["id"] == first_id));
    let linked = memories
        .iter()
        .find(|memory| memory["related"] == true)
        .expect("memory with compact related hint");
    assert_eq!(linked["related"], true);
    assert_eq!(linked["type"], "semantic");
    assert!(!linked["text"].as_str().unwrap_or("").is_empty());
}

#[tokio::test]
async fn test_api_v2_recall_exposes_has_related_false_without_links() {
    let (base, client) = spawn_server().await;
    let user_id = uid();

    let remembered = client
        .post(format!("{base}/v2/memory/remember"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "content": "Solo memory about a private deployment checklist",
            "type": "semantic",
            "session_id": "sess-solo",
            "tags": ["solo"]
        }))
        .send()
        .await
        .expect("remember solo");
    assert_eq!(remembered.status(), 201);
    let remembered_body: Value = remembered.json().await.expect("remember solo json");
    let memory_id = remembered_body["memory_id"]
        .as_str()
        .expect("solo memory id")
        .to_string();

    let recall = client
        .post(format!("{base}/v2/memory/recall"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "query": "private deployment checklist",
            "top_k": 5,
            "type": "semantic"
        }))
        .send()
        .await
        .expect("recall solo memory");
    assert_eq!(recall.status(), 200);
    let body: Value = recall.json().await.expect("recall solo memory json");
    let memories = body["memories"].as_array().expect("recall memories");
    let solo = memories
        .iter()
        .find(|memory| memory["id"] == memory_id)
        .expect("solo memory");
    assert_eq!(solo["related"], false);
    assert!(solo["text"]
        .as_str()
        .unwrap_or("")
        .contains("private deployment checklist"));
}

#[tokio::test]
async fn test_api_v2_recall_surfaces_one_hop_links_by_default() {
    let (base, client) = spawn_server().await;
    let user_id = uid();

    let seed = client
        .post(format!("{base}/v2/memory/remember"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "content": "OAuth token gateway hardening guide for staged rollouts",
            "type": "semantic",
            "session_id": "sess-hop",
            "tags": ["oauth", "shared-bridge"]
        }))
        .send()
        .await
        .expect("remember seed");
    assert_eq!(seed.status(), 201);
    let seed_body: Value = seed.json().await.expect("remember seed json");
    let seed_id = seed_body["memory_id"]
        .as_str()
        .expect("seed memory id")
        .to_string();

    let linked = client
        .post(format!("{base}/v2/memory/remember"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "content": "Incident runbook for midnight refresh failures",
            "type": "semantic",
            "session_id": "sess-hop",
            "tags": ["incident", "shared-bridge"]
        }))
        .send()
        .await
        .expect("remember linked");
    assert_eq!(linked.status(), 201);
    let linked_body: Value = linked.json().await.expect("remember linked json");
    let linked_id = linked_body["memory_id"]
        .as_str()
        .expect("linked memory id")
        .to_string();
    let _ = wait_for_jobs_done(&base, &client, &user_id, &seed_id).await;
    let _ = wait_for_jobs_done(&base, &client, &user_id, &linked_id).await;

    let default_recall = client
        .post(format!("{base}/v2/memory/recall"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "query": "staged rollouts hardening guide",
            "top_k": 5,
            "type": "semantic"
        }))
        .send()
        .await
        .expect("default recall");
    assert_eq!(default_recall.status(), 200);
    let default_body: Value = default_recall.json().await.expect("default recall json");
    let default_memories = default_body["memories"]
        .as_array()
        .expect("default memories");
    assert!(default_memories
        .iter()
        .any(|memory| memory["id"] == linked_id));
}

#[tokio::test]
async fn test_api_v2_recall_exposes_ranking_breakdown() {
    let (base, client) = spawn_server().await;
    let user_id = uid();

    let seed = client
        .post(format!("{base}/v2/memory/remember"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "content": "OAuth token gateway hardening guide for staged rollouts",
            "type": "semantic",
            "session_id": "sess-ranking",
            "tags": ["oauth", "shared-bridge"]
        }))
        .send()
        .await
        .expect("remember seed");
    assert_eq!(seed.status(), 201);
    let seed_body: Value = seed.json().await.expect("remember seed json");
    let seed_id = seed_body["memory_id"]
        .as_str()
        .expect("seed memory id")
        .to_string();

    let linked = client
        .post(format!("{base}/v2/memory/remember"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "content": "zzqv narwhal lattice handbook",
            "type": "semantic",
            "session_id": "sess-ranking",
            "tags": ["incident", "shared-bridge"]
        }))
        .send()
        .await
        .expect("remember linked");
    assert_eq!(linked.status(), 201);
    let linked_body: Value = linked.json().await.expect("remember linked json");
    let linked_id = linked_body["memory_id"]
        .as_str()
        .expect("linked memory id")
        .to_string();

    let _ = wait_for_jobs_done(&base, &client, &user_id, &seed_id).await;
    let _ = wait_for_jobs_done(&base, &client, &user_id, &linked_id).await;

    let focus = client
        .post(format!("{base}/v2/memory/focus"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "type": "memory_id",
            "value": seed_id,
            "boost": 4.0,
            "ttl_secs": 300
        }))
        .send()
        .await
        .expect("focus seed");
    assert_eq!(focus.status(), 201);

    let recall = client
        .post(format!("{base}/v2/memory/recall"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "query": "OAuth token gateway hardening",
            "top_k": 5,
            "session_id": "sess-ranking",
            "view": "full",
            "type": "semantic"
        }))
        .send()
        .await
        .expect("recall ranking");
    assert_eq!(recall.status(), 200);
    let body: Value = recall.json().await.expect("recall ranking json");
    let memories = body["memories"]
        .as_array()
        .expect("recall ranking memories");

    let seed_memory = memories
        .iter()
        .find(|memory| memory["id"] == seed_id)
        .expect("seed memory in recall");
    assert_eq!(seed_memory["ranking"]["session_affinity_applied"], true);
    assert_eq!(seed_memory["ranking"]["session_affinity_multiplier"], 1.12);
    assert_eq!(seed_memory["ranking"]["focus_boost"], 4.0);
    assert!(
        seed_memory["ranking"]["vector_component"]
            .as_f64()
            .unwrap_or_default()
            > 0.0
    );
    assert!(
        seed_memory["ranking"]["confidence_component"]
            .as_f64()
            .unwrap_or_default()
            > 0.0
    );
    assert!(seed_memory["ranking"]["focus_matches"]
        .as_array()
        .map(|items| items.iter().any(|item| {
            item["type"] == "memory_id" && item["value"] == seed_id && item["boost"] == 4.0
        }))
        .unwrap_or(false));

    let expanded_memory = memories
        .iter()
        .find(|memory| {
            memory["ranking"]["linked_expansion_applied"] == true
                && memory["ranking"]["link_bonus"].as_f64().unwrap_or_default() > 0.0
        })
        .expect("memory with expansion explainability");
    assert!(expanded_memory["ranking"]["expansion_sources"]
        .as_array()
        .map(|items| {
            !items.is_empty()
                && items
                    .iter()
                    .any(|item| item["bonus"].as_f64().unwrap_or_default() > 0.0)
        })
        .unwrap_or(false));
}

#[tokio::test]
async fn test_api_v2_recall_exposes_retrieval_path() {
    let (base, client) = spawn_server().await;
    let user_id = uid();

    let seed = client
        .post(format!("{base}/v2/memory/remember"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "content": "OAuth token gateway hardening guide for staged rollouts",
            "type": "semantic",
            "session_id": "sess-path",
            "tags": ["shared-bridge"]
        }))
        .send()
        .await
        .expect("remember seed");
    assert_eq!(seed.status(), 201);
    let seed_body: Value = seed.json().await.expect("remember seed json");
    let seed_id = seed_body["memory_id"]
        .as_str()
        .expect("seed id")
        .to_string();

    let expanded_only = client
        .post(format!("{base}/v2/memory/remember"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "content": "zzqv narwhal lattice handbook",
            "type": "semantic",
            "session_id": "sess-path",
            "tags": ["shared-bridge"]
        }))
        .send()
        .await
        .expect("remember expanded-only");
    assert_eq!(expanded_only.status(), 201);
    let expanded_only_body: Value = expanded_only.json().await.expect("expanded-only json");
    let expanded_only_id = expanded_only_body["memory_id"]
        .as_str()
        .expect("expanded-only id")
        .to_string();

    let direct_only = client
        .post(format!("{base}/v2/memory/remember"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "content": "OAuth platform hardening checklist for release owners",
            "type": "semantic",
            "session_id": "sess-path",
            "tags": ["isolated"]
        }))
        .send()
        .await
        .expect("remember direct-only");
    assert_eq!(direct_only.status(), 201);
    let direct_only_body: Value = direct_only.json().await.expect("direct-only json");
    let direct_only_id = direct_only_body["memory_id"]
        .as_str()
        .expect("direct-only id")
        .to_string();

    let hybrid = client
        .post(format!("{base}/v2/memory/remember"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "content": "OAuth hardening bridge playbook for rollout incidents",
            "type": "semantic",
            "session_id": "sess-path",
            "tags": ["shared-bridge"]
        }))
        .send()
        .await
        .expect("remember hybrid");
    assert_eq!(hybrid.status(), 201);
    let hybrid_body: Value = hybrid.json().await.expect("hybrid json");
    let hybrid_id = hybrid_body["memory_id"]
        .as_str()
        .expect("hybrid id")
        .to_string();
    let isolated_direct = client
        .post(format!("{base}/v2/memory/remember"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "content": "Release owner checklist with unique zebra marker",
            "type": "semantic",
            "session_id": "sess-path",
            "tags": ["zebra-unique"]
        }))
        .send()
        .await
        .expect("remember isolated direct");
    assert_eq!(isolated_direct.status(), 201);
    let isolated_direct_body: Value = isolated_direct.json().await.expect("isolated direct json");
    let isolated_direct_id = isolated_direct_body["memory_id"]
        .as_str()
        .expect("isolated direct id")
        .to_string();

    let _ = wait_for_jobs_done(&base, &client, &user_id, &seed_id).await;
    let _ = wait_for_jobs_done(&base, &client, &user_id, &expanded_only_id).await;
    let _ = wait_for_jobs_done(&base, &client, &user_id, &direct_only_id).await;
    let _ = wait_for_jobs_done(&base, &client, &user_id, &hybrid_id).await;
    let _ = wait_for_jobs_done(&base, &client, &user_id, &isolated_direct_id).await;

    let recall = client
        .post(format!("{base}/v2/memory/recall"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "query": "OAuth hardening rollout",
            "top_k": 10,
            "session_id": "sess-path",
            "view": "full",
            "type": "semantic"
        }))
        .send()
        .await
        .expect("recall retrieval paths");
    assert_eq!(recall.status(), 200);
    let body: Value = recall.json().await.expect("recall retrieval paths json");
    let memories = body["memories"]
        .as_array()
        .expect("retrieval path memories");

    let hybrid_memory = memories
        .iter()
        .find(|memory| memory["id"] == hybrid_id)
        .expect("hybrid memory");
    assert_eq!(hybrid_memory["retrieval_path"], "direct_and_expanded");

    let isolated_recall = client
        .post(format!("{base}/v2/memory/recall"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "query": "unique zebra marker",
            "top_k": 5,
            "session_id": "sess-path",
            "view": "full",
            "type": "semantic",
            "expand_links": false
        }))
        .send()
        .await
        .expect("isolated direct recall");
    assert_eq!(isolated_recall.status(), 200);
    let isolated_body: Value = isolated_recall
        .json()
        .await
        .expect("isolated direct recall json");
    let isolated_memories = isolated_body["memories"]
        .as_array()
        .expect("isolated direct memories");
    let isolated_direct_memory = isolated_memories
        .iter()
        .find(|memory| memory["id"] == isolated_direct_id)
        .expect("isolated direct memory");
    assert_eq!(isolated_direct_memory["retrieval_path"], "direct");
}

#[tokio::test]
async fn test_api_v2_recall_exposes_summary_buckets() {
    let (base, client) = spawn_server().await;
    let user_id = uid();

    let seed = client
        .post(format!("{base}/v2/memory/remember"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "content": "OAuth token gateway hardening guide for staged rollouts",
            "type": "semantic",
            "session_id": "sess-summary",
            "tags": ["shared-bridge"]
        }))
        .send()
        .await
        .expect("remember seed");
    assert_eq!(seed.status(), 201);
    let seed_body: Value = seed.json().await.expect("seed json");
    let seed_id = seed_body["memory_id"]
        .as_str()
        .expect("seed id")
        .to_string();

    let expanded_only = client
        .post(format!("{base}/v2/memory/remember"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "content": "zzqv narwhal lattice handbook",
            "type": "semantic",
            "session_id": "sess-summary",
            "tags": ["shared-bridge"]
        }))
        .send()
        .await
        .expect("remember expanded-only");
    assert_eq!(expanded_only.status(), 201);
    let expanded_body: Value = expanded_only.json().await.expect("expanded-only json");
    let expanded_only_id = expanded_body["memory_id"]
        .as_str()
        .expect("expanded-only id")
        .to_string();

    let _ = wait_for_jobs_done(&base, &client, &user_id, &seed_id).await;
    let _ = wait_for_jobs_done(&base, &client, &user_id, &expanded_only_id).await;

    let recall = client
        .post(format!("{base}/v2/memory/recall"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "query": "OAuth token gateway hardening",
            "top_k": 1,
            "session_id": "sess-summary",
            "type": "semantic"
        }))
        .send()
        .await
        .expect("recall summary");
    assert_eq!(recall.status(), 200);
    let body: Value = recall.json().await.expect("recall summary json");

    assert_eq!(body["has_more"], true);
    assert_eq!(body["summary"]["discovered_count"], 2);
    assert_eq!(body["summary"]["returned_count"], 1);
    assert_eq!(body["summary"]["truncated"], true);

    let buckets = body["summary"]["by_retrieval_path"]
        .as_array()
        .expect("summary buckets");
    let expanded_bucket = buckets
        .iter()
        .find(|bucket| bucket["retrieval_path"] == "expanded_only")
        .expect("expanded-only bucket");
    let hybrid_bucket = buckets
        .iter()
        .find(|bucket| bucket["retrieval_path"] == "direct_and_expanded")
        .expect("hybrid bucket");
    assert_eq!(
        expanded_bucket["discovered_count"]
            .as_i64()
            .unwrap_or_default()
            + hybrid_bucket["discovered_count"]
                .as_i64()
                .unwrap_or_default(),
        1
    );

    let isolated_direct = client
        .post(format!("{base}/v2/memory/remember"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "content": "Release owner checklist with zebra-only anchor",
            "type": "semantic",
            "session_id": "sess-summary",
            "tags": ["zebra-unique"]
        }))
        .send()
        .await
        .expect("remember isolated direct");
    assert_eq!(isolated_direct.status(), 201);
    let isolated_body: Value = isolated_direct.json().await.expect("isolated direct json");
    let isolated_direct_id = isolated_body["memory_id"]
        .as_str()
        .expect("isolated direct id")
        .to_string();
    let _ = wait_for_jobs_done(&base, &client, &user_id, &isolated_direct_id).await;

    let direct_only = client
        .post(format!("{base}/v2/memory/recall"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "query": "zebra-only anchor",
            "top_k": 1,
            "session_id": "sess-summary",
            "type": "semantic",
            "tags": ["zebra-unique"],
            "expand_links": false
        }))
        .send()
        .await
        .expect("direct-only recall summary");
    assert_eq!(direct_only.status(), 200);
    let direct_body: Value = direct_only.json().await.expect("direct-only summary json");
    assert_eq!(direct_body["summary"]["discovered_count"], 1);
    assert_eq!(direct_body["summary"]["returned_count"], 1);
    assert_eq!(direct_body["summary"]["truncated"], false);

    let direct_buckets = direct_body["summary"]["by_retrieval_path"]
        .as_array()
        .expect("direct-only summary buckets");
    let direct_bucket = direct_buckets
        .iter()
        .find(|bucket| bucket["retrieval_path"] == "direct")
        .expect("direct bucket");
    assert_eq!(direct_bucket["discovered_count"], 1);
    assert_eq!(direct_bucket["returned_count"], 1);
}

#[tokio::test]
async fn test_api_v2_recall_exposes_temporal_decay() {
    let db = isolated_db_url();
    let store = Arc::new(
        SqlMemoryStore::connect(&db, test_dim(), uuid::Uuid::new_v4().to_string())
            .await
            .expect("connect"),
    );
    migrate_store_with_retry(store.as_ref()).await;
    let (base, client) = spawn_server_for_store_with_llm(&db, store.clone(), None).await;
    let user_id = uid();

    let fresh_working = client
        .post(format!("{base}/v2/memory/remember"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "content": "Auth outage triage anchor",
            "type": "working"
        }))
        .send()
        .await
        .expect("remember fresh working");
    assert_eq!(fresh_working.status(), 201);
    let fresh_working_body: Value = fresh_working.json().await.expect("fresh working json");
    let fresh_working_id = fresh_working_body["memory_id"]
        .as_str()
        .expect("fresh working id")
        .to_string();

    let stale_working = client
        .post(format!("{base}/v2/memory/remember"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "content": "Auth outage triage anchor",
            "type": "working"
        }))
        .send()
        .await
        .expect("remember stale working");
    assert_eq!(stale_working.status(), 201);
    let stale_working_body: Value = stale_working.json().await.expect("stale working json");
    let stale_working_id = stale_working_body["memory_id"]
        .as_str()
        .expect("stale working id")
        .to_string();

    let stale_semantic = client
        .post(format!("{base}/v2/memory/remember"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "content": "Auth outage triage anchor",
            "type": "semantic"
        }))
        .send()
        .await
        .expect("remember stale semantic");
    assert_eq!(stale_semantic.status(), 201);
    let stale_semantic_body: Value = stale_semantic.json().await.expect("stale semantic json");
    let stale_semantic_id = stale_semantic_body["memory_id"]
        .as_str()
        .expect("stale semantic id")
        .to_string();

    let family = store
        .v2_store()
        .ensure_user_tables(&user_id)
        .await
        .expect("ensure tables");
    let stale_created_at = (Utc::now() - Duration::days(10)).naive_utc();
    sqlx::query(&format!(
        "UPDATE {} SET created_at = ? WHERE memory_id IN (?, ?)",
        family.heads_table
    ))
    .bind(stale_created_at)
    .bind(&stale_working_id)
    .bind(&stale_semantic_id)
    .execute(store.pool())
    .await
    .expect("age stale memories");

    let recall = client
        .post(format!("{base}/v2/memory/recall"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "query": "Auth outage triage anchor",
            "top_k": 5,
            "view": "full",
            "type": "all",
            "expand_links": false
        }))
        .send()
        .await
        .expect("recall temporal decay");
    assert_eq!(recall.status(), 200);
    let body: Value = recall.json().await.expect("recall temporal decay json");
    let memories = body["memories"]
        .as_array()
        .expect("temporal decay memories");

    assert_eq!(memories[0]["id"], fresh_working_id);
    assert_eq!(memories[1]["id"], stale_semantic_id);
    assert_eq!(memories[2]["id"], stale_working_id);

    let fresh_memory = memories
        .iter()
        .find(|memory| memory["id"] == fresh_working_id)
        .expect("fresh memory");
    let stale_semantic_memory = memories
        .iter()
        .find(|memory| memory["id"] == stale_semantic_id)
        .expect("stale semantic memory");
    let stale_working_memory = memories
        .iter()
        .find(|memory| memory["id"] == stale_working_id)
        .expect("stale working memory");

    assert_eq!(fresh_memory["ranking"]["temporal_decay_applied"], false);
    assert!(
        fresh_memory["ranking"]["temporal_multiplier"]
            .as_f64()
            .unwrap_or_default()
            > 0.999
    );
    assert_eq!(
        stale_semantic_memory["ranking"]["temporal_decay_applied"],
        true
    );
    assert_eq!(
        stale_semantic_memory["ranking"]["temporal_half_life_hours"],
        2160.0
    );
    assert_eq!(
        stale_working_memory["ranking"]["temporal_decay_applied"],
        true
    );
    assert_eq!(
        stale_working_memory["ranking"]["temporal_half_life_hours"],
        48.0
    );
    assert!(
        stale_working_memory["ranking"]["age_hours"]
            .as_f64()
            .unwrap_or_default()
            > 200.0
    );
    assert!(
        stale_working_memory["ranking"]["temporal_multiplier"]
            .as_f64()
            .unwrap_or_default()
            < stale_semantic_memory["ranking"]["temporal_multiplier"]
                .as_f64()
                .unwrap_or_default()
    );
}

#[tokio::test]
async fn test_api_v2_memory_flow_with_fake_llm_enrichment() {
    let _guard = heavy_flow_lock().lock().await;
    let (llm, _shutdown) = memoria_test_utils::spawn_fake_llm(vec![(
        "Memory V2 derive views",
        json!({
            "overview": "LLM overview from fake server",
            "detail": "LLM detail from fake server"
        }),
    )])
    .await;
    let (base, client) = spawn_server_with_llm(Some(llm)).await;
    let user_id = uid();

    let first = client
        .post(format!("{base}/v2/memory/remember"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "content": "Primary V2 memory for fake llm enrichment processing via the derive_views background job pipeline. This content is long enough to trigger the enricher path so that the fake LLM server receives the derive_views request. Platform engineers use shared tags to connect related memories.",
            "session_id": "sess-v2-llm",
            "tags": ["shared"]
        }))
        .send()
        .await
        .expect("first remember");
    assert_eq!(first.status(), 201);
    let first_body: Value = first.json().await.expect("first json");
    let first_id = first_body["memory_id"]
        .as_str()
        .expect("first id")
        .to_string();

    let second = client
        .post(format!("{base}/v2/memory/remember"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "content": "Secondary V2 memory for fake llm enrichment processing alongside the primary memory in this test. This content is also long enough to trigger derive_views so the fake LLM enricher is invoked. Shared tags ensure tag_overlap links are created between the primary and secondary memories.",
            "session_id": "sess-v2-llm",
            "tags": ["shared"]
        }))
        .send()
        .await
        .expect("second remember");
    assert_eq!(second.status(), 201);
    let second_body: Value = second.json().await.expect("second json");
    let second_id = second_body["memory_id"]
        .as_str()
        .expect("second id")
        .to_string();

    let enriched = wait_for_views(&base, &client, &user_id, &first_id).await;
    assert_eq!(enriched["overview"], "LLM overview from fake server");
    assert_eq!(enriched["detail"], "LLM detail from fake server");

    let _ = wait_for_jobs_done(&base, &client, &user_id, &first_id).await;
    let _ = wait_for_views(&base, &client, &user_id, &second_id).await;
    let _ = wait_for_jobs_done(&base, &client, &user_id, &second_id).await;
    let links = wait_for_link_count(&base, &client, &user_id, &first_id, "both", 1).await;
    let links = links["items"].as_array().expect("links");
    assert!(!links.is_empty());
    assert!(links.iter().any(|link| {
        link["provenance"]["evidence"]
            .as_array()
            .map(|items| {
                items.iter().any(|detail| {
                    detail["type"] == "tag_overlap"
                        && detail["overlap_count"] == 1
                        && detail["source_tag_count"] == 1
                        && detail["target_tag_count"] == 1
                })
            })
            .unwrap_or(false)
    }));
}

#[tokio::test]
async fn test_api_v2_fake_llm_link_refinement_preserves_fallbacks() {
    let db = isolated_db_url();
    let store = Arc::new(
        SqlMemoryStore::connect(&db, test_dim(), uuid::Uuid::new_v4().to_string())
            .await
            .expect("connect"),
    );
    migrate_store_with_retry(store.as_ref()).await;

    let user_id = uid();
    let v2 = store.v2_store();
    let first = v2
        .remember(
            &user_id,
            MemoryV2RememberInput {
                content: "Primary V2 memory for refined link preservation".to_string(),
                memory_type: MemoryType::Semantic,
                session_id: Some("sess-v2-llm-links".to_string()),
                importance: Some(0.5),
                trust_tier: Some(TrustTier::T2Curated),
                tags: vec!["shared".to_string(), "alpha".to_string()],
                source: None,
                embedding: None,
                actor: user_id.clone(),
            },
        )
        .await
        .expect("remember first");
    let second = v2
        .remember(
            &user_id,
            MemoryV2RememberInput {
                content: "Secondary V2 memory chosen by fake llm refinement".to_string(),
                memory_type: MemoryType::Semantic,
                session_id: Some("sess-v2-llm-links".to_string()),
                importance: Some(0.4),
                trust_tier: Some(TrustTier::T2Curated),
                tags: vec!["shared".to_string(), "alpha".to_string()],
                source: None,
                embedding: None,
                actor: user_id.clone(),
            },
        )
        .await
        .expect("remember second");
    let third = v2
        .remember(
            &user_id,
            MemoryV2RememberInput {
                content: "Third V2 memory kept from fallback links".to_string(),
                memory_type: MemoryType::Semantic,
                session_id: Some("sess-v2-llm-links".to_string()),
                importance: Some(0.3),
                trust_tier: Some(TrustTier::T2Curated),
                tags: vec!["shared".to_string()],
                source: None,
                embedding: None,
                actor: user_id.clone(),
            },
        )
        .await
        .expect("remember third");

    let (llm, _shutdown) = memoria_test_utils::spawn_fake_llm(vec![(
        "Memory V2 refine links",
        json!([{
            "memory_id": second.memory_id,
            "link_type": "supports",
            "strength": 0.93
        }]),
    )])
    .await;
    let (base, client) = spawn_server_for_store_with_llm(&db, store, Some(llm)).await;
    let _ = wait_for_jobs_done(&base, &client, &user_id, &first.memory_id).await;
    let _ = wait_for_jobs_done(&base, &client, &user_id, &second.memory_id).await;
    let _ = wait_for_jobs_done(&base, &client, &user_id, &third.memory_id).await;

    let links =
        wait_for_link_count(&base, &client, &user_id, &first.memory_id, "outbound", 2).await;
    let link_items = links["items"].as_array().expect("link items");
    assert_eq!(link_items.len(), 2);
    assert!(link_items.iter().any(|item| {
        item["id"] == second.memory_id
            && item["link_type"] == "supports"
            && item["provenance"]["refined"] == true
            && item["provenance"]["primary_evidence_type"] == "tag_overlap"
            && item["provenance"]["evidence_types"]
                .as_array()
                .map(|items| items.iter().any(|value| value == "tag_overlap"))
                .unwrap_or(false)
    }));
    assert!(link_items.iter().any(|item| {
        item["id"] == second.memory_id
            && item["provenance"]["extraction_trace"]["derivation_state"] == "complete"
            && item["provenance"]["extraction_trace"]["latest_job_status"] == "done"
            && item["provenance"]["extraction_trace"]["latest_job_attempts"]
                .as_i64()
                .unwrap_or_default()
                >= 1
    }));
    assert!(link_items.iter().any(|item| {
        item["id"] == second.memory_id
            && item["provenance"]["evidence"]
                .as_array()
                .map(|items| {
                    items.iter().any(|detail| {
                        detail["type"] == "tag_overlap"
                            && detail["overlap_count"] == 2
                            && detail["source_tag_count"] == 2
                            && detail["target_tag_count"] == 2
                    })
                })
                .unwrap_or(false)
    }));
    assert!(link_items.iter().any(|item| {
        item["id"] == third.memory_id
            && item["link_type"] != "supports"
            && item["provenance"]["refined"] == false
    }));

    let expanded = client
        .post(format!("{base}/v2/memory/expand"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "memory_id": first.memory_id,
            "level": "links"
        }))
        .send()
        .await
        .expect("expand refined links");
    assert_eq!(expanded.status(), 200);
    let expanded_body: Value = expanded.json().await.expect("expand refined links json");
    let expanded_links = expanded_body["links"]
        .as_array()
        .expect("expanded refined links");
    assert!(expanded_links.iter().any(|link| {
        link["memory_id"] == second.memory_id
            && link["link_type"] == "supports"
            && link["provenance"]["refined"] == true
            && link["provenance"]["primary_evidence_type"] == "tag_overlap"
    }));
}

#[tokio::test]
async fn test_api_v2_update_and_tags() {
    let (base, client) = spawn_server().await;
    let user_id = uid();

    let first = client
        .post(format!("{base}/v2/memory/remember"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "content": "Legacy rust handbook for platform teams",
            "session_id": "sess-update",
            "tags": ["legacy"]
        }))
        .send()
        .await
        .expect("remember first");
    assert_eq!(first.status(), 201);
    let first_body: Value = first.json().await.expect("first json");
    let first_id = first_body["memory_id"]
        .as_str()
        .expect("first id")
        .to_string();

    let second = client
        .post(format!("{base}/v2/memory/remember"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "content": "Shared rust service guide for platform teams",
            "session_id": "sess-update",
            "tags": ["shared"]
        }))
        .send()
        .await
        .expect("remember second");
    assert_eq!(second.status(), 201);

    let update = client
        .patch(format!("{base}/v2/memory/update"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "memory_id": first_id,
            "content": "Updated rust handbook for shared platform teams covering complete Rust development lifecycle. This guide helps platform engineers understand ownership, borrowing, and lifetimes in shared service contexts. Teams building on shared platform infrastructure benefit from Rust safety guarantees.",
            "importance": 0.95,
            "trust_tier": "T1",
            "tags_add": ["shared", "verified"],
            "tags_remove": ["legacy"],
            "reason": "clarified"
        }))
        .send()
        .await
        .expect("update");
    assert_eq!(update.status(), 200);
    let updated: Value = update.json().await.expect("update json");
    assert!(updated["abstract"]
        .as_str()
        .unwrap_or("")
        .starts_with("Updated rust handbook for shared platform teams"));
    assert!(!updated["updated_at"].is_null());

    let tags = client
        .get(format!("{base}/v2/memory/tags"))
        .header("X-User-Id", &user_id)
        .query(&[("limit", "10")])
        .send()
        .await
        .expect("list tags");
    assert_eq!(tags.status(), 200);
    let tags_body: Value = tags.json().await.expect("tags json");
    let tag_items = tags_body["items"].as_array().expect("tag items");
    assert!(tag_items
        .iter()
        .any(|item| item["tag"] == "shared" && item["memory_count"] == 2));
    assert!(tag_items
        .iter()
        .any(|item| item["tag"] == "verified" && item["memory_count"] == 1));
    assert!(!tag_items.iter().any(|item| item["tag"] == "legacy"));

    let filtered_tags = client
        .get(format!("{base}/v2/memory/tags"))
        .header("X-User-Id", &user_id)
        .query(&[("limit", "10"), ("query", "ver")])
        .send()
        .await
        .expect("filtered tags");
    assert_eq!(filtered_tags.status(), 200);
    let filtered_body: Value = filtered_tags.json().await.expect("filtered tags json");
    let filtered_items = filtered_body["items"].as_array().expect("filtered items");
    assert_eq!(filtered_items.len(), 1);
    assert_eq!(filtered_items[0]["tag"], "verified");

    let enriched = wait_for_views(
        &base,
        &client,
        &user_id,
        first_body["memory_id"].as_str().unwrap(),
    )
    .await;
    assert!(enriched["abstract"]
        .as_str()
        .unwrap_or("")
        .starts_with("Updated rust handbook for shared platform teams"));
    assert!(enriched["detail"]
        .as_str()
        .unwrap_or("")
        .contains("shared platform teams"));
}

#[tokio::test]
async fn test_api_v2_memory_history_reads_v2_events() {
    let (base, client) = spawn_server().await;
    let user_id = uid();

    let remember = client
        .post(format!("{base}/v2/memory/remember"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "content": "Legacy rust handbook for history",
            "session_id": "sess-history",
            "tags": ["legacy"]
        }))
        .send()
        .await
        .expect("remember");
    assert_eq!(remember.status(), 201);
    let remembered: Value = remember.json().await.expect("remember json");
    let memory_id = remembered["memory_id"]
        .as_str()
        .expect("memory id")
        .to_string();

    let update = client
        .patch(format!("{base}/v2/memory/update"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "memory_id": memory_id,
            "content": "Updated rust handbook for history",
            "importance": 0.95,
            "trust_tier": "T1",
            "tags_add": ["shared"],
            "tags_remove": ["legacy"],
            "reason": "clarified"
        }))
        .send()
        .await
        .expect("update");
    assert_eq!(update.status(), 200);

    let forget = client
        .post(format!("{base}/v2/memory/forget"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "memory_id": memory_id,
            "reason": "cleanup"
        }))
        .send()
        .await
        .expect("forget");
    assert_eq!(forget.status(), 200);

    let history = client
        .get(format!("{base}/v2/memory/{memory_id}/history"))
        .header("X-User-Id", &user_id)
        .query(&[("limit", "10")])
        .send()
        .await
        .expect("history");
    assert_eq!(history.status(), 200);
    let body: Value = history.json().await.expect("history json");
    let items = body["items"].as_array().expect("history items");
    assert_eq!(body["memory_id"], memory_id);
    assert_eq!(items.len(), 3);
    assert_eq!(items[0]["event_type"], "forgotten");
    assert_eq!(items[0]["actor"], user_id);
    assert_eq!(items[0]["processing_state"], "committed");
    assert_eq!(items[0]["payload"]["reason"], "cleanup");
    assert!(items[0]["created_at"].is_string());
    assert_eq!(items[1]["event_type"], "updated");
    assert_eq!(items[1]["payload"]["reason"], "clarified");
    assert_eq!(items[1]["payload"]["content_updated"], true);
    assert_eq!(items[2]["event_type"], "remembered");
    assert_eq!(items[2]["payload"]["type"], "semantic");
    assert!(items[2]["payload"].get("memory_type").is_none());
    assert_eq!(items[2]["payload"]["session_id"], "sess-history");

    let limited = client
        .get(format!("{base}/v2/memory/{memory_id}/history"))
        .header("X-User-Id", &user_id)
        .query(&[("limit", "2")])
        .send()
        .await
        .expect("limited history");
    assert_eq!(limited.status(), 200);
    let limited_body: Value = limited.json().await.expect("limited history json");
    let limited_items = limited_body["items"].as_array().expect("limited items");
    assert_eq!(limited_items.len(), 2);
    assert_eq!(limited_items[0]["event_type"], "forgotten");
    assert_eq!(limited_items[1]["event_type"], "updated");
}

#[tokio::test]
async fn test_api_v2_job_observability() {
    let (base, client) = spawn_server().await;
    let user_id = uid();

    let remember = client
        .post(format!("{base}/v2/memory/remember"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "content": "Observable async enrichment job for V2 memory processing and view derivation pipeline. This content has been made sufficiently long to trigger the derive_views background job. The system enriches memories with overview and detail text derived from the full source content for retrieval purposes.",
            "session_id": "sess-jobs-view",
            "tags": ["jobs"]
        }))
        .send()
        .await
        .expect("remember");
    assert_eq!(remember.status(), 201);
    let remembered: Value = remember.json().await.expect("remember json");
    let memory_id = remembered["memory_id"]
        .as_str()
        .expect("memory_id")
        .to_string();

    let jobs = client
        .get(format!("{base}/v2/memory/jobs"))
        .header("X-User-Id", &user_id)
        .query(&[("memory_id", memory_id.as_str()), ("limit", "10")])
        .send()
        .await
        .expect("jobs");
    assert_eq!(jobs.status(), 200);
    let jobs_body: Value = jobs.json().await.expect("jobs json");
    let items = jobs_body["items"].as_array().expect("job items");
    let job_types = jobs_body["job_types"].as_array().expect("job type items");
    assert_eq!(items.len(), 3);
    assert!(jobs_body["derivation_state"].is_string());
    assert_eq!(job_types.len(), 3);
    assert!(job_types.iter().all(|item| {
        item["pending_count"].as_u64().unwrap_or_default()
            + item["in_progress_count"].as_u64().unwrap_or_default()
            + item["done_count"].as_u64().unwrap_or_default()
            + item["failed_count"].as_u64().unwrap_or_default()
            == 1
    }));
    assert!(job_types
        .iter()
        .all(|item| !item["latest_status"].is_null()));
    assert!(job_types
        .iter()
        .any(|item| item["type"] == "extract_entities"));
    assert!(job_types
        .iter()
        .all(|item| !item["latest_updated_at"].is_null()));
    assert!(
        jobs_body["pending_count"].as_u64().unwrap_or_default()
            + jobs_body["in_progress_count"].as_u64().unwrap_or_default()
            + jobs_body["done_count"].as_u64().unwrap_or_default()
            + jobs_body["failed_count"].as_u64().unwrap_or_default()
            >= 2
    );
    assert!(items.iter().all(|item| !item["type"].is_null()));
    assert!(items.iter().all(|item| !item["available_at"].is_null()));

    let final_jobs = wait_for_jobs_done(&base, &client, &user_id, &memory_id).await;
    assert_eq!(final_jobs["memory_id"], memory_id);
    assert_eq!(final_jobs["derivation_state"], "complete");
    assert_eq!(final_jobs["done_count"], 3);
    assert_eq!(final_jobs["pending_count"], 0);
    assert_eq!(final_jobs["failed_count"], 0);
    assert_eq!(final_jobs["link_count"], 0);
    let final_job_types = final_jobs["job_types"].as_array().expect("final job types");
    assert_eq!(final_job_types.len(), 3);
    assert!(final_job_types
        .iter()
        .all(|item| item["done_count"].as_u64().unwrap_or_default() >= 1));
    assert!(final_job_types
        .iter()
        .all(|item| item["latest_status"] == "done"));
}

#[tokio::test]
async fn test_api_v2_stats_summarize_memory_state() {
    let (base, client) = spawn_server().await;
    let user_id = uid();

    let active = client
        .post(format!("{base}/v2/memory/remember"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "content": "Rust platform guide for active services running across shared infrastructure clusters. This content is intentionally long enough to trigger derive_views so the active memory contributes overview and detail statistics after background processing. Platform teams rely on Rust ownership and borrowing to keep service workflows reliable under load.",
            "type": "semantic",
            "session_id": "sess-stats-live",
            "tags": ["rust", "active"]
        }))
        .send()
        .await
        .expect("remember active");
    assert_eq!(active.status(), 201);
    let active_body: Value = active.json().await.expect("active json");
    let active_id = active_body["memory_id"]
        .as_str()
        .expect("active id")
        .to_string();

    let archived = client
        .post(format!("{base}/v2/memory/remember"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "content": "Archived episodic note for a retired service lineage that used Rust operations workflows. This memory is also long enough to trigger derive_views before it is forgotten, ensuring job counters include view derivation in this stats scenario. Historical incident playbooks and migration notes are preserved for retrospective analysis.",
            "type": "episodic",
            "session_id": "sess-stats-archived",
            "tags": ["rust", "archive"]
        }))
        .send()
        .await
        .expect("remember archived");
    assert_eq!(archived.status(), 201);
    let archived_body: Value = archived.json().await.expect("archived json");
    let archived_id = archived_body["memory_id"]
        .as_str()
        .expect("archived id")
        .to_string();

    let _ = wait_for_jobs_done(&base, &client, &user_id, &active_id).await;
    let _ = wait_for_jobs_done(&base, &client, &user_id, &archived_id).await;

    let focus = client
        .post(format!("{base}/v2/memory/focus"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "type": "tag",
            "value": "rust",
            "boost": 1.5,
            "ttl_secs": 300
        }))
        .send()
        .await
        .expect("focus");
    assert_eq!(focus.status(), 201);

    let feedback_active = client
        .post(format!("{base}/v2/memory/{active_id}/feedback"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "signal": "useful",
            "context": "active useful"
        }))
        .send()
        .await
        .expect("feedback active");
    assert_eq!(feedback_active.status(), 201);

    let feedback_archived = client
        .post(format!("{base}/v2/memory/{archived_id}/feedback"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "signal": "wrong",
            "context": "archived wrong"
        }))
        .send()
        .await
        .expect("feedback archived");
    assert_eq!(feedback_archived.status(), 201);

    let forget = client
        .post(format!("{base}/v2/memory/forget"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "memory_id": archived_id,
            "reason": "archive"
        }))
        .send()
        .await
        .expect("forget archived");
    assert_eq!(forget.status(), 200);

    let stats = client
        .get(format!("{base}/v2/memory/stats"))
        .header("X-User-Id", &user_id)
        .send()
        .await
        .expect("stats");
    assert_eq!(stats.status(), 200);
    let body: Value = stats.json().await.expect("stats json");
    assert_eq!(body["total_memories"], 2);
    assert_eq!(body["active_memories"], 1);
    assert_eq!(body["forgotten_memories"], 1);
    assert_eq!(body["distinct_sessions"], 1);
    assert_eq!(body["has_overview_count"], 1);
    assert_eq!(body["has_detail_count"], 1);
    assert_eq!(body["active_direct_links"], 0);
    assert_eq!(body["active_focus_count"], 1);
    assert_eq!(body["tags"]["unique_count"], 2);
    assert_eq!(body["tags"]["assignment_count"], 2);
    assert_eq!(body["jobs"]["total_count"], 6);
    assert_eq!(body["jobs"]["pending_count"], 0);
    assert_eq!(body["jobs"]["in_progress_count"], 0);
    assert_eq!(body["jobs"]["done_count"], 6);
    assert_eq!(body["jobs"]["failed_count"], 0);
    assert_eq!(body["feedback"]["total"], 2);
    assert_eq!(body["feedback"]["useful"], 1);
    assert_eq!(body["feedback"]["irrelevant"], 0);
    assert_eq!(body["feedback"]["outdated"], 0);
    assert_eq!(body["feedback"]["wrong"], 1);
    let by_type = body["by_type"].as_array().expect("by_type");
    assert!(by_type.iter().any(|item| {
        item["type"] == "semantic"
            && item["total_count"] == 1
            && item["active_count"] == 1
            && item["forgotten_count"] == 0
    }));
    assert!(by_type.iter().any(|item| {
        item["type"] == "episodic"
            && item["total_count"] == 1
            && item["active_count"] == 0
            && item["forgotten_count"] == 1
    }));
}

#[tokio::test]
async fn test_api_v2_related_supports_multi_hop_traversal() {
    let (base, client) = spawn_server().await;
    let user_id = uid();

    let first = client
        .post(format!("{base}/v2/memory/remember"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "content": "Alpha memory linked to bridge",
            "session_id": "sess-hops",
            "tags": ["alpha"]
        }))
        .send()
        .await
        .expect("remember first");
    assert_eq!(first.status(), 201);
    let first_body: Value = first.json().await.expect("first json");
    let first_id = first_body["memory_id"]
        .as_str()
        .expect("first id")
        .to_string();

    let middle = client
        .post(format!("{base}/v2/memory/remember"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "content": "Bridge memory linked to alpha and beta",
            "session_id": "sess-hops",
            "tags": ["alpha", "beta"]
        }))
        .send()
        .await
        .expect("remember middle");
    assert_eq!(middle.status(), 201);
    let middle_body: Value = middle.json().await.expect("middle json");
    let middle_id = middle_body["memory_id"]
        .as_str()
        .expect("middle id")
        .to_string();

    let third = client
        .post(format!("{base}/v2/memory/remember"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "content": "Beta memory reachable in two hops",
            "session_id": "sess-hops",
            "tags": ["beta"]
        }))
        .send()
        .await
        .expect("remember third");
    assert_eq!(third.status(), 201);
    let third_body: Value = third.json().await.expect("third json");
    let third_id = third_body["memory_id"]
        .as_str()
        .expect("third id")
        .to_string();

    let _ = wait_for_link_count(&base, &client, &user_id, &middle_id, "outbound", 2).await;

    let direct = client
        .get(format!("{base}/v2/memory/related"))
        .header("X-User-Id", &user_id)
        .query(&[
            ("memory_id", first_id.as_str()),
            ("limit", "10"),
            ("max_hops", "1"),
        ])
        .send()
        .await
        .expect("direct related");
    assert_eq!(direct.status(), 200);
    let direct_body: Value = direct.json().await.expect("direct json");
    let direct_items = direct_body["items"].as_array().expect("direct items");
    assert_eq!(direct_items.len(), 1);
    assert_eq!(direct_body["summary"]["discovered_count"], 1);
    assert_eq!(direct_body["summary"]["returned_count"], 1);
    assert_eq!(direct_body["summary"]["truncated"], false);
    assert_eq!(direct_items[0]["id"], middle_id);
    assert_eq!(direct_items[0]["hop_distance"], 1);
    assert_eq!(direct_items[0]["via_memory_ids"], json!([]));
    assert!(
        direct_items[0]["supporting_path_count"]
            .as_i64()
            .unwrap_or_default()
            >= 1
    );
    assert_eq!(
        direct_items[0]["supporting_paths_truncated"],
        Value::Bool(
            direct_items[0]["supporting_path_count"]
                .as_i64()
                .expect("direct supporting path count")
                > direct_items[0]["supporting_paths"]
                    .as_array()
                    .expect("direct supporting paths")
                    .len() as i64
        )
    );
    let direct_supporting_paths = direct_items[0]["supporting_paths"]
        .as_array()
        .expect("direct supporting paths");
    assert!(direct_supporting_paths
        .iter()
        .enumerate()
        .all(|(index, path)| path["path_rank"] == json!(index as i64 + 1)));
    let selected_direct_paths = direct_supporting_paths
        .iter()
        .filter(|path| path["selected"] == true)
        .collect::<Vec<_>>();
    assert_eq!(selected_direct_paths.len(), 1);
    assert_eq!(selected_direct_paths[0]["path_rank"], 1);
    assert_eq!(selected_direct_paths[0]["hop_distance"], 1);
    assert_eq!(selected_direct_paths[0]["via_memory_ids"], json!([]));
    assert_eq!(selected_direct_paths[0]["selection_reason"], "best_path");
    assert_eq!(
        selected_direct_paths[0]["lineage"]
            .as_array()
            .expect("selected direct lineage")
            .len(),
        1
    );
    assert!(direct_supporting_paths
        .iter()
        .filter(|path| path["selected"] != true)
        .all(|path| matches!(
            path["selection_reason"].as_str(),
            Some("higher_hop_distance" | "lower_strength" | "tie_break")
        )));
    let direct_lineage = direct_items[0]["lineage"]
        .as_array()
        .expect("direct lineage");
    assert_eq!(direct_lineage.len(), 1);
    assert_eq!(direct_lineage[0]["from_memory_id"], first_id);
    assert_eq!(direct_lineage[0]["to_memory_id"], middle_id);
    assert_eq!(
        direct_lineage[0]["direction"],
        selected_direct_paths[0]["lineage"][0]["direction"]
    );
    assert_eq!(direct_lineage[0]["link_type"], "tag_overlap");
    assert_eq!(
        direct_lineage[0]["provenance"]["evidence_types"],
        json!(["tag_overlap"])
    );
    assert_eq!(
        direct_lineage[0]["provenance"]["primary_evidence_type"],
        "tag_overlap"
    );
    assert_eq!(direct_lineage[0]["provenance"]["refined"], false);
    assert_eq!(
        direct_lineage[0]["provenance"]["extraction_trace"]["derivation_state"],
        "complete"
    );
    assert_eq!(
        direct_lineage[0]["provenance"]["extraction_trace"]["latest_job_status"],
        "done"
    );
    match direct_lineage[0]["direction"].as_str() {
        Some("outbound") => {
            assert_eq!(direct_lineage[0]["strength"], 1.0);
            assert_eq!(
                direct_lineage[0]["provenance"]["primary_evidence_strength"],
                1.0
            );
            assert_eq!(
                direct_lineage[0]["provenance"]["evidence"],
                json!([{
                    "type": "tag_overlap",
                    "strength": 1.0,
                    "overlap_count": 1,
                    "source_tag_count": 1,
                    "target_tag_count": 2
                }])
            );
        }
        Some("inbound") => {
            assert_eq!(direct_lineage[0]["strength"], 0.5);
            assert_eq!(
                direct_lineage[0]["provenance"]["primary_evidence_strength"],
                0.5
            );
            assert_eq!(
                direct_lineage[0]["provenance"]["evidence"],
                json!([{
                    "type": "tag_overlap",
                    "strength": 0.5,
                    "overlap_count": 1,
                    "source_tag_count": 2,
                    "target_tag_count": 1
                }])
            );
        }
        other => panic!("unexpected direct lineage direction: {other:?}"),
    }

    let multi_hop = client
        .get(format!("{base}/v2/memory/related"))
        .header("X-User-Id", &user_id)
        .query(&[
            ("memory_id", first_id.as_str()),
            ("limit", "10"),
            ("max_hops", "2"),
        ])
        .send()
        .await
        .expect("multi hop related");
    assert_eq!(multi_hop.status(), 200);
    let multi_body: Value = multi_hop.json().await.expect("multi json");
    let multi_items = multi_body["items"].as_array().expect("multi items");
    assert_eq!(multi_items.len(), 2);
    assert_eq!(multi_body["summary"]["discovered_count"], 2);
    assert_eq!(multi_body["summary"]["returned_count"], 2);
    assert_eq!(multi_body["summary"]["truncated"], false);
    let multi_hops = multi_body["summary"]["by_hop"]
        .as_array()
        .expect("multi hop summary");
    assert!(multi_hops
        .iter()
        .any(|item| item["hop_distance"] == 1 && item["count"] == 1));
    assert!(multi_hops
        .iter()
        .any(|item| item["hop_distance"] == 2 && item["count"] == 1));
    assert_eq!(multi_items[0]["id"], middle_id);
    assert_eq!(multi_items[0]["hop_distance"], 1);
    assert_eq!(multi_items[0]["via_memory_ids"], json!([]));
    assert!(multi_items.iter().any(|item| {
        let supporting_paths = item["supporting_paths"]
            .as_array()
            .expect("supporting paths");
        let selected_paths = supporting_paths
            .iter()
            .filter(|path| path["selected"] == true)
            .collect::<Vec<_>>();
        item["id"] == third_id
            && item["hop_distance"] == 2
            && item["via_memory_ids"] == json!([middle_id])
            && item["supporting_path_count"].as_i64().unwrap_or_default() >= 1
            && item["supporting_paths_truncated"]
                == json!(
                    item["supporting_path_count"].as_i64().unwrap_or_default()
                        > supporting_paths.len() as i64
                )
            && supporting_paths
                .iter()
                .enumerate()
                .all(|(index, path)| path["path_rank"] == json!(index as i64 + 1))
            && selected_paths.len() == 1
            && selected_paths[0]["path_rank"] == 1
            && selected_paths[0]["hop_distance"] == 2
            && selected_paths[0]["via_memory_ids"] == json!([middle_id])
            && selected_paths[0]["selection_reason"] == "best_path"
            && selected_paths[0]["lineage"]
                .as_array()
                .map(|steps| steps.len() == 2)
                .unwrap_or(false)
            && supporting_paths
                .iter()
                .filter(|path| path["selected"] != true)
                .all(|path| {
                    matches!(
                        path["selection_reason"].as_str(),
                        Some("higher_hop_distance" | "lower_strength" | "tie_break")
                    )
                })
            && item["lineage"]
                .as_array()
                .map(|steps| {
                    steps.len() == 2
                        && steps[0]["from_memory_id"] == first_id
                        && steps[0]["to_memory_id"] == middle_id
                        && steps[1]["from_memory_id"] == middle_id
                        && steps[1]["to_memory_id"] == third_id
                        && item["lineage"] == selected_paths[0]["lineage"]
                })
                .unwrap_or(false)
    }));

    let truncated = client
        .get(format!("{base}/v2/memory/related"))
        .header("X-User-Id", &user_id)
        .query(&[
            ("memory_id", first_id.as_str()),
            ("limit", "1"),
            ("max_hops", "2"),
        ])
        .send()
        .await
        .expect("truncated related");
    assert_eq!(truncated.status(), 200);
    let truncated_body: Value = truncated.json().await.expect("truncated json");
    assert_eq!(truncated_body["summary"]["discovered_count"], 2);
    assert_eq!(truncated_body["summary"]["returned_count"], 1);
    assert_eq!(truncated_body["summary"]["truncated"], true);
    assert_eq!(
        truncated_body["items"]
            .as_array()
            .expect("truncated items")
            .len(),
        1
    );
}

#[tokio::test]
async fn test_api_v2_related_reorders_with_session_affinity_and_focus() {
    let (base, client) = spawn_server().await;
    let user_id = uid();

    let root = client
        .post(format!("{base}/v2/memory/remember"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "content": "Root memory for related ranking",
            "session_id": "sess-related-rank",
            "tags": ["shared", "root"]
        }))
        .send()
        .await
        .expect("remember root");
    assert_eq!(root.status(), 201);
    let root_body: Value = root.json().await.expect("root json");
    let root_id = root_body["memory_id"]
        .as_str()
        .expect("root id")
        .to_string();

    let focused_other = client
        .post(format!("{base}/v2/memory/remember"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "content": "Priority memory in another session",
            "session_id": "sess-other",
            "tags": ["shared", "priority"]
        }))
        .send()
        .await
        .expect("remember focused other");
    assert_eq!(focused_other.status(), 201);
    let focused_other_body: Value = focused_other.json().await.expect("focused other json");
    let focused_other_id = focused_other_body["memory_id"]
        .as_str()
        .expect("focused other id")
        .to_string();

    let same_session = client
        .post(format!("{base}/v2/memory/remember"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "content": "Same-session related memory",
            "session_id": "sess-related-rank",
            "tags": ["shared", "same"]
        }))
        .send()
        .await
        .expect("remember same session");
    assert_eq!(same_session.status(), 201);
    let same_session_body: Value = same_session.json().await.expect("same session json");
    let same_session_id = same_session_body["memory_id"]
        .as_str()
        .expect("same session id")
        .to_string();

    let _ = wait_for_jobs_done(&base, &client, &user_id, &root_id).await;
    let _ = wait_for_jobs_done(&base, &client, &user_id, &focused_other_id).await;
    let _ = wait_for_jobs_done(&base, &client, &user_id, &same_session_id).await;
    let _ = wait_for_link_count(&base, &client, &user_id, &root_id, "both", 2).await;

    let baseline = client
        .get(format!("{base}/v2/memory/related"))
        .header("X-User-Id", &user_id)
        .query(&[
            ("memory_id", root_id.as_str()),
            ("limit", "10"),
            ("max_hops", "1"),
        ])
        .send()
        .await
        .expect("baseline related");
    assert_eq!(baseline.status(), 200);
    let baseline_body: Value = baseline.json().await.expect("baseline json");
    let baseline_items = baseline_body["items"].as_array().expect("baseline items");
    assert_eq!(baseline_items.len(), 2);
    assert_eq!(baseline_items[0]["id"], same_session_id);
    assert_eq!(
        baseline_items[0]["ranking"]["session_affinity_applied"],
        true
    );
    assert_eq!(
        baseline_items[0]["ranking"]["session_affinity_multiplier"],
        1.08
    );
    assert_eq!(baseline_items[0]["ranking"]["focus_boost"], 1.0);

    let focus = client
        .post(format!("{base}/v2/memory/focus"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "type": "tag",
            "value": "priority",
            "boost": 3.0,
            "ttl_secs": 300
        }))
        .send()
        .await
        .expect("focus");
    assert_eq!(focus.status(), 201);

    let focused = client
        .get(format!("{base}/v2/memory/related"))
        .header("X-User-Id", &user_id)
        .query(&[
            ("memory_id", root_id.as_str()),
            ("limit", "10"),
            ("max_hops", "1"),
        ])
        .send()
        .await
        .expect("focused related");
    assert_eq!(focused.status(), 200);
    let focused_body: Value = focused.json().await.expect("focused json");
    let focused_items = focused_body["items"].as_array().expect("focused items");
    assert_eq!(focused_items.len(), 2);
    assert_eq!(focused_items[0]["id"], focused_other_id);
    assert_eq!(focused_items[0]["ranking"]["focus_boost"], 3.0);
    assert!(
        focused_items[0]["ranking"]["same_hop_score"]
            .as_f64()
            .unwrap_or_default()
            > focused_items[1]["ranking"]["same_hop_score"]
                .as_f64()
                .unwrap_or_default()
    );
    assert!(focused_items[0]["ranking"]["focus_matches"]
        .as_array()
        .map(|items| {
            items.iter().any(|item| {
                item["type"] == "tag" && item["value"] == "priority" && item["boost"] == 3.0
            })
        })
        .unwrap_or(false));
}

#[tokio::test]
async fn test_api_v2_uses_canonical_request_contracts() {
    let (base, client) = spawn_server().await;
    let user_id = uid();

    let remember = client
        .post(format!("{base}/v2/memory/remember"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "content": "Rust platform guide for systems teams working on shared infrastructure services and platforms. This content is long enough to trigger view derivation so that overview and detail text are populated after processing. Platform engineers use Rust ownership to build reliable distributed systems.",
            "type": "semantic",
            "session_id": "sess-alias",
            "tags": ["rust", "systems"]
        }))
        .send()
        .await
        .expect("remember with alias");
    assert_eq!(remember.status(), 201);
    let remembered: Value = remember.json().await.expect("remember alias json");
    let memory_id = remembered["memory_id"]
        .as_str()
        .expect("alias memory id")
        .to_string();

    let related = client
        .post(format!("{base}/v2/memory/remember"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "content": "Rust service handbook for infrastructure teams building shared platform services across the stack. This content is long enough to trigger derive_views so the test can verify overview and detail after processing. Infrastructure engineers rely on Rust memory safety for distributed service work.",
            "type": "semantic",
            "session_id": "sess-alias",
            "tags": ["rust", "infra"]
        }))
        .send()
        .await
        .expect("related remember with alias");
    assert_eq!(related.status(), 201);
    let related_body: Value = related.json().await.expect("related alias json");
    let related_id = related_body["memory_id"]
        .as_str()
        .expect("related alias memory id")
        .to_string();

    let procedural = client
        .post(format!("{base}/v2/memory/remember"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "content": "Rotate on-call checklist every Friday across platform teams and document ownership handoff before weekend support windows. This procedural note is intentionally long enough to trigger derive_views so canonical request contract assertions can wait for overview and detail fields without timing out.",
            "type": "procedural",
            "session_id": "sess-alias"
        }))
        .send()
        .await
        .expect("procedural remember");
    assert_eq!(procedural.status(), 201);
    let procedural_body: Value = procedural.json().await.expect("procedural json");
    let procedural_id = procedural_body["memory_id"]
        .as_str()
        .expect("procedural memory id")
        .to_string();

    let _ = wait_for_views(&base, &client, &user_id, &memory_id).await;
    let _ = wait_for_views(&base, &client, &user_id, &related_id).await;
    let _ = wait_for_views(&base, &client, &user_id, &procedural_id).await;
    let _ = wait_for_link_count(&base, &client, &user_id, &memory_id, "both", 1).await;

    let list = client
        .get(format!("{base}/v2/memory/list"))
        .header("X-User-Id", &user_id)
        .query(&[("type", "semantic"), ("limit", "10")])
        .send()
        .await
        .expect("list with canonical type");
    assert_eq!(list.status(), 200);
    let listed: Value = list.json().await.expect("list json");
    let items = listed["items"].as_array().expect("list items");
    assert!(items.iter().any(|memory| memory["id"] == memory_id));
    assert!(items.iter().any(|memory| memory["id"] == related_id));
    assert!(items.iter().all(|memory| memory["type"] == "semantic"));
    assert!(items.iter().all(|memory| memory["id"] != procedural_id));

    let recall = client
        .post(format!("{base}/v2/memory/recall"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "query": "rust platform",
            "top_k": 5,
            "max_tokens": 2000,
            "view": "full",
            "type": "semantic"
        }))
        .send()
        .await
        .expect("recall with aliases");
    assert_eq!(recall.status(), 200);
    let recalled: Value = recall.json().await.expect("recall alias json");
    let memories = recalled["memories"].as_array().expect("recalled memories");
    assert!(!memories.is_empty());
    let recalled_memory = memories
        .iter()
        .find(|memory| memory["id"] == memory_id)
        .expect("recalled original memory");
    assert!(!recalled_memory["overview"].is_null());

    let expand = client
        .post(format!("{base}/v2/memory/expand"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "memory_id": memory_id,
            "level": "detail"
        }))
        .send()
        .await
        .expect("expand with full alias");
    assert_eq!(expand.status(), 200);
    let expanded: Value = expand.json().await.expect("expand alias json");
    assert!(!expanded["detail"].is_null());

    let focus = client
        .post(format!("{base}/v2/memory/focus"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "type": "session",
            "value": "sess-alias",
            "ttl_secs": 3600
        }))
        .send()
        .await
        .expect("focus with aliases");
    assert_eq!(focus.status(), 201);
    let focused: Value = focus.json().await.expect("focus alias json");
    assert_eq!(focused["type"], "session");
    assert_eq!(focused["value"], "sess-alias");
    assert!(focused["active_until"].as_str().is_some());
}

#[tokio::test]
async fn test_api_v2_recall_filters_by_tags_any_and_all() {
    let (base, client) = spawn_server().await;
    let user_id = uid();

    for payload in [
        json!({
            "content": "Rust systems handbook",
            "session_id": "sess-tag-filter",
            "tags": ["rust", "systems"]
        }),
        json!({
            "content": "Python data handbook",
            "session_id": "sess-tag-filter",
            "tags": ["python", "data"]
        }),
        json!({
            "content": "Rust runtime cheatsheet",
            "session_id": "sess-tag-filter",
            "tags": ["rust", "runtime"]
        }),
    ] {
        let response = client
            .post(format!("{base}/v2/memory/remember"))
            .header("X-User-Id", &user_id)
            .json(&payload)
            .send()
            .await
            .expect("remember");
        assert_eq!(response.status(), 201);
    }

    let any = client
        .post(format!("{base}/v2/memory/recall"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "query": "handbook",
            "top_k": 10,
            "scope": "session",
            "session_id": "sess-tag-filter",
            "tags": ["SYSTEMS", "python"],
            "tag_filter_mode": "any"
        }))
        .send()
        .await
        .expect("recall any");
    assert_eq!(any.status(), 200);
    let any_body: Value = any.json().await.expect("recall any json");
    let any_memories = any_body["memories"].as_array().expect("any memories");
    assert_eq!(any_memories.len(), 2);
    assert!(any_memories
        .iter()
        .any(|memory| memory["text"] == "Rust systems handbook"));
    assert!(any_memories
        .iter()
        .any(|memory| memory["text"] == "Python data handbook"));

    let all = client
        .post(format!("{base}/v2/memory/recall"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "query": "handbook",
            "top_k": 10,
            "scope": "session",
            "session_id": "sess-tag-filter",
            "tags": ["rust", "systems"],
            "tag_filter_mode": "all"
        }))
        .send()
        .await
        .expect("recall all");
    assert_eq!(all.status(), 200);
    let all_body: Value = all.json().await.expect("recall all json");
    let all_memories = all_body["memories"].as_array().expect("all memories");
    assert_eq!(all_memories.len(), 1);
    assert_eq!(all_memories[0]["text"], "Rust systems handbook");

    let unfiltered = client
        .post(format!("{base}/v2/memory/recall"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "query": "handbook",
            "top_k": 10,
            "scope": "session",
            "session_id": "sess-tag-filter",
            "tags": [],
            "tag_filter_mode": "all"
        }))
        .send()
        .await
        .expect("recall unfiltered");
    assert_eq!(unfiltered.status(), 200);
    let unfiltered_body: Value = unfiltered.json().await.expect("recall unfiltered json");
    assert_eq!(
        unfiltered_body["memories"]
            .as_array()
            .expect("unfiltered memories")
            .len(),
        3
    );
}

#[tokio::test]
async fn test_api_v2_recall_rejects_invalid_tag_filter_mode() {
    let (base, client) = spawn_server().await;
    let user_id = uid();

    let response = client
        .post(format!("{base}/v2/memory/recall"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "query": "platform",
            "tags": ["rust"],
            "tag_filter_mode": "bad"
        }))
        .send()
        .await
        .expect("invalid tag filter mode");
    assert_eq!(response.status(), 422);
}

#[tokio::test]
async fn test_api_v2_links_and_related_navigation() {
    let (base, client) = spawn_server().await;
    let user_id = uid();

    let first = client
        .post(format!("{base}/v2/memory/remember"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "content": "Alpha rust handbook for platform teams",
            "session_id": "sess-links",
            "tags": ["shared", "alpha"]
        }))
        .send()
        .await
        .expect("remember first");
    assert_eq!(first.status(), 201);
    let first_body: Value = first.json().await.expect("first json");
    let first_id = first_body["memory_id"]
        .as_str()
        .expect("first id")
        .to_string();

    let middle = client
        .post(format!("{base}/v2/memory/remember"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "content": "Shared beta rust guide for platform teams",
            "session_id": "sess-links",
            "tags": ["shared", "beta"]
        }))
        .send()
        .await
        .expect("remember middle");
    assert_eq!(middle.status(), 201);
    let middle_body: Value = middle.json().await.expect("middle json");
    let middle_id = middle_body["memory_id"]
        .as_str()
        .expect("middle id")
        .to_string();

    let third = client
        .post(format!("{base}/v2/memory/remember"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "content": "Gamma beta operations guide for platform teams",
            "session_id": "sess-links",
            "tags": ["beta", "gamma"]
        }))
        .send()
        .await
        .expect("remember third");
    assert_eq!(third.status(), 201);
    let third_body: Value = third.json().await.expect("third json");
    let third_id = third_body["memory_id"]
        .as_str()
        .expect("third id")
        .to_string();

    let update_first = client
        .patch(format!("{base}/v2/memory/update"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "memory_id": &first_id,
            "tags_add": ["anchor"]
        }))
        .send()
        .await
        .expect("update first");
    assert_eq!(update_first.status(), 200);

    let update_middle = client
        .patch(format!("{base}/v2/memory/update"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "memory_id": &middle_id,
            "tags_add": ["pivot"]
        }))
        .send()
        .await
        .expect("update middle");
    assert_eq!(update_middle.status(), 200);

    let links = wait_for_link_count(&base, &client, &user_id, &middle_id, "both", 4).await;
    let link_items = links["items"].as_array().expect("link items");
    assert_eq!(link_items.len(), 4);
    assert!(link_items
        .iter()
        .any(|item| item["id"] == first_id && item["direction"] == "outbound"));
    assert!(link_items
        .iter()
        .any(|item| item["id"] == first_id && item["direction"] == "inbound"));
    assert!(link_items
        .iter()
        .any(|item| item["id"] == third_id && item["direction"] == "outbound"));

    let filtered_type = link_items[0]["link_type"]
        .as_str()
        .expect("link type")
        .to_string();

    let outbound = client
        .get(format!("{base}/v2/memory/links"))
        .header("X-User-Id", &user_id)
        .query(&[
            ("memory_id", middle_id.as_str()),
            ("direction", "outbound"),
            ("limit", "10"),
            ("link_type", filtered_type.as_str()),
            ("min_strength", "0.0"),
        ])
        .send()
        .await
        .expect("outbound links");
    assert_eq!(outbound.status(), 200);
    let outbound_body: Value = outbound.json().await.expect("outbound json");
    let outbound_items = outbound_body["items"].as_array().expect("outbound items");
    assert_eq!(outbound_body["summary"]["outbound_count"], 2);
    assert_eq!(outbound_body["summary"]["inbound_count"], 2);
    assert_eq!(outbound_body["summary"]["total_count"], 4);
    let link_type_summary = outbound_body["summary"]["link_types"]
        .as_array()
        .expect("link type summary");
    assert!(link_type_summary.iter().any(|item| {
        item["type"] == filtered_type && item["outbound_count"].as_u64().unwrap_or_default() >= 1
    }));
    assert_eq!(
        link_type_summary
            .iter()
            .map(|item| {
                item["outbound_count"].as_u64().unwrap_or_default()
                    + item["inbound_count"].as_u64().unwrap_or_default()
            })
            .sum::<u64>(),
        4
    );
    assert!(!outbound_items.is_empty());
    assert!(outbound_items
        .iter()
        .all(|item| item["direction"] == "outbound"));
    assert!(outbound_items
        .iter()
        .all(|item| item["link_type"] == filtered_type));
    assert!(outbound_items.iter().all(|item| item["type"].is_string()));
    assert!(outbound_items
        .iter()
        .all(|item| item.get("memory_type").is_none()));
    assert!(outbound_items
        .iter()
        .all(|item| !item["provenance"].is_null()));
    assert!(outbound_items.iter().all(|item| {
        item["provenance"]["evidence_types"]
            .as_array()
            .map(|items| !items.is_empty())
            .unwrap_or(false)
    }));
    assert!(outbound_items.iter().all(|item| {
        item["provenance"]["evidence"]
            .as_array()
            .map(|items| {
                items.iter().any(|detail| {
                    detail["type"] == "tag_overlap"
                        && detail["overlap_count"] == 1
                        && detail["source_tag_count"] == 3
                })
            })
            .unwrap_or(false)
    }));

    let mut related_body = None;
    for _ in 0..V2_WAIT_ATTEMPTS {
        let related = client
            .get(format!("{base}/v2/memory/related"))
            .header("X-User-Id", &user_id)
            .query(&[("memory_id", middle_id.as_str()), ("limit", "10")])
            .send()
            .await
            .expect("related");
        assert_eq!(related.status(), 200);
        let body: Value = related.json().await.expect("related json");
        let items = body["items"].as_array().expect("related items");
        let first_ready = items.iter().any(|item| {
            item["id"] == first_id
                && item["supporting_path_count"].as_i64().unwrap_or_default() >= 1
                && item["supporting_paths"]
                    .as_array()
                    .map(|paths| {
                        paths.iter().any(|path| {
                            path["selected"] == true
                                && path["path_rank"] == 1
                                && path["selection_reason"] == "best_path"
                        })
                    })
                    .unwrap_or(false)
                && item["lineage"]
                    .as_array()
                    .map(|steps| {
                        steps.len() == 1
                            && steps[0]["from_memory_id"] == middle_id
                            && steps[0]["to_memory_id"] == first_id
                            && steps[0]["direction"] == "inbound"
                            && steps[0]["provenance"]["extraction_trace"]["latest_job_status"]
                                == "done"
                    })
                    .unwrap_or(false)
        });
        let third_ready = items.iter().any(|item| {
            item["id"] == third_id
                && item["supporting_path_count"].as_i64().unwrap_or_default() >= 1
                && item["supporting_paths"]
                    .as_array()
                    .map(|paths| {
                        paths.iter().any(|path| {
                            path["selected"] == true
                                && path["path_rank"] == 1
                                && path["selection_reason"] == "best_path"
                        })
                    })
                    .unwrap_or(false)
                && item["lineage"]
                    .as_array()
                    .map(|steps| {
                        steps.len() == 1
                            && steps[0]["from_memory_id"] == middle_id
                            && steps[0]["to_memory_id"] == third_id
                            && steps[0]["direction"] == "inbound"
                    })
                    .unwrap_or(false)
        });
        if first_ready && third_ready {
            related_body = Some(body);
            break;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(V2_WAIT_SLEEP_MS)).await;
    }
    let related_body = related_body.expect("timed out waiting for V2 related surface");
    let related_items = related_body["items"].as_array().expect("related items");
    assert_eq!(related_items.len(), 2);
    assert!(related_items.iter().all(|item| item["type"].is_string()));
    assert!(related_items
        .iter()
        .all(|item| item.get("memory_type").is_none()));
    assert_eq!(related_body["summary"]["discovered_count"], 2);
    assert_eq!(related_body["summary"]["returned_count"], 2);
    assert_eq!(related_body["summary"]["truncated"], false);
    let related_hops = related_body["summary"]["by_hop"]
        .as_array()
        .expect("related hop summary");
    assert_eq!(related_hops.len(), 1);
    assert_eq!(related_hops[0]["hop_distance"], 1);
    assert_eq!(related_hops[0]["count"], 2);
    assert!(related_body["summary"]["link_types"]
        .as_array()
        .expect("related link type summary")
        .iter()
        .any(|item| item["type"] == "tag_overlap"));
    assert!(related_items.iter().any(|item| {
        item["id"] == first_id
            && item["directions"]
                .as_array()
                .map(|dirs| dirs.len() == 2)
                .unwrap_or(false)
            && item["link_types"]
                .as_array()
                .map(|tys| !tys.is_empty())
                .unwrap_or(false)
            && item["supporting_path_count"].as_i64().unwrap_or_default() >= 1
            && item["supporting_paths"]
                .as_array()
                .map(|paths| {
                    paths.iter().any(|path| {
                        path["selected"] == true
                            && path["path_rank"] == 1
                            && path["selection_reason"] == "best_path"
                    })
                })
                .unwrap_or(false)
            && item["lineage"]
                .as_array()
                .map(|steps| {
                    steps.len() == 1
                        && steps[0]["from_memory_id"] == middle_id
                        && steps[0]["to_memory_id"] == first_id
                        && steps[0]["direction"] == "inbound"
                        && steps[0]["provenance"]["extraction_trace"]["latest_job_status"] == "done"
                })
                .unwrap_or(false)
    }));
    assert!(related_items.iter().any(|item| {
        item["id"] == third_id
            && item["link_types"]
                .as_array()
                .map(|tys| tys.iter().any(|ty| ty == "tag_overlap"))
                .unwrap_or(false)
            && item["supporting_path_count"].as_i64().unwrap_or_default() >= 1
            && item["supporting_paths"]
                .as_array()
                .map(|paths| {
                    paths.iter().any(|path| {
                        path["selected"] == true
                            && path["path_rank"] == 1
                            && path["selection_reason"] == "best_path"
                    })
                })
                .unwrap_or(false)
            && item["lineage"]
                .as_array()
                .map(|steps| {
                    steps.len() == 1
                        && steps[0]["from_memory_id"] == middle_id
                        && steps[0]["to_memory_id"] == third_id
                        && steps[0]["direction"] == "inbound"
                })
                .unwrap_or(false)
    }));

    let forget = client
        .post(format!("{base}/v2/memory/forget"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "memory_id": third_id,
            "reason": "cleanup"
        }))
        .send()
        .await
        .expect("forget third");
    assert_eq!(forget.status(), 200);

    let links_after_forget = client
        .get(format!("{base}/v2/memory/links"))
        .header("X-User-Id", &user_id)
        .query(&[
            ("memory_id", middle_id.as_str()),
            ("direction", "both"),
            ("limit", "10"),
            ("link_type", filtered_type.as_str()),
            ("min_strength", "0.0"),
        ])
        .send()
        .await
        .expect("links after forget");
    assert_eq!(links_after_forget.status(), 200);
    let links_after_forget_body: Value = links_after_forget
        .json()
        .await
        .expect("links after forget json");
    assert_eq!(links_after_forget_body["summary"]["outbound_count"], 1);
    assert_eq!(links_after_forget_body["summary"]["inbound_count"], 1);
    assert_eq!(links_after_forget_body["summary"]["total_count"], 2);

    let related_after = client
        .get(format!("{base}/v2/memory/related"))
        .header("X-User-Id", &user_id)
        .query(&[("memory_id", middle_id.as_str()), ("limit", "10")])
        .send()
        .await
        .expect("related after forget");
    assert_eq!(related_after.status(), 200);
    let related_after_body: Value = related_after.json().await.expect("related after json");
    let related_after_items = related_after_body["items"]
        .as_array()
        .expect("related after items");
    assert_eq!(related_after_items.len(), 1);
    assert_eq!(related_after_items[0]["id"], first_id);
}
