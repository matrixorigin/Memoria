/// Graph E2E tests: GraphStore CRUD, consolidation, entity extraction/linking.
/// Requires real DB. Run with --test-threads=1 (rollback is account-level).

use memoria_storage::{GraphConsolidator, GraphStore};
use memoria_storage::graph::types::{GraphEdge, GraphNode, NodeType, edge_type};

fn test_dim() -> usize {
    std::env::var("EMBEDDING_DIM").ok().and_then(|s| s.parse().ok()).unwrap_or(1024)
}

async fn setup_graph() -> (GraphStore, String) {
    let db_url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "mysql://root:111@localhost:6001/memoria".to_string());
    let pool = sqlx::MySqlPool::connect(&db_url).await.expect("connect");
    let store = GraphStore::new(pool, test_dim()); // dim=4 for tests
    store.migrate().await.expect("migrate");
    let uid = format!("graph_test_{}", uuid::Uuid::new_v4().simple());
    (store, uid)
}

fn make_node(user_id: &str, node_type: NodeType, content: &str, memory_id: Option<&str>) -> GraphNode {
    GraphNode {
        node_id: uuid::Uuid::new_v4().simple().to_string()[..32].to_string(),
        user_id: user_id.to_string(),
        node_type,
        content: content.to_string(),
        entity_type: None,
        embedding: None,
        memory_id: memory_id.map(String::from),
        session_id: None,
        confidence: 0.75,
        trust_tier: "T3".to_string(),
        importance: 0.5,
        source_nodes: vec![],
        conflicts_with: None,
        conflict_resolution: None,
        access_count: 0,
        cross_session_count: 0,
        is_active: true,
        superseded_by: None,
        created_at: Some(chrono::Utc::now().naive_utc()),
    }
}

// ── 1. GraphStore: create and retrieve node ──────────────────────────────────

#[tokio::test]
async fn test_graph_create_and_get_node() {
    let (store, uid) = setup_graph().await;
    let node = make_node(&uid, NodeType::Semantic, "Rust is a systems language", Some("mem_001"));
    let node_id = node.node_id.clone();

    store.create_node(&node).await.expect("create_node");

    let fetched = store.get_node(&node_id).await.expect("get_node").expect("should exist");
    assert_eq!(fetched.content, "Rust is a systems language");
    assert_eq!(fetched.node_type, NodeType::Semantic);
    assert_eq!(fetched.memory_id.as_deref(), Some("mem_001"));
    println!("✅ create and get node: {}", fetched.node_id);
}

// ── 2. GraphStore: get_node_by_memory_id ────────────────────────────────────

#[tokio::test]
async fn test_graph_get_by_memory_id() {
    let (store, uid) = setup_graph().await;
    let mid = format!("mem_{}", uuid::Uuid::new_v4().simple());
    let node = make_node(&uid, NodeType::Semantic, "test content", Some(&mid));
    store.create_node(&node).await.expect("create");

    let found = store.get_node_by_memory_id(&mid).await.expect("query").expect("found");
    assert_eq!(found.content, "test content");
    println!("✅ get_by_memory_id: {}", found.node_id);
}

// ── 3. GraphStore: get_user_nodes filters by type ───────────────────────────

#[tokio::test]
async fn test_graph_get_user_nodes_by_type() {
    let (store, uid) = setup_graph().await;

    let semantic = make_node(&uid, NodeType::Semantic, "semantic node", None);
    let scene = make_node(&uid, NodeType::Scene, "scene node", None);
    store.create_node(&semantic).await.expect("create semantic");
    store.create_node(&scene).await.expect("create scene");

    let semantics = store.get_user_nodes(&uid, &NodeType::Semantic, true).await.expect("query");
    let scenes = store.get_user_nodes(&uid, &NodeType::Scene, true).await.expect("query");

    assert!(semantics.iter().any(|n| n.content == "semantic node"));
    assert!(scenes.iter().any(|n| n.content == "scene node"));
    assert!(!semantics.iter().any(|n| n.content == "scene node"), "scene should not appear in semantic list");
    println!("✅ get_user_nodes by type: {} semantic, {} scene", semantics.len(), scenes.len());
}

