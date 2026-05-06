//! User-facing group management endpoints.
//! Any authenticated user can create groups, manage members (if owner), and delete groups.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use memoria_storage::DbRouter;
use serde::{Deserialize, Serialize};
use sqlx::{MySqlPool, Row};

use crate::{auth::AuthUser, routes::memory::api_err, state::AppState};

// ── Request / Response types ─────────────────────────────────────────

#[derive(Deserialize)]
pub struct CreateGroupRequest {
    pub group_name: String,
    /// Initial members (besides the creator). Creator is always added.
    #[serde(default)]
    pub members: Vec<String>,
    /// Custom database name. Auto-generated if omitted.
    pub db_name: Option<String>,
    #[serde(default)]
    pub seed: Option<CreateGroupSeedRequest>,
}

#[derive(Deserialize)]
pub struct CreateGroupSeedRequest {
    pub db_name: String,
    #[serde(default = "default_seed_mode")]
    pub mode: String,
}

#[derive(Serialize, Clone)]
pub struct GroupEntry {
    pub group_id: String,
    pub group_name: String,
    pub db_name: String,
    pub owner_user_id: String,
    pub members: Vec<String>,
    pub status: String,
    pub created_at: String,
    pub updated_at: String,
}

fn default_seed_mode() -> String {
    "active_only".to_string()
}

// ── Helpers ──────────────────────────────────────────────────────────

fn get_shared_pool(state: &AppState) -> Result<&MySqlPool, (StatusCode, String)> {
    state
        .auth_pool
        .as_ref()
        .or_else(|| state.service.sql_store.as_ref().map(|s| s.pool()))
        .ok_or((
            StatusCode::INTERNAL_SERVER_ERROR,
            "No shared SQL pool".into(),
        ))
}

fn make_group_id() -> String {
    format!("grp_{}", &uuid::Uuid::new_v4().simple().to_string()[..12])
}

fn default_group_db_name(group_id: &str) -> String {
    format!("mem_grp_{}", group_id.trim_start_matches("grp_"))
}

/// Load active member user_ids for a group from mem_group_members.
async fn load_group_member_ids(
    pool: &MySqlPool,
    group_id: &str,
) -> Result<Vec<String>, (StatusCode, String)> {
    let rows = sqlx::query(
        "SELECT user_id FROM mem_group_members WHERE group_id = ? AND is_active = 1 ORDER BY user_id",
    )
    .bind(group_id)
    .fetch_all(pool)
    .await
    .map_err(api_err)?;
    Ok(rows
        .iter()
        .map(|r| r.try_get::<String, _>("user_id").unwrap_or_default())
        .collect())
}

async fn load_group(
    pool: &MySqlPool,
    group_id: &str,
) -> Result<Option<GroupEntry>, (StatusCode, String)> {
    let row = sqlx::query(
        "SELECT group_id, group_name, db_name, owner_user_id, status, created_at, updated_at \
         FROM mem_groups WHERE group_id = ?",
    )
    .bind(group_id)
    .fetch_optional(pool)
    .await
    .map_err(api_err)?;
    match row {
        None => Ok(None),
        Some(row) => {
            let gid: String = row.try_get("group_id").map_err(api_err)?;
            let members = load_group_member_ids(pool, &gid).await?;
            Ok(Some(GroupEntry {
                group_id: gid,
                group_name: row.try_get("group_name").map_err(api_err)?,
                db_name: row.try_get("db_name").map_err(api_err)?,
                owner_user_id: row.try_get("owner_user_id").map_err(api_err)?,
                members,
                status: row.try_get("status").map_err(api_err)?,
                created_at: row
                    .try_get::<chrono::NaiveDateTime, _>("created_at")
                    .map_err(api_err)?
                    .to_string(),
                updated_at: row
                    .try_get::<chrono::NaiveDateTime, _>("updated_at")
                    .map_err(api_err)?
                    .to_string(),
            }))
        }
    }
}

