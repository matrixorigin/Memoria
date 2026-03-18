use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use serde::Deserialize;
use serde_json::json;
use sqlx::Row;

use crate::{auth::AuthUser, models::*, routes::memory::api_err, state::AppState};

#[derive(Deserialize, Default)]
pub struct ListSnapshotsQuery {
    #[serde(default = "default_snap_limit")]
    pub limit: i64,
    #[serde(default)]
    pub offset: i64,
}
fn default_snap_limit() -> i64 {
    20
}

#[derive(Deserialize, Default)]
pub struct GetSnapshotQuery {
    pub limit: Option<i64>,
    pub offset: Option<i64>,
    pub detail: Option<String>,
}

#[derive(Deserialize, Default)]
pub struct DiffSnapshotQuery {
    pub limit: Option<i64>,
}

/// Delegate to git_tools::call for snapshot/branch operations.
async fn git_call(
    state: &AppState,
    user_id: &str,
    tool: &str,
    args: serde_json::Value,
) -> Result<serde_json::Value, (StatusCode, String)> {
    let result = memoria_mcp::git_tools::call(tool, args, &state.git, &state.service, user_id)
        .await
        .map_err(api_err)?;
    // Extract text from MCP response
    let text = result["content"][0]["text"]
        .as_str()
        .unwrap_or("")
        .to_string();
    Ok(json!({ "result": text }))
}

pub async fn create_snapshot(
    State(state): State<AppState>,
    AuthUser { user_id, .. }: AuthUser,
    Json(req): Json<CreateSnapshotRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), (StatusCode, String)> {
    let r = git_call(
        &state,
        &user_id,
        "memory_snapshot",
        json!({ "name": req.name }),
    )
    .await?;
    Ok((StatusCode::CREATED, Json(r)))
}

pub async fn list_snapshots(
    State(state): State<AppState>,
    AuthUser { user_id, .. }: AuthUser,
    Query(q): Query<ListSnapshotsQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let r = git_call(
        &state,
        &user_id,
        "memory_snapshots",
        json!({ "limit": q.limit, "offset": q.offset }),
    )
    .await?;
    Ok(Json(r))
}

/// GET /v1/snapshots/:name — read snapshot detail with time-travel query
pub async fn get_snapshot(
    State(state): State<AppState>,
    AuthUser { user_id, .. }: AuthUser,
    Path(name): Path<String>,
    Query(q): Query<GetSnapshotQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let pool = state
        .service
        .sql_store
        .as_ref()
        .map(|s| s.pool())
        .ok_or((StatusCode::SERVICE_UNAVAILABLE, "SQL store required".into()))?;

    let snap_name = format!("mem_snap_{name}");
    let limit = q.limit.unwrap_or(50).min(500);
    let offset = q.offset.unwrap_or(0);
    let detail = q.detail.as_deref().unwrap_or("brief");

    // Total count via time-travel
    let count_sql = format!(
        "SELECT COUNT(*) as cnt FROM mem_memories {{SNAPSHOT = '{snap_name}'}} WHERE user_id = ? AND is_active > 0"
    );
    let total: i64 = sqlx::query_scalar(&count_sql)
        .bind(&user_id)
        .fetch_one(pool)
        .await
        .map_err(api_err)?;

    // Type distribution
    let type_sql = format!(
        "SELECT memory_type, COUNT(*) as cnt FROM mem_memories {{SNAPSHOT = '{snap_name}'}} \
         WHERE user_id = ? AND is_active > 0 GROUP BY memory_type"
    );
    let type_rows = sqlx::query(&type_sql)
        .bind(&user_id)
        .fetch_all(pool)
        .await
        .map_err(api_err)?;
    let by_type: serde_json::Map<String, serde_json::Value> = type_rows
        .iter()
        .map(|r| {
            let t: String = r.try_get("memory_type").unwrap_or_default();
            let c: i64 = r.try_get("cnt").unwrap_or(0);
            (t, json!(c))
        })
        .collect();

    // Paginated memories
    let content_limit: usize = match detail {
        "full" => 2000,
        "normal" => 200,
        _ => 80,
    };
    let mem_sql = format!(
        "SELECT memory_id, content, memory_type, initial_confidence FROM mem_memories {{SNAPSHOT = '{snap_name}'}} \
         WHERE user_id = ? AND is_active > 0 ORDER BY observed_at DESC LIMIT ? OFFSET ?"
    );
    let rows = sqlx::query(&mem_sql)
        .bind(&user_id)
        .bind(limit)
        .bind(offset)
        .fetch_all(pool)
        .await
        .map_err(api_err)?;

    let memories: Vec<serde_json::Value> = rows
        .iter()
        .map(|r| {
            let content: String = r.try_get("content").unwrap_or_default();
            let truncated = if content.len() > content_limit {
                format!("{} [truncated]", &content[..content_limit])
            } else {
                content
            };
            let mut m = json!({
                "memory_id": r.try_get::<String, _>("memory_id").unwrap_or_default(),
                "memory_type": r.try_get::<String, _>("memory_type").unwrap_or_default(),
                "content": truncated,
            });
            if detail == "full" {
                m["confidence"] = json!(r.try_get::<f64, _>("initial_confidence").unwrap_or(0.0));
            }
            m
        })
        .collect();

    Ok(Json(json!({
        "name": name,
        "snapshot_name": snap_name,
        "memory_count": total,
        "by_type": by_type,
        "memories": memories,
        "limit": limit,
        "offset": offset,
        "has_more": offset + limit < total,
    })))
}

