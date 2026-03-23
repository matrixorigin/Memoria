use serde_json::{json, Value};
use sqlx::MySqlPool;

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
    format!("dbv_{}", uuid::Uuid::new_v4().simple())
}

async fn spawn_server() -> (String, reqwest::Client, MySqlPool) {
    use memoria_git::GitForDataService;
    use memoria_service::{Config, MemoryService};
    use memoria_storage::SqlMemoryStore;
    use std::sync::Arc;

    let cfg = Config::from_env();
    let db = db_url();
    let store = SqlMemoryStore::connect(&db, test_dim(), uuid::Uuid::new_v4().to_string())
        .await
        .expect("connect");
    store.migrate().await.expect("migrate");
    let pool = MySqlPool::connect(&db).await.expect("pool");
    let git = Arc::new(GitForDataService::new(pool.clone(), &cfg.db_name));
    let service = Arc::new(MemoryService::new_sql_with_llm(Arc::new(store), None, None).await);
    let state = memoria_api::AppState::new(service, git, String::new());
    let app = memoria_api::build_router(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await });

    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let base = format!("http://127.0.0.1:{port}");
    wait_for_server(&client, &base, &pool).await;
    (base, client, pool)
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

fn v2_heads_table(user_id: &str) -> String {
    memoria_storage::MemoryV2TableFamily::for_user(user_id).heads_table
}

fn v2_stats_table(user_id: &str) -> String {
    memoria_storage::MemoryV2TableFamily::for_user(user_id).stats_table
}

async fn db_table_exists(pool: &MySqlPool, table_name: &str) -> bool {
    sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM information_schema.tables WHERE table_schema = DATABASE() AND table_name = ?",
    )
    .bind(table_name)
    .fetch_one(pool)
    .await
    .unwrap_or(0)
        > 0
}

