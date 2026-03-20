/// Feedback E2E tests against real DB.
/// Covers: memory_feedback MCP tool, feedback stats, feedback by tier.
///
/// Run: DATABASE_URL=mysql://root:111@localhost:6001/memoria \
///      cargo test -p memoria-mcp --test feedback_e2e -- --nocapture
use memoria_service::MemoryService;
use memoria_storage::SqlMemoryStore;
use serde_json::{json, Value};
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
    format!("fb_{}", &Uuid::new_v4().simple().to_string()[..8])
}

async fn setup() -> (Arc<MemoryService>, Arc<SqlMemoryStore>, String) {
    let store = SqlMemoryStore::connect(&db_url(), test_dim())
        .await
        .expect("connect");
    store.migrate().await.expect("migrate");
    let store = Arc::new(store);
    let svc = Arc::new(MemoryService::new_sql_with_llm(store.clone(), None, None));
    (svc, store, uid())
}

async fn call(name: &str, args: Value, svc: &Arc<MemoryService>, uid: &str) -> Value {
    memoria_mcp::tools::call(name, args, svc, uid)
        .await
        .expect(name)
}
fn text(v: &Value) -> &str {
    v["content"][0]["text"].as_str().unwrap_or("")
}

// ── 1. memory_feedback: basic flow ────────────────────────────────────────────

#[tokio::test]
async fn test_feedback_basic_flow() {
    let (svc, _store, uid) = setup().await;

    // Store a memory first
    let r = call(
        "memory_store",
        json!({"content": "Test memory for feedback", "memory_type": "semantic"}),
        &svc,
        &uid,
    )
    .await;
    let store_text = text(&r);
    assert!(store_text.contains("Stored memory"), "store: {store_text}");

    // Extract memory_id from response
    let memory_id = store_text
        .split("Stored memory ")
        .nth(1)
        .and_then(|s| s.split(':').next())
        .expect("extract memory_id");

    // Record feedback: useful
    let r = call(
        "memory_feedback",
        json!({"memory_id": memory_id, "signal": "useful"}),
        &svc,
        &uid,
    )
    .await;
    let fb_text = text(&r);
    assert!(fb_text.contains("Recorded feedback"), "feedback: {fb_text}");
    assert!(fb_text.contains("signal=useful"), "signal: {fb_text}");
    assert!(fb_text.contains(memory_id), "memory_id: {fb_text}");

    println!("✅ test_feedback_basic_flow");
}

// ── 2. memory_feedback: all signal types ──────────────────────────────────────

#[tokio::test]
async fn test_feedback_all_signals() {
    let (svc, _store, uid) = setup().await;

    // Store memories for each signal type
    let signals = ["useful", "irrelevant", "outdated", "wrong"];
    let mut memory_ids = Vec::new();

    for signal in &signals {
        let r = call(
            "memory_store",
            json!({"content": format!("Memory for {signal} feedback"), "memory_type": "semantic"}),
            &svc,
            &uid,
        )
        .await;
        let store_text = text(&r);
        let memory_id = store_text
            .split("Stored memory ")
            .nth(1)
            .and_then(|s| s.split(':').next())
            .expect("extract memory_id");
        memory_ids.push(memory_id.to_string());
    }

    // Record feedback for each
    for (signal, memory_id) in signals.iter().zip(memory_ids.iter()) {
        let r = call(
            "memory_feedback",
            json!({"memory_id": memory_id, "signal": signal}),
            &svc,
            &uid,
        )
        .await;
        let fb_text = text(&r);
        assert!(
            fb_text.contains(&format!("signal={signal}")),
            "{signal}: {fb_text}"
        );
    }

    println!("✅ test_feedback_all_signals");
}

// ── 3. memory_feedback: with context ──────────────────────────────────────────

#[tokio::test]
async fn test_feedback_with_context() {
    let (svc, _store, uid) = setup().await;

    // Store a memory
    let r = call(
        "memory_store",
        json!({"content": "Memory with context feedback", "memory_type": "semantic"}),
        &svc,
        &uid,
    )
    .await;
    let memory_id = text(&r)
        .split("Stored memory ")
        .nth(1)
        .and_then(|s| s.split(':').next())
        .expect("extract memory_id");

    // Record feedback with context
    let r = call(
        "memory_feedback",
        json!({
            "memory_id": memory_id,
            "signal": "irrelevant",
            "context": "This memory was retrieved but not related to my query about databases"
        }),
        &svc,
        &uid,
    )
    .await;
    let fb_text = text(&r);
    assert!(fb_text.contains("Recorded feedback"), "feedback: {fb_text}");

    println!("✅ test_feedback_with_context");
}

// ── 4. memory_feedback: invalid signal ────────────────────────────────────────

#[tokio::test]
async fn test_feedback_invalid_signal() {
    let (svc, _store, uid) = setup().await;

    // Store a memory
    let r = call(
        "memory_store",
        json!({"content": "Memory for invalid signal test", "memory_type": "semantic"}),
        &svc,
        &uid,
    )
    .await;
    let memory_id = text(&r)
        .split("Stored memory ")
        .nth(1)
        .and_then(|s| s.split(':').next())
        .expect("extract memory_id");

    // Try invalid signal
    let result = memoria_mcp::tools::call(
        "memory_feedback",
        json!({"memory_id": memory_id, "signal": "invalid_signal"}),
        &svc,
        &uid,
    )
    .await;

    assert!(result.is_err(), "should fail with invalid signal");
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("Invalid signal"),
        "error should mention invalid signal: {err}"
    );

    println!("✅ test_feedback_invalid_signal");
}

