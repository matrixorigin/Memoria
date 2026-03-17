//! Admin endpoints — system stats, user management, governance triggers.
//! All routes require master key auth (same Bearer token).

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};
use sqlx::{MySqlPool, Row};

use crate::{auth::AuthUser, state::AppState, routes::memory::api_err};

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
pub async fn reset_access_counts(
    State(state): State<AppState>,
    _auth: AuthUser,
    Path(user_id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let sql = state.service.sql_store.as_ref()
        .ok_or_else(|| (StatusCode::SERVICE_UNAVAILABLE, "SQL store required".to_string()))?;
    let reset = sql.reset_access_counts(&user_id).await.map_err(api_err)?;
    Ok(Json(serde_json::json!({"user_id": user_id, "reset": reset})))
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
            let cleaned_stale = sql.cleanup_stale(&user_id).await.map_err(db_err)?;
            let cleaned_tool_results = sql.cleanup_tool_results(72).await.map_err(db_err)?;
            let archived_working = sql.archive_stale_working(24).await.map_err(db_err)?;
            let compressed = sql.compress_redundant(&user_id, 0.95, 30, 10_000).await.map_err(db_err)?;
            let cleaned_incrementals = sql.cleanup_orphaned_incrementals(&user_id, 24).await.map_err(db_err)?;
            let pollution_detected = sql.detect_pollution(&user_id, 24).await.map_err(db_err)?;
            Ok(Json(serde_json::json!({
                "op": op, "user_id": user_id,
                "quarantined": quarantined,
                "cleaned_stale": cleaned_stale,
                "cleaned_tool_results": cleaned_tool_results,
                "archived_working": archived_working,
                "compressed_redundant": compressed,
                "cleaned_incrementals": cleaned_incrementals,
                "pollution_detected": pollution_detected,
            })))
        }
        "consolidate" => {
            let graph = sql.graph_store();
            let consolidator = memoria_storage::GraphConsolidator::new(&graph);
            let r = consolidator.consolidate(&user_id).await;
            Ok(Json(serde_json::json!({"op": op, "user_id": user_id, "conflicts_detected": r.conflicts_detected, "orphaned_scenes": r.orphaned_scenes})))
        }
        "extract_entities" => {
            let r = memoria_storage::graph::backfill::backfill_graph(sql, &user_id).await.map_err(db_err)?;
            Ok(Json(serde_json::json!({
                "op": op, "user_id": user_id,
                "processed": r.processed, "skipped": r.skipped,
                "edges_created": r.edges_created, "entities_linked": r.entities_linked,
            })))
        }
        "weekly" => {
            let cleaned_snapshots = sql.cleanup_snapshots(5).await.map_err(db_err)?;
            let cleaned_branches = sql.cleanup_orphan_branches().await.map_err(db_err)?;
            let _ = sql.rebuild_vector_index("mem_memories").await;
            Ok(Json(serde_json::json!({"op": op, "user_id": user_id, "cleaned_snapshots": cleaned_snapshots, "cleaned_branches": cleaned_branches})))
        }
        _ => Err((StatusCode::BAD_REQUEST, format!("Invalid op: {op}. Must be governance|consolidate|extract_entities|weekly"))),
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

/// POST /admin/users/:id/strategy?strategy=... — set retrieval strategy (no-op stub for benchmark compat)
pub async fn set_user_strategy(
    Path(user_id): Path<String>,
    Query(params): Query<std::collections::HashMap<String, String>>,
    _state: State<AppState>,
) -> Json<serde_json::Value> {
    let strategy = params.get("strategy").cloned().unwrap_or_else(|| "vector:v1".to_string());
    Json(serde_json::json!({
        "user_id": user_id,
        "strategy": strategy,
        "previous": "vector:v1",
        "status": "ok",
    }))
}

/// GET /admin/users/:user_id/keys — list all active API keys for a user (admin only)
pub async fn list_user_keys(
    _auth: AuthUser,
    State(state): State<AppState>,
    Path(user_id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let pool = get_pool(&state)?;
    let rows = sqlx::query(
        "SELECT key_id, name, key_prefix, created_at, expires_at, last_used_at \
         FROM mem_api_keys WHERE user_id = ? AND is_active = 1 ORDER BY created_at DESC"
    ).bind(&user_id).fetch_all(pool).await.map_err(db_err)?;

    let keys: Vec<serde_json::Value> = rows.iter().map(|r| {
        serde_json::json!({
            "key_id": r.try_get::<String, _>("key_id").unwrap_or_default(),
            "name": r.try_get::<String, _>("name").unwrap_or_default(),
            "key_prefix": r.try_get::<String, _>("key_prefix").unwrap_or_default(),
            "created_at": r.try_get::<chrono::NaiveDateTime, _>("created_at").map(|d| d.to_string()).unwrap_or_default(),
            "expires_at": r.try_get::<Option<chrono::NaiveDateTime>, _>("expires_at").ok().flatten().map(|d| d.to_string()),
            "last_used_at": r.try_get::<Option<chrono::NaiveDateTime>, _>("last_used_at").ok().flatten().map(|d| d.to_string()),
        })
    }).collect();

    Ok(Json(serde_json::json!({"user_id": user_id, "keys": keys})))
}

/// DELETE /admin/users/:user_id/keys — revoke all active API keys for a user (admin only)
pub async fn revoke_all_user_keys(
    _auth: AuthUser,
    State(state): State<AppState>,
    Path(user_id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let pool = get_pool(&state)?;
    let result = sqlx::query("UPDATE mem_api_keys SET is_active = 0 WHERE user_id = ? AND is_active = 1")
        .bind(&user_id).execute(pool).await.map_err(db_err)?;
    Ok(Json(serde_json::json!({"user_id": user_id, "revoked": result.rows_affected()})))
}

/// POST /admin/users/:user_id/params — set per-user activation param overrides
pub async fn set_user_params(
    _auth: AuthUser,
    State(state): State<AppState>,
    Path(user_id): Path<String>,
    Json(params): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let pool = get_pool(&state)?;
    let pj = serde_json::to_string(&params).map_err(db_err)?;
    sqlx::query(
        "UPDATE mem_user_memory_config SET params_json = ?, updated_at = NOW() WHERE user_id = ?"
    ).bind(&pj).bind(&user_id).execute(pool).await.map_err(db_err)?;
    Ok(Json(serde_json::json!({"user_id": user_id, "params": params})))
}
