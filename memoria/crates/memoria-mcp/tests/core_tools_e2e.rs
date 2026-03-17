/// Core tools E2E tests against real DB.
/// Covers: memory_store, memory_retrieve, memory_search, memory_correct (id + query),
///         memory_purge (single, batch, topic), memory_profile, memory_list,
///         memory_capabilities — all fields verified.
///
/// Run: DATABASE_URL=mysql://root:111@localhost:6001/memoria \
///      cargo test -p memoria-mcp --test core_tools_e2e -- --nocapture

use memoria_service::MemoryService;
use memoria_storage::SqlMemoryStore;
use serde_json::{json, Value};
use sqlx::Row;
use std::sync::Arc;
use uuid::Uuid;

fn test_dim() -> usize {
    std::env::var("EMBEDDING_DIM").ok().and_then(|s| s.parse().ok()).unwrap_or(1024)
}
fn db_url() -> String {
    std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "mysql://root:111@localhost:6001/memoria".to_string())
}
fn uid() -> String { format!("ct_{}", &Uuid::new_v4().simple().to_string()[..8]) }

/// Returns LlmClient if LLM_API_KEY is set, else None.
fn try_llm() -> Option<Arc<memoria_embedding::LlmClient>> {
    let key = std::env::var("LLM_API_KEY").ok().filter(|s| !s.is_empty())?;
    let base = std::env::var("LLM_BASE_URL").unwrap_or_else(|_| "https://api.openai.com/v1".into());
    let model = std::env::var("LLM_MODEL").unwrap_or_else(|_| "gpt-4o-mini".into());
    Some(Arc::new(memoria_embedding::LlmClient::new(key, base, model)))
}

async fn setup() -> (Arc<MemoryService>, String) {
    let store = SqlMemoryStore::connect(&db_url(), test_dim()).await.expect("connect");
    store.migrate().await.expect("migrate");
    let svc = Arc::new(MemoryService::new_sql_with_llm(Arc::new(store), None, None));
    (svc, uid())
}

async fn call(name: &str, args: Value, svc: &Arc<MemoryService>, uid: &str) -> Value {
    memoria_mcp::tools::call(name, args, svc, uid).await.expect(name)
}
fn text(v: &Value) -> &str { v["content"][0]["text"].as_str().unwrap_or("") }

// ── 1. memory_store: all memory_type variants ─────────────────────────────────

#[tokio::test]
async fn test_store_all_memory_types() {
    let (svc, uid) = setup().await;
    for mt in &["semantic", "profile", "procedural", "working", "tool_result", "episodic"] {
        let r = call("memory_store", json!({"content": format!("{mt} content"), "memory_type": mt}), &svc, &uid).await;
        assert!(text(&r).contains("Stored memory"), "type={mt}: {}", text(&r));
    }
    let list = svc.list_active(&uid, 10).await.unwrap();
    assert_eq!(list.len(), 6);
    println!("✅ store all 6 memory types");
}

// ── 2. memory_store: session_id and trust_tier round-trip ────────────────────

#[tokio::test]
async fn test_store_session_and_trust_tier() {
    let (svc, uid) = setup().await;
    let r = call("memory_store",
        json!({"content": "session memory", "session_id": "sess-abc", "trust_tier": "T1"}),
        &svc, &uid).await;
    let t = text(&r);
    assert!(t.contains("Stored"), "{t}");
    let mid = t.split_whitespace().nth(2).unwrap().trim_end_matches(':');
    let m = svc.get(mid).await.unwrap().unwrap();
    assert_eq!(m.session_id.as_deref(), Some("sess-abc"));
    assert_eq!(m.trust_tier.to_string(), "T1");
    println!("✅ store session_id + trust_tier: session={:?} tier={}", m.session_id, m.trust_tier);
}

// ── 3. memory_retrieve: returns relevant memories ────────────────────────────

#[tokio::test]
async fn test_retrieve_finds_relevant() {
    let (svc, uid) = setup().await;
    call("memory_store", json!({"content": "rust ownership model"}), &svc, &uid).await;
    call("memory_store", json!({"content": "python list comprehension"}), &svc, &uid).await;

    let r = call("memory_retrieve", json!({"query": "rust", "top_k": 5}), &svc, &uid).await;
    assert!(text(&r).contains("rust ownership"), "got: {}", text(&r));
    println!("✅ retrieve finds relevant: {}", text(&r));
}