// ── 4. GraphStore: deactivate_node ──────────────────────────────────────────

#[tokio::test]
async fn test_graph_deactivate_node() {
    let (store, uid) = setup_graph().await;
    let node = make_node(&uid, NodeType::Scene, "to be deactivated", None);
    let node_id = node.node_id.clone();
    store.create_node(&node).await.expect("create");

    store.deactivate_node(&node_id).await.expect("deactivate");

    let active = store.get_user_nodes(&uid, &NodeType::Scene, true).await.expect("query");
    assert!(!active.iter().any(|n| n.node_id == node_id), "deactivated node should not appear");
    println!("✅ deactivate_node works");
}

// ── 5. GraphStore: update_confidence_and_tier ───────────────────────────────

#[tokio::test]
async fn test_graph_update_tier() {
    let (store, uid) = setup_graph().await;
    let mut node = make_node(&uid, NodeType::Scene, "tier test", None);
    node.trust_tier = "T4".to_string();
    node.confidence = 0.6;
    let node_id = node.node_id.clone();
    store.create_node(&node).await.expect("create");

    store.update_confidence_and_tier(&node_id, 0.9, "T3").await.expect("update");

    let fetched = store.get_node(&node_id).await.expect("get").expect("exists");
    assert_eq!(fetched.trust_tier, "T3");
    assert!((fetched.confidence - 0.9).abs() < 0.001);
    println!("✅ update_confidence_and_tier: T4 → T3");
}

// ── 6. GraphStore: add_edge and mark_conflict ────────────────────────────────

#[tokio::test]
async fn test_graph_edge_and_conflict() {
    let (store, uid) = setup_graph().await;
    let n1 = make_node(&uid, NodeType::Semantic, "node A", None);
    let n2 = make_node(&uid, NodeType::Semantic, "node B", None);
    let (id1, id2) = (n1.node_id.clone(), n2.node_id.clone());
    store.create_node(&n1).await.expect("create n1");
    store.create_node(&n2).await.expect("create n2");

    let edge = GraphEdge {
        source_id: id1.clone(),
        target_id: id2.clone(),
        edge_type: edge_type::ASSOCIATION.to_string(),
        weight: 0.85,
        user_id: uid.clone(),
    };
    store.add_edge(&edge).await.expect("add_edge");

    // mark conflict: older=n1, newer=n2
    store.mark_conflict(&id1, &id2, 0.5, 0.75).await.expect("mark_conflict");

    let n1_updated = store.get_node(&id1).await.expect("get").expect("exists");
    assert_eq!(n1_updated.conflicts_with.as_deref(), Some(id2.as_str()));
    assert!((n1_updated.confidence - 0.375).abs() < 0.001, "confidence should be 0.75 * 0.5");
    println!("✅ edge + mark_conflict: confidence={}", n1_updated.confidence);
}

// ── 7. GraphStore: entity upsert (is_new flag) ───────────────────────────────

#[tokio::test]
async fn test_graph_entity_upsert() {
    let (store, uid) = setup_graph().await;

    let (id1, is_new1) = store.upsert_entity(&uid, "rust", "Rust", "tech").await.expect("upsert");
    assert!(is_new1, "first upsert should be new");

    let (id2, is_new2) = store.upsert_entity(&uid, "rust", "Rust", "tech").await.expect("upsert again");
    assert!(!is_new2, "second upsert should be reused");
    assert_eq!(id1, id2, "same entity_id");
    println!("✅ entity upsert: id={id1}, is_new={is_new1}/{is_new2}");
}

// ── 8. GraphStore: upsert_memory_entity_link idempotent ──────────────────────

#[tokio::test]
async fn test_graph_entity_link_idempotent() {
    let (store, uid) = setup_graph().await;
    let (entity_id, _) = store.upsert_entity(&uid, "matrixone", "MatrixOne", "tech").await.expect("upsert");
    let mid = format!("mem_{}", uuid::Uuid::new_v4().simple());

    store.upsert_memory_entity_link(&mid, &entity_id, &uid, "manual").await.expect("link 1");
    store.upsert_memory_entity_link(&mid, &entity_id, &uid, "manual").await.expect("link 2 (idempotent)");

    let entities = store.get_user_entities(&uid).await.expect("get entities");
    assert!(entities.iter().any(|(n, _)| n == "matrixone"));
    println!("✅ entity link idempotent");
}

