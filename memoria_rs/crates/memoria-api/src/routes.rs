use crate::state::AppState;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use memoria_core::{MemoryType, TrustTier};
use serde::{Deserialize, Serialize};
use std::str::FromStr;

// ── Request / Response types ──────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct StoreRequest {
    pub user_id: String,
    pub content: String,
    #[serde(default = "default_memory_type")]
    pub memory_type: String,
    pub session_id: Option<String>,
    pub trust_tier: Option<String>,
}

fn default_memory_type() -> String { "semantic".to_string() }

#[derive(Deserialize)]
pub struct RetrieveRequest {
    pub user_id: String,
    pub query: String,
    #[serde(default = "default_top_k")]
    pub top_k: i64,
}

fn default_top_k() -> i64 { 5 }

#[derive(Deserialize)]
pub struct CorrectRequest {
    pub new_content: String,
}

#[derive(Serialize)]
pub struct MemoryResponse {
    pub memory_id: String,
    pub user_id: String,
    pub memory_type: String,
    pub content: String,
    pub trust_tier: String,
    pub is_active: bool,
    pub session_id: Option<String>,
}

impl From<memoria_core::Memory> for MemoryResponse {
    fn from(m: memoria_core::Memory) -> Self {
        Self {
            memory_id: m.memory_id,
            user_id: m.user_id,
            memory_type: m.memory_type.to_string(),
            content: m.content,
            trust_tier: m.trust_tier.to_string(),
            is_active: m.is_active,
            session_id: m.session_id,
        }
    }
}

type ApiResult<T> = Result<Json<T>, (StatusCode, String)>;

fn api_err(e: impl std::fmt::Display) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

// ── Handlers ──────────────────────────────────────────────────────────────────

pub async fn health() -> &'static str { "ok" }

pub async fn store_memory(
    State(state): State<AppState>,
    Json(req): Json<StoreRequest>,
) -> ApiResult<MemoryResponse> {
    let memory_type = MemoryType::from_str(&req.memory_type).map_err(api_err)?;
    let trust_tier = req.trust_tier.as_deref()
        .map(TrustTier::from_str).transpose().map_err(api_err)?;
    let m = state.service
        .store_memory(&req.user_id, &req.content, memory_type, req.session_id, trust_tier)
        .await.map_err(api_err)?;
    Ok(Json(m.into()))
}

pub async fn retrieve(
    State(state): State<AppState>,
    Json(req): Json<RetrieveRequest>,
) -> ApiResult<Vec<MemoryResponse>> {
    let results = state.service
        .retrieve(&req.user_id, &req.query, req.top_k)
        .await.map_err(api_err)?;
    Ok(Json(results.into_iter().map(Into::into).collect()))
}

pub async fn search(
    State(state): State<AppState>,
    Json(req): Json<RetrieveRequest>,
) -> ApiResult<Vec<MemoryResponse>> {
    let results = state.service
        .search(&req.user_id, &req.query, req.top_k)
        .await.map_err(api_err)?;
    Ok(Json(results.into_iter().map(Into::into).collect()))
}

pub async fn get_memory(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Option<MemoryResponse>> {
    let m = state.service.get(&id).await.map_err(api_err)?;
    Ok(Json(m.map(Into::into)))
}

pub async fn correct_memory(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<CorrectRequest>,
) -> ApiResult<MemoryResponse> {
    let m = state.service.correct(&id, &req.new_content).await.map_err(api_err)?;
    Ok(Json(m.into()))
}

pub async fn purge_memory(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, (StatusCode, String)> {
    state.service.purge(&id).await.map_err(api_err)?;
    Ok(StatusCode::NO_CONTENT)
}
