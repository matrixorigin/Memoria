/// Admin endpoints — system stats, user management, governance triggers.
/// All routes require master key auth (same Bearer token).

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};
use sqlx::MySqlPool;

use crate::{auth::AuthUser, state::AppState};

fn get_pool(state: &AppState) -> Result<&MySqlPool, (StatusCode, String)> {
    state.service.sql_store.as_ref()
        .map(|s| s.pool())
        .ok_or((StatusCode::INTERNAL_SERVER_ERROR, "No SQL store".into()))
}

fn db_err(e: impl std::fmt::Display) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

// ── Types ────────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct CursorParams {
    pub cursor: Option<String>,
    pub limit: Option<i64>,
}

#[derive(Serialize)]
pub struct SystemStats {
    pub total_users: i64,
    pub total_memories: i64,
    pub total_snapshots: i64,
}

#[derive(Serialize)]
pub struct UserEntry {
    pub user_id: String,
}

#[derive(Serialize)]
pub struct UserListResponse {
    pub users: Vec<UserEntry>,
    pub next_cursor: Option<String>,
}

#[derive(Serialize)]
pub struct UserStats {
    pub user_id: String,
    pub memory_count: i64,
    pub snapshot_count: i64,
}

#[derive(Deserialize)]
pub struct TriggerParams {
    pub op: Option<String>,
}

// ── Handlers ─────────────────────────────────────────────────────────────────

/// GET /admin/stats
pub async fn system_stats(
    _auth: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<SystemStats>, (StatusCode, String)> {
    let pool = get_pool(&state)?;

    let (total_users,): (i64,) = sqlx::query_as(
        "SELECT COUNT(DISTINCT user_id) FROM mem_memories WHERE is_active > 0"
    ).fetch_one(pool).await.map_err(db_err)?;

    let (total_memories,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM mem_memories WHERE is_active > 0"
    ).fetch_one(pool).await.map_err(db_err)?;

    let snapshots = state.git.list_snapshots().await.map_err(db_err)?;

    Ok(Json(SystemStats { total_users, total_memories, total_snapshots: snapshots.len() as i64 }))
}

/// GET /admin/users
pub async fn list_users(
    _auth: AuthUser,
    State(state): State<AppState>,
    Query(params): Query<CursorParams>,
) -> Result<Json<UserListResponse>, (StatusCode, String)> {
    let pool = get_pool(&state)?;
    let limit = params.limit.unwrap_or(100);

    let rows: Vec<(String,)> = if let Some(ref cursor) = params.cursor {
        sqlx::query_as(
            "SELECT DISTINCT user_id FROM mem_memories WHERE is_active > 0 AND user_id > ? ORDER BY user_id LIMIT ?"
        ).bind(cursor).bind(limit).fetch_all(pool).await
    } else {
        sqlx::query_as(
            "SELECT DISTINCT user_id FROM mem_memories WHERE is_active > 0 ORDER BY user_id LIMIT ?"
        ).bind(limit).fetch_all(pool).await
    }.map_err(db_err)?;

    let next_cursor = if rows.len() as i64 == limit { rows.last().map(|r| r.0.clone()) } else { None };

    Ok(Json(UserListResponse {
        users: rows.into_iter().map(|r| UserEntry { user_id: r.0 }).collect(),
        next_cursor,
    }))
}

/// GET /admin/users/:user_id/stats
pub async fn user_stats(
    _auth: AuthUser,
    State(state): State<AppState>,
    Path(user_id): Path<String>,
) -> Result<Json<UserStats>, (StatusCode, String)> {
    let pool = get_pool(&state)?;

    let (memory_count,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM mem_memories WHERE user_id = ? AND is_active > 0"
    ).bind(&user_id).fetch_one(pool).await.map_err(db_err)?;

    let snapshots = state.git.list_snapshots().await.map_err(db_err)?;

    Ok(Json(UserStats { user_id, memory_count, snapshot_count: snapshots.len() as i64 }))
}

/// DELETE /admin/users/:user_id
pub async fn delete_user(
    _auth: AuthUser,
    State(state): State<AppState>,
    Path(user_id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let pool = get_pool(&state)?;
    sqlx::query("UPDATE mem_memories SET is_active = 0 WHERE user_id = ?")
        .bind(&user_id).execute(pool).await.map_err(db_err)?;
    Ok(Json(serde_json::json!({"status": "ok", "user_id": user_id})))
}

/// POST /admin/users/:user_id/reset-access-counts
/// No-op in Rust version (access_count not yet in schema). Returns OK for API compat.
pub async fn reset_access_counts(
    _auth: AuthUser,
    Path(user_id): Path<String>,
) -> Json<serde_json::Value> {
    Json(serde_json::json!({"user_id": user_id, "status": "ok"}))
}

/// POST /admin/governance/:user_id/trigger?op=governance|consolidate
/// Skips cooldown checks (admin override).
pub async fn trigger_governance(
    _auth: AuthUser,
    State(state): State<AppState>,
    Path(user_id): Path<String>,
    Query(params): Query<TriggerParams>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let op = params.op.as_deref().unwrap_or("governance");
    let sql = state.service.sql_store.as_ref()
        .ok_or((StatusCode::SERVICE_UNAVAILABLE, "SQL store required".into()))?;

    match op {
        "governance" => {
            let quarantined = sql.quarantine_low_confidence(&user_id).await.map_err(db_err)?;
            let cleaned = sql.cleanup_stale(&user_id).await.map_err(db_err)?;
            Ok(Json(serde_json::json!({"op": op, "user_id": user_id, "quarantined": quarantined, "cleaned_stale": cleaned})))
        }
        "consolidate" => {
            let graph = sql.graph_store();
            let consolidator = memoria_storage::GraphConsolidator::new(&graph);
            let r = consolidator.consolidate(&user_id).await;
            Ok(Json(serde_json::json!({"op": op, "user_id": user_id, "conflicts_detected": r.conflicts_detected, "orphaned_scenes": r.orphaned_scenes})))
        }
        _ => Err((StatusCode::BAD_REQUEST, format!("Invalid op: {op}. Must be governance|consolidate"))),
    }
}

// ── Health endpoints (per-user, no admin required) ───────────────────────────

/// GET /v1/health/analyze — per-type stats
pub async fn health_analyze(
    AuthUser(user_id): AuthUser,
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let sql = state.service.sql_store.as_ref()
        .ok_or((StatusCode::SERVICE_UNAVAILABLE, "SQL store required".into()))?;
    let result = sql.health_analyze(&user_id).await.map_err(db_err)?;
    Ok(Json(result))
}

/// GET /v1/health/storage — storage stats
pub async fn health_storage(
    AuthUser(user_id): AuthUser,
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let sql = state.service.sql_store.as_ref()
        .ok_or((StatusCode::SERVICE_UNAVAILABLE, "SQL store required".into()))?;
    let result = sql.health_storage_stats(&user_id).await.map_err(db_err)?;
    Ok(Json(result))
}

/// GET /v1/health/capacity — IVF capacity estimate
pub async fn health_capacity(
    AuthUser(user_id): AuthUser,
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let sql = state.service.sql_store.as_ref()
        .ok_or((StatusCode::SERVICE_UNAVAILABLE, "SQL store required".into()))?;
    let result = sql.health_capacity(&user_id).await.map_err(db_err)?;
    Ok(Json(result))
}
