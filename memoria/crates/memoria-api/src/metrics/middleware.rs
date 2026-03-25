//! Tower middleware that records HTTP request metrics.
//!
//! Captures per-request count, duration histogram, and in-flight gauge.
//! Traffic is classified by a `source` label derived from request headers:
//!
//! | Header                 | Resolved source |
//! |------------------------|-----------------|
//! | `X-Memoria-Source: internal` | `internal` |
//! | `X-Memoria-Source: sdk`      | `sdk`      |
//! | `X-Memoria-Tool` present     | `mcp`      |
//! | *(none of the above)*        | `api`      |

use axum::{
    body::Body,
    extract::MatchedPath,
    http::{HeaderMap, Request},
    middleware::Next,
    response::Response,
};
use std::time::Instant;

/// Classify request origin from headers.
///
/// Priority: explicit `X-Memoria-Source` > MCP tool header > default `api`.
fn extract_source(headers: &HeaderMap) -> &'static str {
    if let Some(v) = headers.get("x-memoria-source") {
        if let Ok(s) = v.to_str() {
            return match s {
                "internal" => "internal",
                "sdk" => "sdk",
                _ => "api",
            };
        }
    }
    if headers.contains_key("x-memoria-tool") || headers.contains_key("x-tool-name") {
        return "mcp";
    }
    "api"
}

fn status_class(code: u16) -> &'static str {
    match code {
        200..=299 => "2xx",
        300..=399 => "3xx",
        400..=499 => "4xx",
        _ => "5xx",
    }
}

/// Axum middleware — wire with `axum::middleware::from_fn(http_metrics)`.
pub async fn http_metrics(
    matched_path: Option<MatchedPath>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let route = matched_path
        .map(|p| p.as_str().to_owned())
        .unwrap_or_else(|| "unmatched".to_owned());

    // Skip monitoring endpoints to avoid feedback loops.
    if route == "/metrics" || route == "/health" {
        return next.run(request).await;
    }

    let reg = super::registry();
    let method = request.method().as_str().to_owned();
    let source = extract_source(request.headers());

    reg.http.inflight.inc();
    let start = Instant::now();

    let response = next.run(request).await;

    let elapsed = start.elapsed().as_secs_f64();
    let status = status_class(response.status().as_u16());
    reg.http.inflight.dec();

    // Counter: method|route|status|source
    reg.http.requests_total
        .inc(&format!("{method}|{route}|{status}|{source}"));

    // Histogram: route|source
    reg.http.request_duration
        .observe(&format!("{route}|{source}"), elapsed);

    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn source_from_explicit_header() {
        let mut headers = HeaderMap::new();
        headers.insert("x-memoria-source", HeaderValue::from_static("internal"));
        assert_eq!(extract_source(&headers), "internal");
    }

    #[test]
    fn source_from_mcp_tool() {
        let mut headers = HeaderMap::new();
        headers.insert("x-memoria-tool", HeaderValue::from_static("cursor"));
        assert_eq!(extract_source(&headers), "mcp");
    }

    #[test]
    fn source_default_api() {
        let headers = HeaderMap::new();
        assert_eq!(extract_source(&headers), "api");
    }

    #[test]
    fn status_class_mapping() {
        assert_eq!(status_class(200), "2xx");
        assert_eq!(status_class(201), "2xx");
        assert_eq!(status_class(301), "3xx");
        assert_eq!(status_class(404), "4xx");
        assert_eq!(status_class(500), "5xx");
        assert_eq!(status_class(503), "5xx");
    }
}
