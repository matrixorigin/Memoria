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
    config::Config,
    governance::{
        DefaultGovernanceStrategy, GovernanceExecution, GovernanceStore, GovernanceStrategy,
        GovernanceTask,
    },
    plugin::load_active_governance_plugin,
    plugin_registry::PluginRegistry,
    strategy_domain::StrategyStatus,
    MemoriaError, MemoryService,
};

pub struct GovernanceScheduler {
    store: Option<Arc<dyn GovernanceStore>>,
    strategy: Arc<dyn GovernanceStrategy>,
    fallback_strategy: Arc<dyn GovernanceStrategy>,
    enabled: bool,
    breaker_threshold: usize,
    breaker_cooldown: Duration,
}

const DEFAULT_BREAKER_THRESHOLD: usize = 3;
const DEFAULT_BREAKER_COOLDOWN_SECS: u64 = 300;

impl GovernanceScheduler {
    pub fn new(service: Arc<MemoryService>) -> Self {
        let default_strategy: Arc<dyn GovernanceStrategy> = Arc::new(DefaultGovernanceStrategy);
        Self::from_parts(service, default_strategy.clone(), default_strategy)
    }

    pub async fn from_config(
        service: Arc<MemoryService>,
        config: &Config,
    ) -> Result<Self, MemoriaError> {
        let delegate: Arc<dyn GovernanceStrategy> = Arc::new(DefaultGovernanceStrategy);
        if let Some(store) = &service.sql_store {
            if let Some(active_plugin) = load_active_governance_plugin(
                store.as_ref(),
                &config.governance_plugin_binding,
                delegate.clone(),
            )
            .await?
            {
                info!(
                    plugin_key = %active_plugin.plugin_key,
                    plugin_version = %active_plugin.version,
                    binding_key = %active_plugin.binding_key,
                    "Loaded governance plugin from shared repository binding"
                );
                return Ok(Self::from_parts(
                    service,
                    Arc::new(active_plugin.strategy),
                    delegate,
                ));
            }
        }

        Ok(Self::from_parts(service, delegate.clone(), delegate))
    }

    pub fn new_with_registry(
        service: Arc<MemoryService>,
        registry: &PluginRegistry,
        governance_key: &str,
    ) -> Result<Self, MemoriaError> {
        let store = service
            .sql_store
            .clone()
            .map(|store| -> Arc<dyn GovernanceStore> { store });
        Self::from_store_with_registry(store, registry, governance_key, read_enabled_flag())
    }

    pub fn from_store_with_registry(
        store: Option<Arc<dyn GovernanceStore>>,
        registry: &PluginRegistry,
        governance_key: &str,
        enabled: bool,
    ) -> Result<Self, MemoriaError> {
        let strategy = registry.create_governance(governance_key).ok_or_else(|| {
            MemoriaError::Blocked(format!(
                "Governance plugin `{governance_key}` is not registered"
            ))
        })?;
        let fallback_strategy: Arc<dyn GovernanceStrategy> = Arc::new(DefaultGovernanceStrategy);
        Ok(Self::new_with_components(
            store,
            strategy,
            fallback_strategy,
            enabled,
        ))
    }

    fn from_parts(
        service: Arc<MemoryService>,
        strategy: Arc<dyn GovernanceStrategy>,
        fallback_strategy: Arc<dyn GovernanceStrategy>,
    ) -> Self {
        let store = service
            .sql_store
            .clone()
            .map(|store| -> Arc<dyn GovernanceStore> { store });
        Self::new_with_components(store, strategy, fallback_strategy, read_enabled_flag())
    }

    fn new_with_components(
        store: Option<Arc<dyn GovernanceStore>>,
        strategy: Arc<dyn GovernanceStrategy>,
        fallback_strategy: Arc<dyn GovernanceStrategy>,
        enabled: bool,
    ) -> Self {
        Self::new_with_components_and_breaker(
            store,
            strategy,
            fallback_strategy,
            enabled,
            DEFAULT_BREAKER_THRESHOLD,
            Duration::from_secs(DEFAULT_BREAKER_COOLDOWN_SECS),
        )
    }