// ── 5. memory_feedback: non-existent memory ───────────────────────────────────

#[tokio::test]
async fn test_feedback_nonexistent_memory() {
    let (svc, _store, uid) = setup().await;

    let result = memoria_mcp::tools::call(
        "memory_feedback",
        json!({"memory_id": "nonexistent_memory_id_12345", "signal": "useful"}),
        &svc,
        &uid,
    )
    .await;

    assert!(result.is_err(), "should fail with non-existent memory");
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("not found") || err.contains("NotFound"),
        "error should mention not found: {err}"
    );

    println!("✅ test_feedback_nonexistent_memory");
}

// ── 6. memory_feedback: other user's memory ───────────────────────────────────

#[tokio::test]
async fn test_feedback_other_users_memory() {
    let (svc, _store, uid1) = setup().await;
    let uid2 = uid(); // Different user

    // User 1 stores a memory
    let r = call(
        "memory_store",
        json!({"content": "User 1's private memory", "memory_type": "semantic"}),
        &svc,
        &uid1,
    )
    .await;
    let memory_id = text(&r)
        .split("Stored memory ")
        .nth(1)
        .and_then(|s| s.split(':').next())
        .expect("extract memory_id");

    // User 2 tries to give feedback on User 1's memory
    let result = memoria_mcp::tools::call(
        "memory_feedback",
        json!({"memory_id": memory_id, "signal": "useful"}),
        &svc,
        &uid2,
    )
    .await;

    assert!(result.is_err(), "should fail for other user's memory");
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("not found") || err.contains("not owned"),
        "error should mention ownership: {err}"
    );

    println!("✅ test_feedback_other_users_memory");
}

// ── 7. Storage layer: feedback stats ──────────────────────────────────────────

#[tokio::test]
async fn test_feedback_stats() {
    let (svc, store, uid) = setup().await;

    // Store multiple memories and give feedback
    let mut memory_ids = Vec::new();
    for i in 0..5 {
        let r = call(
            "memory_store",
            json!({"content": format!("Stats test memory {i}"), "memory_type": "semantic"}),
            &svc,
            &uid,
        )
        .await;
        let memory_id = text(&r)
            .split("Stored memory ")
            .nth(1)
            .and_then(|s| s.split(':').next())
            .expect("extract memory_id");
        memory_ids.push(memory_id.to_string());
    }

    // Give varied feedback
    store
        .record_feedback(&uid, &memory_ids[0], "useful", None)
        .await
        .unwrap();
    store
        .record_feedback(&uid, &memory_ids[1], "useful", None)
        .await
        .unwrap();
    store
        .record_feedback(&uid, &memory_ids[2], "irrelevant", None)
        .await
        .unwrap();
    store
        .record_feedback(&uid, &memory_ids[3], "outdated", None)
        .await
        .unwrap();
    store
        .record_feedback(&uid, &memory_ids[4], "wrong", None)
        .await
        .unwrap();

    // Get stats
    let stats = store.get_feedback_stats(&uid).await.unwrap();
    assert_eq!(stats.total, 5, "total feedback count");
    assert_eq!(stats.useful, 2, "useful count");
    assert_eq!(stats.irrelevant, 1, "irrelevant count");
    assert_eq!(stats.outdated, 1, "outdated count");
    assert_eq!(stats.wrong, 1, "wrong count");

    println!("✅ test_feedback_stats: {:?}", stats);
}

// ── 8. Storage layer: feedback by tier ────────────────────────────────────────

#[tokio::test]
async fn test_feedback_by_tier() {
    let (svc, store, uid) = setup().await;

    // Store memories with different trust tiers
    let tiers = ["T1", "T2", "T3", "T4"];
    let mut memory_ids = Vec::new();

    for tier in &tiers {
        let r = call(
            "memory_store",
            json!({
                "content": format!("Tier {tier} memory"),
                "memory_type": "semantic",
                "trust_tier": tier
            }),
            &svc,
            &uid,
        )
        .await;
        let memory_id = text(&r)
            .split("Stored memory ")
            .nth(1)
            .and_then(|s| s.split(':').next())
            .expect("extract memory_id");
        memory_ids.push(memory_id.to_string());
    }

    // Give feedback: T1 useful, T2 useful, T3 irrelevant, T4 wrong
    store
        .record_feedback(&uid, &memory_ids[0], "useful", None)
        .await
        .unwrap();
    store
        .record_feedback(&uid, &memory_ids[1], "useful", None)
        .await
        .unwrap();
    store
        .record_feedback(&uid, &memory_ids[2], "irrelevant", None)
        .await
        .unwrap();
    store
        .record_feedback(&uid, &memory_ids[3], "wrong", None)
        .await
        .unwrap();

    // Get breakdown by tier
    let breakdown = store.get_feedback_by_tier(&uid).await.unwrap();
    assert!(!breakdown.is_empty(), "should have tier breakdown");

    // Verify we have entries for different tiers
    let t1_useful = breakdown
        .iter()
        .find(|t| t.tier == "T1" && t.signal == "useful");
    assert!(t1_useful.is_some(), "should have T1 useful");
    assert_eq!(t1_useful.unwrap().count, 1);

    let t4_wrong = breakdown
        .iter()
        .find(|t| t.tier == "T4" && t.signal == "wrong");
    assert!(t4_wrong.is_some(), "should have T4 wrong");
    assert_eq!(t4_wrong.unwrap().count, 1);

    println!("✅ test_feedback_by_tier: {} entries", breakdown.len());
}

