/// 8 core MCP tools for Phase 2.
/// Phase 4 will add 14 more (Git-for-Data, admin, graph).

use anyhow::Result;
use memoria_core::{MemoryType, TrustTier};
use memoria_service::MemoryService;
use serde_json::{json, Value};
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
             memory_correct, memory_purge, memory_profile, memory_list, memory_capabilities"
        )),

        _ => Err(anyhow::anyhow!("Unknown tool: {name}")),
    }
}

fn mcp_text(text: &str) -> Value {
    json!({"content": [{"type": "text", "text": text}]})
}
