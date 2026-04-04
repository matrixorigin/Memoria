//! Git-for-Data MCP tools: snapshot, branch, merge, rollback, diff.
//! 9 tools — brings total to 17 (8 core + 9 git).
//!
//! Parity with Python version:
//! - snapshot names prefixed with "mem_snap_", scoped by user DB, sanitized to 40 chars
//! - snapshot list filters to current user's mem_snap_ + global mem_milestone_, strips prefix for display
//! - snapshot delete supports names, prefix, older_than
//! - snapshot limit: 20 per user
//! - rollback restores mem_memories + graph tables
//! - branch limit: 20 per user
//! - branch duplicate name rejected (including deleted)
//! - branch name sanitized to 40 chars

use chrono::NaiveDateTime;
use memoria_core::MemoriaError;
use memoria_git::GitForDataService;
use memoria_service::MemoryService;
use serde_json::{json, Value};
use sqlx::Row;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use uuid::Uuid;

/// Convert sqlx::Error to MemoriaError::Database
fn db_err(e: sqlx::Error) -> MemoriaError {
    MemoriaError::Database(e.to_string())
}

/// Convert memoria_git errors to MemoriaError
fn git_err(e: impl std::fmt::Display) -> MemoriaError {
    MemoriaError::Internal(e.to_string())
}

fn validate_identifier(name: &str) -> Result<&str, MemoriaError> {
    if !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        Ok(name)
    } else {
        Err(MemoriaError::Internal(format!(
            "Invalid identifier: {name:?} — only alphanumeric and underscore allowed"
        )))
    }
}

const MAX_USER_SNAPSHOTS: i64 = 20;
const MAX_BRANCHES: i64 = 20;
const MAX_IDENTIFIER_LEN: usize = 64;
const SNAP_PREFIX: &str = "mem_snap_";
const MILESTONE_PREFIX: &str = "mem_milestone_";
const SAFETY_PREFIX: &str = "mem_snap_pre_";
const SNAP_SCOPE_MAX_LEN: usize = MAX_IDENTIFIER_LEN - SNAP_PREFIX.len() - 2 - 2 - 40;
const SAFETY_SCOPE_MAX_LEN: usize = 21;

fn safety_prefix(db_name: Option<&str>) -> String {
    match db_name {
        Some(db_name) => format!(
            "mem_snap_{}_pre_",
            compact_identifier_fragment(db_name, SAFETY_SCOPE_MAX_LEN)
        ),
        None => SAFETY_PREFIX.to_string(),
    }
}

fn legacy_safety_prefix(db_name: Option<&str>) -> Option<String> {
    db_name.map(|db_name| format!("mem_snap_{db_name}_pre_"))
}

fn is_safety_snapshot_name(name: &str, db_name: Option<&str>) -> bool {
    name.starts_with(SAFETY_PREFIX)
        || name.starts_with(&safety_prefix(db_name))
        || legacy_safety_prefix(db_name)
            .as_deref()
            .is_some_and(|prefix| name.starts_with(prefix))
}

/// Sanitize a user-provided name: keep alphanumeric+underscore, truncate to 40 chars.
/// If result starts with non-alpha, prepend "s_".
fn sanitize_name(name: &str) -> String {
    let mut clean: String = name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .take(40)
        .collect();
    if clean.is_empty() || !clean.chars().next().unwrap().is_alphabetic() {
        clean = format!("s_{clean}");
    }
    clean
}