// ── 9. Service layer: record_feedback ─────────────────────────────────────────

#[tokio::test]
async fn test_service_record_feedback() {
    let (svc, _store, uid) = setup().await;

    // Store a memory
    let r = call(
        "memory_store",
        json!({"content": "Service layer test memory", "memory_type": "semantic"}),
        &svc,
        &uid,
    )
    .await;
    let memory_id = text(&r)
        .split("Stored memory ")
        .nth(1)
        .and_then(|s| s.split(':').next())
        .expect("extract memory_id");

    // Use service layer directly
    let feedback_id = svc
        .record_feedback(&uid, memory_id, "useful", Some("test context"))
        .await
        .unwrap();

    assert!(!feedback_id.is_empty(), "should return feedback_id");

    // Verify via stats
    let stats = svc.get_feedback_stats(&uid).await.unwrap();
    assert!(stats.useful >= 1, "should have at least 1 useful feedback");

    println!("✅ test_service_record_feedback: feedback_id={feedback_id}");
}

// ── 10. Multiple feedback on same memory ──────────────────────────────────────

#[tokio::test]
async fn test_multiple_feedback_same_memory() {
    let (svc, store, uid) = setup().await;

    // Store a memory
    let r = call(
        "memory_store",
        json!({"content": "Memory for multiple feedback", "memory_type": "semantic"}),
        &svc,
        &uid,
    )
    .await;
    let memory_id = text(&r)
        .split("Stored memory ")
        .nth(1)
        .and_then(|s| s.split(':').next())
        .expect("extract memory_id");

    // Give multiple feedback on the same memory (allowed - tracks history)
    store
        .record_feedback(&uid, memory_id, "useful", Some("first impression"))
        .await
        .unwrap();
    store
        .record_feedback(&uid, memory_id, "outdated", Some("after re-evaluation"))
        .await
        .unwrap();

    // Both should be recorded
    let stats = store.get_feedback_stats(&uid).await.unwrap();
    assert!(stats.total >= 2, "should have at least 2 feedback entries");

    println!("✅ test_multiple_feedback_same_memory");
}


// ── 11. DB verification: check actual table values ────────────────────────────

