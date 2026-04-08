//! Prometheus-compatible `/metrics` endpoint.
//!
//! Exposes key operational metrics in Prometheus text exposition format.
//! DB-based metrics (memory counts, users, graph, etc.) are queried at scrape
//! time and cached.  Process-level metrics (HTTP, auth, entity extraction) are
//! rendered by [`crate::metrics::render`].

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use crate::{
    metrics_summary::{
        render_business_metrics, render_error_health_metrics, GlobalSummaryMetrics,
        SummaryRefreshStats,
    },
    state::CachedMetrics,
    AppState,
};

async fn list_known_users(state: &AppState) -> Result<Vec<String>, String> {
    let sql = state
        .service
        .sql_store
        .as_ref()
        .ok_or("SQL store not available")?;

    if let Some(router) = sql.db_router() {
        return router.list_active_users().await.map_err(|e| e.to_string());
    }

    let rows: Vec<(String,)> = sqlx::query_as(
        "SELECT DISTINCT user_id FROM mem_memories WHERE is_active > 0 ORDER BY user_id",
    )
    .fetch_all(sql.pool())
    .await
    .map_err(|e| e.to_string())?;
    Ok(rows.into_iter().map(|row| row.0).collect())
}

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

    let sql = state
        .service
        .sql_store
        .as_ref()
        .ok_or("SQL store not available")?;
    let shared_pool = state.auth_pool.as_ref().unwrap_or(sql.pool());
    let mut out = String::with_capacity(2048);
    let summary_manager = state.metrics_summary.clone();
    let refresh_stats = summary_manager
        .as_ref()
        .map(|manager| manager.refresh_stats())
        .unwrap_or_default();
    let global_metrics = if let Some(manager) = summary_manager {
        match manager.load_global_metrics().await {
            Ok(metrics) => metrics,
            Err(e) => {
                tracing::warn!(error = %e, "failed to load metrics summary");
                GlobalSummaryMetrics::default()
            }
        }
    } else {
        collect_live_global_metrics(state).await?
    };
    render_business_metrics(&mut out, &global_metrics);
    if state.metrics_summary.is_some() {
        render_summary_health_metrics(&mut out, &global_metrics, &refresh_stats);
        render_error_health_metrics(&mut out, &global_metrics);
    }

    // ── Async tasks ───────────────────────────────────────────────────────
    let task_rows: Vec<(String, i64)> =
        sqlx::query_as("SELECT status, COUNT(*) FROM mem_async_tasks GROUP BY status")
            .fetch_all(shared_pool)
            .await
            .unwrap_or_default();

    out.push_str("# HELP memoria_async_tasks Async tasks by status.\n");
    out.push_str("# TYPE memoria_async_tasks gauge\n");
    for (status, cnt) in &task_rows {
        out.push_str(&format!(
            "memoria_async_tasks{{status=\"{status}\"}} {cnt}\n"
        ));
    }

    // ── Governance last run ───────────────────────────────────────────────
    let last_gov: Option<(String,)> = sqlx::query_as(
        "SELECT MAX(updated_at) FROM mem_async_tasks WHERE task_type LIKE 'governance_%'",
    )
    .fetch_optional(shared_pool)
    .await
    .ok()
    .flatten();

    if let Some((ts,)) = last_gov {
        // Parse and convert to unix timestamp
        if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(&ts, "%Y-%m-%d %H:%M:%S%.f") {
            let unix = dt.and_utc().timestamp();
            out.push_str(
                "# HELP memoria_governance_last_run_timestamp Last governance run (unix).\n",
            );
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
    let pool_health = sql.pool_health_snapshot();
    out.push_str("# HELP memoria_pool_size Total established connections in main pool.\n");
    out.push_str("# TYPE memoria_pool_size gauge\n");
    out.push_str(&format!("memoria_pool_size {}\n", pool_health.size));
    out.push_str("# HELP memoria_pool_active Busy connections in main pool.\n");
    out.push_str("# TYPE memoria_pool_active gauge\n");
    out.push_str(&format!("memoria_pool_active {}\n", pool_health.active));
    out.push_str("# HELP memoria_pool_idle Idle connections in main pool.\n");
    out.push_str("# TYPE memoria_pool_idle gauge\n");
    out.push_str(&format!("memoria_pool_idle {}\n", pool_health.idle));
    if let Some(max) = sql.configured_max_connections() {
        out.push_str("# HELP memoria_pool_configured_max_connections Configured max connections for the main pool.\n");
        out.push_str("# TYPE memoria_pool_configured_max_connections gauge\n");
        out.push_str(&format!("memoria_pool_configured_max_connections {max}\n"));
    }
    out.push_str("# HELP memoria_pool_state Main pool health state as one-hot gauges.\n");
    out.push_str("# TYPE memoria_pool_state gauge\n");
    for state_name in ["healthy", "high_utilization", "saturated", "empty"] {
        let value = if pool_health.level.as_str() == state_name {
            1
        } else {
            0
        };
        out.push_str(&format!(
            "memoria_pool_state{{state=\"{state_name}\"}} {value}\n"
        ));
    }
    out.push_str("# HELP memoria_pool_state_duration_seconds Seconds spent in the current main-pool health state.\n");
    out.push_str("# TYPE memoria_pool_state_duration_seconds gauge\n");
    out.push_str(&format!(
        "memoria_pool_state_duration_seconds {}\n",
        pool_health.since.elapsed().as_secs()
    ));
    out.push_str("# HELP memoria_pool_state_consecutive_observations Consecutive pool-monitor observations in the current main-pool health state.\n");
    out.push_str("# TYPE memoria_pool_state_consecutive_observations gauge\n");
    out.push_str(&format!(
        "memoria_pool_state_consecutive_observations {}\n",
        pool_health.consecutive_observations
    ));
    out.push_str("# HELP memoria_pool_connection_anomalies_total Total observed main-pool connectivity anomalies in this process.\n");
    out.push_str("# TYPE memoria_pool_connection_anomalies_total counter\n");
    out.push_str(&format!(
        "memoria_pool_connection_anomalies_total {}\n",
        pool_health.connection_anomalies_total
    ));
    out.push_str("# HELP memoria_pool_timeouts_total Total main-pool timeout anomalies observed in this process.\n");
    out.push_str("# TYPE memoria_pool_timeouts_total counter\n");
    out.push_str(&format!(
        "memoria_pool_timeouts_total {}\n",
        pool_health.pool_timeouts_total
    ));
    out.push_str("# HELP memoria_pool_last_connection_anomaly Main-pool last connectivity anomaly as one-hot gauges.\n");
    out.push_str("# TYPE memoria_pool_last_connection_anomaly gauge\n");
    for kind in [
        "none",
        "pool_timed_out",
        "pool_closed",
        "io",
        "tls",
        "protocol",
        "too_many_connections",
    ] {
        let value = if pool_health.last_connection_anomaly_kind == kind {
            1
        } else {
            0
        };
        out.push_str(&format!(
            "memoria_pool_last_connection_anomaly{{kind=\"{kind}\"}} {value}\n"
        ));
    }
    if let Some(age) = pool_health.last_connection_anomaly_age_secs {
        out.push_str("# HELP memoria_pool_last_connection_anomaly_age_seconds Seconds since the last main-pool connectivity anomaly.\n");
        out.push_str("# TYPE memoria_pool_last_connection_anomaly_age_seconds gauge\n");
        out.push_str(&format!(
            "memoria_pool_last_connection_anomaly_age_seconds {age}\n"
        ));
    }
    out.push_str(
        "# HELP memoria_pool_empty_hint Main pool has zero established connections (1=true).\n",
    );
    out.push_str("# TYPE memoria_pool_empty_hint gauge\n");
    out.push_str(&format!(
        "memoria_pool_empty_hint {}\n",
        if pool_health.level.as_str() == "empty" {
            1
        } else {
            0
        }
    ));

    if let Some(auth_pool) = &state.auth_pool {
        out.push_str("# HELP memoria_auth_pool_size Total connections in auth pool.\n");
        out.push_str("# TYPE memoria_auth_pool_size gauge\n");
        out.push_str(&format!("memoria_auth_pool_size {}\n", auth_pool.size()));
        out.push_str("# HELP memoria_auth_pool_active Busy connections in auth pool.\n");
        out.push_str("# TYPE memoria_auth_pool_active gauge\n");
        out.push_str(&format!(
            "memoria_auth_pool_active {}\n",
            auth_pool.size().saturating_sub(auth_pool.num_idle() as u32)
        ));
        out.push_str("# HELP memoria_auth_pool_idle Idle connections in auth pool.\n");
        out.push_str("# TYPE memoria_auth_pool_idle gauge\n");
        out.push_str(&format!(
            "memoria_auth_pool_idle {}\n",
            auth_pool.num_idle()
        ));
    }

    // ── Process-level metrics (HTTP, auth, entity extraction, embedding) ──
    // Rendered by the metrics module — includes http_requests_total,
    // http_request_duration_seconds, http_requests_inflight, auth_failures,
    // sensitivity_blocks, entity extraction counters, and embedding metrics.
    crate::metrics::render::render_process_metrics(&mut out);

    let body = Arc::new(out);
    {
        let mut cache = state.metrics_cache.write().await;
        if let Some(snapshot) = cache.as_ref() {
            if snapshot.generated_at.elapsed() < state.metrics_cache_ttl {
                return Ok(snapshot.body.clone());
            }
        }
        *cache = Some(CachedMetrics {
            body: body.clone(),
            generated_at: std::time::Instant::now(),
        });
    }

    Ok(body)
}

async fn collect_live_global_metrics(state: &AppState) -> Result<GlobalSummaryMetrics, String> {
    let user_ids = list_known_users(state).await?;
    let mut metrics = GlobalSummaryMetrics {
        available: true,
        total_users: user_ids.len() as i64,
        ..GlobalSummaryMetrics::default()
    };

    for user_id in &user_ids {
        let user_store = state
            .service
            .user_sql_store(user_id)
            .await
            .map_err(|e| e.to_string())?;
        let memories_table = user_store.t("mem_memories");
        let feedback_table = user_store.t("mem_retrieval_feedback");
        let graph_nodes_table = user_store.t("memory_graph_nodes");
        let graph_edges_table = user_store.t("memory_graph_edges");

        // memory_type (labelled) + memory_total (scalar)
        let rows: Vec<(String, i64)> = sqlx::query_as(&format!(
            "SELECT memory_type, COUNT(*) FROM {memories_table} WHERE is_active > 0 GROUP BY memory_type"
        ))
        .fetch_all(user_store.pool())
        .await
        .map_err(|e| e.to_string())?;
        let mut user_memory_total: i64 = 0;
        for (memory_type, count) in rows {
            *metrics
                .labelled_totals
                .entry("memory_type".to_string())
                .or_default()
                .entry(memory_type)
                .or_default() += count;
            user_memory_total += count;
        }
        *metrics
            .scalar_totals
            .entry("memory_total".to_string())
            .or_default() += user_memory_total;

        // feedback_signal (labelled)
        let fb_rows: Vec<(String, i64)> = sqlx::query_as(&format!(
            "SELECT signal, COUNT(*) FROM {feedback_table} GROUP BY signal"
        ))
        .fetch_all(user_store.pool())
        .await
        .unwrap_or_default();
        for (signal, count) in fb_rows {
            *metrics
                .labelled_totals
                .entry("feedback_signal".to_string())
                .or_default()
                .entry(signal)
                .or_default() += count;
        }

        // graph_nodes_total
        *metrics
            .scalar_totals
            .entry("graph_nodes_total".to_string())
            .or_default() += sqlx::query_scalar::<_, i64>(&format!(
            "SELECT COUNT(*) FROM {graph_nodes_table} WHERE is_active = 1"
        ))
        .fetch_one(user_store.pool())
        .await
        .unwrap_or(0);

        // graph_edges_total
        *metrics
            .scalar_totals
            .entry("graph_edges_total".to_string())
            .or_default() +=
            sqlx::query_scalar::<_, i64>(&format!("SELECT COUNT(*) FROM {graph_edges_table}"))
                .fetch_one(user_store.pool())
                .await
                .unwrap_or(0);

        // snapshots_total
        *metrics
            .scalar_totals
            .entry("snapshots_total".to_string())
            .or_default() += user_store
            .list_snapshot_registrations(user_id)
            .await
            .unwrap_or_default()
            .len() as i64;

        // branches_extra_total
        *metrics
            .scalar_totals
            .entry("branches_extra_total".to_string())
            .or_default() += user_store
            .list_branches(user_id)
            .await
            .unwrap_or_default()
            .len() as i64;
    }

    Ok(metrics)
}

fn render_summary_health_metrics(
    out: &mut String,
    metrics: &GlobalSummaryMetrics,
    refresh: &SummaryRefreshStats,
) {
    out.push_str(
        "# HELP memoria_metrics_summary_available Shared summary-backed metrics are available.\n",
    );
    out.push_str("# TYPE memoria_metrics_summary_available gauge\n");
    out.push_str(&format!(
        "memoria_metrics_summary_available {}\n",
        if metrics.available { 1 } else { 0 }
    ));

    out.push_str(
        "# HELP memoria_metrics_summary_ready_users_total Users with fresh summary rows.\n",
    );
    out.push_str("# TYPE memoria_metrics_summary_ready_users_total gauge\n");
    out.push_str(&format!(
        "memoria_metrics_summary_ready_users_total {}\n",
        metrics.ready_users_total
    ));

    out.push_str(
        "# HELP memoria_metrics_summary_dirty_users_total Users waiting for summary refresh.\n",
    );
    out.push_str("# TYPE memoria_metrics_summary_dirty_users_total gauge\n");
    out.push_str(&format!(
        "memoria_metrics_summary_dirty_users_total {}\n",
        metrics.dirty_users_total
    ));

    out.push_str("# HELP memoria_metrics_summary_missing_users_total Active users without summary rows yet.\n");
    out.push_str("# TYPE memoria_metrics_summary_missing_users_total gauge\n");
    out.push_str(&format!(
        "memoria_metrics_summary_missing_users_total {}\n",
        metrics.missing_users_total
    ));

    out.push_str("# HELP memoria_metrics_summary_oldest_ready_refresh_age_seconds Age of the stalest ready summary row.\n");
    out.push_str("# TYPE memoria_metrics_summary_oldest_ready_refresh_age_seconds gauge\n");
    out.push_str(&format!(
        "memoria_metrics_summary_oldest_ready_refresh_age_seconds {}\n",
        metrics.oldest_ready_refresh_age_secs.unwrap_or(0)
    ));

    out.push_str(
        "# HELP memoria_metrics_summary_refresh_inflight_users Users currently being refreshed.\n",
    );
    out.push_str("# TYPE memoria_metrics_summary_refresh_inflight_users gauge\n");
    out.push_str(&format!(
        "memoria_metrics_summary_refresh_inflight_users {}\n",
        refresh.inflight_users
    ));

    out.push_str(
        "# HELP memoria_metrics_summary_refresh_failures_total Total summary refresh failures.\n",
    );
    out.push_str("# TYPE memoria_metrics_summary_refresh_failures_total counter\n");
    out.push_str(&format!(
        "memoria_metrics_summary_refresh_failures_total {}\n",
        refresh.failures_total
    ));

    out.push_str("# HELP memoria_metrics_summary_last_refresh_duration_seconds Duration of the last summary refresh batch.\n");
    out.push_str("# TYPE memoria_metrics_summary_last_refresh_duration_seconds gauge\n");
    out.push_str(&format!(
        "memoria_metrics_summary_last_refresh_duration_seconds {}\n",
        refresh.last_duration_secs
    ));

    out.push_str("# HELP memoria_metrics_summary_last_success_age_seconds Seconds since the last successful summary refresh batch.\n");
    out.push_str("# TYPE memoria_metrics_summary_last_success_age_seconds gauge\n");
    out.push_str(&format!(
        "memoria_metrics_summary_last_success_age_seconds {}\n",
        refresh.last_success_age_secs.unwrap_or(0)
    ));

    out.push_str("# HELP memoria_metrics_summary_last_failure_age_seconds Seconds since the last failed summary refresh batch.\n");
    out.push_str("# TYPE memoria_metrics_summary_last_failure_age_seconds gauge\n");
    out.push_str(&format!(
        "memoria_metrics_summary_last_failure_age_seconds {}\n",
        refresh.last_failure_age_secs.unwrap_or(0)
    ));

    // Pool-aware backoff observability
    out.push_str("# HELP memoria_metrics_summary_effective_concurrency Current effective refresh concurrency (reduced under pool pressure).\n");
    out.push_str("# TYPE memoria_metrics_summary_effective_concurrency gauge\n");
    out.push_str(&format!(
        "memoria_metrics_summary_effective_concurrency {}\n",
        refresh.effective_concurrency
    ));

    out.push_str("# HELP memoria_metrics_summary_effective_batch_size Current effective claim batch size (reduced under pool pressure).\n");
    out.push_str("# TYPE memoria_metrics_summary_effective_batch_size gauge\n");
    out.push_str(&format!(
        "memoria_metrics_summary_effective_batch_size {}\n",
        refresh.effective_batch_size
    ));

    out.push_str("# HELP memoria_metrics_summary_pool_backoff_active Whether pool-aware backoff is currently reducing refresh throughput.\n");
    out.push_str("# TYPE memoria_metrics_summary_pool_backoff_active gauge\n");
    out.push_str(&format!(
        "memoria_metrics_summary_pool_backoff_active {}\n",
        if refresh.pool_backoff_active { 1 } else { 0 }
    ));
}
