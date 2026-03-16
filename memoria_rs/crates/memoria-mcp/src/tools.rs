/// 8 core MCP tools for Phase 2.
/// Phase 4 will add 14 more (Git-for-Data, admin, graph).

use anyhow::Result;
use memoria_core::{MemoryType, TrustTier};
use memoria_service::MemoryService;
use serde_json::{json, Value};
use sqlx::Row;
use std::str::FromStr;
use std::sync::Arc;

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
                    "session_id": {"type": "string"}
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
                    "top_k": {"type": "integer", "default": 10}
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
        }
    ])
}

pub async fn call(
    name: &str,
    args: Value,
    service: &Arc<MemoryService>,
    user_id: &str,
) -> Result<Value> {
    match name {
        "memory_store" => {
            let content = args["content"].as_str().unwrap_or("").to_string();
            let memory_type = args["memory_type"].as_str().unwrap_or("semantic");
            let session_id = args["session_id"].as_str().map(String::from);
            let trust_tier = args["trust_tier"].as_str()
                .map(TrustTier::from_str).transpose().ok().flatten();
            let mt = MemoryType::from_str(memory_type)
                .unwrap_or(MemoryType::Semantic);
            let m = service.store_memory(user_id, &content, mt, session_id, trust_tier).await?;
            Ok(mcp_text(&format!("Stored memory {}: {}", m.memory_id, m.content)))
        }

        "memory_retrieve" | "memory_search" => {
            let query = args["query"].as_str().unwrap_or("").to_string();
            let top_k = args["top_k"].as_i64().unwrap_or(5);
            let results = service.retrieve(user_id, &query, top_k).await?;
            if results.is_empty() {
                return Ok(mcp_text("No relevant memories found."));
            }
            let text = results.iter().map(|m| {
                format!("[{}] ({}) {}", m.memory_id, m.memory_type, m.content)
            }).collect::<Vec<_>>().join("\n");
            Ok(mcp_text(&text))
        }

        "memory_correct" => {
            let new_content = args["new_content"].as_str().unwrap_or("");
            if new_content.is_empty() {
                return Ok(mcp_text("new_content is required"));
            }
            let memory_id = args["memory_id"].as_str().unwrap_or("");
            let query = args["query"].as_str().unwrap_or("");
            let m = if !memory_id.is_empty() {
                service.correct(memory_id, new_content).await?
            } else if !query.is_empty() {
                // Semantic search to find best match, then correct it
                let results = service.retrieve(user_id, query, 1).await?;
                match results.into_iter().next() {
                    Some(found) => {
                        service.correct(&found.memory_id, new_content).await?
                    }
                    None => return Ok(mcp_text("No matching memory found for query")),
                }
            } else {
                return Ok(mcp_text("Provide memory_id or query"));
            };
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
                }
                Ok(mcp_text(&format!("Purged {count} memory(s)")))
            } else if !topic.is_empty() {
                // Bulk by keyword: search then purge all matches
                let results = service.retrieve(user_id, topic, 100).await?;
                let count = results.len();
                for m in &results {
                    service.purge(&m.memory_id).await?;
                }
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
             memory_consolidate, memory_reflect, memory_extract_entities, memory_link_entities"
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
            // Count rows to compute optimal lists
            let count_row = sqlx::query(&format!("SELECT COUNT(*) as cnt FROM {table}"))
                .fetch_one(sql.pool()).await.map_err(|e| anyhow::anyhow!("{e}"))?;
            let total_rows: i64 = count_row.try_get("cnt").unwrap_or(0);
            let new_lists = (total_rows / 50).max(1).min(1024);
            // Rebuild: drop + recreate IVF index
            let idx_name = format!("{table}_embedding_ivf");
            let _ = sqlx::raw_sql(&format!("ALTER TABLE {table} DROP INDEX {idx_name}"))
                .execute(sql.pool()).await; // ignore error if not exists
            sqlx::raw_sql(&format!(
                "ALTER TABLE {table} ADD INDEX {idx_name} (embedding) USING IVFFLAT LISTS={new_lists}"
            ))
            .execute(sql.pool()).await
            .map_err(|e| anyhow::anyhow!("rebuild index failed: {e}"))?;
            Ok(mcp_text(&format!(
                "Rebuilt IVF index for {table}: lists={new_lists} (rows={total_rows})"
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

            // candidates mode (or auto without internal LLM — we never have internal LLM in Rust)
            if mode == "internal" {
                return Ok(mcp_text(
                    "Reflection with internal LLM is not available in this deployment. \
                     Use mode='candidates' to get raw clusters for synthesis."
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

            // Cluster memories by memory_type — each type is a "cluster"
            let rows = sqlx::query(
                "SELECT memory_type, content, memory_id FROM mem_memories \
                 WHERE user_id = ? AND is_active = 1 ORDER BY memory_type, created_at DESC"
            )
            .bind(user_id)
            .fetch_all(sql.pool()).await.map_err(|e| anyhow::anyhow!("{e}"))?;

            if rows.is_empty() {
                return Ok(mcp_text("No memories found for reflection."));
            }

            // Group by memory_type, take top 5 per cluster
            let mut clusters: std::collections::BTreeMap<String, Vec<(String, String)>> = Default::default();
            for row in &rows {
                let mtype: String = row.try_get("memory_type").unwrap_or_default();
                let content: String = row.try_get("content").unwrap_or_default();
                let mid: String = row.try_get("memory_id").unwrap_or_default();
                let entries = clusters.entry(mtype).or_default();
                if entries.len() < 5 {
                    entries.push((mid, content));
                }
            }

            if clusters.is_empty() {
                return Ok(mcp_text("No memory clusters found for reflection."));
            }

            if mode != "candidates" {
                sql.set_cooldown(user_id, "reflect").await?;
            }

            let mut parts = Vec::new();
            for (i, (mtype, mems)) in clusters.iter().enumerate() {
                let mem_lines: Vec<String> = mems.iter()
                    .map(|(_, c)| format!("  - [{}] {}", mtype, c))
                    .collect();
                parts.push(format!(
                    "Cluster {} ({}, importance=0.5):\n{}",
                    i + 1, mtype, mem_lines.join("\n")
                ));
            }

            Ok(mcp_text(&format!(
                "Here are memory clusters for reflection. Synthesize 1-2 insights per cluster, \
                 then store each via memory_store(content=..., memory_type='semantic').\n\n{}",
                parts.join("\n\n")
            )))
        }

        "memory_extract_entities" => {
            let mode = args["mode"].as_str().unwrap_or("auto");
            let sql = match &service.sql_store {
                Some(s) => s.clone(),
                None => return Ok(mcp_text("Extract entities requires SQL store")),
            };

            if mode == "internal" {
                return Ok(mcp_text(
                    "LLM entity extraction is not available in this deployment. \
                     Use mode='candidates' to get unlinked memories for manual extraction."
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

        _ => Err(anyhow::anyhow!("Unknown tool: {name}")),
    }
}

fn mcp_text(text: &str) -> Value {
    json!({"content": [{"type": "text", "text": text}]})
}
