/// End-to-end tests for mem_edit_log audit trail.
/// Verifies that every mutation (inject, correct, purge, governance) writes
/// the correct audit record, and that purge creates safety snapshots.
///
/// Run: DATABASE_URL=mysql://root:111@localhost:6001/memoria \
///      cargo test -p memoria-mcp --test edit_log_e2e -- --nocapture
use memoria_git::GitForDataService;
use memoria_service::MemoryService;
use memoria_storage::SqlMemoryStore;
use serde_json::{json, Value};
use serial_test::serial;
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
    format!("elog_{}", &Uuid::new_v4().simple().to_string()[..8])
}

async fn setup() -> (
    Arc<MemoryService>,
    Arc<GitForDataService>,
    MySqlPool,
    String,
) {
    let pool = MySqlPool::connect(&db_url()).await.expect("pool");
    let db_name = db_url().rsplit('/').next().unwrap_or("memoria").to_string();
    let store = SqlMemoryStore::connect(&db_url(), test_dim())
        .await
        .expect("store");
    store.migrate().await.expect("migrate");
    let git = Arc::new(GitForDataService::new(pool.clone(), &db_name));
    let svc = Arc::new(MemoryService::new_sql_with_llm(Arc::new(store), None, None));
    (svc, git, pool, uid())
}

async fn call(name: &str, args: Value, svc: &Arc<MemoryService>, uid: &str) -> Value {
    memoria_mcp::tools::call(name, args, svc, uid)
        .await
        .expect(name)
}
fn text(v: &Value) -> &str {
    v["content"][0]["text"].as_str().unwrap_or("")
}

