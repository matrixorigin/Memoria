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
    let m = state.service.store_memory(&user_id, &req.content, mt, req.session_id, tier)
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
        let m = state.service.store_memory(&user_id, &r.content, mt, r.session_id, tier)
            .await.map_err(api_err)?;
        results.push(m.into());
    }
    Ok((StatusCode::CREATED, Json(results)))
}

pub async fn retrieve(
    State(state): State<AppState>,
    AuthUser(user_id): AuthUser,
    Json(req): Json<RetrieveRequest>,
) -> ApiResult<Vec<MemoryResponse>> {
    let results = state.service.retrieve(&user_id, &req.query, req.top_k).await.map_err(api_err)?;
    Ok(Json(results.into_iter().map(Into::into).collect()))
}

pub async fn search(
    State(state): State<AppState>,
    AuthUser(user_id): AuthUser,
    Json(req): Json<RetrieveRequest>,
) -> ApiResult<Vec<MemoryResponse>> {
    let results = state.service.search(&user_id, &req.query, req.top_k).await.map_err(api_err)?;
    Ok(Json(results.into_iter().map(Into::into).collect()))
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