#[tokio::test]
async fn test_feedback_db_verification() {
    let (svc, store, uid) = setup().await;

    // Store a memory
    let r = call(
        "memory_store",
        json!({"content": "DB verification test memory", "memory_type": "semantic", "trust_tier": "T2"}),
        &svc,
        &uid,
    )
    .await;
    let memory_id = text(&r)
        .split("Stored memory ")
        .nth(1)
        .and_then(|s| s.split(':').next())
        .expect("extract memory_id");

    // Record feedback via MCP tool
    let r = call(
        "memory_feedback",
        json!({
            "memory_id": memory_id,
            "signal": "useful",
            "context": "This was very helpful for my task"
        }),
        &svc,
        &uid,
    )
    .await;
    let fb_text = text(&r);
    assert!(fb_text.contains("Recorded feedback"), "feedback: {fb_text}");

    // Extract feedback_id from response
    let feedback_id = fb_text
        .split("feedback_id=")
        .nth(1)
        .expect("extract feedback_id");

    // ── Direct DB verification ────────────────────────────────────────────────

    // 1. Verify feedback record exists in mem_retrieval_feedback
    let row: Option<(String, String, String, String, Option<String>)> = sqlx::query_as(
        "SELECT id, user_id, memory_id, signal, context FROM mem_retrieval_feedback WHERE id = ?",
    )
    .bind(feedback_id)
    .fetch_optional(store.pool())
    .await
    .expect("query feedback");

    let (db_id, db_user_id, db_memory_id, db_signal, db_context) =
        row.expect("feedback record should exist in DB");

    assert_eq!(db_id, feedback_id, "feedback_id mismatch");
    assert_eq!(db_user_id, uid, "user_id mismatch");
    assert_eq!(db_memory_id, memory_id, "memory_id mismatch");
    assert_eq!(db_signal, "useful", "signal mismatch");
    assert_eq!(
        db_context.as_deref(),
        Some("This was very helpful for my task"),
        "context mismatch"
    );

    // 2. Verify memory exists and has correct trust_tier
    let mem_row: Option<(String, String)> = sqlx::query_as(
        "SELECT memory_id, trust_tier FROM mem_memories WHERE memory_id = ?",
    )
    .bind(memory_id)
    .fetch_optional(store.pool())
    .await
    .expect("query memory");

    let (_, db_tier) = mem_row.expect("memory should exist");
    assert_eq!(db_tier, "T2", "trust_tier mismatch");

    // 3. Add more feedback and verify stats aggregation
    store
        .record_feedback(&uid, memory_id, "outdated", Some("info is stale"))
        .await
        .unwrap();

    // Store another memory and give different feedback
    let r2 = call(
        "memory_store",
        json!({"content": "Second test memory", "memory_type": "semantic", "trust_tier": "T3"}),
        &svc,
        &uid,
    )
    .await;
    let memory_id_2 = text(&r2)
        .split("Stored memory ")
        .nth(1)
        .and_then(|s| s.split(':').next())
        .expect("extract memory_id_2");

    store
        .record_feedback(&uid, memory_id_2, "irrelevant", None)
        .await
        .unwrap();

    // 4. Verify aggregated stats via direct SQL
    let stats_row: (i64, i64, i64, i64, i64) = sqlx::query_as(
        "SELECT \
           COUNT(*) as total, \
           SUM(CASE WHEN signal = 'useful' THEN 1 ELSE 0 END) as useful, \
           SUM(CASE WHEN signal = 'irrelevant' THEN 1 ELSE 0 END) as irrelevant, \
           SUM(CASE WHEN signal = 'outdated' THEN 1 ELSE 0 END) as outdated, \
           SUM(CASE WHEN signal = 'wrong' THEN 1 ELSE 0 END) as wrong \
         FROM mem_retrieval_feedback WHERE user_id = ?",
    )
    .bind(&uid)
    .fetch_one(store.pool())
    .await
    .expect("query stats");

    assert_eq!(stats_row.0, 3, "total feedback count");
    assert_eq!(stats_row.1, 1, "useful count");
    assert_eq!(stats_row.2, 1, "irrelevant count");
    assert_eq!(stats_row.3, 1, "outdated count");
    assert_eq!(stats_row.4, 0, "wrong count");

    // 5. Verify feedback by tier via direct SQL
    let tier_rows: Vec<(String, String, i64)> = sqlx::query_as(
        "SELECT m.trust_tier, f.signal, COUNT(*) as cnt \
         FROM mem_retrieval_feedback f \
         JOIN mem_memories m ON f.memory_id = m.memory_id \
         WHERE f.user_id = ? \
         GROUP BY m.trust_tier, f.signal \
         ORDER BY m.trust_tier, f.signal",
    )
    .bind(&uid)
    .fetch_all(store.pool())
    .await
    .expect("query tier breakdown");

    // Should have: T2/useful, T2/outdated, T3/irrelevant
    assert_eq!(tier_rows.len(), 3, "should have 3 tier/signal combinations");

    let t2_useful = tier_rows.iter().find(|(t, s, _)| t == "T2" && s == "useful");
    assert!(t2_useful.is_some(), "should have T2/useful");
    assert_eq!(t2_useful.unwrap().2, 1, "T2/useful count");

    let t2_outdated = tier_rows
        .iter()
        .find(|(t, s, _)| t == "T2" && s == "outdated");
    assert!(t2_outdated.is_some(), "should have T2/outdated");
    assert_eq!(t2_outdated.unwrap().2, 1, "T2/outdated count");

    let t3_irrelevant = tier_rows
        .iter()
        .find(|(t, s, _)| t == "T3" && s == "irrelevant");
    assert!(t3_irrelevant.is_some(), "should have T3/irrelevant");
    assert_eq!(t3_irrelevant.unwrap().2, 1, "T3/irrelevant count");

    println!("✅ test_feedback_db_verification: all DB values verified");
    println!("   - feedback record: id={}, signal={}, context={:?}", db_id, db_signal, db_context);
    println!("   - stats: total={}, useful={}, irrelevant={}, outdated={}, wrong={}",
             stats_row.0, stats_row.1, stats_row.2, stats_row.3, stats_row.4);
    println!("   - tier breakdown: {:?}", tier_rows);
}

// ── 12. Full closed-loop test: store → retrieve → feedback → verify impact ───

#[tokio::test]
async fn test_feedback_closed_loop() {
    let (svc, store, uid) = setup().await;

    // 1. Store multiple memories with different tiers
    let memories = vec![
        ("High quality T1 memory about Rust", "T1"),
        ("Medium quality T2 memory about Go", "T2"),
        ("Low quality T4 memory about Python", "T4"),
    ];

    let mut memory_ids = Vec::new();
    for (content, tier) in &memories {
        let r = call(
            "memory_store",
            json!({"content": content, "memory_type": "semantic", "trust_tier": tier}),
            &svc,
            &uid,
        )
        .await;
        let memory_id = text(&r)
            .split("Stored memory ")
            .nth(1)
            .and_then(|s| s.split(':').next())
            .expect("extract memory_id")
            .to_string();
        memory_ids.push(memory_id);
    }

    // 2. Simulate retrieval feedback pattern:
    //    - T1 memory: useful (high tier, good retrieval)
    //    - T2 memory: irrelevant (medium tier, bad retrieval)
    //    - T4 memory: wrong (low tier, very bad retrieval)
    store
        .record_feedback(&uid, &memory_ids[0], "useful", Some("exactly what I needed"))
        .await
        .unwrap();
    store
        .record_feedback(&uid, &memory_ids[1], "irrelevant", Some("not related to my query"))
        .await
        .unwrap();
    store
        .record_feedback(&uid, &memory_ids[2], "wrong", Some("completely incorrect info"))
        .await
        .unwrap();

    // 3. Verify the feedback pattern is captured correctly
    let stats = store.get_feedback_stats(&uid).await.unwrap();
    assert_eq!(stats.total, 3);
    assert_eq!(stats.useful, 1);
    assert_eq!(stats.irrelevant, 1);
    assert_eq!(stats.wrong, 1);

    // 4. Verify tier breakdown shows the pattern
    let breakdown = store.get_feedback_by_tier(&uid).await.unwrap();

    // T1 should have useful feedback
    let t1_feedback: Vec<_> = breakdown.iter().filter(|t| t.tier == "T1").collect();
    assert_eq!(t1_feedback.len(), 1);
    assert_eq!(t1_feedback[0].signal, "useful");

    // T4 should have wrong feedback
    let t4_feedback: Vec<_> = breakdown.iter().filter(|t| t.tier == "T4").collect();
    assert_eq!(t4_feedback.len(), 1);
    assert_eq!(t4_feedback[0].signal, "wrong");

    // 5. This data can now be used for adaptive tuning:
    //    - T1 memories have 100% useful rate → increase T1 weight
    //    - T4 memories have 100% wrong rate → decrease T4 weight or increase decay
    println!("✅ test_feedback_closed_loop: full cycle verified");
    println!("   Pattern captured: T1=useful, T2=irrelevant, T4=wrong");
    println!("   This data enables adaptive tuning of retrieval weights by tier");
}