async fn db_count_active(pool: &MySqlPool, user_id: &str) -> i64 {
    sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM mem_memories WHERE user_id = ? AND is_active > 0",
    )
    .bind(user_id)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn db_count_active_v2(pool: &MySqlPool, user_id: &str) -> i64 {
    let heads_table = v2_heads_table(user_id);
    if !db_table_exists(pool, &heads_table).await {
        return 0;
    }
    sqlx::query_scalar::<_, i64>(&format!(
        "SELECT COUNT(*) FROM {} WHERE forgotten_at IS NULL",
        heads_table
    ))
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn db_count_total_v2(pool: &MySqlPool, user_id: &str) -> i64 {
    let heads_table = v2_heads_table(user_id);
    if !db_table_exists(pool, &heads_table).await {
        return 0;
    }
    sqlx::query_scalar::<_, i64>(&format!("SELECT COUNT(*) FROM {}", heads_table))
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn db_sum_v1_access_count(pool: &MySqlPool, user_id: &str) -> i64 {
    sqlx::query_scalar::<_, Option<i64>>(
        "SELECT SUM(s.access_count) FROM mem_memories_stats s \
         JOIN mem_memories m ON s.memory_id = m.memory_id \
         WHERE m.user_id = ?",
    )
    .bind(user_id)
    .fetch_one(pool)
    .await
    .unwrap()
    .unwrap_or(0)
}

async fn db_sum_v2_access_count(pool: &MySqlPool, user_id: &str) -> i64 {
    let stats_table = v2_stats_table(user_id);
    if !db_table_exists(pool, &stats_table).await {
        return 0;
    }
    sqlx::query_scalar::<_, Option<i64>>(&format!("SELECT SUM(access_count) FROM {}", stats_table))
        .fetch_one(pool)
        .await
        .unwrap()
        .unwrap_or(0)
}

#[tokio::test]
async fn test_admin_delete_user_verify_db() {
    let (base, client, pool) = spawn_server().await;
    let uid = uid();
    let v2_heads = v2_heads_table(&uid);

    for i in 0..3 {
        client
            .post(format!("{base}/v1/memories"))
            .header("X-User-Id", &uid)
            .json(&json!({"content": format!("user del {i}")}))
            .send()
            .await
            .unwrap();
    }
    assert_eq!(db_count_active(&pool, &uid).await, 3);

    for i in 0..2 {
        let response = client.post(format!("{base}/v2/memory/remember"))
            .header("X-User-Id", &uid)
            .json(&json!({ "content": format!("user v2 del {i}"), "session_id": "sess-admin-delete" }))
            .send().await.unwrap();
        assert_eq!(response.status(), 201);
    }
    assert!(db_table_exists(&pool, &v2_heads).await);
    assert_eq!(db_count_active_v2(&pool, &uid).await, 2);

    let r = client
        .delete(format!("{base}/admin/users/{uid}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    assert_eq!(db_count_active(&pool, &uid).await, 0);
    assert_eq!(db_count_active_v2(&pool, &uid).await, 0);

    let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM mem_memories WHERE user_id = ?")
        .bind(&uid)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(total, 3);
    assert_eq!(db_count_total_v2(&pool, &uid).await, 2);

    let active_vals: Vec<i8> =
        sqlx::query_scalar("SELECT is_active FROM mem_memories WHERE user_id = ?")
            .bind(&uid)
            .fetch_all(&pool)
            .await
            .unwrap();
    assert!(active_vals.iter().all(|&v| v == 0));
    let forgotten_v2: i64 = sqlx::query_scalar(&format!(
        "SELECT COUNT(*) FROM {} WHERE forgotten_at IS NOT NULL AND is_active = 0",
        v2_heads
    ))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(forgotten_v2, 2);
}

#[tokio::test]
async fn test_admin_reset_access_counts_resets_v1_and_v2_verify_db() {
    let (base, client, pool) = spawn_server().await;
    let uid = uid();
    let v2_stats = v2_stats_table(&uid);

    let v1_remember = client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({"content": "admin reset v1 memory"}))
        .send()
        .await
        .unwrap();
    assert_eq!(v1_remember.status(), 201);

    let first_v2 = client
        .post(format!("{base}/v2/memory/remember"))
        .header("X-User-Id", &uid)
        .json(&json!({"content": "admin reset v2 first", "session_id": "sess-admin-reset"}))
        .send()
        .await
        .unwrap();
    assert_eq!(first_v2.status(), 201);
    let first_v2_id = first_v2.json::<Value>().await.unwrap()["memory_id"]
        .as_str()
        .unwrap()
        .to_string();

    let second_v2 = client
        .post(format!("{base}/v2/memory/remember"))
        .header("X-User-Id", &uid)
        .json(&json!({"content": "admin reset v2 second", "session_id": "sess-admin-reset"}))
        .send()
        .await
        .unwrap();
    assert_eq!(second_v2.status(), 201);
    let second_v2_id = second_v2.json::<Value>().await.unwrap()["memory_id"]
        .as_str()
        .unwrap()
        .to_string();

    let v1_search = client
        .post(format!("{base}/v1/memories/search"))
        .header("X-User-Id", &uid)
        .json(&json!({"query": "admin reset v1", "top_k": 5}))
        .send()
        .await
        .unwrap();
    assert_eq!(v1_search.status(), 200);

    let v1_memory_id = v1_remember.json::<Value>().await.unwrap()["memory_id"]
        .as_str()
        .unwrap()
        .to_string();

    sqlx::query(
        "INSERT INTO mem_memories_stats (memory_id, access_count) VALUES (?, ?) \
         ON DUPLICATE KEY UPDATE access_count = VALUES(access_count)",
    )
    .bind(&v1_memory_id)
    .bind(4_i64)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(&format!(
        "INSERT INTO {} (memory_id, access_count, last_accessed_at) VALUES (?, ?, NOW(6)), (?, ?, NOW(6)) \
         ON DUPLICATE KEY UPDATE access_count = VALUES(access_count), last_accessed_at = VALUES(last_accessed_at)",
        v2_stats
    ))
    .bind(&first_v2_id)
    .bind(2_i64)
    .bind(&second_v2_id)
    .bind(3_i64)
    .execute(&pool)
    .await
    .unwrap();

    let reset = client
        .post(format!("{base}/admin/users/{uid}/reset-access-counts"))
        .send()
        .await
        .unwrap();
    assert_eq!(reset.status(), 200);
    let reset_body: Value = reset.json().await.unwrap();
    assert_eq!(reset_body["reset"], 3);

    assert_eq!(db_sum_v1_access_count(&pool, &uid).await, 0);
    assert_eq!(db_sum_v2_access_count(&pool, &uid).await, 0);
}
