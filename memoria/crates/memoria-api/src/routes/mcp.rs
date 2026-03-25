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

    // JSON-RPC 2.0: a Notification is a Request without an "id" member.
    // The server MUST NOT reply to Notifications.
    if req.get("id").is_none() {
        let method = req["method"].as_str().unwrap_or("").to_string();
        let params = req.get("params").cloned();
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
    let method = req["method"].as_str().unwrap_or("").to_string();
    let params = req.get("params").cloned();

    match memoria_mcp::dispatch_http(&method, params, &state.service, &state.git, &auth.user_id)
        .await
    {
        Ok(v) => Json(json!({"jsonrpc": "2.0", "id": id, "result": v})).into_response(),
        Err(e) => Json(json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": e.code, "message": e.message}
        }))
        .into_response(),
    }
}