// ── 4. memory_retrieve: empty returns "No relevant memories" ─────────────────

#[tokio::test]
async fn test_retrieve_empty() {
    let (svc, uid) = setup().await;
    let r = call("memory_retrieve", json!({"query": "nothing here"}), &svc, &uid).await;
    assert!(text(&r).contains("No relevant memories"), "{}", text(&r));
    println!("✅ retrieve empty: {}", text(&r));
}

// ── 5. memory_search: top_k respected ────────────────────────────────────────

#[tokio::test]
async fn test_search_top_k() {
    let (svc, uid) = setup().await;
    for i in 0..5 {
        call("memory_store", json!({"content": format!("search item {i}")}), &svc, &uid).await;
    }
    let r = call("memory_search", json!({"query": "search item", "top_k": 3}), &svc, &uid).await;
    let t = text(&r);
    let count = t.lines().count();
    assert!(count <= 3, "expected ≤3 results, got {count}: {t}");
    println!("✅ search top_k=3: {count} results");
}

// ── 6. memory_correct by memory_id ───────────────────────────────────────────

#[tokio::test]
async fn test_correct_by_id() {
    let (svc, uid) = setup().await;
    let stored = call("memory_store", json!({"content": "old content"}), &svc, &uid).await;
    let mid = text(&stored).split_whitespace().nth(2).unwrap().trim_end_matches(':').to_string();

    let r = call("memory_correct",
        json!({"memory_id": mid, "new_content": "corrected content", "reason": "test"}),
        &svc, &uid).await;
    let t = text(&r);
    assert!(t.contains("corrected content"), "{t}");

    // correct creates a new memory — extract new id from response
    let new_mid = t.split_whitespace().nth(2).unwrap_or("").trim_end_matches(':');
    assert!(!new_mid.is_empty(), "should have new memory_id");
    assert_ne!(new_mid, mid, "new memory should have different id");

    // New memory should have corrected content
    let new = svc.get(new_mid).await.unwrap().unwrap();
    assert_eq!(new.content, "corrected content");
    assert!(new.is_active, "new memory should be active");

    // Old memory should be deactivated (get returns None for inactive)
    let old = svc.get(&mid).await.unwrap();
    assert!(old.is_none(), "old memory should not be returned by get (deactivated)");
    println!("✅ correct by id: old={mid} → new={new_mid} (superseded_by chain)");
}

// ── 7. memory_correct by query (semantic search) ─────────────────────────────

#[tokio::test]
async fn test_correct_by_query() {
    let (svc, uid) = setup().await;
    call("memory_store", json!({"content": "uses black for formatting"}), &svc, &uid).await;

    let r = call("memory_correct",
        json!({"query": "formatting tool", "new_content": "uses ruff for formatting", "reason": "switched"}),
        &svc, &uid).await;
    assert!(text(&r).contains("ruff"), "got: {}", text(&r));
    println!("✅ correct by query: {}", text(&r));
}

// ── 8. memory_correct: no target returns error ───────────────────────────────

#[tokio::test]
async fn test_correct_no_target() {
    let (svc, uid) = setup().await;
    let r = call("memory_correct", json!({"new_content": "something"}), &svc, &uid).await;
    assert!(text(&r).contains("memory_id") || text(&r).contains("query"), "{}", text(&r));
    println!("✅ correct no target: {}", text(&r));
}

// ── 9. memory_correct: no new_content returns error ──────────────────────────

#[tokio::test]
async fn test_correct_no_content() {
    let (svc, uid) = setup().await;
    let r = call("memory_correct", json!({"memory_id": "some-id"}), &svc, &uid).await;
    assert!(text(&r).contains("new_content"), "{}", text(&r));
    println!("✅ correct no content: {}", text(&r));
}

// ── 10. memory_purge: single ID ───────────────────────────────────────────────

#[tokio::test]
async fn test_purge_single() {
    let (svc, uid) = setup().await;
    let stored = call("memory_store", json!({"content": "to delete"}), &svc, &uid).await;
    let mid = text(&stored).split_whitespace().nth(2).unwrap().trim_end_matches(':').to_string();

    let r = call("memory_purge", json!({"memory_id": mid}), &svc, &uid).await;
    assert!(text(&r).contains("1"), "{}", text(&r));
    assert!(svc.get(&mid).await.unwrap().is_none());
    println!("✅ purge single: {}", text(&r));
}

