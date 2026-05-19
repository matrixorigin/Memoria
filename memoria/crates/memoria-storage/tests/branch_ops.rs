/// Branch-aware memory integration tests against real MatrixOne.
/// Run: DATABASE_URL=mysql://root:111@localhost:6001/memoria_test \
///      cargo test -p memoria-storage --test branch_ops -- --nocapture
use memoria_core::{Memory, MemoryType, TrustTier};
use memoria_storage::SqlMemoryStore;
use sqlx::mysql::MySqlPool;
use uuid::Uuid;

fn db_url() -> String {
    std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "mysql://root:111@localhost:6001/memoria_test".to_string())
}

fn uid() -> String {
    format!(
        "test_{}",
        Uuid::new_v4().simple().to_string()[..8].to_string()
    )
}

fn make_memory(user_id: &str, content: &str) -> Memory {
    Memory {
        memory_id: Uuid::new_v4().simple().to_string(),
        user_id: user_id.to_string(),
        memory_type: MemoryType::Semantic,
        content: content.to_string(),
        embedding: None,
        session_id: None,
        source_event_ids: vec![],
        extra_metadata: None,
        is_active: true,
        superseded_by: None,
        trust_tier: TrustTier::T3Inferred,
        initial_confidence: 0.75,
        access_count: 0,
        retrieval_score: None,
        observed_at: None,
        created_at: None,
        updated_at: None,
    }
}

fn test_dim() -> usize {
    std::env::var("EMBEDDING_DIM")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1024)
}

async fn create_branch_table(pool: &sqlx::MySqlPool, table: &str, dim: usize) {
    let sql = format!(
        r#"CREATE TABLE IF NOT EXISTS {table} (
            memory_id       VARCHAR(64)  PRIMARY KEY,
            user_id         VARCHAR(64)  NOT NULL,
            memory_type     VARCHAR(20)  NOT NULL,
            content         TEXT         NOT NULL,
            embedding       vecf32({dim}),
            session_id      VARCHAR(64),
            source_event_ids JSON        NOT NULL,
            extra_metadata  JSON,
            is_active       TINYINT(1)   NOT NULL DEFAULT 1,
            superseded_by   VARCHAR(64),
            trust_tier      VARCHAR(10)  DEFAULT 'T1',
            initial_confidence FLOAT     DEFAULT 0.95,
            observed_at     DATETIME(6)  NOT NULL,
            created_at      DATETIME(6)  NOT NULL,
            updated_at      DATETIME(6),
            INDEX idx_user_active (user_id, is_active, memory_type)
        )"#
    );
    sqlx::query(&sql)
        .execute(pool)
        .await
        .expect("create branch table");
}

async fn setup() -> SqlMemoryStore {
    let pool = MySqlPool::connect(&db_url()).await.expect("connect");
    let instance_id = uuid::Uuid::new_v4().to_string();
    let store = SqlMemoryStore::new(pool, test_dim(), instance_id);
    store.migrate().await.expect("migrate");
    store
}

// ── 1. active_table defaults to mem_memories ─────────────────────────────────

#[tokio::test]
async fn test_active_table_default_is_main() {
    let store = setup().await;
    let user = uid();

    let table = store.active_table(&user).await.expect("active_table");
    assert_eq!(table, "mem_memories");
    println!("✅ active_table default = mem_memories");
}

// ── 2. set_active_branch + active_table returns branch table ─────────────────

#[tokio::test]
async fn test_set_active_branch_changes_table() {
    let store = setup().await;
    let user = uid();
    let branch_name = format!("br_{}", &uid()[5..]);
    let table_name = format!("mem_br_{}", &uid()[5..]);

    // Register branch + switch
    store
        .register_branch(&user, &branch_name, &table_name)
        .await
        .expect("register");
    store
        .set_active_branch(&user, &branch_name)
        .await
        .expect("set_active");

    let table = store.active_table(&user).await.expect("active_table");
    assert_eq!(table, table_name);
    println!("✅ active_table after checkout = {table}");

    // Cleanup
    store.deregister_branch(&user, &branch_name).await.ok();
    store.set_active_branch(&user, "main").await.ok();
}

// ── 3. active_table falls back to main if branch deleted ─────────────────────