/// Get edit log entries for a user, ordered by created_at desc.
async fn get_edit_logs(pool: &MySqlPool, user_id: &str) -> Vec<EditLogRow> {
    sqlx::query_as::<_, EditLogRow>(
        "SELECT operation, memory_id, CAST(payload AS CHAR) as payload, reason, snapshot_before \
         FROM mem_edit_log WHERE user_id = ? ORDER BY created_at DESC",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await
    .unwrap()
}

/// Get edit log entries filtered by operation.
async fn get_edit_logs_by_op(pool: &MySqlPool, user_id: &str, op: &str) -> Vec<EditLogRow> {
    sqlx::query_as::<_, EditLogRow>(
        "SELECT operation, memory_id, CAST(payload AS CHAR) as payload, reason, snapshot_before \
         FROM mem_edit_log WHERE user_id = ? AND operation = ? ORDER BY created_at DESC",
    )
    .bind(user_id)
    .bind(op)
    .fetch_all(pool)
    .await
    .unwrap()
}

// (operation, memory_id, payload, reason, snapshot_before)
type EditLogRow = (String, Option<String>, Option<String>, String, Option<String>);

/// Check if a snapshot exists by name.
async fn snapshot_exists(pool: &MySqlPool, name: &str) -> bool {
    let rows: Vec<(String,)> =
        sqlx::query_as("SELECT sname FROM mo_catalog.mo_snapshots WHERE sname = ?")
            .bind(name)
            .fetch_all(pool)
            .await
            .unwrap_or_default();
    !rows.is_empty()
}

/// Cleanup: remove test user's edit logs and snapshots.
async fn cleanup(pool: &MySqlPool, user_id: &str) {
    let _ = sqlx::query("DELETE FROM mem_edit_log WHERE user_id = ?")
        .bind(user_id)
        .execute(pool)
        .await;
    // Drop any pre_ snapshots created by this test
    let rows: Vec<(String,)> = sqlx::query_as(
        "SELECT sname FROM mo_catalog.mo_snapshots WHERE prefix_eq(sname, 'mem_snap_pre_')",
    )
    .fetch_all(pool)
    .await
    .unwrap_or_default();
    for (name,) in rows {
        let _ = sqlx::raw_sql(&format!("DROP SNAPSHOT {name}"))
            .execute(pool)
            .await;
    }
}

/// Ensure snapshot quota has room by dropping old test snapshots.
async fn ensure_snapshot_quota(pool: &MySqlPool) {
    // Drop ALL snapshots to free quota for tests
    let rows: Vec<(String,)> = sqlx::query_as("SELECT sname FROM mo_catalog.mo_snapshots")
        .fetch_all(pool)
        .await
        .unwrap_or_default();
    for (name,) in &rows {
        let _ = sqlx::raw_sql(&format!("DROP SNAPSHOT {name}"))
            .execute(pool)
            .await;
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// 1. INJECT — store_memory writes audit log
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
#[serial]
async fn test_inject_writes_edit_log() {
    let (svc, _git, pool, uid) = setup().await;
    cleanup(&pool, &uid).await;

    let r = call("memory_store", json!({"content": "test fact"}), &svc, &uid).await;
    let mid = text(&r)
        .split_whitespace()
        .nth(2)
        .unwrap()
        .trim_end_matches(':');

    let logs = get_edit_logs_by_op(&pool, &uid, "inject").await;
    assert!(!logs.is_empty(), "inject should write edit log");
    let (op, memory_id, payload, reason, _snap) = &logs[0];
    assert_eq!(op, "inject");
    assert_eq!(memory_id.as_deref(), Some(mid), "memory_id should match");
    assert!(payload.as_ref().map_or(false, |p| p.contains("test fact")), "payload should contain content");
    assert!(reason.contains("store_memory"), "reason should mention store_memory: {reason}");

    cleanup(&pool, &uid).await;
    println!("✅ inject writes edit log with memory_id");
}

// ═══════════════════════════════════════════════════════════════════════════════
// 2. CORRECT — correct writes audit log with old + new IDs
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
#[serial]
async fn test_correct_writes_edit_log() {
    let (svc, _git, pool, uid) = setup().await;
    cleanup(&pool, &uid).await;

    let r = call("memory_store", json!({"content": "old fact"}), &svc, &uid).await;
    let old_mid = text(&r)
        .split_whitespace()
        .nth(2)
        .unwrap()
        .trim_end_matches(':')
        .to_string();

    let r = call(
        "memory_correct",
        json!({"memory_id": old_mid, "new_content": "corrected fact"}),
        &svc,
        &uid,
    )
    .await;
    let t = text(&r);
    assert!(t.contains("Corrected"), "{t}");

    let logs = get_edit_logs_by_op(&pool, &uid, "correct").await;
    assert!(!logs.is_empty(), "correct should write edit log");
    let (op, memory_id, payload, _reason, _snap) = &logs[0];
    assert_eq!(op, "correct");
    assert_eq!(memory_id.as_deref(), Some(old_mid.as_str()), "memory_id should be old_id");
    assert!(payload.as_ref().map_or(false, |p| p.contains("corrected fact")), "payload should contain new_content");
    assert!(payload.as_ref().map_or(false, |p| p.contains("new_memory_id")), "payload should contain new_memory_id");

    cleanup(&pool, &uid).await;
    println!("✅ correct writes edit log with memory_id and payload");
}

// ═══════════════════════════════════════════════════════════════════════════════
// 3. PURGE SINGLE — creates safety snapshot + writes audit log
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
#[serial]
async fn test_purge_single_creates_snapshot_and_edit_log() {
    let (svc, _git, pool, uid) = setup().await;
    cleanup(&pool, &uid).await;
    ensure_snapshot_quota(&pool).await;

    let r = call("memory_store", json!({"content": "to delete"}), &svc, &uid).await;
    let mid = text(&r)
        .split_whitespace()
        .nth(2)
        .unwrap()
        .trim_end_matches(':')
        .to_string();

    let r = call("memory_purge", json!({"memory_id": mid}), &svc, &uid).await;
    let t = text(&r);
    assert!(t.contains("Purged"), "{t}");
    assert!(
        t.contains("Safety snapshot"),
        "purge response should mention safety snapshot: {t}"
    );

    // Verify edit log
    let logs = get_edit_logs_by_op(&pool, &uid, "purge").await;
    assert!(!logs.is_empty(), "purge should write edit log");
    let (op, memory_id, _payload, _reason, snap_before) = &logs[0];
    assert_eq!(op, "purge");
    assert_eq!(memory_id.as_deref(), Some(mid.as_str()), "memory_id should match purged id");

    // Verify safety snapshot was created and recorded
    assert!(snap_before.is_some(), "snapshot_before should be set");
    let snap_name = snap_before.as_ref().unwrap();
    assert!(
        snap_name.starts_with("mem_snap_pre_purge_"),
        "snapshot name: {snap_name}"
    );
    assert!(
        snapshot_exists(&pool, snap_name).await,
        "snapshot should exist in DB"
    );

    cleanup(&pool, &uid).await;
    println!("✅ purge single: safety snapshot + edit log");
}

// ═══════════════════════════════════════════════════════════════════════════════
// 4. PURGE BATCH — single audit log entry for multiple IDs
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
#[serial]
async fn test_purge_batch_single_edit_log() {
    let (svc, _git, pool, uid) = setup().await;
    cleanup(&pool, &uid).await;
    ensure_snapshot_quota(&pool).await;

    let mut ids = vec![];
    for i in 0..3 {
        let r = call(
            "memory_store",
            json!({"content": format!("batch {i}")}),
            &svc,
            &uid,
        )
        .await;
        ids.push(
            text(&r)
                .split_whitespace()
                .nth(2)
                .unwrap()
                .trim_end_matches(':')
                .to_string(),
        );
    }

    let batch = ids.join(",");
    let r = call("memory_purge", json!({"memory_id": batch}), &svc, &uid).await;
    let t = text(&r);
    assert!(t.contains("3"), "{t}");
    assert!(
        t.contains("Safety snapshot"),
        "batch purge should mention snapshot: {t}"
    );

    // Should be 3 purge edit log entries (one per memory)
    let logs = get_edit_logs_by_op(&pool, &uid, "purge").await;
    assert_eq!(logs.len(), 3, "batch purge should produce 3 edit logs (one per memory), got {}", logs.len());
    let purged_ids: Vec<_> = logs.iter().filter_map(|(_, mid, _, _, _)| mid.clone()).collect();
    for id in &ids {
        assert!(purged_ids.contains(id), "purged_ids should contain {id}");
    }
    assert!(logs[0].4.is_some(), "snapshot_before should be set");

    cleanup(&pool, &uid).await;
    println!("✅ purge batch: 3 edit log entries (one per memory)");
}

// ═══════════════════════════════════════════════════════════════════════════════
// 5. PURGE BY TOPIC — audit log with topic reason
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
#[serial]
async fn test_purge_topic_edit_log() {
    let (svc, _git, pool, uid) = setup().await;
    cleanup(&pool, &uid).await;
    ensure_snapshot_quota(&pool).await;

    call(
        "memory_store",
        json!({"content": "rust ownership rules"}),
        &svc,
        &uid,
    )
    .await;
    call(
        "memory_store",
        json!({"content": "rust borrow checker"}),
        &svc,
        &uid,
    )
    .await;
    call(
        "memory_store",
        json!({"content": "python is great"}),
        &svc,
        &uid,
    )
    .await;

    let r = call("memory_purge", json!({"topic": "rust"}), &svc, &uid).await;
    let t = text(&r);
    assert!(t.contains("Purged"), "{t}");

    // Topic purge produces one log per purged memory
    let logs = get_edit_logs_by_op(&pool, &uid, "purge").await;
    assert_eq!(logs.len(), 2, "topic purge should produce 2 edit logs (one per rust memory)");
    let (_, _memory_id, _payload, reason, snap_before) = &logs[0];
    assert!(reason.contains("topic:rust"), "reason should contain topic: {reason}");
    assert!(snap_before.is_some(), "snapshot_before should be set");

    cleanup(&pool, &uid).await;
    println!("✅ purge by topic: edit log with topic reason + snapshot");
}

// ═══════════════════════════════════════════════════════════════════════════════
// 6. ROLLBACK via safety snapshot — purge then restore
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
#[serial]
async fn test_purge_rollback_via_safety_snapshot() {
    let (svc, git, pool, uid) = setup().await;
    cleanup(&pool, &uid).await;
    ensure_snapshot_quota(&pool).await;

    // Store a memory
    let r = call(
        "memory_store",
        json!({"content": "important fact"}),
        &svc,
        &uid,
    )
    .await;
    let mid = text(&r)
        .split_whitespace()
        .nth(2)
        .unwrap()
        .trim_end_matches(':')
        .to_string();

    // Purge it
    let r = call("memory_purge", json!({"memory_id": mid}), &svc, &uid).await;
    let t = text(&r);

    // Extract snapshot name from response
    let snap_line = t
        .lines()
        .find(|l| l.contains("Safety snapshot"))
        .expect("should have snapshot line");
    let snap_display = snap_line
        .split("Safety snapshot: ")
        .nth(1)
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap();

    // Memory should be gone
    let active = svc.list_active(&uid, 10).await.unwrap();
    assert_eq!(active.len(), 0, "memory should be purged");

    // Rollback using the safety snapshot
    let r = memoria_mcp::git_tools::call(
        "memory_rollback",
        json!({"name": snap_display}),
        &git,
        &svc,
        &uid,
    )
    .await
    .expect("rollback");
    assert!(text(&r).contains("Rolled back"), "{}", text(&r));

    // Memory should be restored
    let active = svc.list_active(&uid, 10).await.unwrap();
    assert_eq!(active.len(), 1, "memory should be restored after rollback");
    assert_eq!(active[0].content, "important fact");

    cleanup(&pool, &uid).await;
    println!("✅ purge → rollback via safety snapshot restores memory");
}

// ═══════════════════════════════════════════════════════════════════════════════
// 7. GOVERNANCE — quarantine writes audit log
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
#[serial]
async fn test_governance_quarantine_writes_edit_log() {
    let (svc, _git, pool, uid) = setup().await;
    cleanup(&pool, &uid).await;

    // Store a memory with very low confidence (T4 = 0.5) then age it
    let r = call(
        "memory_store",
        json!({"content": "low confidence fact", "trust_tier": "T4"}),
        &svc,
        &uid,
    )
    .await;
    let mid = text(&r)
        .split_whitespace()
        .nth(2)
        .unwrap()
        .trim_end_matches(':')
        .to_string();

    // Artificially age the memory so it gets quarantined
    sqlx::query("UPDATE mem_memories SET observed_at = DATE_SUB(NOW(), INTERVAL 60 DAY) WHERE memory_id = ?")
        .bind(&mid).execute(&pool).await.unwrap();

    // Run governance via MCP
    let r = call("memory_governance", json!({"force": true}), &svc, &uid).await;
    let t = text(&r);
    println!("governance result: {t}");

    // Check if quarantine happened (depends on confidence decay)
    let logs = get_edit_logs_by_op(&pool, &uid, "governance:quarantine").await;
    assert!(
        !logs.is_empty(),
        "governance should have written quarantine edit log (quarantined=1 in output)"
    );
    let (op, _, _, reason, _) = &logs[0];
    assert_eq!(op, "governance:quarantine");
    assert!(reason.contains("quarantined"), "reason: {reason}");
    println!("✅ governance quarantine writes edit log");

    cleanup(&pool, &uid).await;
}

// ═══════════════════════════════════════════════════════════════════════════════
// 8. STORE BATCH — single audit log entry
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
#[serial]
async fn test_store_batch_writes_single_edit_log() {
    let (svc, _git, pool, uid) = setup().await;
    cleanup(&pool, &uid).await;

    let items = vec![
        (
            "batch fact 1".to_string(),
            memoria_core::MemoryType::Semantic,
            None,
            None,
        ),
        (
            "batch fact 2".to_string(),
            memoria_core::MemoryType::Semantic,
            None,
            None,
        ),
        (
            "batch fact 3".to_string(),
            memoria_core::MemoryType::Semantic,
            None,
            None,
        ),
    ];
    let results = svc.store_batch(&uid, items).await.unwrap();
    assert_eq!(results.len(), 3);

    // Should be exactly 1 inject log for the batch
    // Batch store now produces one log per memory
    let logs = get_edit_logs_by_op(&pool, &uid, "inject").await;
    assert_eq!(logs.len(), 3, "batch store should produce 3 edit logs, got {}", logs.len());
    let (_, _, _, reason, _) = &logs[0];
    assert!(reason.contains("store_batch"), "reason: {reason}");
    let logged_ids: Vec<_> = logs.iter().filter_map(|(_, mid, _, _, _)| mid.clone()).collect();
    for m in &results {
        assert!(logged_ids.contains(&m.memory_id), "logged_ids should contain {}", m.memory_id);
    }

    cleanup(&pool, &uid).await;
    println!("✅ store_batch: 3 edit logs (one per memory)");
}

// ═══════════════════════════════════════════════════════════════════════════════
// 9. FULL AUDIT TRAIL — inject → correct → purge, verify chronological order
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
#[serial]
async fn test_full_audit_trail_inject_correct_purge() {
    let (svc, _git, pool, uid) = setup().await;
    cleanup(&pool, &uid).await;
    ensure_snapshot_quota(&pool).await;

    // 1. Inject
    let r = call(
        "memory_store",
        json!({"content": "original fact"}),
        &svc,
        &uid,
    )
    .await;
    let mid = text(&r)
        .split_whitespace()
        .nth(2)
        .unwrap()
        .trim_end_matches(':')
        .to_string();

    // 2. Correct
    let r = call(
        "memory_correct",
        json!({"memory_id": mid, "new_content": "corrected fact"}),
        &svc,
        &uid,
    )
    .await;
    let corrected_text = text(&r);
    let new_mid = corrected_text
        .split_whitespace()
        .nth(2)
        .unwrap()
        .trim_end_matches(':')
        .to_string();

    // 3. Purge the corrected memory
    call("memory_purge", json!({"memory_id": new_mid}), &svc, &uid).await;

    // Verify full audit trail (ordered by created_at DESC)
    let logs = get_edit_logs(&pool, &uid).await;
    assert!(
        logs.len() >= 3,
        "should have at least 3 edit log entries, got {}",
        logs.len()
    );

    // Most recent first: purge, correct, inject
    let ops: Vec<&str> = logs.iter().map(|(op, _, _, _, _)| op.as_str()).collect();
    assert_eq!(ops[0], "purge", "most recent should be purge");
    assert_eq!(ops[1], "correct", "second should be correct");
    // inject logs may be 2 (original + inject from correct's new memory) or just 1
    assert!(ops.iter().any(|o| *o == "inject"), "should have inject");

    // Purge should have snapshot_before
    assert!(logs[0].4.is_some(), "purge should have snapshot_before");

    cleanup(&pool, &uid).await;
    println!("✅ full audit trail: inject → correct → purge in chronological order");
}

// ═══════════════════════════════════════════════════════════════════════════════
// 10. SAFETY SNAPSHOT WARNING — verify MCP response includes warning info
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
#[serial]
async fn test_purge_response_includes_snapshot_info() {
    let (svc, _git, pool, uid) = setup().await;
    cleanup(&pool, &uid).await;

    let r = call(
        "memory_store",
        json!({"content": "will be purged"}),
        &svc,
        &uid,
    )
    .await;
    let mid = text(&r)
        .split_whitespace()
        .nth(2)
        .unwrap()
        .trim_end_matches(':')
        .to_string();

    let r = call("memory_purge", json!({"memory_id": mid}), &svc, &uid).await;
    let t = text(&r);

    // Response should contain either:
    // - "Safety snapshot: xxx" (success)
    // - "⚠️" warning about snapshot failure
    assert!(
        t.contains("Safety snapshot") || t.contains("⚠️"),
        "purge response should include snapshot info or warning: {t}"
    );

    cleanup(&pool, &uid).await;
    println!("✅ purge response includes snapshot info");
}

// ═══════════════════════════════════════════════════════════════════════════════
// 11. API PURGE — verify PurgeResponse includes snapshot_name
// ═══════════════════════════════════════════════════════════════════════════════

async fn spawn_server() -> (String, reqwest::Client, MySqlPool) {
    use memoria_service::Config;

    let cfg = Config::from_env();
    let db = db_url();
    let store = SqlMemoryStore::connect(&db, test_dim())
        .await
        .expect("connect");
    store.migrate().await.expect("migrate");
    let pool = MySqlPool::connect(&db).await.expect("pool");
    let git = Arc::new(GitForDataService::new(pool.clone(), &cfg.db_name));
    let service = Arc::new(MemoryService::new_sql_with_llm(Arc::new(store), None, None));
    let state = memoria_api::AppState::new(service, git, String::new());
    let app = memoria_api::build_router(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await });
    tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;

    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let base = format!("http://127.0.0.1:{port}");
    (base, client, pool)
}

#[tokio::test]
#[serial]
async fn test_api_purge_returns_snapshot_name() {
    let (base, client, pool) = spawn_server().await;
    let uid = uid();
    cleanup(&pool, &uid).await;
    ensure_snapshot_quota(&pool).await;

    // Store
    let r = client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({"content": "api purge test"}))
        .send()
        .await
        .unwrap();
    let mid = r.json::<Value>().await.unwrap()["memory_id"]
        .as_str()
        .unwrap()
        .to_string();

    // Purge via API
    let r = client
        .post(format!("{base}/v1/memories/purge"))
        .header("X-User-Id", &uid)
        .json(&json!({"memory_ids": [mid]}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["purged"], 1);
    assert!(
        body["snapshot_name"].is_string(),
        "API response should include snapshot_name: {body}"
    );
    let snap = body["snapshot_name"].as_str().unwrap();
    assert!(
        snap.starts_with("mem_snap_pre_purge_"),
        "snapshot_name: {snap}"
    );

    // Verify snapshot exists in DB
    assert!(snapshot_exists(&pool, snap).await, "snapshot should exist");

    // Verify edit log in DB
    let logs = get_edit_logs_by_op(&pool, &uid, "purge").await;
    assert!(!logs.is_empty(), "purge should write edit log");
    assert_eq!(
        logs[0].4.as_deref(),
        Some(snap),
        "edit log snapshot_before should match"
    );

    cleanup(&pool, &uid).await;
    println!("✅ API purge returns snapshot_name + edit log verified");
}

// ═══════════════════════════════════════════════════════════════════════════════
// 12. API PURGE BY TOPIC — verify snapshot + edit log via API
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
#[serial]
async fn test_api_purge_topic_returns_snapshot() {
    let (base, client, pool) = spawn_server().await;
    let uid = uid();
    cleanup(&pool, &uid).await;
    ensure_snapshot_quota(&pool).await;

    for c in ["topicX alpha", "topicX beta", "unrelated"] {
        client
            .post(format!("{base}/v1/memories"))
            .header("X-User-Id", &uid)
            .json(&json!({"content": c}))
            .send()
            .await
            .unwrap();
    }

    let r = client
        .post(format!("{base}/v1/memories/purge"))
        .header("X-User-Id", &uid)
        .json(&json!({"topic": "topicX"}))
        .send()
        .await
        .unwrap();
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["purged"], 2);
    assert!(
        body["snapshot_name"].is_string(),
        "should have snapshot_name: {body}"
    );

    // Topic purge produces one log per purged memory
    let logs = get_edit_logs_by_op(&pool, &uid, "purge").await;
    assert_eq!(logs.len(), 2, "topic purge should produce 2 edit logs");
    assert!(logs[0].3.contains("topic:topicX"), "reason: {:?}", logs[0].3);

    cleanup(&pool, &uid).await;
    println!("✅ API purge by topic: snapshot + edit log");
}
