use anyhow::Result;
use memoria_service::MemoryService;
use serde_json::Value;
use std::sync::Arc;

pub const TOOL_NAMES: &[&str] = &[
    "memory_v2_remember",
    "memory_v2_recall",
    "memory_v2_list",
    "memory_v2_profile",
    "memory_v2_expand",
    "memory_v2_focus",
    "memory_v2_history",
    "memory_v2_update",
    "memory_v2_forget",
    "memory_v2_reflect",
];

pub const TOOL_NAMES_TEXT: &str = "memory_v2_remember, memory_v2_recall, memory_v2_list, \
memory_v2_profile, memory_v2_expand, memory_v2_focus, memory_v2_history, memory_v2_update, \
memory_v2_forget, memory_v2_reflect";

pub fn list() -> Vec<Value> {
    crate::v2::tools::list()
        .as_array()
        .cloned()
        .unwrap_or_default()
}

pub fn handles_tool(name: &str) -> bool {
    TOOL_NAMES.contains(&name)
}

pub async fn call_embedded(
    name: &str,
    args: Value,
    service: &Arc<MemoryService>,
    user_id: &str,
) -> Result<Option<Value>> {
    if !handles_tool(name) {
        return Ok(None);
    }
    Ok(Some(
        crate::v2::tools::call(name, args, service, user_id).await?,
    ))
}