    fn new_with_components_and_breaker(
        store: Option<Arc<dyn GovernanceStore>>,
        strategy: Arc<dyn GovernanceStrategy>,
        fallback_strategy: Arc<dyn GovernanceStrategy>,
        enabled: bool,
        breaker_threshold: usize,
        breaker_cooldown: Duration,
    ) -> Self {
        Self {
            store,
            strategy,
            fallback_strategy,
            enabled,
            breaker_threshold,
            breaker_cooldown,
        }
    }

    pub fn strategy_key(&self) -> &str {
        self.strategy.strategy_key()
    }

    pub fn fallback_strategy_key(&self) -> &str {
        self.fallback_strategy.strategy_key()
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

    async fn run_task(
        &self,
        task: GovernanceTask,
    ) -> Result<GovernanceExecution, crate::MemoriaError> {
        let Some(store) = &self.store else {
            return Ok(GovernanceExecution::default());
        };

        let primary_key = self.strategy.strategy_key();
        let fallback_key = self.fallback_strategy.strategy_key();
        let breaker_remaining = store.check_shared_breaker(primary_key, task).await?;
        let mut execution = if let Some(remaining_secs) =
            breaker_remaining.filter(|_| primary_key != fallback_key)
        {
            warn!(
                task = task.as_str(),
                strategy = primary_key,
                fallback = fallback_key,
                remaining_secs,
                "Primary governance strategy circuit breaker is open; using fallback"
            );
            self.run_degraded_fallback(
                store.as_ref(),
                task,
                primary_key,
                fallback_key,
                format!(
                    "Primary strategy {primary_key} circuit breaker is open for another {remaining_secs}s. Fell back to {fallback_key}."
                ),
                true,
            )
            .await?
        } else {
            match self.strategy.run(store.as_ref(), task).await {
                Ok(execution) => {
                    store.clear_shared_breaker(primary_key, task).await?;
                    execution
                }
                Err(primary_err) => {
                    if primary_key == fallback_key {
                        return Err(primary_err);
                    }
                    let breaker_remaining = store
                        .record_shared_breaker_failure(
                            primary_key,
                            task,
                            self.breaker_threshold,
                            self.breaker_cooldown.as_secs() as i64,
                        )
                        .await?;
                    let breaker_opened = breaker_remaining.is_some();
                    warn!(
                        task = task.as_str(),
                        strategy = primary_key,
                        fallback = fallback_key,
                        %primary_err,
                        breaker_opened,
                        "Primary governance strategy failed; degrading to fallback"
                    );

                    let reason = if breaker_opened {
                        format!(
                            "Primary strategy {primary_key} failed: {primary_err}. Fell back to {fallback_key} and opened circuit breaker."
                        )
                    } else {
                        format!(
                            "Primary strategy {primary_key} failed: {primary_err}. Fell back to {fallback_key}."
                        )
                    };
                    self.run_degraded_fallback(
                        store.as_ref(),
                        task,
                        primary_key,
                        fallback_key,
                        reason,
                        breaker_opened,
                    )
                    .await?
                }
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

    async fn run_degraded_fallback(
        &self,
        store: &dyn GovernanceStore,
        task: GovernanceTask,
        primary_key: &str,
        fallback_key: &str,
        warning: String,
        circuit_open: bool,
    ) -> Result<GovernanceExecution, crate::MemoriaError> {
        let mut fallback_execution = self.fallback_strategy.run(store, task).await?;
        fallback_execution.report.status = StrategyStatus::Degraded;
        fallback_execution.report.warnings.push(warning);
        fallback_execution
            .report
            .metrics
            .insert("governance.degraded".to_string(), 1.0);
        if circuit_open {
            fallback_execution
                .report
                .metrics
                .insert("governance.circuit_open".to_string(), 1.0);
            fallback_execution.report.warnings.push(format!(
                "Primary strategy {primary_key} remains fenced off until the scheduler breaker cools down; fallback {fallback_key} handled this run."
            ));
        }
        Ok(fallback_execution)
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
    use crate::{
        Config, GovernancePlan, GovernanceRunSummary, HostPluginPolicy, PluginRegistry,
        StrategyReport,
    };
    use async_trait::async_trait;
    use memoria_core::{interfaces::MemoryStore, Memory};
    use std::fs;
    use std::sync::Mutex;
    use std::time::{SystemTime, UNIX_EPOCH};

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
    struct SharedBreakerStore {
        state: Mutex<TestBreakerState>,
    }

    #[derive(Default)]
    struct TestBreakerState {
        consecutive_failures: usize,
        remaining_secs: Option<i64>,
    }

    #[async_trait]
    impl GovernanceStore for SharedBreakerStore {
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
            (None, None)
        }
        async fn log_edit(&self, _: &str, _: &str, _: &[&str], _: &str, _: Option<&str>) {}

        async fn check_shared_breaker(
            &self,
            _: &str,
            _: GovernanceTask,
        ) -> Result<Option<i64>, crate::MemoriaError> {
            Ok(self.state.lock().unwrap().remaining_secs)
        }

        async fn record_shared_breaker_failure(
            &self,
            _: &str,
            _: GovernanceTask,
            threshold: usize,
            cooldown_secs: i64,
        ) -> Result<Option<i64>, crate::MemoriaError> {
            let mut state = self.state.lock().unwrap();
            if state.remaining_secs.is_some() {
                return Ok(state.remaining_secs);
            }
            state.consecutive_failures += 1;
            if state.consecutive_failures >= threshold {
                state.consecutive_failures = 0;
                state.remaining_secs = Some(cooldown_secs);
            }
            Ok(state.remaining_secs)
        }

        async fn clear_shared_breaker(
            &self,
            _: &str,
            _: GovernanceTask,
        ) -> Result<(), crate::MemoriaError> {
            let mut state = self.state.lock().unwrap();
            state.consecutive_failures = 0;
            state.remaining_secs = None;
            Ok(())
        }
    }

    #[derive(Default)]
    struct RecordingStrategy {
        tasks: Mutex<Vec<GovernanceTask>>,
    }

    #[async_trait]
    impl GovernanceStrategy for RecordingStrategy {
        fn strategy_key(&self) -> &str {
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
        fn strategy_key(&self) -> &str {
            "governance:failing:v1"
        }

        async fn plan(
            &self,
            _: &dyn GovernanceStore,
            _: GovernanceTask,
        ) -> Result<GovernancePlan, crate::MemoriaError> {
            Err(crate::MemoriaError::Internal(
                "primary strategy failed".into(),
            ))
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

    #[derive(Default)]
    struct CountingFailingStrategy {
        calls: Mutex<usize>,
    }

    #[async_trait]
    impl GovernanceStrategy for CountingFailingStrategy {
        fn strategy_key(&self) -> &str {
            "governance:counting-failure:v1"
        }

        async fn plan(
            &self,
            _: &dyn GovernanceStore,
            _: GovernanceTask,
        ) -> Result<GovernancePlan, crate::MemoriaError> {
            *self.calls.lock().unwrap() += 1;
            Err(crate::MemoriaError::Internal(
                "counting primary strategy failed".into(),
            ))
        }

        async fn execute(
            &self,
            _: &dyn GovernanceStore,
            _: GovernanceTask,
            _: &GovernancePlan,
        ) -> Result<GovernanceExecution, crate::MemoriaError> {
            unreachable!("counting failure strategy never reaches execute")
        }
    }

    #[derive(Default)]
    struct FlakyStrategy {
        calls: Mutex<usize>,
    }

    #[async_trait]
    impl GovernanceStrategy for FlakyStrategy {
        fn strategy_key(&self) -> &str {
            "governance:flaky:v1"
        }

        async fn plan(
            &self,
            _: &dyn GovernanceStore,
            _: GovernanceTask,
        ) -> Result<GovernancePlan, crate::MemoriaError> {
            let mut calls = self.calls.lock().unwrap();
            *calls += 1;
            if *calls == 1 {
                Err(crate::MemoriaError::Internal("flaky primary failed".into()))
            } else {
                Ok(GovernancePlan::default())
            }
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

    #[derive(Default)]
    struct NoopMemoryStore;

    #[async_trait]
    impl MemoryStore for NoopMemoryStore {
        async fn insert(&self, _: &Memory) -> Result<(), crate::MemoriaError> {
            Ok(())
        }

        async fn get(&self, _: &str) -> Result<Option<Memory>, crate::MemoriaError> {
            Ok(None)
        }

        async fn update(&self, _: &Memory) -> Result<(), crate::MemoriaError> {
            Ok(())
        }

        async fn soft_delete(&self, _: &str) -> Result<(), crate::MemoriaError> {
            Ok(())
        }

        async fn list_active(&self, _: &str, _: i64) -> Result<Vec<Memory>, crate::MemoriaError> {
            Ok(vec![])
        }

        async fn search_fulltext(
            &self,
            _: &str,
            _: &str,
            _: i64,
        ) -> Result<Vec<Memory>, crate::MemoriaError> {
            Ok(vec![])
        }

        async fn search_vector(
            &self,
            _: &str,
            _: &[f32],
            _: i64,
        ) -> Result<Vec<Memory>, crate::MemoriaError> {
            Ok(vec![])
        }
    }

    fn temp_plugin_dir(name: &str) -> std::path::PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("memoria-scheduler-plugin-{name}-{nonce}"))
    }

    fn write_manifest(dir: &std::path::Path, script: &str, timeout_ms: u64) {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join("plugin.rhai"), script).unwrap();
        let mut manifest = serde_json::json!({
            "name": "memoria-governance-scheduler-test",
            "version": "1.0.0",
            "api_version": "v1",
            "runtime": "rhai",
            "entry": { "rhai": { "script": "plugin.rhai", "entrypoint": "memoria_plugin" } },
            "capabilities": ["governance.plan", "governance.execute"],
            "compatibility": { "memoria": ">=0.1.0-rc1 <0.2.0" },
            "permissions": { "network": false, "filesystem": false, "env": [] },
            "limits": { "timeout_ms": timeout_ms, "max_memory_mb": 64, "max_output_bytes": 16384 },
            "integrity": { "sha256": "", "signature": "dev-signature", "signer": "dev-signer" },
            "metadata": { "display_name": "Scheduler Test Plugin" }
        });
        fs::write(
            dir.join("manifest.json"),
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();
        let sha = crate::plugin::compute_package_sha256(dir).unwrap();
        manifest["integrity"]["sha256"] = serde_json::Value::String(sha);
        fs::write(
            dir.join("manifest.json"),
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();
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

    #[tokio::test]
    async fn scheduler_opens_circuit_after_repeated_failures() {
        let primary = Arc::new(CountingFailingStrategy::default());
        let scheduler = GovernanceScheduler::new_with_components_and_breaker(
            Some(Arc::new(SharedBreakerStore::default())),
            primary.clone(),
            Arc::new(DefaultGovernanceStrategy),
            true,
            2,
            Duration::from_secs(60),
        );

        let first = scheduler.run_task(GovernanceTask::Daily).await.unwrap();
        let second = scheduler.run_task(GovernanceTask::Daily).await.unwrap();
        let third = scheduler.run_task(GovernanceTask::Daily).await.unwrap();

        assert_eq!(*primary.calls.lock().unwrap(), 2);
        assert_eq!(first.report.status, StrategyStatus::Degraded);
        assert_eq!(second.report.status, StrategyStatus::Degraded);
        assert_eq!(third.report.status, StrategyStatus::Degraded);
        assert_eq!(
            third.report.metrics.get("governance.circuit_open"),
            Some(&1.0)
        );
        assert!(third
            .report
            .warnings
            .iter()
            .any(|warning| warning.contains("circuit breaker is open")));
    }

    #[tokio::test]
    async fn scheduler_resets_circuit_after_primary_success() {
        let primary = Arc::new(FlakyStrategy::default());
        let scheduler = GovernanceScheduler::new_with_components_and_breaker(
            Some(Arc::new(SharedBreakerStore::default())),
            primary.clone(),
            Arc::new(DefaultGovernanceStrategy),
            true,
            2,
            Duration::from_secs(60),
        );

        let first = scheduler.run_task(GovernanceTask::Daily).await.unwrap();
        let second = scheduler.run_task(GovernanceTask::Daily).await.unwrap();

        assert_eq!(*primary.calls.lock().unwrap(), 2);
        assert_eq!(first.report.status, StrategyStatus::Degraded);
        assert_eq!(second.report.status, StrategyStatus::Success);
        assert!(second
            .report
            .metrics
            .get("governance.circuit_open")
            .is_none());
    }

    #[tokio::test]
    async fn scheduler_uses_registered_rhai_plugin() {
        let dir = temp_plugin_dir("registry");
        write_manifest(
            &dir,
            r#"
                fn memoria_plugin(ctx) {
                    if ctx["phase"] == "plan" {
                        return #{
                            requires_approval: true,
                            actions: [ decision("plugin:scheduler", "scheduler loaded plugin", 0.8) ]
                        };
                    }
                    return #{ "warnings": ["scheduler plugin executed"], "metrics": #{ "plugin.scheduler.executed": 1.0 } };
                }
            "#,
            200,
        );

        let mut registry = PluginRegistry::new();
        let key = registry
            .register_rhai_governance_plugin(
                &dir,
                HostPluginPolicy::development(),
                Arc::new(DefaultGovernanceStrategy),
            )
            .unwrap();

        let scheduler = GovernanceScheduler::from_store_with_registry(
            Some(Arc::new(NoopStore)),
            &registry,
            &key,
            true,
        )
        .unwrap();
        let execution = scheduler.run_task(GovernanceTask::Weekly).await.unwrap();

        assert_eq!(execution.report.status, StrategyStatus::Success);
        assert!(execution
            .report
            .warnings
            .iter()
            .any(|warning| warning.contains("scheduler plugin executed")));
        assert_eq!(
            execution.report.metrics.get("plugin.scheduler.executed"),
            Some(&1.0)
        );
        assert!(registry.governance_metadata(&key).is_some());

        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn scheduler_falls_back_when_registered_plugin_exceeds_limits() {
        let dir = temp_plugin_dir("limit");
        write_manifest(
            &dir,
            r#"
                fn memoria_plugin(ctx) {
                    if ctx["phase"] == "plan" {
                        while true {}
                    }
                    return #{};
                }
            "#,
            5,
        );

        let mut registry = PluginRegistry::new();
        let key = registry
            .register_rhai_governance_plugin(
                &dir,
                HostPluginPolicy::development(),
                Arc::new(DefaultGovernanceStrategy),
            )
            .unwrap();

        let scheduler = GovernanceScheduler::from_store_with_registry(
            Some(Arc::new(FallbackStore)),
            &registry,
            &key,
            true,
        )
        .unwrap();
        let execution = scheduler.run_task(GovernanceTask::Daily).await.unwrap();

        assert_eq!(execution.report.status, StrategyStatus::Degraded);
        assert!(execution
            .report
            .warnings
            .iter()
            .any(|warning| warning.contains("Fell back")));
        assert_eq!(
            execution.report.metrics.get("governance.degraded"),
            Some(&1.0)
        );

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn scheduler_from_config_without_shared_store_uses_default_strategy() {
        let config = Config {
            db_url: "mysql://root:111@localhost:6001/memoria".into(),
            db_name: "memoria".into(),
            embedding_provider: "mock".into(),
            embedding_model: "BAAI/bge-m3".into(),
            embedding_dim: 1024,
            embedding_api_key: String::new(),
            embedding_base_url: String::new(),
            llm_api_key: None,
            llm_base_url: "https://api.openai.com/v1".into(),
            llm_model: "gpt-4o-mini".into(),
            user: "default".into(),
            governance_plugin_binding: "default".into(),
        };
        let service = Arc::new(MemoryService::new(Arc::new(NoopMemoryStore), None));

        let scheduler = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(GovernanceScheduler::from_config(service, &config))
            .unwrap();

        assert_eq!(scheduler.strategy.strategy_key(), "governance:default:v1");
        assert_eq!(scheduler.fallback_strategy.strategy_key(), "governance:default:v1");
    }
}
