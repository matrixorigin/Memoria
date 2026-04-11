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

use crate::{
    auth::{AuthUser, RpcMeta},
    state::AppState,
};

/// Derive a stable tracking path from the JSON-RPC method + params so that
/// Streamable HTTP calls appear alongside the existing `/v1/*` entries in
/// `mem_api_call_log` and surface correctly in the Usage / Monitor dashboards.
///
/// Convention:
///   `tools/call` → `/mcp/<tool_name>`   (e.g. `/mcp/memory_store`)
///   anything else → `/mcp/<method>`     (e.g. `/mcp/initialize`, `/mcp/tools.list`)
///
/// The tool name comes from client-supplied `params.name`, so it is sanitized:
///   - only ASCII alphanumerics and `_` are kept (no `/`, `.`, `..`, spaces, etc.)
///   - clamped to 64 chars to bound DB column width
///   - falls back to `/mcp/tools.call` when missing or empty after sanitization
fn tracking_path(method: &str, params: Option<&serde_json::Value>) -> String {
    if method == "tools/call" {
        let raw = params
            .and_then(|p| p.get("name"))
            .and_then(|n| n.as_str())
            .unwrap_or("");
        // Allow only [a-zA-Z0-9_] — same character set used by MCP tool names.
        let sanitized: String = raw
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
            .take(64)
            .collect();
        if !sanitized.is_empty() {
            return format!("/mcp/{sanitized}");
        }
        // Unknown / malformed tool name — bucket under a single fixed path.
        return "/mcp/tools.call".to_string();
    }
    // Sanitize and clamp: allow only [a-zA-Z0-9_.] and cap at 64 chars so the result
    // always fits within the VARCHAR(256) path column.
    // Additionally require at least one alphanumeric character so that inputs like
    // "///" (all slashes → all dots) fall back to the unknown bucket rather than
    // producing a misleading "/mcp/..." path.
    let sanitized: String = method
        .chars()
        .map(|c| if c == '/' { '.' } else { c })
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '.')
        .take(64)
        .collect();
    let has_alnum = sanitized
        .chars()
        .any(|c| c.is_ascii_alphanumeric() || c == '_');
    if sanitized.is_empty() || !has_alnum {
        return "/mcp/unknown".to_string();
    }
    format!("/mcp/{sanitized}")
}

fn mcp_tool_dirty_mask(tool: &str) -> Option<crate::metrics_summary::DirtyMask> {
    use crate::metrics_summary::DirtyMask;
    match tool {
        "memory_store" | "memory_correct" | "memory_purge" | "memory_observe" => {
            Some(DirtyMask::MEMORY)
        }
        "memory_governance"
        | "memory_consolidate"
        | "memory_reflect"
        | "memory_extract_entities"
        | "memory_link_entities" => Some(DirtyMask::MEMORY | DirtyMask::GRAPH),
        "memory_feedback" => Some(DirtyMask::FEEDBACK),
        "memory_snapshot" | "memory_snapshot_delete" => Some(DirtyMask::SNAPSHOT),
        "memory_rollback" => Some(DirtyMask::FULL),
        "memory_branch" | "memory_branch_delete" => Some(DirtyMask::BRANCH),
        "memory_checkout" | "memory_merge" => Some(DirtyMask::FULL),
        _ => None,
    }
}

fn spawn_metrics_dirty_mark(
    state: AppState,
    user_id: String,
    mask: crate::metrics_summary::DirtyMask,
) {
    tokio::spawn(async move {
        if let Err(e) = state.mark_metrics_dirty(&user_id, mask).await {
            tracing::warn!(
                user_id = user_id,
                error = %e,
                "failed to mark metrics summary dirty after mcp mutation"
            );
        }
    });
}

