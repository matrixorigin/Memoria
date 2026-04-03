use memoria_core::interfaces::EmbeddingProvider;
use memoria_embedding::MockEmbedder;
/// Branch end-to-end tests — from the user's perspective.
/// Covers: create, checkout, store, merge, diff, delete, limits, from_snapshot,
///         from_timestamp, multi-user isolation, delete-main protection.
///
/// IMPORTANT: tests that use rollback must run serially (--test-threads=1).
/// Branch-only tests can run in parallel.
///
/// Run: DATABASE_URL=mysql://root:111@localhost:6001/memoria \
///      cargo test -p memoria-mcp --test branch_e2e -- --test-threads=1 --nocapture
use memoria_git::GitForDataService;
use memoria_service::MemoryService;
use memoria_storage::SqlMemoryStore;
use serde_json::{json, Value};
use sqlx::{mysql::MySqlPool, Row};
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
    format!("br_{}", &Uuid::new_v4().simple().to_string()[..8])
}
fn bname(suffix: &str) -> String {
    format!("b{}_{suffix}", &Uuid::new_v4().simple().to_string()[..6])
}

async fn setup() -> (Arc<MemoryService>, Arc<GitForDataService>, String) {
    let pool = MySqlPool::connect(&db_url()).await.expect("pool");
    let db_name = db_url().rsplit('/').next().unwrap_or("memoria").to_string();
    let store = SqlMemoryStore::connect(&db_url(), test_dim(), uuid::Uuid::new_v4().to_string())
        .await
        .expect("store");
    store.migrate().await.expect("migrate");
    let git = Arc::new(GitForDataService::new(pool, &db_name));
    let svc = Arc::new(MemoryService::new_sql_with_llm(Arc::new(store), None, None).await);
    (svc, git, uid())
}

async fn setup_with_mock_embedder() -> (
    Arc<MemoryService>,
    Arc<GitForDataService>,
    MySqlPool,
    String,
) {
    let pool = MySqlPool::connect(&db_url()).await.expect("pool");
    let db_name = db_url().rsplit('/').next().unwrap_or("memoria").to_string();
    let store = SqlMemoryStore::connect(&db_url(), test_dim(), uuid::Uuid::new_v4().to_string())
        .await
        .expect("store");
    store.migrate().await.expect("migrate");
    let git = Arc::new(GitForDataService::new(pool.clone(), &db_name));
    let embedder: Option<Arc<dyn EmbeddingProvider>> =
        Some(Arc::new(MockEmbedder::new(test_dim())));
    let svc = Arc::new(MemoryService::new_sql_with_llm(Arc::new(store), embedder, None).await);
    (svc, git, pool, uid())
}

async fn gc(
    name: &str,
    args: Value,
    git: &Arc<GitForDataService>,
    svc: &Arc<MemoryService>,
    uid: &str,
) -> Value {
    memoria_mcp::git_tools::call(name, args, git, svc, uid)
        .await
        .expect(name)
}
async fn store_mem(content: &str, svc: &Arc<MemoryService>, uid: &str) {
    memoria_mcp::tools::call("memory_store", json!({"content": content}), svc, uid)
        .await
        .expect("store");
}
fn text(v: &Value) -> &str {
    v["content"][0]["text"].as_str().unwrap_or("")
}

// ── 1. Basic workflow: create → checkout → store → checkout main → merge ──────

#[tokio::test]
async fn test_basic_branch_workflow() {
    let (svc, git, uid) = setup().await;
    let branch = bname("basic");

    store_mem("main memory", &svc, &uid).await;

    // Create + checkout
    let r = gc("memory_branch", json!({"name": branch}), &git, &svc, &uid).await;
    assert!(text(&r).contains("Created"), "{}", text(&r));
    let r = gc("memory_checkout", json!({"name": branch}), &git, &svc, &uid).await;
    assert!(text(&r).contains("Switched"), "{}", text(&r));

    // Store on branch
    store_mem("branch memory", &svc, &uid).await;
    assert_eq!(svc.list_active(&uid, 10).await.unwrap().len(), 2);

    // Checkout main — branch memory should not appear
    gc("memory_checkout", json!({"name": "main"}), &git, &svc, &uid).await;
    assert_eq!(svc.list_active(&uid, 10).await.unwrap().len(), 1);

    // Merge
    let r = gc("memory_merge", json!({"source": branch}), &git, &svc, &uid).await;
    assert!(
        text(&r).contains("1 new"),
        "expected 1 new, got: {}",
        text(&r)
    );
    assert_eq!(svc.list_active(&uid, 10).await.unwrap().len(), 2);
    println!("✅ basic workflow: {}", text(&r));

    gc(
        "memory_branch_delete",
        json!({"name": branch}),
        &git,
        &svc,
        &uid,
    )
    .await;
}

#[tokio::test]
async fn test_merge_accept_alias() {
    let (svc, git, uid) = setup().await;
    let branch = bname("accept");

    store_mem("main memory", &svc, &uid).await;
    gc("memory_branch", json!({"name": branch}), &git, &svc, &uid).await;
    gc("memory_checkout", json!({"name": branch}), &git, &svc, &uid).await;
    store_mem("branch memory", &svc, &uid).await;
    gc("memory_checkout", json!({"name": "main"}), &git, &svc, &uid).await;

    let r = gc(
        "memory_merge",
        json!({"source": branch, "strategy": "accept"}),
        &git,
        &svc,
        &uid,
    )
    .await;
    assert!(
        text(&r).contains("1 new"),
        "expected 1 new, got: {}",
        text(&r)
    );
    assert_eq!(svc.list_active(&uid, 10).await.unwrap().len(), 2);
    println!("✅ accept alias: {}", text(&r));

    gc(
        "memory_branch_delete",
        json!({"name": branch}),
        &git,
        &svc,
        &uid,
    )
    .await;
}

