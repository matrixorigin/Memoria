use axum::{
    routing::{get, patch, post},
    Router,
};

use crate::{v2::routes, AppState};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/v2/memory/remember", post(routes::remember))
        .route("/v2/memory/batch-remember", post(routes::batch_remember))
        .route("/v2/memory/recall", post(routes::recall))
        .route("/v2/memory/batch-recall", post(routes::batch_recall))
        .route("/v2/memory/expand", post(routes::expand))
        .route("/v2/memory/batch-expand", post(routes::batch_expand))
        .route("/v2/memory/forget", post(routes::forget))
        .route("/v2/memory/batch-forget", post(routes::batch_forget))
        .route("/v2/memory/focus", post(routes::focus))
        .route("/v2/memory/list", get(routes::list))
        .route("/v2/memory/reflect", post(routes::reflect))
        .route("/v2/memory/profile", get(routes::profile))
        .route(
            "/v2/memory/entities/extract",
            post(routes::extract_entities),
        )
        .route("/v2/memory/entities", get(routes::list_entities))
        .route("/v2/memory/tags", get(routes::list_tags))
        .route("/v2/memory/stats", get(routes::stats))
        .route("/v2/memory/:id/history", get(routes::history))
        .route("/v2/memory/jobs", get(routes::jobs))
        .route("/v2/admin/job-metrics", get(routes::job_metrics))
        .route("/v2/memory/links", get(routes::links))
        .route("/v2/memory/related", get(routes::related))
        .route(
            "/v2/memory/:id/feedback/history",
            get(routes::get_memory_feedback_history),
        )
        .route(
            "/v2/memory/:id/feedback",
            get(routes::get_memory_feedback).post(routes::feedback),
        )
        .route("/v2/feedback/history", get(routes::get_feedback_history))
        .route("/v2/feedback/stats", get(routes::get_feedback_stats))
        .route("/v2/feedback/by-tier", get(routes::get_feedback_by_tier))
        .route("/v2/memory/update", patch(routes::update))
}