pub async fn mcp_handler(
    State(state): State<AppState>,
    auth: AuthUser,
    body: String,
) -> impl IntoResponse {
    // Start timing here — auth already succeeded, this is real billable traffic.
    let t = std::time::Instant::now();

    // Helper: record a validation-failure entry and return early.
    // Uses underscore-prefixed paths so they never collide with real tool names.
    macro_rules! validation_err {
        ($path:expr, $code:expr, $body:expr) => {{
            state.call_log_batcher.record_rpc(
                auth.user_id.clone(),
                "POST".to_string(),
                $path.to_string(),
                200,
                t.elapsed().as_millis() as u32,
                RpcMeta::err($code),
            );
            if let Some(reporter) = &state.stats_reporter {
                reporter.report(
                    memoria_service::stats_reporter::StatsEvent::ApiCallLogged {
                        user_id: auth.user_id.clone(),
                        path: $path.to_string(),
                        is_mcp: true,
                        is_success: false,
                    },
                );
            }
            return Json($body).into_response();
        }};
    }

    let req: serde_json::Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(e) => validation_err!(
            "/mcp/_parse_error",
            -32700,
            json!({"jsonrpc": "2.0", "id": null,
                   "error": {"code": -32700, "message": e.to_string()}})
        ),
    };

    // Per JSON-RPC 2.0 §4, a Request object MUST be a JSON object.
    // Anything else (array, string, number, …) is Invalid Request.
    if !req.is_object() {
        validation_err!(
            "/mcp/_invalid_request",
            -32600,
            json!({"jsonrpc": "2.0", "id": null,
                   "error": {"code": -32600,
                              "message": "Invalid Request: payload must be a JSON object"}})
        );
    }

    // "jsonrpc" MUST equal exactly "2.0".
    if req.get("jsonrpc").and_then(|v| v.as_str()) != Some("2.0") {
        let id = req.get("id").cloned().unwrap_or(serde_json::Value::Null);
        validation_err!(
            "/mcp/_invalid_request",
            -32600,
            json!({"jsonrpc": "2.0", "id": id,
                   "error": {"code": -32600,
                              "message": "Invalid Request: jsonrpc must be \"2.0\""}})
        );
    }

    // "method" MUST be a non-empty string.
    let method = match req.get("method").and_then(|v| v.as_str()) {
        Some(m) if !m.is_empty() => m.to_string(),
        _ => {
            let id = req.get("id").cloned().unwrap_or(serde_json::Value::Null);
            validation_err!(
                "/mcp/_invalid_request",
                -32600,
                json!({"jsonrpc": "2.0", "id": id,
                       "error": {"code": -32600,
                                  "message": "Invalid Request: method must be a non-empty string"}})
            );
        }
    };

    let params = req.get("params").cloned();
    let track_path = tracking_path(&method, params.as_ref());
    let tracked_tool = if method == "tools/call" {
        track_path
            .strip_prefix("/mcp/")
            .filter(|name| !name.is_empty() && *name != "tools.call")
            .map(str::to_string)
    } else {
        None
    };
    let user_id = auth.user_id.clone();
    if let Some(tool) = tracked_tool.clone() {
        state.tool_usage_batcher.mark_used(user_id.clone(), tool);
    }

    // Single reporting point for MCP call stats, shared by both the
    // notification path and the regular-request path below.
    let report_stats = {
        let reporter = state.stats_reporter.clone();
        let uid = user_id.clone();
        move |path: &str, is_success: bool| {
            if let Some(r) = &reporter {
                r.report(memoria_service::stats_reporter::StatsEvent::ApiCallLogged {
                    user_id: uid.clone(),
                    path: path.to_string(),
                    is_mcp: true,
                    is_success,
                });
            }
        }
    };

    // JSON-RPC 2.0: a Notification is a *valid* Request without an "id" member.
    // The server MUST NOT reply to Notifications.
    if req.get("id").is_none() {
        let dispatch_result = memoria_mcp::dispatch_http(
            method.clone(),
            params,
            state.service.clone(),
            state.git.clone(),
            user_id.clone(),
        )
        .await;
        let rpc = match &dispatch_result {
            Ok(_) => RpcMeta::ok(),
            Err(e) => RpcMeta::err(e.code),
        };
        if dispatch_result.is_ok() {
            if let Some(mask) = tracked_tool.as_deref().and_then(mcp_tool_dirty_mask) {
                spawn_metrics_dirty_mark(state.clone(), user_id.clone(), mask);
            }
        }
        // Report accurate ops metrics using the real RPC path and success flag
        // (JSON-RPC errors still return HTTP 200, so is_success must come from rpc.success).
        report_stats(&track_path, rpc.success);
        state.call_log_batcher.record_rpc(
            user_id,
            "POST".to_string(),
            track_path,
            204, // HTTP 204 No Content — correct for notifications
            t.elapsed().as_millis() as u32,
            rpc,
        );
        return StatusCode::NO_CONTENT.into_response();
    }

    let id = req["id"].clone();

    // JSON-RPC spec: the HTTP response is always 200 OK, even for RPC errors.
    // Business-level error tracking uses rpc_success / rpc_error_code in the call log.
    let (response, rpc) = match memoria_mcp::dispatch_http(
        method.clone(),
        params,
        state.service.clone(),
        state.git.clone(),
        user_id.clone(),
    )
    .await
    {
        Ok(v) => {
            let result = if v.is_null() { json!({}) } else { v };
            (
                Json(json!({"jsonrpc": "2.0", "id": id, "result": result})).into_response(),
                RpcMeta::ok(),
            )
        }
        Err(e) => (
            Json(json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": e.code, "message": e.message}
            }))
            .into_response(),
            RpcMeta::err(e.code),
        ),
    };

    if rpc.success {
        if let Some(mask) = tracked_tool.as_deref().and_then(mcp_tool_dirty_mask) {
            spawn_metrics_dirty_mark(state.clone(), user_id.clone(), mask);
        }
    }

    // Report accurate ops metrics using the real RPC path and success flag
    // (JSON-RPC errors still return HTTP 200, so is_success must come from rpc.success).
    report_stats(&track_path, rpc.success);
    state.call_log_batcher.record_rpc(
        user_id,
        "POST".to_string(),
        track_path,
        200, // HTTP 200 — always correct for JSON-RPC responses
        t.elapsed().as_millis() as u32,
        rpc,
    );

    response
}