#[tokio::test]
async fn test_merge_replace_updates_conflicting_memory() {
    let (svc, git, pool, uid) = setup_with_mock_embedder().await;
    let branch = bname("replace");
    let replacement = "branch replacement memory";

    store_mem("shared conflict memory", &svc, &uid).await;
    let original = svc
        .list_active(&uid, 10)
        .await
        .unwrap()
        .into_iter()
        .find(|memory| memory.content == "shared conflict memory")
        .expect("original memory");

    gc("memory_branch", json!({"name": branch}), &git, &svc, &uid).await;
    let branch_table = svc
        .sql_store
        .as_ref()
        .expect("sql store")
        .list_branches(&uid)
        .await
        .unwrap()
        .into_iter()
        .find(|(name, _)| name == &branch)
        .map(|(_, table)| table)
        .expect("branch table");

    let branch_memory_id = Uuid::new_v4().simple().to_string();
    let insert_sql = format!(
        "INSERT INTO {branch_table} \
            (memory_id, user_id, memory_type, content, embedding, session_id, \
             source_event_ids, extra_metadata, is_active, superseded_by, \
             trust_tier, initial_confidence, observed_at, created_at, updated_at) \
         SELECT ?, user_id, memory_type, ?, embedding, session_id, \
                source_event_ids, extra_metadata, is_active, superseded_by, \
                trust_tier, initial_confidence, observed_at, created_at, updated_at \
         FROM mem_memories WHERE memory_id = ?"
    );
    sqlx::query(&insert_sql)
        .bind(&branch_memory_id)
        .bind(replacement)
        .bind(&original.memory_id)
        .execute(&pool)
        .await
        .expect("insert conflicting branch row");

    let r = gc(
        "memory_merge",
        json!({"source": branch, "strategy": "replace"}),
        &git,
        &svc,
        &uid,
    )
    .await;
    assert!(
        text(&r).contains("1 replaced"),
        "expected one replacement, got: {}",
        text(&r)
    );

    let active = svc.list_active(&uid, 10).await.unwrap();
    assert!(
        active.iter().any(|memory| memory.content == replacement),
        "replacement content should be visible in main"
    );
    let row = sqlx::query("SELECT content FROM mem_memories WHERE memory_id = ?")
        .bind(&original.memory_id)
        .fetch_one(&pool)
        .await
        .expect("main row after replace");
    assert_eq!(
        row.try_get::<String, _>("content").unwrap(),
        replacement,
        "main row should be updated without NULL content"
    );

    gc(
        "memory_branch_delete",
        json!({"name": branch}),
        &git,
        &svc,
        &uid,
    )
    .await;
}

// ── 2. memory_diff shows new/modified before merge ────────────────────────────

#[tokio::test]
async fn test_diff_before_merge() {
    let (svc, git, uid) = setup().await;
    let branch = bname("diff");

    store_mem("shared memory", &svc, &uid).await;
    gc("memory_branch", json!({"name": branch}), &git, &svc, &uid).await;
    gc("memory_checkout", json!({"name": branch}), &git, &svc, &uid).await;
    store_mem("branch-only A", &svc, &uid).await;
    store_mem("branch-only B", &svc, &uid).await;
    gc("memory_checkout", json!({"name": "main"}), &git, &svc, &uid).await;

    let r = gc("memory_diff", json!({"source": branch}), &git, &svc, &uid).await;
    let t = text(&r);
    assert!(t.contains("[new]"), "expected '[new]' entries, got: {t}");
    assert!(
        t.contains("branch-only A") && t.contains("branch-only B"),
        "both branch memories should appear in diff, got: {t}"
    );
    println!("✅ diff: {t}");

    gc(
        "memory_branch_delete",
        json!({"name": branch}),
        &git,
        &svc,
        &uid,
    )
    .await;
}

// ── 3. diff on branch with no changes returns "No changes" ───────────────────

#[tokio::test]
async fn test_diff_no_changes() {
    let (svc, git, uid) = setup().await;
    let branch = bname("nochange");

    store_mem("existing memory", &svc, &uid).await;
    gc("memory_branch", json!({"name": branch}), &git, &svc, &uid).await;
    // Don't add anything on branch

    let r = gc("memory_diff", json!({"source": branch}), &git, &svc, &uid).await;
    assert!(text(&r).contains("No changes"), "got: {}", text(&r));
    println!("✅ diff no changes: {}", text(&r));

    gc(
        "memory_branch_delete",
        json!({"name": branch}),
        &git,
        &svc,
        &uid,
    )
    .await;
}

// ── 4. Cannot delete main ─────────────────────────────────────────────────────

#[tokio::test]
async fn test_cannot_delete_main() {
    let (svc, git, uid) = setup().await;
    let r = gc(
        "memory_branch_delete",
        json!({"name": "main"}),
        &git,
        &svc,
        &uid,
    )
    .await;
    assert!(text(&r).contains("Cannot delete main"), "got: {}", text(&r));
    println!("✅ cannot delete main: {}", text(&r));
}

// ── 5. Delete nonexistent branch returns error message ───────────────────────

