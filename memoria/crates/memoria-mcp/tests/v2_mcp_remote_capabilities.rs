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

#[tokio::test]
async fn test_e2e_remote_capabilities_include_v2_tools() {
    let base = spawn_api_server().await;
    let uid = format!("remote_caps_{}", Uuid::new_v4().simple());
    let remote = memoria_mcp::remote::RemoteClient::new(&base, None, uid, Some("codex"));

    let caps = call_remote(&remote, "memory_capabilities", json!({})).await;
    let body = text(&caps);
    assert!(body.contains("memory_v2_remember"), "got: {body}");
    assert!(body.contains("memory_v2_recall"), "got: {body}");
    assert!(body.contains("memory_v2_reflect"), "got: {body}");
}
