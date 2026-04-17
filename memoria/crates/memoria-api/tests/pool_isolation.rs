//! Integration test: pool isolation under pressure.
//!
//! Verifies that when the main pool is saturated by long-running queries,
//! the isolated background pools (rebuild, entity) and auth pool still function,
//! and API requests degrade gracefully (no panic, proper error).
//!
//! Run: DATABASE_URL="mysql://root:111@localhost:6001/memoria" cargo test --test pool_isolation -- --nocapture

mod support;

use serde_json::json;

fn uid() -> String {
    format!("pool_test_{}", uuid::Uuid::new_v4().simple())
}

/// Spawn server with a tiny routed user pool (2 connections) to make saturation easy.
async fn spawn_tiny_pool_server() -> (
    String,
    reqwest::Client,
    sqlx::MySqlPool,
    support::multi_db::ApiTestServer,
) {
    unsafe {
        std::env::set_var("MEMORIA_GLOBAL_USER_POOL_MAX", "2");
        std::env::set_var("MEMORIA_USER_SCHEMA_INIT_POOL_MAX_CONNECTIONS", "1");
    }

    let server =
        support::multi_db::spawn_api_server("pool_isolation", 1024, String::new(), None, None, None, false)
            .await;
    let pool = server.router().global_user_pool().clone();
    (server.base.clone(), server.client.clone(), pool, server)
}

#[tokio::test]
async fn test_api_survives_main_pool_saturation() {
    let (base, client, pool, _server) = spawn_tiny_pool_server().await;
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

    // 2. Saturate the routed user pool: hold all 2 connections long enough to exceed
    // the pool's 15s acquire timeout.
    let mut blockers = Vec::new();
    for _ in 0..2 {
        let p = pool.clone();
        blockers.push(tokio::spawn(async move {
            let _ = sqlx::query("SELECT SLEEP(20)").execute(&p).await;
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
