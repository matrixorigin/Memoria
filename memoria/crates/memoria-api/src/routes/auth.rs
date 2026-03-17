//! API key management: POST/GET/DELETE /auth/keys, PUT /auth/keys/:id/rotate
//! Master key required for create. Users can list/revoke their own keys.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};
use sqlx::Row;

use crate::{auth::AuthUser, routes::memory::api_err, state::AppState};

// ── DDL ───────────────────────────────────────────────────────────────────────

pub async fn migrate_api_keys(pool: &sqlx::MySqlPool) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"CREATE TABLE IF NOT EXISTS mem_api_keys (
            key_id      VARCHAR(36)  NOT NULL,
            user_id     VARCHAR(64)  NOT NULL,
            name        VARCHAR(100) NOT NULL,
            key_hash    VARCHAR(64)  NOT NULL,
            key_prefix  VARCHAR(12)  NOT NULL,
            is_active   TINYINT(1)   NOT NULL DEFAULT 1,
            created_at  DATETIME(6)  NOT NULL,
            expires_at  DATETIME(6)  DEFAULT NULL,
            last_used_at DATETIME(6) DEFAULT NULL,
            PRIMARY KEY (key_id),
            KEY idx_key_hash (key_hash),
            KEY idx_user_active (user_id, is_active)
        )"#,
    )
    .execute(pool)
    .await?;
    Ok(())
}

// ── Key generation ────────────────────────────────────────────────────────────

fn generate_key() -> (String, String, String) {
    use sha2::{Sha256, Digest};
    let raw = format!("sk-{}", uuid::Uuid::new_v4().simple());
    let prefix = raw[..12].to_string();
    let hash = format!("{:x}", Sha256::digest(raw.as_bytes()));
    (raw, hash, prefix)
}

// ── Request / Response ────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct CreateKeyRequest {
    pub user_id: String,
    pub name: String,
    pub expires_at: Option<String>,
}

#[derive(Serialize)]
pub struct KeyResponse {
    pub key_id: String,
    pub user_id: String,
    pub name: String,
    pub key_prefix: String,
    pub created_at: String,
    pub expires_at: Option<String>,
    pub last_used_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_key: Option<String>,
}

// ── Handlers ──────────────────────────────────────────────────────────────────

/// POST /auth/keys — create API key (master key required)
pub async fn create_key(
    State(state): State<AppState>,
    AuthUser(_): AuthUser, // master key validated by AuthUser extractor
    Json(req): Json<CreateKeyRequest>,
) -> Result<(StatusCode, Json<KeyResponse>), (StatusCode, String)> {
    let sql = state.service.sql_store.as_ref()
        .ok_or_else(|| (StatusCode::SERVICE_UNAVAILABLE, "SQL store required".to_string()))?;

    // Ensure table exists
    migrate_api_keys(sql.pool()).await.map_err(api_err)?;

    let (raw_key, key_hash, key_prefix) = generate_key();
    let key_id = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now().naive_utc();

    sqlx::query(
        "INSERT INTO mem_api_keys (key_id, user_id, name, key_hash, key_prefix, is_active, created_at, expires_at) \
         VALUES (?,?,?,?,?,1,?,?)"
    )
    .bind(&key_id).bind(&req.user_id).bind(&req.name)
    .bind(&key_hash).bind(&key_prefix).bind(now)
    .bind(req.expires_at.as_deref())
    .execute(sql.pool()).await.map_err(api_err)?;

    Ok((StatusCode::CREATED, Json(KeyResponse {
        key_id,
        user_id: req.user_id,
        name: req.name,
        key_prefix,
        created_at: now.to_string(),
        expires_at: req.expires_at,
        last_used_at: None,
        raw_key: Some(raw_key),
    })))
}

/// GET /auth/keys — list keys for current user
pub async fn list_keys(
    State(state): State<AppState>,
    AuthUser(user_id): AuthUser,
) -> Result<Json<Vec<KeyResponse>>, (StatusCode, String)> {
    let sql = state.service.sql_store.as_ref()
        .ok_or_else(|| (StatusCode::SERVICE_UNAVAILABLE, "SQL store required".to_string()))?;

    migrate_api_keys(sql.pool()).await.map_err(api_err)?;

    let rows = sqlx::query(
        "SELECT key_id, user_id, name, key_prefix, created_at, expires_at, last_used_at \
         FROM mem_api_keys WHERE user_id = ? AND is_active = 1 ORDER BY created_at DESC"
    )
    .bind(&user_id)
    .fetch_all(sql.pool()).await.map_err(api_err)?;

    let keys = rows.iter().map(|r| KeyResponse {
        key_id: r.try_get("key_id").unwrap_or_default(),
        user_id: r.try_get("user_id").unwrap_or_default(),
        name: r.try_get("name").unwrap_or_default(),
        key_prefix: r.try_get("key_prefix").unwrap_or_default(),
        created_at: r.try_get::<chrono::NaiveDateTime, _>("created_at").map(|d| d.to_string()).unwrap_or_default(),
        expires_at: r.try_get::<Option<chrono::NaiveDateTime>, _>("expires_at").ok().flatten().map(|d| d.to_string()),
        last_used_at: r.try_get::<Option<chrono::NaiveDateTime>, _>("last_used_at").ok().flatten().map(|d| d.to_string()),
        raw_key: None,
    }).collect();

    Ok(Json(keys))
}