#[tokio::test]
async fn test_delete_nonexistent_branch() {
    let (svc, git, uid) = setup().await;
    let r = gc(
        "memory_branch_delete",
        json!({"name": "no_such_branch_xyz"}),
        &git,
        &svc,
        &uid,
    )
    .await;
    assert!(text(&r).contains("not found"), "got: {}", text(&r));
    println!("✅ delete nonexistent: {}", text(&r));
}

// ── 6. Checkout nonexistent branch returns error ──────────────────────────────

#[tokio::test]
async fn test_checkout_nonexistent_branch() {
    let (svc, git, uid) = setup().await;
    let result = memoria_mcp::git_tools::call(
        "memory_checkout",
        json!({"name": "ghost_branch"}),
        &git,
        &svc,
        &uid,
    )
    .await;
    assert!(result.is_err(), "should error on missing branch");
    println!("✅ checkout nonexistent → error");
}

#[tokio::test]
async fn test_merge_unknown_strategy_rejected() {
    let (svc, git, uid) = setup().await;
    let branch = bname("badmerge");

    store_mem("main memory", &svc, &uid).await;
    gc("memory_branch", json!({"name": branch}), &git, &svc, &uid).await;

    let result = memoria_mcp::git_tools::call(
        "memory_merge",
        json!({"source": branch, "strategy": "bogus"}),
        &git,
        &svc,
        &uid,
    )
    .await;
    assert!(result.is_err(), "unknown merge strategy should error");
    println!("✅ unknown merge strategy rejected");

    gc(
        "memory_branch_delete",
        json!({"name": branch}),
        &git,
        &svc,
        &uid,
    )
    .await;
}

// ── 7. Duplicate branch name rejected (even after delete) ────────────────────

#[tokio::test]
async fn test_duplicate_branch_name_rejected() {
    let (svc, git, uid) = setup().await;
    let branch = bname("dup");

    gc("memory_branch", json!({"name": branch}), &git, &svc, &uid).await;

    // Same name again → rejected
    let r = gc("memory_branch", json!({"name": branch}), &git, &svc, &uid).await;
    assert!(text(&r).contains("already exists"), "got: {}", text(&r));
    println!("✅ duplicate rejected: {}", text(&r));

    // Delete then try same name → still rejected (soft-deleted)
    gc(
        "memory_branch_delete",
        json!({"name": branch}),
        &git,
        &svc,
        &uid,
    )
    .await;
    let r2 = gc("memory_branch", json!({"name": branch}), &git, &svc, &uid).await;
    assert!(
        text(&r2).contains("already exists"),
        "should reject reuse of deleted name, got: {}",
        text(&r2)
    );
    println!("✅ deleted name reuse rejected: {}", text(&r2));
}

// ── 8. from_snapshot + from_timestamp mutual exclusion ───────────────────────

#[tokio::test]
async fn test_branch_from_snapshot_and_timestamp_exclusive() {
    let (svc, git, uid) = setup().await;
    let r = gc(
        "memory_branch",
        json!({"name": "x", "from_snapshot": "snap1", "from_timestamp": "2026-01-01 00:00:00"}),
        &git,
        &svc,
        &uid,
    )
    .await;
    assert!(text(&r).contains("not both"), "got: {}", text(&r));
    println!("✅ mutual exclusion: {}", text(&r));
}

// ── 9. from_timestamp future rejected ────────────────────────────────────────

#[tokio::test]
async fn test_branch_from_timestamp_future_rejected() {
    let (svc, git, uid) = setup().await;
    let r = gc(
        "memory_branch",
        json!({"name": bname("fut"), "from_timestamp": "2099-01-01 00:00:00"}),
        &git,
        &svc,
        &uid,
    )
    .await;
    assert!(text(&r).contains("future"), "got: {}", text(&r));
    println!("✅ future timestamp rejected: {}", text(&r));
}

// ── 10. from_timestamp too old rejected ──────────────────────────────────────

#[tokio::test]
async fn test_branch_from_timestamp_too_old_rejected() {
    let (svc, git, uid) = setup().await;
    let r = gc(
        "memory_branch",
        json!({"name": bname("old"), "from_timestamp": "2020-01-01 00:00:00"}),
        &git,
        &svc,
        &uid,
    )
    .await;
    assert!(text(&r).contains("30 minutes"), "got: {}", text(&r));
    println!("✅ too-old timestamp rejected: {}", text(&r));
}

// ── 11. from_timestamp invalid format rejected ────────────────────────────────

#[tokio::test]
async fn test_branch_from_timestamp_invalid_format() {
    let (svc, git, uid) = setup().await;
    let result = memoria_mcp::git_tools::call(
        "memory_branch",
        json!({"name": bname("fmt"), "from_timestamp": "not-a-date"}),
        &git,
        &svc,
        &uid,
    )
    .await;
    assert!(result.is_err(), "invalid format should error");
    println!("✅ invalid timestamp format → error");
}

// ── 12. Active branch resets to main after branch delete ─────────────────────

#[tokio::test]
async fn test_active_branch_resets_on_delete() {
    let (svc, git, uid) = setup().await;
    let branch = bname("reset");

    gc("memory_branch", json!({"name": branch}), &git, &svc, &uid).await;
    gc("memory_checkout", json!({"name": branch}), &git, &svc, &uid).await;

    // Verify we're on branch
    let sql = svc.sql_store.as_ref().unwrap();
    let active = sql.active_table(&uid).await.unwrap();
    assert_ne!(active, "mem_memories", "should be on branch table");

    // Delete branch while checked out
    gc(
        "memory_branch_delete",
        json!({"name": branch}),
        &git,
        &svc,
        &uid,
    )
    .await;

    // Should auto-reset to main
    let active = sql.active_table(&uid).await.unwrap();
    assert_eq!(
        active, "mem_memories",
        "should reset to main after branch delete"
    );
    println!("✅ active branch reset to main after delete");
}

