//! Prometheus-compatible `/metrics` endpoint.
//!
//! Exposes key operational metrics in Prometheus text exposition format.
//! No external crate needed — we emit the text format directly.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use crate::{state::CachedMetrics, AppState};

// ── Process-lifetime counters (no external crate needed) ─────────────────────

/// Incremented on every 401/403 response from the auth extractor.
pub static AUTH_FAILURES: AtomicU64 = AtomicU64::new(0);
/// Incremented when sensitivity filter blocks a store request.
pub static SENSITIVITY_BLOCKS: AtomicU64 = AtomicU64::new(0);

/// GET /metrics — Prometheus text exposition format.
pub async fn prometheus_metrics(State(state): State<AppState>) -> Response {
    match collect_metrics(&state).await {
        Ok(body) => (
            StatusCode::OK,
            [("content-type", "text/plain; version=0.0.4; charset=utf-8")],
            body.as_ref().to_owned(),
        )
            .into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn collect_metrics(state: &AppState) -> Result<Arc<String>, String> {
    {
        let cache = state.metrics_cache.read().await;
        if let Some(snapshot) = cache.as_ref() {
            if snapshot.generated_at.elapsed() < state.metrics_cache_ttl {
                return Ok(snapshot.body.clone());
            }
        }
    }

    let mut cache = state.metrics_cache.write().await;
    if let Some(snapshot) = cache.as_ref() {
        if snapshot.generated_at.elapsed() < state.metrics_cache_ttl {
            return Ok(snapshot.body.clone());
        }
    }

    let sql = state
        .service
        .sql_store
        .as_ref()
        .ok_or("SQL store not available")?;
    let pool = sql.pool();
    let mut out = String::with_capacity(2048);

    // ── Memory counts by type ─────────────────────────────────────────────
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT memory_type, COUNT(*) FROM mem_memories WHERE is_active > 0 GROUP BY memory_type",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| e.to_string())?;

    out.push_str("# HELP memoria_memories_total Active memories by type.\n");
    out.push_str("# TYPE memoria_memories_total gauge\n");
    let mut total = 0i64;
    for (mt, cnt) in &rows {
        out.push_str(&format!("memoria_memories_total{{type=\"{mt}\"}} {cnt}\n"));
        total += cnt;
    }
    out.push_str(&format!("memoria_memories_total{{type=\"all\"}} {total}\n"));

    // ── Users ─────────────────────────────────────────────────────────────
    let (users,): (i64,) =
        sqlx::query_as("SELECT COUNT(DISTINCT user_id) FROM mem_memories WHERE is_active > 0")
            .fetch_all(pool)
            .await
            .map_err(|e| e.to_string())?
            .into_iter()
            .next()
            .unwrap_or((0,));
    out.push_str("# HELP memoria_users_total Active users.\n");
    out.push_str("# TYPE memoria_users_total gauge\n");
    out.push_str(&format!("memoria_users_total {users}\n"));

    // ── Feedback counts ───────────────────────────────────────────────────
    let fb_rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT signal, COUNT(*) FROM mem_retrieval_feedback GROUP BY signal",
    )
    .fetch_all(pool)
    .await
    .unwrap_or_default();

    out.push_str("# HELP memoria_feedback_total Feedback signals by type.\n");
    out.push_str("# TYPE memoria_feedback_total counter\n");
    for (signal, cnt) in &fb_rows {
        out.push_str(&format!("memoria_feedback_total{{signal=\"{signal}\"}} {cnt}\n"));
    }

    // ── Entity graph ──────────────────────────────────────────────────────
    let (nodes,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM memory_graph_nodes WHERE is_active = 1")
        .fetch_one(pool)
        .await
        .unwrap_or((0,));
    let (edges,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM memory_graph_edges")
        .fetch_one(pool)
        .await
        .unwrap_or((0,));
    out.push_str("# HELP memoria_graph_nodes_total Entity graph nodes.\n");
    out.push_str("# TYPE memoria_graph_nodes_total gauge\n");
    out.push_str(&format!("memoria_graph_nodes_total {nodes}\n"));
    out.push_str("# HELP memoria_graph_edges_total Entity graph edges.\n");
    out.push_str("# TYPE memoria_graph_edges_total gauge\n");
    out.push_str(&format!("memoria_graph_edges_total {edges}\n"));

    // ── Snapshots ─────────────────────────────────────────────────────────
    let snapshots = state.git.list_snapshots().await.unwrap_or_default();
    out.push_str("# HELP memoria_snapshots_total Snapshots.\n");
    out.push_str("# TYPE memoria_snapshots_total gauge\n");
    out.push_str(&format!("memoria_snapshots_total {}\n", snapshots.len()));

    // ── Branches ──────────────────────────────────────────────────────────
    let branches: Vec<(String,)> =
        sqlx::query_as("SELECT DISTINCT branch_name FROM mem_branch_state")
            .fetch_all(pool)
            .await
            .unwrap_or_default();
    out.push_str("# HELP memoria_branches_total Active branches.\n");
    out.push_str("# TYPE memoria_branches_total gauge\n");
    out.push_str(&format!("memoria_branches_total {}\n", branches.len()));

    // ── Async tasks ───────────────────────────────────────────────────────
    let task_rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT status, COUNT(*) FROM mem_async_tasks GROUP BY status",
    )
    .fetch_all(pool)
    .await
    .unwrap_or_default();

    out.push_str("# HELP memoria_async_tasks Async tasks by status.\n");
    out.push_str("# TYPE memoria_async_tasks gauge\n");
    for (status, cnt) in &task_rows {
        out.push_str(&format!("memoria_async_tasks{{status=\"{status}\"}} {cnt}\n"));
    }

    // ── Governance last run ───────────────────────────────────────────────
    let last_gov: Option<(String,)> = sqlx::query_as(
        "SELECT MAX(updated_at) FROM mem_async_tasks WHERE task_type LIKE 'governance_%'",
    )
    .fetch_optional(pool)
    .await
    .ok()
    .flatten();

    if let Some((ts,)) = last_gov {
        // Parse and convert to unix timestamp
        if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(&ts, "%Y-%m-%d %H:%M:%S%.f") {
            let unix = dt.and_utc().timestamp();
            out.push_str("# HELP memoria_governance_last_run_timestamp Last governance run (unix).\n");
            out.push_str("# TYPE memoria_governance_last_run_timestamp gauge\n");
            out.push_str(&format!("memoria_governance_last_run_timestamp {unix}\n"));
        }
    }

    // ── Instance info ─────────────────────────────────────────────────────
    out.push_str("# HELP memoria_info Build information.\n");
    out.push_str("# TYPE memoria_info gauge\n");
    out.push_str(&format!(
        "memoria_info{{instance=\"{}\",version=\"{}\"}} 1\n",
        state.instance_id,
        env!("CARGO_PKG_VERSION")
    ));

    // ── Connection pool ───────────────────────────────────────────────────
    out.push_str("# HELP memoria_pool_size Total connections in main pool.\n");
    out.push_str("# TYPE memoria_pool_size gauge\n");
    out.push_str(&format!("memoria_pool_size {}\n", pool.size()));
    out.push_str("# HELP memoria_pool_idle Idle connections in main pool.\n");
    out.push_str("# TYPE memoria_pool_idle gauge\n");
    out.push_str(&format!("memoria_pool_idle {}\n", pool.num_idle()));

    if let Some(auth_pool) = &state.auth_pool {
        out.push_str("# HELP memoria_auth_pool_size Total connections in auth pool.\n");
        out.push_str("# TYPE memoria_auth_pool_size gauge\n");
        out.push_str(&format!("memoria_auth_pool_size {}\n", auth_pool.size()));
        out.push_str("# HELP memoria_auth_pool_idle Idle connections in auth pool.\n");
        out.push_str("# TYPE memoria_auth_pool_idle gauge\n");
        out.push_str(&format!("memoria_auth_pool_idle {}\n", auth_pool.num_idle()));
    }

    // ── Security counters ─────────────────────────────────────────────────
    let auth_failures = AUTH_FAILURES.load(Ordering::Relaxed);
    out.push_str("# HELP memoria_auth_failures_total Authentication failures (401/403).\n");
    out.push_str("# TYPE memoria_auth_failures_total counter\n");
    out.push_str(&format!("memoria_auth_failures_total {auth_failures}\n"));

    let sensitivity_blocks = SENSITIVITY_BLOCKS.load(Ordering::Relaxed);
    out.push_str("# HELP memoria_sensitivity_blocks_total Requests blocked by sensitivity filter.\n");
    out.push_str("# TYPE memoria_sensitivity_blocks_total counter\n");
    out.push_str(&format!("memoria_sensitivity_blocks_total {sensitivity_blocks}\n"));

    let body = Arc::new(out);
    *cache = Some(CachedMetrics {
        body: body.clone(),
        generated_at: std::time::Instant::now(),
    });

    Ok(body)
}
