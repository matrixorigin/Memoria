//! 8 core MCP tools for Phase 2.
//! Phase 4 will add 14 more (Git-for-Data, admin, graph).

use anyhow::Result;
use memoria_core::{MemoryType, TrustTier};
use memoria_service::MemoryService;
use serde_json::{json, Value};
use sqlx::Row;
use std::str::FromStr;
use std::sync::Arc;
use uuid::Uuid;

pub fn list() -> Value {
    json!([
        {
            "name": "memory_store",
            "description": "Store a new memory",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "content": {"type": "string"},
                    "memory_type": {"type": "string", "default": "semantic"},
                    "session_id": {"type": "string"},
                    "trust_tier": {"type": "string"}
                },
                "required": ["content"]
            }
        },
        {
            "name": "memory_retrieve",
            "description": "Retrieve relevant memories for a query",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": {"type": "string"},
                    "top_k": {"type": "integer", "default": 5},
                    "session_id": {"type": "string"},
                    "explain": {"type": ["boolean", "string"], "default": false, "description": "Explain level: false/\"none\"=off, true/\"basic\"=timing+path, \"verbose\"=+per-candidate scores, \"analyze\"=full"}
                },
                "required": ["query"]
            }
        },
        {
            "name": "memory_search",
            "description": "Semantic search across all memories",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": {"type": "string"},
                    "top_k": {"type": "integer", "default": 10},
                    "explain": {"type": ["boolean", "string"], "default": false, "description": "Explain level: false/\"none\"=off, true/\"basic\"=timing+path, \"verbose\"=+per-candidate scores, \"analyze\"=full"}
                },
                "required": ["query"]
            }
        },
        {
            "name": "memory_correct",
            "description": "Update an existing memory with new content",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "memory_id": {"type": "string"},
                    "query": {"type": "string", "description": "Semantic search to find memory to correct"},
                    "new_content": {"type": "string"},
                    "reason": {"type": "string"}
                },
                "required": ["new_content"]
            }
        },
        {
            "name": "memory_purge",
            "description": "Delete memories by ID (single or comma-separated batch) or by topic keyword",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "memory_id": {"type": "string", "description": "Single ID or comma-separated batch"},
                    "topic": {"type": "string", "description": "Keyword — bulk-delete all matching memories"},
                    "reason": {"type": "string"}
                }
            }
        },
        {
            "name": "memory_profile",
            "description": "Get user memory-derived profile summary",
            "inputSchema": {"type": "object", "properties": {}}
        },
        {
            "name": "memory_capabilities",
            "description": "List available memory tools",
            "inputSchema": {"type": "object", "properties": {}}
        },
        {
            "name": "memory_list",
            "description": "List active memories for the user",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "limit": {"type": "integer", "default": 20}
                }
            }
        },
        {
            "name": "memory_governance",
            "description": "Run memory governance: quarantine low-confidence memories, clean stale data. 1-hour cooldown.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "force": {"type": "boolean", "default": false}
                }
            }
        },
        {
            "name": "memory_rebuild_index",
            "description": "Rebuild IVF vector index for a memory table. Only call when governance reports needs_rebuild=True.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "table": {"type": "string", "default": "mem_memories"}
                }
            }
        },
        {
            "name": "memory_consolidate",
            "description": "Run graph consolidation: detect contradicting memories, fix orphaned nodes, manage trust tiers. 30-minute cooldown.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "force": {"type": "boolean", "default": false}
                }
            }
        },
        {
            "name": "memory_reflect",
            "description": "Analyze memory clusters and synthesize high-level insights. 2-hour cooldown.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "force": {"type": "boolean", "default": false},
                    "mode": {"type": "string", "default": "auto"}
                }
            }
        },
        {
            "name": "memory_extract_entities",
            "description": "Extract named entities from memories and build entity graph.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "mode": {"type": "string", "default": "auto"}
                }
            }
        },
        {
            "name": "memory_link_entities",
            "description": "Write entity links from user-LLM extraction results.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "entities": {"type": "string"}
                },
                "required": ["entities"]
            }
        },
        {
            "name": "memory_observe",
            "description": "Observe a conversation turn and extract memories from messages. Stores assistant/user messages as semantic memories.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "messages": {
                        "type": "array",
                        "items": {"type": "object"},
                        "description": "Array of {role, content} message objects"
                    },
                    "session_id": {"type": "string"}
                },
                "required": ["messages"]
            }
        }
    ])
}