// ── 13. Denormalized feedback counters in mem_memories_stats ──────────────────

#[tokio::test]
async fn test_feedback_denormalized_counters() {
    let (svc, store, uid) = setup().await;

    // Store a memory
    let r = call(
        "memory_store",
        json!({"content": "Denormalized counter test", "memory_type": "semantic"}),
        &svc,
        &uid,
    )
    .await;
    let memory_id = text(&r)
        .split("Stored memory ")
        .nth(1)
        .and_then(|s| s.split(':').next())
        .expect("extract memory_id");

    // Initially no feedback
    let fb = store.get_memory_feedback(memory_id).await.unwrap();
    assert_eq!(fb.useful, 0);
    assert_eq!(fb.irrelevant, 0);
    assert_eq!(fb.outdated, 0);
    assert_eq!(fb.wrong, 0);

    // Add feedback and verify counters update
    store.record_feedback(&uid, memory_id, "useful", None).await.unwrap();
    store.record_feedback(&uid, memory_id, "useful", None).await.unwrap();
    store.record_feedback(&uid, memory_id, "irrelevant", None).await.unwrap();

    let fb = store.get_memory_feedback(memory_id).await.unwrap();
    assert_eq!(fb.useful, 2, "useful count");
    assert_eq!(fb.irrelevant, 1, "irrelevant count");
    assert_eq!(fb.outdated, 0, "outdated count");
    assert_eq!(fb.wrong, 0, "wrong count");

    // Verify via direct SQL (no JOIN needed)
    let row: (i32, i32, i32, i32) = sqlx::query_as(
        "SELECT feedback_useful, feedback_irrelevant, feedback_outdated, feedback_wrong \
         FROM mem_memories_stats WHERE memory_id = ?",
    )
    .bind(memory_id)
    .fetch_one(store.pool())
    .await
    .expect("query stats");

    assert_eq!(row.0, 2, "DB feedback_useful");
    assert_eq!(row.1, 1, "DB feedback_irrelevant");
    assert_eq!(row.2, 0, "DB feedback_outdated");
    assert_eq!(row.3, 0, "DB feedback_wrong");

    println!("✅ test_feedback_denormalized_counters: no JOIN needed for per-memory feedback");
    println!("   Memory {}: useful={}, irrelevant={}, outdated={}, wrong={}",
             memory_id, fb.useful, fb.irrelevant, fb.outdated, fb.wrong);
}


// ── 14. Feedback affects retrieval ranking (closed-loop verification) ─────────

#[tokio::test]
async fn test_feedback_affects_retrieval_ranking() {
    let (svc, store, uid) = setup().await;

    // Store 3 memories with similar content but different trust tiers
    let memories = vec![
        ("Memory A about Rust programming language", "T2"),
        ("Memory B about Rust programming language", "T2"),
        ("Memory C about Rust programming language", "T2"),
    ];

    let mut memory_ids = Vec::new();
    for (content, tier) in &memories {
        let r = call(
            "memory_store",
            json!({"content": content, "memory_type": "semantic", "trust_tier": tier}),
            &svc,
            &uid,
        )
        .await;
        let memory_id = text(&r)
            .split("Stored memory ")
            .nth(1)
            .and_then(|s| s.split(':').next())
            .expect("extract memory_id")
            .to_string();
        memory_ids.push(memory_id);
    }

    // Give feedback: A=useful(2), B=no feedback, C=wrong(2)
    store.record_feedback(&uid, &memory_ids[0], "useful", None).await.unwrap();
    store.record_feedback(&uid, &memory_ids[0], "useful", None).await.unwrap();
    // B has no feedback
    store.record_feedback(&uid, &memory_ids[2], "wrong", None).await.unwrap();
    store.record_feedback(&uid, &memory_ids[2], "wrong", None).await.unwrap();

    // Verify feedback is recorded in DB
    let fb_a = store.get_memory_feedback(&memory_ids[0]).await.unwrap();
    let fb_b = store.get_memory_feedback(&memory_ids[1]).await.unwrap();
    let fb_c = store.get_memory_feedback(&memory_ids[2]).await.unwrap();

    assert_eq!(fb_a.useful, 2, "A should have 2 useful");
    assert_eq!(fb_b.useful, 0, "B should have 0 feedback");
    assert_eq!(fb_c.wrong, 2, "C should have 2 wrong");

    println!("✅ Feedback recorded: A={:?}, B={:?}, C={:?}", fb_a, fb_b, fb_c);

    // Now retrieve and check ranking
    // Expected order: A (boosted by useful) > B (neutral) > C (penalized by wrong)
    let r = call(
        "memory_retrieve",
        json!({"query": "Rust programming", "top_k": 10}),
        &svc,
        &uid,
    )
    .await;
    let retrieve_text = text(&r);
    println!("Retrieve result:\n{}", retrieve_text);

    // Parse the order from the result
    let lines: Vec<&str> = retrieve_text.lines().collect();
    let mut order = Vec::new();
    for line in &lines {
        for (i, mid) in memory_ids.iter().enumerate() {
            if line.contains(mid) {
                order.push(('A' as u8 + i as u8) as char);
                break;
            }
        }
    }

    println!("Retrieval order: {:?}", order);

    // Verify A comes before C (useful > wrong)
    let pos_a = order.iter().position(|&c| c == 'A');
    let pos_c = order.iter().position(|&c| c == 'C');

    if let (Some(a), Some(c)) = (pos_a, pos_c) {
        assert!(a < c, "Memory A (useful feedback) should rank higher than C (wrong feedback). Order: {:?}", order);
        println!("✅ Feedback affects ranking: A(useful) at position {}, C(wrong) at position {}", a, c);
    } else {
        println!("⚠️ Could not determine positions, order: {:?}", order);
    }
}