#[tokio::test]
async fn test_active_table_fallback_when_branch_deleted() {
    let store = setup().await;
    let user = uid();
    let branch_name = format!("br_{}", &uid()[5..]);
    let table_name = format!("mem_br_{}", &uid()[5..]);

    store
        .register_branch(&user, &branch_name, &table_name)
        .await
        .expect("register");
    store
        .set_active_branch(&user, &branch_name)
        .await
        .expect("set_active");

    // Delete branch while still "checked out"
    store
        .deregister_branch(&user, &branch_name)
        .await
        .expect("deregister");

    // Should auto-fallback to main
    let table = store.active_table(&user).await.expect("active_table");
    assert_eq!(table, "mem_memories");
    println!("✅ active_table falls back to mem_memories after branch deleted");
}

// ── 4. list_branches returns only active branches ────────────────────────────

#[tokio::test]
async fn test_list_branches() {
    let store = setup().await;
    let user = uid();
    let b1 = format!("br1_{}", &uid()[5..]);
    let b2 = format!("br2_{}", &uid()[5..]);

    store
        .register_branch(&user, &b1, &format!("tbl_{b1}"))
        .await
        .expect("reg b1");
    store
        .register_branch(&user, &b2, &format!("tbl_{b2}"))
        .await
        .expect("reg b2");

    let branches = store.list_branches(&user).await.expect("list");
    let names: Vec<_> = branches.iter().map(|(n, _)| n.as_str()).collect();
    assert!(names.contains(&b1.as_str()), "b1 missing");
    assert!(names.contains(&b2.as_str()), "b2 missing");
    println!("✅ list_branches: {:?}", names);

    // Delete one — should disappear
    store.deregister_branch(&user, &b1).await.expect("dereg");
    let branches = store.list_branches(&user).await.expect("list after delete");
    let names: Vec<_> = branches.iter().map(|(n, _)| n.as_str()).collect();
    assert!(!names.contains(&b1.as_str()), "b1 should be gone");
    assert!(names.contains(&b2.as_str()), "b2 should remain");
    println!("✅ list_branches after delete: {:?}", names);

    store.deregister_branch(&user, &b2).await.ok();
}

// ── 5. insert_into writes to branch table, not mem_memories ──────────────────

#[tokio::test]
async fn test_insert_into_branch_table() {
    let store = setup().await;
    let user = uid();

    // Create a real branch table (copy of mem_memories schema)
    let branch_table = format!("mem_br_{}", &uid()[5..]);
    create_branch_table(store.pool(), &branch_table, test_dim()).await;

    let mem = make_memory(&user, "branch-only fact");
    store
        .insert_into(&branch_table, &mem)
        .await
        .expect("insert_into branch");

    // Should appear in branch table
    let rows = store
        .list_active_from(&branch_table, &user, 10)
        .await
        .expect("list branch");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].content, "branch-only fact");
    println!("✅ insert_into branch table: found {} memory", rows.len());

    // Should NOT appear in mem_memories
    let main_rows = store
        .list_active_from("mem_memories", &user, 10)
        .await
        .expect("list main");
    assert!(main_rows.is_empty(), "should not be in main");
    println!("✅ mem_memories untouched");

    // Cleanup
    sqlx::raw_sql(&format!("DROP TABLE IF EXISTS {branch_table}"))
        .execute(store.pool())
        .await
        .ok();
}

// ── 6. merge: INSERT IGNORE from branch into mem_memories ────────────────────

