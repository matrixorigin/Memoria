/// CRUD integration tests for SqlMemoryStore against real MatrixOne.
/// Each test uses a unique user_id — safe to run in parallel (default cargo test behavior).
///
/// Run: DATABASE_URL=mysql://root:111@localhost:6001/memoria \
///      SQLX_OFFLINE=true cargo test -p memoria-storage --test store_crud -- --nocapture
use chrono::Utc;
use memoria_core::{interfaces::MemoryStore, Memory, MemoryType, TrustTier};
use memoria_storage::SqlMemoryStore;
use uuid::Uuid;

fn test_dim() -> usize {
    std::env::var("EMBEDDING_DIM")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1024)
}

/// Make a test-dim-length vector with `val` at position `idx`, rest zeros.
fn dim_vec(idx: usize, val: f32) -> Vec<f32> {
    let mut v = vec![0.0f32; test_dim()];
    v[idx] = val;
    v
}

async fn setup() -> (SqlMemoryStore, String) {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "mysql://root:111@localhost:6001/memoria".to_string());
    let store = SqlMemoryStore::connect(&url, test_dim())
        .await
        .expect("connect");
    store.migrate().await.expect("migrate");
    // Unique user_id per test — no cleanup needed, no interference between parallel tests
    let user_id = format!("test_{}", Uuid::new_v4().simple());
    (store, user_id)
}

fn make_memory(id: &str, content: &str, user_id: &str) -> Memory {
    Memory {
        memory_id: id.to_string(),
        user_id: user_id.to_string(),
        memory_type: MemoryType::Semantic,
        content: content.to_string(),
        initial_confidence: 0.8,
        embedding: Some(vec![0.1; test_dim()]),
        source_event_ids: vec!["evt-1".to_string()],
        superseded_by: None,
        is_active: true,
        access_count: 0,
        session_id: Some("sess-1".to_string()),
        observed_at: Some(Utc::now()),
        created_at: None,
        updated_at: None,
        extra_metadata: None,
        trust_tier: TrustTier::T3Inferred,
        retrieval_score: None,
    }
}

#[tokio::test]
async fn test_insert_and_get() {
    let (store, uid) = setup().await;
    let id = format!("crud-1-{uid}");
    let m = make_memory(&id, "rust is a systems programming language", &uid);
    store.insert(&m).await.expect("insert");

    let got = store.get(&id).await.expect("get").expect("should exist");
    assert_eq!(got.memory_id, id);
    assert_eq!(got.content, m.content);
    assert_eq!(got.memory_type, MemoryType::Semantic);
    assert_eq!(got.trust_tier, TrustTier::T3Inferred);
    assert!(got.is_active);
    assert_eq!(got.source_event_ids, vec!["evt-1"]);
    let emb = got.embedding.unwrap();
    assert!((emb[0] - 0.1).abs() < 1e-4);
    println!("✅ insert_and_get");
}

#[tokio::test]
async fn test_get_nonexistent() {
    let (store, _) = setup().await;
    let got = store.get("no-such-id").await.expect("get");
    assert!(got.is_none());
    println!("✅ get_nonexistent");
}

#[tokio::test]
async fn test_update() {
    let (store, uid) = setup().await;
    let id = format!("crud-2-{uid}");
    let mut m = make_memory(&id, "original content", &uid);
    store.insert(&m).await.expect("insert");

    m.content = "updated content".to_string();
    m.trust_tier = TrustTier::T2Curated;
    store.update(&m).await.expect("update");

    let got = store.get(&id).await.expect("get").unwrap();
    assert_eq!(got.content, "updated content");
    assert_eq!(got.trust_tier, TrustTier::T2Curated);
    println!("✅ update");
}

#[tokio::test]
async fn test_soft_delete() {
    let (store, uid) = setup().await;
    let id = format!("crud-3-{uid}");
    let m = make_memory(&id, "to be deleted", &uid);
    store.insert(&m).await.expect("insert");

    store.soft_delete(&id).await.expect("soft_delete");

    let got = store.get(&id).await.expect("get");
    assert!(got.is_none());
    println!("✅ soft_delete");
}

