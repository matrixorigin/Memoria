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
use sqlx::mysql::MySqlPool;
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
    let svc = Arc::new(MemoryService::new_sql_with_llm(Arc::new(store), None, None));
    (svc, git, uid())
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
