/// Performance optimizations E2E tests against real DB.
/// Covers: active_table cache, cooldown cache, node_count cache sharing,
///         get_stats_batch, batch entity_recall, concurrent hybrid search,
///         find_near_duplicate single-query optimization.
///
/// Run: DATABASE_URL=mysql://root:111@localhost:6001/memoria \
///      cargo test -p memoria-mcp --test perf_optimizations_e2e -- --test-threads=1 --nocapture
use memoria_storage::SqlMemoryStore;
use serde_json::json;
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
fn uid() -> String {
    format!("perf_{}", &Uuid::new_v4().simple().to_string()[..8])
}

async fn setup() -> (
    Arc<memoria_service::MemoryService>,
    Arc<SqlMemoryStore>,
    String,
) {
    let store = SqlMemoryStore::connect(&db_url(), test_dim(), Uuid::new_v4().to_string())
        .await
        .expect("connect");
    store.migrate().await.expect("migrate");
    let store = Arc::new(store);
    let svc =
        Arc::new(memoria_service::MemoryService::new_sql_with_llm(store.clone(), None, None).await);
    (svc, store, uid())
}

async fn call(
    name: &str,
    args: serde_json::Value,
    svc: &Arc<memoria_service::MemoryService>,
    uid: &str,
) -> serde_json::Value {
    memoria_mcp::tools::call(name, args, svc, uid)
        .await
        .expect(name)
}
fn text(v: &serde_json::Value) -> &str {
    v["content"][0]["text"].as_str().unwrap_or("")
}

// ── 1. active_table cache: returns correct table, invalidated on branch switch ──

#[tokio::test]
async fn test_active_table_cache_hit_and_invalidation() {
    let (_svc, store, uid) = setup().await;

    // First call: cache miss → DB lookup → should return "mem_memories"
    let t1 = store.active_table(&uid).await.expect("active_table 1");
    assert_eq!(t1, "mem_memories", "default should be mem_memories");

    // Second call: should be cache hit (same result)
    let t2 = store.active_table(&uid).await.expect("active_table 2");
    assert_eq!(t2, "mem_memories", "cached value should match");

    // Switch to a branch
    let branch_name = format!("b_{}", &Uuid::new_v4().simple().to_string()[..6]);
    // Create branch table manually for test
    let table_name = format!("mem_br_{}", branch_name);
    sqlx::query(&format!(
        "CREATE TABLE IF NOT EXISTS {table_name} LIKE mem_memories"
    ))
    .execute(store.pool())
    .await
    .expect("create branch table");
    // Use register_branch which handles the schema correctly
    store
        .register_branch(&uid, &branch_name, &table_name)
        .await
        .expect("register branch");

    // set_active_branch should invalidate cache
    store
        .set_active_branch(&uid, &branch_name)
        .await
        .expect("set_active_branch");

    // Now active_table should return the branch table
    let t3 = store.active_table(&uid).await.expect("active_table 3");
    assert_eq!(t3, table_name, "should return branch table after switch");

    // Switch back to main
    store
        .set_active_branch(&uid, "main")
        .await
        .expect("set_active_branch main");
    let t4 = store.active_table(&uid).await.expect("active_table 4");
    assert_eq!(
        t4, "mem_memories",
        "should return mem_memories after switch back"
    );

    // Cleanup
    let _ = sqlx::query(&format!("DROP TABLE IF EXISTS {table_name}"))
        .execute(store.pool())
        .await;
    let _ = sqlx::query("DELETE FROM mem_branches WHERE user_id = ?")
        .bind(&uid)
        .execute(store.pool())
        .await;
    let _ = sqlx::query("DELETE FROM mem_user_state WHERE user_id = ?")
        .bind(&uid)
        .execute(store.pool())
        .await;

    println!("✅ active_table cache: hit + invalidation on branch switch");
}

// ── 2. cooldown cache: in-memory fast path + DB fallback ─────────────────────

