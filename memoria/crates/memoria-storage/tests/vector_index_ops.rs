use memoria_storage::SqlMemoryStore;

fn test_dim() -> usize {
    std::env::var("EMBEDDING_DIM")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1024)
}

async fn setup() -> SqlMemoryStore {
    let database_url = std::env::var("TEST_DATABASE_URL")
        .unwrap_or_else(|_| "mysql://root:111@localhost:6001/memoria_test".to_string());
    let instance_id = uuid::Uuid::new_v4().to_string();
    let store = SqlMemoryStore::connect(&database_url, test_dim(), instance_id)
        .await
        .expect("Failed to connect");
    store.migrate().await.expect("Failed to migrate");
    store
}

#[tokio::test]
async fn test_cleanup_orphan_stats() {
    let store = setup().await;

    // 插入正常 memory + stats
    let memory_id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO mem_memories (memory_id, user_id, memory_type, content, is_active, initial_confidence, source_event_ids, observed_at, created_at, updated_at) \
         VALUES (?, 'test_user', 'semantic', 'test', 1, 0.9, '[]', NOW(), NOW(), NOW())"
    )
    .bind(&memory_id)
    .execute(store.pool())
    .await
    .expect("Insert memory");

    sqlx::query("INSERT INTO mem_memories_stats (memory_id, access_count) VALUES (?, 10)")
        .bind(&memory_id)
        .execute(store.pool())
        .await
        .expect("Insert stats");

    // 插入孤儿 stats（没有对应 memory）
    let orphan_id = uuid::Uuid::new_v4().to_string();
    sqlx::query("INSERT INTO mem_memories_stats (memory_id, access_count) VALUES (?, 5)")
        .bind(&orphan_id)
        .execute(store.pool())
        .await
        .expect("Insert orphan stats");

    // 清理
    let cleaned = store.cleanup_orphan_stats().await.expect("Cleanup");
    assert_eq!(cleaned, 1, "Should clean 1 orphan");

    // 验证正常记录还在
    let (count,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM mem_memories_stats WHERE memory_id = ?")
            .bind(&memory_id)
            .fetch_one(store.pool())
            .await
            .expect("Query stats");
    assert_eq!(count, 1, "Normal stats should remain");

    // 验证孤儿记录已删除
    let (orphan_count,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM mem_memories_stats WHERE memory_id = ?")
            .bind(&orphan_id)
            .fetch_one(store.pool())
            .await
            .expect("Query orphan");
    assert_eq!(orphan_count, 0, "Orphan should be deleted");
}

#[tokio::test]
async fn test_should_rebuild_vector_index() {
    let store = setup().await;
    // 使用唯一的 key 避免并发冲突
    let test_id = uuid::Uuid::new_v4().to_string();
    let table = format!("test_table_{}", &test_id[..8]);

    // 清理之前的状态
    let key = format!("vector_index_rebuild:{}", table);
    sqlx::query("DELETE FROM mem_governance_runtime_state WHERE strategy_key = ?")
        .bind(&key)
        .execute(store.pool())
        .await
        .ok();

    // 模拟有数据的场景：直接记录一个初始状态
    store
        .record_vector_index_rebuild(&table, 0, 0)
        .await
        .expect("Record initial state");

    // 首次检查（有历史记录，但行数为0）
    let (should, rows, _cooldown) = store
        .should_rebuild_vector_index(&table)
        .await
        .expect("Check rebuild");

    // 行数为0时不应该重建
    assert!(!should, "Should not rebuild with 0 rows");
    assert_eq!(rows, 0, "Should count 0 rows for non-existent table");

    // 模拟数据增长到1000行
    store
        .record_vector_index_rebuild(&table, 1000, 3600)
        .await
        .expect("Record rebuild with 1000 rows");

    // 立即检查：应该在冷却期
    let (should2, _, cooldown2) = store
        .should_rebuild_vector_index(&table)
        .await
        .expect("Check rebuild again");

    assert!(!should2, "Should not rebuild during cooldown");
    assert!(cooldown2.is_some(), "Should have cooldown");
    assert!(
        cooldown2.unwrap() > 0 && cooldown2.unwrap() <= 3600,
        "Cooldown should be within range"
    );
}

#[tokio::test]
async fn test_distributed_lock() {
    let store1 = setup().await;
    let store2 = setup().await;

    let lock_key = format!("test_lock_{}", uuid::Uuid::new_v4());

    // store1 获取锁
    let acquired1 = store1
        .try_acquire_lock(&lock_key, 60)
        .await
        .expect("Acquire lock");
    assert!(acquired1, "First should acquire");

    // store2 尝试获取同一个锁
    let acquired2 = store2
        .try_acquire_lock(&lock_key, 60)
        .await
        .expect("Try acquire");
    assert!(!acquired2, "Second should fail");

    // store1 释放锁
    store1.release_lock(&lock_key).await.expect("Release lock");

    // store2 现在可以获取
    let acquired3 = store2
        .try_acquire_lock(&lock_key, 60)
        .await
        .expect("Acquire after release");
    assert!(acquired3, "Should acquire after release");
}

#[tokio::test]
async fn test_rebuild_vector_index_adaptive_cooldown() {
    let store = setup().await;
    let table = "mem_memories";

    // 测试不同数据量的冷却时间
    let test_cases = vec![
        (1000, 3600),    // 1k rows → 1h
        (10000, 10800),  // 10k rows → 3h
        (30000, 21600),  // 30k rows → 6h
        (60000, 43200),  // 60k rows → 12h
        (150000, 86400), // 150k rows → 24h
    ];

    for (row_count, expected_cooldown) in test_cases {
        store
            .record_vector_index_rebuild(table, row_count, expected_cooldown)
            .await
            .expect("Record rebuild");

        let (_, _, cooldown) = store
            .should_rebuild_vector_index(table)
            .await
            .expect("Check cooldown");

        assert!(
            cooldown.is_some(),
            "Should have cooldown for {} rows",
            row_count
        );
        let remaining = cooldown.unwrap();
        // 允许一些误差（因为时间流逝）
        assert!(
            remaining > expected_cooldown - 10 && remaining <= expected_cooldown,
            "Cooldown for {} rows should be ~{}s, got {}s",
            row_count,
            expected_cooldown,
            remaining
        );
    }
}

#[tokio::test]
async fn test_rebuild_failure_exponential_backoff() {
    let store = setup().await;
    let table = "mem_memories";

    // 清理之前的状态
    let key = format!("vector_index_rebuild:{}", table);
    let _ = sqlx::query("DELETE FROM mem_governance_runtime_state WHERE strategy_key = ?")
        .bind(&key)
        .execute(store.pool())
        .await;

    // 第1次失败：5分钟
    let cooldown1 = store
        .record_vector_index_rebuild_failure(table)
        .await
        .expect("Record failure 1");
    assert_eq!(cooldown1, 300, "First failure should have 5min cooldown");

    // 第2次失败：15分钟
    let cooldown2 = store
        .record_vector_index_rebuild_failure(table)
        .await
        .expect("Record failure 2");
    assert_eq!(cooldown2, 900, "Second failure should have 15min cooldown");

    // 第3次失败：1小时
    let cooldown3 = store
        .record_vector_index_rebuild_failure(table)
        .await
        .expect("Record failure 3");
    assert_eq!(cooldown3, 3600, "Third+ failure should have 1h cooldown");

    // 成功后重置
    store
        .record_vector_index_rebuild(table, 1000, 3600)
        .await
        .expect("Record success");

    // 再次失败应该从5分钟开始
    let cooldown4 = store
        .record_vector_index_rebuild_failure(table)
        .await
        .expect("Record failure 4");
    assert_eq!(cooldown4, 300, "After success, should reset to 5min");

    println!("✅ Exponential backoff test passed");
}

/// Test: multi-user vector search with pre-filter mode.
/// 1. Insert memories for two different users.
/// 2. Build IVF index after data import.
/// 3. Verify each user only gets their own results.
#[tokio::test]
async fn test_vector_search_pre_filter_multi_user() {
    let store = setup().await;
    let dim = test_dim();

    let uid_a = format!("vec_pre_a_{}", &uuid::Uuid::new_v4().to_string()[..8]);
    let uid_b = format!("vec_pre_b_{}", &uuid::Uuid::new_v4().to_string()[..8]);

    // Build a dim-matched embedding with a hot dimension at index `hot`
    let make_emb = |hot: usize| -> Vec<f32> {
        assert!(hot < dim, "hot index {hot} exceeds embedding dim {dim}");
        let mut v = vec![0.0f32; dim];
        v[hot] = 1.0;
        v
    };

    let insert = |uid: String, content: String, emb: Vec<f32>| {
        let pool = store.pool().clone();
        let mid = uuid::Uuid::new_v4().simple().to_string();
        let vec_lit = format!(
            "[{}]",
            emb.iter()
                .map(|f| f.to_string())
                .collect::<Vec<_>>()
                .join(",")
        );
        async move {
            sqlx::query(&format!(
                "INSERT INTO mem_memories \
                 (memory_id, user_id, memory_type, content, embedding, is_active, \
                  initial_confidence, source_event_ids, observed_at, created_at) \
                 VALUES (?, ?, 'semantic', ?, '{vec_lit}', 1, 0.9, '[]', NOW(), NOW())"
            ))
            .bind(&mid)
            .bind(&uid)
            .bind(&content)
            .execute(&pool)
            .await
            .expect("insert");
            mid
        }
    };

    // User A: two memories (hot dims 0, 1)
    insert(uid_a.clone(), "user A memory 1".into(), make_emb(0)).await;
    insert(uid_a.clone(), "user A memory 2".into(), make_emb(1)).await;
    // User B: memory in same direction as query (hot dim 0) but different user.
    // Note: IVF index is approximate — orthogonal vectors may not be found in probed clusters.
    insert(uid_b.clone(), "user B memory 1".into(), make_emb(0)).await;

    // Build IVF index after data import
    let indexed = store
        .rebuild_vector_index("mem_memories")
        .await
        .expect("rebuild");
    assert!(
        indexed > 0,
        "expected at least 1 indexed row, got {indexed}"
    );

    // Query close to user A's memories
    let query = make_emb(0);

    let results_a = store
        .search_vector_from("mem_memories", &uid_a, &query, 10)
        .await
        .expect("search user A");

    let results_b = store
        .search_vector_from("mem_memories", &uid_b, &query, 10)
        .await
        .expect("search user B");

    assert!(!results_a.is_empty(), "user A should have results");
    assert!(
        results_a.iter().all(|m| m.user_id == uid_a),
        "user A results must only contain user A memories"
    );

    assert!(!results_b.is_empty(), "user B should have results");
    assert!(
        results_b.iter().all(|m| m.user_id == uid_b),
        "user B results must only contain user B memories"
    );

    let a_ids: std::collections::HashSet<_> = results_a.iter().map(|m| &m.memory_id).collect();
    let b_ids: std::collections::HashSet<_> = results_b.iter().map(|m| &m.memory_id).collect();
    assert!(
        a_ids.is_disjoint(&b_ids),
        "results must not overlap between users"
    );

    // Cleanup
    sqlx::query("DELETE FROM mem_memories WHERE user_id = ? OR user_id = ?")
        .bind(&uid_a)
        .bind(&uid_b)
        .execute(store.pool())
        .await
        .ok();
}

// ═══════════════════════════════════════════════════════════════════════════════
// cleanup_tool_results
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_cleanup_tool_results() {
    let store = setup().await;
    let uid = uuid::Uuid::new_v4().to_string();

    // Insert an old tool_result (observed 100 hours ago)
    let old_id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO mem_memories (memory_id, user_id, memory_type, content, is_active, initial_confidence, source_event_ids, observed_at, created_at, updated_at) \
         VALUES (?, ?, 'tool_result', 'old result', 1, 0.9, '[]', NOW() - INTERVAL 100 HOUR, NOW(), NOW())"
    ).bind(&old_id).bind(&uid).execute(store.pool()).await.unwrap();

    // Insert a fresh tool_result
    let fresh_id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO mem_memories (memory_id, user_id, memory_type, content, is_active, initial_confidence, source_event_ids, observed_at, created_at, updated_at) \
         VALUES (?, ?, 'tool_result', 'fresh result', 1, 0.9, '[]', NOW(), NOW(), NOW())"
    ).bind(&fresh_id).bind(&uid).execute(store.pool()).await.unwrap();

    // Insert a non-tool_result that is also old — should NOT be deleted
    let semantic_id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO mem_memories (memory_id, user_id, memory_type, content, is_active, initial_confidence, source_event_ids, observed_at, created_at, updated_at) \
         VALUES (?, ?, 'semantic', 'old semantic', 1, 0.9, '[]', NOW() - INTERVAL 100 HOUR, NOW(), NOW())"
    ).bind(&semantic_id).bind(&uid).execute(store.pool()).await.unwrap();

    let cleaned = store.cleanup_tool_results(72).await.unwrap();
    assert!(cleaned >= 1, "should clean at least the old tool_result");

    // Old tool_result gone
    let (c,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM mem_memories WHERE memory_id = ?")
        .bind(&old_id)
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(c, 0, "old tool_result should be deleted");

    // Fresh tool_result still there
    let (c,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM mem_memories WHERE memory_id = ?")
        .bind(&fresh_id)
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(c, 1, "fresh tool_result should remain");

    // Semantic memory untouched
    let (c,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM mem_memories WHERE memory_id = ?")
        .bind(&semantic_id)
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(c, 1, "semantic memory should remain");
}

// ═══════════════════════════════════════════════════════════════════════════════
// cleanup_orphaned_incrementals
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_cleanup_orphaned_incrementals() {
    let store = setup().await;
    let uid = uuid::Uuid::new_v4().to_string();
    let sid = uuid::Uuid::new_v4().to_string();

    // Insert an old incremental summary (>24h, has session_id, no full summary exists)
    let inc_id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO mem_memories (memory_id, user_id, memory_type, content, is_active, initial_confidence, source_event_ids, session_id, observed_at, created_at, updated_at) \
         VALUES (?, ?, 'episodic', '[session_summary:incremental] partial summary', 1, 0.9, '[]', ?, NOW() - INTERVAL 48 HOUR, NOW(), NOW())"
    ).bind(&inc_id).bind(&uid).bind(&sid).execute(store.pool()).await.unwrap();

    // Insert a normal memory — should NOT be affected
    let normal_id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO mem_memories (memory_id, user_id, memory_type, content, is_active, initial_confidence, source_event_ids, observed_at, created_at, updated_at) \
         VALUES (?, ?, 'semantic', 'normal memory', 1, 0.9, '[]', NOW() - INTERVAL 48 HOUR, NOW(), NOW())"
    ).bind(&normal_id).bind(&uid).execute(store.pool()).await.unwrap();

    let cleaned = store.cleanup_orphaned_incrementals(&uid, 24).await.unwrap();
    assert_eq!(cleaned, 1, "should clean the orphaned incremental");

    // Incremental deactivated
    let (active,): (i8,) = sqlx::query_as("SELECT is_active FROM mem_memories WHERE memory_id = ?")
        .bind(&inc_id)
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(active, 0, "incremental should be deactivated");

    // Normal memory untouched
    let (active,): (i8,) = sqlx::query_as("SELECT is_active FROM mem_memories WHERE memory_id = ?")
        .bind(&normal_id)
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(active, 1, "normal memory should remain active");
}

