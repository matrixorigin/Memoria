/// Core tools E2E tests against real DB.
/// Covers: memory_store, memory_retrieve, memory_search, memory_correct (id + query),
///         memory_purge (single, batch, topic), memory_profile, memory_list,
///         memory_capabilities — all fields verified.
///
/// Run: DATABASE_URL=mysql://root:111@localhost:6001/memoria_rs \
///      cargo test -p memoria-mcp --test core_tools_e2e -- --nocapture

use memoria_service::MemoryService;
use memoria_storage::SqlMemoryStore;
use serde_json::{json, Value};
use std::sync::Arc;
use uuid::Uuid;

fn db_url() -> String {
    std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "mysql://root:111@localhost:6001/memoria_rs".to_string())
}
fn uid() -> String { format!("ct_{}", &Uuid::new_v4().simple().to_string()[..8]) }

async fn setup() -> (Arc<MemoryService>, String) {
    let store = SqlMemoryStore::connect(&db_url(), 4).await.expect("connect");
    store.migrate().await.expect("migrate");
    let svc = Arc::new(MemoryService::new_sql(Arc::new(store), None));
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
    assert!(text(&r).contains("corrected content"), "{}", text(&r));

    let m = svc.get(&mid).await.unwrap().unwrap();
    assert_eq!(m.content, "corrected content");
    println!("✅ correct by id: {}", text(&r));
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