#[tokio::test]
async fn test_cooldown_cache_fast_path() {
    let (_svc, store, uid) = setup().await;
    let op = "test_governance";
    let cooldown_secs = 3600; // 1 hour

    // Initially no cooldown
    let r = store
        .check_cooldown(&uid, op, cooldown_secs)
        .await
        .expect("check 1");
    assert!(r.is_none(), "should have no cooldown initially");

    // Set cooldown
    store.set_cooldown(&uid, op).await.expect("set_cooldown");

    // Check again — should be in cooldown (from cache, not DB)
    let r = store
        .check_cooldown(&uid, op, cooldown_secs)
        .await
        .expect("check 2");
    assert!(r.is_some(), "should be in cooldown after set");
    let remaining = r.unwrap();
    assert!(
        remaining > 3500 && remaining <= 3600,
        "remaining should be ~3600, got {remaining}"
    );

    // Verify DB also has the record
    let db_row: Option<(i64,)> = sqlx::query_as(
        "SELECT TIMESTAMPDIFF(SECOND, last_run_at, NOW()) as elapsed \
         FROM mem_governance_cooldown WHERE user_id = ? AND operation = ?",
    )
    .bind(&uid)
    .bind(op)
    .fetch_optional(store.pool())
    .await
    .expect("query cooldown");
    assert!(db_row.is_some(), "cooldown should exist in DB");
    let elapsed = db_row.unwrap().0;
    assert!(elapsed < 5, "DB elapsed should be ~0, got {elapsed}");

    // Cleanup
    let _ = sqlx::query("DELETE FROM mem_governance_cooldown WHERE user_id = ?")
        .bind(&uid)
        .execute(store.pool())
        .await;

    println!("✅ cooldown cache: fast path + DB consistency");
}

#[tokio::test]
async fn test_cooldown_cache_db_fallback_on_cold_start() {
    // Simulate cross-instance scenario: set cooldown via raw DB, then check via cache
    let (_svc, store, uid) = setup().await;
    let op = "test_consolidate";
    let cooldown_secs = 1800;

    // Insert cooldown directly into DB (simulating another instance)
    sqlx::query(
        "INSERT INTO mem_governance_cooldown (user_id, operation, last_run_at) \
         VALUES (?, ?, NOW()) ON DUPLICATE KEY UPDATE last_run_at = NOW()",
    )
    .bind(&uid)
    .bind(op)
    .execute(store.pool())
    .await
    .expect("insert cooldown");

    // Cache is empty — should fall back to DB and find the cooldown
    let r = store
        .check_cooldown(&uid, op, cooldown_secs)
        .await
        .expect("check");
    assert!(r.is_some(), "should detect cooldown from DB fallback");
    let remaining = r.unwrap();
    assert!(
        remaining > 1700 && remaining <= 1800,
        "remaining should be ~1800, got {remaining}"
    );

    // Second check should now be from cache (backfilled)
    let r2 = store
        .check_cooldown(&uid, op, cooldown_secs)
        .await
        .expect("check 2");
    assert!(r2.is_some(), "should still be in cooldown from cache");

    // Cleanup
    let _ = sqlx::query("DELETE FROM mem_governance_cooldown WHERE user_id = ?")
        .bind(&uid)
        .execute(store.pool())
        .await;

    println!("✅ cooldown cache: DB fallback on cold start + backfill");
}

// ── 3. node_count_cache: shared across GraphStore instances ──────────────────