// ── 13. Multi-user: branches are per-user ────────────────────────────────────

#[tokio::test]
async fn test_multiuser_branch_isolation() {
    let (svc, git, _) = setup().await;
    let uid_a = uid();
    let uid_b = uid();
    let branch_a = bname("ua");
    let branch_b = bname("ub");

    // User A creates branch and stores memory
    store_mem("A main", &svc, &uid_a).await;
    gc(
        "memory_branch",
        json!({"name": branch_a}),
        &git,
        &svc,
        &uid_a,
    )
    .await;
    gc(
        "memory_checkout",
        json!({"name": branch_a}),
        &git,
        &svc,
        &uid_a,
    )
    .await;
    store_mem("A branch memory", &svc, &uid_a).await;

    // User B creates their own branch
    store_mem("B main", &svc, &uid_b).await;
    gc(
        "memory_branch",
        json!({"name": branch_b}),
        &git,
        &svc,
        &uid_b,
    )
    .await;
    gc(
        "memory_checkout",
        json!({"name": branch_b}),
        &git,
        &svc,
        &uid_b,
    )
    .await;
    store_mem("B branch memory", &svc, &uid_b).await;

    // A's branch list should not contain B's branch
    let r_a = gc("memory_branches", json!({}), &git, &svc, &uid_a).await;
    assert!(text(&r_a).contains(&branch_a), "A should see own branch");
    assert!(
        !text(&r_a).contains(&branch_b),
        "A should not see B's branch"
    );

    // B's memories should not appear in A's retrieval
    let a_mems = svc.list_active(&uid_a, 10).await.unwrap();
    assert!(!a_mems.iter().any(|m| m.content == "B branch memory"));
    println!(
        "✅ multiuser isolation: A sees {} memories, B's data not visible",
        a_mems.len()
    );

    // Cleanup
    gc(
        "memory_checkout",
        json!({"name": "main"}),
        &git,
        &svc,
        &uid_a,
    )
    .await;
    gc(
        "memory_checkout",
        json!({"name": "main"}),
        &git,
        &svc,
        &uid_b,
    )
    .await;
    gc(
        "memory_branch_delete",
        json!({"name": branch_a}),
        &git,
        &svc,
        &uid_a,
    )
    .await;
    gc(
        "memory_branch_delete",
        json!({"name": branch_b}),
        &git,
        &svc,
        &uid_b,
    )
    .await;
}

// ── 14. memory_branches shows active marker ───────────────────────────────────

#[tokio::test]
async fn test_branches_list_shows_active_marker() {
    let (svc, git, uid) = setup().await;
    let b1 = bname("list1");
    let b2 = bname("list2");

    gc("memory_branch", json!({"name": b1}), &git, &svc, &uid).await;
    gc("memory_branch", json!({"name": b2}), &git, &svc, &uid).await;
    gc("memory_checkout", json!({"name": b1}), &git, &svc, &uid).await;

    let r = gc("memory_branches", json!({}), &git, &svc, &uid).await;
    let t = text(&r);
    assert!(t.contains(&b1), "b1 should appear");
    assert!(t.contains(&b2), "b2 should appear");
    assert!(t.contains("← active"), "active marker should appear");
    // b1 should be marked active, b2 should not
    let b1_line = t.lines().find(|l| l.contains(&b1)).unwrap_or("");
    assert!(
        b1_line.contains("← active"),
        "b1 should be active, got: {b1_line}"
    );
    println!("✅ branches list with active marker:\n{t}");

    gc("memory_checkout", json!({"name": "main"}), &git, &svc, &uid).await;
    gc(
        "memory_branch_delete",
        json!({"name": b1}),
        &git,
        &svc,
        &uid,
    )
    .await;
    gc(
        "memory_branch_delete",
        json!({"name": b2}),
        &git,
        &svc,
        &uid,
    )
    .await;
}

// ── 15. Merge idempotent: second merge adds 0 new memories ───────────────────

#[tokio::test]
async fn test_merge_idempotent() {
    let (svc, git, uid) = setup().await;
    let branch = bname("idem");

    gc("memory_branch", json!({"name": branch}), &git, &svc, &uid).await;
    gc("memory_checkout", json!({"name": branch}), &git, &svc, &uid).await;
    store_mem("idempotent memory", &svc, &uid).await;
    gc("memory_checkout", json!({"name": "main"}), &git, &svc, &uid).await;

    let r1 = gc("memory_merge", json!({"source": branch}), &git, &svc, &uid).await;
    assert!(text(&r1).contains("1 new"), "first merge: {}", text(&r1));

    let r2 = gc("memory_merge", json!({"source": branch}), &git, &svc, &uid).await;
    assert!(
        text(&r2).contains("0 new"),
        "second merge should be 0: {}",
        text(&r2)
    );
    println!("✅ merge idempotent: {}", text(&r2));

    gc(
        "memory_branch_delete",
        json!({"name": branch}),
        &git,
        &svc,
        &uid,
    )
    .await;
}

// ── 16. Merge nonexistent branch errors ──────────────────────────────────────