/// GET /v1/snapshots/:name/diff — compare snapshot vs current state
pub async fn diff_snapshot(
    State(state): State<AppState>,
    AuthUser { user_id, .. }: AuthUser,
    Path(name): Path<String>,
    Query(q): Query<DiffSnapshotQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let pool = state
        .service
        .sql_store
        .as_ref()
        .map(|s| s.pool())
        .ok_or((StatusCode::SERVICE_UNAVAILABLE, "SQL store required".into()))?;

    let snap_name = format!("mem_snap_{name}");
    let limit = q.limit.unwrap_or(50).min(200);

    // Counts
    let snap_count: i64 = sqlx::query_scalar(&format!(
        "SELECT COUNT(*) FROM mem_memories {{SNAPSHOT = '{snap_name}'}} WHERE user_id = ? AND is_active > 0"
    )).bind(&user_id).fetch_one(pool).await.map_err(api_err)?;

    let curr_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM mem_memories WHERE user_id = ? AND is_active > 0")
            .bind(&user_id)
            .fetch_one(pool)
            .await
            .map_err(api_err)?;

    // Added (in current but not in snapshot)
    let added_sql = format!(
        "SELECT c.memory_id, c.content, c.memory_type FROM mem_memories c \
         LEFT JOIN mem_memories {{SNAPSHOT = '{snap_name}'}} s ON c.memory_id = s.memory_id AND s.is_active > 0 \
         WHERE c.user_id = ? AND c.is_active > 0 AND s.memory_id IS NULL LIMIT ?"
    );
    let added_rows = sqlx::query(&added_sql)
        .bind(&user_id)
        .bind(limit)
        .fetch_all(pool)
        .await
        .map_err(api_err)?;

    // Removed (in snapshot but not in current)
    let removed_sql = format!(
        "SELECT s.memory_id, s.content, s.memory_type FROM mem_memories {{SNAPSHOT = '{snap_name}'}} s \
         LEFT JOIN mem_memories c ON s.memory_id = c.memory_id AND c.is_active > 0 \
         WHERE s.user_id = ? AND s.is_active > 0 AND c.memory_id IS NULL LIMIT ?"
    );
    let removed_rows = sqlx::query(&removed_sql)
        .bind(&user_id)
        .bind(limit)
        .fetch_all(pool)
        .await
        .map_err(api_err)?;

    let to_json = |rows: &[sqlx::mysql::MySqlRow]| -> Vec<serde_json::Value> {
        rows.iter()
            .map(|r| {
                json!({
                    "memory_id": r.try_get::<String, _>("memory_id").unwrap_or_default(),
                    "content": r.try_get::<String, _>("content").unwrap_or_default(),
                    "memory_type": r.try_get::<String, _>("memory_type").unwrap_or_default(),
                })
            })
            .collect()
    };

    Ok(Json(json!({
        "snapshot_name": snap_name,
        "snapshot_count": snap_count,
        "current_count": curr_count,
        "added": to_json(&added_rows),
        "removed": to_json(&removed_rows),
    })))
}

pub async fn delete_snapshot(
    State(state): State<AppState>,
    AuthUser { user_id, .. }: AuthUser,
    Path(name): Path<String>,
) -> Result<StatusCode, (StatusCode, String)> {
    git_call(
        &state,
        &user_id,
        "memory_snapshot_delete",
        json!({ "names": name }),
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn delete_snapshot_bulk(
    State(state): State<AppState>,
    AuthUser { user_id, .. }: AuthUser,
    Json(req): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let r = git_call(&state, &user_id, "memory_snapshot_delete", req).await?;
    Ok(Json(r))
}

pub async fn rollback(
    State(state): State<AppState>,
    AuthUser { user_id, .. }: AuthUser,
    Path(name): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let r = git_call(&state, &user_id, "memory_rollback", json!({ "name": name })).await?;
    Ok(Json(r))
}

pub async fn list_branches(
    State(state): State<AppState>,
    AuthUser { user_id, .. }: AuthUser,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let r = git_call(&state, &user_id, "memory_branches", json!({})).await?;
    Ok(Json(r))
}

pub async fn create_branch(
    State(state): State<AppState>,
    AuthUser { user_id, .. }: AuthUser,
    Json(req): Json<CreateBranchRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), (StatusCode, String)> {
    let r = git_call(
        &state,
        &user_id,
        "memory_branch",
        json!({
            "name": req.name,
            "from_snapshot": req.from_snapshot,
            "from_timestamp": req.from_timestamp,
        }),
    )
    .await?;
    Ok((StatusCode::CREATED, Json(r)))
}

pub async fn checkout_branch(
    State(state): State<AppState>,
    AuthUser { user_id, .. }: AuthUser,
    Path(name): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let r = git_call(&state, &user_id, "memory_checkout", json!({ "name": name })).await?;
    Ok(Json(r))
}

pub async fn merge_branch(
    State(state): State<AppState>,
    AuthUser { user_id, .. }: AuthUser,
    Path(name): Path<String>,
    Json(req): Json<MergeRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let r = git_call(
        &state,
        &user_id,
        "memory_merge",
        json!({ "source": name, "strategy": req.strategy }),
    )
    .await?;
    Ok(Json(r))
}

pub async fn diff_branch(
    State(state): State<AppState>,
    AuthUser { user_id, .. }: AuthUser,
    Path(name): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let r = git_call(&state, &user_id, "memory_diff", json!({ "source": name })).await?;
    Ok(Json(r))
}

pub async fn delete_branch(
    State(state): State<AppState>,
    AuthUser { user_id, .. }: AuthUser,
    Path(name): Path<String>,
) -> Result<StatusCode, (StatusCode, String)> {
    git_call(
        &state,
        &user_id,
        "memory_branch_delete",
        json!({ "name": name }),
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}