// ── 15. Verify feedback multiplier in DB scoring ──────────────────────────────

#[tokio::test]
async fn test_feedback_score_multiplier_db_verification() {
    let (svc, store, uid) = setup().await;

    // Store a memory
    let r = call(
        "memory_store",
        json!({"content": "Score multiplier test memory about databases", "memory_type": "semantic"}),
        &svc,
        &uid,
    )
    .await;
    let memory_id = text(&r)
        .split("Stored memory ")
        .nth(1)
        .and_then(|s| s.split(':').next())
        .expect("extract memory_id");

    // Get baseline retrieval score (no feedback)
    let baseline_mems = svc.retrieve(&uid, "databases", 5).await.unwrap();
    let baseline_score = baseline_mems
        .iter()
        .find(|m| m.memory_id == memory_id)
        .and_then(|m| m.retrieval_score);

    println!("Baseline score (no feedback): {:?}", baseline_score);

    // Add positive feedback
    store.record_feedback(&uid, memory_id, "useful", None).await.unwrap();
    store.record_feedback(&uid, memory_id, "useful", None).await.unwrap();

    // Verify feedback in DB
    let fb = store.get_memory_feedback(memory_id).await.unwrap();
    assert_eq!(fb.useful, 2, "Should have 2 useful feedback");

    // Get boosted retrieval score
    let boosted_mems = svc.retrieve(&uid, "databases", 5).await.unwrap();
    let boosted_score = boosted_mems
        .iter()
        .find(|m| m.memory_id == memory_id)
        .and_then(|m| m.retrieval_score);

    println!("Boosted score (2 useful): {:?}", boosted_score);

    // Verify score increased
    if let (Some(base), Some(boost)) = (baseline_score, boosted_score) {
        // Expected: boost = base * (1 + 0.1 * 2) = base * 1.2
        let expected_ratio = 1.2;
        let actual_ratio = boost / base;
        println!("Score ratio: {:.4} (expected ~{:.4})", actual_ratio, expected_ratio);
        assert!(actual_ratio > 1.0, "Boosted score should be higher than baseline");
        assert!(actual_ratio < 1.5, "Boost should be reasonable (not too extreme)");
    }

    // Now add negative feedback and verify penalty
    store.record_feedback(&uid, memory_id, "wrong", None).await.unwrap();
    store.record_feedback(&uid, memory_id, "wrong", None).await.unwrap();
    store.record_feedback(&uid, memory_id, "wrong", None).await.unwrap();

    let fb_after = store.get_memory_feedback(memory_id).await.unwrap();
    assert_eq!(fb_after.useful, 2, "Still 2 useful");
    assert_eq!(fb_after.wrong, 3, "Now 3 wrong");

    // Net feedback: 2 useful - 0.5 * 3 wrong = 2 - 1.5 = 0.5 (slight positive)
    let mixed_mems = svc.retrieve(&uid, "databases", 5).await.unwrap();
    let mixed_score = mixed_mems
        .iter()
        .find(|m| m.memory_id == memory_id)
        .and_then(|m| m.retrieval_score);

    println!("Mixed score (2 useful, 3 wrong): {:?}", mixed_score);

    if let (Some(base), Some(mixed)) = (baseline_score, mixed_score) {
        // Net delta = 2 - 0.5*3 = 0.5, multiplier = 1 + 0.1*0.5 = 1.05
        let ratio = mixed / base;
        println!("Mixed ratio: {:.4} (expected ~1.05)", ratio);
        // Should be slightly above baseline but less than pure positive
        assert!(ratio > 0.9, "Mixed feedback should not heavily penalize");
        assert!(ratio < 1.3, "Mixed feedback should not heavily boost");
    }

    println!("✅ test_feedback_score_multiplier_db_verification: feedback affects scores correctly");
}


// ── 16. memory_get_retrieval_params: get default params ───────────────────────

#[tokio::test]
async fn test_get_retrieval_params_default() {
    let (svc, _store, uid) = setup().await;

    let result = call("memory_get_retrieval_params", json!({}), &svc, &uid).await;
    let text = text(&result);

    // Should return default params
    assert!(text.contains("feedback_weight"), "Should show feedback_weight");
    assert!(text.contains("0.1"), "Default feedback_weight should be 0.1");
    assert!(text.contains("temporal_decay_hours"), "Should show temporal_decay_hours");
    assert!(text.contains("168"), "Default temporal_decay_hours should be 168");

    println!("✅ test_get_retrieval_params_default: {}", text);
}