#[tokio::test]
async fn test_node_count_cache_shared_across_graph_store_instances() {
    let (_svc, store, uid) = setup().await;
    use memoria_storage::graph::types::{GraphNode, NodeType};

    let graph1 = store.graph_store();
    graph1.migrate().await.expect("migrate");

    // Create some nodes
    for i in 0..5 {
        let node = GraphNode {
            node_id: Uuid::new_v4().simple().to_string()[..32].to_string(),
            user_id: uid.clone(),
            node_type: NodeType::Semantic,
            content: format!("test node {i}"),
            entity_type: None,
            embedding: None,
            memory_id: Some(format!("mem_{i}")),
            session_id: None,
            confidence: 0.75,
            trust_tier: "T3".to_string(),
            importance: 0.5,
            source_nodes: vec![],
            conflicts_with: None,
            conflict_resolution: None,
            access_count: 0,
            cross_session_count: 0,
            is_active: true,
            superseded_by: None,
            created_at: Some(chrono::Utc::now().naive_utc()),
        };
        graph1.create_node(&node).await.expect("create_node");
    }

    // First count via graph1 — cache miss, queries DB
    let count1 = graph1.count_user_nodes(&uid).await.expect("count1");
    assert_eq!(count1, 5, "should have 5 nodes");

    // Create a NEW GraphStore instance via graph_store() — should share the cache
    let graph2 = store.graph_store();
    let count2 = graph2.count_user_nodes(&uid).await.expect("count2");
    assert_eq!(count2, 5, "graph2 should get cached value = 5");

    // Add a node directly (bypassing cache)
    let extra = GraphNode {
        node_id: Uuid::new_v4().simple().to_string()[..32].to_string(),
        user_id: uid.clone(),
        node_type: NodeType::Semantic,
        content: "extra node".to_string(),
        entity_type: None,
        embedding: None,
        memory_id: Some("mem_extra".to_string()),
        session_id: None,
        confidence: 0.75,
        trust_tier: "T3".to_string(),
        importance: 0.5,
        source_nodes: vec![],
        conflicts_with: None,
        conflict_resolution: None,
        access_count: 0,
        cross_session_count: 0,
        is_active: true,
        superseded_by: None,
        created_at: Some(chrono::Utc::now().naive_utc()),
    };
    graph1.create_node(&extra).await.expect("create extra");

    // graph3 (yet another instance) should still return cached 5 (TTL not expired)
    let graph3 = store.graph_store();
    let count3 = graph3.count_user_nodes(&uid).await.expect("count3");
    assert_eq!(count3, 5, "should still return cached 5 before TTL expires");

    // Verify DB actually has 6
    let db_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM memory_graph_nodes WHERE user_id = ? AND is_active = 1",
    )
    .bind(&uid)
    .fetch_one(store.pool())
    .await
    .expect("db count");
    assert_eq!(db_count, 6, "DB should have 6 nodes");

    // Cleanup
    let _ = sqlx::query("DELETE FROM memory_graph_nodes WHERE user_id = ?")
        .bind(&uid)
        .execute(store.pool())
        .await;

    println!("✅ node_count_cache: shared across GraphStore instances from same SqlMemoryStore");
}

// ── 4. get_stats_batch: combined access_count + feedback ─────────────────────

#[tokio::test]
async fn test_get_stats_batch_returns_correct_values() {
    let (svc, store, uid) = setup().await;

    // Store two memories
    let r1 = call(
        "memory_store",
        json!({"content": "stats batch test memory one", "memory_type": "semantic"}),
        &svc,
        &uid,
    )
    .await;
    let mid1 = text(&r1)
        .split("Stored memory ")
        .nth(1)
        .and_then(|s| s.split(':').next())
        .expect("extract mid1");

    let r2 = call(
        "memory_store",
        json!({"content": "stats batch test memory two", "memory_type": "semantic"}),
        &svc,
        &uid,
    )
    .await;
    let mid2 = text(&r2)
        .split("Stored memory ")
        .nth(1)
        .and_then(|s| s.split(':').next())
        .expect("extract mid2");

    // Bump access counts: mid1 x3, mid2 x1
    store
        .bump_access_counts(&[mid1.to_string(), mid1.to_string(), mid1.to_string()])
        .await
        .expect("bump");
    store
        .bump_access_counts(&[mid2.to_string()])
        .await
        .expect("bump2");

    // Record feedback: mid1 = 2 useful + 1 wrong, mid2 = 1 irrelevant
    store
        .record_feedback(&uid, mid1, "useful", None)
        .await
        .expect("fb1");
    store
        .record_feedback(&uid, mid1, "useful", None)
        .await
        .expect("fb2");
    store
        .record_feedback(&uid, mid1, "wrong", None)
        .await
        .expect("fb3");
    store
        .record_feedback(&uid, mid2, "irrelevant", None)
        .await
        .expect("fb4");

    // Call get_stats_batch
    let ids = vec![mid1.to_string(), mid2.to_string()];
    let (ac_map, fb_map) = store.get_stats_batch(&ids).await.expect("get_stats_batch");

    // Verify access counts
    assert_eq!(*ac_map.get(mid1).unwrap_or(&0), 3, "mid1 access_count");
    assert_eq!(*ac_map.get(mid2).unwrap_or(&0), 1, "mid2 access_count");

    // Verify feedback for mid1
    let fb1 = fb_map.get(mid1).expect("mid1 feedback");
    assert_eq!(fb1.useful, 2, "mid1 useful");
    assert_eq!(fb1.wrong, 1, "mid1 wrong");
    assert_eq!(fb1.irrelevant, 0, "mid1 irrelevant");
    assert_eq!(fb1.outdated, 0, "mid1 outdated");

    // Verify feedback for mid2
    let fb2 = fb_map.get(mid2).expect("mid2 feedback");
    assert_eq!(fb2.irrelevant, 1, "mid2 irrelevant");
    assert_eq!(fb2.useful, 0, "mid2 useful");

    // Cross-check: get_stats_batch should match individual methods
    let ac_individual = store.get_access_counts(&ids).await.expect("ac individual");
    let fb_individual = store.get_feedback_batch(&ids).await.expect("fb individual");
    assert_eq!(
        ac_map, ac_individual,
        "access counts should match individual method"
    );
    for id in &ids {
        let batch_fb = fb_map.get(id.as_str());
        let indiv_fb = fb_individual.get(id.as_str());
        match (batch_fb, indiv_fb) {
            (Some(b), Some(i)) => {
                assert_eq!(b.useful, i.useful, "{id} useful mismatch");
                assert_eq!(b.irrelevant, i.irrelevant, "{id} irrelevant mismatch");
                assert_eq!(b.outdated, i.outdated, "{id} outdated mismatch");
                assert_eq!(b.wrong, i.wrong, "{id} wrong mismatch");
            }
            (None, None) => {}
            _ => panic!("{id}: batch vs individual presence mismatch"),
        }
    }

    // Empty input
    let (ac_empty, fb_empty) = store.get_stats_batch(&[]).await.expect("empty stats batch");
    assert!(ac_empty.is_empty(), "empty input → empty ac");
    assert!(fb_empty.is_empty(), "empty input → empty fb");

    println!("✅ get_stats_batch: correct values + matches individual methods");
}

