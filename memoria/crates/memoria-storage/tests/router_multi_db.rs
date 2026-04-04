use chrono::Utc;
use memoria_core::{interfaces::MemoryStore, Memory, MemoryType, TrustTier};
use memoria_storage::DbRouter;
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
