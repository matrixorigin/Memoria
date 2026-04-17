use serde_json::{json, Value};
use serial_test::serial;
use sqlx::{mysql::MySqlPoolOptions, MySqlPool};
use std::{sync::Arc, time::Duration};

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

fn replace_db_name(database_url: &str, db_name: &str) -> String {
    let suffix_start = database_url.find(['?', '#']).unwrap_or(database_url.len());
    let (without_suffix, suffix) = database_url.split_at(suffix_start);
    let (base, _) = without_suffix
        .rsplit_once('/')
        .expect("database url must include db name");
    format!("{base}/{db_name}{suffix}")
}

fn shared_db_url() -> String {
    replace_db_name(
        &db_url(),
        &format!(
            "memoria_api_multi_{}",
            &uuid::Uuid::new_v4().simple().to_string()[..8]
        ),
    )
}

fn uid(prefix: &str) -> String {
    format!("{prefix}_{}", uuid::Uuid::new_v4().simple())
}

async fn wait_for_server(client: &reqwest::Client, base: &str, pool: &MySqlPool) {
    for _ in 0..20 {
        if client.get(format!("{base}/health")).send().await.is_ok() {
            break;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    }

    for _ in 0..20 {
        if sqlx::query("SELECT 1").execute(pool).await.is_ok() {
            return;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    }

    panic!("DB not ready after 1s");
}

async fn spawn_multi_db_server() -> (
    String,
    reqwest::Client,
    Arc<memoria_storage::DbRouter>,
    String,
) {
    use memoria_git::GitForDataService;
    use memoria_service::MemoryService;
    use memoria_storage::{DbRouter, SqlMemoryStore};

    let shared_db_url = shared_db_url();
    memoria_test_utils::wait_for_mysql_ready(&shared_db_url, Duration::from_secs(30)).await;

    let router = Arc::new(
        DbRouter::connect(&shared_db_url, test_dim(), uuid::Uuid::new_v4().to_string())
            .await
            .expect("router"),
    );
    let shared_pool = router.shared_pool().clone();
    let shared_pool_max_connections = router.shared_pool_max_connections();
    let mut store = SqlMemoryStore::from_existing_pool(
        shared_pool.clone(),
        test_dim(),
        uuid::Uuid::new_v4().to_string(),
        Some(shared_db_url.clone()),
        Some(shared_pool_max_connections),
        "api_multi_db_shared_pool",
    );
    store.migrate_shared().await.expect("migrate_shared");
    store.set_db_router(router.clone());

    let git = Arc::new(GitForDataService::new(
        shared_pool.clone(),
        router.shared_db_name().to_string(),
    ));
    let service = Arc::new(
        MemoryService::new_sql_with_llm_and_router(
            Arc::new(store),
            Some(router.clone()),
            None,
            None,
        )
        .await,
    );
    let state = memoria_api::AppState::new(service, git, String::new());
    let app = memoria_api::build_router(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let port = listener.local_addr().expect("local addr").port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let base = format!("http://127.0.0.1:{port}");
    wait_for_server(&client, &base, &shared_pool).await;
    (base, client, router, shared_db_url)
}

async fn store_memory(
    base: &str,
    client: &reqwest::Client,
    user_id: &str,
    content: &str,
) -> String {
    let response = client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", user_id)
        .json(&json!({ "content": content }))
        .send()
        .await
        .expect("store request");
    assert_eq!(response.status(), 201);
    let body: Value = response.json().await.expect("store response body");
    body["memory_id"].as_str().expect("memory_id").to_string()
}

async fn list_memory_ids(base: &str, client: &reqwest::Client, user_id: &str) -> Vec<String> {
    let response = client
        .get(format!("{base}/v1/memories"))
        .header("X-User-Id", user_id)
        .send()
        .await
        .expect("list request");
    assert_eq!(response.status(), 200);
    let body: Value = response.json().await.expect("list response body");
    body["items"]
        .as_array()
        .expect("items")
        .iter()
        .map(|item| {
            item["memory_id"]
                .as_str()
                .expect("list memory_id")
                .to_string()
        })
        .collect()
}

async fn user_db_pool(shared_db_url: &str, db_name: &str) -> MySqlPool {
    MySqlPoolOptions::new()
        .max_connections(1)
        .connect(&replace_db_name(shared_db_url, db_name))
        .await
        .expect("connect user db")
}

async fn count_user_memories(pool: &MySqlPool, user_id: &str) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM mem_memories WHERE user_id = ?")
        .bind(user_id)
        .fetch_one(pool)
        .await
        .expect("count user memories")
}

#[tokio::test]
#[serial]
async fn api_multi_db_routes_reads_and_writes_to_each_users_database() {
    let (base, client, router, shared_db_url) = spawn_multi_db_server().await;
    let user_a = uid("api_multi_a");
    let user_b = uid("api_multi_b");

    let memory_a = store_memory(&base, &client, &user_a, "alpha memory").await;
    let memory_b = store_memory(&base, &client, &user_b, "beta memory").await;

    let db_a = router.user_db_name(&user_a).await.expect("user A db");
    let db_b = router.user_db_name(&user_b).await.expect("user B db");
    assert_ne!(
        db_a, db_b,
        "multi-db API test must route users to different databases"
    );

    let listed_a = list_memory_ids(&base, &client, &user_a).await;
    let listed_b = list_memory_ids(&base, &client, &user_b).await;
    assert_eq!(listed_a, vec![memory_a]);
    assert_eq!(listed_b, vec![memory_b]);

    let pool_a = user_db_pool(&shared_db_url, &db_a).await;
    let pool_b = user_db_pool(&shared_db_url, &db_b).await;
    assert_eq!(count_user_memories(&pool_a, &user_a).await, 1);
    assert_eq!(count_user_memories(&pool_a, &user_b).await, 0);
    assert_eq!(count_user_memories(&pool_b, &user_b).await, 1);
    assert_eq!(count_user_memories(&pool_b, &user_a).await, 0);
}
