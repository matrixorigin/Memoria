//! Bearer token auth extractor.
//! Validates Bearer token against master key OR API key (sk-... hashed lookup).
//! When authenticated via API key, user_id is resolved from the key's owner.

use axum::{
    extract::FromRequestParts,
    http::{request::Parts, StatusCode},
};
use serde::Deserialize;
use sha2::{Sha256, Digest};
use sqlx::Row;
use subtle::ConstantTimeEq;
use tracing::warn;

use crate::state::AppState;

pub struct AuthUser {
    pub user_id: String,
    pub is_master: bool,
}

impl AuthUser {
    pub fn require_master(&self) -> Result<(), (StatusCode, String)> {
        if !self.is_master {
            Err((StatusCode::FORBIDDEN, "Master key required".to_string()))
        } else {
            Ok(())
        }
    }
}

#[derive(Deserialize)]
struct UserQuery {
    user_id: Option<String>,
}

#[axum::async_trait]
impl FromRequestParts<AppState> for AuthUser {
    type Rejection = (StatusCode, String);

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let bearer = parts
            .headers
            .get("Authorization")
            .and_then(|v| v.to_str().ok())
            .filter(|v| v.starts_with("Bearer "))
            .map(|v| &v[7..]);

        if let Some(token) = bearer {
            // 1) Master key — full access, fall through to X-User-Id extraction
            let master_match = !state.master_key.is_empty()
                && token.len() == state.master_key.len()
                && token.as_bytes().ct_eq(state.master_key.as_bytes()).into();
            if master_match {
                // fall through
            }
            // 2) API key — user_id resolved from DB, never master
            else if let Some(uid) = validate_api_key(token, state).await {
                return Ok(AuthUser { user_id: uid, is_master: false });
            } else {
                return Err((StatusCode::UNAUTHORIZED, "Invalid token".to_string()));
            }
        } else if !state.master_key.is_empty() {
            // master_key is configured but caller sent no Bearer token
            return Err((StatusCode::UNAUTHORIZED, "Missing Bearer token".to_string()));
        }
        // Reached here: master key validated, or no-auth open mode (master_key not configured)

        let user_id = parts
            .headers
            .get("X-User-Id")
            .or_else(|| parts.headers.get("X-Impersonate-User"))
            .and_then(|v| v.to_str().ok())
            .map(String::from)
            .or_else(|| {
                let uri = parts.uri.query().unwrap_or("");
                serde_urlencoded::from_str::<UserQuery>(uri)
                    .ok()
                    .and_then(|q| q.user_id)
            })
            .unwrap_or_else(|| "default".to_string());

        Ok(AuthUser { user_id, is_master: true })
    }
}

/// Hash the raw API key and look it up in mem_api_keys.
/// Returns Some(user_id) if valid, None otherwise.
async fn validate_api_key(token: &str, state: &AppState) -> Option<String> {
    let sql = state.service.sql_store.as_ref()?;
    let key_hash = format!("{:x}", Sha256::digest(token.as_bytes()));

    let row = sqlx::query(
        "SELECT user_id FROM mem_api_keys \
         WHERE key_hash = ? AND is_active = 1 \
         AND (expires_at IS NULL OR expires_at > NOW(6))"
    )
    .bind(&key_hash)
    .fetch_optional(sql.pool())
    .await
    .map_err(|e| warn!("validate_api_key: DB query failed: {e}"))
    .ok()??;

    // Update last_used_at (fire-and-forget)
    let pool = sql.pool().clone();
    let hash = key_hash.clone();
    tokio::spawn(async move {
        if let Err(e) = sqlx::query("UPDATE mem_api_keys SET last_used_at = NOW(6) WHERE key_hash = ?")
            .bind(&hash)
            .execute(&pool)
            .await
        {
            warn!("Failed to update API key last_used_at: {e}");
        }
    });

    row.try_get::<String, _>("user_id").ok()
}
