use anyhow::Result;
use memoria_core::MemoryType;
use serde_json::Value;
use std::str::FromStr;

pub(crate) struct MemoryPurgeArgs {
    pub(crate) memory_id: Option<String>,
    pub(crate) topic: Option<String>,
    pub(crate) session_id: Option<String>,
    pub(crate) memory_types: Option<Vec<MemoryType>>,
}

pub(crate) fn parse_memory_purge_args(args: &Value) -> Result<MemoryPurgeArgs> {
    let memory_id = args["memory_id"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let topic = args["topic"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let session_id = args["session_id"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let memory_types = match args.get("memory_types") {
        None | Some(Value::Null) => None,
        Some(Value::Array(values)) => Some(
            values
                .iter()
                .map(|value| {
                    value
                        .as_str()
                        .ok_or_else(|| anyhow::anyhow!("memory_types entries must be strings"))
                        .and_then(|value| {
                            MemoryType::from_str(value).map_err(|e| anyhow::anyhow!("{e}"))
                        })
                })
                .collect::<Result<Vec<_>>>()?,
        ),
        Some(_) => return Err(anyhow::anyhow!("memory_types must be an array of strings")),
    }
    .filter(|types| !types.is_empty());
    if memory_types.is_some() && session_id.is_none() {
        return Err(anyhow::anyhow!("memory_types requires session_id"));
    }
    let selector_count = usize::from(memory_id.is_some())
        + usize::from(topic.is_some())
        + usize::from(session_id.is_some());
    if selector_count > 1 {
        return Err(anyhow::anyhow!(
            "Provide only one of memory_id, topic, or session_id"
        ));
    }
    Ok(MemoryPurgeArgs {
        memory_id,
        topic,
        session_id,
        memory_types,
    })
}
