use anyhow::Result;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use chrono::{DateTime, Utc};
use memoria_core::{MemoryType, TrustTier};
use memoria_service::MemoryService;
use memoria_storage::{
    ExpandLevel, FocusV2Input, ListV2Filter, MemoryV2ExpandResult, MemoryV2FeedbackImpact,
    MemoryV2LinkProvenance, MemoryV2RecallItem, MemoryV2RecallRanking, MemoryV2RememberInput,
    MemoryV2UpdateInput, RecallV2Request, ReflectV2Filter,
};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::{json, Value};
use std::{str::FromStr, sync::Arc};

const DEFAULT_LIST_LIMIT: i64 = 50;
const DEFAULT_TOP_K: i64 = 20;
const DEFAULT_MAX_TOKENS: usize = 500;
const DEFAULT_SCOPE: &str = "all";
const DEFAULT_TAG_FILTER_MODE: &str = "any";
const DEFAULT_RECALL_VIEW: &str = "compact";
const RECALL_FILTER_FETCH_MULTIPLIER: i64 = 3;
const DEFAULT_REFLECT_MODE: &str = "auto";
const DEFAULT_REFLECT_MIN_CLUSTER_SIZE: i64 = 2;
const DEFAULT_REFLECT_MIN_LINK_STRENGTH: f64 = 0.35;