#[tokio::test]
async fn test_merge_nonexistent_branch_errors() {
    let (svc, git, uid) = setup().await;
    let result = memoria_mcp::git_tools::call(
        "memory_merge",
        json!({"source": "ghost_branch_xyz"}),
        &git,
        &svc,
        &uid,
    )
    .await;
    assert!(result.is_err(), "should error");
    println!("✅ merge nonexistent → error");
}

// ── 17. diff on nonexistent branch errors ────────────────────────────────────

#[tokio::test]
async fn test_diff_nonexistent_branch_errors() {
    let (svc, git, uid) = setup().await;
    let result = memoria_mcp::git_tools::call(
        "memory_diff",
        json!({"source": "ghost_xyz"}),
        &git,
        &svc,
        &uid,
    )
    .await;
    assert!(result.is_err(), "should error");
    println!("✅ diff nonexistent → error");
}

// ── 18. Branch name sanitization (special chars) ─────────────────────────────

#[tokio::test]
async fn test_branch_name_sanitization() {
    let (svc, git, uid) = setup().await;
    // Name with spaces/dashes — should be sanitized and created successfully
    let r = gc(
        "memory_branch",
        json!({"name": "my-branch name!"}),
        &git,
        &svc,
        &uid,
    )
    .await;
    // Either created or rejected — must not panic
    println!("✅ special chars branch name: {}", text(&r));
    // Cleanup if created
    gc(
        "memory_branch_delete",
        json!({"name": "my-branch name!"}),
        &git,
        &svc,
        &uid,
    )
    .await;
}

// ── 19. diff validates all returned fields ────────────────────────────────────

#[tokio::test]
async fn test_diff_fields_complete() {
    let (svc, git, uid) = setup().await;
    let branch = bname("fields");

    store_mem("existing on main", &svc, &uid).await;
    gc("memory_branch", json!({"name": branch}), &git, &svc, &uid).await;
    gc("memory_checkout", json!({"name": branch}), &git, &svc, &uid).await;

    // Store with specific memory_type
    memoria_mcp::tools::call(
        "memory_store",
        json!({"content": "profile memory on branch", "memory_type": "profile"}),
        &svc,
        &uid,
    )
    .await
    .expect("store");

    gc("memory_checkout", json!({"name": "main"}), &git, &svc, &uid).await;

    // Use GitForDataService directly to check DiffRow fields
    let sql = svc.sql_store.as_ref().unwrap();
    let branches = sql.list_branches(&uid).await.unwrap();
    let table = branches
        .iter()
        .find(|(n, _)| n == &branch)
        .map(|(_, t)| t.clone())
        .unwrap();

    let rows = git
        .diff_branch_rows(&table, "mem_memories", &uid, 50)
        .await
        .unwrap();
    // data branch diff is account-level (no user_id filter), find our specific row
    let our_row = rows
        .iter()
        .find(|r| r.content == "profile memory on branch")
        .expect("our branch memory should appear in diff");
    assert_eq!(our_row.flag, "INSERT", "new memory should be INSERT");
    assert!(!our_row.memory_id.is_empty(), "memory_id must not be empty");
    assert_eq!(our_row.memory_type, "profile");
    println!(
        "✅ diff fields: flag={}, memory_type={}, content={}",
        our_row.flag, our_row.memory_type, our_row.content
    );

    // Native count >= 1 (may include other users' changes)
    let count = git
        .diff_branch_count(&table, "mem_memories", &uid)
        .await
        .unwrap();
    assert!(count >= 1, "native diff count should be at least 1");
    println!("✅ native diff count = {count}");

    gc(
        "memory_branch_delete",
        json!({"name": branch}),
        &git,
        &svc,
        &uid,
    )
    .await;
}

// ── 20. diff limit truncation ─────────────────────────────────────────────────

#[tokio::test]
async fn test_diff_limit_truncation() {
    let (svc, git, uid) = setup().await;
    let branch = bname("trunc");

    gc("memory_branch", json!({"name": branch}), &git, &svc, &uid).await;
    gc("memory_checkout", json!({"name": branch}), &git, &svc, &uid).await;
    for i in 0..5 {
        store_mem(&format!("branch memory {i}"), &svc, &uid).await;
    }
    gc("memory_checkout", json!({"name": "main"}), &git, &svc, &uid).await;

    // limit=2 should truncate
    let r = gc(
        "memory_diff",
        json!({"source": branch, "limit": 2}),
        &git,
        &svc,
        &uid,
    )
    .await;
    let t = text(&r);
    assert!(
        t.contains("showing 2/5") || t.contains("showing 2/"),
        "expected truncation note, got: {t}"
    );
    println!("✅ diff truncation: {t}");

    gc(
        "memory_branch_delete",
        json!({"name": branch}),
        &git,
        &svc,
        &uid,
    )
    .await;
}

// ── 21. diff count (native LCA) vs rows (native LCA) consistency ─────────────
// Both use native `data branch diff`. The sqlx patch maps MatrixOne's 0xf1 JSON
// type code to Blob, allowing result set decoding.
// count uses `output count`, rows use `output limit N`.

