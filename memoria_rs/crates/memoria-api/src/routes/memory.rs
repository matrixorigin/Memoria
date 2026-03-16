use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use serde::Deserialize;

use crate::{auth::AuthUser, models::*, state::AppState};

type ApiResult<T> = Result<Json<T>, (StatusCode, String)>;
pub fn api_err(e: impl std::fmt::Display) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

#[derive(Deserialize, Default)]
pub struct ListQuery {
    pub memory_type: Option<String>,
    #[serde(default = "default_limit")]
    pub limit: i64,
    pub cursor: Option<String>,
}
fn default_limit() -> i64 { 100 }

pub async fn health() -> &'static str { "ok" }

pub async fn list_memories(
    State(state): State<AppState>,
    AuthUser(user_id): AuthUser,
    Query(q): Query<ListQuery>,
) -> ApiResult<ListResponse> {
    let limit = q.limit.min(500);
    let memories = state.service.list_active(&user_id, limit).await.map_err(api_err)?;
    let items: Vec<MemoryResponse> = memories.into_iter()
        .filter(|m| q.memory_type.as_deref().map(|t| m.memory_type.to_string() == t).unwrap_or(true))
        .map(Into::into)
        .collect();
    let next_cursor = if items.len() == limit as usize {
        items.last().map(|m| format!("{}|{}", m.observed_at.as_deref().unwrap_or(""), m.memory_id))
    } else { None };
    Ok(Json(ListResponse { items, next_cursor }))
}

pub async fn store_memory(
    State(state): State<AppState>,
    AuthUser(user_id): AuthUser,
    Json(req): Json<StoreRequest>,
) -> Result<(StatusCode, Json<MemoryResponse>), (StatusCode, String)> {
    let mt = parse_memory_type(&req.memory_type).map_err(|e| (StatusCode::UNPROCESSABLE_ENTITY, e))?;
    let tier = req.trust_tier.as_deref()
        .map(parse_trust_tier).transpose()
        .map_err(|e| (StatusCode::UNPROCESSABLE_ENTITY, e))?;
    let observed_at = req.observed_at.as_deref()
        .map(|s| chrono::DateTime::parse_from_rfc3339(s).map(|dt| dt.with_timezone(&chrono::Utc)))
        .transpose().map_err(|e| (StatusCode::UNPROCESSABLE_ENTITY, e.to_string()))?;
    let m = state.service.store_memory(&user_id, &req.content, mt, req.session_id, tier, observed_at, req.initial_confidence)
        .await.map_err(api_err)?;
    Ok((StatusCode::CREATED, Json(m.into())))
}

pub async fn batch_store(
    State(state): State<AppState>,
    AuthUser(user_id): AuthUser,
    Json(req): Json<BatchStoreRequest>,
) -> Result<(StatusCode, Json<Vec<MemoryResponse>>), (StatusCode, String)> {
    let mut results = Vec::new();
    for r in req.memories {
        let mt = parse_memory_type(&r.memory_type).map_err(|e| (StatusCode::UNPROCESSABLE_ENTITY, e))?;
        let tier = r.trust_tier.as_deref()
            .map(parse_trust_tier).transpose()
            .map_err(|e| (StatusCode::UNPROCESSABLE_ENTITY, e))?;
        let m = state.service.store_memory(&user_id, &r.content, mt, r.session_id, tier, None, None)
            .await.map_err(api_err)?;
        results.push(m.into());
    }
    Ok((StatusCode::CREATED, Json(results)))
}

pub async fn retrieve(
    State(state): State<AppState>,
    AuthUser(user_id): AuthUser,
    Json(req): Json<RetrieveRequest>,
) -> ApiResult<serde_json::Value> {
    let level = memoria_service::ExplainLevel::from_str_or_bool(&req.explain);
    if level != memoria_service::ExplainLevel::None {
        let (results, explain) = state.service.retrieve_explain_level(&user_id, &req.query, req.top_k, level).await.map_err(api_err)?;
        let items: Vec<MemoryResponse> = results.into_iter().map(Into::into).collect();
        Ok(Json(serde_json::json!({"results": items, "explain": explain})))
    } else {
        let results = state.service.retrieve(&user_id, &req.query, req.top_k).await.map_err(api_err)?;
        Ok(Json(serde_json::json!(results.into_iter().map(Into::into).collect::<Vec<MemoryResponse>>())))
    }
}

pub async fn search(
    State(state): State<AppState>,
    AuthUser(user_id): AuthUser,
    Json(req): Json<RetrieveRequest>,
) -> ApiResult<serde_json::Value> {
    let level = memoria_service::ExplainLevel::from_str_or_bool(&req.explain);
    if level != memoria_service::ExplainLevel::None {
        let (results, explain) = state.service.search_explain_level(&user_id, &req.query, req.top_k, level).await.map_err(api_err)?;
        let items: Vec<MemoryResponse> = results.into_iter().map(Into::into).collect();
        Ok(Json(serde_json::json!({"results": items, "explain": explain})))
    } else {
        let results = state.service.search(&user_id, &req.query, req.top_k).await.map_err(api_err)?;
        Ok(Json(serde_json::json!(results.into_iter().map(Into::into).collect::<Vec<MemoryResponse>>())))
    }
}