/// Verify the caller is the owner of the group. Returns the loaded group on success.
async fn require_owner(
    pool: &MySqlPool,
    group_id: &str,
    user_id: &str,
) -> Result<GroupEntry, (StatusCode, String)> {
    let group = load_group(pool, group_id)
        .await?
        .ok_or((StatusCode::NOT_FOUND, "Group not found".into()))?;
    if group.owner_user_id != user_id {
        return Err((
            StatusCode::FORBIDDEN,
            "Only the group owner can perform this action".into(),
        ));
    }
    Ok(group)
}

async fn seed_database_exists(
    pool: &MySqlPool,
    db_name: &str,
) -> Result<bool, (StatusCode, String)> {
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM information_schema.schemata WHERE schema_name = ?",
    )
    .bind(db_name)
    .fetch_one(pool)
    .await
    .map_err(api_err)?;
    Ok(count > 0)
}

async fn current_scope_db_name(
    pool: &MySqlPool,
    router: &DbRouter,
    auth: &AuthUser,
) -> Result<String, (StatusCode, String)> {
    if auth.is_group_scoped() {
        return router.group_db_name(auth.scope_id()).await.map_err(api_err);
    }

    let row = sqlx::query(
        "SELECT db_name FROM mem_user_registry WHERE user_id = ? AND status = 'active'",
    )
    .bind(&auth.user_id)
    .fetch_optional(pool)
    .await
    .map_err(api_err)?;
    Ok(row
        .and_then(|row| row.try_get::<String, _>("db_name").ok())
        .unwrap_or_else(|| DbRouter::user_db_name_for_id(&auth.user_id)))
}

async fn seed_group_memories(
    router: &DbRouter,
    auth: &AuthUser,
    target_group_id: &str,
    target_db_name: &str,
    seed: &CreateGroupSeedRequest,
    pool: &MySqlPool,
) -> Result<usize, (StatusCode, String)> {
    if seed.db_name.trim().is_empty() {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            "seed.db_name must not be empty".into(),
        ));
    }
    if seed.mode != "active_only" {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            "seed.mode must be 'active_only'".into(),
        ));
    }
    if seed.db_name == target_db_name {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            "seed.db_name must differ from the target group db".into(),
        ));
    }

    let accessible_db_name = current_scope_db_name(pool, router, auth).await?;
    if seed.db_name != accessible_db_name {
        return Err((
            StatusCode::FORBIDDEN,
            "seed db is not accessible by the current key".into(),
        ));
    }
    if !seed_database_exists(pool, &seed.db_name).await? {
        return Err((StatusCode::NOT_FOUND, "seed database not found".into()));
    }

    let source_scope_id = if auth.is_group_scoped() {
        auth.scope_id().to_string()
    } else {
        auth.user_id.clone()
    };
    let source_store = router
        .routed_store_for_db_name(&seed.db_name)
        .map_err(api_err)?;
    let source_table = source_store.qualified_table("mem_memories");
    let target_store = router
        .routed_store_for_db_name(target_db_name)
        .map_err(api_err)?;
    let target_table = target_store.qualified_table("mem_memories");
    let fallback_author_id = if auth.is_group_scoped() {
        None
    } else {
        Some(auth.user_id.clone())
    };
    let result = sqlx::query(&format!(
        "INSERT INTO {target_table} \
         (memory_id, user_id, author_id, memory_type, content, embedding, session_id, \
          source_event_ids, extra_metadata, is_active, superseded_by, \
          trust_tier, initial_confidence, observed_at, created_at, updated_at) \
         SELECT memory_id, ?, COALESCE(author_id, ?), memory_type, content, embedding, session_id, \
                source_event_ids, extra_metadata, is_active, superseded_by, \
                trust_tier, initial_confidence, observed_at, created_at, NOW(6) \
         FROM {source_table} WHERE user_id = ? AND is_active = 1 \
         ORDER BY memory_id DESC"
    ))
    .bind(target_group_id)
    .bind(fallback_author_id)
    .bind(&source_scope_id)
    .execute(pool)
    .await
    .map_err(api_err)?;
    Ok(result.rows_affected() as usize)
}

