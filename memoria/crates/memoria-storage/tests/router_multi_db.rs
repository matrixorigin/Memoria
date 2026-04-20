use chrono::Utc;
use memoria_core::{interfaces::MemoryStore, Memory, MemoryType, TrustTier};
use memoria_storage::store::CURRENT_USER_SCHEMA_VERSION;
use memoria_storage::{DbRouter, SqlMemoryStore};
use sqlx::Row;
use uuid::Uuid;

fn test_dim() -> usize {
    std::env::var("EMBEDDING_DIM")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1024)
}

fn database_url() -> String {
    std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "mysql://root:111@localhost:6001/memoria_test".to_string())
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
    let db_name = format!(
        "memoria_router_shared_{}",
        &Uuid::new_v4().simple().to_string()[..8]
    );
    replace_db_name(&database_url(), &db_name)
}

fn make_memory(id: &str, content: &str, user_id: &str) -> Memory {
    Memory {
        memory_id: id.to_string(),
        user_id: user_id.to_string(),
        memory_type: MemoryType::Semantic,
        content: content.to_string(),
        initial_confidence: 0.8,
        embedding: Some(vec![0.1; test_dim()]),
        source_event_ids: vec!["evt-router".to_string()],
        superseded_by: None,
        is_active: true,
        access_count: 0,
        session_id: Some("router-test".to_string()),
        observed_at: Some(Utc::now()),
        created_at: None,
        updated_at: None,
        extra_metadata: None,
        trust_tier: TrustTier::T3Inferred,
        retrieval_score: None,
    }
}

#[tokio::test]
async fn router_isolates_users_into_distinct_databases() {
    let router = DbRouter::connect(&shared_db_url(), test_dim(), Uuid::new_v4().to_string())
        .await
        .expect("connect router");

    let user_a = format!("router_a_{}", Uuid::new_v4().simple());
    let user_b = format!("router_b_{}", Uuid::new_v4().simple());
    let memory_id = format!("shared-memory-{}", Uuid::new_v4().simple());

    let store_a = router.user_store(&user_a).await.expect("user A store");
    let store_b = router.user_store(&user_b).await.expect("user B store");

    store_a
        .insert(&make_memory(&memory_id, "alpha content", &user_a))
        .await
        .expect("insert user A memory");
    store_b
        .insert(&make_memory(&memory_id, "beta content", &user_b))
        .await
        .expect("insert user B memory");

    let got_a = store_a
        .get(&memory_id)
        .await
        .expect("get user A memory")
        .expect("user A memory exists");
    let got_b = store_b
        .get(&memory_id)
        .await
        .expect("get user B memory")
        .expect("user B memory exists");

    assert_eq!(got_a.user_id, user_a);
    assert_eq!(got_a.content, "alpha content");
    assert_eq!(got_b.user_id, user_b);
    assert_eq!(got_b.content, "beta content");

    let db_a = router.user_db_name(&user_a).await.expect("user A db");
    let db_b = router.user_db_name(&user_b).await.expect("user B db");
    assert_ne!(
        db_a, db_b,
        "users must route to distinct physical databases"
    );

    let registry_count: i64 =
        sqlx::query("SELECT COUNT(*) AS cnt FROM mem_user_registry WHERE user_id IN (?, ?)")
            .bind(&user_a)
            .bind(&user_b)
            .fetch_one(router.shared_pool())
            .await
            .expect("query registry")
            .try_get("cnt")
            .expect("registry count");
    assert_eq!(registry_count, 2);
}

#[tokio::test]
async fn router_persists_and_repairs_user_schema_version_marker() {
    let shared_url = shared_db_url();
    let router = DbRouter::connect(&shared_url, test_dim(), Uuid::new_v4().to_string())
        .await
        .expect("connect router");

    let user = format!("router_schema_{}", Uuid::new_v4().simple());
    router.user_store(&user).await.expect("user store");

    let db_name = router.user_db_name(&user).await.expect("user db");
    let user_db_url = replace_db_name(&shared_url, &db_name);
    let direct_pool = sqlx::mysql::MySqlPoolOptions::new()
        .max_connections(1)
        .connect(&user_db_url)
        .await
        .expect("connect user db");

    let row =
        sqlx::query("SELECT schema_version, updated_at FROM mem_schema_meta WHERE schema_key = ?")
            .bind("user_schema")
            .fetch_one(&direct_pool)
            .await
            .expect("fetch schema marker");
    let initial_version: i64 = row.try_get("schema_version").expect("initial version");
    let initial_updated_at = row
        .try_get::<chrono::NaiveDateTime, _>("updated_at")
        .expect("initial updated_at");
    assert_eq!(initial_version, CURRENT_USER_SCHEMA_VERSION);

    router.invalidate_user(&user).await;
    router.user_store(&user).await.expect("user store reentry");

    let row_after_skip =
        sqlx::query("SELECT schema_version, updated_at FROM mem_schema_meta WHERE schema_key = ?")
            .bind("user_schema")
            .fetch_one(&direct_pool)
            .await
            .expect("fetch schema marker after skip");
    let skipped_version: i64 = row_after_skip
        .try_get("schema_version")
        .expect("version after skip");
    let skipped_updated_at = row_after_skip
        .try_get::<chrono::NaiveDateTime, _>("updated_at")
        .expect("updated_at after skip");
    assert_eq!(skipped_version, CURRENT_USER_SCHEMA_VERSION);
    assert_eq!(skipped_updated_at, initial_updated_at);

    let stale_at = chrono::NaiveDate::from_ymd_opt(2000, 1, 1)
        .expect("stale date")
        .and_hms_micro_opt(0, 0, 0, 0)
        .expect("stale datetime");
    sqlx::query(
        "UPDATE mem_schema_meta SET schema_version = ?, updated_at = ? WHERE schema_key = ?",
    )
    .bind(0_i64)
    .bind(stale_at)
    .bind("user_schema")
    .execute(&direct_pool)
    .await
    .expect("set stale schema marker");

    router.invalidate_user(&user).await;
    router
        .user_store(&user)
        .await
        .expect("user store repairs version");

    let repaired_row =
        sqlx::query("SELECT schema_version, updated_at FROM mem_schema_meta WHERE schema_key = ?")
            .bind("user_schema")
            .fetch_one(&direct_pool)
            .await
            .expect("fetch repaired schema marker");
    let repaired_version: i64 = repaired_row
        .try_get("schema_version")
        .expect("repaired version");
    let repaired_updated_at = repaired_row
        .try_get::<chrono::NaiveDateTime, _>("updated_at")
        .expect("repaired updated_at");
    assert_eq!(repaired_version, CURRENT_USER_SCHEMA_VERSION);
    assert!(repaired_updated_at > stale_at);
}

#[tokio::test]
async fn shared_migrate_is_idempotent_for_async_task_user_id() {
    let shared_url = shared_db_url();
    let shared_store = SqlMemoryStore::connect(&shared_url, test_dim(), Uuid::new_v4().to_string())
        .await
        .expect("connect shared store");

    shared_store
        .migrate_shared()
        .await
        .expect("first shared migrate");
    shared_store
        .migrate_shared()
        .await
        .expect("second shared migrate");

    let user_id_column_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM information_schema.columns \
         WHERE table_schema = DATABASE() AND table_name = 'mem_async_tasks' AND column_name = 'user_id'",
    )
    .fetch_one(shared_store.pool())
    .await
    .expect("query async task user_id column count");
    assert_eq!(user_id_column_count, 1);
}