#[tokio::test]
async fn test_diff_native_count_vs_join_rows() {
    let (svc, git, uid) = setup().await;
    let branch = bname("countvsrows");

    // Store 2 memories on main
    store_mem("memory X", &svc, &uid).await;
    store_mem("memory Y", &svc, &uid).await;

    // Create branch (inherits both)
    gc("memory_branch", json!({"name": branch}), &git, &svc, &uid).await;
    gc("memory_checkout", json!({"name": branch}), &git, &svc, &uid).await;

    // Add 1 new memory on branch
    store_mem("memory Z (branch only)", &svc, &uid).await;

    gc("memory_checkout", json!({"name": "main"}), &git, &svc, &uid).await;

    let sql = svc.sql_store.as_ref().unwrap();
    let branches = sql.list_branches(&uid).await.unwrap();
    let table = branches
        .iter()
        .find(|(n, _)| n == &branch)
        .map(|(_, t)| t.clone())
        .unwrap();

    // Native count: >= 1 (account-level, may include other users)
    let count = git
        .diff_branch_count(&table, "mem_memories", &uid)
        .await
        .unwrap();
    assert!(count >= 1, "native LCA diff count should be at least 1");

    // Find our specific row
    let rows = git
        .diff_branch_rows(&table, "mem_memories", &uid, 50)
        .await
        .unwrap();
    let our_row = rows
        .iter()
        .find(|r| r.content == "memory Z (branch only)")
        .expect("our branch memory should appear in diff");
    assert_eq!(our_row.flag, "INSERT");

    println!("✅ native count={count}, found our INSERT row — native LCA diff works");
    println!("ℹ️  diff is account-level (no user_id filter), count includes all users' changes");

    gc(
        "memory_branch_delete",
        json!({"name": branch}),
        &git,
        &svc,
        &uid,
    )
    .await;
}

// ── 22. correct on branch does NOT affect main ───────────────────────────────

#[tokio::test]
async fn test_correct_on_branch_isolated_from_main() {
    let (svc, git, uid) = setup().await;
    let branch = bname("correctiso");

    // Store on main
    store_mem("original fact", &svc, &uid).await;
    let main_mems = svc.list_active(&uid, 10).await.unwrap();
    assert_eq!(main_mems.len(), 1);
    let original_id = main_mems[0].memory_id.clone();

    // Create branch (inherits the memory)
    gc("memory_branch", json!({"name": branch}), &git, &svc, &uid).await;
    gc("memory_checkout", json!({"name": branch}), &git, &svc, &uid).await;

    // Correct on branch
    let r = memoria_mcp::tools::call(
        "memory_correct",
        json!({"memory_id": original_id, "new_content": "corrected on branch"}),
        &svc,
        &uid,
    )
    .await
    .expect("correct on branch");
    let t = text(&r);
    assert!(t.contains("corrected on branch"), "correct response: {t}");

    // Branch should show corrected content
    let branch_mems = svc.list_active(&uid, 10).await.unwrap();
    assert!(
        branch_mems
            .iter()
            .any(|m| m.content == "corrected on branch"),
        "branch should have corrected memory"
    );
    assert!(
        !branch_mems
            .iter()
            .any(|m| m.content == "original fact" && m.is_active),
        "original should be deactivated on branch"
    );

    // Switch to main — original should still be there, untouched
    gc("memory_checkout", json!({"name": "main"}), &git, &svc, &uid).await;
    let main_mems = svc.list_active(&uid, 10).await.unwrap();
    assert!(
        main_mems.iter().any(|m| m.content == "original fact"),
        "main should still have original fact, got: {:?}",
        main_mems.iter().map(|m| &m.content).collect::<Vec<_>>()
    );
    assert!(
        !main_mems.iter().any(|m| m.content == "corrected on branch"),
        "main should NOT have branch correction"
    );
    println!("✅ correct on branch isolated from main");

    gc("memory_checkout", json!({"name": "main"}), &git, &svc, &uid).await;
    gc(
        "memory_branch_delete",
        json!({"name": branch}),
        &git,
        &svc,
        &uid,
    )
    .await;
}

// ── 23. purge on branch does NOT affect main ─────────────────────────────────

#[tokio::test]
async fn test_purge_on_branch_isolated_from_main() {
    let (svc, git, uid) = setup().await;
    let branch = bname("purgeiso");

    // Store on main
    store_mem("keep this memory", &svc, &uid).await;
    let main_mems = svc.list_active(&uid, 10).await.unwrap();
    assert_eq!(main_mems.len(), 1);
    let mid = main_mems[0].memory_id.clone();

    // Create branch, checkout
    gc("memory_branch", json!({"name": branch}), &git, &svc, &uid).await;
    gc("memory_checkout", json!({"name": branch}), &git, &svc, &uid).await;

    // Purge on branch
    memoria_mcp::tools::call("memory_purge", json!({"memory_id": mid}), &svc, &uid)
        .await
        .expect("purge on branch");

    // Branch should be empty
    let branch_mems = svc.list_active(&uid, 10).await.unwrap();
    assert!(
        !branch_mems.iter().any(|m| m.content == "keep this memory"),
        "memory should be purged on branch"
    );

    // Switch to main — memory should still be there
    gc("memory_checkout", json!({"name": "main"}), &git, &svc, &uid).await;
    let main_mems = svc.list_active(&uid, 10).await.unwrap();
    assert!(
        main_mems.iter().any(|m| m.content == "keep this memory"),
        "main should still have the memory, got: {:?}",
        main_mems.iter().map(|m| &m.content).collect::<Vec<_>>()
    );
    println!("✅ purge on branch isolated from main");

    gc(
        "memory_branch_delete",
        json!({"name": branch}),
        &git,
        &svc,
        &uid,
    )
    .await;
}

// ── 24. purge_by_topic on branch does NOT affect main ────────────────────────

