use std::sync::Arc;

use memoria_storage::SqlMemoryStore;

pub fn test_dim() -> usize {
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

pub fn uid() -> String {
    format!("api_v2_test_{}", uuid::Uuid::new_v4().simple())
}

pub const V2_WAIT_ATTEMPTS: usize = 500;
pub const V2_WAIT_SLEEP_MS: u64 = 300;

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

pub async fn spawn_server() -> (String, reqwest::Client) {
    spawn_server_with_llm(None).await
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

pub async fn spawn_server_with_master_key(master_key: &str) -> (String, reqwest::Client) {
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
        None,
    ).await);
    let state = memoria_api::AppState::new(service, git, master_key.to_string());
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