fn sanitize_snapshot_scope(scope: &str) -> String {
    compact_identifier_fragment(scope, SNAP_SCOPE_MAX_LEN)
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

/// Convert user-facing snapshot name → internal MatrixOne snapshot name.
fn snap_internal(db_name: &str, name: &str) -> String {
    if name.starts_with(SNAP_PREFIX) || name.starts_with(MILESTONE_PREFIX) {
        name.to_string()
    } else {
        let scope = sanitize_snapshot_scope(db_name);
        let internal = format!(
            "{SNAP_PREFIX}{}_{scope}_{}",
            scope.len(),
            sanitize_name(name)
        );
        debug_assert!(internal.len() <= MAX_IDENTIFIER_LEN);
        internal
    }
}

/// Convert internal snapshot name → user-facing display name.
fn snap_display(internal: &str) -> String {
    if let Some(rest) = internal.strip_prefix(SNAP_PREFIX) {
        if let Some((len_part, scoped_rest)) = rest.split_once('_') {
            if let Ok(scope_len) = len_part.parse::<usize>() {
                if scoped_rest.len() > scope_len
                    && scoped_rest.as_bytes().get(scope_len) == Some(&b'_')
                {
                    return scoped_rest[scope_len + 1..].to_string();
                }
            }
        }
        rest.to_string()
    } else if let Some(rest) = internal.strip_prefix(MILESTONE_PREFIX) {
        format!("auto:{rest}")
    } else {
        internal.to_string()
    }
}

fn safety_display_name(internal: &str) -> Option<String> {
    if let Some(rest) = internal.strip_prefix(SAFETY_PREFIX) {
        return Some(format!("pre_{rest}"));
    }
    let rest = internal.strip_prefix(SNAP_PREFIX)?;
    let (_, after_prefix) = rest.split_once("_pre_")?;
    Some(format!("pre_{after_prefix}"))
}

#[derive(Clone)]
pub struct VisibleSnapshot {
    pub display_name: String,
    pub internal_name: String,
    pub timestamp: Option<NaiveDateTime>,
    pub registered: bool,
}

#[derive(Debug, Clone)]
struct ReplaceCandidate {
    main_memory_id: String,
    branch_memory_id: String,
    replacement_content: String,
    conflict_distance: f64,
}

fn milestone_internal(name: &str) -> Option<String> {
    if let Some(rest) = name.strip_prefix("auto:") {
        Some(format!("{MILESTONE_PREFIX}{rest}"))
    } else if name.starts_with(MILESTONE_PREFIX) {
        Some(name.to_string())
    } else {
        None
    }
}

async fn snapshot_store(
    svc: &Arc<MemoryService>,
    user_id: &str,
) -> Result<Arc<memoria_storage::SqlMemoryStore>, MemoriaError> {
    svc.user_sql_store(user_id).await
}

fn git_for_store(
    sql: &Arc<memoria_storage::SqlMemoryStore>,
) -> Result<GitForDataService, MemoriaError> {
    let db_name = sql.database_name().ok_or_else(|| {
        MemoriaError::Internal("Git ops require a database-backed SQL store".into())
    })?;
    Ok(GitForDataService::new(
        sql.pool().clone(),
        db_name.to_string(),
    ))
}

pub async fn visible_snapshots_for_user(
    svc: &Arc<MemoryService>,
    user_id: &str,
) -> Result<Vec<VisibleSnapshot>, MemoriaError> {
    let sql = snapshot_store(svc, user_id).await?;
    let db_name = sql.database_name();
    let git = git_for_store(&sql)?;
    let all = git.list_snapshots().await.map_err(git_err)?;
    let actual_by_name: HashMap<String, memoria_git::Snapshot> = all
        .into_iter()
        .filter(|s| {
            s.snapshot_name.starts_with(SNAP_PREFIX)
                || s.snapshot_name.starts_with(MILESTONE_PREFIX)
        })
        .map(|s| (s.snapshot_name.clone(), s))
        .collect();

    let mut snapshots = Vec::new();
    let mut seen_internal = HashSet::new();
    for reg in sql.list_snapshot_registrations(user_id).await? {
        if let Some(actual) = actual_by_name.get(&reg.snapshot_name) {
            seen_internal.insert(reg.snapshot_name.clone());
            snapshots.push(VisibleSnapshot {
                display_name: reg.name,
                internal_name: reg.snapshot_name,
                timestamp: actual.timestamp.or(Some(reg.created_at)),
                registered: true,
            });
        }
    }

    for actual in actual_by_name.values() {
        if !seen_internal.contains(&actual.snapshot_name)
            && (actual.snapshot_name.starts_with(MILESTONE_PREFIX)
                || is_safety_snapshot_name(&actual.snapshot_name, db_name))
        {
            snapshots.push(VisibleSnapshot {
                display_name: if is_safety_snapshot_name(&actual.snapshot_name, db_name) {
                    safety_display_name(&actual.snapshot_name)
                        .unwrap_or_else(|| snap_display(&actual.snapshot_name))
                } else {
                    snap_display(&actual.snapshot_name)
                },
                internal_name: actual.snapshot_name.clone(),
                timestamp: actual.timestamp,
                registered: false,
            });
        }
    }

    snapshots.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
    Ok(snapshots)
}

async fn resolve_snapshot_for_user(
    svc: &Arc<MemoryService>,
    user_id: &str,
    name: &str,
) -> Result<Option<String>, MemoriaError> {
    let sql = snapshot_store(svc, user_id).await?;
    let git = git_for_store(&sql)?;
    let db_name = sql.database_name();
    if let Some(internal) = milestone_internal(name) {
        return Ok(git
            .get_snapshot(&internal)
            .await
            .map_err(git_err)?
            .map(|_| internal));
    }
    if is_safety_snapshot_name(name, db_name) {
        return Ok(git
            .get_snapshot(name)
            .await
            .map_err(git_err)?
            .map(|_| name.to_string()));
    }

    let reg = if name.starts_with(SNAP_PREFIX) {
        sql.get_snapshot_registration_by_internal(user_id, name)
            .await?
    } else {
        sql.get_snapshot_registration(user_id, name).await?
    };
    if let Some(reg) = reg {
        return Ok(Some(reg.snapshot_name));
    }
    Ok(visible_snapshots_for_user(svc, user_id)
        .await?
        .into_iter()
        .find(|snapshot| snapshot.display_name == name)
        .map(|snapshot| snapshot.internal_name))
}

async fn acquire_snapshot_create_lock(
    lock_store: &Arc<memoria_storage::SqlMemoryStore>,
    sql: &Arc<memoria_storage::SqlMemoryStore>,
    git: &GitForDataService,
    user_id: &str,
    display: &str,
    internal: &str,
) -> Result<Option<String>, MemoriaError> {
    let lock_key = format!("snapshot_create:{user_id}:{internal}");
    for _ in 0..20 {
        if lock_store.try_acquire_lock(&lock_key, 30).await? {
            return Ok(Some(lock_key));
        }
        if sql
            .get_snapshot_registration(user_id, display)
            .await?
            .is_some()
            || sql
                .get_snapshot_registration_by_internal(user_id, internal)
                .await?
                .is_some()
            || git.get_snapshot(internal).await.map_err(git_err)?.is_some()
        {
            return Ok(None);
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    Ok(None)
}

pub fn list() -> Value {
    json!([
        {
            "name": "memory_snapshot",
            "description": "Create a named snapshot of current memory state",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": {"type": "string"},
                    "description": {"type": "string"}
                },
                "required": ["name"]
            }
        },
        {
            "name": "memory_snapshots",
            "description": "List snapshots with pagination",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "limit": {"type": "integer", "default": 20},
                    "offset": {"type": "integer", "default": 0}
                }
            }
        },
        {
            "name": "memory_snapshot_delete",
            "description": "Delete snapshots by name(s), prefix, or age",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "names": {"type": "string"},
                    "prefix": {"type": "string"},
                    "older_than": {"type": "string", "description": "ISO date e.g. 2026-03-01"}
                }
            }
        },
        {
            "name": "memory_rollback",
            "description": "Restore memories to a previous snapshot",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": {"type": "string"}
                },
                "required": ["name"]
            }
        },
        {
            "name": "memory_branch",
            "description": "Create a new memory branch for isolated experimentation",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": {"type": "string"},
                    "from_snapshot": {"type": "string"},
                    "from_timestamp": {"type": "string"}
                },
                "required": ["name"]
            }
        },
        {
            "name": "memory_branches",
            "description": "List all memory branches",
            "inputSchema": {"type": "object", "properties": {}}
        },
        {
            "name": "memory_checkout",
            "description": "Switch to a different memory branch",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": {"type": "string"}
                },
                "required": ["name"]
            }
        },
        {
            "name": "memory_merge",
            "description": "Merge a branch back into main",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "source": {"type": "string"},
                    "strategy": {
                        "type": "string",
                        "default": "accept",
                        "description": "accept | replace | append (accept is the default and an alias of replace / branch-wins on detected conflicts)"
                    }
                },
                "required": ["source"]
            }
        },
        {
            "name": "memory_branch_delete",
            "description": "Delete a memory branch",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": {"type": "string"}
                },
                "required": ["name"]
            }
        },
        {
            "name": "memory_diff",
            "description": "Preview what would change if a branch were merged into main",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "source": {"type": "string"},
                    "limit": {"type": "integer", "default": 50}
                },
                "required": ["source"]
            }
        }
    ])
}