// ── 11. memory_purge: batch comma-separated IDs ───────────────────────────────

#[tokio::test]
async fn test_purge_batch() {
    let (svc, uid) = setup().await;
    let mut ids = vec![];
    for i in 0..3 {
        let r = call("memory_store", json!({"content": format!("batch {i}")}), &svc, &uid).await;
        let mid = text(&r).split_whitespace().nth(2).unwrap().trim_end_matches(':').to_string();
        ids.push(mid);
    }
    let batch = ids.join(",");
    let r = call("memory_purge", json!({"memory_id": batch}), &svc, &uid).await;
    assert!(text(&r).contains("3"), "{}", text(&r));
    assert_eq!(svc.list_active(&uid, 10).await.unwrap().len(), 0);
    println!("✅ purge batch 3: {}", text(&r));
}

// ── 12. memory_purge: topic bulk delete ───────────────────────────────────────

#[tokio::test]
async fn test_purge_topic() {
    let (svc, uid) = setup().await;
    call("memory_store", json!({"content": "rust ownership rules"}), &svc, &uid).await;
    call("memory_store", json!({"content": "rust borrow checker"}), &svc, &uid).await;
    call("memory_store", json!({"content": "python is great"}), &svc, &uid).await;

    let r = call("memory_purge", json!({"topic": "rust", "reason": "cleanup"}), &svc, &uid).await;
    let t = text(&r);
    // Should have deleted the rust memories
    assert!(t.contains("Purged"), "{t}");
    println!("✅ purge topic 'rust': {t}");
}

// ── 13. memory_purge: no target returns error ─────────────────────────────────

#[tokio::test]
async fn test_purge_no_target() {
    let (svc, uid) = setup().await;
    let r = call("memory_purge", json!({}), &svc, &uid).await;
    assert!(text(&r).contains("memory_id") || text(&r).contains("topic"), "{}", text(&r));
    println!("✅ purge no target: {}", text(&r));
}

// ── 14. memory_profile: returns profile memories ─────────────────────────────

#[tokio::test]
async fn test_profile() {
    let (svc, uid) = setup().await;
    call("memory_store", json!({"content": "prefers tabs", "memory_type": "profile"}), &svc, &uid).await;
    call("memory_store", json!({"content": "uses vim", "memory_type": "profile"}), &svc, &uid).await;
    call("memory_store", json!({"content": "some semantic fact"}), &svc, &uid).await;

    let r = call("memory_profile", json!({}), &svc, &uid).await;
    let t = text(&r);
    assert!(t.contains("prefers tabs"), "{t}");
    assert!(t.contains("uses vim"), "{t}");
    assert!(!t.contains("some semantic fact"), "should only show profile memories: {t}");
    println!("✅ profile: {t}");
}

// ── 15. memory_profile: empty returns message ────────────────────────────────

#[tokio::test]
async fn test_profile_empty() {
    let (svc, uid) = setup().await;
    let r = call("memory_profile", json!({}), &svc, &uid).await;
    assert!(text(&r).contains("No profile"), "{}", text(&r));
    println!("✅ profile empty: {}", text(&r));
}

// ── 16. memory_list: limit respected, shows all fields ───────────────────────

#[tokio::test]
async fn test_list_limit_and_fields() {
    let (svc, uid) = setup().await;
    for i in 0..5 {
        call("memory_store", json!({"content": format!("list item {i}")}), &svc, &uid).await;
    }
    let r = call("memory_list", json!({"limit": 3}), &svc, &uid).await;
    let t = text(&r);
    let count = t.lines().count();
    assert_eq!(count, 3, "expected 3 lines, got {count}: {t}");
    // Each line should have memory_id, type, content
    for line in t.lines() {
        assert!(line.contains('['), "missing memory_id bracket: {line}");
        assert!(line.contains('('), "missing type parens: {line}");
    }
    println!("✅ list limit=3: {count} items with id+type+content");
}

// ── 17. memory_capabilities: lists all tools ─────────────────────────────────

