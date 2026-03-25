//! Embedding sub-system metrics.
//!
//! Tracks latency and error rate for embedding provider calls, broken down by
//! provider backend (http / round-robin / mock / local) and operation
//! (single / batch).

use crate::metrics::types::{CounterVec, HistogramVec};

/// Histogram bucket boundaries for embedding duration (seconds).
///
/// Covers fast mock/local calls (<5ms) through slow remote API calls (30s).
pub(crate) const DURATION_BOUNDS: &[f64] = &[
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0,
];

/// Embedding sub-system metrics.
pub struct EmbeddingMetrics {
    /// Embedding call duration. Key: `"provider|operation"` (operation = `single` | `batch`).
    pub duration_seconds: HistogramVec,
    /// Embedding calls that returned an error. Key: `"provider|operation"`.
    pub errors_total: CounterVec,
}

impl EmbeddingMetrics {
    pub(crate) fn new() -> Self {
        Self {
            duration_seconds: HistogramVec::new(DURATION_BOUNDS),
            errors_total: CounterVec::new(),
        }
    }
}
