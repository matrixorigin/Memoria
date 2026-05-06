use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;

use crate::{
    auth::AuthUser,
    models::*,
    routes::memory::{api_err, api_err_typed},
    state::AppState,
};
use memoria_core::TrustTier;
use memoria_git::GitForDataService;
use std::sync::Arc;

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

#[derive(Deserialize, Default)]
pub struct DiffBranchItemsQuery {
    pub limit: Option<i64>,
}

pub type ApplyBranchRequest = memoria_git::ApplySelection;

/// Delegate to git_tools::call for snapshot/branch operations.
async fn git_call_text(
    state: &AppState,
    user_id: &str,
    tool: &str,
    args: serde_json::Value,
) -> Result<String, (StatusCode, String)> {
    let result = memoria_mcp::git_tools::call(tool, args, &state.git, &state.service, user_id)
        .await
        .map_err(api_err_typed)?;
    Ok(result["content"][0]["text"]
        .as_str()
        .unwrap_or("")
        .to_string())
}

async fn git_call(
    state: &AppState,
    user_id: &str,
    tool: &str,
    args: serde_json::Value,
) -> Result<serde_json::Value, (StatusCode, String)> {
    let text = git_call_text(state, user_id, tool, args).await?;
    Ok(json!({ "result": text }))
}

async fn user_snapshot_store(
    state: &AppState,
    user_id: &str,
) -> Result<Arc<memoria_storage::SqlMemoryStore>, (StatusCode, String)> {
    state.service.user_sql_store(user_id).await.map_err(api_err)
}

fn user_git_service(
    sql: &Arc<memoria_storage::SqlMemoryStore>,
) -> Result<GitForDataService, (StatusCode, String)> {
    let db_name = sql.database_name().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "SQL store missing database URL".into(),
    ))?;
    Ok(GitForDataService::new(
        sql.pool().clone(),
        db_name.to_string(),
    ))
}

fn validate_snapshot_identifier(name: &str) -> Result<&str, (StatusCode, String)> {
    if !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_') {
        Ok(name)
    } else {
        Err((StatusCode::BAD_REQUEST, "Invalid snapshot name".into()))
    }
}

fn milestone_internal(name: &str) -> Option<String> {
    if let Some(rest) = name.strip_prefix("auto:") {
        Some(format!("mem_milestone_{rest}"))
    } else if name.starts_with("mem_milestone_") || name.starts_with("mem_snap_pre_") {
        Some(name.to_string())
    } else {
        None
    }
}

fn sanitize_identifier_fragment(value: &str) -> String {
    let sanitized = value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect::<String>()
        .trim_matches('_')
        .to_string();
    if sanitized.is_empty() {
        "db".to_string()
    } else {
        sanitized
    }
}

fn compact_identifier_fragment(value: &str, max_len: usize) -> String {
    let sanitized = sanitize_identifier_fragment(value);
    if sanitized.len() <= max_len {
        return sanitized;
    }
    if max_len <= 4 {
        return sanitized.chars().take(max_len).collect();
    }
    let head_len = (max_len - 1) / 2;
    let tail_len = max_len - head_len - 1;
    let head: String = sanitized.chars().take(head_len).collect();
    let tail_chars: Vec<char> = sanitized.chars().collect();
    let tail: String = tail_chars[tail_chars.len().saturating_sub(tail_len)..]
        .iter()
        .collect();
    format!("{head}_{tail}")
}

fn compact_safety_prefix(db_name: &str) -> String {
    format!("mem_snap_{}_pre_", compact_identifier_fragment(db_name, 21))
}

fn safety_snapshot_display_name(internal: &str) -> Option<String> {
    if let Some(rest) = internal.strip_prefix("mem_snap_pre_") {
        return Some(format!("pre_{rest}"));
    }
    let rest = internal.strip_prefix("mem_snap_")?;
    let (_, after_prefix) = rest.split_once("_pre_")?;
    Some(format!("pre_{after_prefix}"))
}

fn format_snapshot_timestamp(timestamp: Option<chrono::NaiveDateTime>) -> Option<String> {
    timestamp.map(|ts| ts.and_utc().to_rfc3339())
}