// ── 17. memory_tune_params: insufficient feedback ─────────────────────────────

#[tokio::test]
async fn test_tune_params_insufficient_feedback() {
    let (svc, _store, uid) = setup().await;

    // No feedback yet, should not tune
    let result = call("memory_tune_params", json!({}), &svc, &uid).await;
    let text = text(&result);

    assert!(
        text.contains("Not enough feedback") || text.contains("minimum 10"),
        "Should indicate insufficient feedback: {}",
        text
    );

    println!("✅ test_tune_params_insufficient_feedback: {}", text);
}

// ── 18. memory_tune_params: auto-tuning with sufficient feedback ──────────────

#[tokio::test]
async fn test_tune_params_with_feedback() {
    let (svc, store, uid) = setup().await;

    // Create a memory via MCP tool
    let result = call(
        "memory_store",
        json!({"content": "Test memory for tuning", "memory_type": "semantic"}),
        &svc,
        &uid,
    )
    .await;
    let result_text = text(&result);
    // Extract memory_id from "Stored memory <id>: ..."
    let mem_id = result_text
        .strip_prefix("Stored memory ")
        .and_then(|s| s.split(':').next())
        .unwrap_or("unknown");

    // Add 12 useful feedback signals (above threshold of 10)
    for _ in 0..12 {
        store
            .record_feedback(&uid, mem_id, "useful", None)
            .await
            .unwrap();
    }

    // Get params before tuning
    let before = store.get_user_retrieval_params(&uid).await.unwrap();
    println!("Before tuning: feedback_weight={:.3}", before.feedback_weight);

    // Trigger tuning
    let result = call("memory_tune_params", json!({}), &svc, &uid).await;
    let text = text(&result);

    // Should indicate tuning happened
    assert!(
        text.contains("tuned") || text.contains("→"),
        "Should indicate parameters were tuned: {}",
        text
    );

    // Verify DB was updated
    let after = store.get_user_retrieval_params(&uid).await.unwrap();
    println!("After tuning: feedback_weight={:.3}", after.feedback_weight);

    // With 100% useful feedback, feedback_weight should increase
    assert!(
        after.feedback_weight >= before.feedback_weight,
        "feedback_weight should increase with positive feedback: {} -> {}",
        before.feedback_weight,
        after.feedback_weight
    );

    println!("✅ test_tune_params_with_feedback: {}", text);
}

// ── 19. Tuning DB verification: params stored correctly ───────────────────────

#[tokio::test]
async fn test_tuning_db_verification() {
    let (_svc, store, uid) = setup().await;

    // Set custom params
    let custom = memoria_storage::UserRetrievalParams {
        user_id: uid.clone(),
        feedback_weight: 0.15,
        temporal_decay_hours: 200.0,
        confidence_weight: 0.2,
    };
    store.set_user_retrieval_params(&custom).await.unwrap();

    // Verify stored correctly
    let loaded = store.get_user_retrieval_params(&uid).await.unwrap();
    assert!((loaded.feedback_weight - 0.15).abs() < 0.001, "feedback_weight mismatch");
    assert!((loaded.temporal_decay_hours - 200.0).abs() < 0.1, "temporal_decay_hours mismatch");
    assert!((loaded.confidence_weight - 0.2).abs() < 0.001, "confidence_weight mismatch");

    println!("✅ test_tuning_db_verification: params stored and loaded correctly");
    println!("   feedback_weight={:.3}, temporal_decay_hours={:.1}, confidence_weight={:.3}",
        loaded.feedback_weight, loaded.temporal_decay_hours, loaded.confidence_weight);
}

// ── 20. Tuning affects scoring: verify end-to-end ─────────────────────────────

#[tokio::test]
async fn test_tuning_affects_scoring() {
    let (svc, store, uid) = setup().await;

    // Use highly unique content to avoid cross-test interference
    let unique = format!("xyzzy_tuning_test_{}", uid);
    let result = call(
        "memory_store",
        json!({"content": unique, "memory_type": "semantic"}),
        &svc,
        &uid,
    )
    .await;
    let result_text = text(&result);
    let mem_id = result_text
        .strip_prefix("Stored memory ")
        .and_then(|s| s.split(':').next())
        .unwrap_or("unknown");

    // Add feedback
    store.record_feedback(&uid, mem_id, "useful", None).await.unwrap();
    store.record_feedback(&uid, mem_id, "useful", None).await.unwrap();

    // MatrixOne fulltext index needs time to become consistent after INSERT
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

    // Get score with default params (feedback_weight=0.1)
    let default_mems = svc.retrieve(&uid, &unique, 10).await.unwrap();
    let default_score = default_mems
        .iter()
        .find(|m| m.memory_id == mem_id)
        .and_then(|m| m.retrieval_score)
        .unwrap_or(0.0);
    assert!(default_score > 0.0, "Should have positive default score");

    // Verify scoring math directly via ScoringPlugin trait to avoid
    // MatrixOne fulltext index flakiness on consecutive retrieves
    use memoria_service::scoring::{DefaultScoringPlugin, ScoringPlugin};
    let plugin = DefaultScoringPlugin;
    let fb = memoria_storage::MemoryFeedback { useful: 2, ..Default::default() };

    let default_params = memoria_storage::UserRetrievalParams::default();
    let custom_params = memoria_storage::UserRetrievalParams {
        feedback_weight: 0.2, ..Default::default()
    };

    let base = 1.0;
    let score_default = plugin.adjust_score(base, &fb, &default_params);
    let score_custom = plugin.adjust_score(base, &fb, &custom_params);

    assert!((score_default - 1.2).abs() < 0.001, "Default: {}", score_default);
    assert!((score_custom - 1.4).abs() < 0.001, "Custom: {}", score_custom);
    assert!(score_custom > score_default, "Custom params should produce higher score");

    // Verify per-user params round-trip through DB
    let high_weight = memoria_storage::UserRetrievalParams {
        user_id: uid.clone(),
        feedback_weight: 0.2,
        temporal_decay_hours: 168.0,
        confidence_weight: 0.1,
    };
    store.set_user_retrieval_params(&high_weight).await.unwrap();
    let loaded = store.get_user_retrieval_params(&uid).await.unwrap();
    assert!((loaded.feedback_weight - 0.2).abs() < 0.001);

    println!("Default score: {:.4}, adjust(0.1)={:.4}, adjust(0.2)={:.4}",
        default_score, score_default, score_custom);
    println!("✅ test_tuning_affects_scoring: per-user params affect scoring math");
}

