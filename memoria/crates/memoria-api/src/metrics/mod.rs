//! Process-level metrics infrastructure for the Memoria API server.
//!
//! # Architecture
//!
//! ```text
//! ┌──────────────┐   records    ┌──────────────┐   renders   ┌──────────────┐
//! │  middleware   │ ──────────► │   Registry   │ ◄────────── │  /metrics    │
//! │  (per-req)   │             │  (global)    │             │  endpoint    │
//! └──────────────┘             └──────────────┘             └──────────────┘
//! ```
//!
//! Metrics are grouped into sub-modules by concern:
//!
//! | Module      | Contents                                           |
//! |-------------|----------------------------------------------------|
//! | `http`      | Request count, latency histogram, in-flight gauge  |
//! | `security`  | Auth failures, sensitivity blocks                  |
//! | `worker`    | Entity extraction queue pressure                   |
//! | `embedding` | Embedding call latency and error rate              |
//! | `types`     | Lock-free Counter / Gauge / Histogram primitives   |
//! | `middleware` | Tower HTTP middleware layer                       |
//! | `render`    | Prometheus text-format rendering helpers           |
//!
//! # Adding a new metric
//!
//! 1. Add a field to the appropriate sub-module struct (e.g. [`security::SecurityMetrics`]).
//! 2. Initialise it in that struct's `new()`.
//! 3. Record observations at the appropriate call site.
//! 4. Render it in [`render::render_process_metrics`].

pub mod embedding;
pub mod http;
pub mod middleware;
pub mod render;
pub mod security;
pub mod types;
pub mod worker;

use std::sync::OnceLock;

/// Central metrics registry.
///
/// Metrics are organised by concern under typed sub-structs.
/// Access via [`registry()`]; all fields are lock-free.
pub struct Registry {
    /// HTTP layer: request count, latency histogram, in-flight gauge.
    pub http: http::HttpMetrics,
    /// Security: auth failures, sensitivity blocks.
    pub security: security::SecurityMetrics,
    /// Background workers: entity extraction queue pressure.
    pub worker: worker::WorkerMetrics,
    /// Embedding sub-system: call latency and error rate.
    pub embedding: embedding::EmbeddingMetrics,
}

impl Registry {
    fn new() -> Self {
        Self {
            http: http::HttpMetrics::new(),
            security: security::SecurityMetrics::new(),
            worker: worker::WorkerMetrics::new(),
            embedding: embedding::EmbeddingMetrics::new(),
        }
    }
}

static REGISTRY: OnceLock<Registry> = OnceLock::new();

/// Return the global metrics registry, initialising it on first call.
pub fn registry() -> &'static Registry {
    REGISTRY.get_or_init(Registry::new)
}
