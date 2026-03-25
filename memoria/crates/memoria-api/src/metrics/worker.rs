//! Background worker metrics.
//!
//! Provides a typed view over the process-global atomics in `memoria_service`
//! that track entity extraction queue health.

use std::sync::atomic::Ordering;

/// Background worker / async pipeline metrics.
pub struct WorkerMetrics;

impl WorkerMetrics {
    pub(crate) fn new() -> Self {
        Self
    }

    /// Entity extraction jobs dropped because the queue was full.
    pub fn entity_extraction_drops(&self) -> u64 {
        memoria_service::ENTITY_EXTRACTION_DROPS.load(Ordering::Relaxed)
    }
}