pub async fn get_memory(
    State(state): State<AppState>,
    AuthUser(_): AuthUser,
    Path(id): Path<String>,
) -> ApiResult<Option<MemoryResponse>> {
    let m = state.service.get(&id).await.map_err(api_err)?;
    Ok(Json(m.map(Into::into)))
}

pub async fn correct_memory(
    State(state): State<AppState>,
    AuthUser(_): AuthUser,
    Path(id): Path<String>,
    Json(req): Json<CorrectRequest>,
) -> ApiResult<MemoryResponse> {
    let m = state.service.correct(&id, &req.new_content).await.map_err(api_err)?;
    Ok(Json(m.into()))
}

pub async fn correct_by_query(
    State(state): State<AppState>,
    AuthUser(user_id): AuthUser,
    Json(req): Json<CorrectByQueryRequest>,
) -> ApiResult<MemoryResponse> {
    let results = state.service.retrieve(&user_id, &req.query, 1).await.map_err(api_err)?;
    let found = results.into_iter().next()
        .ok_or_else(|| (StatusCode::NOT_FOUND, "No matching memory found".to_string()))?;
    let m = state.service.correct(&found.memory_id, &req.new_content).await.map_err(api_err)?;
    Ok(Json(m.into()))
}

pub async fn delete_memory(
    State(state): State<AppState>,
    AuthUser(_): AuthUser,
    Path(id): Path<String>,
) -> Result<StatusCode, (StatusCode, String)> {
    state.service.purge(&id).await.map_err(api_err)?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn purge_memories(
    State(state): State<AppState>,
    AuthUser(user_id): AuthUser,
    Json(req): Json<PurgeRequest>,
) -> ApiResult<PurgeResponse> {
    let mut purged = 0usize;
    if let Some(ids) = &req.memory_ids {
        for id in ids {
            state.service.purge(id).await.map_err(api_err)?;
            purged += 1;
        }
    } else if let Some(topic) = &req.topic {
        let results = state.service.retrieve(&user_id, topic, 100).await.map_err(api_err)?;
        for m in &results {
            state.service.purge(&m.memory_id).await.map_err(api_err)?;
            purged += 1;
        }
    }
    Ok(Json(PurgeResponse { purged }))
}

pub async fn get_profile(
    State(state): State<AppState>,
    AuthUser(user_id): AuthUser,
) -> ApiResult<serde_json::Value> {
    let memories = state.service.list_active(&user_id, 50).await.map_err(api_err)?;
    let profile: Vec<_> = memories.iter()
        .filter(|m| m.memory_type == memoria_core::MemoryType::Profile)
        .map(|m| m.content.as_str())
        .collect();
    Ok(Json(serde_json::json!({ "profile": profile.join("\n") })))
}

#[derive(serde::Deserialize)]
pub struct ObserveRequest {
    pub messages: Vec<serde_json::Value>,
    pub source_event_ids: Option<Vec<String>>,
    pub session_id: Option<String>,
}

/// Extract and store memories from a conversation turn.
/// Without LLM: stores each non-empty assistant message as a semantic memory.
/// With LLM: (future) extract facts from the conversation.
pub async fn observe_turn(
    State(state): State<AppState>,
    AuthUser(user_id): AuthUser,
    Json(req): Json<ObserveRequest>,
) -> ApiResult<serde_json::Value> {
    let mut stored = Vec::new();
    let mut warning: Option<&str> = None;

    if state.service.llm.is_none() {
        warning = Some("LLM not configured — storing assistant messages as-is");
    }

    for msg in &req.messages {
        let role = msg["role"].as_str().unwrap_or("");
        let content = msg["content"].as_str().unwrap_or("").trim();
        if content.is_empty() { continue; }
        // Store assistant messages as semantic memories
        if role == "assistant" || role == "user" {
            let m = state.service.store_memory(
                &user_id, content,
                memoria_core::MemoryType::Semantic,
                req.session_id.clone(), None, None, None,
            ).await.map_err(api_err)?;
            stored.push(serde_json::json!({
                "memory_id": m.memory_id,
                "content": m.content,
                "memory_type": m.memory_type.to_string(),
            }));
        }
    }

    let mut result = serde_json::json!({ "memories": stored });
    if let Some(w) = warning {
        result["warning"] = serde_json::json!(w);
    }
    Ok(Json(result))
}

/// GET /v1/memories/:id/history — version chain via superseded_by links.
pub async fn get_memory_history(
    State(state): State<AppState>,
    AuthUser(user_id): AuthUser,
    Path(id): Path<String>,
) -> ApiResult<serde_json::Value> {
    use sqlx::Row;

    let sql = state.service.sql_store.as_ref()
        .ok_or_else(|| (StatusCode::SERVICE_UNAVAILABLE, "SQL store required".to_string()))?;
    let table = sql.active_table(&user_id).await.map_err(api_err)?;

    let mut chain = Vec::new();
    let mut visited = std::collections::HashSet::new();

    // Walk forward from the given id following superseded_by
    let mut current_id = Some(id.clone());
    while let Some(cid) = current_id {
        if !visited.insert(cid.clone()) { break; }
        let row = sqlx::query(
            &format!(
                "SELECT memory_id, content, is_active, superseded_by, observed_at, memory_type \
                 FROM `{}` WHERE memory_id = ? AND user_id = ?", table
            )
        )
        .bind(&cid).bind(&user_id)
        .fetch_optional(sql.pool()).await.map_err(api_err)?;

        match row {
            Some(r) => {
                let mid: String = r.try_get("memory_id").unwrap_or_default();
                let sup: Option<String> = r.try_get("superseded_by").ok().flatten();
                chain.push(serde_json::json!({
                    "memory_id": mid,
                    "content": r.try_get::<String, _>("content").unwrap_or_default(),
                    "is_active": r.try_get::<i8, _>("is_active").unwrap_or(0) != 0,
                    "superseded_by": sup,
                    "observed_at": r.try_get::<Option<String>, _>("observed_at").ok().flatten(),
                    "memory_type": r.try_get::<String, _>("memory_type").unwrap_or_default(),
                }));
                current_id = sup;
            }
            None => {
                if chain.is_empty() {
                    return Err((StatusCode::NOT_FOUND, "Memory not found".to_string()));
                }
                break;
            }
        }
    }

    // Walk backwards: find older versions that point to our root
    if let Some(root_id) = chain.first().and_then(|v| v["memory_id"].as_str()) {
        let mut prev_id = root_id.to_string();
        loop {
            let older = sqlx::query(
                &format!(
                    "SELECT memory_id, content, is_active, superseded_by, observed_at, memory_type \
                     FROM `{}` WHERE superseded_by = ? AND user_id = ?", table
                )
            )
            .bind(&prev_id).bind(&user_id)
            .fetch_optional(sql.pool()).await.map_err(api_err)?;

            match older {
                Some(r) => {
                    let mid: String = r.try_get("memory_id").unwrap_or_default();
                    if !visited.insert(mid.clone()) { break; }
                    prev_id = mid.clone();
                    chain.insert(0, serde_json::json!({
                        "memory_id": mid,
                        "content": r.try_get::<String, _>("content").unwrap_or_default(),
                        "is_active": r.try_get::<i8, _>("is_active").unwrap_or(0) != 0,
                        "superseded_by": r.try_get::<Option<String>, _>("superseded_by").ok().flatten(),
                        "observed_at": r.try_get::<Option<String>, _>("observed_at").ok().flatten(),
                        "memory_type": r.try_get::<String, _>("memory_type").unwrap_or_default(),
                    }));
                }
                None => break,
            }
        }
    }

    Ok(Json(serde_json::json!({
        "memory_id": id,
        "versions": chain,
        "total": chain.len(),
    })))
}

// ── Pipeline ──────────────────────────────────────────────────────────────────

#[derive(serde::Deserialize)]
pub struct PipelineRequest {
    pub candidates: Vec<PipelineCandidate>,
    pub sandbox_query: Option<String>,
}

#[derive(serde::Deserialize)]
pub struct PipelineCandidate {
    pub content: String,
    pub memory_type: Option<String>,
    pub trust_tier: Option<String>,
}

pub async fn run_pipeline(
    State(state): State<AppState>,
    AuthUser(user_id): AuthUser,
    Json(req): Json<PipelineRequest>,
) -> ApiResult<serde_json::Value> {
    use memoria_service::MemoryPipeline;
    use crate::models::{parse_memory_type, parse_trust_tier};
    use memoria_core::{MemoryType, TrustTier};

    let candidates = req.candidates.into_iter().map(|c| {
        let mt = c.memory_type.as_deref()
            .map(|s| parse_memory_type(s).unwrap_or(MemoryType::Semantic))
            .unwrap_or(MemoryType::Semantic);
        let tier = c.trust_tier.as_deref()
            .map(parse_trust_tier).transpose().ok().flatten();
        (c.content, mt, tier)
    }).collect();

    let pipeline = MemoryPipeline::new(state.service.clone(), Some(state.git.clone()));
    let result = pipeline.run(&user_id, candidates, req.sandbox_query.as_deref()).await;

    Ok(Json(serde_json::json!({
        "memories_stored": result.memories_stored,
        "memories_rejected": result.memories_rejected,
        "memories_redacted": result.memories_redacted,
        "errors": result.errors,
    })))
}