async fn cleanup_failed_group_creation(pool: &MySqlPool, group_id: &str, db_name: &str) {
    let _ = sqlx::query("DELETE FROM mem_group_members WHERE group_id = ?")
        .bind(group_id)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM mem_groups WHERE group_id = ?")
        .bind(group_id)
        .execute(pool)
        .await;
    let safe_db = db_name.replace('`', "``");
    let _ = sqlx::query(&format!("DROP DATABASE IF EXISTS `{safe_db}`"))
        .execute(pool)
        .await;
}

// ── Handlers ─────────────────────────────────────────────────────────

/// POST /v1/groups — create a new group (caller becomes owner)
pub async fn create_group(
    auth: AuthUser,
    State(state): State<AppState>,
    Json(req): Json<CreateGroupRequest>,
) -> Result<(StatusCode, Json<GroupEntry>), (StatusCode, String)> {
    if req.group_name.trim().is_empty() {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            "group_name must not be empty".into(),
        ));
    }
    let pool = get_shared_pool(&state)?;
    let router = state
        .service
        .db_router
        .as_ref()
        .ok_or((StatusCode::SERVICE_UNAVAILABLE, "DB router required".into()))?;

    let group_id = make_group_id();
    let db_name = req
        .db_name
        .unwrap_or_else(|| default_group_db_name(&group_id));
    let now = chrono::Utc::now().naive_utc();

    let mut members = req.members;
    if !members.iter().any(|m| m == &auth.user_id) {
        members.push(auth.user_id.clone());
    }
    members.sort();
    members.dedup();

    // Validate all initial members (except creator, who may be new) are registered
    let non_creator: Vec<String> = members
        .iter()
        .filter(|m| *m != &auth.user_id)
        .cloned()
        .collect();
    if !non_creator.is_empty() {
        require_registered_users(pool, &non_creator).await?;
    }

    sqlx::raw_sql(&format!(
        "CREATE DATABASE IF NOT EXISTS `{}`",
        db_name.replace('`', "``")
    ))
    .execute(pool)
    .await
    .map_err(api_err)?;

    sqlx::query(
        "INSERT INTO mem_groups (group_id, group_name, db_name, owner_user_id, status, created_at, updated_at) \
         VALUES (?, ?, ?, ?, 'active', ?, ?)",
    )
    .bind(&group_id)
    .bind(&req.group_name)
    .bind(&db_name)
    .bind(&auth.user_id)
    .bind(now)
    .bind(now)
    .execute(pool)
    .await
    .map_err(api_err)?;

    // Insert members into mem_group_members
    for member in &members {
        let role = if member == &auth.user_id {
            "owner"
        } else {
            "member"
        };
        sqlx::query(
            "INSERT INTO mem_group_members (group_id, user_id, role, is_active, joined_at) \
             VALUES (?, ?, ?, 1, ?)",
        )
        .bind(&group_id)
        .bind(member)
        .bind(role)
        .bind(now)
        .execute(pool)
        .await
        .map_err(api_err)?;
    }

    let group_store = router.routed_store_for_db_name(&db_name).map_err(api_err)?;
    group_store.migrate_user().await.map_err(api_err)?;

    if let Some(seed) = req.seed.as_ref() {
        if let Err(err) = seed_group_memories(router, &auth, &group_id, &db_name, seed, pool).await
        {
            cleanup_failed_group_creation(pool, &group_id, &db_name).await;
            return Err(err);
        }
    }

    let group = load_group(pool, &group_id).await?.ok_or((
        StatusCode::INTERNAL_SERVER_ERROR,
        "group creation verification failed".into(),
    ))?;
    Ok((StatusCode::CREATED, Json(group)))
}