#[tokio::test]
async fn test_list_active() {
    let (store, uid) = setup().await;
    for i in 0..3 {
        let m = make_memory(
            &format!("list-{i}-{uid}"),
            &format!("memory number {i}"),
            &uid,
        );
        store.insert(&m).await.expect("insert");
    }
    store
        .soft_delete(&format!("list-1-{uid}"))
        .await
        .expect("soft_delete");

    let results = store.list_active(&uid, 10).await.expect("list_active");
    assert_eq!(results.len(), 2);
    let ids: Vec<&str> = results.iter().map(|m| m.memory_id.as_str()).collect();
    assert!(!ids.iter().any(|id| id.contains("list-1")));
    println!("✅ list_active: {} results", results.len());
}

#[tokio::test]
async fn test_search_fulltext() {
    let (store, uid) = setup().await;
    store
        .insert(&make_memory(
            &format!("ft-1-{uid}"),
            "rust programming language systems performance",
            &uid,
        ))
        .await
        .unwrap();
    store
        .insert(&make_memory(
            &format!("ft-2-{uid}"),
            "python memory service embedding vector search",
            &uid,
        ))
        .await
        .unwrap();

    let results = store
        .search_fulltext(&uid, "rust", 5)
        .await
        .expect("fulltext");
    assert!(!results.is_empty());
    assert!(results[0].content.contains("rust"));
    println!("✅ search_fulltext: top={}", results[0].memory_id);
}

#[tokio::test]
async fn test_search_vector() {
    let (store, uid) = setup().await;

    let mut m1 = make_memory(&format!("vec-1-{uid}"), "close to query", &uid);
    m1.embedding = Some(dim_vec(0, 1.0));
    let mut m2 = make_memory(&format!("vec-2-{uid}"), "far from query", &uid);
    m2.embedding = Some(dim_vec(1, 1.0));
    store.insert(&m1).await.unwrap();
    store.insert(&m2).await.unwrap();

    let query = dim_vec(0, 1.0);
    let results = store
        .search_vector(&uid, &query, 2)
        .await
        .expect("vector search");
    assert!(!results.is_empty());
    assert!(results[0].memory_id.contains("vec-1"));
    println!("✅ search_vector: nearest={}", results[0].memory_id);
}

// ── Field round-trip: every column written and read back ─────────────────────

#[tokio::test]
async fn test_all_fields_round_trip() {
    let (store, uid) = setup().await;
    let id = format!("rt-{uid}");
    let now = Utc::now();

    let mut meta = std::collections::HashMap::new();
    meta.insert("key".to_string(), serde_json::json!("value"));
    meta.insert("num".to_string(), serde_json::json!(42));

    let m = Memory {
        memory_id: id.clone(),
        user_id: uid.clone(),
        memory_type: MemoryType::Profile,
        content: "full field test".to_string(),
        initial_confidence: 0.91,
        embedding: Some(vec![0.1; test_dim()]),
        source_event_ids: vec!["e1".to_string(), "e2".to_string()],
        superseded_by: Some("other-id".to_string()),
        is_active: true,
        access_count: 0,
        session_id: Some("sess-xyz".to_string()),
        observed_at: Some(now),
        created_at: None,
        updated_at: None,
        extra_metadata: Some(meta),
        trust_tier: TrustTier::T1Verified,
        retrieval_score: None,
    };
    store.insert(&m).await.expect("insert");

    let got = store.get(&id).await.expect("get").expect("exists");

    assert_eq!(got.memory_type, MemoryType::Profile);
    assert_eq!(got.trust_tier, TrustTier::T1Verified);
    assert!(
        (got.initial_confidence - 0.91).abs() < 1e-4,
        "initial_confidence mismatch"
    );
    assert_eq!(got.session_id.as_deref(), Some("sess-xyz"));
    assert_eq!(got.superseded_by.as_deref(), Some("other-id"));
    assert_eq!(got.source_event_ids, vec!["e1", "e2"]);

    let emb = got.embedding.as_ref().expect("embedding");
    assert!((emb[0] - 0.1).abs() < 1e-4);
    assert!((emb[test_dim() - 1] - 0.1).abs() < 1e-4);

    let meta = got.extra_metadata.as_ref().expect("extra_metadata");
    assert_eq!(meta["key"], serde_json::json!("value"));
    assert_eq!(meta["num"], serde_json::json!(42));

    // observed_at round-trips within 1 second
    let got_ts = got.observed_at.expect("observed_at").timestamp();
    assert!(
        (got_ts - now.timestamp()).abs() <= 1,
        "observed_at mismatch"
    );

    // created_at is set by DB
    assert!(got.created_at.is_some(), "created_at should be set by DB");

    println!("✅ all_fields_round_trip: all columns verified");
}

