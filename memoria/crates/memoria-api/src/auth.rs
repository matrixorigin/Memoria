//! Bearer token auth extractor.
//! Extracts user_id from X-User-Id header (or query param).
//! Validates Bearer token against master key.

use axum::{
    extract::FromRequestParts,
    http::{request::Parts, StatusCode},
};
use serde::Deserialize;

use crate::state::AppState;

pub struct AuthUser(pub String);

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
        // Validate Bearer token if master_key is set
        if !state.master_key.is_empty() {
            let auth = parts
                .headers
                .get("Authorization")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            if !auth.starts_with("Bearer ") {
                return Err((StatusCode::UNAUTHORIZED, "Missing Bearer token".to_string()));
            }
            let token = &auth[7..];
            if token != state.master_key {
                return Err((StatusCode::UNAUTHORIZED, "Invalid token".to_string()));
            }
        }

        // Extract user_id from X-User-Id header, X-Impersonate-User, then query param, then default
        let user_id = parts
            .headers
            .get("X-User-Id")
            .or_else(|| parts.headers.get("X-Impersonate-User"))
            .and_then(|v| v.to_str().ok())
            .map(String::from)
            .or_else(|| {
                // Try query param
                let uri = parts.uri.query().unwrap_or("");
                serde_urlencoded::from_str::<UserQuery>(uri)
                    .ok()
                    .and_then(|q| q.user_id)
            })
            .unwrap_or_else(|| "default".to_string());

        Ok(AuthUser(user_id))
    }
}
