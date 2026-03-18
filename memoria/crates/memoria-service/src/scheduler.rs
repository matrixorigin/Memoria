//! Memory governance scheduler — periodic orchestration over pluggable governance strategies.
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
use tracing::{error, info, warn};

use crate::{
    governance::{
        DefaultGovernanceStrategy, GovernanceExecution, GovernanceStore, GovernanceStrategy,
        GovernanceTask,
    },
    strategy_domain::StrategyStatus,
    MemoryService,
};

pub struct GovernanceScheduler {
    store: Option<Arc<dyn GovernanceStore>>,
    strategy: Arc<dyn GovernanceStrategy>,
    fallback_strategy: Arc<dyn GovernanceStrategy>,
    enabled: bool,
}

impl GovernanceScheduler {
    pub fn new(service: Arc<MemoryService>) -> Self {
        let store = service
            .sql_store
            .clone()
            .map(|store| -> Arc<dyn GovernanceStore> { store });
        let default_strategy: Arc<dyn GovernanceStrategy> = Arc::new(DefaultGovernanceStrategy);
        Self::new_with_components(
            store,
            default_strategy.clone(),
            default_strategy,
            read_enabled_flag(),
        )
    }

    fn new_with_components(
        store: Option<Arc<dyn GovernanceStore>>,
        strategy: Arc<dyn GovernanceStrategy>,
        fallback_strategy: Arc<dyn GovernanceStrategy>,
        enabled: bool,
    ) -> Self {
        Self {
            store,
            strategy,
            fallback_strategy,
            enabled,
        }
    }

    /// Spawn background tasks. Returns immediately; tasks run in background.
    pub fn start(self: Arc<Self>) {
        if !self.enabled {
            info!("Governance scheduler disabled (set MEMORIA_GOVERNANCE_ENABLED=true to enable)");
            return;
        }
        info!(
            strategy = self.strategy.strategy_key(),
            fallback = self.fallback_strategy.strategy_key(),
            "Governance scheduler starting"
        );

        let s = self.clone();
        tokio::spawn(async move { s.run_loop(GovernanceTask::Hourly, 3600).await });

        let s = self.clone();
        tokio::spawn(async move { s.run_loop(GovernanceTask::Daily, 86400).await });

        let s = self.clone();
        tokio::spawn(async move { s.run_loop(GovernanceTask::Weekly, 604800).await });
    }

    async fn run_loop(&self, task: GovernanceTask, interval_secs: u64) {
        let mut ticker = interval(Duration::from_secs(interval_secs));
        ticker.tick().await; // skip first immediate tick
        loop {
            ticker.tick().await;
            if let Err(err) = self.run_task(task).await {
                error!(task = task.as_str(), %err, "Governance task failed");
            }
        }
    }

    async fn run_task(&self, task: GovernanceTask) -> Result<GovernanceExecution, crate::MemoriaError> {
        let Some(store) = &self.store else {
            return Ok(GovernanceExecution::default());
        };

        let primary_key = self.strategy.strategy_key();
        let mut execution = match self.strategy.run(store.as_ref(), task).await {
            Ok(execution) => execution,
            Err(primary_err) => {
                let fallback_key = self.fallback_strategy.strategy_key();
                if primary_key == fallback_key {
                    return Err(primary_err);
                }
                warn!(
                    task = task.as_str(),
                    strategy = primary_key,
                    fallback = fallback_key,
                    %primary_err,
                    "Primary governance strategy failed; degrading to fallback"
                );

                let mut fallback_execution = self.fallback_strategy.run(store.as_ref(), task).await?;
                fallback_execution.report.status = StrategyStatus::Degraded;
                fallback_execution.report.warnings.push(format!(
                    "Primary strategy {primary_key} failed: {primary_err}. Fell back to {fallback_key}."
                ));
                fallback_execution
                    .report
                    .metrics
                    .insert("governance.degraded".to_string(), 1.0);
                fallback_execution
            }
        };

        if execution.report.status == StrategyStatus::Failed {
            execution
                .report
                .warnings
                .push("Governance execution finished in failed state".to_string());
        }

        info!(
            task = task.as_str(),
            strategy = primary_key,
            report_status = execution.report.status.as_str(),
            users = execution.summary.users_processed,
            quarantined = execution.summary.total_quarantined,
            cleaned = execution.summary.total_cleaned,
            tool_results_cleaned = execution.summary.tool_results_cleaned,
            archived_working = execution.summary.archived_working,
            stale_cleaned = execution.summary.stale_cleaned,
            redundant_compressed = execution.summary.redundant_compressed,
            orphaned_incrementals_cleaned = execution.summary.orphaned_incrementals_cleaned,
            vector_index_rows = execution.summary.vector_index_rows,
            snapshots_cleaned = execution.summary.snapshots_cleaned,
            orphan_branches_cleaned = execution.summary.orphan_branches_cleaned,
            warnings = execution.report.warnings.len(),
            decisions = execution.report.decisions.len(),
            "Governance task complete"
        );
        Ok(execution)
    }
}