pub async fn call(
    name: &str,
    args: Value,
    service: &Arc<MemoryService>,
    user_id: &str,
) -> Result<Value> {
    tracing::debug!(tool = name, user_id, "MCP tool call");
    match name {
        "memory_store" => {
            let content = args["content"].as_str().unwrap_or("").to_string();
            let memory_type = args["memory_type"].as_str().unwrap_or("semantic");
            let session_id = args["session_id"].as_str().map(String::from);
            let trust_tier = args["trust_tier"].as_str()
                .map(TrustTier::from_str).transpose().ok().flatten();
            let mt = MemoryType::from_str(memory_type)
                .unwrap_or(MemoryType::Semantic);
            let m = match service.store_memory(user_id, &content, mt, session_id.clone(), trust_tier, None, None).await {
                Ok(m) => m,
                Err(memoria_core::MemoriaError::Blocked(reason)) => {
                    return Ok(json!({"result": format!("⚠️ Memory blocked: {reason}")}));
                }
                Err(e) => return Err(e.into()),
            };

            // Graph sync: create SEMANTIC node + auto entity extraction (best-effort)
            if let Some(sql) = &service.sql_store {
                let graph = sql.graph_store();
                let node = memoria_storage::GraphNode {
                    node_id: uuid::Uuid::new_v4().simple().to_string()[..32].to_string(),
                    user_id: user_id.to_string(),
                    node_type: memoria_storage::NodeType::Semantic,
                    content: m.content.clone(),
                    entity_type: None,
                    embedding: None,
                    memory_id: Some(m.memory_id.clone()),
                    session_id: session_id.clone(),
                    confidence: m.initial_confidence as f32,
                    trust_tier: format!("{}", m.trust_tier),
                    importance: 0.5,
                    source_nodes: vec![],
                    conflicts_with: None,
                    conflict_resolution: None,
                    access_count: 0,
                    cross_session_count: 0,
                    is_active: true,
                    superseded_by: None,
                    created_at: m.created_at.map(|dt| dt.naive_utc()),
                };
                let _ = graph.create_node(&node).await; // best-effort

                // Auto entity extraction (regex, lightweight)
                let entities = memoria_storage::extract_entities(&m.content);
                for ent in &entities {
                    if let Ok((entity_id, _)) = graph.upsert_entity(
                        user_id, &ent.name, &ent.display, &ent.entity_type
                    ).await {
                        let _ = graph.upsert_memory_entity_link(
                            &m.memory_id, &entity_id, user_id, "regex"
                        ).await;
                    }
                }
            }

            Ok(mcp_text(&format!("Stored memory {}: {}", m.memory_id, m.content)))
        }

        "memory_retrieve" | "memory_search" => {
            let query = args["query"].as_str().unwrap_or("").to_string();
            let top_k = args["top_k"].as_i64().unwrap_or(5);
            // explain accepts bool or string: true/"basic"/"verbose"/"analyze"
            let explain_str = match &args["explain"] {
                serde_json::Value::Bool(true) => "basic",
                serde_json::Value::String(s) => s.as_str(),
                _ => "none",
            };
            let level = memoria_service::ExplainLevel::from_str_or_bool(explain_str);

            if level != memoria_service::ExplainLevel::None {
                let (results, stats) = service.retrieve_explain_level(user_id, &query, top_k, level).await?;
                if results.is_empty() {
                    let explain_json = serde_json::to_string_pretty(&stats).unwrap_or_default();
                    return Ok(mcp_text(&format!("No relevant memories found.\n\n--- explain ---\n{explain_json}")));
                }
                let text = results.iter().map(|m| {
                    format!("[{}] ({}) {}", m.memory_id, m.memory_type, m.content)
                }).collect::<Vec<_>>().join("\n");
                let explain_json = serde_json::to_string_pretty(&stats).unwrap_or_default();
                Ok(mcp_text(&format!("{text}\n\n--- explain ---\n{explain_json}")))
            } else {
                let results = service.retrieve(user_id, &query, top_k).await?;
                if results.is_empty() {
                    return Ok(mcp_text("No relevant memories found."));
                }
                let text = results.iter().map(|m| {
                    format!("[{}] ({}) {}", m.memory_id, m.memory_type, m.content)
                }).collect::<Vec<_>>().join("\n");
                Ok(mcp_text(&text))
            }
        }

        "memory_correct" => {
            let new_content = args["new_content"].as_str().unwrap_or("");
            if new_content.is_empty() {
                return Ok(mcp_text("new_content is required"));
            }
            let memory_id = args["memory_id"].as_str().unwrap_or("");
            let query = args["query"].as_str().unwrap_or("");

            // Resolve old memory_id for graph sync
            let old_mid = if !memory_id.is_empty() {
                memory_id.to_string()
            } else if !query.is_empty() {
                let results = service.retrieve(user_id, query, 1).await?;
                match results.into_iter().next() {
                    Some(found) => found.memory_id,
                    None => return Ok(mcp_text("No matching memory found for query")),
                }
            } else {
                return Ok(mcp_text("Provide memory_id or query"));
            };

            let m = service.correct(&old_mid, new_content).await?;

            // Graph sync: deactivate old node, create/update new node
            if let Some(sql) = &service.sql_store {
                let graph = sql.graph_store();
                let _ = graph.deactivate_by_memory_id(&old_mid).await;
                let _ = graph.update_content_by_memory_id(&m.memory_id, &m.content).await;
            }
            Ok(mcp_text(&format!("Corrected memory {}: {}", m.memory_id, m.content)))
        }

        "memory_purge" => {
            let memory_id = args["memory_id"].as_str().unwrap_or("");
            let topic = args["topic"].as_str().unwrap_or("");
            if !memory_id.is_empty() {
                // Batch: comma-separated IDs
                let ids: Vec<&str> = memory_id.split(',').map(str::trim).filter(|s| !s.is_empty()).collect();
                let count = ids.len();
                for id in &ids {
                    service.purge(id).await?;
                    // Graph sync: deactivate graph node (best-effort)
                    if let Some(sql) = &service.sql_store {
                        let _ = sql.graph_store().deactivate_by_memory_id(id).await;
                    }
                }
                Ok(mcp_text(&format!("Purged {count} memory(s)")))
            } else if !topic.is_empty() {
                // Bulk by keyword: exact text match then purge
                let count = service.purge_by_topic(user_id, topic).await?;
                Ok(mcp_text(&format!("Purged {count} memory(s) matching '{topic}'")))
            } else {
                Ok(mcp_text("Provide memory_id or topic"))
            }
        }

        "memory_profile" => {
            let memories = service.list_active(user_id, 50).await?;
            let profile_mems: Vec<_> = memories.iter()
                .filter(|m| m.memory_type == MemoryType::Profile)
                .collect();
            if profile_mems.is_empty() {
                return Ok(mcp_text("No profile memories found."));
            }
            let text = profile_mems.iter()
                .map(|m| m.content.as_str())
                .collect::<Vec<_>>().join("\n");
            Ok(mcp_text(&text))
        }

        "memory_list" => {
            let limit = args["limit"].as_i64().unwrap_or(20);
            let memories = service.list_active(user_id, limit).await?;
            if memories.is_empty() {
                return Ok(mcp_text("No memories found."));
            }
            let text = memories.iter().map(|m| {
                format!("[{}] ({}) {}", m.memory_id, m.memory_type, m.content)
            }).collect::<Vec<_>>().join("\n");
            Ok(mcp_text(&text))
        }

        "memory_capabilities" => Ok(mcp_text(
            "Available tools: memory_store, memory_retrieve, memory_search, \
             memory_correct, memory_purge, memory_profile, memory_list, \
             memory_capabilities, memory_governance, memory_rebuild_index, \
             memory_consolidate, memory_reflect, memory_extract_entities, \
             memory_link_entities, memory_observe"
        )),

        "memory_governance" => {
            let force = args["force"].as_bool().unwrap_or(false);
            let sql = match &service.sql_store {
                Some(s) => s.clone(),
                None => return Ok(mcp_text("Governance requires SQL store")),
            };
            const COOLDOWN_SECS: i64 = 3600; // 1 hour
            if !force {
                if let Some(remaining) = sql.check_cooldown(user_id, "governance", COOLDOWN_SECS).await? {
                    return Ok(mcp_text(&format!(
                        "Governance skipped (cooldown: {remaining}s remaining). Use force=true to override."
                    )));
                }
            }
            let quarantined = sql.quarantine_low_confidence(user_id).await?;
            let cleaned = sql.cleanup_stale(user_id).await?;
            sql.set_cooldown(user_id, "governance").await?;

            // Snapshot health
            let snap_health = {
                let snaps = sqlx::query("SHOW SNAPSHOTS")
                    .fetch_all(sql.pool()).await.unwrap_or_default();
                let total = snaps.len();
                let auto = snaps.iter().filter(|r| {
                    let name: String = r.try_get("SNAPSHOT_NAME").unwrap_or_default();
                    name.starts_with("mem_milestone_") || name.starts_with("mem_snap_pre_")
                }).count();
                let ratio = if total > 0 { auto as f64 / total as f64 } else { 0.0 };
                format!("snapshots: {total} total, {:.0}% auto-generated", ratio * 100.0)
            };

            Ok(mcp_text(&format!(
                "Governance complete: quarantined={quarantined}, cleaned_stale={cleaned}. {snap_health}"
            )))
        }

        "memory_rebuild_index" => {
            let table = args["table"].as_str().unwrap_or("mem_memories");
            if !["mem_memories", "memory_graph_nodes"].contains(&table) {
                return Ok(mcp_text(&format!("Invalid table '{table}'. Use mem_memories or memory_graph_nodes")));
            }
            let sql = match &service.sql_store {
                Some(s) => s.clone(),
                None => return Ok(mcp_text("Rebuild index requires SQL store")),
            };
            let total_rows = sql.rebuild_vector_index(table).await
                .map_err(|e| anyhow::anyhow!("rebuild index failed: {e}"))?;
            Ok(mcp_text(&format!(
                "Rebuilt IVF index for {table}: rows={total_rows}"
            )))
        }

        "memory_consolidate" => {
            let force = args["force"].as_bool().unwrap_or(false);
            let sql = match &service.sql_store {
                Some(s) => s.clone(),
                None => return Ok(mcp_text("Consolidate requires SQL store")),
            };
            const COOLDOWN_SECS: i64 = 1800; // 30 minutes
            if !force {
                if let Some(remaining) = sql.check_cooldown(user_id, "consolidate", COOLDOWN_SECS).await? {
                    return Ok(mcp_text(&format!(
                        "Consolidation skipped (cooldown: {remaining}s remaining). Use force=true to override."
                    )));
                }
            }

            let graph = sql.graph_store();
            let consolidator = memoria_storage::GraphConsolidator::new(&graph);
            let result = consolidator.consolidate(user_id).await;

            sql.set_cooldown(user_id, "consolidate").await?;

            let mut msg = format!(
                "Consolidation complete: conflicts_detected={}, orphaned_scenes={}, promoted={}, demoted={}",
                result.conflicts_detected, result.orphaned_scenes, result.promoted, result.demoted
            );
            if !result.errors.is_empty() {
                msg.push_str(&format!(" (warnings: {})", result.errors.join("; ")));
            }
            Ok(mcp_text(&msg))
        }

        "memory_reflect" => {
            let force = args["force"].as_bool().unwrap_or(false);
            let mode = args["mode"].as_str().unwrap_or("auto");
            let sql = match &service.sql_store {
                Some(s) => s.clone(),
                None => return Ok(mcp_text("Reflect requires SQL store")),
            };

            if mode == "internal" && service.llm.is_none() {
                return Ok(mcp_text(
                    "Reflection with internal LLM requires LLM_API_KEY to be set."
                ));
            }

            const COOLDOWN_SECS: i64 = 7200; // 2 hours
            if mode != "candidates" && !force {
                if let Some(remaining) = sql.check_cooldown(user_id, "reflect", COOLDOWN_SECS).await? {
                    return Ok(mcp_text(&format!(
                        "Reflection skipped (cooldown: {remaining}s remaining). Use force=true to override."
                    )));
                }
            }

            let graph = sql.graph_store();
            let clusters = build_reflect_clusters(&graph, user_id).await?;

            if clusters.is_empty() {
                return Ok(mcp_text(
                    "No memory clusters found for reflection. \
                     Store more memories across multiple sessions first."
                ));
            }

            // candidates mode: return raw clusters for agent to synthesize
            if mode == "candidates" || service.llm.is_none() {
                let mut parts = Vec::new();
                for (i, (signal, importance, mems)) in clusters.iter().enumerate() {
                    let mem_lines: Vec<String> = mems.iter()
                        .map(|(_, c, _)| format!("  - [semantic] {c}"))
                        .collect();
                    parts.push(format!(
                        "Cluster {} ({signal}, importance={importance:.3}):\n{}",
                        i + 1, mem_lines.join("\n")
                    ));
                }
                return Ok(mcp_text(&format!(
                    "Here are memory clusters for reflection. Synthesize 1-2 insights per cluster, \
                     then store each via memory_store(content=..., memory_type='semantic').\n\n{}",
                    parts.join("\n\n")
                )));
            }

            // auto/internal mode: use LLM to synthesize insights
            let llm = service.llm.as_ref().unwrap();
            let mut scenes_created = 0usize;

            // Get existing high-confidence memories as "existing knowledge"
            let existing_rows = sqlx::query(
                "SELECT content FROM mem_memories WHERE user_id = ? AND is_active = 1 \
                 AND trust_tier IN ('T1','T2') ORDER BY created_at DESC LIMIT 10"
            )
            .bind(user_id)
            .fetch_all(sql.pool()).await.map_err(|e| anyhow::anyhow!("{e}"))?;
            let existing_knowledge = existing_rows.iter()
                .filter_map(|r| r.try_get::<String, _>("content").ok())
                .collect::<Vec<_>>().join("\n");

            for (_, _, mems) in &clusters {
                let experiences = mems.iter()
                    .map(|(_, c, _)| format!("- {c}"))
                    .collect::<Vec<_>>().join("\n");

                let prompt = reflection_prompt(&experiences, &existing_knowledge);
                let msgs = vec![
                    memoria_embedding::ChatMessage { role: "user".to_string(), content: prompt }
                ];
                let raw = match llm.chat(&msgs, 0.3, Some(400)).await {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::warn!("LLM reflect call failed: {e}");
                        continue;
                    }
                };

                // Parse JSON array from response
                let start = raw.find('[').unwrap_or(raw.len());
                let end = raw.rfind(']').map(|i| i + 1).unwrap_or(raw.len());
                if start >= end { continue; }
                let items: Vec<serde_json::Value> = match serde_json::from_str(&raw[start..end]) {
                    Ok(v) => v,
                    Err(_) => continue,
                };

                for item in &items {
                    let content = item["content"].as_str().unwrap_or("").trim().to_string();
                    if content.is_empty() { continue; }
                    let mt_str = item["type"].as_str().unwrap_or("semantic");
                    let mt = MemoryType::from_str(mt_str).unwrap_or(MemoryType::Semantic);
                    let confidence = item["confidence"].as_f64().unwrap_or(0.5) as f32;
                    // Store as T4 (unverified insight from reflection)
                    let _ = service.store_memory(
                        user_id, &content, mt, None,
                        Some(TrustTier::from_str("T4").unwrap_or(TrustTier::T4Unverified)),
                        None, None,
                    ).await;
                    scenes_created += 1;
                    let _ = confidence; // used in future for graph node confidence
                }
            }

            sql.set_cooldown(user_id, "reflect").await?;
            Ok(mcp_text(&format!(
                "Reflection complete: scenes_created={scenes_created}, candidates_found={}",
                clusters.len()
            )))
        }

        "memory_extract_entities" => {
            let mode = args["mode"].as_str().unwrap_or("auto");
            let sql = match &service.sql_store {
                Some(s) => s.clone(),
                None => return Ok(mcp_text("Extract entities requires SQL store")),
            };

            if mode == "internal" && service.llm.is_none() {
                return Ok(mcp_text(
                    "LLM entity extraction requires LLM_API_KEY to be set."
                ));
            }

            let graph = sql.graph_store();
            let unlinked = graph.get_unlinked_memories(user_id, 50).await?;

            if unlinked.is_empty() {
                return Ok(mcp_text(&serde_json::to_string(&json!({
                    "status": "complete",
                    "unlinked": 0,
                    "message": "All memories already have entity links."
                }))?));
            }

            // auto with LLM: extract via LLM and write directly
            if mode != "candidates" {
              if let Some(llm) = service.llm.as_ref() {
                let mut total_created = 0usize;
                let mut total_edges = 0usize;

                for (memory_id, content) in &unlinked {
                    let prompt = entity_extract_prompt(content);
                    let msgs = vec![
                        memoria_embedding::ChatMessage { role: "user".to_string(), content: prompt }
                    ];
                    let raw = match llm.chat(&msgs, 0.0, Some(300)).await {
                        Ok(r) => r,
                        Err(_) => continue,
                    };
                    let start = raw.find('[').unwrap_or(raw.len());
                    let end = raw.rfind(']').map(|i| i + 1).unwrap_or(raw.len());
                    if start >= end { continue; }
                    let items: Vec<serde_json::Value> = match serde_json::from_str(&raw[start..end]) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };
                    for item in &items {
                        let name = item["name"].as_str().unwrap_or("").trim().to_lowercase();
                        if name.is_empty() { continue; }
                        let display = item["name"].as_str().unwrap_or("").trim().to_string();
                        let etype = item["type"].as_str().unwrap_or("concept").to_string();
                        if let Ok((entity_id, is_new)) = graph.upsert_entity(user_id, &name, &display, &etype).await {
                            let _ = graph.upsert_memory_entity_link(memory_id, &entity_id, user_id, "llm").await;
                            if is_new { total_created += 1; total_edges += 1; }
                        }
                    }
                }

                return Ok(mcp_text(&serde_json::to_string(&json!({
                    "status": "done",
                    "total_memories": unlinked.len(),
                    "entities_found": total_created,
                    "edges_created": total_edges
                }))?));
              }
            }

            // candidates mode (or auto without LLM): return for agent to process
            let existing = graph.get_user_entities(user_id).await?;
            let existing_json: Vec<serde_json::Value> = existing.iter()
                .map(|(name, etype)| json!({"name": name, "entity_type": etype}))
                .collect();
            let memories_json: Vec<serde_json::Value> = unlinked.iter()
                .map(|(mid, content)| json!({"memory_id": mid, "content": content}))
                .collect();

            Ok(mcp_text(&serde_json::to_string(&json!({
                "status": "candidates",
                "unlinked": memories_json.len(),
                "memories": memories_json,
                "existing_entities": existing_json,
                "instruction": "Extract named entities (people, tech, projects, repos) from each memory, then call memory_link_entities."
            }))?))
        }

        "memory_link_entities" => {
            let entities_str = args["entities"].as_str().unwrap_or("");
            let sql = match &service.sql_store {
                Some(s) => s.clone(),
                None => return Ok(mcp_text("Link entities requires SQL store")),
            };

            let parsed: Vec<serde_json::Value> = match serde_json::from_str(entities_str) {
                Ok(v) => v,
                Err(_) => return Ok(mcp_text(&serde_json::to_string(&json!({
                    "status": "error",
                    "error": "Invalid JSON",
                    "expected_format": [{"memory_id": "...", "entities": [{"name": "...", "type": "..."}]}]
                }))?)),
            };

            let graph = sql.graph_store();
            let mut total_created = 0usize;
            let mut total_reused = 0usize;
            let mut total_edges = 0usize;

            for item in &parsed {
                let memory_id = match item["memory_id"].as_str() {
                    Some(id) => id,
                    None => continue,
                };
                let ents: Vec<(String, String, String)> = item["entities"].as_array()
                    .unwrap_or(&vec![])
                    .iter()
                    .filter_map(|e| {
                        let name = e["name"].as_str()?.trim().to_lowercase();
                        if name.is_empty() { return None; }
                        let display = e["name"].as_str().unwrap_or("").trim().to_string();
                        let etype = e["type"].as_str().unwrap_or("concept").to_string();
                        Some((name, display, etype))
                    })
                    .collect();

                if ents.is_empty() { continue; }
                for (name, display, etype) in &ents {
                    let (entity_id, is_new) = graph.upsert_entity(user_id, name, display, etype).await?;
                    graph.upsert_memory_entity_link(memory_id, &entity_id, user_id, "manual").await?;
                    if is_new { total_created += 1; total_edges += 1; } else { total_reused += 1; }
                }
            }

            Ok(mcp_text(&serde_json::to_string(&json!({
                "status": "done",
                "entities_created": total_created,
                "entities_reused": total_reused,
                "edges_created": total_edges
            }))?))
        }

        "memory_observe" => {
            let messages = args["messages"].as_array().cloned().unwrap_or_default();
            let session_id = args["session_id"].as_str().map(String::from);

            let (memories, has_llm) = service.observe_turn(user_id, &messages, session_id.clone()).await?;

            // Graph sync (best-effort) for each stored memory
            for m in &memories {
                if let Some(sql) = &service.sql_store {
                    let graph = sql.graph_store();
                    let node = memoria_storage::GraphNode {
                        node_id: Uuid::new_v4().simple().to_string()[..32].to_string(),
                        user_id: user_id.to_string(),
                        node_type: memoria_storage::NodeType::Semantic,
                        content: m.content.clone(),
                        entity_type: None,
                        embedding: None,
                        memory_id: Some(m.memory_id.clone()),
                        session_id: session_id.clone(),
                        confidence: m.initial_confidence as f32,
                        trust_tier: format!("{}", m.trust_tier),
                        importance: 0.5,
                        source_nodes: vec![],
                        conflicts_with: None,
                        conflict_resolution: None,
                        access_count: 0,
                        cross_session_count: 0,
                        is_active: true,
                        superseded_by: None,
                        created_at: m.created_at.map(|dt| dt.naive_utc()),
                    };
                    let _ = graph.create_node(&node).await;
                    let entities = memoria_storage::extract_entities(&m.content);
                    for ent in &entities {
                        if let Ok((entity_id, _)) = graph.upsert_entity(
                            user_id, &ent.name, &ent.display, &ent.entity_type
                        ).await {
                            let _ = graph.upsert_memory_entity_link(
                                &m.memory_id, &entity_id, user_id, "regex"
                            ).await;
                        }
                    }
                }
            }

            let stored: Vec<_> = memories.iter().map(|m| json!({
                "memory_id": m.memory_id,
                "content": m.content,
                "memory_type": m.memory_type.to_string(),
            })).collect();

            let mut result = json!({ "memories": stored });
            if !has_llm {
                result["warning"] = json!("LLM not configured — storing messages as-is without extraction");
            }
            Ok(mcp_text(&serde_json::to_string_pretty(&result)?))
        }

        _ => Err(anyhow::anyhow!("Unknown tool: {name}")),
    }
}