// ═══════════════════════════════════════════════════════════════════════════════
// compress_redundant
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_compress_redundant() {
    let store = setup().await;
    let uid = uuid::Uuid::new_v4().to_string();
    let dim = test_dim();

    // Create two memories with identical embeddings (cosine similarity = 1.0)
    let emb = format!("[{}]", vec!["0.1"; dim].join(","));

    let id_old = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO mem_memories (memory_id, user_id, memory_type, content, embedding, is_active, initial_confidence, source_event_ids, observed_at, created_at, updated_at) \
         VALUES (?, ?, 'semantic', 'duplicate A', ?, 1, 0.9, '[]', NOW() - INTERVAL 2 HOUR, NOW(), NOW())"
    ).bind(&id_old).bind(&uid).bind(&emb).execute(store.pool()).await.unwrap();

    let id_new = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO mem_memories (memory_id, user_id, memory_type, content, embedding, is_active, initial_confidence, source_event_ids, observed_at, created_at, updated_at) \
         VALUES (?, ?, 'semantic', 'duplicate B', ?, 1, 0.9, '[]', NOW() - INTERVAL 1 HOUR, NOW(), NOW())"
    ).bind(&id_new).bind(&uid).bind(&emb).execute(store.pool()).await.unwrap();

    // Create a memory with a very different embedding — should NOT be compressed
    let diff_emb = format!("[{}]", vec!["0.9"; dim].join(","));
    let id_diff = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO mem_memories (memory_id, user_id, memory_type, content, embedding, is_active, initial_confidence, source_event_ids, observed_at, created_at, updated_at) \
         VALUES (?, ?, 'semantic', 'different', ?, 1, 0.9, '[]', NOW(), NOW(), NOW())"
    ).bind(&id_diff).bind(&uid).bind(&diff_emb).execute(store.pool()).await.unwrap();

    let compressed = store
        .compress_redundant(&uid, 0.95, 30, 10_000)
        .await
        .unwrap();
    assert_eq!(compressed, 1, "should compress 1 redundant pair");

    // Older duplicate physically deleted
    let (c,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM mem_memories WHERE memory_id = ?")
        .bind(&id_old)
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(c, 0, "older duplicate should be physically deleted");

    // Newer duplicate still active
    let (active,): (i8,) = sqlx::query_as("SELECT is_active FROM mem_memories WHERE memory_id = ?")
        .bind(&id_new)
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(active, 1, "newer duplicate should remain active");

    // Different memory untouched
    let (active,): (i8,) = sqlx::query_as("SELECT is_active FROM mem_memories WHERE memory_id = ?")
        .bind(&id_diff)
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(active, 1, "different memory should remain active");
}

