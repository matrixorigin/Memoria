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
    assert!(cooldown2.unwrap() > 0 && cooldown2.unwrap() <= 3600, "Cooldown should be within range");
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

        assert!(cooldown.is_some(), "Should have cooldown for {} rows", row_count);
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
        let vec_lit = format!("[{}]", emb.iter().map(|f| f.to_string()).collect::<Vec<_>>().join(","));
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
    let indexed = store.rebuild_vector_index("mem_memories").await.expect("rebuild");
    assert!(indexed > 0, "expected at least 1 indexed row, got {indexed}");

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
    assert!(a_ids.is_disjoint(&b_ids), "results must not overlap between users");

    // Cleanup
    sqlx::query("DELETE FROM mem_memories WHERE user_id = ? OR user_id = ?")
        .bind(&uid_a)
        .bind(&uid_b)
        .execute(store.pool())
        .await
        .ok();
}