#[tokio::test]
async fn test_merge_branch_into_main() {
    let store = setup().await;
    let user = uid();

    let branch_table = format!("mem_br_{}", &uid()[5..]);
    create_branch_table(store.pool(), &branch_table, test_dim()).await;

    // Write 2 memories to branch
    let m1 = make_memory(&user, "branch memory A");
    let m2 = make_memory(&user, "branch memory B");
    store
        .insert_into(&branch_table, &m1)
        .await
        .expect("insert m1");
    store
        .insert_into(&branch_table, &m2)
        .await
        .expect("insert m2");

    // Merge: NOT EXISTS instead of INSERT IGNORE (MatrixOne bug: INSERT IGNORE SELECT * skips all rows with vecf32 column)
    let sql = format!(
        "INSERT INTO mem_memories \
            (memory_id, user_id, memory_type, content, embedding, session_id, \
             source_event_ids, extra_metadata, is_active, superseded_by, \
             trust_tier, initial_confidence, observed_at, created_at, updated_at) \
         SELECT b.memory_id, b.user_id, b.memory_type, b.content, b.embedding, b.session_id, \
             b.source_event_ids, b.extra_metadata, b.is_active, b.superseded_by, \
             b.trust_tier, b.initial_confidence, b.observed_at, b.created_at, b.updated_at \
         FROM {branch_table} b \
         WHERE b.user_id = ? \
           AND NOT EXISTS (SELECT 1 FROM mem_memories m WHERE m.memory_id = b.memory_id)"
    );
    sqlx::query(&sql)
        .bind(&user)
        .execute(store.pool())
        .await
        .expect("merge");

    // Both should now be in main
    let main_rows = store
        .list_active_from("mem_memories", &user, 10)
        .await
        .expect("list main");
    assert_eq!(main_rows.len(), 2);
    let contents: Vec<_> = main_rows.iter().map(|m| m.content.as_str()).collect();
    assert!(contents.contains(&"branch memory A"));
    assert!(contents.contains(&"branch memory B"));
    println!("✅ merge: {} memories in main after merge", main_rows.len());

    // Idempotent: second merge should skip duplicates (INSERT IGNORE)
    sqlx::query(&sql)
        .bind(&user)
        .execute(store.pool())
        .await
        .expect("merge again");
    let main_rows2 = store
        .list_active_from("mem_memories", &user, 10)
        .await
        .expect("list main 2");
    assert_eq!(main_rows2.len(), 2, "INSERT IGNORE must not duplicate");
    println!("✅ merge idempotent: still {} memories", main_rows2.len());

    // Cleanup
    sqlx::raw_sql(&format!("DROP TABLE IF EXISTS {branch_table}"))
        .execute(store.pool())
        .await
        .ok();
    // Remove test memories from main
    sqlx::query("DELETE FROM mem_memories WHERE user_id = ?")
        .bind(&user)
        .execute(store.pool())
        .await
        .ok();
}

// ── 7. full workflow: branch → write → checkout main → merge ─────────────────

#[tokio::test]
async fn test_full_branch_workflow() {
    let store = setup().await;
    let user = uid();
    let branch_name = format!("eval_{}", &uid()[5..]);
    let branch_table = format!("mem_br_{}", &uid()[5..]);

    // 1. Write a memory on main
    let main_mem = make_memory(&user, "main fact");
    store
        .insert_into("mem_memories", &main_mem)
        .await
        .expect("insert main");

    // 2. Create branch table + register + checkout
    create_branch_table(store.pool(), &branch_table, test_dim()).await;
    store
        .register_branch(&user, &branch_name, &branch_table)
        .await
        .expect("register");
    store
        .set_active_branch(&user, &branch_name)
        .await
        .expect("checkout");

    // 3. Write branch-only memory
    let branch_mem = make_memory(&user, "branch experiment");
    let active = store.active_table(&user).await.expect("active_table");
    assert_eq!(active, branch_table);
    store
        .insert_into(&active, &branch_mem)
        .await
        .expect("insert branch");

    // 4. Main should still have only 1 memory
    let main_rows = store
        .list_active_from("mem_memories", &user, 10)
        .await
        .expect("list main");
    assert_eq!(main_rows.len(), 1);
    assert_eq!(main_rows[0].content, "main fact");

    // 5. Checkout main
    store
        .set_active_branch(&user, "main")
        .await
        .expect("checkout main");
    let active = store.active_table(&user).await.expect("active_table");
    assert_eq!(active, "mem_memories");

    // 6. Merge branch into main
    let sql = format!(
        "INSERT INTO mem_memories \
            (memory_id, user_id, memory_type, content, embedding, session_id, \
             source_event_ids, extra_metadata, is_active, superseded_by, \
             trust_tier, initial_confidence, observed_at, created_at, updated_at) \
         SELECT b.memory_id, b.user_id, b.memory_type, b.content, b.embedding, b.session_id, \
             b.source_event_ids, b.extra_metadata, b.is_active, b.superseded_by, \
             b.trust_tier, b.initial_confidence, b.observed_at, b.created_at, b.updated_at \
         FROM {branch_table} b \
         WHERE b.user_id = ? \
           AND NOT EXISTS (SELECT 1 FROM mem_memories m WHERE m.memory_id = b.memory_id)"
    );
    sqlx::query(&sql)
        .bind(&user)
        .execute(store.pool())
        .await
        .expect("merge");

    let main_rows = store
        .list_active_from("mem_memories", &user, 10)
        .await
        .expect("list main after merge");
    assert_eq!(main_rows.len(), 2);
    println!(
        "✅ full workflow: main has {} memories after merge",
        main_rows.len()
    );

    // 7. Delete branch
    store
        .deregister_branch(&user, &branch_name)
        .await
        .expect("deregister");
    sqlx::raw_sql(&format!("DROP TABLE IF EXISTS {branch_table}"))
        .execute(store.pool())
        .await
        .ok();

    // Cleanup main
    sqlx::query("DELETE FROM mem_memories WHERE user_id = ?")
        .bind(&user)
        .execute(store.pool())
        .await
        .ok();

    println!("✅ full branch workflow passed");
}