/// GET /auth/keys/:id — get a single API key by ID
pub async fn get_key(
    State(state): State<AppState>,
    AuthUser(user_id): AuthUser,
    Path(key_id): Path<String>,
) -> Result<Json<KeyResponse>, (StatusCode, String)> {
    let sql = state.service.sql_store.as_ref()
        .ok_or_else(|| (StatusCode::SERVICE_UNAVAILABLE, "SQL store required".to_string()))?;
    migrate_api_keys(sql.pool()).await.map_err(api_err)?;

    let row = sqlx::query(
        "SELECT key_id, user_id, name, key_prefix, created_at, expires_at, last_used_at \
         FROM mem_api_keys WHERE key_id = ? AND is_active = 1"
    ).bind(&key_id).fetch_optional(sql.pool()).await.map_err(api_err)?;

    let r = row.ok_or_else(|| (StatusCode::NOT_FOUND, "Key not found".to_string()))?;
    let owner: String = r.try_get("user_id").unwrap_or_default();
    // Non-admin can only see own keys
    if user_id != "admin" && owner != user_id {
        return Err((StatusCode::FORBIDDEN, "Not your key".to_string()));
    }
    Ok(Json(KeyResponse {
        key_id: r.try_get("key_id").unwrap_or_default(),
        user_id: owner,
        name: r.try_get("name").unwrap_or_default(),
        key_prefix: r.try_get("key_prefix").unwrap_or_default(),
        created_at: r.try_get::<chrono::NaiveDateTime, _>("created_at").map(|d| d.to_string()).unwrap_or_default(),
        expires_at: r.try_get::<Option<chrono::NaiveDateTime>, _>("expires_at").ok().flatten().map(|d| d.to_string()),
        last_used_at: r.try_get::<Option<chrono::NaiveDateTime>, _>("last_used_at").ok().flatten().map(|d| d.to_string()),
        raw_key: None,
    }))
}

/// PUT /auth/keys/:id/rotate — revoke old key, issue new one
pub async fn rotate_key(
    State(state): State<AppState>,
    AuthUser(user_id): AuthUser,
    Path(key_id): Path<String>,
) -> Result<(StatusCode, Json<KeyResponse>), (StatusCode, String)> {
    let sql = state.service.sql_store.as_ref()
        .ok_or_else(|| (StatusCode::SERVICE_UNAVAILABLE, "SQL store required".to_string()))?;

    // Get old key
    let old = sqlx::query(
        "SELECT user_id, name, expires_at FROM mem_api_keys WHERE key_id = ? AND is_active = 1"
    )
    .bind(&key_id)
    .fetch_optional(sql.pool()).await.map_err(api_err)?
    .ok_or_else(|| (StatusCode::NOT_FOUND, "Key not found".to_string()))?;

    let old_user: String = old.try_get("user_id").map_err(api_err)?;
    // Only owner or master key can rotate
    if old_user != user_id && !state.master_key.is_empty() {
        // master key user can rotate any key
    } else if old_user != user_id {
        return Err((StatusCode::FORBIDDEN, "Not your key".to_string()));
    }

    let name: String = old.try_get("name").map_err(api_err)?;
    let expires_at: Option<chrono::NaiveDateTime> = old.try_get("expires_at").ok().flatten();

    // Deactivate old
    sqlx::query("UPDATE mem_api_keys SET is_active = 0 WHERE key_id = ?")
        .bind(&key_id).execute(sql.pool()).await.map_err(api_err)?;

    // Create new
    let (raw_key, key_hash, key_prefix) = generate_key();
    let new_id = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now().naive_utc();

    sqlx::query(
        "INSERT INTO mem_api_keys (key_id, user_id, name, key_hash, key_prefix, is_active, created_at, expires_at) \
         VALUES (?,?,?,?,?,1,?,?)"
    )
    .bind(&new_id).bind(&old_user).bind(&name)
    .bind(&key_hash).bind(&key_prefix).bind(now)
    .bind(expires_at)
    .execute(sql.pool()).await.map_err(api_err)?;

    Ok((StatusCode::CREATED, Json(KeyResponse {
        key_id: new_id,
        user_id: old_user,
        name,
        key_prefix,
        created_at: now.to_string(),
        expires_at: expires_at.map(|d| d.to_string()),
        last_used_at: None,
        raw_key: Some(raw_key),
    })))
}

/// DELETE /auth/keys/:id — revoke key
pub async fn revoke_key(
    State(state): State<AppState>,
    AuthUser(user_id): AuthUser,
    Path(key_id): Path<String>,
) -> Result<StatusCode, (StatusCode, String)> {
    let sql = state.service.sql_store.as_ref()
        .ok_or_else(|| (StatusCode::SERVICE_UNAVAILABLE, "SQL store required".to_string()))?;

    let row = sqlx::query("SELECT user_id FROM mem_api_keys WHERE key_id = ?")
        .bind(&key_id)
        .fetch_optional(sql.pool()).await.map_err(api_err)?
        .ok_or_else(|| (StatusCode::NOT_FOUND, "Key not found".to_string()))?;

    let owner: String = row.try_get("user_id").map_err(api_err)?;
    if owner != user_id && state.master_key.is_empty() {
        return Err((StatusCode::FORBIDDEN, "Not your key".to_string()));
    }

    sqlx::query("UPDATE mem_api_keys SET is_active = 0 WHERE key_id = ?")
        .bind(&key_id).execute(sql.pool()).await.map_err(api_err)?;

    Ok(StatusCode::NO_CONTENT)
}