pub fn list() -> Value {
    json!([
        {
            "name": "memory_v2_remember",
            "description": "Store a new V2 memory",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "content": {"type": "string"},
                    "type": {"type": "string", "default": "semantic"},
                    "session_id": {"type": "string"},
                    "importance": {"type": "number"},
                    "trust_tier": {"type": "string"},
                    "tags": {"type": "array", "items": {"type": "string"}},
                    "source": {"type": "object"}
                },
                "required": ["content"]
            }
        },
        {
            "name": "memory_v2_recall",
            "description": "Recall V2 memories with compact or full views",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": {"type": "string"},
                    "top_k": {"type": "integer", "default": 20},
                    "max_tokens": {"type": "integer", "default": 500},
                    "scope": {"type": "string", "default": "all"},
                    "session_id": {"type": "string"},
                    "type": {"type": "string"},
                    "expand_links": {"type": "boolean", "default": true},
                    "view": {"type": "string", "default": "compact"},
                    "tags": {"type": "array", "items": {"type": "string"}},
                    "tag_filter_mode": {"type": "string", "default": "any"},
                    "start_at": {"type": "string"},
                    "end_at": {"type": "string"},
                    "time_range": {
                        "type": "object",
                        "properties": {
                            "start_at": {"type": "string"},
                            "end_at": {"type": "string"}
                        }
                    },
                    "min_confidence": {"type": "number"},
                    "min_importance": {"type": "number"},
                    "exclude_memory_ids": {"type": "array", "items": {"type": "string"}},
                    "prefer_recent": {"type": "boolean"},
                    "diversity_factor": {"type": "number"}
                },
                "required": ["query"]
            }
        },
        {
            "name": "memory_v2_list",
            "description": "List active V2 memories",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "limit": {"type": "integer", "default": 50},
                    "cursor": {"type": "string"},
                    "type": {"type": "string"},
                    "session_id": {"type": "string"}
                }
            }
        },
        {
            "name": "memory_v2_profile",
            "description": "List active V2 profile memories",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "limit": {"type": "integer", "default": 50},
                    "cursor": {"type": "string"},
                    "session_id": {"type": "string"}
                }
            }
        },
        {
            "name": "memory_v2_expand",
            "description": "Expand a V2 memory to overview, detail, or links",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "memory_id": {"type": "string"},
                    "level": {"type": "string"}
                },
                "required": ["memory_id", "level"]
            }
        },
        {
            "name": "memory_v2_focus",
            "description": "Create a V2 focus entry",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "type": {"type": "string"},
                    "value": {"type": "string"},
                    "boost": {"type": "number"},
                    "ttl_secs": {"type": "integer"}
                },
                "required": ["type", "value"]
            }
        },
        {
            "name": "memory_v2_history",
            "description": "Read V2 memory event history",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "memory_id": {"type": "string"},
                    "limit": {"type": "integer", "default": 50}
                },
                "required": ["memory_id"]
            }
        },
        {
            "name": "memory_v2_update",
            "description": "Update a V2 memory",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "memory_id": {"type": "string"},
                    "content": {"type": "string"},
                    "importance": {"type": "number"},
                    "trust_tier": {"type": "string"},
                    "tags_add": {"type": "array", "items": {"type": "string"}},
                    "tags_remove": {"type": "array", "items": {"type": "string"}},
                    "reason": {"type": "string"}
                },
                "required": ["memory_id"]
            }
        },
        {
            "name": "memory_v2_forget",
            "description": "Forget a V2 memory",
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
            "name": "memory_v2_reflect",
            "description": "Run V2 reflect candidates or internal write-back",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "mode": {"type": "string", "default": "auto"},
                    "limit": {"type": "integer", "default": 50},
                    "session_id": {"type": "string"},
                    "min_cluster_size": {"type": "integer", "default": 2},
                    "min_link_strength": {"type": "number", "default": 0.35}
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
    let store = v2_store(service)?;
    match name {
        "memory_v2_remember" => {
            let req: RememberToolRequest = serde_json::from_value(args)?;
            let memory_type = parse_memory_type(&req.memory_type)?;
            let trust_tier = parse_optional_trust_tier(req.trust_tier.as_deref())?;
            let embedding = service.embed(&req.content).await?;
            let remembered = store
                .remember(
                    user_id,
                    MemoryV2RememberInput {
                        content: req.content,
                        memory_type,
                        session_id: req.session_id,
                        importance: req.importance,
                        trust_tier,
                        tags: req.tags,
                        source: req.source,
                        embedding,
                        actor: "mcp_v2_remember".to_string(),
                    },
                )
                .await?;
            Ok(mcp_json(&json!({
                "memory_id": remembered.memory_id,
                "abstract": remembered.abstract_text,
                "has_overview": remembered.has_overview,
                "has_detail": remembered.has_detail,
            })))
        }
        "memory_v2_recall" => {
            let req: RecallToolRequest = serde_json::from_value(args)?;
            let response_mode = req.resolved_response_mode()?;
            let with_overview = req.resolved_with_overview()?;
            let with_links = req.resolved_with_links()?;
            let expand_links = req.resolved_expand_links();
            let tag_filter_mode = req.resolved_tag_filter_mode()?;
            let (created_after, created_before) = req.resolved_time_range()?;
            let session_only = match req.scope.as_str() {
                "all" => false,
                "session" => true,
                _ => anyhow::bail!("scope must be 'all' or 'session'"),
            };
            let memory_type = parse_optional_memory_type(req.memory_type.as_deref())?;
            let query_embedding = service.embed(&req.query).await?;
            let min_confidence = req.resolved_min_confidence()?;
            let min_importance = req.resolved_min_importance()?;
            let exclude_memory_ids = req.resolved_exclude_memory_ids();
            let prefer_recent = req.prefer_recent.unwrap_or(false);
            let diversity_factor = req.resolved_diversity_factor()?;
            let has_active_filters = min_confidence.is_some()
                || min_importance.is_some()
                || !exclude_memory_ids.is_empty()
                || prefer_recent
                || diversity_factor.unwrap_or(0.0) > 0.0;
            let store_top_k = if has_active_filters {
                req.top_k
                    .saturating_mul(RECALL_FILTER_FETCH_MULTIPLIER)
                    .clamp(1, 200)
            } else {
                req.top_k
            };
            let mut recalled = store
                .recall(
                    user_id,
                    RecallV2Request {
                        query: req.query,
                        top_k: store_top_k,
                        max_tokens: req.max_tokens,
                        session_only,
                        session_id: req.session_id,
                        memory_type,
                        tags: req.tags,
                        tag_filter_mode,
                        created_after,
                        created_before,
                        with_overview,
                        with_links,
                        expand_links,
                        query_embedding,
                    },
                )
                .await?;
            if let Some(min_confidence) = min_confidence {
                recalled.memories.retain(|m| m.confidence >= min_confidence);
            }
            if let Some(min_importance) = min_importance {
                recalled
                    .memories
                    .retain(|m| m.ranking.importance_component >= (0.05 * min_importance));
            }
            if !exclude_memory_ids.is_empty() {
                let excluded: std::collections::HashSet<&str> =
                    exclude_memory_ids.iter().map(String::as_str).collect();
                recalled
                    .memories
                    .retain(|m| !excluded.contains(m.memory_id.as_str()));
            }
            if prefer_recent {
                recalled.memories.sort_by(|a, b| {
                    a.ranking
                        .age_hours
                        .partial_cmp(&b.ranking.age_hours)
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
            }
            if let Some(diversity) = diversity_factor {
                if diversity > 0.0 && recalled.memories.len() > 2 {
                    let mut by_type = std::collections::HashMap::<String, usize>::new();
                    recalled.memories.sort_by(|a, b| {
                        b.score
                            .partial_cmp(&a.score)
                            .unwrap_or(std::cmp::Ordering::Equal)
                    });
                    let mut diversified = Vec::with_capacity(recalled.memories.len());
                    for item in recalled.memories {
                        let t = item.memory_type.to_string();
                        let seen = by_type.get(&t).copied().unwrap_or(0);
                        let allow = seen == 0 || ((seen as f64) < (1.0 / diversity.max(0.1)));
                        if allow {
                            *by_type.entry(t).or_insert(0) += 1;
                            diversified.push(item);
                        }
                    }
                    recalled.memories = diversified;
                }
            }
            let requested_top_k = req.top_k.clamp(1, 200) as usize;
            let truncated = recalled.memories.len() > requested_top_k;
            if truncated {
                recalled.memories.truncate(requested_top_k);
            }
            recalled.has_more = recalled.has_more || truncated;
            recalled.summary.returned_count = recalled.memories.len() as i64;
            let memories = recalled
                .memories
                .into_iter()
                .map(|item| map_recall_item(item, response_mode))
                .collect::<Vec<_>>();
            Ok(mcp_json(&json!({
                "summary": map_recall_summary(recalled.summary),
                "memories": memories,
                "token_used": recalled.token_used,
                "has_more": recalled.has_more,
            })))
        }
        "memory_v2_list" => {
            let req: ListToolRequest = serde_json::from_value(args)?;
            let cursor = req.cursor.as_deref().map(decode_cursor).transpose()?;
            let memory_type = parse_optional_memory_type(req.memory_type.as_deref())?;
            let listed = store
                .list(
                    user_id,
                    ListV2Filter {
                        limit: req.limit,
                        cursor,
                        memory_type,
                        session_id: req.session_id,
                    },
                )
                .await?;
            Ok(mcp_json(&json!({
                "items": listed.items.into_iter().map(map_list_item).collect::<Vec<_>>(),
                "next_cursor": listed.next_cursor.as_ref().map(encode_cursor),
            })))
        }
        "memory_v2_profile" => {
            let req: ProfileToolRequest = serde_json::from_value(args)?;
            let cursor = req.cursor.as_deref().map(decode_cursor).transpose()?;
            let profiled = store
                .profile(
                    user_id,
                    memoria_storage::ProfileV2Filter {
                        limit: req.limit,
                        cursor,
                        session_id: req.session_id,
                    },
                )
                .await?;
            Ok(mcp_json(&json!({
                "items": profiled.items.into_iter().map(map_profile_item).collect::<Vec<_>>(),
                "next_cursor": profiled.next_cursor.as_ref().map(encode_cursor),
            })))
        }
        "memory_v2_expand" => {
            let req: ExpandToolRequest = serde_json::from_value(args)?;
            let level = parse_expand_level(&req.level)?;
            let expanded = store.expand(user_id, &req.memory_id, level).await?;
            Ok(mcp_json(&map_expand_result(expanded)))
        }
        "memory_v2_focus" => {
            let req: FocusToolRequest = serde_json::from_value(args)?;
            let focus_type = req.resolved_focus_type()?;
            let value = req.resolved_value()?;
            let focused = store
                .focus(
                    user_id,
                    FocusV2Input {
                        focus_type,
                        value,
                        boost: req.boost,
                        ttl_secs: req.ttl_secs,
                        actor: "mcp_v2_focus".to_string(),
                    },
                )
                .await?;
            Ok(mcp_json(&json!({
                "focus_id": focused.focus_id,
                "type": focused.focus_type,
                "value": focused.value,
                "boost": focused.boost,
                "active_until": focused.active_until.to_rfc3339(),
            })))
        }
        "memory_v2_history" => {
            let req: HistoryToolRequest = serde_json::from_value(args)?;
            let history = store
                .memory_history(user_id, &req.memory_id, req.limit)
                .await?;
            Ok(mcp_json(&json!({
                "memory_id": history.memory_id,
                "items": history
                    .items
                    .into_iter()
                    .map(|item| json!({
                        "event_id": item.event_id,
                        "event_type": item.event_type,
                        "actor": item.actor,
                        "processing_state": item.processing_state,
                        "payload": normalize_history_payload(item.payload),
                        "created_at": item.created_at.to_rfc3339(),
                    }))
                    .collect::<Vec<_>>(),
            })))
        }
        "memory_v2_update" => {
            let req: UpdateToolRequest = serde_json::from_value(args)?;
            let embedding = match req.content.as_deref() {
                Some(content) => service.embed(content).await?,
                None => None,
            };
            let updated = store
                .update(
                    user_id,
                    MemoryV2UpdateInput {
                        memory_id: req.memory_id,
                        content: req.content,
                        importance: req.importance,
                        trust_tier: parse_optional_trust_tier(req.trust_tier.as_deref())?,
                        tags_add: req.tags_add,
                        tags_remove: req.tags_remove,
                        embedding,
                        actor: "mcp_v2_update".to_string(),
                        reason: req.reason,
                    },
                )
                .await?;
            Ok(mcp_json(&json!({
                "memory_id": updated.memory_id,
                "abstract": updated.abstract_text,
                "updated_at": updated.updated_at.to_rfc3339(),
                "has_overview": updated.has_overview,
                "has_detail": updated.has_detail,
            })))
        }
        "memory_v2_forget" => {
            let req: ForgetToolRequest = serde_json::from_value(args)?;
            store
                .forget(
                    user_id,
                    &req.memory_id,
                    req.reason.as_deref(),
                    "mcp_v2_forget",
                )
                .await?;
            Ok(mcp_json(&json!({
                "memory_id": req.memory_id,
                "forgotten": true,
            })))
        }
        "memory_v2_reflect" => {
            let req: ReflectToolRequest = serde_json::from_value(args)?;
            let mode = req.mode.trim();
            if !mode.is_empty() && mode != "auto" && mode != "candidates" && mode != "internal" {
                anyhow::bail!("mode must be 'auto', 'candidates', or 'internal'");
            }
            let reflected = store
                .reflect(
                    user_id,
                    ReflectV2Filter {
                        limit: req.limit,
                        mode: if mode.is_empty() {
                            DEFAULT_REFLECT_MODE.to_string()
                        } else {
                            mode.to_string()
                        },
                        session_id: req.session_id,
                        min_cluster_size: req.min_cluster_size,
                        min_link_strength: req.min_link_strength,
                    },
                )
                .await?;
            Ok(mcp_json(&json!({
                "mode": reflected.mode,
                "synthesized": reflected.synthesized,
                "scenes_created": reflected.scenes_created,
                "candidates": reflected
                    .candidates
                    .into_iter()
                    .map(|candidate| json!({
                        "signal": candidate.signal,
                        "importance": candidate.importance,
                        "memory_count": candidate.memory_count,
                        "session_count": candidate.session_count,
                        "link_count": candidate.link_count,
                        "memories": candidate
                            .memories
                            .into_iter()
                            .map(|memory| json!({
                                "id": memory.memory_id,
                                "abstract": memory.abstract_text,
                                "type": memory.memory_type.to_string(),
                                "session_id": memory.session_id,
                                "importance": memory.importance,
                            }))
                            .collect::<Vec<_>>(),
                    }))
                    .collect::<Vec<_>>(),
            })))
        }
        _ => Err(anyhow::anyhow!("Unknown V2 tool: {name}")),
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RememberToolRequest {
    content: String,
    #[serde(default = "default_memory_type", rename = "type")]
    memory_type: String,
    session_id: Option<String>,
    importance: Option<f64>,
    trust_tier: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
    source: Option<Value>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RecallTimeRangeToolRequest {
    #[serde(default)]
    start_at: Option<String>,
    #[serde(default)]
    end_at: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RecallToolRequest {
    query: String,
    #[serde(default = "default_top_k")]
    top_k: i64,
    #[serde(default = "default_max_tokens")]
    max_tokens: usize,
    #[serde(default = "default_scope")]
    scope: String,
    session_id: Option<String>,
    #[serde(rename = "type")]
    memory_type: Option<String>,
    #[serde(default)]
    expand_links: Option<bool>,
    #[serde(default = "default_recall_view")]
    view: String,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default = "default_tag_filter_mode")]
    tag_filter_mode: String,
    #[serde(default)]
    start_at: Option<String>,
    #[serde(default)]
    end_at: Option<String>,
    #[serde(default)]
    time_range: Option<RecallTimeRangeToolRequest>,
    #[serde(default)]
    min_confidence: Option<f64>,
    #[serde(default)]
    min_importance: Option<f64>,
    #[serde(default)]
    exclude_memory_ids: Vec<String>,
    #[serde(default)]
    prefer_recent: Option<bool>,
    #[serde(default)]
    diversity_factor: Option<f64>,
}

#[derive(Debug, Clone, Copy)]
enum RecallResponseMode {
    CompactAbstract,
    CompactOverview,
    Verbose,
}

impl RecallToolRequest {
    fn parse_optional_rfc3339(value: Option<&str>, field: &str) -> Result<Option<DateTime<Utc>>> {
        let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
            return Ok(None);
        };
        Ok(Some(
            chrono::DateTime::parse_from_rfc3339(value)
                .map_err(|e| anyhow::anyhow!("{field} must be RFC3339: {e}"))?
                .with_timezone(&Utc),
        ))
    }

    fn resolved_view(&self) -> Result<&str> {
        match self.view.trim() {
            "" | "compact" => Ok("compact"),
            "overview" => Ok("overview"),
            "full" => Ok("full"),
            _ => anyhow::bail!("view must be 'compact', 'overview', or 'full'"),
        }
    }

    fn resolved_with_overview(&self) -> Result<bool> {
        Ok(matches!(self.resolved_view()?, "overview" | "full"))
    }

    fn resolved_with_links(&self) -> Result<bool> {
        Ok(matches!(self.resolved_view()?, "full"))
    }

    fn resolved_response_mode(&self) -> Result<RecallResponseMode> {
        match self.resolved_view()? {
            "compact" => Ok(RecallResponseMode::CompactAbstract),
            "overview" => Ok(RecallResponseMode::CompactOverview),
            "full" => Ok(RecallResponseMode::Verbose),
            _ => unreachable!(),
        }
    }

    fn resolved_expand_links(&self) -> bool {
        self.expand_links.unwrap_or(true)
    }

    fn resolved_tag_filter_mode(&self) -> Result<String> {
        match self.tag_filter_mode.trim() {
            "" | "any" => Ok("any".to_string()),
            "all" => Ok("all".to_string()),
            _ => anyhow::bail!("tag_filter_mode must be 'any' or 'all'"),
        }
    }

    #[allow(clippy::type_complexity)]
    fn resolved_time_range(&self) -> Result<(Option<DateTime<Utc>>, Option<DateTime<Utc>>)> {
        let start_at = self.start_at.as_deref().or_else(|| {
            self.time_range
                .as_ref()
                .and_then(|time_range| time_range.start_at.as_deref())
        });
        let end_at = self.end_at.as_deref().or_else(|| {
            self.time_range
                .as_ref()
                .and_then(|time_range| time_range.end_at.as_deref())
        });
        let start_at = Self::parse_optional_rfc3339(start_at, "start_at")?;
        let end_at = Self::parse_optional_rfc3339(end_at, "end_at")?;
        if let (Some(start_at), Some(end_at)) = (start_at.as_ref(), end_at.as_ref()) {
            if start_at > end_at {
                anyhow::bail!("start_at must be less than or equal to end_at");
            }
        }
        Ok((start_at, end_at))
    }

    fn resolved_min_confidence(&self) -> Result<Option<f64>> {
        match self.min_confidence {
            Some(v) if !(0.0..=1.0).contains(&v) => {
                anyhow::bail!("min_confidence must be between 0.0 and 1.0")
            }
            other => Ok(other),
        }
    }

    fn resolved_min_importance(&self) -> Result<Option<f64>> {
        match self.min_importance {
            Some(v) if !(0.0..=1.0).contains(&v) => {
                anyhow::bail!("min_importance must be between 0.0 and 1.0")
            }
            other => Ok(other),
        }
    }

    fn resolved_diversity_factor(&self) -> Result<Option<f64>> {
        match self.diversity_factor {
            Some(v) if !(0.0..=1.0).contains(&v) => {
                anyhow::bail!("diversity_factor must be between 0.0 and 1.0")
            }
            other => Ok(other),
        }
    }

    fn resolved_exclude_memory_ids(&self) -> Vec<String> {
        self.exclude_memory_ids
            .iter()
            .map(|id| id.trim())
            .filter(|id| !id.is_empty())
            .map(ToOwned::to_owned)
            .collect()
    }
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct ListToolRequest {
    #[serde(default = "default_list_limit")]
    limit: i64,
    cursor: Option<String>,
    #[serde(rename = "type")]
    memory_type: Option<String>,
    session_id: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct ProfileToolRequest {
    #[serde(default = "default_list_limit")]
    limit: i64,
    cursor: Option<String>,
    session_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExpandToolRequest {
    memory_id: String,
    level: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FocusToolRequest {
    #[serde(rename = "type")]
    focus_type: String,
    value: String,
    boost: Option<f64>,
    #[serde(default)]
    ttl_secs: Option<i64>,
}

impl FocusToolRequest {
    fn resolved_focus_type(&self) -> Result<String> {
        match self.focus_type.trim() {
            "topic" | "tag" | "memory_id" | "session" => Ok(self.focus_type.trim().to_string()),
            _ => anyhow::bail!("focus type must be 'topic', 'tag', 'memory_id', or 'session'"),
        }
    }

    fn resolved_value(&self) -> Result<String> {
        let value = self.value.trim();
        if value.is_empty() {
            anyhow::bail!("focus value must not be empty");
        }
        Ok(value.to_string())
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct HistoryToolRequest {
    memory_id: String,
    #[serde(default = "default_list_limit")]
    limit: i64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdateToolRequest {
    memory_id: String,
    content: Option<String>,
    importance: Option<f64>,
    trust_tier: Option<String>,
    #[serde(default)]
    tags_add: Vec<String>,
    #[serde(default)]
    tags_remove: Vec<String>,
    reason: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ForgetToolRequest {
    memory_id: String,
    reason: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReflectToolRequest {
    #[serde(default = "default_reflect_mode")]
    mode: String,
    #[serde(default = "default_list_limit")]
    limit: i64,
    session_id: Option<String>,
    #[serde(default = "default_reflect_min_cluster_size")]
    min_cluster_size: i64,
    #[serde(default = "default_reflect_min_link_strength")]
    min_link_strength: f64,
}

fn default_memory_type() -> String {
    "semantic".to_string()
}

fn default_top_k() -> i64 {
    DEFAULT_TOP_K
}

fn default_max_tokens() -> usize {
    DEFAULT_MAX_TOKENS
}

fn default_scope() -> String {
    DEFAULT_SCOPE.to_string()
}

fn default_tag_filter_mode() -> String {
    DEFAULT_TAG_FILTER_MODE.to_string()
}

fn default_recall_view() -> String {
    DEFAULT_RECALL_VIEW.to_string()
}

fn default_list_limit() -> i64 {
    DEFAULT_LIST_LIMIT
}

fn default_reflect_mode() -> String {
    DEFAULT_REFLECT_MODE.to_string()
}

fn default_reflect_min_cluster_size() -> i64 {
    DEFAULT_REFLECT_MIN_CLUSTER_SIZE
}

fn default_reflect_min_link_strength() -> f64 {
    DEFAULT_REFLECT_MIN_LINK_STRENGTH
}

fn v2_store(service: &Arc<MemoryService>) -> Result<memoria_storage::MemoryV2Store> {
    Ok(service
        .sql_store
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("V2 MCP tools require SQL store"))?
        .v2_store())
}

fn parse_memory_type(raw: &str) -> Result<MemoryType> {
    MemoryType::from_str(raw).map_err(|e| anyhow::anyhow!(e.to_string()))
}

fn parse_optional_memory_type(raw: Option<&str>) -> Result<Option<MemoryType>> {
    match raw.map(str::trim).filter(|value| !value.is_empty()) {
        Some("all") | None => Ok(None),
        Some(value) => Ok(Some(parse_memory_type(value)?)),
    }
}

fn parse_optional_trust_tier(raw: Option<&str>) -> Result<Option<TrustTier>> {
    match raw.map(str::trim).filter(|value| !value.is_empty()) {
        Some(value) => Ok(Some(
            TrustTier::from_str(value).map_err(|e| anyhow::anyhow!(e.to_string()))?,
        )),
        None => Ok(None),
    }
}

fn parse_expand_level(level: &str) -> Result<ExpandLevel> {
    match level {
        "overview" => Ok(ExpandLevel::Overview),
        "detail" => Ok(ExpandLevel::Detail),
        "links" => Ok(ExpandLevel::Links),
        _ => anyhow::bail!("invalid expand level"),
    }
}

fn decode_cursor<T: DeserializeOwned>(encoded: &str) -> Result<T> {
    let bytes = URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|e| anyhow::anyhow!("invalid cursor: {e}"))?;
    serde_json::from_slice(&bytes).map_err(|e| anyhow::anyhow!("invalid cursor: {e}"))
}

fn encode_cursor<T: Serialize>(cursor: &T) -> String {
    URL_SAFE_NO_PAD.encode(serde_json::to_vec(cursor).unwrap_or_default())
}

fn normalize_history_payload(mut payload: Value) -> Value {
    if let Some(object) = payload.as_object_mut() {
        if let Some(value) = object.remove("memory_type") {
            object.entry("type".to_string()).or_insert(value);
        }
    }
    payload
}

fn map_list_item(item: memoria_storage::ListV2Item) -> Value {
    json!({
        "id": item.memory_id,
        "abstract": item.abstract_text,
        "type": item.memory_type.to_string(),
        "session_id": item.session_id,
        "created_at": item.created_at.to_rfc3339(),
        "has_overview": item.has_overview,
        "has_detail": item.has_detail,
    })
}

fn map_profile_item(item: memoria_storage::ProfileV2Item) -> Value {
    json!({
        "id": item.memory_id,
        "content": item.content,
        "abstract": item.abstract_text,
        "session_id": item.session_id,
        "created_at": item.created_at.to_rfc3339(),
        "updated_at": item.updated_at.to_rfc3339(),
        "trust_tier": item.trust_tier.to_string(),
        "confidence": item.confidence,
        "importance": item.importance,
        "has_overview": item.has_overview,
        "has_detail": item.has_detail,
    })
}

fn map_feedback_impact(impact: MemoryV2FeedbackImpact) -> Value {
    json!({
        "useful": impact.counts.useful,
        "irrelevant": impact.counts.irrelevant,
        "outdated": impact.counts.outdated,
        "wrong": impact.counts.wrong,
        "multiplier": impact.multiplier,
    })
}

fn map_link_provenance(provenance: MemoryV2LinkProvenance) -> Value {
    json!({
        "evidence_types": provenance.evidence_types,
        "primary_evidence_type": provenance.primary_evidence_type,
        "primary_evidence_strength": provenance.primary_evidence_strength,
        "refined": provenance.refined,
        "evidence": provenance
            .evidence
            .into_iter()
            .map(|detail| json!({
                "type": detail.evidence_type,
                "strength": detail.strength,
                "overlap_count": detail.overlap_count,
                "source_tag_count": detail.source_tag_count,
                "target_tag_count": detail.target_tag_count,
                "vector_distance": detail.vector_distance,
            }))
            .collect::<Vec<_>>(),
        "extraction_trace": provenance.extraction_trace.map(|trace| json!({
            "content_version_id": trace.content_version_id,
            "derivation_state": trace.derivation_state,
            "latest_job_status": trace.latest_job_status,
            "latest_job_attempts": trace.latest_job_attempts,
            "latest_job_updated_at": trace.latest_job_updated_at.map(|ts| ts.to_rfc3339()),
            "latest_job_error": trace.latest_job_error,
        })),
    })
}

fn map_link(link: memoria_storage::LinkV2Ref) -> Value {
    json!({
        "memory_id": link.memory_id,
        "abstract": link.abstract_text,
        "link_type": link.link_type,
        "strength": link.strength,
        "provenance": map_link_provenance(link.provenance),
    })
}

fn map_recall_summary(summary: memoria_storage::MemoryV2RecallSummary) -> Value {
    json!({
        "discovered_count": summary.discovered_count,
        "returned_count": summary.returned_count,
        "truncated": summary.truncated,
        "by_retrieval_path": summary
            .by_retrieval_path
            .into_iter()
            .map(|bucket| json!({
                "retrieval_path": bucket.retrieval_path.as_str(),
                "discovered_count": bucket.discovered_count,
                "returned_count": bucket.returned_count,
            }))
            .collect::<Vec<_>>(),
    })
}

fn map_recall_ranking(ranking: MemoryV2RecallRanking) -> Value {
    json!({
        "final_score": ranking.final_score,
        "base_score": ranking.base_score,
        "vector_component": ranking.vector_component,
        "keyword_component": ranking.keyword_component,
        "confidence_component": ranking.confidence_component,
        "importance_component": ranking.importance_component,
        "entity_component": ranking.entity_component,
        "link_bonus": ranking.link_bonus,
        "linked_expansion_applied": ranking.linked_expansion_applied,
        "temporal_decay_applied": ranking.temporal_decay_applied,
        "age_hours": ranking.age_hours,
        "temporal_half_life_hours": ranking.temporal_half_life_hours,
        "temporal_multiplier": ranking.temporal_multiplier,
        "session_affinity_applied": ranking.session_affinity_applied,
        "session_affinity_multiplier": ranking.session_affinity_multiplier,
        "access_count": ranking.access_count,
        "access_multiplier": ranking.access_multiplier,
        "feedback_multiplier": ranking.feedback_multiplier,
        "focus_boost": ranking.focus_boost,
        "type_affinity_boost": ranking.type_affinity_boost,
        "focus_matches": ranking
            .focus_matches
            .into_iter()
            .map(|focus| json!({
                "type": focus.focus_type,
                "value": focus.value,
                "boost": focus.boost,
            }))
            .collect::<Vec<_>>(),
        "expansion_sources": ranking
            .expansion_sources
            .into_iter()
            .map(|source| json!({
                "seed_memory_id": source.seed_memory_id,
                "seed_score": source.seed_score,
                "link_type": source.link_type,
                "link_strength": source.link_strength,
                "bonus": source.bonus,
            }))
            .collect::<Vec<_>>(),
    })
}

fn map_recall_item(item: MemoryV2RecallItem, mode: RecallResponseMode) -> Value {
    match mode {
        RecallResponseMode::CompactAbstract => json!({
            "id": item.memory_id,
            "text": item.abstract_text,
            "type": item.memory_type.to_string(),
            "score": item.score,
            "related": item.has_related,
        }),
        RecallResponseMode::CompactOverview => json!({
            "id": item.memory_id,
            "text": item.overview_text.unwrap_or(item.abstract_text),
            "type": item.memory_type.to_string(),
            "score": item.score,
            "related": item.has_related,
        }),
        RecallResponseMode::Verbose => json!({
            "id": item.memory_id,
            "abstract": item.abstract_text,
            "overview": item.overview_text,
            "score": item.score,
            "type": item.memory_type.to_string(),
            "confidence": item.confidence,
            "has_overview": item.has_overview,
            "has_detail": item.has_detail,
            "access_count": item.access_count,
            "link_count": item.link_count,
            "has_related": item.has_related,
            "retrieval_path": item.retrieval_path.as_str(),
            "feedback_impact": map_feedback_impact(item.feedback_impact),
            "ranking": map_recall_ranking(item.ranking),
            "links": item.links.map(|links| links.into_iter().map(map_link).collect::<Vec<_>>()),
        }),
    }
}

fn map_expand_result(expanded: MemoryV2ExpandResult) -> Value {
    json!({
        "memory_id": expanded.memory_id,
        "level": expanded.level.as_str(),
        "abstract": expanded.abstract_text,
        "overview": expanded.overview_text,
        "detail": expanded.detail_text,
        "links": expanded
            .links
            .map(|links| links.into_iter().map(map_link).collect::<Vec<_>>()),
    })
}

fn mcp_json(value: &Value) -> Value {
    mcp_text(&serde_json::to_string_pretty(value).unwrap_or_default())
}

fn mcp_text(text: &str) -> Value {
    json!({"content": [{"type": "text", "text": text}]})
}
