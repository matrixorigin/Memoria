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

        // Global (non-per-user) tasks first
        if task == "hourly" {
            if let Err(e) = sql.cleanup_tool_results(72).await { error!("cleanup_tool_results: {e}"); }
            match sql.archive_stale_working(24).await {
                Ok(per_user) => {
                    for (uid, count) in &per_user {
                        sql.log_edit(uid, "governance:archive_working", &[], &format!("archived {count} stale working memories (>24h)"), None).await;
                        total_cleaned += count;
                    }
                }
                Err(e) => error!("archive_stale_working: {e}"),
            }
        }

        for (user_id,) in &users {
            match task {
                "hourly" => {
                    let cleaned = sql.cleanup_stale(user_id).await.unwrap_or(0);
                    if cleaned > 0 {
                        sql.log_edit(user_id, "governance:cleanup_stale", &[], &format!("cleaned {cleaned}"), None).await;
                    }
                    total_cleaned += cleaned;
                }
                "daily" => {
                    let quarantined = sql.quarantine_low_confidence(user_id).await.unwrap_or(0);
                    if quarantined > 0 {
                        sql.log_edit(user_id, "governance:quarantine", &[], &format!("quarantined {quarantined}"), None).await;
                    }
                    let cleaned = sql.cleanup_stale(user_id).await.unwrap_or(0);
                    if cleaned > 0 {
                        sql.log_edit(user_id, "governance:cleanup_stale", &[], &format!("cleaned {cleaned}"), None).await;
                    }
                    match sql.compress_redundant(user_id, 0.95, 30, 10_000).await {
                        Ok(n) if n > 0 => sql.log_edit(user_id, "governance:compress_redundant", &[], &format!("compressed {n}"), None).await,
                        Err(e) => error!("compress_redundant: {e}"),
                        _ => {}
                    }
                    match sql.cleanup_orphaned_incrementals(user_id, 24).await {
                        Ok(n) if n > 0 => sql.log_edit(user_id, "governance:cleanup_orphaned_incrementals", &[], &format!("cleaned {n}"), None).await,
                        Err(e) => error!("cleanup_orphaned_incrementals: {e}"),
                        _ => {}
                    }
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