/// GET /v1/groups — list groups the caller belongs to
pub async fn list_my_groups(
    auth: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<Vec<GroupEntry>>, (StatusCode, String)> {
    let pool = get_shared_pool(&state)?;
    let rows = sqlx::query(
        "SELECT DISTINCT g.group_id, g.group_name, g.db_name, g.owner_user_id, \
                g.status, g.created_at, g.updated_at \
         FROM mem_groups g \
         JOIN mem_group_members m ON g.group_id = m.group_id \
         WHERE g.status = 'active' AND m.user_id = ? AND m.is_active = 1 \
         ORDER BY g.created_at DESC",
    )
    .bind(&auth.user_id)
    .fetch_all(pool)
    .await
    .map_err(api_err)?;

    let mut groups = Vec::with_capacity(rows.len());
    for row in &rows {
        let gid: String = row.try_get("group_id").map_err(api_err)?;
        let members = load_group_member_ids(pool, &gid).await?;
        groups.push(GroupEntry {
            group_id: gid,
            group_name: row.try_get("group_name").map_err(api_err)?,
            db_name: row.try_get("db_name").map_err(api_err)?,
            owner_user_id: row.try_get("owner_user_id").map_err(api_err)?,
            members,
            status: row.try_get("status").map_err(api_err)?,
            created_at: row
                .try_get::<chrono::NaiveDateTime, _>("created_at")
                .map_err(api_err)?
                .to_string(),
            updated_at: row
                .try_get::<chrono::NaiveDateTime, _>("updated_at")
                .map_err(api_err)?
                .to_string(),
        });
    }
    Ok(Json(groups))
}

/// GET /v1/groups/:group_id — get a group (must be a member)
pub async fn get_group(
    auth: AuthUser,
    State(state): State<AppState>,
    Path(group_id): Path<String>,
) -> Result<Json<GroupEntry>, (StatusCode, String)> {
    let pool = get_shared_pool(&state)?;
    let group = load_group(pool, &group_id)
        .await?
        .ok_or((StatusCode::NOT_FOUND, "Group not found".into()))?;
    if !group.members.iter().any(|m| m == &auth.user_id) {
        return Err((StatusCode::FORBIDDEN, "Not a member of this group".into()));
    }
    Ok(Json(group))
}

/// Verify that every user_id in `user_ids` exists in mem_user_registry with status = 'active'.
/// Returns an error naming the first unknown user.
async fn require_registered_users(
    pool: &MySqlPool,
    user_ids: &[String],
) -> Result<(), (StatusCode, String)> {
    for uid in user_ids {
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM mem_user_registry WHERE user_id = ? AND status = 'active'",
        )
        .bind(uid)
        .fetch_one(pool)
        .await
        .map_err(api_err)?;
        if count == 0 {
            return Err((
                StatusCode::UNPROCESSABLE_ENTITY,
                format!("user '{uid}' is not registered"),
            ));
        }
    }
    Ok(())
}

/// POST /v1/groups/:group_id/members/:user_id — invite/add a member (owner only)
pub async fn add_member(
    auth: AuthUser,
    State(state): State<AppState>,
    Path((group_id, target_user_id)): Path<(String, String)>,
) -> Result<Json<GroupEntry>, (StatusCode, String)> {
    let pool = get_shared_pool(&state)?;
    let _group = require_owner(pool, &group_id, &auth.user_id).await?;

    require_registered_users(pool, std::slice::from_ref(&target_user_id)).await?;

    // Check if already an active member
    let existing: Option<(i8,)> = sqlx::query_as(
        "SELECT is_active FROM mem_group_members WHERE group_id = ? AND user_id = ?",
    )
    .bind(&group_id)
    .bind(&target_user_id)
    .fetch_optional(pool)
    .await
    .map_err(api_err)?;

    let now = chrono::Utc::now().naive_utc();
    match existing {
        Some((1,)) => {
            return Err((
                StatusCode::CONFLICT,
                format!("{target_user_id} is already a member"),
            ));
        }
        Some(_) => {
            // Re-activate previously removed member
            sqlx::query(
                "UPDATE mem_group_members SET is_active = 1, removed_at = NULL, joined_at = ? \
                 WHERE group_id = ? AND user_id = ?",
            )
            .bind(now)
            .bind(&group_id)
            .bind(&target_user_id)
            .execute(pool)
            .await
            .map_err(api_err)?;
        }
        None => {
            sqlx::query(
                "INSERT INTO mem_group_members (group_id, user_id, role, is_active, joined_at) \
                 VALUES (?, ?, 'member', 1, ?)",
            )
            .bind(&group_id)
            .bind(&target_user_id)
            .bind(now)
            .execute(pool)
            .await
            .map_err(api_err)?;
        }
    }

    sqlx::query("UPDATE mem_groups SET updated_at = ? WHERE group_id = ?")
        .bind(now)
        .bind(&group_id)
        .execute(pool)
        .await
        .map_err(api_err)?;

    let group = load_group(pool, &group_id).await?.ok_or((
        StatusCode::INTERNAL_SERVER_ERROR,
        "group update failed".into(),
    ))?;
    Ok(Json(group))
}