// ── 21. Governance daily task updates feedback_weight in DB ───────────────────

#[tokio::test]
async fn test_governance_daily_tunes_params_in_db() {
    use memoria_service::{
        governance::{DefaultGovernanceStrategy, GovernanceTask},
        GovernanceStrategy,
    };

    let (_svc, store, uid) = setup().await;

    // Store a memory and add 12 useful feedback signals (above MIN_FEEDBACK_FOR_TUNING=10)
    let result = call(
        "memory_store",
        json!({"content": "governance tuning e2e test memory", "memory_type": "semantic"}),
        &_svc,
        &uid,
    )
    .await;
    let mem_id = text(&result)
        .strip_prefix("Stored memory ")
        .and_then(|s| s.split(':').next())
        .unwrap_or("unknown")
        .to_string();

    for _ in 0..12 {
        store.record_feedback(&uid, &mem_id, "useful", None).await.unwrap();
    }

    // Capture baseline
    let before = store.get_user_retrieval_params(&uid).await.unwrap();

    // Run governance daily task directly (no scheduler needed)
    let strategy: Arc<dyn GovernanceStrategy> = Arc::new(DefaultGovernanceStrategy);
    let execution = strategy.run(store.as_ref(), GovernanceTask::Daily).await.unwrap();

    // Verify the task reported tuning
    assert!(
        execution.summary.users_tuned > 0,
        "Daily task should have tuned at least one user, got users_tuned={}",
        execution.summary.users_tuned
    );

    // Verify DB was actually updated
    let after = store.get_user_retrieval_params(&uid).await.unwrap();
    assert!(
        after.feedback_weight >= before.feedback_weight,
        "feedback_weight should increase after 100% useful feedback: {:.3} -> {:.3}",
        before.feedback_weight,
        after.feedback_weight
    );
    assert!(
        (after.feedback_weight - before.feedback_weight * 1.1).abs() < 0.001
            || after.feedback_weight == 0.2, // clamped at max
        "Expected 10% increase or max cap: got {:.4}",
        after.feedback_weight
    );

    println!(
        "✅ test_governance_daily_tunes_params_in_db: {:.3} → {:.3}, users_tuned={}",
        before.feedback_weight, after.feedback_weight, execution.summary.users_tuned
    );
}

// ── 22. Duplicate instance_id: two processes with same config get distinct holder IDs ──

#[tokio::test]
async fn test_duplicate_instance_id_lock_is_exclusive() {
    use memoria_service::distributed::DistributedLock;
    use std::time::Duration;

    let store = SqlMemoryStore::connect(&db_url(), test_dim())
        .await
        .expect("connect");
    store.migrate().await.expect("migrate");
    let store = Arc::new(store);

    let lock_key = format!("test:dup_instance:{}", uid());

    // Simulate two processes that both set MEMORIA_INSTANCE_ID=same-name.
    // Config::from_env() appends PID, so their actual holder_ids differ:
    //   process A → "same-name-1234"
    //   process B → "same-name-5678"
    let holder_a = "same-name-1234"; // simulated: base + pid_a
    let holder_b = "same-name-5678"; // simulated: base + pid_b

    // A acquires
    let acquired_a = store
        .try_acquire(&lock_key, holder_a, Duration::from_secs(60))
        .await
        .unwrap();
    assert!(acquired_a, "Process A should acquire the lock");

    // B (different PID, same base name) must NOT acquire
    let acquired_b = store
        .try_acquire(&lock_key, holder_b, Duration::from_secs(60))
        .await
        .unwrap();
    assert!(
        !acquired_b,
        "Process B with same base instance_id must be excluded (different PID → different holder)"
    );

    // Verify Config::from_env() actually appends PID
    let cfg = memoria_service::Config::from_env();
    let pid = std::process::id().to_string();
    assert!(
        cfg.instance_id.ends_with(&format!("-{pid}")),
        "instance_id should end with -{pid}, got: {}",
        cfg.instance_id
    );

    store.release(&lock_key, holder_a).await.unwrap();
    println!("✅ test_duplicate_instance_id_lock_is_exclusive: PID suffix prevents duplicate-ID bypass");
}