#[tokio::test]
async fn test_capabilities() {
    let (svc, uid) = setup().await;
    let r = call("memory_capabilities", json!({}), &svc, &uid).await;
    let t = text(&r);
    for tool in &["memory_store", "memory_retrieve", "memory_search",
                  "memory_correct", "memory_purge", "memory_profile"] {
        assert!(t.contains(tool), "missing {tool}: {t}");
    }
    println!("✅ capabilities: {t}");
}

// ── 18. unknown tool returns error ────────────────────────────────────────────

#[tokio::test]
async fn test_unknown_tool_errors() {
    let (svc, uid) = setup().await;
    let result = memoria_mcp::tools::call("nonexistent", json!({}), &svc, &uid).await;
    assert!(result.is_err());
    println!("✅ unknown tool → error");
}

// ── 19. memory_governance: quarantine + cleanup + cooldown ───────────────────

#[tokio::test]
async fn test_governance_quarantine_and_cooldown() {
    let (svc, uid) = setup().await;

    // Store a memory with very low initial_confidence (T4 = 0.4) and old observed_at
    // so effective_confidence = 0.4 * exp(-365/30) ≈ 0 < 0.2 threshold
    let sql = svc.sql_store.as_ref().unwrap();
    let mid = format!("gov_{}", uuid::Uuid::new_v4().simple());
    sqlx::query(
        "INSERT INTO mem_memories (memory_id, user_id, memory_type, content, source_event_ids, \
         is_active, trust_tier, initial_confidence, observed_at, created_at) \
         VALUES (?, ?, 'semantic', 'old low confidence memory', '[]', 1, 'T4', 0.4, \
         DATE_SUB(NOW(), INTERVAL 365 DAY), NOW())"
    )
    .bind(&mid).bind(&uid)
    .execute(sql.pool()).await.expect("insert old memory");

    // Store a high-confidence memory that should NOT be quarantined
    call("memory_store", json!({"content": "recent high confidence", "trust_tier": "T1"}), &svc, &uid).await;

    // Run governance
    let r = call("memory_governance", json!({"force": true}), &svc, &uid).await;
    let t = text(&r);
    assert!(t.contains("quarantined=1"), "expected 1 quarantined, got: {t}");
    assert!(t.contains("Governance complete"), "{t}");
    println!("✅ governance: {t}");

    // Verify the old memory is now inactive in DB
    let row = sqlx::query("SELECT is_active FROM mem_memories WHERE memory_id = ?")
        .bind(&mid).fetch_one(sql.pool()).await.expect("fetch");
    let active: i8 = row.try_get("is_active").unwrap_or(1);
    assert_eq!(active, 0, "quarantined memory should have is_active=0");
    println!("✅ quarantined memory has is_active=0 in DB");

    // High-confidence memory should still be active
    let list = svc.list_active(&uid, 10).await.unwrap();
    assert!(list.iter().any(|m| m.content == "recent high confidence"));
    println!("✅ high-confidence memory still active");

    // Cooldown: second call without force should be skipped
    let r2 = call("memory_governance", json!({}), &svc, &uid).await;
    assert!(text(&r2).contains("cooldown"), "expected cooldown message: {}", text(&r2));
    println!("✅ cooldown enforced: {}", text(&r2));

    // force=true bypasses cooldown
    let r3 = call("memory_governance", json!({"force": true}), &svc, &uid).await;
    assert!(text(&r3).contains("Governance complete"), "{}", text(&r3));
    println!("✅ force=true bypasses cooldown");
}

// ── 20. memory_governance: cleanup_stale removes soft-deleted low-confidence ──

#[tokio::test]
async fn test_governance_cleanup_stale() {
    let (svc, uid) = setup().await;
    let sql = svc.sql_store.as_ref().unwrap();

    // Insert a soft-deleted memory with very low confidence
    let mid = format!("stale_{}", uuid::Uuid::new_v4().simple());
    sqlx::query(
        "INSERT INTO mem_memories (memory_id, user_id, memory_type, content, source_event_ids, \
         is_active, trust_tier, initial_confidence, observed_at, created_at) \
         VALUES (?, ?, 'semantic', 'stale deleted memory', '[]', 0, 'T4', 0.05, NOW(), NOW())"
    )
    .bind(&mid).bind(&uid)
    .execute(sql.pool()).await.expect("insert stale");

    let r = call("memory_governance", json!({"force": true}), &svc, &uid).await;
    let t = text(&r);
    assert!(t.contains("cleaned_stale=1"), "expected 1 cleaned, got: {t}");
    println!("✅ cleanup_stale: {t}");

    // Verify physically deleted
    let row = sqlx::query("SELECT COUNT(*) as cnt FROM mem_memories WHERE memory_id = ?")
        .bind(&mid).fetch_one(sql.pool()).await.expect("fetch");
    let cnt: i64 = row.try_get("cnt").unwrap_or(1);
    assert_eq!(cnt, 0, "stale memory should be physically deleted");
    println!("✅ stale memory physically deleted from DB");
}