/// DELETE /v1/groups/:group_id/members/:user_id — remove a member (owner only)
pub async fn remove_member(
    auth: AuthUser,
    State(state): State<AppState>,
    Path((group_id, target_user_id)): Path<(String, String)>,
) -> Result<Json<GroupEntry>, (StatusCode, String)> {
    let pool = get_shared_pool(&state)?;
    let group = require_owner(pool, &group_id, &auth.user_id).await?;

    if target_user_id == group.owner_user_id {
        return Err((
            StatusCode::FORBIDDEN,
            "Cannot remove the group owner".into(),
        ));
    }

    let now = chrono::Utc::now().naive_utc();

    // Soft-delete from mem_group_members
    sqlx::query(
        "UPDATE mem_group_members SET is_active = 0, removed_at = ? \
         WHERE group_id = ? AND user_id = ? AND is_active = 1",
    )
    .bind(now)
    .bind(&group_id)
    .bind(&target_user_id)
    .execute(pool)
    .await
    .map_err(api_err)?;

    sqlx::query("UPDATE mem_groups SET updated_at = ? WHERE group_id = ?")
        .bind(now)
        .bind(&group_id)
        .execute(pool)
        .await
        .map_err(api_err)?;

    // Deactivate & invalidate keys for the removed user
    let key_rows = sqlx::query(
        "SELECT key_hash FROM mem_api_keys WHERE group_id = ? AND user_id = ? AND is_active = 1",
    )
    .bind(&group_id)
    .bind(&target_user_id)
    .fetch_all(pool)
    .await
    .map_err(api_err)?;
    for row in &key_rows {
        if let Ok(key_hash) = row.try_get::<String, _>("key_hash") {
            state.api_key_cache.invalidate(&key_hash);
        }
    }
    sqlx::query(
        "UPDATE mem_api_keys SET is_active = 0 WHERE group_id = ? AND user_id = ? AND is_active = 1",
    )
    .bind(&group_id)
    .bind(&target_user_id)
    .execute(pool)
    .await
    .map_err(api_err)?;

    let group = load_group(pool, &group_id).await?.ok_or((
        StatusCode::INTERNAL_SERVER_ERROR,
        "group update failed".into(),
    ))?;
    Ok(Json(group))
}

/// DELETE /v1/groups/:group_id — soft-delete a group (owner only)
pub async fn delete_group(
    auth: AuthUser,
    State(state): State<AppState>,
    Path(group_id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let pool = get_shared_pool(&state)?;
    let group = require_owner(pool, &group_id, &auth.user_id).await?;

    let now = chrono::Utc::now().naive_utc();
    sqlx::query("UPDATE mem_groups SET status = 'deleted', updated_at = ? WHERE group_id = ?")
        .bind(now)
        .bind(&group_id)
        .execute(pool)
        .await
        .map_err(api_err)?;

    // Deactivate all API keys for this group & invalidate cache
    let key_rows =
        sqlx::query("SELECT key_hash FROM mem_api_keys WHERE group_id = ? AND is_active = 1")
            .bind(&group_id)
            .fetch_all(pool)
            .await
            .map_err(api_err)?;
    for row in &key_rows {
        if let Ok(key_hash) = row.try_get::<String, _>("key_hash") {
            state.api_key_cache.invalidate(&key_hash);
        }
    }
    sqlx::query("UPDATE mem_api_keys SET is_active = 0 WHERE group_id = ? AND is_active = 1")
        .bind(&group_id)
        .execute(pool)
        .await
        .map_err(api_err)?;

    // Drop the group database
    let safe_db = group.db_name.replace('`', "``");
    sqlx::query(&format!("DROP DATABASE IF EXISTS `{safe_db}`"))
        .execute(pool)
        .await
        .map_err(api_err)?;

    Ok(Json(serde_json::json!({
        "group_id": group_id,
        "status": "deleted",
    })))
}
