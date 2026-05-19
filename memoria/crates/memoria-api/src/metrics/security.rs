//! Security-related metrics.
//!
//! Tracks authentication failures and sensitivity filter blocks.

use crate::metrics::types::Counter;

/// Security layer metrics.
pub struct SecurityMetrics {
    /// 401/403 authentication failures.
    pub auth_failures: Counter,
    /// Requests blocked by the content sensitivity filter.
    pub sensitivity_blocks: Counter,
}

impl SecurityMetrics {
    pub(crate) fn new() -> Self {
        Self {
            auth_failures: Counter::new(),
            sensitivity_blocks: Counter::new(),
        }
    }
}
