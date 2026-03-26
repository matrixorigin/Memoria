//! Streamable HTTP MCP endpoint.
//!
//! Exposes `POST /mcp` so AI clients (Cursor, Claude, etc.) can reach Memoria
//! without installing a local binary — just a URL + Bearer token:
//!
//! ```json
//! {
//!   "mcpServers": {
//!     "memoria": {
//!       "url": "https://memoria-host:8100/mcp",
//!       "headers": { "Authorization": "Bearer sk-..." }
//!     }
//!   }
//! }
//! ```
//!
//! Auth is handled by the existing `AuthUser` extractor (Bearer → SHA-256 →
//! `mem_api_keys` DB lookup with cache + rate limiting).  Tool dispatch reuses
//! `memoria_mcp::dispatch_http` which drives `Mode::Embedded` internally.

use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};
use serde_json::json;

use crate::{auth::AuthUser, state::AppState};

pub async fn mcp_handler(
    State(state): State<AppState>,
    auth: AuthUser,
    body: String,
) -> impl IntoResponse {
    let req: serde_json::Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(e) => {
            return Json(json!({
                "jsonrpc": "2.0",
                "id": null,
                "error": {"code": -32700, "message": e.to_string()}
            }))
            .into_response()
        }
    };

    // Per JSON-RPC 2.0 §4, a Request object MUST be a JSON object.
    // Anything else (array, string, number, …) is Invalid Request.
    if !req.is_object() {
        return Json(json!({
            "jsonrpc": "2.0",
            "id": null,
            "error": {"code": -32600, "message": "Invalid Request: payload must be a JSON object"}
        }))
        .into_response();
    }

    // "jsonrpc" MUST equal exactly "2.0".
    if req.get("jsonrpc").and_then(|v| v.as_str()) != Some("2.0") {
        return Json(json!({
            "jsonrpc": "2.0",
            "id": req.get("id").cloned().unwrap_or(serde_json::Value::Null),
            "error": {"code": -32600, "message": "Invalid Request: jsonrpc must be \"2.0\""}
        }))
        .into_response();
    }

    // "method" MUST be a non-empty string.
    let method = match req.get("method").and_then(|v| v.as_str()) {
        Some(m) if !m.is_empty() => m.to_string(),
        _ => {
            return Json(json!({
                "jsonrpc": "2.0",
                "id": req.get("id").cloned().unwrap_or(serde_json::Value::Null),
                "error": {"code": -32600, "message": "Invalid Request: method must be a non-empty string"}
            }))
            .into_response();
        }
    };

    let params = req.get("params").cloned();

    // JSON-RPC 2.0: a Notification is a *valid* Request without an "id" member.
    // The server MUST NOT reply to Notifications.
    if req.get("id").is_none() {
        let _ = memoria_mcp::dispatch_http(
            &method,
            params,
            &state.service,
            &state.git,
            &auth.user_id,
        )
        .await;
        return StatusCode::NO_CONTENT.into_response();
    }

    let id = req["id"].clone();

    match memoria_mcp::dispatch_http(&method, params, &state.service, &state.git, &auth.user_id)
        .await
    {
        Ok(v) => {
            let result = if v.is_null() { json!({}) } else { v };
            Json(json!({"jsonrpc": "2.0", "id": id, "result": result})).into_response()
        }
        Err(e) => Json(json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": e.code, "message": e.message}
        }))
        .into_response(),
    }
}