fn format_snapshot_list_result(snapshots: &[Value], total: usize) -> String {
    if snapshots.is_empty() {
        return "Snapshots (0):".to_string();
    }
    let text = snapshots
        .iter()
        .map(|snapshot| {
            let name = snapshot["name"].as_str().unwrap_or_default();
            let ts = snapshot["timestamp"]
                .as_str()
                .or_else(|| snapshot["created_at"].as_str())
                .unwrap_or_default();
            format!("{name} ({ts})")
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!("Snapshots ({total} total):\n{text}")
}

fn format_branch_list_result(branches: &[Value]) -> String {
    if branches.is_empty() {
        return "Branches:\nmain ← active".to_string();
    }
    let text = branches
        .iter()
        .map(|branch| {
            let name = branch["name"].as_str().unwrap_or_default();
            if branch["active"].as_bool().unwrap_or(false) {
                format!("{name} ← active")
            } else {
                name.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!("Branches:\n{text}")
}

/// Like `branch_table_name` but returns the raw table name (no db prefix or backticks),
/// suitable for `data branch diff` which builds its own qualified identifiers.
async fn branch_table_name_raw(
    sql: &Arc<memoria_storage::SqlMemoryStore>,
    scope_id: &str,
    branch_name: &str,
) -> Result<String, (StatusCode, String)> {
    if branch_name == "main" {
        return Ok("mem_memories".to_string());
    }
    for (name, table_name) in sql.list_branches(scope_id).await.map_err(api_err)? {
        if name == branch_name {
            return Ok(table_name);
        }
    }
    Err((
        StatusCode::NOT_FOUND,
        format!("Branch not found: {branch_name}"),
    ))
}

async fn branch_diff_items_payload(
    state: &AppState,
    scope_id: &str,
    branch_name: &str,
    limit: i64,
) -> Result<Value, (StatusCode, String)> {
    let sql = user_snapshot_store(state, scope_id).await?;
    let git = user_git_service(&sql)?;
    let branch_table_raw = branch_table_name_raw(&sql, scope_id, branch_name).await?;
    let limit = limit.clamp(1, 500);

    // Use MatrixOne native `data branch diff` + classify (with source-aware conflict detection)
    let raw_rows = git
        .diff_branch_rows(&branch_table_raw, "mem_memories", scope_id, limit * 3)
        .await
        .map_err(api_err)?;
    let classified = memoria_git::classify_diff_rows(raw_rows, &branch_table_raw);

    let item_to_json = |item: &memoria_git::DiffItem| -> Value {
        json!({
            "memory_id": item.memory_id,
            "content": item.content,
            "memory_type": item.memory_type,
            "author_id": item.author_id,
        })
    };

    let added: Vec<Value> = classified
        .added
        .iter()
        .take(limit as usize)
        .map(item_to_json)
        .collect();
    let updated: Vec<Value> = classified
        .updated
        .iter()
        .take(limit as usize)
        .map(|pair| {
            json!({
                "memory_id": pair.new_memory_id,
                "content": pair.new_content,
                "memory_type": pair.memory_type,
                "old_memory_id": pair.old_memory_id,
                "old_content": pair.old_content,
                "author_id": pair.author_id,
            })
        })
        .collect();
    let removed: Vec<Value> = classified
        .removed
        .iter()
        .take(limit as usize)
        .map(item_to_json)
        .collect();
    let conflicts: Vec<Value> = classified
        .conflicts
        .iter()
        .take(limit as usize)
        .map(|c| {
            json!({
                "memory_id": c.memory_id,
                "branch": {
                    "content": c.branch_side.content,
                    "is_active": c.branch_side.is_active,
                    "superseded_by": c.branch_side.superseded_by,
                    "superseded_by_content": c.branch_side.superseded_by_content,
                    "author_id": c.branch_side.author_id,
                },
                "main": {
                    "content": c.main_side.content,
                    "is_active": c.main_side.is_active,
                    "superseded_by": c.main_side.superseded_by,
                    "superseded_by_content": c.main_side.superseded_by_content,
                    "author_id": c.main_side.author_id,
                },
            })
        })
        .collect();

    let behind_main: Vec<Value> = classified
        .behind_main
        .iter()
        .take(limit as usize)
        .map(item_to_json)
        .collect();

    Ok(json!({
        "branch": branch_name,
        "against": "main",
        "added": added,
        "updated": updated,
        "removed": removed,
        "conflicts": conflicts,
        "behind_main": behind_main,
    }))
}

fn parse_created_snapshot_result(text: &str) -> Option<(String, String)> {
    let rest = text.strip_prefix("Snapshot '")?;
    let (name, timestamp) = rest.split_once("' created at ")?;
    Some((name.to_string(), timestamp.to_string()))
}

async fn snapshot_summary_value(
    sql: &Arc<memoria_storage::SqlMemoryStore>,
    user_id: &str,
    snapshot: &memoria_mcp::git_tools::VisibleSnapshot,
) -> Result<Value, (StatusCode, String)> {
    let snapshot_name = validate_snapshot_identifier(&snapshot.internal_name)?.to_string();
    let table = sql.t("mem_memories");
    let count_sql = format!(
        "SELECT COUNT(*) as cnt FROM {table} {{SNAPSHOT = '{snapshot_name}'}} WHERE user_id = ? AND is_active > 0"
    );
    let memory_count: i64 = sqlx::query_scalar(&count_sql)
        .bind(user_id)
        .fetch_one(sql.pool())
        .await
        .map_err(api_err)?;
    let timestamp = format_snapshot_timestamp(snapshot.timestamp);
    Ok(json!({
        "name": snapshot.display_name,
        "snapshot_name": snapshot_name,
        "description": Value::Null,
        "memory_count": memory_count,
        "created_at": timestamp,
        "timestamp": timestamp,
        "registered": snapshot.registered,
    }))
}

async fn snapshot_list_payload(
    state: &AppState,
    user_id: &str,
    limit: i64,
    offset: i64,
) -> Result<Value, (StatusCode, String)> {
    let sql = user_snapshot_store(state, user_id).await?;
    let all = memoria_mcp::git_tools::visible_snapshots_for_user(&state.service, user_id)
        .await
        .map_err(api_err_typed)?;
    let total = all.len();
    let mut snapshots = Vec::new();
    for snapshot in all
        .iter()
        .skip(offset.max(0) as usize)
        .take(limit.max(0) as usize)
    {
        snapshots.push(snapshot_summary_value(&sql, user_id, snapshot).await?);
    }
    let has_more = offset.max(0) as usize + snapshots.len() < total;
    let result = format_snapshot_list_result(&snapshots, total);
    Ok(json!({
        "snapshots": snapshots,
        "total": total,
        "limit": limit.max(0),
        "offset": offset.max(0),
        "has_more": has_more,
        "result": result,
    }))
}

fn is_safety_snapshot_name(sql: &Arc<memoria_storage::SqlMemoryStore>, name: &str) -> bool {
    if name.starts_with("mem_snap_pre_") {
        return true;
    }
    match sql.database_name() {
        Some(db_name) => {
            name.starts_with(&format!("mem_snap_{db_name}_pre_"))
                || name.starts_with(&compact_safety_prefix(db_name))
        }
        None => false,
    }
}

async fn resolve_snapshot_internal(
    state: &AppState,
    user_id: &str,
    name: &str,
) -> Result<String, (StatusCode, String)> {
    let sql = user_snapshot_store(state, user_id).await?;
    let git = user_git_service(&sql)?;

    let internal = if let Some(milestone) = milestone_internal(name) {
        milestone
    } else if is_safety_snapshot_name(&sql, name) {
        git.get_snapshot(name)
            .await
            .map_err(api_err)?
            .map(|_| name.to_string())
            .ok_or((StatusCode::NOT_FOUND, "Snapshot not found".into()))?
    } else if name.starts_with("mem_snap_") {
        sql.get_snapshot_registration_by_internal(user_id, name)
            .await
            .map_err(api_err)?
            .map(|r| r.snapshot_name)
            .ok_or((StatusCode::NOT_FOUND, "Snapshot not found".into()))?
    } else if let Some(internal) = sql
        .get_snapshot_registration(user_id, name)
        .await
        .map_err(api_err)?
        .map(|r| r.snapshot_name)
    {
        internal
    } else if name.starts_with("pre_") {
        git.list_snapshots()
            .await
            .map_err(api_err)?
            .into_iter()
            .find(|snapshot| {
                safety_snapshot_display_name(&snapshot.snapshot_name).as_deref() == Some(name)
            })
            .map(|snapshot| snapshot.snapshot_name)
            .ok_or((StatusCode::NOT_FOUND, "Snapshot not found".into()))?
    } else {
        return Err((StatusCode::NOT_FOUND, "Snapshot not found".into()));
    };

    let internal = validate_snapshot_identifier(&internal)?.to_string();
    git.get_snapshot(&internal)
        .await
        .map_err(api_err)?
        .ok_or((StatusCode::NOT_FOUND, "Snapshot not found".into()))?;
    Ok(internal)
}

pub async fn create_snapshot(
    State(state): State<AppState>,
    auth: AuthUser,
    Json(req): Json<CreateSnapshotRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), (StatusCode, String)> {
    let result = git_call_text(
        &state,
        auth.scope_id(),
        "memory_snapshot",
        json!({ "name": req.name }),
    )
    .await?;
    let mut body = json!({
        "name": req.name.clone(),
        "description": req.description.clone(),
        "result": result.clone(),
    });
    if let Some((display_name, created_at)) =
        parse_created_snapshot_result(body["result"].as_str().unwrap_or_default())
    {
        body["name"] = json!(display_name.clone());
        body["created_at"] = json!(created_at.clone());
        body["timestamp"] = json!(created_at);

        let sql = user_snapshot_store(&state, auth.scope_id()).await?;
        let snapshots =
            memoria_mcp::git_tools::visible_snapshots_for_user(&state.service, auth.scope_id())
                .await
                .map_err(api_err_typed)?;
        if let Some(snapshot) = snapshots
            .iter()
            .find(|snapshot| snapshot.display_name == display_name)
        {
            body = snapshot_summary_value(&sql, auth.scope_id(), snapshot).await?;
            body["description"] = json!(req.description.clone());
            body["result"] = json!(result.clone());
        }
    }
    Ok((StatusCode::CREATED, Json(body)))
}

pub async fn list_snapshots(
    State(state): State<AppState>,
    auth: AuthUser,
    Query(q): Query<ListSnapshotsQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    Ok(Json(
        snapshot_list_payload(&state, auth.scope_id(), q.limit, q.offset).await?,
    ))
}

/// GET /v1/snapshots/:name — read snapshot detail with time-travel query
pub async fn get_snapshot(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(name): Path<String>,
    Query(q): Query<GetSnapshotQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let sql = user_snapshot_store(&state, auth.scope_id()).await?;
    let pool = sql.pool();
    let table = sql.active_table(auth.scope_id()).await.map_err(api_err)?;

    let snap_name = resolve_snapshot_internal(&state, auth.scope_id(), &name).await?;
    let limit = q.limit.unwrap_or(50).min(500);
    let offset = q.offset.unwrap_or(0);
    let detail = q.detail.as_deref().unwrap_or("brief");

    // Total count via time-travel
    let count_sql = format!(
        "SELECT COUNT(*) as cnt FROM {table} {{SNAPSHOT = '{snap_name}'}} WHERE user_id = ? AND is_active > 0"
    );
    let total: i64 = sqlx::query_scalar(&count_sql)
        .bind(auth.scope_id())
        .fetch_one(pool)
        .await
        .map_err(api_err)?;

    // Type distribution
    let type_sql = format!(
        "SELECT memory_type, COUNT(*) as cnt FROM {table} {{SNAPSHOT = '{snap_name}'}} \
         WHERE user_id = ? AND is_active > 0 GROUP BY memory_type"
    );
    let type_rows = sqlx::query(&type_sql)
        .bind(auth.scope_id())
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
        "SELECT memory_id, user_id, content, memory_type, trust_tier, initial_confidence, is_active, session_id, observed_at, created_at FROM {table} {{SNAPSHOT = '{snap_name}'}} \
          WHERE user_id = ? AND is_active > 0 ORDER BY observed_at DESC LIMIT ? OFFSET ?"
    );
    let rows = sqlx::query(&mem_sql)
        .bind(auth.scope_id())
        .bind(limit)
        .bind(offset)
        .fetch_all(pool)
        .await
        .map_err(api_err)?;

    let memories: Vec<serde_json::Value> = rows
        .iter()
        .map(|r| {
            let content: String = r.try_get("content").unwrap_or_default();
            let initial_confidence = r.try_get::<f64, _>("initial_confidence").unwrap_or(0.0);
            let truncated = if content.len() > content_limit {
                format!("{} [truncated]", &content[..content_limit])
            } else {
                content
            };
            let mut m = json!({
                "memory_id": r.try_get::<String, _>("memory_id").unwrap_or_default(),
                "user_id": r.try_get::<String, _>("user_id").unwrap_or_default(),
                "memory_type": r.try_get::<String, _>("memory_type").unwrap_or_default(),
                "content": truncated,
                "initial_confidence": initial_confidence,
                "is_active": r.try_get::<i8, _>("is_active").unwrap_or(1) != 0,
                "session_id": r.try_get::<Option<String>, _>("session_id").ok().flatten(),
                "observed_at": format_snapshot_timestamp(
                    r.try_get::<Option<chrono::NaiveDateTime>, _>("observed_at")
                        .ok()
                        .flatten(),
                ),
                "created_at": format_snapshot_timestamp(
                    r.try_get::<Option<chrono::NaiveDateTime>, _>("created_at")
                        .ok()
                        .flatten(),
                ),
                "trust_tier": r
                    .try_get::<String, _>("trust_tier")
                    .unwrap_or_else(|_| TrustTier::default().to_string()),
                "retrieval_score": Value::Null,
            });
            if detail == "full" {
                m["confidence"] = json!(initial_confidence);
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
    auth: AuthUser,
    Path(name): Path<String>,
    Query(q): Query<DiffSnapshotQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let sql = user_snapshot_store(&state, auth.scope_id()).await?;
    let pool = sql.pool();
    let table = sql.active_table(auth.scope_id()).await.map_err(api_err)?;

    let snap_name = resolve_snapshot_internal(&state, auth.scope_id(), &name).await?;
    let limit = q.limit.unwrap_or(50).min(200);

    // Counts
    let snap_count: i64 = sqlx::query_scalar(&format!(
        "SELECT COUNT(*) FROM {table} {{SNAPSHOT = '{snap_name}'}} WHERE user_id = ? AND is_active > 0"
    )).bind(auth.scope_id()).fetch_one(pool).await.map_err(api_err)?;

    let curr_count: i64 = sqlx::query_scalar(&format!(
        "SELECT COUNT(*) FROM {table} WHERE user_id = ? AND is_active > 0"
    ))
    .bind(auth.scope_id())
    .fetch_one(pool)
    .await
    .map_err(api_err)?;

    // Added (in current but not in snapshot)
    let added_sql = format!(
        "SELECT c.memory_id, c.content, c.memory_type, c.trust_tier FROM {table} c \
          LEFT JOIN {table} {{SNAPSHOT = '{snap_name}'}} s ON c.memory_id = s.memory_id AND s.is_active > 0 \
          WHERE c.user_id = ? AND c.is_active > 0 AND s.memory_id IS NULL LIMIT ?"
    );
    let added_rows = sqlx::query(&added_sql)
        .bind(auth.scope_id())
        .bind(limit)
        .fetch_all(pool)
        .await
        .map_err(api_err)?;

    // Removed (in snapshot but not in current)
    let removed_sql = format!(
        "SELECT s.memory_id, s.content, s.memory_type, s.trust_tier FROM {table} {{SNAPSHOT = '{snap_name}'}} s \
          LEFT JOIN {table} c ON s.memory_id = c.memory_id AND c.is_active > 0 \
          WHERE s.user_id = ? AND s.is_active > 0 AND c.memory_id IS NULL LIMIT ?"
    );
    let removed_rows = sqlx::query(&removed_sql)
        .bind(auth.scope_id())
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
                    "trust_tier": r
                        .try_get::<String, _>("trust_tier")
                        .unwrap_or_else(|_| TrustTier::default().to_string()),
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
    auth: AuthUser,
    Path(name): Path<String>,
) -> Result<StatusCode, (StatusCode, String)> {
    git_call(
        &state,
        auth.scope_id(),
        "memory_snapshot_delete",
        json!({ "names": name }),
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn delete_snapshot_bulk(
    State(state): State<AppState>,
    auth: AuthUser,
    Json(req): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let r = git_call(&state, auth.scope_id(), "memory_snapshot_delete", req).await?;
    Ok(Json(r))
}

pub async fn rollback(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(name): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let r = git_call(
        &state,
        auth.scope_id(),
        "memory_rollback",
        json!({ "name": name }),
    )
    .await?;
    Ok(Json(r))
}

pub async fn list_branches(
    State(state): State<AppState>,
    auth: AuthUser,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let sql = state
        .service
        .user_sql_store(auth.scope_id())
        .await
        .map_err(api_err)?;
    let active_branch = sql
        .active_branch_name(auth.scope_id())
        .await
        .map_err(api_err)?;
    let mut branches = vec![json!({
        "name": "main",
        "active": active_branch == "main",
    })];
    for (name, _table_name) in sql.list_branches(auth.scope_id()).await.map_err(api_err)? {
        branches.push(json!({
            "name": name,
            "active": name == active_branch,
        }));
    }
    if !branches
        .iter()
        .any(|branch| branch["active"].as_bool().unwrap_or(false))
    {
        branches[0]["active"] = json!(true);
    }
    let result = format_branch_list_result(&branches);
    Ok(Json(json!({
        "branches": branches,
        "result": result,
    })))
}

pub async fn create_branch(
    State(state): State<AppState>,
    auth: AuthUser,
    Json(req): Json<CreateBranchRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), (StatusCode, String)> {
    let r = git_call(
        &state,
        auth.scope_id(),
        "memory_branch",
        json!({
            "name": req.name,
            "from_snapshot": req.from_snapshot,
            "from_timestamp": req.from_timestamp,
        }),
    )
    .await?;
    // MCP returns success with "already exists" text — surface as 409 Conflict
    if let Some(text) = r["content"][0]["text"].as_str() {
        if text.contains("already exists") {
            return Err((StatusCode::CONFLICT, text.to_string()));
        }
    }
    Ok((StatusCode::CREATED, Json(r)))
}

pub async fn checkout_branch(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(name): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let r = git_call(
        &state,
        auth.scope_id(),
        "memory_checkout",
        json!({ "name": name }),
    )
    .await?;
    Ok(Json(r))
}

pub async fn merge_branch(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(name): Path<String>,
    Json(req): Json<MergeRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let r = git_call(
        &state,
        auth.scope_id(),
        "memory_merge",
        json!({ "source": name, "strategy": req.strategy }),
    )
    .await?;
    Ok(Json(r))
}

pub async fn diff_branch(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(name): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let r = git_call(
        &state,
        auth.scope_id(),
        "memory_diff",
        json!({ "source": name }),
    )
    .await?;
    Ok(Json(r))
}

pub async fn diff_branch_items(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(name): Path<String>,
    Query(q): Query<DiffBranchItemsQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    Ok(Json(
        branch_diff_items_payload(&state, auth.scope_id(), &name, q.limit.unwrap_or(100)).await?,
    ))
}

pub async fn apply_branch(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(name): Path<String>,
    Json(req): Json<ApplyBranchRequest>,
) -> Result<Json<memoria_git::ApplyResult>, (StatusCode, String)> {
    if name == "main" {
        return Err((StatusCode::BAD_REQUEST, "Cannot apply from main".into()));
    }
    let sql = user_snapshot_store(&state, auth.scope_id()).await?;
    let git = user_git_service(&sql)?;
    let branch_table = branch_table_name_raw(&sql, auth.scope_id(), &name).await?;
    let response = git
        .selective_apply(&branch_table, "mem_memories", auth.scope_id(), req)
        .await
        .map_err(api_err)?;
    let payload = json!({
        "actor_user_id": auth.user_id,
        "group_id": auth.group_id,
        "source_branch": name,
        "applied_adds": response.applied_adds.clone(),
        "applied_updates": response.applied_updates.clone(),
        "applied_removes": response.applied_removes.clone(),
        "applied_conflicts": response.applied_conflicts.clone(),
        "skipped_adds": response.skipped_adds.clone(),
        "skipped_updates": response.skipped_updates.clone(),
        "skipped_removes": response.skipped_removes.clone(),
        "skipped_conflicts": response.skipped_conflicts.clone(),
    })
    .to_string();
    state.service.send_edit_log(
        auth.scope_id(),
        "branch_apply",
        None,
        Some(&payload),
        "selective apply to main",
        None,
    );

    Ok(Json(response))
}

pub async fn delete_branch(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(name): Path<String>,
) -> Result<StatusCode, (StatusCode, String)> {
    git_call(
        &state,
        auth.scope_id(),
        "memory_branch_delete",
        json!({ "name": name }),
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}