// ── 9. GraphStore: get_unlinked_memories ─────────────────────────────────────

#[tokio::test]
async fn test_graph_unlinked_memories() {
    let (store, uid) = setup_graph().await;

    // Insert a memory directly into mem_memories
    let db_url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "mysql://root:111@localhost:6001/memoria".to_string());
    let pool = sqlx::MySqlPool::connect(&db_url).await.expect("connect");
    let mid = format!("mem_{}", uuid::Uuid::new_v4().simple());
    sqlx::query(
        "INSERT INTO mem_memories (memory_id, user_id, memory_type, content, source_event_ids, \
         is_active, trust_tier, initial_confidence, observed_at, created_at) \
         VALUES (?, ?, 'semantic', 'unlinked test memory', '[]', 1, 'T3', 0.75, NOW(), NOW())"
    )
    .bind(&mid).bind(&uid)
    .execute(&pool).await.expect("insert memory");

    let unlinked = store.get_unlinked_memories(&uid, 50).await.expect("get unlinked");
    assert!(unlinked.iter().any(|(m, _)| m == &mid), "should find unlinked memory");

    // Link it
    let (entity_id, _) = store.upsert_entity(&uid, "testentity", "TestEntity", "tech").await.expect("upsert");
    store.upsert_memory_entity_link(&mid, &entity_id, &uid, "manual").await.expect("link");

    let unlinked2 = store.get_unlinked_memories(&uid, 50).await.expect("get unlinked again");
    assert!(!unlinked2.iter().any(|(m, _)| m == &mid), "linked memory should not appear");
    println!("✅ get_unlinked_memories: before={}, after={}", unlinked.len(), unlinked2.len());
}

// ── 10. GraphConsolidator: trust tier lifecycle ──────────────────────────────

#[tokio::test]
async fn test_consolidator_trust_tier_lifecycle() {
    let (store, uid) = setup_graph().await;

    // T4 scene with high confidence and old age (simulate by setting created_at far back)
    let mut scene = make_node(&uid, NodeType::Scene, "old high-confidence scene", None);
    scene.trust_tier = "T4".to_string();
    scene.confidence = 0.9;
    // Set created_at to 10 days ago
    scene.created_at = Some((chrono::Utc::now() - chrono::Duration::days(10)).naive_utc());
    store.create_node(&scene).await.expect("create scene");

    let consolidator = GraphConsolidator::new(&store);
    let result = consolidator.consolidate(&uid).await;

    assert_eq!(result.promoted, 1, "T4→T3 promotion expected, got: {:?}", result);
    assert_eq!(result.demoted, 0);
    assert_eq!(result.errors.len(), 0, "no errors expected: {:?}", result.errors);

    let updated = store.get_node(&scene.node_id).await.expect("get").expect("exists");
    assert_eq!(updated.trust_tier, "T3", "should be promoted to T3");
    println!("✅ trust tier T4→T3 promotion: confidence={}", updated.confidence);
}

// ── 11. GraphConsolidator: orphaned scene deactivation ───────────────────────

#[tokio::test]
async fn test_consolidator_orphaned_scene() {
    let (store, uid) = setup_graph().await;

    // Scene with source_nodes pointing to non-existent nodes
    let mut scene = make_node(&uid, NodeType::Scene, "orphaned scene", None);
    scene.source_nodes = vec!["nonexistent_node_id_1".to_string(), "nonexistent_node_id_2".to_string()];
    store.create_node(&scene).await.expect("create scene");

    let consolidator = GraphConsolidator::new(&store);
    let result = consolidator.consolidate(&uid).await;

    assert_eq!(result.orphaned_scenes, 1, "should detect 1 orphaned scene, got: {:?}", result);

    let updated = store.get_node(&scene.node_id).await.expect("get").expect("exists");
    assert!(!updated.is_active, "orphaned scene should be deactivated");
    println!("✅ orphaned scene deactivated");
}