// ── 21. memory_consolidate: cooldown and basic run ──────────────────────────

#[tokio::test]
async fn test_consolidate_basic() {
    let (svc, uid) = setup().await;

    // First run should succeed
    let r = call("memory_consolidate", json!({"force": true}), &svc, &uid).await;
    let t = text(&r);
    assert!(t.contains("Consolidation complete"), "got: {t}");
    assert!(t.contains("conflicts_detected="), "got: {t}");
    println!("✅ consolidate basic: {t}");

    // Second run without force should hit cooldown
    let r2 = call("memory_consolidate", json!({}), &svc, &uid).await;
    let t2 = text(&r2);
    assert!(t2.contains("cooldown"), "expected cooldown, got: {t2}");
    println!("✅ consolidate cooldown: {t2}");
}

// ── 22. memory_reflect: returns candidates ───────────────────────────────────

#[tokio::test]
async fn test_reflect_candidates() {
    let (svc, uid) = setup().await;

    // Store a few memories first
    call("memory_store", json!({"content": "Uses Rust for backend services", "memory_type": "semantic"}), &svc, &uid).await;
    call("memory_store", json!({"content": "Prefers async/await patterns", "memory_type": "profile"}), &svc, &uid).await;

    let r = call("memory_reflect", json!({"mode": "candidates"}), &svc, &uid).await;
    let t = text(&r);
    assert!(t.contains("Cluster") || t.contains("memory clusters") || t.contains("No memories"), "got: {t}");
    println!("✅ reflect candidates: {}", &t[..t.len().min(120)]);
}

// ── 23. memory_reflect: internal mode returns error ──────────────────────────

#[tokio::test]
async fn test_reflect_internal_unavailable() {
    let (svc, uid) = setup().await;
    let r = call("memory_reflect", json!({"mode": "internal"}), &svc, &uid).await;
    let t = text(&r);
    assert!(t.contains("not available") || t.contains("requires"), "got: {t}");
    println!("✅ reflect internal unavailable: {t}");
}

// ── 24. memory_extract_entities + memory_link_entities ───────────────────────

#[tokio::test]
async fn test_extract_and_link_entities() {
    let (svc, uid) = setup().await;

    // Store a memory
    let store_r = call("memory_store", json!({"content": "Project uses Rust and MatrixOne database"}), &svc, &uid).await;
    let store_t = text(&store_r);
    println!("store: {store_t}");

    // Extract candidates
    let r = call("memory_extract_entities", json!({"mode": "candidates"}), &svc, &uid).await;
    let t = text(&r);
    println!("extract: {}", &t[..t.len().min(200)]);

    let parsed: serde_json::Value = serde_json::from_str(&t).unwrap_or(serde_json::Value::Null);
    if parsed["status"] == "complete" {
        println!("✅ no unlinked memories (already linked or empty)");
        return;
    }
    assert_eq!(parsed["status"], "candidates", "got: {t}");
    let memories = parsed["memories"].as_array().expect("memories array");
    assert!(!memories.is_empty(), "should have unlinked memories");

    let memory_id = memories[0]["memory_id"].as_str().expect("memory_id");

    // Link entities
    let link_payload = serde_json::to_string(&json!([{
        "memory_id": memory_id,
        "entities": [{"name": "Rust", "type": "tech"}, {"name": "MatrixOne", "type": "tech"}]
    }])).unwrap();

    let r2 = call("memory_link_entities", json!({"entities": link_payload}), &svc, &uid).await;
    let t2 = text(&r2);
    let parsed2: serde_json::Value = serde_json::from_str(&t2).expect("valid json");
    assert_eq!(parsed2["status"], "done", "got: {t2}");
    assert!(parsed2["entities_created"].as_i64().unwrap_or(0) >= 1, "got: {t2}");
    println!("✅ link_entities: {t2}");

    // Re-extract: this memory should now be linked
    let r3 = call("memory_extract_entities", json!({"mode": "candidates"}), &svc, &uid).await;
    let t3 = text(&r3);
    let parsed3: serde_json::Value = serde_json::from_str(&t3).unwrap_or(serde_json::Value::Null);
    // The linked memory should no longer appear in unlinked list
    if parsed3["status"] == "candidates" {
        let mems = parsed3["memories"].as_array().unwrap();
        assert!(!mems.iter().any(|m| m["memory_id"] == memory_id), "linked memory should not appear again");
    }
    println!("✅ extract after link: {}", &t3[..t3.len().min(100)]);
}