#[tokio::test]
async fn test_purge_by_topic_on_branch_isolated_from_main() {
    let (svc, git, uid) = setup().await;
    let branch = bname("topiciso");

    // Store on main
    store_mem("important database config", &svc, &uid).await;
    store_mem("important cache config", &svc, &uid).await;
    assert_eq!(svc.list_active(&uid, 10).await.unwrap().len(), 2);

    // Create branch, checkout
    gc("memory_branch", json!({"name": branch}), &git, &svc, &uid).await;
    gc("memory_checkout", json!({"name": branch}), &git, &svc, &uid).await;

    // Purge by topic on branch
    memoria_mcp::tools::call(
        "memory_purge",
        json!({"topic": "database config"}),
        &svc,
        &uid,
    )
    .await
    .expect("purge by topic on branch");

    // Branch should have only cache config
    let branch_mems = svc.list_active(&uid, 10).await.unwrap();
    assert!(
        !branch_mems.iter().any(|m| m.content.contains("database")),
        "database config should be purged on branch"
    );

    // Switch to main — both should still be there
    gc("memory_checkout", json!({"name": "main"}), &git, &svc, &uid).await;
    let main_mems = svc.list_active(&uid, 10).await.unwrap();
    assert_eq!(
        main_mems.len(),
        2,
        "main should still have both memories, got: {:?}",
        main_mems.iter().map(|m| &m.content).collect::<Vec<_>>()
    );
    println!("✅ purge_by_topic on branch isolated from main");

    gc(
        "memory_branch_delete",
        json!({"name": branch}),
        &git,
        &svc,
        &uid,
    )
    .await;
}

// ── 25. purge_batch on branch does NOT affect main ───────────────────────────

#[tokio::test]
async fn test_purge_batch_on_branch_isolated_from_main() {
    let (svc, git, uid) = setup().await;
    let branch = bname("batchiso");

    // Store two memories on main
    store_mem("batch keep alpha", &svc, &uid).await;
    store_mem("batch keep beta", &svc, &uid).await;
    let main_mems = svc.list_active(&uid, 10).await.unwrap();
    assert_eq!(main_mems.len(), 2);
    let ids: Vec<String> = main_mems.iter().map(|m| m.memory_id.clone()).collect();

    // Create branch, checkout
    gc("memory_branch", json!({"name": branch}), &git, &svc, &uid).await;
    gc("memory_checkout", json!({"name": branch}), &git, &svc, &uid).await;

    // Purge batch on branch (comma-separated IDs)
    let batch = format!("{},{}", ids[0], ids[1]);
    memoria_mcp::tools::call("memory_purge", json!({"memory_id": batch}), &svc, &uid)
        .await
        .expect("purge batch on branch");

    // Branch should be empty
    let branch_mems = svc.list_active(&uid, 10).await.unwrap();
    assert_eq!(
        branch_mems.len(),
        0,
        "branch should have 0 memories after batch purge"
    );

    // Switch to main — both should still be there
    gc("memory_checkout", json!({"name": "main"}), &git, &svc, &uid).await;
    let main_mems = svc.list_active(&uid, 10).await.unwrap();
    assert_eq!(
        main_mems.len(),
        2,
        "main should still have both memories, got: {:?}",
        main_mems.iter().map(|m| &m.content).collect::<Vec<_>>()
    );
    println!("✅ purge_batch on branch isolated from main");

    gc(
        "memory_branch_delete",
        json!({"name": branch}),
        &git,
        &svc,
        &uid,
    )
    .await;
}

// ── 26. correct with sensitive content is blocked ────────────────────────────

#[tokio::test]
async fn test_correct_blocks_sensitive_content() {
    let (svc, _git, uid) = setup().await;

    // Store a normal memory
    store_mem("database config info", &svc, &uid).await;
    let mems = svc.list_active(&uid, 10).await.unwrap();
    let mid = mems[0].memory_id.clone();

    // Try to correct with sensitive content (password pattern → HIGH tier → blocked)
    let result = memoria_mcp::tools::call(
        "memory_correct",
        json!({"memory_id": mid, "new_content": "password=supersecret123"}),
        &svc,
        &uid,
    )
    .await;

    // Should fail with Blocked error
    assert!(
        result.is_err(),
        "correct with sensitive content should be blocked"
    );
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("sensitive") || err.contains("Blocked"),
        "error should mention sensitive/blocked, got: {err}"
    );

    // Original memory should be untouched
    let mems = svc.list_active(&uid, 10).await.unwrap();
    assert_eq!(mems.len(), 1);
    assert_eq!(mems[0].content, "database config info");
    println!("✅ correct blocks sensitive content");
}

// ── 27. correct triggers entity extraction ───────────────────────────────────

#[tokio::test]
async fn test_correct_triggers_entity_extraction() {
    let (svc, _git, uid) = setup().await;

    // Store a memory with an entity
    store_mem("Project uses PostgreSQL database", &svc, &uid).await;
    // Wait for async entity extraction from store
    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

    let mems = svc.list_active(&uid, 10).await.unwrap();
    let mid = mems[0].memory_id.clone();

    // Correct it to mention a different entity
    let r = memoria_mcp::tools::call(
        "memory_correct",
        json!({"memory_id": mid, "new_content": "Project uses MatrixOne and Redis"}),
        &svc,
        &uid,
    )
    .await
    .expect("correct");
    let t = text(&r);
    assert!(t.contains("MatrixOne"), "corrected content: {t}");

    let new_mid = t
        .split_whitespace()
        .nth(2)
        .unwrap_or("")
        .trim_end_matches(':');

    // Wait for async entity extraction worker to process the corrected memory.
    // The worker writes to mem_memory_entity_links (graph table).
    let sql = svc.sql_store.as_ref().expect("sql_store");
    let graph = sql.graph_store();
    let mut found = false;
    for _ in 0..10 {
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
        let unlinked = graph
            .get_unlinked_memories(&uid, 100)
            .await
            .unwrap_or_default();
        // If new_mid is NOT in unlinked, it means entity links were created
        if !unlinked.iter().any(|(m, _)| m == new_mid) {
            found = true;
            break;
        }
    }
    assert!(
        found,
        "corrected memory {new_mid} should have entity links within 5s"
    );
    println!("✅ correct triggers entity extraction: new memory {new_mid} is entity-linked");
}

