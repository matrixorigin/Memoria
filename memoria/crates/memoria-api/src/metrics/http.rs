//! HTTP-layer metrics.
//!
//! Tracks request count, latency distribution, and in-flight concurrency.

use crate::metrics::types::{CounterVec, Gauge, HistogramVec};

/// Histogram bucket boundaries for HTTP request duration (seconds).
///
/// Covers fast cache hits (5ms) through slow pool timeouts (30s).
pub(crate) const DURATION_BOUNDS: &[f64] = &[
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0,
];

/// HTTP layer metrics.
pub struct HttpMetrics {
    /// Total HTTP requests. Key: `"method|route|status_class|source"`.
    pub requests_total: CounterVec,
    /// HTTP request duration histogram. Key: `"route|source"`.
    pub request_duration: HistogramVec,
    /// Requests currently being processed.
    pub inflight: Gauge,
}

impl HttpMetrics {
    pub(crate) fn new() -> Self {
        Self {
            requests_total: CounterVec::new(),
            request_duration: HistogramVec::new(DURATION_BOUNDS),
            inflight: Gauge::new(),
        }
    }
}