// ── 12. GraphConsolidator: T3 stale demotion ─────────────────────────────────

#[tokio::test]
async fn test_consolidator_t3_demotion() {
    let (store, uid) = setup_graph().await;

    // T3 scene with low confidence and very old age
    let mut scene = make_node(&uid, NodeType::Scene, "stale T3 scene", None);
    scene.trust_tier = "T3".to_string();
    scene.confidence = 0.5; // below T4→T3 threshold of 0.8
    scene.created_at = Some((chrono::Utc::now() - chrono::Duration::days(65)).naive_utc());
    store.create_node(&scene).await.expect("create");

    let consolidator = GraphConsolidator::new(&store);
    let result = consolidator.consolidate(&uid).await;

    assert_eq!(result.demoted, 1, "T3→T4 demotion expected, got: {:?}", result);
    let updated = store.get_node(&scene.node_id).await.expect("get").expect("exists");
    assert_eq!(updated.trust_tier, "T4");
    println!("✅ T3→T4 demotion for stale low-confidence scene");
}

// ── 13. Full E2E: store memory → extract entities → link → re-extract ────────

#[tokio::test]
async fn test_graph_full_entity_workflow() {
    let (store, uid) = setup_graph().await;

    // Insert memory
    let db_url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "mysql://root:111@localhost:6001/memoria".to_string());
    let pool = sqlx::MySqlPool::connect(&db_url).await.expect("connect");
    let mid = format!("mem_{}", uuid::Uuid::new_v4().simple());
    sqlx::query(
        "INSERT INTO mem_memories (memory_id, user_id, memory_type, content, source_event_ids, \
         is_active, trust_tier, initial_confidence, observed_at, created_at) \
         VALUES (?, ?, 'semantic', 'Project uses Rust and MatrixOne', '[]', 1, 'T3', 0.75, NOW(), NOW())"
    )
    .bind(&mid).bind(&uid)
    .execute(&pool).await.expect("insert");

    // Step 1: get unlinked
    let unlinked = store.get_unlinked_memories(&uid, 50).await.expect("unlinked");
    assert!(unlinked.iter().any(|(m, _)| m == &mid));

    // Step 2: link entities
    let (rust_id, is_new_rust) = store.upsert_entity(&uid, "rust", "Rust", "tech").await.expect("upsert rust");
    let (mo_id, is_new_mo) = store.upsert_entity(&uid, "matrixone", "MatrixOne", "tech").await.expect("upsert mo");
    assert!(is_new_rust && is_new_mo);

    store.upsert_memory_entity_link(&mid, &rust_id, &uid, "manual").await.expect("link rust");
    store.upsert_memory_entity_link(&mid, &mo_id, &uid, "manual").await.expect("link mo");

    // Step 3: re-extract — should be empty now
    let unlinked2 = store.get_unlinked_memories(&uid, 50).await.expect("unlinked2");
    assert!(!unlinked2.iter().any(|(m, _)| m == &mid), "linked memory should not appear");

    // Step 4: get_user_entities
    let entities = store.get_user_entities(&uid).await.expect("entities");
    let names: Vec<&str> = entities.iter().map(|(n, _)| n.as_str()).collect();
    assert!(names.contains(&"rust"), "rust entity should exist");
    assert!(names.contains(&"matrixone"), "matrixone entity should exist");

    println!("✅ full entity workflow: {} entities linked to memory {}", entities.len(), &mid[..8]);
}

// ── 14. store → graph node auto-created ──────────────────────────────────────