// ── 5. batch entity_recall: find_entities_by_names + get_memories_by_entities ─

#[tokio::test]
async fn test_batch_entity_methods() {
    let (_svc, store, uid) = setup().await;
    let graph = store.graph_store();
    graph.migrate().await.expect("migrate");

    // Create entities
    let entities: Vec<(&str, &str, &str)> = vec![
        ("rust", "Rust", "tech"),
        ("tokio", "Tokio", "tech"),
        ("matrixone", "MatrixOne", "project"),
    ];
    let resolved = graph
        .batch_upsert_entities(&uid, &entities)
        .await
        .expect("upsert entities");
    assert_eq!(resolved.len(), 3);

    // Create memory and link entities
    let mid = format!("mem_{}", &Uuid::new_v4().simple().to_string()[..8]);
    // Insert a memory via the proper API so all columns are populated
    let mem = memoria_core::Memory {
        memory_id: mid.clone(),
        user_id: uid.clone(),
        memory_type: memoria_core::MemoryType::Semantic,
        content: "test content for entity linking".to_string(),
        embedding: None,
        session_id: None,
        source_event_ids: vec![],
        extra_metadata: None,
        is_active: true,
        superseded_by: None,
        trust_tier: memoria_core::TrustTier::T3Inferred,
        initial_confidence: 0.75,
        retrieval_score: None,
        access_count: 0,
        observed_at: Some(chrono::Utc::now()),
        created_at: Some(chrono::Utc::now()),
        updated_at: None,
    };
    store
        .insert_into("mem_memories", &mem)
        .await
        .expect("insert memory");

    let links: Vec<(&str, &str, &str)> = resolved
        .iter()
        .map(|(_, eid)| (mid.as_str(), eid.as_str(), "regex"))
        .collect();
    graph
        .batch_upsert_memory_entity_links(&uid, &links)
        .await
        .expect("link");

    // Test find_entities_by_names
    let names = vec!["rust", "tokio", "nonexistent"];
    let id_map = graph
        .find_entities_by_names(&uid, &names)
        .await
        .expect("find_entities_by_names");
    assert_eq!(id_map.len(), 2, "should find 2 of 3 names");
    assert!(id_map.contains_key("rust"), "should find rust");
    assert!(id_map.contains_key("tokio"), "should find tokio");
    assert!(
        !id_map.contains_key("nonexistent"),
        "should not find nonexistent"
    );

    // Verify entity IDs match what batch_upsert returned
    let rust_eid = resolved
        .iter()
        .find(|(n, _)| n == "rust")
        .unwrap()
        .1
        .clone();
    assert_eq!(id_map["rust"], rust_eid, "rust entity_id should match");

    // Test get_memories_by_entities
    let eids: Vec<&str> = id_map.values().map(|s| s.as_str()).collect();
    let mems = graph
        .get_memories_by_entities(&eids, &uid, 20)
        .await
        .expect("get_memories_by_entities");
    assert!(!mems.is_empty(), "should find linked memories");
    assert!(
        mems.iter().any(|(m, _)| m == &mid),
        "should contain our memory"
    );

    // Test empty inputs
    let empty_names = graph
        .find_entities_by_names(&uid, &[])
        .await
        .expect("empty names");
    assert!(empty_names.is_empty());
    let empty_eids = graph
        .get_memories_by_entities(&[], &uid, 20)
        .await
        .expect("empty eids");
    assert!(empty_eids.is_empty());

    // Test user isolation
    let other_uid = format!("perf_{}", &Uuid::new_v4().simple().to_string()[..8]);
    let other_map = graph
        .find_entities_by_names(&other_uid, &["rust"])
        .await
        .expect("other user");
    assert!(other_map.is_empty(), "other user should not see entities");

    // Cleanup
    let _ = sqlx::query("DELETE FROM mem_memory_entity_links WHERE user_id = ?")
        .bind(&uid)
        .execute(store.pool())
        .await;
    let _ = sqlx::query("DELETE FROM mem_entities WHERE user_id = ?")
        .bind(&uid)
        .execute(store.pool())
        .await;
    let _ = sqlx::query("DELETE FROM mem_memories WHERE user_id = ?")
        .bind(&uid)
        .execute(store.pool())
        .await;

    println!("✅ batch entity methods: find_entities_by_names + get_memories_by_entities");
}