fn read_enabled_flag() -> bool {
    std::env::var("MEMORIA_GOVERNANCE_ENABLED")
        .map(|v| v.to_lowercase() == "true")
        .unwrap_or(false)
}

/// Convenience type alias for use in AppState
pub type SchedulerHandle = Arc<GovernanceScheduler>;

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::Mutex;
    use crate::{GovernancePlan, GovernanceRunSummary, StrategyReport};

    #[derive(Default)]
    struct NoopStore;

    #[async_trait]
    impl GovernanceStore for NoopStore {
        async fn list_active_users(&self) -> Result<Vec<String>, crate::MemoriaError> {
            Ok(vec![])
        }
        async fn cleanup_tool_results(&self, _: i64) -> Result<i64, crate::MemoriaError> {
            Ok(0)
        }
        async fn archive_stale_working(
            &self,
            _: i64,
        ) -> Result<Vec<(String, i64)>, crate::MemoriaError> {
            Ok(vec![])
        }
        async fn cleanup_stale(&self, _: &str) -> Result<i64, crate::MemoriaError> {
            Ok(0)
        }
        async fn quarantine_low_confidence(&self, _: &str) -> Result<i64, crate::MemoriaError> {
            Ok(0)
        }
        async fn compress_redundant(
            &self,
            _: &str,
            _: f64,
            _: i64,
            _: usize,
        ) -> Result<i64, crate::MemoriaError> {
            Ok(0)
        }
        async fn cleanup_orphaned_incrementals(
            &self,
            _: &str,
            _: i64,
        ) -> Result<i64, crate::MemoriaError> {
            Ok(0)
        }
        async fn rebuild_vector_index(&self, _: &str) -> Result<i64, crate::MemoriaError> {
            Ok(0)
        }
        async fn cleanup_snapshots(&self, _: usize) -> Result<i64, crate::MemoriaError> {
            Ok(0)
        }
        async fn cleanup_orphan_branches(&self) -> Result<i64, crate::MemoriaError> {
            Ok(0)
        }
        async fn create_safety_snapshot(&self, _: &str) -> (Option<String>, Option<String>) {
            (Some("mem_snap_pre_daily_test".into()), None)
        }
        async fn log_edit(&self, _: &str, _: &str, _: &[&str], _: &str, _: Option<&str>) {}
    }

    struct FallbackStore;

    #[async_trait]
    impl GovernanceStore for FallbackStore {
        async fn list_active_users(&self) -> Result<Vec<String>, crate::MemoriaError> {
            Ok(vec!["u1".into()])
        }
        async fn cleanup_tool_results(&self, _: i64) -> Result<i64, crate::MemoriaError> {
            Ok(0)
        }
        async fn archive_stale_working(
            &self,
            _: i64,
        ) -> Result<Vec<(String, i64)>, crate::MemoriaError> {
            Ok(vec![])
        }
        async fn cleanup_stale(&self, _: &str) -> Result<i64, crate::MemoriaError> {
            Ok(1)
        }
        async fn quarantine_low_confidence(&self, _: &str) -> Result<i64, crate::MemoriaError> {
            Ok(2)
        }
        async fn compress_redundant(
            &self,
            _: &str,
            _: f64,
            _: i64,
            _: usize,
        ) -> Result<i64, crate::MemoriaError> {
            Ok(0)
        }
        async fn cleanup_orphaned_incrementals(
            &self,
            _: &str,
            _: i64,
        ) -> Result<i64, crate::MemoriaError> {
            Ok(0)
        }
        async fn rebuild_vector_index(&self, _: &str) -> Result<i64, crate::MemoriaError> {
            Ok(0)
        }
        async fn cleanup_snapshots(&self, _: usize) -> Result<i64, crate::MemoriaError> {
            Ok(0)
        }
        async fn cleanup_orphan_branches(&self) -> Result<i64, crate::MemoriaError> {
            Ok(0)
        }
        async fn create_safety_snapshot(&self, _: &str) -> (Option<String>, Option<String>) {
            (Some("mem_snap_pre_daily_fallback".into()), None)
        }
        async fn log_edit(&self, _: &str, _: &str, _: &[&str], _: &str, _: Option<&str>) {}
    }

    #[derive(Default)]
    struct RecordingStrategy {
        tasks: Mutex<Vec<GovernanceTask>>,
    }

    #[async_trait]
    impl GovernanceStrategy for RecordingStrategy {
        fn strategy_key(&self) -> &'static str {
            "governance:test:v1"
        }

        async fn plan(
            &self,
            _: &dyn GovernanceStore,
            task: GovernanceTask,
        ) -> Result<GovernancePlan, crate::MemoriaError> {
            self.tasks.lock().unwrap().push(task);
            Ok(GovernancePlan::default())
        }

        async fn execute(
            &self,
            _: &dyn GovernanceStore,
            _: GovernanceTask,
            _: &GovernancePlan,
        ) -> Result<GovernanceExecution, crate::MemoriaError> {
            Ok(GovernanceExecution {
                summary: GovernanceRunSummary::default(),
                report: StrategyReport::default(),
            })
        }
    }

    struct FailingStrategy;

    #[async_trait]
    impl GovernanceStrategy for FailingStrategy {
        fn strategy_key(&self) -> &'static str {
            "governance:failing:v1"
        }

        async fn plan(
            &self,
            _: &dyn GovernanceStore,
            _: GovernanceTask,
        ) -> Result<GovernancePlan, crate::MemoriaError> {
            Err(crate::MemoriaError::Internal("primary strategy failed".into()))
        }

        async fn execute(
            &self,
            _: &dyn GovernanceStore,
            _: GovernanceTask,
            _: &GovernancePlan,
        ) -> Result<GovernanceExecution, crate::MemoriaError> {
            unreachable!("failing strategy never reaches execute")
        }
    }

    #[tokio::test]
    async fn scheduler_delegates_task_execution_to_strategy() {
        let strategy = Arc::new(RecordingStrategy::default());
        let fallback = Arc::new(DefaultGovernanceStrategy);
        let scheduler = GovernanceScheduler::new_with_components(
            Some(Arc::new(NoopStore)),
            strategy.clone(),
            fallback,
            true,
        );

        let execution = scheduler
            .run_task(GovernanceTask::Daily)
            .await
            .expect("scheduler should delegate without error");

        assert_eq!(*strategy.tasks.lock().unwrap(), vec![GovernanceTask::Daily]);
        assert_eq!(execution.report.status, StrategyStatus::Success);
    }

    #[tokio::test]
    async fn scheduler_skips_task_when_store_is_missing() {
        let strategy = Arc::new(RecordingStrategy::default());
        let fallback = Arc::new(DefaultGovernanceStrategy);
        let scheduler =
            GovernanceScheduler::new_with_components(None, strategy.clone(), fallback, true);

        let execution = scheduler
            .run_task(GovernanceTask::Weekly)
            .await
            .expect("missing store should be treated as a no-op");

        assert!(strategy.tasks.lock().unwrap().is_empty());
        assert_eq!(execution.summary, GovernanceRunSummary::default());
    }

    #[tokio::test]
    async fn scheduler_falls_back_when_primary_strategy_fails() {
        let scheduler = GovernanceScheduler::new_with_components(
            Some(Arc::new(NoopStore)),
            Arc::new(FailingStrategy),
            Arc::new(DefaultGovernanceStrategy),
            true,
        );

        let execution = scheduler
            .run_task(GovernanceTask::Daily)
            .await
            .expect("scheduler should fall back to default strategy");

        assert_eq!(execution.report.status, StrategyStatus::Degraded);
        assert!(execution
            .report
            .warnings
            .iter()
            .any(|warning| warning.contains("Fell back")));
        assert_eq!(execution.report.metrics["governance.degraded"], 1.0);
    }

    #[tokio::test]
    async fn scheduler_fallback_uses_default_strategy_results() {
        let scheduler = GovernanceScheduler::new_with_components(
            Some(Arc::new(FallbackStore)),
            Arc::new(FailingStrategy),
            Arc::new(DefaultGovernanceStrategy),
            true,
        );

        let execution = scheduler
            .run_task(GovernanceTask::Daily)
            .await
            .expect("scheduler should return fallback default strategy results");

        assert_eq!(execution.report.status, StrategyStatus::Degraded);
        assert_eq!(execution.summary.users_processed, 1);
        assert_eq!(execution.summary.total_quarantined, 2);
        assert_eq!(execution.summary.total_cleaned, 1);
        assert!(!execution.report.decisions.is_empty());
        assert_eq!(execution.report.metrics["governance.snapshot_created"], 1.0);
    }
}