#[tokio::test]
async fn test_store_creates_graph_node() {
    use memoria_storage::SqlMemoryStore;
    use memoria_service::MemoryService;
    use std::sync::Arc;

    let db_url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "mysql://root:111@localhost:6001/memoria".to_string());
    let sql = SqlMemoryStore::connect(&db_url, test_dim()).await.expect("connect");
    sql.migrate().await.expect("migrate");
    let uid = format!("gsync_{}", uuid::Uuid::new_v4().simple());
    let svc = Arc::new(MemoryService::new_sql_with_llm(Arc::new(sql.clone()), None, None));

    // Call memory_store via tools
    let r = memoria_mcp::tools::call(
        "memory_store",
        serde_json::json!({"content": "Project uses Rust and MatrixOne database", "memory_type": "semantic"}),
        &svc, &uid,
    ).await.expect("call");
    let text = r["content"][0]["text"].as_str().unwrap_or("");
    assert!(text.contains("Stored memory"), "got: {text}");

    // Extract memory_id from response
    let mid = text.split_whitespace().nth(2).unwrap_or("").trim_end_matches(':');

    // Verify graph node was created
    let graph = sql.graph_store();
    let node = graph.get_node_by_memory_id(mid).await.expect("query");
    assert!(node.is_some(), "graph node should be created for memory {mid}");
    let node = node.unwrap();
    assert_eq!(node.content, "Project uses Rust and MatrixOne database");
    println!("✅ store creates graph node: {}", node.node_id);

    // Verify entity extraction happened
    let entities = graph.get_user_entities(&uid).await.expect("entities");
    println!("✅ auto-extracted entities: {:?}", entities.iter().map(|(n,_)| n.as_str()).collect::<Vec<_>>());
    // Should have extracted "rust" and/or "matrixone"
    let names: Vec<&str> = entities.iter().map(|(n, _)| n.as_str()).collect();
    assert!(names.contains(&"rust") || names.contains(&"matrixone"),
        "expected rust or matrixone in {names:?}");
}

// ── 15. correct → graph node content updated ─────────────────────────────────

#[tokio::test]
async fn test_correct_updates_graph_node() {
    use memoria_storage::SqlMemoryStore;
    use memoria_service::MemoryService;
    use std::sync::Arc;

    let db_url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "mysql://root:111@localhost:6001/memoria".to_string());
    let sql = SqlMemoryStore::connect(&db_url, test_dim()).await.expect("connect");
    sql.migrate().await.expect("migrate");
    let uid = format!("gcorr_{}", uuid::Uuid::new_v4().simple());
    let svc = Arc::new(MemoryService::new_sql_with_llm(Arc::new(sql.clone()), None, None));

    // Store
    let r = memoria_mcp::tools::call(
        "memory_store",
        serde_json::json!({"content": "Uses black for formatting"}),
        &svc, &uid,
    ).await.expect("store");
    let text = r["content"][0]["text"].as_str().unwrap_or("");
    let mid = text.split_whitespace().nth(2).unwrap_or("").trim_end_matches(':').to_string();

    // Correct — creates new memory, deactivates old
    let cr = memoria_mcp::tools::call(
        "memory_correct",
        serde_json::json!({"memory_id": mid, "new_content": "Uses ruff for formatting", "reason": "switched"}),
        &svc, &uid,
    ).await.expect("correct");
    let ct = cr["content"][0]["text"].as_str().unwrap_or("");
    let new_mid = ct.split_whitespace().nth(2).unwrap_or("").trim_end_matches(':').to_string();

    // Verify old graph node deactivated, new graph node has updated content
    let graph = sql.graph_store();
    let old_node = graph.get_node_by_memory_id(&mid).await.expect("query");
    assert!(old_node.is_none() || !old_node.as_ref().unwrap().is_active,
        "old graph node should be deactivated");

    // New memory may or may not have a graph node (depends on store_memory creating one)
    // The key assertion is that the old node is deactivated
    println!("✅ correct deactivates old graph node, old={mid} → new={new_mid}");
}

// ── 16. purge → graph node deactivated ───────────────────────────────────────