// ═══════════════════════════════════════════════════════════════════════════════
// cleanup_orphan_branches
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_cleanup_orphan_branches() {
    let store = setup().await;

    // Create a sandbox table that looks like an orphan branch
    let (db_name,): (String,) = sqlx::query_as("SELECT DATABASE()")
        .fetch_one(store.pool())
        .await
        .unwrap();
    let table_name = format!("memories_sandbox_{}", uuid::Uuid::new_v4().simple());

    // Create via DATA BRANCH (the same mechanism the app uses)
    let create_sql =
        format!("DATA BRANCH CREATE TABLE {db_name}.{table_name} FROM {db_name}.mem_memories");
    let create_result = sqlx::raw_sql(&create_sql).execute(store.pool()).await;

    if create_result.is_err() {
        // If DATA BRANCH is not supported in this test environment, skip
        eprintln!("⚠️ DATA BRANCH not supported, skipping test_cleanup_orphan_branches");
        return;
    }

    // Verify table exists
    let (count,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM information_schema.tables WHERE table_name = ?")
            .bind(&table_name)
            .fetch_one(store.pool())
            .await
            .unwrap();
    assert_eq!(count, 1, "sandbox table should exist");

    // Clean orphan branches
    let cleaned = store.cleanup_orphan_branches().await.unwrap();
    assert!(cleaned >= 1, "should clean at least 1 orphan branch table");

    // Verify table is gone
    let (count,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM information_schema.tables WHERE table_name = ?")
            .bind(&table_name)
            .fetch_one(store.pool())
            .await
            .unwrap();
    assert_eq!(count, 0, "sandbox table should be deleted");
}

// ═══════════════════════════════════════════════════════════════════════════════
// health_hygiene — dangling graph nodes (memory physically deleted)
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_health_hygiene_detects_dangling_graph_nodes() {
    let store = setup().await;
    let uid = uuid::Uuid::new_v4().to_string();
    let nonexistent_mid = uuid::Uuid::new_v4().to_string();
    let node_id = uuid::Uuid::new_v4().as_simple().to_string();

    // Insert an active graph node pointing to a memory_id that doesn't exist at all
    sqlx::query(
        "INSERT INTO memory_graph_nodes (node_id, user_id, node_type, content, memory_id, is_active, created_at) \
         VALUES (?, ?, 'memory', 'dangling node', ?, 1, NOW())"
    ).bind(&node_id).bind(&uid).bind(&nonexistent_mid)
    .execute(store.pool()).await.unwrap();

    let result = store.health_hygiene(&uid).await.unwrap();
    let orphan_gn = result["orphan_graph_nodes"].as_i64().unwrap();
    assert!(
        orphan_gn >= 1,
        "should detect dangling graph node (memory physically deleted)"
    );

    // Also verify global
    let global = store.health_hygiene_global().await.unwrap();
    let global_gn = global["orphan_graph_nodes"].as_i64().unwrap();
    assert!(
        global_gn >= 1,
        "global should also detect dangling graph node"
    );

    // Cleanup
    sqlx::query("DELETE FROM memory_graph_nodes WHERE node_id = ?")
        .bind(&node_id)
        .execute(store.pool())
        .await
        .ok();
}

// ═══════════════════════════════════════════════════════════════════════════════
// health_hygiene — entity nodes (memory_id IS NULL) must NOT be reported as orphan
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_health_hygiene_does_not_misreport_entity_nodes() {
    let store = setup().await;
    let uid = uuid::Uuid::new_v4().to_string();
    let node_id = uuid::Uuid::new_v4().as_simple().to_string();

    // Insert an entity node with memory_id = NULL — this is legitimate
    sqlx::query(
        "INSERT INTO memory_graph_nodes (node_id, user_id, node_type, content, memory_id, is_active, created_at) \
         VALUES (?, ?, 'entity', 'some concept', NULL, 1, NOW())"
    ).bind(&node_id).bind(&uid)
    .execute(store.pool()).await.unwrap();

    let result = store.health_hygiene(&uid).await.unwrap();
    let orphan_gn = result["orphan_graph_nodes"].as_i64().unwrap();
    assert_eq!(
        orphan_gn, 0,
        "entity node (memory_id=NULL) must not be counted as orphan"
    );

    // Cleanup
    sqlx::query("DELETE FROM memory_graph_nodes WHERE node_id = ?")
        .bind(&node_id)
        .execute(store.pool())
        .await
        .ok();
}

// ═══════════════════════════════════════════════════════════════════════════════
// detect_pollution — empty result set (SUM returns NULL)
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_detect_pollution_empty_user() {
    let store = setup().await;
    // Brand new user with zero memories — SUM(CASE...) returns NULL
    let uid = uuid::Uuid::new_v4().to_string();
    let result = store.detect_pollution(&uid, 24).await.unwrap();
    assert!(!result, "empty user should not be flagged as polluted");
}

// ═══════════════════════════════════════════════════════════════════════════════
// cleanup_stale — preserves version chain (superseded_by rows kept)
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_cleanup_stale_preserves_history_chain() {
    let store = setup().await;
    let uid = uuid::Uuid::new_v4().to_string();
    let old_mid = uuid::Uuid::new_v4().to_string();
    let new_mid = uuid::Uuid::new_v4().to_string();
    let plain_mid = uuid::Uuid::new_v4().to_string();

    // Insert the target of the version chain (active, the "new" version)
    sqlx::query(
        "INSERT INTO mem_memories (memory_id, user_id, memory_type, content, source_event_ids, \
         is_active, observed_at, created_at) \
         VALUES (?, ?, 'semantic', 'new version', '[]', 1, NOW(), NOW())",
    )
    .bind(&new_mid)
    .bind(&uid)
    .execute(store.pool())
    .await
    .unwrap();

    // Insert a superseded memory (part of version chain) — inactive, old, has superseded_by
    sqlx::query(
        "INSERT INTO mem_memories (memory_id, user_id, memory_type, content, source_event_ids, \
         is_active, superseded_by, updated_at, observed_at, created_at) \
         VALUES (?, ?, 'semantic', 'old version', '[]', 0, ?, \
         DATE_SUB(NOW(), INTERVAL 48 HOUR), DATE_SUB(NOW(), INTERVAL 48 HOUR), NOW())",
    )
    .bind(&old_mid)
    .bind(&uid)
    .bind(&new_mid)
    .execute(store.pool())
    .await
    .unwrap();

    // Insert a plain inactive memory (no superseded_by, old enough) — should be deleted
    sqlx::query(
        "INSERT INTO mem_memories (memory_id, user_id, memory_type, content, source_event_ids, \
         is_active, updated_at, observed_at, created_at) \
         VALUES (?, ?, 'semantic', 'plain stale', '[]', 0, \
         DATE_SUB(NOW(), INTERVAL 48 HOUR), DATE_SUB(NOW(), INTERVAL 48 HOUR), NOW())",
    )
    .bind(&plain_mid)
    .bind(&uid)
    .execute(store.pool())
    .await
    .unwrap();

    let cleaned = store.cleanup_stale(&uid).await.unwrap();
    assert_eq!(cleaned, 1, "should delete only the plain inactive memory");

    // Version chain row preserved
    let (c,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM mem_memories WHERE memory_id = ?")
        .bind(&old_mid)
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(
        c, 1,
        "superseded memory must be preserved for history chain"
    );

    // Plain stale deleted
    let (c,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM mem_memories WHERE memory_id = ?")
        .bind(&plain_mid)
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(c, 0, "plain inactive memory should be deleted");

    // Cleanup
    sqlx::query("DELETE FROM mem_memories WHERE memory_id IN (?, ?)")
        .bind(&old_mid)
        .bind(&new_mid)
        .execute(store.pool())
        .await
        .ok();
}

// ═══════════════════════════════════════════════════════════════════════════════
// cleanup_stale — deletes broken version chain (superseded_by target gone)
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_cleanup_stale_deletes_broken_chain() {
    let store = setup().await;
    let uid = uuid::Uuid::new_v4().to_string();
    let broken_mid = uuid::Uuid::new_v4().to_string();
    let gone_target = uuid::Uuid::new_v4().to_string(); // never inserted

    // Insert inactive memory whose superseded_by points to a nonexistent memory
    sqlx::query(
        "INSERT INTO mem_memories (memory_id, user_id, memory_type, content, source_event_ids, \
         is_active, superseded_by, updated_at, observed_at, created_at) \
         VALUES (?, ?, 'semantic', 'broken chain', '[]', 0, ?, \
         DATE_SUB(NOW(), INTERVAL 48 HOUR), DATE_SUB(NOW(), INTERVAL 48 HOUR), NOW())",
    )
    .bind(&broken_mid)
    .bind(&uid)
    .bind(&gone_target)
    .execute(store.pool())
    .await
    .unwrap();

    let cleaned = store.cleanup_stale(&uid).await.unwrap();
    assert_eq!(cleaned, 1, "should delete broken chain row");

    let (c,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM mem_memories WHERE memory_id = ?")
        .bind(&broken_mid)
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(c, 0, "broken chain memory should be deleted");
}

// ═══════════════════════════════════════════════════════════════════════════════
// cleanup_stale — cascading broken chain deletion (A→B→C, C gone → B gone → A gone)
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_cleanup_stale_cascading_broken_chain() {
    let store = setup().await;
    let uid = uuid::Uuid::new_v4().to_string();
    let mid_a = uuid::Uuid::new_v4().to_string();
    let mid_b = uuid::Uuid::new_v4().to_string();
    let mid_c = uuid::Uuid::new_v4().to_string(); // never inserted — the root break

    // Chain: A → B → C (C doesn't exist)
    // B is inactive with superseded_by = C (broken)
    sqlx::query(
        "INSERT INTO mem_memories (memory_id, user_id, memory_type, content, source_event_ids, \
         is_active, superseded_by, updated_at, observed_at, created_at) \
         VALUES (?, ?, 'semantic', 'version B', '[]', 0, ?, \
         DATE_SUB(NOW(), INTERVAL 48 HOUR), DATE_SUB(NOW(), INTERVAL 48 HOUR), NOW())",
    )
    .bind(&mid_b)
    .bind(&uid)
    .bind(&mid_c)
    .execute(store.pool())
    .await
    .unwrap();

    // A is inactive with superseded_by = B (valid chain, but B will be deleted)
    sqlx::query(
        "INSERT INTO mem_memories (memory_id, user_id, memory_type, content, source_event_ids, \
         is_active, superseded_by, updated_at, observed_at, created_at) \
         VALUES (?, ?, 'semantic', 'version A', '[]', 0, ?, \
         DATE_SUB(NOW(), INTERVAL 48 HOUR), DATE_SUB(NOW(), INTERVAL 48 HOUR), NOW())",
    )
    .bind(&mid_a)
    .bind(&uid)
    .bind(&mid_b)
    .execute(store.pool())
    .await
    .unwrap();

    let cleaned = store.cleanup_stale(&uid).await.unwrap();
    assert_eq!(cleaned, 2, "should cascade-delete both A and B");

    let (c,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM mem_memories WHERE memory_id IN (?, ?)")
            .bind(&mid_a)
            .bind(&mid_b)
            .fetch_one(store.pool())
            .await
            .unwrap();
    assert_eq!(c, 0, "both chain nodes should be gone");
}

// ═══════════════════════════════════════════════════════════════════════════════
// detect_pollution — normal user returns false
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_detect_pollution_normal_user() {
    let store = setup().await;
    let uid = uuid::Uuid::new_v4().to_string();

    // Insert 5 active memories, none superseded → ratio = 0/5 = 0 < 0.3
    for i in 0..5 {
        let mid = uuid::Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO mem_memories (memory_id, user_id, memory_type, content, source_event_ids, \
             is_active, observed_at, created_at, updated_at) \
             VALUES (?, ?, 'semantic', ?, '[]', 1, NOW(), NOW(), NOW())"
        ).bind(&mid).bind(&uid).bind(format!("fact {i}"))
        .execute(store.pool()).await.unwrap();
    }

    let result = store.detect_pollution(&uid, 24).await.unwrap();
    assert!(
        !result,
        "normal user with no supersedes should not be polluted"
    );

    // Cleanup
    sqlx::query("DELETE FROM mem_memories WHERE user_id = ?")
        .bind(&uid)
        .execute(store.pool())
        .await
        .ok();
}