// ── All MemoryType variants ───────────────────────────────────────────────────

#[tokio::test]
async fn test_all_memory_types() {
    let (store, uid) = setup().await;
    let types = [
        MemoryType::Semantic,
        MemoryType::Profile,
        MemoryType::Procedural,
        MemoryType::Working,
        MemoryType::ToolResult,
        MemoryType::Episodic,
    ];
    for mt in &types {
        let id = format!("mt-{mt:?}-{uid}");
        let mut m = make_memory(&id, &format!("type test {mt:?}"), &uid);
        m.memory_type = mt.clone();
        store.insert(&m).await.expect("insert");
        let got = store.get(&id).await.expect("get").expect("exists");
        assert_eq!(&got.memory_type, mt, "memory_type mismatch for {mt:?}");
    }
    println!("✅ all_memory_types: {} variants", types.len());
}

// ── All TrustTier variants ────────────────────────────────────────────────────

#[tokio::test]
async fn test_all_trust_tiers() {
    let (store, uid) = setup().await;
    let tiers = [
        TrustTier::T1Verified,
        TrustTier::T2Curated,
        TrustTier::T3Inferred,
        TrustTier::T4Unverified,
    ];
    for tier in &tiers {
        let id = format!("tt-{tier:?}-{uid}");
        let mut m = make_memory(&id, &format!("tier test {tier:?}"), &uid);
        m.trust_tier = tier.clone();
        store.insert(&m).await.expect("insert");
        let got = store.get(&id).await.expect("get").expect("exists");
        assert_eq!(&got.trust_tier, tier, "trust_tier mismatch for {tier:?}");
    }
    println!("✅ all_trust_tiers: {} variants", tiers.len());
}

// ── NULL optional fields ──────────────────────────────────────────────────────

#[tokio::test]
async fn test_null_optional_fields() {
    let (store, uid) = setup().await;
    let id = format!("null-{uid}");
    let m = Memory {
        memory_id: id.clone(),
        user_id: uid.clone(),
        memory_type: MemoryType::Semantic,
        content: "null fields test".to_string(),
        initial_confidence: 0.5,
        embedding: None, // NULL vecf32
        source_event_ids: vec![],
        superseded_by: None,
        is_active: true,
        access_count: 0,
        session_id: None,
        observed_at: None,
        created_at: None,
        updated_at: None,
        extra_metadata: None, // NULL JSON
        trust_tier: TrustTier::T3Inferred,
        retrieval_score: None,
    };
    store.insert(&m).await.expect("insert with nulls");

    let got = store.get(&id).await.expect("get").expect("exists");
    assert!(got.embedding.is_none(), "embedding should be NULL");
    assert!(got.session_id.is_none(), "session_id should be NULL");
    assert!(got.superseded_by.is_none(), "superseded_by should be NULL");
    assert!(
        got.extra_metadata.is_none(),
        "extra_metadata should be NULL"
    );
    assert_eq!(got.source_event_ids, Vec::<String>::new());
    println!("✅ null_optional_fields: all NULLs round-trip correctly");
}

// ── insert_entity_links batch optimization tests ─────────────────────────────

#[tokio::test]
async fn test_insert_entity_links_empty() {
    let (store, uid) = setup().await;
    let mid = Uuid::new_v4().simple().to_string();
    let (created, reused) = store.insert_entity_links(&uid, &mid, &[]).await.unwrap();
    assert_eq!(created, 0);
    assert_eq!(reused, 0);
    println!("✅ insert_entity_links: empty input returns (0, 0)");
}

#[tokio::test]
async fn test_insert_entity_links_batch() {
    let (store, uid) = setup().await;
    let mid = Uuid::new_v4().simple().to_string();
    let entities = vec![
        ("Rust".to_string(), "tech".to_string()),
        ("MatrixOne".to_string(), "tech".to_string()),
        ("Memoria".to_string(), "project".to_string()),
    ];
    let (created, reused) = store.insert_entity_links(&uid, &mid, &entities).await.unwrap();
    assert_eq!(created, 3);
    assert_eq!(reused, 0);

    // Verify all 3 exist in DB
    let rows = sqlx::query("SELECT entity_name FROM mem_entity_links WHERE user_id = ? AND memory_id = ? ORDER BY entity_name")
        .bind(&uid).bind(&mid)
        .fetch_all(store.pool()).await.unwrap();
    let names: Vec<String> = rows.iter().map(|r| sqlx::Row::get::<String, _>(r, "entity_name")).collect();
    assert_eq!(names, vec!["matrixone", "memoria", "rust"]);
    println!("✅ insert_entity_links: batch of 3 inserted in single statement");
}