#[cfg(test)]
mod tests {
    use super::{mcp_tool_dirty_mask, tracking_path};
    use serde_json::json;

    // ── tools/call — happy path ───────────────────────────────────────────────

    #[test]
    fn tools_call_known_tool() {
        let params = json!({"name": "memory_store", "arguments": {}});
        assert_eq!(
            tracking_path("tools/call", Some(&params)),
            "/mcp/memory_store"
        );
    }

    #[test]
    fn tools_call_representative_names() {
        // Spot-checks a sample of valid tool names to verify the happy-path formatting.
        // This is NOT an exhaustive registry check — tool names are defined in
        // memoria-mcp and may change independently of this list.
        for name in &["memory_store", "memory_search", "memory_purge"] {
            let params = json!({"name": name});
            assert_eq!(
                tracking_path("tools/call", Some(&params)),
                format!("/mcp/{name}"),
                "unexpected path for tool {name}"
            );
        }
    }

    #[test]
    fn mutating_tool_classification_matches_write_tools() {
        assert!(mcp_tool_dirty_mask("memory_store").is_some());
        assert!(mcp_tool_dirty_mask("memory_snapshot").is_some());
        assert!(mcp_tool_dirty_mask("memory_merge").is_some());
        assert!(mcp_tool_dirty_mask("memory_search").is_none());
        assert!(mcp_tool_dirty_mask("memory_branches").is_none());
    }

    // ── tools/call — missing / malformed name ─────────────────────────────────

    #[test]
    fn tools_call_missing_name_falls_back() {
        let params = json!({});
        assert_eq!(
            tracking_path("tools/call", Some(&params)),
            "/mcp/tools.call"
        );
    }

    #[test]
    fn tools_call_no_params_falls_back() {
        assert_eq!(tracking_path("tools/call", None), "/mcp/tools.call");
    }

    #[test]
    fn tools_call_empty_name_falls_back() {
        let params = json!({"name": ""});
        assert_eq!(
            tracking_path("tools/call", Some(&params)),
            "/mcp/tools.call"
        );
    }

    // ── tools/call — sanitization ─────────────────────────────────────────────

    #[test]
    fn tools_call_name_with_slash_is_stripped() {
        // A malicious or buggy client sends a name containing path separators.
        let params = json!({"name": "../../etc/passwd"});
        let path = tracking_path("tools/call", Some(&params));
        // The tool segment (everything after "/mcp/") must contain no '/' or '.'.
        let segment = path
            .strip_prefix("/mcp/")
            .expect("path must start with /mcp/");
        assert!(
            !segment.contains('/'),
            "slash must not appear in tool segment: {segment}"
        );
        assert!(
            !segment.contains('.'),
            "dot must not appear in tool segment: {segment}"
        );
        assert!(
            !segment.contains(".."),
            "path traversal must not survive: {segment}"
        );
        // Only alphanumeric + '_' survive → "etcpasswd"
        assert_eq!(path, "/mcp/etcpasswd");
    }

    #[test]
    fn tools_call_name_only_special_chars_falls_back() {
        let params = json!({"name": "/../"});
        assert_eq!(
            tracking_path("tools/call", Some(&params)),
            "/mcp/tools.call"
        );
    }

    #[test]
    fn tools_call_name_clamped_to_64_chars() {
        let long_name = "a".repeat(200);
        let params = json!({"name": long_name});
        let path = tracking_path("tools/call", Some(&params));
        // "/mcp/" is 5 chars; the rest must be ≤ 64.
        let segment = path.strip_prefix("/mcp/").unwrap();
        assert!(
            segment.len() <= 64,
            "segment length {} exceeds 64",
            segment.len()
        );
    }

    // ── non-tools/call methods ────────────────────────────────────────────────

    #[test]
    fn initialize_method() {
        assert_eq!(tracking_path("initialize", None), "/mcp/initialize");
    }

    #[test]
    fn tools_list_method() {
        // '/' in the method name is replaced with '.'
        assert_eq!(tracking_path("tools/list", None), "/mcp/tools.list");
    }

    #[test]
    fn unknown_method_sanitized() {
        assert_eq!(
            tracking_path("notifications/initialized", None),
            "/mcp/notifications.initialized"
        );
    }

    #[test]
    fn empty_method_falls_back_to_unknown() {
        assert_eq!(tracking_path("", None), "/mcp/unknown");
    }

    #[test]
    fn method_with_only_special_chars_falls_back_to_unknown() {
        assert_eq!(tracking_path("///", None), "/mcp/unknown");
    }
}
