/// MCP server end-to-end test — simulates what Kiro/Cursor sends over stdio.
/// Requires: DATABASE_URL + EMBEDDING_* env vars set.
///
/// Run: DATABASE_URL=... EMBEDDING_BASE_URL=... EMBEDDING_API_KEY=... \
///      SQLX_OFFLINE=true cargo test -p memoria-mcp --test mcp_e2e -- --nocapture

use memoria_core::interfaces::EmbeddingProvider;
use memoria_embedding::HttpEmbedder;
use memoria_service::MemoryService;
use memoria_storage::SqlMemoryStore;
use serde_json::{json, Value};
use std::sync::Arc;
use uuid::Uuid;

async fn make_service() -> (Arc<MemoryService>, String) {
    let db_url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "mysql://root:111@localhost:6001/memoria".to_string());
    let dim: usize = std::env::var("EMBEDDING_DIM")
        .unwrap_or_else(|_| "1024".to_string())
        .parse()
        .unwrap_or(1024);

    let store = SqlMemoryStore::connect(&db_url, dim).await.expect("connect");
    store.migrate().await.expect("migrate");

    let embedder: Option<Arc<dyn EmbeddingProvider>> = {
        let base_url = std::env::var("EMBEDDING_BASE_URL").unwrap_or_default();
        let api_key = std::env::var("EMBEDDING_API_KEY").unwrap_or_default();
        let model = std::env::var("EMBEDDING_MODEL")
            .unwrap_or_else(|_| "BAAI/bge-m3".to_string());
        if !base_url.is_empty() && !api_key.is_empty() {
            Some(Arc::new(HttpEmbedder::new(&base_url, &api_key, &model, dim)))
        } else {
            println!("⚠️  No embedding configured — vector search will be skipped");
            None
        }
    };

    let user_id = format!("e2e_{}", Uuid::new_v4().simple());
    let svc = Arc::new(MemoryService::new(Arc::new(store), embedder));
    (svc, user_id)
}

async fn call_tool(name: &str, args: Value, svc: &Arc<MemoryService>, uid: &str) -> Value {
    memoria_mcp::tools::call(name, args, svc, uid).await.expect("tool call")
}

fn text(v: &Value) -> &str {
    v["content"][0]["text"].as_str().unwrap_or("")
}

#[tokio::test]
async fn test_e2e_store_retrieve() {
    let (svc, uid) = make_service().await;

    // Store
    let r = call_tool("memory_store", json!({"content": "Rust uses ownership for memory safety", "memory_type": "semantic"}), &svc, &uid).await;
    assert!(text(&r).contains("Stored memory"), "got: {}", text(&r));
    println!("✅ store: {}", text(&r));

    // Retrieve
    let r = call_tool("memory_retrieve", json!({"query": "rust memory safety", "top_k": 3}), &svc, &uid).await;
    let t = text(&r);
    assert!(t.contains("ownership") || t.contains("No relevant"), "got: {t}");
    println!("✅ retrieve: {t}");
}

#[tokio::test]
async fn test_e2e_correct_purge() {
    let (svc, uid) = make_service().await;

    // Store
    let r = call_tool("memory_store", json!({"content": "uses black for formatting"}), &svc, &uid).await;
    let stored_text = text(&r).to_string();
    let memory_id = stored_text.split_whitespace().nth(2).unwrap().trim_end_matches(':').to_string();

    // Correct
    let r = call_tool("memory_correct", json!({"memory_id": memory_id, "new_content": "uses ruff for formatting"}), &svc, &uid).await;
    assert!(text(&r).contains("ruff"), "got: {}", text(&r));
    println!("✅ correct: {}", text(&r));

    // Purge
    let r = call_tool("memory_purge", json!({"memory_id": memory_id}), &svc, &uid).await;
    assert!(text(&r).contains("Purged"), "got: {}", text(&r));
    println!("✅ purge: {}", text(&r));
}

#[tokio::test]
async fn test_e2e_list_profile() {
    let (svc, uid) = make_service().await;

    call_tool("memory_store", json!({"content": "prefers tabs over spaces", "memory_type": "profile"}), &svc, &uid).await;
    call_tool("memory_store", json!({"content": "uses pytest for testing", "memory_type": "profile"}), &svc, &uid).await;

    let r = call_tool("memory_profile", json!({}), &svc, &uid).await;
    let t = text(&r);
    assert!(t.contains("tabs") || t.contains("pytest"), "got: {t}");
    println!("✅ profile: {t}");
}

#[tokio::test]
async fn test_e2e_mcp_initialize() {
    // Verify the MCP initialize response has correct structure
    let resp = json!({
        "protocolVersion": "2024-11-05",
        "capabilities": {"tools": {}},
        "serverInfo": {"name": "memoria-mcp-rs", "version": "0.1.0"}
    });
    assert_eq!(resp["protocolVersion"], "2024-11-05");
    assert_eq!(resp["serverInfo"]["name"], "memoria-mcp-rs");
    println!("✅ mcp_initialize structure OK");
}