// ── 8. Regression: INSERT IGNORE SELECT * silently drops rows with vecf32 ────
//
// Root cause analysis: the original merge used INSERT IGNORE ... SELECT *.
// In production, this correctly skipped rows whose memory_id already existed in main
// (because memory_store wrote to main instead of the branch — user stored before checkout).
// The fix uses explicit column list + NOT EXISTS, which is more explicit and avoids
// any potential MatrixOne behavior differences with INSERT IGNORE on vecf32 columns.

#[tokio::test]
async fn test_merge_not_insert_ignore_select_star() {
    let store = setup().await;
    let user = uid();

    let branch_table = format!("mem_br_{}", &uid()[5..]);
    create_branch_table(store.pool(), &branch_table, test_dim()).await;

    // Write a memory WITH embedding (vecf32) — this is what triggers the MO bug
    let mut mem = make_memory(&user, "memory with embedding");
    mem.embedding = Some(vec![0.1_f32; test_dim()]);
    store
        .insert_into(&branch_table, &mem)
        .await
        .expect("insert with embedding");

    // Correct merge SQL: explicit columns + NOT EXISTS
    let merge_sql = format!(
        "INSERT INTO mem_memories \
            (memory_id, user_id, memory_type, content, embedding, session_id, \
             source_event_ids, extra_metadata, is_active, superseded_by, \
             trust_tier, initial_confidence, observed_at, created_at, updated_at) \
         SELECT b.memory_id, b.user_id, b.memory_type, b.content, b.embedding, b.session_id, \
             b.source_event_ids, b.extra_metadata, b.is_active, b.superseded_by, \
             b.trust_tier, b.initial_confidence, b.observed_at, b.created_at, b.updated_at \
         FROM {branch_table} b \
         WHERE b.user_id = ? \
           AND NOT EXISTS (SELECT 1 FROM mem_memories m WHERE m.memory_id = b.memory_id)"
    );
    let res = sqlx::query(&merge_sql)
        .bind(&user)
        .execute(store.pool())
        .await
        .expect("merge");
    assert_eq!(
        res.rows_affected(),
        1,
        "merge must insert the row with embedding"
    );

    // Verify it landed in main
    let rows = store
        .list_active_from("mem_memories", &user, 10)
        .await
        .expect("list");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].content, "memory with embedding");
    println!("✅ regression: merge with vecf32 embedding inserts correctly");

    // Document the broken pattern: INSERT IGNORE correctly skips rows with duplicate
    // memory_id (PK conflict). The fix (NOT EXISTS) is equivalent but more explicit.
    let broken_sql =
        format!("INSERT IGNORE INTO mem_memories SELECT * FROM {branch_table} WHERE user_id = ?");
    sqlx::query("DELETE FROM mem_memories WHERE user_id = ?")
        .bind(&user)
        .execute(store.pool())
        .await
        .ok();
    let broken_res = sqlx::query(&broken_sql)
        .bind(&user)
        .execute(store.pool())
        .await
        .expect("broken merge");
    // INSERT IGNORE works here because there's no PK conflict (fresh delete above).
    // The production issue was memory_id already existing in main (stored before checkout).
    println!(
        "ℹ️  INSERT IGNORE SELECT * rowcount={} (correct fix uses NOT EXISTS instead)",
        broken_res.rows_affected()
    );

    // Cleanup
    sqlx::raw_sql(&format!("DROP TABLE IF EXISTS {branch_table}"))
        .execute(store.pool())
        .await
        .ok();
    sqlx::query("DELETE FROM mem_memories WHERE user_id = ?")
        .bind(&user)
        .execute(store.pool())
        .await
        .ok();
}
