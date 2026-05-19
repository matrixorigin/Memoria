pub mod auth;
pub mod models;
pub mod otel;
pub mod rate_limit;
pub mod routes;
pub mod state;

pub use state::AppState;

use axum::{
    extract::DefaultBodyLimit,
    http::StatusCode,
    response::IntoResponse,
    routing::{delete, get, post, put},
    Json, Router,
};
use tower_http::catch_panic::CatchPanicLayer;

/// Build the full API router with all routes.
pub fn build_router(state: AppState) -> Router {
    Router::new()
        // Health
        .route("/health", get(routes::memory::health))
        .route("/health/instance", get(routes::memory::health_instance))
        // Metrics
        .route("/metrics", get(routes::metrics::prometheus_metrics))
        // Memory CRUD
        .route("/v1/memories", get(routes::memory::list_memories))
        .route("/v1/memories", post(routes::memory::store_memory))
        .route("/v1/memories/batch", post(routes::memory::batch_store))
        .route("/v1/memories/retrieve", post(routes::memory::retrieve))
        .route("/v1/memories/search", post(routes::memory::search))
        .route(
            "/v1/memories/correct",
            post(routes::memory::correct_by_query),
        )
        .route("/v1/memories/purge", post(routes::memory::purge_memories))
        .route("/v1/memories/:id", get(routes::memory::get_memory))
        .route(
            "/v1/memories/:id/correct",
            put(routes::memory::correct_memory),
        )
        .route(
            "/v1/memories/:id/history",
            get(routes::memory::get_memory_history),
        )
        .route("/v1/memories/:id", delete(routes::memory::delete_memory))
        .route(
            "/v1/profiles/:target_user_id",
            get(routes::memory::get_profile),
        )
        .route("/v1/observe", post(routes::memory::observe_turn))
        // Feedback
        .route(
            "/v1/memories/:id/feedback",
            post(routes::memory::record_feedback),
        )
        .route("/v1/feedback/stats", get(routes::memory::get_feedback_stats))
        .route(
            "/v1/feedback/by-tier",
            get(routes::memory::get_feedback_by_tier),
        )
        // Retrieval params
        .route(
            "/v1/retrieval-params",
            get(routes::memory::get_retrieval_params).put(routes::memory::set_retrieval_params),
        )
        .route(
            "/v1/retrieval-params/tune",
            post(routes::memory::tune_retrieval_params),
        )
        // Governance
        .route("/v1/governance", post(routes::governance::governance))
        .route("/v1/consolidate", post(routes::governance::consolidate))
        .route("/v1/reflect", post(routes::governance::reflect))
        .route(
            "/v1/extract-entities",
            post(routes::governance::extract_entities),
        )
        .route(
            "/v1/extract-entities/link",
            post(routes::governance::link_entities),
        )
        .route("/v1/entities", get(routes::governance::get_entities))
        // Snapshots
        .route("/v1/snapshots", get(routes::snapshots::list_snapshots))
        .route("/v1/snapshots", post(routes::snapshots::create_snapshot))
        .route(
            "/v1/snapshots/delete",
            post(routes::snapshots::delete_snapshot_bulk),
        )
        .route("/v1/snapshots/:name", get(routes::snapshots::get_snapshot))
        .route(
            "/v1/snapshots/:name",
            delete(routes::snapshots::delete_snapshot),
        )
        .route(
            "/v1/snapshots/:name/rollback",
            post(routes::snapshots::rollback),
        )
        .route(
            "/v1/snapshots/:name/diff",
            get(routes::snapshots::diff_snapshot),
        )
        // Branches
        .route("/v1/branches", get(routes::snapshots::list_branches))
        .route("/v1/branches", post(routes::snapshots::create_branch))
        .route(
            "/v1/branches/:name/checkout",
            post(routes::snapshots::checkout_branch),
        )
        .route(
            "/v1/branches/:name/merge",
            post(routes::snapshots::merge_branch),
        )
        .route(
            "/v1/branches/:name/diff",
            get(routes::snapshots::diff_branch),
        )
        .route(
            "/v1/branches/:name",
            delete(routes::snapshots::delete_branch),
        )
        // Sessions (episodic memory)
        .route(
            "/v1/sessions/:session_id/summary",
            post(routes::sessions::create_session_summary),
        )
        .route("/v1/tasks/:task_id", get(routes::sessions::get_task_status))
        // API key management
        .route("/auth/keys", post(routes::auth::create_key))
        .route("/auth/keys", get(routes::auth::list_keys))
        .route("/auth/keys/:id", get(routes::auth::get_key))
        .route("/auth/keys/:id/rotate", put(routes::auth::rotate_key))
        .route("/auth/keys/:id", delete(routes::auth::revoke_key))
        // Admin
        .route("/admin/stats", get(routes::admin::system_stats))
        .route("/admin/config", get(routes::admin::get_config))
        .route("/admin/users", get(routes::admin::list_users))
        .route(
            "/admin/users/:user_id/stats",
            get(routes::admin::user_stats),
        )
        .route("/admin/users/:user_id", delete(routes::admin::delete_user))
        .route(
            "/admin/users/:user_id/keys",
            get(routes::admin::list_user_keys),
        )
        .route(
            "/admin/users/:user_id/keys",
            delete(routes::admin::revoke_all_user_keys),
        )
        .route(
            "/admin/users/:user_id/params",
            post(routes::admin::set_user_params),
        )
        .route(
            "/admin/users/:user_id/reset-access-counts",
            post(routes::admin::reset_access_counts),
        )
        .route(
            "/admin/users/:user_id/strategy",
            post(routes::admin::set_user_strategy),
        )
        .route(
            "/admin/governance/:user_id/trigger",
            post(routes::admin::trigger_governance),
        )
        // Health
        .route("/v1/health/analyze", get(routes::admin::health_analyze))
        .route("/v1/health/storage", get(routes::admin::health_storage))
        .route("/v1/health/capacity", get(routes::admin::health_capacity))
        // Pipeline
        .route("/v1/pipeline/run", post(routes::memory::run_pipeline))
        // Plugins
        .route("/admin/plugins/signers", get(routes::plugins::list_signers))
        .route(
            "/admin/plugins/signers",
            post(routes::plugins::upsert_signer),
        )
        .route("/admin/plugins", get(routes::plugins::list_packages))
        .route("/admin/plugins", post(routes::plugins::publish_package))
        .route(
            "/admin/plugins/:plugin_key/:version/review",
            post(routes::plugins::review_package),
        )
        .route(
            "/admin/plugins/:plugin_key/:version/score",
            post(routes::plugins::score_package),
        )
        .route(
            "/admin/plugins/domains/:domain/bindings",
            get(routes::plugins::list_binding_rules),
        )
        .route(
            "/admin/plugins/domains/:domain/bindings",
            post(routes::plugins::upsert_binding_rule),
        )
        .route(
            "/admin/plugins/domains/:domain/activate",
            post(routes::plugins::activate_binding),
        )
        .route(
            "/admin/plugins/matrix",
            get(routes::plugins::list_compatibility_matrix),
        )
        .route(
            "/admin/plugins/events",
            get(routes::plugins::list_audit_events),
        )
        .with_state(state)
        .layer(DefaultBodyLimit::max(2 * 1024 * 1024)) // 2 MB
        .layer(tower_http::trace::TraceLayer::new_for_http()
            .make_span_with(|req: &axum::http::Request<_>| {
                tracing::info_span!(
                    "http",
                    method = %req.method(),
                    path = %req.uri().path(),
                )
            })
            .on_response(|res: &axum::http::Response<_>, latency: std::time::Duration, _span: &tracing::Span| {
                let status = res.status().as_u16();
                if status >= 500 {
                    tracing::error!(status, latency_ms = latency.as_millis(), "response");
                } else if status == 429 {
                    // rate-limit hits are operationally important
                    tracing::warn!(status, latency_ms = latency.as_millis(), "response");
                } else {
                    tracing::debug!(status, latency_ms = latency.as_millis(), "response");
                }
            })
        )
        .layer(CatchPanicLayer::custom(
            |err: Box<dyn std::any::Any + Send>| {
                let detail = err
                    .downcast_ref::<String>()
                    .map(|s| s.as_str())
                    .or_else(|| err.downcast_ref::<&str>().copied())
                    .unwrap_or("unknown");
                tracing::error!(panic = detail, "handler panicked");
                let body = serde_json::json!({ "error": "Internal server error" });
                (StatusCode::INTERNAL_SERVER_ERROR, Json(body)).into_response()
            },
        ))
}