pub async fn call(
    name: &str,
    args: Value,
    _git: &Arc<GitForDataService>,
    svc: &Arc<MemoryService>,
    user_id: &str,
) -> Result<Value, MemoriaError> {
    match name {
        "memory_snapshot" => {
            let user_snapshots = visible_snapshots_for_user(svc, user_id)
                .await?
                .into_iter()
                .filter(|s| s.registered)
                .count() as i64;
            if user_snapshots >= MAX_USER_SNAPSHOTS {
                return Ok(mcp_text(&format!(
                    "Snapshot limit reached ({MAX_USER_SNAPSHOTS}) for user {user_id}. Delete old snapshots first."
                )));
            }
            let snap_name = args["name"].as_str().unwrap_or("");
            let sql = snapshot_store(svc, user_id).await?;
            let internal = snap_internal(
                sql.database_name().ok_or_else(|| {
                    MemoriaError::Internal(
                        "Snapshot ops require a database-backed SQL store".into(),
                    )
                })?,
                snap_name,
            );
            let display = if snap_name.starts_with(SNAP_PREFIX) {
                snap_display(snap_name)
            } else {
                sanitize_name(snap_name)
            };
            let git = git_for_store(&sql)?;
            let lock_store = svc.sql_store.as_ref().cloned().ok_or_else(|| {
                MemoriaError::Internal("Snapshot ops require a database-backed SQL store".into())
            })?;
            let Some(lock_key) =
                acquire_snapshot_create_lock(&lock_store, &sql, &git, user_id, &display, &internal)
                    .await?
            else {
                return Ok(mcp_text(&format!("Snapshot '{}' already exists.", display)));
            };
            let result = async {
                if sql
                    .get_snapshot_registration(user_id, &display)
                    .await?
                    .is_some()
                    || sql
                        .get_snapshot_registration_by_internal(user_id, &internal)
                        .await?
                        .is_some()
                {
                    return Ok(mcp_text(&format!("Snapshot '{}' already exists.", display)));
                }
                let snap = match git.create_snapshot(&internal).await {
                    Ok(snap) => snap,
                    Err(err) => {
                        if git
                            .get_snapshot(&internal)
                            .await
                            .map_err(git_err)?
                            .is_some()
                        {
                            return Ok(mcp_text(&format!(
                                "Snapshot '{}' already exists.",
                                display
                            )));
                        }
                        return Err(git_err(err));
                    }
                };
                sql.register_snapshot(user_id, &display, &snap.snapshot_name)
                    .await?;
                Ok(mcp_text(&format!(
                    "Snapshot '{}' created at {:?}",
                    display, snap.timestamp
                )))
            }
            .await;
            let _ = lock_store.release_lock(&lock_key).await;
            result
        }

        "memory_snapshots" => {
            let limit = args["limit"].as_i64().unwrap_or(20) as usize;
            let offset = args["offset"].as_i64().unwrap_or(0) as usize;
            let snaps = visible_snapshots_for_user(svc, user_id).await?;
            let total = snaps.len();
            let page: Vec<_> = snaps.into_iter().skip(offset).take(limit).collect();
            if page.is_empty() {
                return Ok(mcp_text("No snapshots found."));
            }
            let text = page
                .iter()
                .map(|s| {
                    format!(
                        "{} ({})",
                        s.display_name,
                        s.timestamp.map(|t| t.to_string()).unwrap_or_default()
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            Ok(mcp_text(&format!("Snapshots ({total} total):\n{text}")))
        }

        "memory_snapshot_delete" => {
            let sql = snapshot_store(svc, user_id).await?;
            let git = git_for_store(&sql)?;
            let snaps = visible_snapshots_for_user(svc, user_id).await?;

            let to_delete: Vec<VisibleSnapshot> = if let Some(names) = args["names"].as_str() {
                let name_set: HashSet<String> =
                    names.split(',').map(|n| n.trim().to_string()).collect();
                snaps
                    .iter()
                    .filter(|s| {
                        name_set.contains(&s.display_name) || name_set.contains(&s.internal_name)
                    })
                    .cloned()
                    .collect()
            } else if let Some(prefix) = args["prefix"].as_str() {
                snaps
                    .iter()
                    .filter(|s| s.display_name.starts_with(prefix))
                    .cloned()
                    .collect()
            } else if let Some(older_than) = args["older_than"].as_str() {
                let cutoff = NaiveDateTime::parse_from_str(
                    &format!("{older_than} 00:00:00"),
                    "%Y-%m-%d %H:%M:%S",
                )
                .or_else(|_| NaiveDateTime::parse_from_str(older_than, "%Y-%m-%dT%H:%M:%S"))
                .map_err(|_| {
                    MemoriaError::Validation("older_than must be ISO date e.g. '2026-03-01'".into())
                })?;
                snaps
                    .iter()
                    .filter(|s| s.timestamp.map(|t| t < cutoff).unwrap_or(false))
                    .cloned()
                    .collect()
            } else {
                return Ok(mcp_text("Specify 'names', 'prefix', or 'older_than'"));
            };

            let count = to_delete.len();
            for snapshot in &to_delete {
                git.drop_snapshot(&snapshot.internal_name)
                    .await
                    .map_err(git_err)?;
                if snapshot.registered {
                    sql.deregister_snapshot_by_internal(user_id, &snapshot.internal_name)
                        .await?;
                }
            }
            let display: Vec<_> = to_delete.iter().map(|s| s.display_name.clone()).collect();
            Ok(mcp_text(&format!(
                "Deleted {count} snapshot(s): {}",
                display.join(", ")
            )))
        }

        "memory_rollback" => {
            let sql = snapshot_store(svc, user_id).await?;
            let git = git_for_store(&sql)?;
            let snap_name = args["name"].as_str().unwrap_or("");
            let internal = resolve_snapshot_for_user(svc, user_id, snap_name)
                .await?
                .ok_or_else(|| MemoriaError::NotFound(format!("Snapshot '{snap_name}'")))?;
            // Restore mem_memories (required) + graph tables (best-effort, like Python)
            git.restore_table_from_snapshot("mem_memories", &internal)
                .await
                .map_err(|e| MemoriaError::Internal(format!("Rollback failed: {e}")))?;
            for table in &["memory_graph_nodes", "memory_graph_edges", "mem_edit_log"] {
                let _ = git.restore_table_from_snapshot(table, &internal).await;
            }
            sql.invalidate_user_caches(user_id).await;
            Ok(mcp_text(&format!("Rolled back to snapshot '{snap_name}'")))
        }

        "memory_branch" => {
            let branch_name = args["name"].as_str().unwrap_or("");
            let from_snapshot = args["from_snapshot"].as_str();
            let from_timestamp = args["from_timestamp"].as_str();

            if from_snapshot.is_some() && from_timestamp.is_some() {
                return Ok(mcp_text(
                    "Specify from_snapshot or from_timestamp, not both.",
                ));
            }

            // from_timestamp validation: must be within last 30 minutes, not future
            if let Some(ts_str) = from_timestamp {
                let ts =
                    NaiveDateTime::parse_from_str(ts_str, "%Y-%m-%d %H:%M:%S").map_err(|_| {
                        MemoriaError::Validation(
                            "from_timestamp must be 'YYYY-MM-DD HH:MM:SS'".into(),
                        )
                    })?;
                let now = chrono::Utc::now().naive_utc();
                if ts > now {
                    return Ok(mcp_text("from_timestamp cannot be in the future"));
                }
                if now - ts > chrono::Duration::minutes(30) {
                    return Ok(mcp_text(
                        "from_timestamp must be within the last 30 minutes",
                    ));
                }
            }

            let sql = svc.user_sql_store(user_id).await?;
            let git = git_for_store(&sql)?;

            // Global branch limit
            let all_branches = sql.list_branches(user_id).await?;
            if all_branches.len() as i64 >= MAX_BRANCHES {
                return Ok(mcp_text(&format!(
                    "Branch limit reached ({MAX_BRANCHES}). Delete old branches first."
                )));
            }

            // Duplicate name check (including deleted)
            let dup = sqlx::query(
                "SELECT COUNT(*) as cnt FROM mem_branches WHERE user_id = ? AND name = ?",
            )
            .bind(user_id)
            .bind(branch_name)
            .fetch_one(sql.pool())
            .await
            .map_err(db_err)?;
            let cnt: i64 = dup.try_get("cnt").unwrap_or(0);
            if cnt > 0 {
                return Ok(mcp_text(&format!("Branch '{branch_name}' already exists.")));
            }

            let safe = sanitize_name(branch_name);
            let table_name = format!("br_{}_{}", &Uuid::new_v4().simple().to_string()[..8], safe);

            if let Some(snap) = from_snapshot {
                // Create branch from snapshot: restore snapshot to temp, then branch
                let internal = resolve_snapshot_for_user(svc, user_id, snap)
                    .await?
                    .ok_or_else(|| MemoriaError::NotFound(format!("Snapshot '{snap}'")))?;
                git.create_branch_from_snapshot(&table_name, "mem_memories", &internal)
                    .await
                    .map_err(git_err)?;
            } else {
                git.create_branch(&table_name, "mem_memories")
                    .await
                    .map_err(git_err)?;
            }
            sql.register_branch(user_id, branch_name, &table_name)
                .await?;
            Ok(mcp_text(&format!("Created branch '{branch_name}'")))
        }

        "memory_branches" => {
            let branches = match svc.user_sql_store(user_id).await {
                Ok(sql) => sql.list_branches(user_id).await?,
                Err(_) => vec![],
            };
            let active = match svc.user_sql_store(user_id).await {
                Ok(sql) => sql
                    .active_table(user_id)
                    .await
                    .unwrap_or_else(|_| "mem_memories".to_string()),
                Err(_) => "mem_memories".to_string(),
            };
            if branches.is_empty() {
                return Ok(mcp_text("No branches. On main."));
            }
            let text = branches
                .iter()
                .map(|(name, table)| {
                    let marker = if *table == active { " ← active" } else { "" };
                    format!("{name}{marker}")
                })
                .collect::<Vec<_>>()
                .join("\n");
            Ok(mcp_text(&format!("Branches:\nmain\n{text}")))
        }

        "memory_checkout" => {
            let branch = args["name"].as_str().unwrap_or("main");
            let sql = svc.user_sql_store(user_id).await?;
            if branch == "main" {
                sql.set_active_branch(user_id, "main").await?;
                return Ok(mcp_text("Switched to branch 'main'"));
            }
            let branches = sql.list_branches(user_id).await?;
            if !branches.iter().any(|(name, _)| name == branch) {
                return Err(MemoriaError::NotFound(format!("Branch '{branch}'")));
            }
            sql.set_active_branch(user_id, branch).await?;
            let count = svc.list_active(user_id, 50).await?.len();
            Ok(mcp_text(&format!(
                "Switched to branch '{branch}'. {count} memories on this branch."
            )))
        }

        "memory_merge" => {
            let source_branch = args["source"].as_str().unwrap_or("");
            let strategy = args["strategy"].as_str().unwrap_or("accept");
            let strategy = match strategy {
                "append" => "append",
                "replace" | "accept" => "replace",
                other => {
                    return Err(MemoriaError::Validation(format!(
                        "Unsupported merge strategy '{other}'. Use append, replace, or accept."
                    )));
                }
            };
            let sql = svc.user_sql_store(user_id).await?;
            let git = git_for_store(&sql)?;
            let branches = sql.list_branches(user_id).await?;
            let table_name = branches
                .iter()
                .find(|(name, _)| name == source_branch)
                .map(|(_, t)| t.clone())
                .ok_or_else(|| MemoriaError::NotFound(format!("Branch '{source_branch}'")))?;
            let table_name = validate_identifier(&table_name)?.to_string();

            // Safety limit: count new memories (branch rows not in main by PK)
            let count_sql = format!(
                "SELECT COUNT(*) as cnt FROM {table_name} b WHERE b.user_id = ? \
                 AND NOT EXISTS (SELECT 1 FROM mem_memories m WHERE m.memory_id = b.memory_id)"
            );
            let new_count: i64 = sqlx::query(&count_sql)
                .bind(user_id)
                .fetch_one(sql.pool())
                .await
                .map_err(db_err)?
                .try_get("cnt")
                .unwrap_or(0);
            if new_count > 5000 {
                return Ok(mcp_text(&format!(
                    "Too many changes ({new_count}). Max 5000. Reduce branch scope."
                )));
            }

            // Cosine 0.9 → L2 for normalized vectors: sqrt(2*(1-0.9)) ≈ 0.4472
            // Uses l2_distance instead of cosine_similarity to leverage IVF vector_l2_ops index.
            const L2_CONFLICT: f64 = 0.4472;

            if strategy != "replace" {
                // append currently means "native append-only branch merge with skip-on-conflict".
                // MatrixOne v1.3.0 passes the current API/MCP regression suite with this path.
                //
                // Important semantic boundary:
                // - this is not a full git-style reconcile/three-way merge;
                // - native merge may also carry branch-only inactive rows (is_active = 0) into main
                //   when their PK does not already exist there.
                //
                // We intentionally keep the native behavior here for now and leave richer
                // reconcile/delete-propagation semantics for a future strategy.
                if new_count > 0 {
                    git.merge_branch(&table_name, "mem_memories")
                        .await
                        .map_err(|e| MemoriaError::Internal(format!("merge failed: {e}")))?;
                }
                return Ok(mcp_text(&format!(
                    "Merged branch '{source_branch}' into main ({new_count} new, 0 replaced, 0 skipped)"
                )));
            }

            // replace strategy: SQL merge with cosine conflict detection
            // Single-pass INSERT using OR short-circuit to avoid cosine on null/empty embeddings
            let insert_sql = format!(
                "INSERT INTO mem_memories \
                    (memory_id, user_id, memory_type, content, embedding, session_id, \
                     source_event_ids, extra_metadata, is_active, superseded_by, \
                     trust_tier, initial_confidence, observed_at, created_at, updated_at) \
                 SELECT b.memory_id, b.user_id, b.memory_type, b.content, b.embedding, b.session_id, \
                     b.source_event_ids, b.extra_metadata, b.is_active, b.superseded_by, \
                     b.trust_tier, b.initial_confidence, b.observed_at, b.created_at, b.updated_at \
                 FROM {table_name} b \
                 WHERE b.user_id = ? AND b.is_active = 1 \
                   AND NOT EXISTS (SELECT 1 FROM mem_memories m WHERE m.memory_id = b.memory_id) \
                   AND ( \
                     b.embedding IS NULL OR vector_dims(b.embedding) = 0 \
                     OR NOT EXISTS ( \
                       SELECT 1 FROM mem_memories m \
                       WHERE m.user_id = ? AND m.is_active = 1 \
                         AND m.embedding IS NOT NULL AND vector_dims(m.embedding) > 0 \
                         AND m.memory_type = b.memory_type \
                         AND l2_distance(m.embedding, b.embedding) < {L2_CONFLICT} \
                     ) \
                   )"
            );
            let inserted = sqlx::query(&insert_sql)
                .bind(user_id)
                .bind(user_id)
                .execute(sql.pool())
                .await
                .map_err(db_err)?
                .rows_affected();

            let replacements =
                collect_replace_candidates(sql.pool(), &table_name, user_id, L2_CONFLICT).await?;
            let (replaced, skipped) = if !replacements.is_empty() {
                let mut tx = sql.pool().begin().await.map_err(db_err)?;
                for candidate in &replacements {
                    sqlx::query(
                        "UPDATE mem_memories SET content = ?, updated_at = NOW() WHERE memory_id = ?",
                    )
                    .bind(&candidate.replacement_content)
                    .bind(&candidate.main_memory_id)
                    .execute(&mut *tx)
                    .await
                    .map_err(db_err)?;
                }
                tx.commit().await.map_err(db_err)?;
                (replacements.len() as u64, 0u64)
            } else {
                (0u64, 0u64)
            };

            Ok(mcp_text(&format!(
                "Merged branch '{source_branch}' into main ({inserted} new, {replaced} replaced, {skipped} skipped)"
            )))
        }

        "memory_branch_delete" => {
            let branch = args["name"].as_str().unwrap_or("");
            if branch == "main" {
                return Ok(mcp_text("Cannot delete main"));
            }
            let sql = svc.user_sql_store(user_id).await?;
            let git = git_for_store(&sql)?;
            let branches = sql.list_branches(user_id).await?;
            if let Some((_, table_name)) = branches.iter().find(|(name, _)| name == branch) {
                git.drop_branch(table_name).await.map_err(git_err)?;
                sql.deregister_branch(user_id, branch).await?;
                let active_table = sql.active_table(user_id).await.unwrap_or_default();
                if active_table == *table_name {
                    sql.set_active_branch(user_id, "main").await?;
                }
                Ok(mcp_text(&format!("Deleted branch '{branch}'")))
            } else {
                Ok(mcp_text(&format!("Branch '{branch}' not found")))
            }
        }

        "memory_diff" => {
            let source_branch = args["source"].as_str().unwrap_or("");
            let limit = args["limit"].as_i64().unwrap_or(50);
            let sql = svc.user_sql_store(user_id).await?;
            let git = git_for_store(&sql)?;
            let branches = sql.list_branches(user_id).await?;
            let table_name = branches
                .iter()
                .find(|(name, _)| name == source_branch)
                .map(|(_, t)| t.clone())
                .ok_or_else(|| MemoriaError::NotFound(format!("Branch '{source_branch}'")))?;

            // Use native LCA-based diff count, SQL JOIN for row details
            // (native diff output limit returns unknown column types that sqlx can't decode)
            let total = git
                .diff_branch_count(&table_name, "mem_memories", user_id)
                .await
                .unwrap_or(0);
            if total == 0 {
                return Ok(mcp_text(&format!(
                    "No changes in branch '{source_branch}' vs main."
                )));
            }
            let rows = git
                .diff_branch_rows(&table_name, "mem_memories", user_id, limit)
                .await
                .map_err(git_err)?;
            let lines: Vec<String> = rows
                .iter()
                .map(|r| {
                    let semantic = match r.flag.as_str() {
                        "INSERT" => "new",
                        "UPDATE" => "modified",
                        "DELETE" => "removed",
                        other => other,
                    };
                    let preview = if r.content.len() > 80 {
                        format!("{}...", &r.content[..80])
                    } else {
                        r.content.clone()
                    };
                    format!(
                        "[{semantic}] {}: {preview}",
                        &r.memory_id[..8.min(r.memory_id.len())]
                    )
                })
                .collect();
            let truncated = if total > limit {
                format!(" (showing {limit}/{total})")
            } else {
                String::new()
            };
            Ok(mcp_text(&format!(
                "Diff '{source_branch}' vs main{truncated}:\n{}",
                lines.join("\n")
            )))
        }

        _ => Err(MemoriaError::NotFound(format!("Unknown git tool: {name}"))),
    }
}

async fn collect_replace_candidates(
    pool: &sqlx::MySqlPool,
    table_name: &str,
    user_id: &str,
    l2_conflict: f64,
) -> Result<Vec<ReplaceCandidate>, MemoriaError> {
    let table_name = validate_identifier(table_name)?;
    let sql = format!(
        "SELECT m.memory_id AS main_memory_id, \
                b.memory_id AS branch_memory_id, \
                b.content AS replacement_content, \
                CAST(l2_distance(m.embedding, b.embedding) AS DOUBLE) AS conflict_distance \
         FROM mem_memories m \
         JOIN {table_name} b \
           ON b.user_id = ? AND b.is_active = 1 \
          AND b.content IS NOT NULL \
          AND b.memory_type = m.memory_type \
          AND b.embedding IS NOT NULL AND vector_dims(b.embedding) > 0 \
         WHERE m.user_id = ? AND m.is_active = 1 \
           AND m.embedding IS NOT NULL AND vector_dims(m.embedding) > 0 \
           AND NOT EXISTS (SELECT 1 FROM mem_memories m2 WHERE m2.memory_id = b.memory_id AND m2.is_active = 1) \
           AND l2_distance(m.embedding, b.embedding) < {l2_conflict}"
    );
    let rows = sqlx::query(&sql)
        .bind(user_id)
        .bind(user_id)
        .fetch_all(pool)
        .await
        .map_err(db_err)?;

    let mut chosen: HashMap<String, ReplaceCandidate> = HashMap::new();
    for row in rows {
        let candidate = ReplaceCandidate {
            main_memory_id: row.try_get("main_memory_id").map_err(db_err)?,
            branch_memory_id: row.try_get("branch_memory_id").map_err(db_err)?,
            replacement_content: row.try_get("replacement_content").map_err(db_err)?,
            conflict_distance: row.try_get("conflict_distance").map_err(db_err)?,
        };
        match chosen.get_mut(&candidate.main_memory_id) {
            Some(current)
                if candidate.conflict_distance < current.conflict_distance
                    || (candidate.conflict_distance == current.conflict_distance
                        && candidate.branch_memory_id < current.branch_memory_id) =>
            {
                *current = candidate;
            }
            None => {
                chosen.insert(candidate.main_memory_id.clone(), candidate);
            }
            _ => {}
        }
    }

    let mut replacements = chosen.into_values().collect::<Vec<_>>();
    replacements.sort_by(|a, b| a.main_memory_id.cmp(&b.main_memory_id));
    Ok(replacements)
}

fn mcp_text(text: &str) -> Value {
    json!({"content": [{"type": "text", "text": text}]})
}

#[cfg(test)]
mod tests {
    use super::{safety_prefix, snap_internal, validate_identifier, MAX_IDENTIFIER_LEN};

    #[test]
    fn scoped_snapshot_internal_names_stay_within_matrixone_limit() {
        let internal = snap_internal(
            "memoria_shared_db_with_a_really_long_name_for_product_runs",
            "snapshot_name_that_is_long_but_should_still_fit_with_db_scope",
        );
        assert!(internal.len() <= MAX_IDENTIFIER_LEN, "{internal}");
    }

    #[test]
    fn scoped_safety_prefix_stays_bounded() {
        let prefix = safety_prefix(Some(
            "memoria_shared_db_with_a_really_long_name_for_product_runs",
        ));
        assert!(prefix.len() < MAX_IDENTIFIER_LEN, "{prefix}");
    }

    #[test]
    fn validate_identifier_accepts_safe_names() {
        assert_eq!(validate_identifier("br_valid_123").unwrap(), "br_valid_123");
    }

    #[test]
    fn validate_identifier_rejects_unsafe_names() {
        assert!(validate_identifier("br_bad-name").is_err());
        assert!(validate_identifier("br bad").is_err());
        assert!(validate_identifier("br`bad").is_err());
    }
}
