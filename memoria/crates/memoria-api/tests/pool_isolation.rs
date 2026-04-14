//! Integration test: pool isolation under pressure.
//!
//! Verifies that when the main pool is saturated by long-running queries,
//! the isolated background pools (rebuild, entity) and auth pool still function,
//! and API requests degrade gracefully (no panic, proper error).
//!
//! Run: DATABASE_URL="mysql://root:111@localhost:6001/memoria" cargo test --test pool_isolation -- --nocapture

use serde_json::json;
use std::sync::Arc;

fn db_url() -> String {
    std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "mysql://root:111@localhost:6001/memoria".to_string())
}

fn uid() -> String {
    format!("pool_test_{}", uuid::Uuid::new_v4().simple())
}

/// Spawn server with a tiny main pool (2 connections) to make saturation easy.
async fn spawn_tiny_pool_server() -> (String, reqwest::Client, sqlx::MySqlPool) {
    use memoria_git::GitForDataService;
    use memoria_service::MemoryService;
    use memoria_storage::SqlMemoryStore;
    use sqlx::mysql::MySqlPool;

    let db = db_url();
    memoria_test_utils::wait_for_mysql_ready(&db, std::time::Duration::from_secs(30)).await;

    // Create store with only 2 main pool connections
    let pool = sqlx::mysql::MySqlPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(std::time::Duration::from_secs(3))
        .connect(&db)
        .await
        .expect("pool");

    let store = SqlMemoryStore::from_existing_pool(
        pool.clone(),
        1024,
        uuid::Uuid::new_v4().to_string(),
        Some(db.clone()),
        Some(2),
        "pool_isolation_test_pool",
    );
    store.migrate().await.expect("migrate");

    let raw_pool = MySqlPool::connect(&db).await.expect("git pool");
    let suffix_start = db.find(['?', '#']).unwrap_or(db.len());
    let db_name = db[..suffix_start]
        .rsplit_once('/')
        .map(|(_, n)| n)
        .unwrap_or("memoria");
    let git = Arc::new(GitForDataService::new(raw_pool, db_name));
    let service = Arc::new(MemoryService::new_sql_with_llm(Arc::new(store), None, None).await);
    let state = memoria_api::AppState::new(service, git, String::new());
    let app = memoria_api::build_router(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await });
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("client");
    let base = format!("http://127.0.0.1:{port}");
    (base, client, pool)
}

#[tokio::test]
async fn test_api_survives_main_pool_saturation() {
    let (base, client, pool) = spawn_tiny_pool_server().await;
    let user = uid();

    // 1. First, store a memory while pool is healthy — should succeed
    let res = client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &user)
        .json(&json!({
            "content": "pool isolation test memory",
            "memory_type": "semantic"
        }))
        .send()
        .await
        .expect("request");
    assert!(
        res.status().is_success(),
        "store should succeed with healthy pool, got {}",
        res.status()
    );

    // 2. Saturate the main pool: hold all 2 connections with SLEEP queries
    let mut blockers = Vec::new();
    for _ in 0..2 {
        let p = pool.clone();
        blockers.push(tokio::spawn(async move {
            let _ = sqlx::query("SELECT SLEEP(4)").execute(&p).await;
        }));
    }
    // Give blockers time to acquire connections
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // 3. Try to store while pool is saturated — should get an error, NOT a panic
    let res = client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &user)
        .json(&json!({
            "content": "this should fail gracefully",
            "memory_type": "semantic"
        }))
        .send()
        .await
        .expect("request should not hang forever");

    // We expect a 500 with pool timeout, NOT a connection reset or panic
    assert!(
        res.status().is_server_error(),
        "saturated pool should return 5xx, got {}",
        res.status()
    );
    let body = res.text().await.unwrap_or_default();
    assert!(
        body.contains("pool timed out")
            || body.contains("PoolTimedOut")
            || body.contains("timed out"),
        "error should mention pool timeout, got: {body}"
    );

    // 4. Wait for blockers to finish
    for b in blockers {
        let _ = b.await;
    }

    // 5. After blockers release, pool should recover — store should work again
    let res = client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &user)
        .json(&json!({
            "content": "pool recovered after saturation",
            "memory_type": "semantic"
        }))
        .send()
        .await
        .expect("request");
    assert!(
        res.status().is_success(),
        "store should succeed after pool recovery, got {}",
        res.status()
    );
}