fn mcp_text(text: &str) -> Value {
    json!({"content": [{"type": "text", "text": text}]})
}

// ── Graph-based reflection helpers ───────────────────────────────────────────

/// Build reflection clusters from graph nodes using connected components.
/// Returns Vec<(signal, importance, Vec<(memory_id, content, session_id)>)>
pub async fn build_reflect_clusters(
    graph: &memoria_storage::GraphStore,
    user_id: &str,
) -> anyhow::Result<Vec<(String, f32, Vec<(String, String, Option<String>)>)>> {
    const MIN_CLUSTER_SIZE: usize = 3;
    const MIN_SESSIONS: usize = 2;

    let count = graph.count_user_nodes(user_id).await?;
    if count < MIN_CLUSTER_SIZE as i64 {
        return Ok(vec![]);
    }

    // Get recent semantic nodes as candidates
    let semantic_nodes = graph.get_user_nodes(user_id, &memoria_storage::NodeType::Semantic, true).await?;
    if semantic_nodes.len() < MIN_CLUSTER_SIZE {
        return Ok(vec![]);
    }

    // Take most recent 50 nodes
    let mut nodes = semantic_nodes;
    nodes.sort_by(|a, b| b.node_id.cmp(&a.node_id));
    nodes.truncate(50);

    let node_ids: Vec<String> = nodes.iter().map(|n| n.node_id.clone()).collect();
    let edges = graph.get_edges_for_nodes(&node_ids).await?;

    // Build adjacency for connected components
    let node_set: std::collections::HashSet<&str> = node_ids.iter().map(|s| s.as_str()).collect();
    let mut adjacency: std::collections::HashMap<&str, Vec<&str>> = Default::default();
    for (src, tgt) in &edges {
        if node_set.contains(src.as_str()) && node_set.contains(tgt.as_str()) {
            adjacency.entry(src.as_str()).or_default().push(tgt.as_str());
            adjacency.entry(tgt.as_str()).or_default().push(src.as_str());
        }
    }

    let node_map: std::collections::HashMap<&str, &memoria_storage::GraphNode> =
        nodes.iter().map(|n| (n.node_id.as_str(), n)).collect();

    // BFS connected components
    let mut visited = std::collections::HashSet::new();
    let mut clusters = Vec::new();
    for nid in &node_ids {
        if visited.contains(nid.as_str()) { continue; }
        let mut component = Vec::new();
        let mut queue = vec![nid.as_str()];
        while let Some(cur) = queue.pop() {
            if !visited.insert(cur) { continue; }
            if let Some(node) = node_map.get(cur) {
                component.push(*node);
            }
            for &neighbor in adjacency.get(cur).unwrap_or(&vec![]) {
                if !visited.contains(neighbor) {
                    queue.push(neighbor);
                }
            }
        }
        if component.len() >= MIN_CLUSTER_SIZE {
            let sessions: std::collections::HashSet<&str> = component.iter()
                .filter_map(|n| n.session_id.as_deref())
                .collect();
            if sessions.len() >= MIN_SESSIONS {
                let has_conflict = component.iter().any(|n| n.conflicts_with.is_some());
                let signal = if has_conflict { "contradiction" } else { "semantic_cluster" };
                let importance = 0.5 + (component.len() as f32 * 0.05).min(0.3);
                let mems: Vec<(String, String, Option<String>)> = component.iter()
                    .map(|n| (
                        n.memory_id.clone().unwrap_or_else(|| n.node_id.clone()),
                        n.content.clone(),
                        n.session_id.clone(),
                    ))
                    .collect();
                clusters.push((signal.to_string(), importance, mems));
            }
        }
    }

    // If no graph clusters (no edges yet), fall back to session-based grouping
    if clusters.is_empty() {
        let mut by_session: std::collections::HashMap<String, Vec<(String, String, Option<String>)>> = Default::default();
        for node in &nodes {
            let sid = node.session_id.clone().unwrap_or_else(|| "default".to_string());
            by_session.entry(sid.clone()).or_default().push((
                node.memory_id.clone().unwrap_or_else(|| node.node_id.clone()),
                node.content.clone(),
                node.session_id.clone(),
            ));
        }
        for (_, mems) in by_session {
            if mems.len() >= MIN_CLUSTER_SIZE {
                clusters.push(("semantic_cluster".to_string(), 0.5, mems));
            }
        }
    }

    Ok(clusters)
}

