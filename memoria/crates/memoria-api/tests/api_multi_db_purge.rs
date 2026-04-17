use serde_json::{json, Value};
use sqlx::MySqlPool;
use std::sync::Arc;
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

fn parse_db_name(database_url: &str) -> String {
    let suffix_start = database_url.find(['?', '#']).unwrap_or(database_url.len());
    let without_suffix = &database_url[..suffix_start];
    without_suffix
        .rsplit_once('/')
        .map(|(_, db_name)| db_name.to_string())
        .expect("database url must include db name")
}

fn shared_db_url() -> String {
    std::env::var("MEMORIA_SHARED_DATABASE_URL").unwrap_or_else(|_| db_url())
}

async fn spawn_server_multi_db() -> (
    String,
    reqwest::Client,
    tokio::task::JoinHandle<()>,
) {
    use memoria_git::GitForDataService;
    use memoria_service::MemoryService;
    use memoria_storage::{DbRouter, SqlMemoryStore};
    use sqlx::mysql::MySqlPool;

    let shared_db_url = shared_db_url();
    let router = Arc::new(
        DbRouter::connect(&shared_db_url, test_dim(), Uuid::new_v4().to_string())
            .await
            .expect("connect router"),
    );
    let mut store = SqlMemoryStore::connect(&shared_db_url, test_dim(), Uuid::new_v4().to_string())
        .await
        .expect("connect shared store");
    store.migrate_shared().await.expect("migrate shared store");
    store.set_db_router(router.clone());

    let pool = MySqlPool::connect(&shared_db_url)
        .await
        .expect("connect git pool");
    let git = Arc::new(GitForDataService::new(
        pool.clone(),
        &parse_db_name(&shared_db_url),
    ));
    let service = Arc::new(
        MemoryService::new_sql_with_llm_and_router(Arc::new(store), Some(router), None, None).await,
    );
    let state = memoria_api::AppState::new(service, git, String::new());
    let app = memoria_api::build_router(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let port = listener.local_addr().unwrap().port();
    let handle = tokio::spawn(async move { let _ = axum::serve(listener, app).await; });

    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(tokio::time::Duration::from_secs(30))
        .build()
        .expect("client");
    let base = format!("http://127.0.0.1:{port}");
    wait_for_server(&client, &base, &pool).await;
    if handle.is_finished() {
        panic!("Server task finished unexpectedly");
    }
    (base, client, handle)
}

async fn wait_for_server(client: &reqwest::Client, base: &str, pool: &MySqlPool) {
    let mut health_ok = false;
    for _ in 0..20 {
        if client.get(format!("{base}/health")).send().await.is_ok() {
            health_ok = true;
            break;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    }
    assert!(health_ok, "server /health never became reachable");
    for _ in 0..20 {
        if sqlx::query("SELECT 1").execute(pool).await.is_ok() {
            return;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    }
    panic!("DB not ready after 1s");
}

async fn store_memory(client: &reqwest::Client, base: &str, user_id: &str, body: Value) -> Value {
    let response = client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", user_id)
        .json(&body)
        .send()
        .await
        .expect("store request");
    assert_eq!(response.status(), 201, "store response should succeed");
    response.json().await.expect("store response body")
}

async fn purge_memories(client: &reqwest::Client, base: &str, user_id: &str, body: Value) -> Value {
    let response = client
        .post(format!("{base}/v1/memories/purge"))
        .header("X-User-Id", user_id)
        .json(&body)
        .send()
        .await
        .expect("purge request");
    assert_eq!(response.status(), 200, "purge response should succeed");
    response.json().await.expect("purge response body")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_multidb_purge_paths_use_qualified_tables() {
    let (base, client, handle) = spawn_server_multi_db().await;

    let ids_user = format!("api-multi-ids-{}", Uuid::new_v4().simple());
    let topic_user = format!("api-multi-topic-{}", Uuid::new_v4().simple());
    let session_user = format!("api-multi-session-{}", Uuid::new_v4().simple());

    let stored = store_memory(
        &client,
        &base,
        &ids_user,
        json!({"content": "multi-db purge by ids", "memory_type": "semantic"}),
    )
    .await;
    let memory_id = stored["memory_id"]
        .as_str()
        .expect("stored memory_id")
        .to_string();
    let body = purge_memories(
        &client,
        &base,
        &ids_user,
        json!({"memory_ids": [memory_id]}),
    )
    .await;
    assert_eq!(body["purged"], 1, "memory_ids purge should delete one row");

    let topic_token = format!("topic-token-{}", Uuid::new_v4().simple());
    store_memory(
        &client,
        &base,
        &topic_user,
        json!({"content": format!("orchid harbor {topic_token}"), "memory_type": "semantic"}),
    )
    .await;
    let body = purge_memories(&client, &base, &topic_user, json!({"topic": topic_token})).await;
    assert_eq!(body["purged"], 1, "topic purge should delete matching rows");

    let target_session = format!("session:multi-db:{}", Uuid::new_v4().simple());
    let other_session = format!("session:multi-db:{}", Uuid::new_v4().simple());
    store_memory(
        &client,
        &base,
        &session_user,
        json!({"content": "lantern graphite orbit", "memory_type": "working", "session_id": target_session}),
    )
    .await;
    store_memory(
        &client,
        &base,
        &session_user,
        json!({"content": "cobalt meadow prism", "memory_type": "working", "session_id": other_session}),
    )
    .await;
    let body = purge_memories(
        &client,
        &base,
        &session_user,
        json!({"session_id": target_session}),
    )
    .await;
    assert_eq!(
        body["purged"], 1,
        "session purge should delete target session rows"
    );

    drop(client);
    handle.abort();
    let _ = handle.await;
}
