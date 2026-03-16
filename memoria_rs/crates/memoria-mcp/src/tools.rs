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
                    "new_content": {"type": "string"},
                    "reason": {"type": "string"}
                },
                "required": ["memory_id", "new_content"]
            }
        },
        {
            "name": "memory_purge",
            "description": "Delete a memory by ID",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "memory_id": {"type": "string"},
                    "reason": {"type": "string"}
                },
                "required": ["memory_id"]
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
            let memory_id = args["memory_id"].as_str().unwrap_or("");
            let new_content = args["new_content"].as_str().unwrap_or("");
            let m = service.correct(memory_id, new_content).await?;
            Ok(mcp_text(&format!("Corrected memory {}: {}", m.memory_id, m.content)))
        }

        "memory_purge" => {
            let memory_id = args["memory_id"].as_str().unwrap_or("");
            service.purge(memory_id).await?;
            Ok(mcp_text(&format!("Purged memory {memory_id}")))
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