#[tokio::test]
async fn test_insert_entity_links_dedup_existing() {
    let (store, uid) = setup().await;
    let mid = Uuid::new_v4().simple().to_string();
    let entities = vec![
        ("Rust".to_string(), "tech".to_string()),
        ("Go".to_string(), "tech".to_string()),
    ];
    let (created, _) = store.insert_entity_links(&uid, &mid, &entities).await.unwrap();
    assert_eq!(created, 2);

    // Insert again with overlap + new
    let entities2 = vec![
        ("Rust".to_string(), "tech".to_string()),   // existing
        ("Python".to_string(), "tech".to_string()),  // new
    ];
    let (created2, reused2) = store.insert_entity_links(&uid, &mid, &entities2).await.unwrap();
    assert_eq!(created2, 1, "only Python should be new");
    assert_eq!(reused2, 1, "Rust should be reused");

    let rows = sqlx::query("SELECT COUNT(*) as cnt FROM mem_entity_links WHERE user_id = ? AND memory_id = ?")
        .bind(&uid).bind(&mid)
        .fetch_one(store.pool()).await.unwrap();
    let cnt: i64 = sqlx::Row::get(&rows, "cnt");
    assert_eq!(cnt, 3, "total should be 3 (rust, go, python)");
    println!("✅ insert_entity_links: dedup existing entities correctly");
}

#[tokio::test]
async fn test_insert_entity_links_dedup_within_batch() {
    let (store, uid) = setup().await;
    let mid = Uuid::new_v4().simple().to_string();
    // Same entity name twice in one batch (different case)
    let entities = vec![
        ("Rust".to_string(), "tech".to_string()),
        ("rust".to_string(), "tech".to_string()),
        ("RUST".to_string(), "tech".to_string()),
    ];
    let (created, reused) = store.insert_entity_links(&uid, &mid, &entities).await.unwrap();
    assert_eq!(created, 1, "only one 'rust' should be inserted");
    assert_eq!(reused, 2, "two duplicates within batch");
    println!("✅ insert_entity_links: dedup within batch (case-insensitive)");
}

#[tokio::test]
async fn test_insert_entity_links_case_insensitive() {
    let (store, uid) = setup().await;
    let mid = Uuid::new_v4().simple().to_string();
    let entities = vec![("MatrixOne".to_string(), "tech".to_string())];
    store.insert_entity_links(&uid, &mid, &entities).await.unwrap();

    // Query back — should be lowercased
    let rows = sqlx::query("SELECT entity_name FROM mem_entity_links WHERE user_id = ? AND memory_id = ?")
        .bind(&uid).bind(&mid)
        .fetch_all(store.pool()).await.unwrap();
    let name: String = sqlx::Row::get(&rows[0], "entity_name");
    assert_eq!(name, "matrixone", "entity name should be lowercased");
    println!("✅ insert_entity_links: names stored as lowercase");
}

#[tokio::test]
async fn test_insert_entity_links_large_batch_chunking() {
    let (store, uid) = setup().await;
    let mid = Uuid::new_v4().simple().to_string();
    // 120 entities — should be split into 3 chunks (50+50+20)
    let entities: Vec<(String, String)> = (0..120)
        .map(|i| (format!("entity_{i}"), "concept".to_string()))
        .collect();
    let (created, reused) = store.insert_entity_links(&uid, &mid, &entities).await.unwrap();
    assert_eq!(created, 120);
    assert_eq!(reused, 0);

    let rows = sqlx::query("SELECT COUNT(*) as cnt FROM mem_entity_links WHERE user_id = ? AND memory_id = ?")
        .bind(&uid).bind(&mid)
        .fetch_one(store.pool()).await.unwrap();
    let cnt: i64 = sqlx::Row::get(&rows, "cnt");
    assert_eq!(cnt, 120);
    println!("✅ insert_entity_links: 120 entities chunked correctly");
}
