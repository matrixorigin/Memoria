//! Memory governance scheduler — runs periodic governance tasks.
//!
//! Tasks:
//!   hourly  (3600s)  — cleanup tool_results, archive stale working memories
//!   daily   (86400s) — quarantine low-confidence, cleanup stale memories
//!   weekly  (604800s)— cleanup old branches and snapshots
//!
//! Uses existing cooldown mechanism to avoid duplicate runs across restarts.
//! Enable via MEMORIA_GOVERNANCE_ENABLED=true (default: false in scheduler, opt-in).

use std::sync::Arc;
use tokio::time::{interval, Duration};
use tracing::{error, info};

use crate::MemoryService;

pub struct GovernanceScheduler {
    service: Arc<MemoryService>,
    /// Run governance for all distinct users found in the DB
    enabled: bool,
}

impl GovernanceScheduler {
    pub fn new(service: Arc<MemoryService>) -> Self {
        let enabled = std::env::var("MEMORIA_GOVERNANCE_ENABLED")
            .map(|v| v.to_lowercase() == "true")
            .unwrap_or(false);
        Self { service, enabled }
    }

    /// Spawn background tasks. Returns immediately; tasks run in background.
    pub fn start(self: Arc<Self>) {
        if !self.enabled {
            info!("Governance scheduler disabled (set MEMORIA_GOVERNANCE_ENABLED=true to enable)");
            return;
        }
        info!("Governance scheduler starting");

        let s = self.clone();
        tokio::spawn(async move { s.run_loop("hourly", 3600).await });

        let s = self.clone();
        tokio::spawn(async move { s.run_loop("daily", 86400).await });

        let s = self.clone();
        tokio::spawn(async move { s.run_loop("weekly", 604800).await });
    }

    async fn run_loop(&self, task: &'static str, interval_secs: u64) {
        let mut ticker = interval(Duration::from_secs(interval_secs));
        ticker.tick().await; // skip first immediate tick
        loop {
            ticker.tick().await;
            if let Err(e) = self.run_task(task).await {
                error!("Governance [{task}] error: {e}");
            }
        }
    }

    async fn run_task(&self, task: &str) -> Result<(), crate::MemoriaError> {
        let sql = match &self.service.sql_store {
            Some(s) => s,
            None => return Ok(()),
        };

        // Get all active users
        let users: Vec<(String,)> = sqlx::query_as(
            "SELECT DISTINCT user_id FROM mem_memories WHERE is_active > 0"
        ).fetch_all(sql.pool()).await.map_err(|e| crate::MemoriaError::Database(e.to_string()))?;

        let mut total_quarantined = 0i64;
        let mut total_cleaned = 0i64;

        for (user_id,) in &users {
            match task {
                "hourly" => {
                    if let Err(e) = sql.cleanup_tool_results(72).await { error!("cleanup_tool_results: {e}"); }
                    if let Err(e) = sql.archive_stale_working(24).await { error!("archive_stale_working: {e}"); }
                    let cleaned = sql.cleanup_stale(user_id).await.unwrap_or(0);
                    total_cleaned += cleaned;
                }
                "daily" => {
                    let quarantined = sql.quarantine_low_confidence(user_id).await.unwrap_or(0);
                    let cleaned = sql.cleanup_stale(user_id).await.unwrap_or(0);
                    if let Err(e) = sql.compress_redundant(user_id, 0.95, 30, 10_000).await { error!("compress_redundant: {e}"); }
                    if let Err(e) = sql.cleanup_orphaned_incrementals(user_id, 24).await { error!("cleanup_orphaned_incrementals: {e}"); }
                    total_quarantined += quarantined;
                    total_cleaned += cleaned;
                }
                "weekly" => {
                    if let Err(e) = sql.rebuild_vector_index("mem_memories").await { error!("rebuild_vector_index: {e}"); }
                    if let Err(e) = sql.cleanup_snapshots(5).await { error!("cleanup_snapshots: {e}"); }
                    if let Err(e) = sql.cleanup_orphan_branches().await { error!("cleanup_orphan_branches: {e}"); }
                }
                _ => {}
            }
        }

        info!(
            "Governance [{task}] complete: users={}, quarantined={total_quarantined}, cleaned={total_cleaned}",
            users.len()
        );
        Ok(())
    }
}

/// Convenience type alias for use in AppState
pub type SchedulerHandle = Arc<GovernanceScheduler>;