// ── 25. memory_link_entities: invalid JSON returns error ─────────────────────

#[tokio::test]
async fn test_link_entities_invalid_json() {
    let (svc, uid) = setup().await;
    let r = call("memory_link_entities", json!({"entities": "not json"}), &svc, &uid).await;
    let t = text(&r);
    let parsed: serde_json::Value = serde_json::from_str(&t).expect("valid json");
    assert_eq!(parsed["status"], "error");
    println!("✅ link_entities invalid json: {t}");
}

// ── LLM-related tests ────────────────────────────────────────────────────────

/// Helper: create service with explicit LLM client (for testing LLM paths).
async fn setup_with_llm(llm: Option<Arc<memoria_embedding::LlmClient>>) -> (Arc<MemoryService>, String) {
    let store = SqlMemoryStore::connect(&db_url(), test_dim()).await.expect("connect");
    store.migrate().await.expect("migrate");
    let svc = Arc::new(MemoryService::new_sql_with_llm(Arc::new(store), None, llm));
    (svc, uid())
}

// ── 26. reflect without LLM returns candidates (not error) ───────────────────

#[tokio::test]
async fn test_reflect_no_llm_returns_candidates() {
    let (svc, uid) = setup_with_llm(None).await;

    // Store memories in two different "sessions" to create clusters
    call("memory_store", json!({"content": "Uses Rust for backend", "session_id": "s1"}), &svc, &uid).await;
    call("memory_store", json!({"content": "Prefers async patterns", "session_id": "s1"}), &svc, &uid).await;
    call("memory_store", json!({"content": "MatrixOne as database", "session_id": "s2"}), &svc, &uid).await;
    call("memory_store", json!({"content": "Deploys with Docker", "session_id": "s2"}), &svc, &uid).await;

    let r = call("memory_reflect", json!({"mode": "auto", "force": true}), &svc, &uid).await;
    let t = text(&r);
    // Without LLM: should return candidates or "no clusters" — NOT an error
    assert!(
        t.contains("cluster") || t.contains("Cluster") || t.contains("No memory") || t.contains("memories"),
        "without LLM should return candidates or no-clusters message, got: {t}"
    );
    // Must NOT say "error" or "failed"
    assert!(!t.to_lowercase().contains("error"), "should not return error without LLM, got: {t}");
    println!("✅ reflect without LLM: {}", &t[..t.len().min(100)]);
}

// ── 27. reflect with mode=candidates always returns candidates ────────────────

#[tokio::test]
async fn test_reflect_candidates_mode_no_llm_needed() {
    let (svc, uid) = setup_with_llm(None).await;
    call("memory_store", json!({"content": "Test memory for candidates mode"}), &svc, &uid).await;

    let r = call("memory_reflect", json!({"mode": "candidates", "force": true}), &svc, &uid).await;
    let t = text(&r);
    // candidates mode should always work regardless of LLM
    assert!(
        t.contains("cluster") || t.contains("Cluster") || t.contains("No memory") || t.contains("memories"),
        "candidates mode should work without LLM, got: {t}"
    );
    println!("✅ reflect candidates mode (no LLM): {}", &t[..t.len().min(100)]);
}

// ── 28. reflect with mode=internal and no LLM returns clear error ─────────────