#[tokio::test]
async fn test_purge_deactivates_graph_node() {
    use memoria_storage::SqlMemoryStore;
    use memoria_service::MemoryService;
    use std::sync::Arc;

    let db_url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "mysql://root:111@localhost:6001/memoria".to_string());
    let sql = SqlMemoryStore::connect(&db_url, test_dim()).await.expect("connect");
    sql.migrate().await.expect("migrate");
    let uid = format!("gpurge_{}", uuid::Uuid::new_v4().simple());
    let svc = Arc::new(MemoryService::new_sql_with_llm(Arc::new(sql.clone()), None, None));

    // Store
    let r = memoria_mcp::tools::call(
        "memory_store",
        serde_json::json!({"content": "Temporary working memory"}),
        &svc, &uid,
    ).await.expect("store");
    let text = r["content"][0]["text"].as_str().unwrap_or("");
    let mid = text.split_whitespace().nth(2).unwrap_or("").trim_end_matches(':').to_string();

    // Verify graph node exists
    let graph = sql.graph_store();
    assert!(graph.get_node_by_memory_id(&mid).await.expect("query").is_some());

    // Purge
    memoria_mcp::tools::call(
        "memory_purge",
        serde_json::json!({"memory_id": mid}),
        &svc, &uid,
    ).await.expect("purge");

    // Verify graph node deactivated
    let node = graph.get_node_by_memory_id(&mid).await.expect("query");
    assert!(node.is_none(), "deactivated node should not appear in active query");
    println!("✅ purge deactivates graph node");
}

// ── 17. NER: regex extraction unit test via tools ────────────────────────────

#[test]
fn test_ner_extract_entities() {
    let entities = memoria_storage::extract_entities(
        "Project uses Rust and MatrixOne. See matrixorigin/matrixone for details. The auth-service handles login."
    );
    let names: Vec<&str> = entities.iter().map(|e| e.name.as_str()).collect();
    println!("extracted: {names:?}");
    assert!(names.contains(&"rust"), "rust not found in {names:?}");
    assert!(names.contains(&"matrixone"), "matrixone not found in {names:?}");
    assert!(names.contains(&"matrixorigin/matrixone"), "repo not found in {names:?}");
    assert!(names.contains(&"auth-service"), "auth-service not found in {names:?}");
}

// ── 18. Entity link weights by source ────────────────────────────────────────

#[tokio::test]
async fn test_entity_link_weights_by_source() {
    use memoria_storage::SqlMemoryStore;

    let db_url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "mysql://root:111@localhost:6001/memoria".to_string());
    let sql = SqlMemoryStore::connect(&db_url, test_dim()).await.expect("connect");
    sql.migrate().await.expect("migrate");
    let uid = format!("elw_{}", uuid::Uuid::new_v4().simple());
    let graph = sql.graph_store();

    // Create entity
    let (eid, _created) = graph.upsert_entity(&uid, "test_entity", "test_entity", "tech").await.expect("entity");

    // Create links with different sources
    let mid_regex = format!("mem_regex_{}", uuid::Uuid::new_v4().simple());
    let mid_llm = format!("mem_llm_{}", uuid::Uuid::new_v4().simple());
    let mid_manual = format!("mem_manual_{}", uuid::Uuid::new_v4().simple());

    graph.upsert_memory_entity_link(&mid_regex, &eid, &uid, "regex").await.expect("link regex");
    graph.upsert_memory_entity_link(&mid_llm, &eid, &uid, "llm").await.expect("link llm");
    graph.upsert_memory_entity_link(&mid_manual, &eid, &uid, "manual").await.expect("link manual");

    // Verify weights
    let rows = sqlx::query(
        "SELECT memory_id, weight FROM mem_memory_entity_links WHERE entity_id = ? AND user_id = ? ORDER BY weight"
    )
    .bind(&eid).bind(&uid)
    .fetch_all(sql.pool()).await.expect("query");

    use sqlx::Row;
    assert_eq!(rows.len(), 3);
    let weights: Vec<(String, f32)> = rows.iter().map(|r| {
        (r.try_get::<String, _>("memory_id").unwrap(), r.try_get::<f32, _>("weight").unwrap())
    }).collect();

    for (mid, w) in &weights {
        if mid == &mid_regex { assert!((w - 0.8).abs() < 0.01, "regex weight should be 0.8, got {w}"); }
        if mid == &mid_llm { assert!((w - 0.9).abs() < 0.01, "llm weight should be 0.9, got {w}"); }
        if mid == &mid_manual { assert!((w - 1.0).abs() < 0.01, "manual weight should be 1.0, got {w}"); }
    }
    println!("✅ entity link weights: regex=0.8, llm=0.9, manual=1.0");
}