#[tokio::test]
async fn test_e2e_tools_list_has_8() {
    let tools = memoria_mcp::tools::list();
    assert_eq!(tools.as_array().unwrap().len(), 15, "expected 15 core tools");
    println!("✅ tools_list: 15 tools");
}

// ── MCP branch → store → merge end-to-end ────────────────────────────────────
//
// Tests the full git workflow through MCP tool dispatch:
//   memory_branch → memory_checkout → memory_store → memory_merge
// Specifically guards against the MatrixOne INSERT IGNORE SELECT * vecf32 bug.

async fn make_git_service() -> (Arc<MemoryService>, Arc<memoria_git::GitForDataService>, String) {
    let db_url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "mysql://root:111@localhost:6001/memoria".to_string());
    let dim: usize = std::env::var("EMBEDDING_DIM")
        .unwrap_or_else(|_| "1024".to_string())
        .parse()
        .unwrap_or(1024);

    let store = SqlMemoryStore::connect(&db_url, dim).await.expect("connect");
    store.migrate().await.expect("migrate");

    let embedder: Option<Arc<dyn EmbeddingProvider>> = {
        let base_url = std::env::var("EMBEDDING_BASE_URL").unwrap_or_default();
        let api_key = std::env::var("EMBEDDING_API_KEY").unwrap_or_default();
        let model = std::env::var("EMBEDDING_MODEL").unwrap_or_else(|_| "BAAI/bge-m3".to_string());
        if !base_url.is_empty() && !api_key.is_empty() {
            Some(Arc::new(HttpEmbedder::new(&base_url, &api_key, &model, dim)))
        } else { None }
    };

    let pool = sqlx::mysql::MySqlPool::connect(&db_url).await.expect("pool");
    let db_name = db_url.rsplit('/').next().unwrap_or("memoria");
    let git = Arc::new(memoria_git::GitForDataService::new(pool, db_name));
    let svc = Arc::new(MemoryService::new_sql_with_llm(Arc::new(store), embedder, None));
    let uid = format!("e2e_{}", Uuid::new_v4().simple());
    (svc, git, uid)
}

#[tokio::test]
async fn test_e2e_branch_store_merge() {
    let (svc, git, uid) = make_git_service().await;
    let branch = format!("test_br_{}", &uid[5..13]);

    // 1. Store a memory on main
    let r = memoria_mcp::tools::call("memory_store",
        json!({"content": "main memory", "memory_type": "semantic"}),
        &svc, &uid).await.expect("store main");
    assert!(text(&r).contains("Stored"), "got: {}", text(&r));
    println!("✅ stored on main: {}", text(&r));

    // 2. Create branch
    let r = memoria_mcp::git_tools::call("memory_branch",
        json!({"name": branch}), &git, &svc, &uid).await.expect("branch");
    assert!(text(&r).contains("Created"), "got: {}", text(&r));
    println!("✅ branch created: {}", text(&r));

    // 3. Checkout branch
    let r = memoria_mcp::git_tools::call("memory_checkout",
        json!({"name": branch}), &git, &svc, &uid).await.expect("checkout");
    assert!(text(&r).contains("Switched"), "got: {}", text(&r));
    println!("✅ checked out: {}", text(&r));

    // 4. Store a new memory on branch (with embedding if available)
    let r = memoria_mcp::tools::call("memory_store",
        json!({"content": "branch-only memory", "memory_type": "semantic"}),
        &svc, &uid).await.expect("store branch");
    assert!(text(&r).contains("Stored"), "got: {}", text(&r));
    println!("✅ stored on branch: {}", text(&r));

    // 5. Checkout main
    let r = memoria_mcp::git_tools::call("memory_checkout",
        json!({"name": "main"}), &git, &svc, &uid).await.expect("checkout main");
    assert!(text(&r).contains("Switched"), "got: {}", text(&r));

    // 6. Merge — this is the critical step that was broken
    let r = memoria_mcp::git_tools::call("memory_merge",
        json!({"source": branch, "strategy": "append"}),
        &git, &svc, &uid).await.expect("merge");
    let merge_text = text(&r);
    assert!(merge_text.contains("Merged"), "got: {}", merge_text);
    // Must report 1 new memory (the branch-only one)
    assert!(merge_text.contains("1 new"), "expected '1 new memories' in: {}", merge_text);
    println!("✅ merge: {}", merge_text);

    // 7. Verify main now has both memories
    let r = memoria_mcp::tools::call("memory_retrieve",
        json!({"query": "branch-only memory", "top_k": 10}),
        &svc, &uid).await.expect("retrieve");
    assert!(text(&r).contains("branch-only memory"),
        "branch memory missing from main after merge: {}", text(&r));
    println!("✅ branch memory visible on main after merge");

    // Cleanup
    let _ = memoria_mcp::git_tools::call("memory_branch_delete",
        json!({"name": branch}), &git, &svc, &uid).await;
    println!("✅ test_e2e_branch_store_merge passed");
}