#[tokio::test]
async fn test_reflect_internal_no_llm_returns_error() {
    let (svc, uid) = setup_with_llm(None).await;
    let r = call("memory_reflect", json!({"mode": "internal"}), &svc, &uid).await;
    let t = text(&r);
    assert!(
        t.contains("LLM_API_KEY") || t.contains("requires") || t.contains("not configured"),
        "internal mode without LLM should return clear error, got: {t}"
    );
    println!("✅ reflect internal without LLM: {t}");
}

// ── 29. extract_entities without LLM returns candidates ──────────────────────

#[tokio::test]
async fn test_extract_entities_no_llm_returns_candidates() {
    let (svc, uid) = setup_with_llm(None).await;
    call("memory_store", json!({"content": "Project uses Rust and MatrixOne"}), &svc, &uid).await;

    let r = call("memory_extract_entities", json!({"mode": "auto"}), &svc, &uid).await;
    let t = text(&r);
    let parsed: serde_json::Value = serde_json::from_str(&t).unwrap_or(serde_json::Value::Null);
    // Should return candidates or complete (if regex already linked), NOT error
    assert!(
        parsed["status"] == "candidates" || parsed["status"] == "complete",
        "without LLM should return candidates or complete, got: {t}"
    );
    println!("✅ extract_entities without LLM: status={}", parsed["status"]);
}

// ── 30. extract_entities with mode=internal and no LLM returns error ──────────

#[tokio::test]
async fn test_extract_entities_internal_no_llm_returns_error() {
    let (svc, uid) = setup_with_llm(None).await;
    let r = call("memory_extract_entities", json!({"mode": "internal"}), &svc, &uid).await;
    let t = text(&r);
    assert!(
        t.contains("LLM_API_KEY") || t.contains("requires") || t.contains("not configured"),
        "internal mode without LLM should return clear error, got: {t}"
    );
    println!("✅ extract_entities internal without LLM: {t}");
}

// ── 31. reflect with LLM (skipped if LLM_API_KEY not set) ────────────────────

#[tokio::test]
async fn test_reflect_with_llm_if_configured() {
    let Some(llm) = try_llm() else {
        println!("⏭️  test_reflect_with_llm skipped (LLM_API_KEY not set)");
        return;
    };
    let (svc, uid) = setup_with_llm(Some(llm)).await;

    for i in 0..4 {
        let sid = if i < 2 { "llm_s1" } else { "llm_s2" };
        call("memory_store", json!({
            "content": format!("Memory {} for LLM reflect test", i),
            "session_id": sid
        }), &svc, &uid).await;
    }

    let r = call("memory_reflect", json!({"mode": "auto", "force": true}), &svc, &uid).await;
    let t = text(&r);
    assert!(
        t.contains("scenes_created") || t.contains("cluster") || t.contains("No memory"),
        "with LLM should return reflect result, got: {t}"
    );
    println!("✅ reflect with LLM: {t}");
}

// ── 32. extract_entities with LLM (skipped if LLM_API_KEY not set) ───────────

#[tokio::test]
async fn test_extract_entities_with_llm_if_configured() {
    let Some(llm) = try_llm() else {
        println!("⏭️  test_extract_entities_with_llm skipped (LLM_API_KEY not set)");
        return;
    };
    let (svc, uid) = setup_with_llm(Some(llm)).await;

    call("memory_store", json!({"content": "Project uses Rust and MatrixOne database"}), &svc, &uid).await;

    let r = call("memory_extract_entities", json!({"mode": "auto"}), &svc, &uid).await;
    let t = text(&r);
    let parsed: serde_json::Value = serde_json::from_str(&t).unwrap_or(serde_json::Value::Null);
    assert!(
        parsed["status"] == "done" || parsed["status"] == "complete",
        "with LLM should return done or complete, got: {t}"
    );
    println!("✅ extract_entities with LLM: {t}");
}

// ── 33. retrieve: fulltext fallback when no embedding ────────────────────────

#[tokio::test]
async fn test_retrieve_fulltext_fallback() {
    let (svc, uid) = setup().await;
    // Store a memory with a unique keyword
    call("memory_store", json!({"content": "xylophone_unique_test_keyword_42"}), &svc, &uid).await;
    // Retrieve by exact keyword — should work via fulltext even without embedding
    let r = call("memory_retrieve", json!({"query": "xylophone_unique_test_keyword_42", "top_k": 5}), &svc, &uid).await;
    let t = text(&r);
    assert!(t.contains("xylophone_unique_test_keyword_42"), "fulltext fallback should find exact keyword: {t}");
    println!("✅ retrieve fulltext fallback works");
}