// ── 28. REST API get/correct/delete work on branch-only memories ─────────────
// This tests that the permission check uses get_for_user (branch-aware)
// rather than get (main-only).

#[tokio::test]
async fn test_get_for_user_finds_branch_only_memory() {
    let (svc, git, uid) = setup().await;
    let branch = bname("getuser");

    // Create branch, checkout, store a branch-only memory
    gc("memory_branch", json!({"name": branch}), &git, &svc, &uid).await;
    gc("memory_checkout", json!({"name": branch}), &git, &svc, &uid).await;
    store_mem("branch-only secret", &svc, &uid).await;

    let branch_mems = svc.list_active(&uid, 10).await.unwrap();
    let branch_mid = branch_mems
        .iter()
        .find(|m| m.content == "branch-only secret")
        .expect("branch memory should exist")
        .memory_id
        .clone();

    // get_for_user should find it (branch-aware)
    let found = svc.get_for_user(&uid, &branch_mid).await.unwrap();
    assert!(
        found.is_some(),
        "get_for_user should find branch-only memory"
    );
    assert_eq!(found.unwrap().content, "branch-only secret");

    // plain get() should NOT find it (hardcoded to mem_memories)
    let not_found = svc.get(&branch_mid).await.unwrap();
    assert!(
        not_found.is_none(),
        "plain get() should not find branch-only memory"
    );

    println!("✅ get_for_user finds branch-only memory, get() does not");

    gc("memory_checkout", json!({"name": "main"}), &git, &svc, &uid).await;
    gc(
        "memory_branch_delete",
        json!({"name": branch}),
        &git,
        &svc,
        &uid,
    )
    .await;
}

// ── 29. Micro-batch entity extraction: burst writes are batched correctly ────

#[tokio::test]
async fn test_micro_batch_entity_extraction() {
    let (svc, _git, uid) = setup().await;

    // Burst-write 10 memories with distinct entities — should trigger micro-batching
    let contents = [
        "Using Rust for backend",
        "PostgreSQL is the database",
        "Redis for caching",
        "Docker for containers",
        "Kubernetes orchestration",
        "GitHub for version control",
        "Python for scripts",
        "TypeScript frontend",
        "MatrixOne analytics",
        "Tokio async runtime",
    ];
    for content in &contents {
        store_mem(content, &svc, &uid).await;
    }

    // Wait for async entity extraction (micro-batch should process all)
    tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;

    // Verify all memories have entity links
    let sql = svc.sql_store.as_ref().expect("sql_store");
    let graph = sql.graph_store();
    let unlinked = graph
        .get_unlinked_memories(&uid, 100)
        .await
        .unwrap_or_default();

    // All 10 memories should have at least one entity extracted
    let mems = svc.list_active(&uid, 20).await.unwrap();
    assert_eq!(mems.len(), 10, "should have 10 memories");

    let linked_count = mems
        .iter()
        .filter(|m| !unlinked.iter().any(|(mid, _)| mid == &m.memory_id))
        .count();
    assert!(
        linked_count >= 8,
        "at least 8/10 memories should have entity links (got {linked_count})"
    );
    println!("✅ micro-batch entity extraction: {linked_count}/10 memories linked");
}

// ── 30. Batch upsert entities with duplicates across jobs ────────────────────

#[tokio::test]
async fn test_batch_entity_deduplication_across_memories() {
    let (svc, _git, uid) = setup().await;

    // Multiple memories mention the same entity — should deduplicate in batch
    store_mem("Rust is great for systems programming", &svc, &uid).await;
    store_mem("I love Rust for its safety", &svc, &uid).await;
    store_mem("Rust and Go are both compiled", &svc, &uid).await;

    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

    // Check that "rust" entity exists only once
    let sql = svc.sql_store.as_ref().expect("sql_store");
    let graph = sql.graph_store();
    let entities = graph.get_user_entities(&uid).await.unwrap();

    let rust_count = entities.iter().filter(|(name, _)| name == "rust").count();
    assert_eq!(rust_count, 1, "rust entity should exist exactly once");

    // All 3 memories should link to the same rust entity
    let mems = svc.list_active(&uid, 10).await.unwrap();
    assert_eq!(mems.len(), 3);

    // Verify links exist (not unlinked)
    let unlinked = graph
        .get_unlinked_memories(&uid, 100)
        .await
        .unwrap_or_default();
    let linked = mems
        .iter()
        .filter(|m| !unlinked.iter().any(|(mid, _)| mid == &m.memory_id))
        .count();
    assert!(
        linked >= 2,
        "at least 2/3 memories should be linked to rust entity"
    );
    println!(
        "✅ batch entity deduplication: rust entity created once, linked to multiple memories"
    );
}