/// LLM prompt for reflection synthesis (matches Python's REFLECTION_SYNTHESIS_PROMPT).
pub fn reflection_prompt(experiences: &str, existing_knowledge: &str) -> String {
    format!(
        "SYSTEM:\nYou are analyzing an agent's experiences across multiple sessions with the same user.\n\
         Your goal: extract 1-2 reusable insights that will help the agent serve this user better.\n\n\
         RULES:\n\
         - Each insight must be ACTIONABLE (a behavioral rule or factual pattern), not just descriptive\n\
         - Each insight must be grounded in the evidence — do not speculate beyond what's shown\n\
         - If the experiences don't reveal a clear pattern, return an empty array []\n\
         - Assign confidence conservatively: 0.3-0.5 for weak patterns, 0.5-0.7 for strong ones\n\
         - Type \"procedural\" = how to do something; \"semantic\" = what is true about the user/project\n\n\
         EXISTING KNOWLEDGE (do not repeat these):\n{existing_knowledge}\n\n\
         EXPERIENCES TO ANALYZE:\n{experiences}\n\n\
         OUTPUT FORMAT (JSON array, 0-2 items):\n\
         [\n  {{\"type\": \"procedural\" | \"semantic\", \"content\": \"One clear sentence\", \
         \"confidence\": 0.3-0.7, \"evidence_summary\": \"Which experiences support this\"}}\n]"
    )
}

/// LLM prompt for entity extraction (matches Python's _LLM_EXTRACT_PROMPT).
pub fn entity_extract_prompt(text: &str) -> String {
    format!(
        "Extract named entities from the following text. Return a JSON array of objects.\n\
         Each object: {{\"name\": \"canonical name\", \"type\": \"tech|person|repo|project|concept\"}}\n\n\
         Rules:\n\
         - Only extract specific, named entities (not generic words)\n\
         - For tech terms: use canonical form (React, Spark, OAuth)\n\
         - Deduplicate: include each entity once\n\
         - Max 10 entities per text\n\
         - Do NOT extract: generic verbs, common nouns, numbers, dates\n\n\
         Text:\n{}\n\nJSON array:",
        &text[..text.len().min(2000)]
    )
}