// ── 34. search: top_k=0 returns empty ────────────────────────────────────────

#[tokio::test]
async fn test_search_top_k_zero() {
    let (svc, uid) = setup().await;
    call("memory_store", json!({"content": "topk zero test"}), &svc, &uid).await;
    let r = call("memory_search", json!({"query": "topk zero", "top_k": 0}), &svc, &uid).await;
    let t = text(&r);
    // top_k=0 should return no results or "No relevant memories"
    assert!(t.contains("No relevant") || !t.contains("topk zero test"),
        "top_k=0 should return empty: {t}");
    println!("✅ search top_k=0: {}", &t[..t.len().min(80)]);
}

// ── 35. store + retrieve with session_id ─────────────────────────────────────

#[tokio::test]
async fn test_store_with_session_id_retrievable() {
    let (svc, uid) = setup().await;
    let sid = format!("sess_{}", uuid::Uuid::new_v4().simple().to_string()[..8].to_string());
    call("memory_store", json!({"content": "session-specific memory alpha", "session_id": sid}), &svc, &uid).await;
    let r = call("memory_retrieve", json!({"query": "session-specific alpha", "top_k": 5}), &svc, &uid).await;
    let t = text(&r);
    assert!(t.contains("session-specific memory alpha"), "should find session memory: {t}");
    println!("✅ store with session_id retrievable");
}

// ── 36. purge by topic then verify gone ──────────────────────────────────────

#[tokio::test]
async fn test_purge_topic_then_search_empty() {
    let (svc, uid) = setup().await;
    let tag = format!("purgetopic_{}", uuid::Uuid::new_v4().simple().to_string()[..6].to_string());
    for i in 0..3 {
        call("memory_store", json!({"content": format!("{tag} item {i}")}), &svc, &uid).await;
    }
    // Purge by topic
    let r = call("memory_purge", json!({"topic": &tag}), &svc, &uid).await;
    let t = text(&r);
    assert!(t.contains("Purged") || t.contains("purged") || t.contains("Deleted"), "purge result: {t}");
    println!("✅ purge by topic: {t}");

    // Search should find nothing
    let r = call("memory_search", json!({"query": &tag, "top_k": 10}), &svc, &uid).await;
    let t = text(&r);
    assert!(t.contains("No relevant") || !t.contains(&tag), "should be empty after purge: {t}");
    println!("✅ search after purge: empty");
}

// ── 37. explain mode on retrieve ─────────────────────────────────────────────

#[tokio::test]
async fn test_retrieve_explain_mode() {
    let (svc, uid) = setup().await;
    call("memory_store", json!({"content": "explain test memory alpha"}), &svc, &uid).await;

    let r = call("memory_retrieve", json!({"query": "explain test alpha", "top_k": 5, "explain": true}), &svc, &uid).await;
    let t = text(&r);
    assert!(t.contains("--- explain ---"), "should contain explain section: {t}");
    assert!(t.contains("\"path\""), "should contain path field: {t}");
    assert!(t.contains("\"total_ms\""), "should contain total_ms: {t}");
    println!("✅ retrieve explain mode: {}", &t[t.find("--- explain ---").unwrap_or(0)..]);
}

#[tokio::test]
async fn test_retrieve_explain_empty() {
    let (svc, uid) = setup().await;
    let r = call("memory_retrieve", json!({"query": "nonexistent_xyz_123", "top_k": 5, "explain": true}), &svc, &uid).await;
    let t = text(&r);
    assert!(t.contains("No relevant memories"), "should say no results: {t}");
    assert!(t.contains("--- explain ---"), "should still have explain: {t}");
    println!("✅ retrieve explain empty: has stats even with no results");
}

#[tokio::test]
async fn test_search_explain_mode() {
    let (svc, uid) = setup().await;
    call("memory_store", json!({"content": "search explain test beta"}), &svc, &uid).await;

    let r = call("memory_search", json!({"query": "search explain beta", "top_k": 5, "explain": true}), &svc, &uid).await;
    let t = text(&r);
    assert!(t.contains("--- explain ---"), "search should also support explain: {t}");
    println!("✅ search explain mode works");
}