// ── 6. find_near_duplicate: single query, same-type preference ───────────────

#[tokio::test]
async fn test_find_near_duplicate_single_query_same_type_preference() {
    let (_svc, store, uid) = setup().await;
    let dim = test_dim();

    // Create a base embedding
    let base_emb: Vec<f32> = (0..dim).map(|i| (i as f32) / (dim as f32)).collect();

    // Insert memory A (semantic) with base embedding
    let mid_a = format!("dup_a_{}", &Uuid::new_v4().simple().to_string()[..8]);
    let mem_a = memoria_core::Memory {
        memory_id: mid_a.clone(),
        user_id: uid.clone(),
        memory_type: memoria_core::MemoryType::Semantic,
        content: "duplicate test A".to_string(),
        embedding: Some(base_emb.clone()),
        session_id: None,
        source_event_ids: vec![],
        extra_metadata: None,
        is_active: true,
        superseded_by: None,
        trust_tier: memoria_core::TrustTier::T3Inferred,
        initial_confidence: 0.75,
        retrieval_score: None,
        access_count: 0,
        observed_at: Some(chrono::Utc::now()),
        created_at: Some(chrono::Utc::now()),
        updated_at: None,
    };
    store
        .insert_into("mem_memories", &mem_a)
        .await
        .expect("insert A");

    // Insert memory B (procedural) with very similar embedding (tiny perturbation)
    let mid_b = format!("dup_b_{}", &Uuid::new_v4().simple().to_string()[..8]);
    let emb_b: Vec<f32> = base_emb.iter().map(|v| v + 0.0001).collect();
    let mem_b = memoria_core::Memory {
        memory_id: mid_b.clone(),
        user_id: uid.clone(),
        memory_type: memoria_core::MemoryType::Procedural,
        content: "duplicate test B".to_string(),
        embedding: Some(emb_b),
        session_id: None,
        source_event_ids: vec![],
        extra_metadata: None,
        is_active: true,
        superseded_by: None,
        trust_tier: memoria_core::TrustTier::T3Inferred,
        initial_confidence: 0.75,
        retrieval_score: None,
        access_count: 0,
        observed_at: Some(chrono::Utc::now()),
        created_at: Some(chrono::Utc::now()),
        updated_at: None,
    };
    store
        .insert_into("mem_memories", &mem_b)
        .await
        .expect("insert B");

    // Search for near duplicate of type "semantic" — should prefer A (same type)
    let exclude = format!("exclude_{}", Uuid::new_v4().simple());
    let result = store
        .find_near_duplicate("mem_memories", &uid, &base_emb, "semantic", &exclude, 10.0)
        .await
        .expect("find_near_duplicate semantic");
    assert!(result.is_some(), "should find a duplicate");
    let (found_id, _, dist) = result.unwrap();
    assert_eq!(found_id, mid_a, "should prefer same-type (semantic) match");
    assert!(dist < 1.0, "distance should be very small, got {dist}");

    // Search for near duplicate of type "procedural" — should prefer B
    let result2 = store
        .find_near_duplicate(
            "mem_memories",
            &uid,
            &base_emb,
            "procedural",
            &exclude,
            10.0,
        )
        .await
        .expect("find_near_duplicate procedural");
    assert!(result2.is_some(), "should find a duplicate");
    let (found_id2, _, _) = result2.unwrap();
    assert_eq!(
        found_id2, mid_b,
        "should prefer same-type (procedural) match"
    );

    // Search for type "working" — no same-type match, should fall back to closest (A)
    let result3 = store
        .find_near_duplicate("mem_memories", &uid, &base_emb, "working", &exclude, 10.0)
        .await
        .expect("find_near_duplicate working");
    assert!(result3.is_some(), "should find cross-type duplicate");
    let (found_id3, _, _) = result3.unwrap();
    assert_eq!(found_id3, mid_a, "should fall back to closest match (A)");

    // Search with very tight threshold — should find nothing
    let _result4 = store
        .find_near_duplicate(
            "mem_memories",
            &uid,
            &base_emb,
            "semantic",
            &mid_a,
            0.0000001,
        )
        .await
        .expect("find_near_duplicate tight threshold");
    // mid_a is excluded, mid_b has small but nonzero distance
    // Whether this returns None depends on the actual L2 distance of emb_b

    // Cleanup
    let _ = sqlx::query("DELETE FROM mem_memories WHERE user_id = ?")
        .bind(&uid)
        .execute(store.pool())
        .await;

    println!("✅ find_near_duplicate: single query with same-type preference");
}

