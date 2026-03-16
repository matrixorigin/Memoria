use axum::{extract::{Path, Query, State}, http::StatusCode, Json};
use serde::Deserialize;
use serde_json::json;

use crate::{auth::AuthUser, models::*, routes::memory::api_err, state::AppState};

#[derive(Deserialize, Default)]
pub struct ListSnapshotsQuery {
    #[serde(default = "default_snap_limit")]
    pub limit: i64,
    #[serde(default)]
    pub offset: i64,
}
fn default_snap_limit() -> i64 { 20 }

/// Delegate to git_tools::call for snapshot/branch operations.
async fn git_call(
    state: &AppState,
    user_id: &str,
    tool: &str,
    args: serde_json::Value,
) -> Result<serde_json::Value, (StatusCode, String)> {
    let result = memoria_mcp::git_tools::call(tool, args, &state.git, &state.service, user_id)
        .await.map_err(api_err)?;
    // Extract text from MCP response
    let text = result["content"][0]["text"].as_str().unwrap_or("").to_string();
    Ok(json!({ "result": text }))
}

pub async fn create_snapshot(
    State(state): State<AppState>,
    AuthUser(user_id): AuthUser,
    Json(req): Json<CreateSnapshotRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), (StatusCode, String)> {
    let r = git_call(&state, &user_id, "memory_snapshot", json!({ "name": req.name })).await?;
    Ok((StatusCode::CREATED, Json(r)))
}

pub async fn list_snapshots(
    State(state): State<AppState>,
    AuthUser(user_id): AuthUser,
    Query(q): Query<ListSnapshotsQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let r = git_call(&state, &user_id, "memory_snapshots",
        json!({ "limit": q.limit, "offset": q.offset })).await?;
    Ok(Json(r))
}

pub async fn delete_snapshot(
    State(state): State<AppState>,
    AuthUser(user_id): AuthUser,
    Path(name): Path<String>,
) -> Result<StatusCode, (StatusCode, String)> {
    git_call(&state, &user_id, "memory_snapshot_delete", json!({ "names": name })).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn delete_snapshot_bulk(
    State(state): State<AppState>,
    AuthUser(user_id): AuthUser,
    Json(req): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let r = git_call(&state, &user_id, "memory_snapshot_delete", req).await?;
    Ok(Json(r))
}

pub async fn rollback(
    State(state): State<AppState>,
    AuthUser(user_id): AuthUser,
    Path(name): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let r = git_call(&state, &user_id, "memory_rollback", json!({ "name": name })).await?;
    Ok(Json(r))
}

pub async fn list_branches(
    State(state): State<AppState>,
    AuthUser(user_id): AuthUser,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let r = git_call(&state, &user_id, "memory_branches", json!({})).await?;
    Ok(Json(r))
}

pub async fn create_branch(
    State(state): State<AppState>,
    AuthUser(user_id): AuthUser,
    Json(req): Json<CreateBranchRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), (StatusCode, String)> {
    let r = git_call(&state, &user_id, "memory_branch", json!({
        "name": req.name,
        "from_snapshot": req.from_snapshot,
        "from_timestamp": req.from_timestamp,
    })).await?;
    Ok((StatusCode::CREATED, Json(r)))
}

pub async fn checkout_branch(
    State(state): State<AppState>,
    AuthUser(user_id): AuthUser,
    Path(name): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let r = git_call(&state, &user_id, "memory_checkout", json!({ "name": name })).await?;
    Ok(Json(r))
}

pub async fn merge_branch(
    State(state): State<AppState>,
    AuthUser(user_id): AuthUser,
    Path(name): Path<String>,
    Json(req): Json<MergeRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let r = git_call(&state, &user_id, "memory_merge",
        json!({ "source": name, "strategy": req.strategy })).await?;
    Ok(Json(r))
}

pub async fn diff_branch(
    State(state): State<AppState>,
    AuthUser(user_id): AuthUser,
    Path(name): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let r = git_call(&state, &user_id, "memory_diff", json!({ "source": name })).await?;
    Ok(Json(r))
}

pub async fn delete_branch(
    State(state): State<AppState>,
    AuthUser(user_id): AuthUser,
    Path(name): Path<String>,
) -> Result<StatusCode, (StatusCode, String)> {
    git_call(&state, &user_id, "memory_branch_delete", json!({ "name": name })).await?;
    Ok(StatusCode::NO_CONTENT)
}