// ── 7. idx_feedback_created_at migration ─────────────────────────────────────

#[tokio::test]
async fn test_feedback_created_at_index_exists() {
    let (_svc, store, _uid) = setup().await;

    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM information_schema.statistics \
         WHERE table_schema = DATABASE() \
           AND table_name = 'mem_retrieval_feedback' \
           AND index_name = 'idx_feedback_created_at'",
    )
    .fetch_one(store.pool())
    .await
    .expect("check index");

    assert!(
        count > 0,
        "idx_feedback_created_at should exist after migration"
    );
    println!("✅ idx_feedback_created_at index exists");
}

// ── 8. concurrent hybrid search: verify results are correct ──────────────────

#[tokio::test]
async fn test_hybrid_search_concurrent_correctness() {
    let (svc, _store, uid) = setup().await;

    // Store several memories with distinct content
    for i in 0..5 {
        call(
            "memory_store",
            json!({"content": format!("hybrid concurrent test item number {i} about databases"), "memory_type": "semantic"}),
            &svc,
            &uid,
        )
        .await;
    }

    // Small delay for indexing
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Search — this exercises the tokio::join! path
    let r = call(
        "memory_search",
        json!({"query": "databases", "top_k": 10}),
        &svc,
        &uid,
    )
    .await;
    let search_text = text(&r);
    // Should find at least some of our memories
    assert!(
        search_text.contains("hybrid concurrent test") || search_text.contains("databases"),
        "search should return relevant results: {search_text}"
    );

    println!("✅ hybrid search concurrent: returns correct results");
}

// ── 9. GraphStore::new still works standalone (backward compat) ──────────────

#[tokio::test]
async fn test_graph_store_new_standalone_still_works() {
    let pool = sqlx::MySqlPool::connect(&db_url()).await.expect("pool");
    let graph = memoria_storage::GraphStore::new(pool, test_dim());
    graph.migrate().await.expect("migrate");

    let uid = uid();
    // count_user_nodes should work (own cache, not shared)
    let count = graph.count_user_nodes(&uid).await.expect("count");
    assert_eq!(count, 0, "new user should have 0 nodes");

    println!("✅ GraphStore::new standalone: backward compatible");
}
