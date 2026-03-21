use std::collections::HashMap;

use async_trait::async_trait;
use memoria_core::MemoriaError;
use memoria_storage::SqlMemoryStore;
use tracing::error;

use crate::strategy_domain::{StrategyDecision, StrategyEvidence, StrategyReport, StrategyStatus};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GovernanceTask {
    Hourly,
    Daily,
    Weekly,
}

impl GovernanceTask {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Hourly => "hourly",
            Self::Daily => "daily",
            Self::Weekly => "weekly",
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct GovernanceRunSummary {
    pub users_processed: usize,
    pub total_quarantined: i64,
    pub total_cleaned: i64,
    pub tool_results_cleaned: i64,
    pub async_tasks_cleaned: i64,
    pub archived_working: i64,
    pub stale_cleaned: i64,
    pub redundant_compressed: i64,
    pub orphaned_incrementals_cleaned: i64,
    pub vector_index_rows: i64,
    pub snapshots_cleaned: i64,
    pub orphan_branches_cleaned: i64,
    pub users_tuned: i64,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct GovernancePlan {
    pub actions: Vec<StrategyDecision>,
    pub estimated_impact: HashMap<String, f64>,
    pub requires_approval: bool,
    pub users: Vec<String>,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct GovernanceExecution {
    pub summary: GovernanceRunSummary,
    pub report: StrategyReport,
}

/// Storage adapter for governance operations.
#[async_trait]
pub trait GovernanceStore: Send + Sync {
    /// Enumerate active users that should be considered by the current task.
    async fn list_active_users(&self) -> Result<Vec<String>, MemoriaError>;

    /// Clean up expired tool-result rows and return the affected row count.
    async fn cleanup_tool_results(&self, ttl_hours: i64) -> Result<i64, MemoriaError>;

    /// Clean up completed/failed async tasks older than `ttl_hours`.
    async fn cleanup_async_tasks(&self, ttl_hours: i64) -> Result<i64, MemoriaError>;

    /// Archive stale working memories and return per-user affected counts.
    async fn archive_stale_working(
        &self,
        stale_hours: i64,
    ) -> Result<Vec<(String, i64)>, MemoriaError>;

    /// Delete stale memories for one user and return the affected row count.
    async fn cleanup_stale(&self, user_id: &str) -> Result<i64, MemoriaError>;

    /// Quarantine low-confidence memories for one user.
    async fn quarantine_low_confidence(&self, user_id: &str) -> Result<i64, MemoriaError>;

    /// Compress redundant memories for one user using the given matching guardrails.
    async fn compress_redundant(
        &self,
        user_id: &str,
        similarity_threshold: f64,
        window_days: i64,
        max_pairs: usize,
    ) -> Result<i64, MemoriaError>;

    /// Remove orphaned incremental rows for one user.
    async fn cleanup_orphaned_incrementals(
        &self,
        user_id: &str,
        older_than_hours: i64,
    ) -> Result<i64, MemoriaError>;

    /// Rebuild the vector index for the target table and report touched rows.
    async fn rebuild_vector_index(&self, table: &str) -> Result<i64, MemoriaError>;

    /// Drop old milestone snapshots while keeping the latest `keep_last_n`.
    async fn cleanup_snapshots(&self, keep_last_n: usize) -> Result<i64, MemoriaError>;

    /// Remove leftover sandbox branches.
    async fn cleanup_orphan_branches(&self) -> Result<i64, MemoriaError>;

    /// Remove orphaned stats records (stats without corresponding memory).
    async fn cleanup_orphan_stats(&self) -> Result<i64, MemoriaError>;

    /// Delete old audit-log rows, keeping only the last `retain_days` days.
    async fn cleanup_edit_log(&self, retain_days: i64) -> Result<i64, MemoriaError>;

    /// Delete old feedback rows, keeping only the last `retain_days` days.
    async fn cleanup_feedback(&self, retain_days: i64) -> Result<i64, MemoriaError>;

    /// Create a rollback snapshot before destructive governance work begins.
    async fn create_safety_snapshot(&self, operation: &str) -> (Option<String>, Option<String>);

    /// Persist an audit-log entry for the governance operation.
    async fn log_edit(
        &self,
        user_id: &str,
        operation: &str,
        memory_id: Option<&str>,
        payload: Option<&str>,
        reason: &str,
        snapshot_before: Option<&str>,
    );

    /// Returns remaining seconds if the shared governance breaker is open.
    async fn check_shared_breaker(
        &self,
        _strategy_key: &str,
        _task: GovernanceTask,
    ) -> Result<Option<i64>, MemoriaError> {
        Ok(None)
    }

    /// Records a primary strategy failure and returns remaining open time if the breaker is open.
    async fn record_shared_breaker_failure(
        &self,
        _strategy_key: &str,
        _task: GovernanceTask,
        _threshold: usize,
        _cooldown_secs: i64,
    ) -> Result<Option<i64>, MemoriaError> {
        Ok(None)
    }

    /// Clears any shared breaker state after a successful primary run.
    async fn clear_shared_breaker(
        &self,
        _strategy_key: &str,
        _task: GovernanceTask,
    ) -> Result<(), MemoriaError> {
        Ok(())
    }

    /// Auto-tune retrieval parameters for a user based on feedback history.
    /// Returns true if parameters were updated.
    async fn tune_user_retrieval_params(&self, _user_id: &str) -> Result<bool, MemoriaError> {
        Ok(false)
    }
}

#[async_trait]
impl GovernanceStore for SqlMemoryStore {
    async fn list_active_users(&self) -> Result<Vec<String>, MemoriaError> {
        let users: Vec<(String,)> =
            sqlx::query_as("SELECT DISTINCT user_id FROM mem_memories WHERE is_active > 0")
                .fetch_all(self.pool())
                .await
                .map_err(|e| MemoriaError::Database(e.to_string()))?;
        Ok(users.into_iter().map(|(user_id,)| user_id).collect())
    }

    async fn cleanup_tool_results(&self, ttl_hours: i64) -> Result<i64, MemoriaError> {
        SqlMemoryStore::cleanup_tool_results(self, ttl_hours).await
    }

    async fn cleanup_async_tasks(&self, ttl_hours: i64) -> Result<i64, MemoriaError> {
        let result = sqlx::query(
            "DELETE FROM mem_async_tasks WHERE status IN ('completed', 'failed') \
             AND updated_at < DATE_SUB(NOW(), INTERVAL ? HOUR)",
        )
        .bind(ttl_hours)
        .execute(self.pool())
        .await
        .map_err(|e| MemoriaError::Database(e.to_string()))?;
        Ok(result.rows_affected() as i64)
    }

    async fn archive_stale_working(
        &self,
        stale_hours: i64,
    ) -> Result<Vec<(String, i64)>, MemoriaError> {
        SqlMemoryStore::archive_stale_working(self, stale_hours).await
    }

    async fn cleanup_stale(&self, user_id: &str) -> Result<i64, MemoriaError> {
        SqlMemoryStore::cleanup_stale(self, user_id).await
    }

    async fn quarantine_low_confidence(&self, user_id: &str) -> Result<i64, MemoriaError> {
        SqlMemoryStore::quarantine_low_confidence(self, user_id).await
    }

    async fn compress_redundant(
        &self,
        user_id: &str,
        similarity_threshold: f64,
        window_days: i64,
        max_pairs: usize,
    ) -> Result<i64, MemoriaError> {
        SqlMemoryStore::compress_redundant(
            self,
            user_id,
            similarity_threshold,
            window_days,
            max_pairs,
        )
        .await
    }

    async fn cleanup_orphaned_incrementals(
        &self,
        user_id: &str,
        older_than_hours: i64,
    ) -> Result<i64, MemoriaError> {
        SqlMemoryStore::cleanup_orphaned_incrementals(self, user_id, older_than_hours).await
    }

    async fn rebuild_vector_index(&self, table: &str) -> Result<i64, MemoriaError> {
        SqlMemoryStore::rebuild_vector_index(self, table).await
    }

    async fn cleanup_snapshots(&self, keep_last_n: usize) -> Result<i64, MemoriaError> {
        SqlMemoryStore::cleanup_snapshots(self, keep_last_n).await
    }

    async fn cleanup_orphan_branches(&self) -> Result<i64, MemoriaError> {
        SqlMemoryStore::cleanup_orphan_branches(self).await
    }

    async fn cleanup_orphan_stats(&self) -> Result<i64, MemoriaError> {
        SqlMemoryStore::cleanup_orphan_stats(self).await
    }

    async fn cleanup_edit_log(&self, retain_days: i64) -> Result<i64, MemoriaError> {
        SqlMemoryStore::cleanup_edit_log(self, retain_days).await
    }

    async fn cleanup_feedback(&self, retain_days: i64) -> Result<i64, MemoriaError> {
        SqlMemoryStore::cleanup_feedback(self, retain_days).await
    }

    async fn create_safety_snapshot(&self, operation: &str) -> (Option<String>, Option<String>) {
        SqlMemoryStore::create_safety_snapshot(self, operation).await
    }

    async fn log_edit(
        &self,
        user_id: &str,
        operation: &str,
        memory_id: Option<&str>,
        payload: Option<&str>,
        reason: &str,
        snapshot_before: Option<&str>,
    ) {
        SqlMemoryStore::log_edit(self, user_id, operation, memory_id, payload, reason, snapshot_before).await;
    }

    async fn check_shared_breaker(
        &self,
        strategy_key: &str,
        task: GovernanceTask,
    ) -> Result<Option<i64>, MemoriaError> {
        SqlMemoryStore::check_governance_runtime_breaker(self, strategy_key, task.as_str()).await
    }

    async fn record_shared_breaker_failure(
        &self,
        strategy_key: &str,
        task: GovernanceTask,
        threshold: usize,
        cooldown_secs: i64,
    ) -> Result<Option<i64>, MemoriaError> {
        SqlMemoryStore::record_governance_runtime_failure(
            self,
            strategy_key,
            task.as_str(),
            threshold,
            cooldown_secs,
        )
        .await
    }

    async fn clear_shared_breaker(
        &self,
        strategy_key: &str,
        task: GovernanceTask,
    ) -> Result<(), MemoriaError> {
        SqlMemoryStore::clear_governance_runtime_breaker(self, strategy_key, task.as_str()).await
    }

    async fn tune_user_retrieval_params(&self, user_id: &str) -> Result<bool, MemoriaError> {
        use crate::scoring::{DefaultScoringPlugin, ScoringPlugin};

        let plugin = DefaultScoringPlugin;
        match plugin.tune_params(self, user_id).await? {
            Some(_) => Ok(true),
            None => Ok(false),
        }
    }
}

/// Governance strategy contract for scheduler-driven maintenance work.
#[async_trait]
pub trait GovernanceStrategy: Send + Sync {
    /// Stable strategy key, e.g. `governance:default:v1`.
    fn strategy_key(&self) -> &str;

    /// Produce the execution plan and estimated impact for a governance task.
    async fn plan(
        &self,
        store: &dyn GovernanceStore,
        task: GovernanceTask,
    ) -> Result<GovernancePlan, MemoriaError>;

    /// Execute a previously generated governance plan.
    async fn execute(
        &self,
        store: &dyn GovernanceStore,
        task: GovernanceTask,
        plan: &GovernancePlan,
    ) -> Result<GovernanceExecution, MemoriaError>;

    async fn run(
        &self,
        store: &dyn GovernanceStore,
        task: GovernanceTask,
    ) -> Result<GovernanceExecution, MemoriaError> {
        let plan = self.plan(store, task).await?;
        self.execute(store, task, &plan).await
    }
}

#[derive(Debug, Default)]
pub struct DefaultGovernanceStrategy;

#[derive(Debug)]
struct ExecutionState {
    summary: GovernanceRunSummary,
    decisions: Vec<StrategyDecision>,
    warnings: Vec<String>,
    snapshot_before: Option<String>,
}

impl DefaultGovernanceStrategy {
    const TOOL_RESULT_TTL_HOURS: i64 = 72;
    const ASYNC_TASK_TTL_HOURS: i64 = 72;
    const STALE_WORKING_HOURS: i64 = 24;
    const REDUNDANCY_SIMILARITY_THRESHOLD: f64 = 0.95;
    const REDUNDANCY_WINDOW_DAYS: i64 = 30;
    const REDUNDANCY_MAX_PAIRS: usize = 10_000;
    const ORPHANED_INCREMENTALS_HOURS: i64 = 24;
    const SNAPSHOTS_TO_KEEP: usize = 5;
    const EDIT_LOG_RETAIN_DAYS: i64 = 90;
    const FEEDBACK_RETAIN_DAYS: i64 = 180;
    const VECTOR_INDEX_TABLE: &'static str = "mem_memories";

    async fn cleanup_tool_results_operation(
        &self,
        plan: &GovernancePlan,
        store: &dyn GovernanceStore,
        state: &mut ExecutionState,
    ) {
        match store
            .cleanup_tool_results(Self::TOOL_RESULT_TTL_HOURS)
            .await
        {
            Ok(cleaned) => {
                state.summary.tool_results_cleaned = cleaned;
                state.decisions.push(governance_decision(
                    GovernanceTask::Hourly,
                    "cleanup_tool_results",
                    format!(
                        "Removed {cleaned} expired tool-result rows older than {} hours",
                        Self::TOOL_RESULT_TTL_HOURS
                    ),
                    Some(if cleaned > 0 { 1.0 } else { 0.0 }),
                    vec![StrategyEvidence {
                        source: "governance.plan".to_string(),
                        summary: format!("Task targeted {} active users", plan.users.len()),
                        score: Some(plan.users.len() as f32),
                        references: vec![format!("task:{}", GovernanceTask::Hourly.as_str())],
                    }],
                    state.snapshot_before.as_deref(),
                ));
            }
            Err(err) => record_warning(
                GovernanceTask::Hourly,
                None,
                "cleanup_tool_results",
                &err,
                &mut state.warnings,
            ),
        }
    }

    async fn cleanup_async_tasks_operation(
        &self,
        store: &dyn GovernanceStore,
        state: &mut ExecutionState,
    ) {
        match store.cleanup_async_tasks(Self::ASYNC_TASK_TTL_HOURS).await {
            Ok(cleaned) => {
                state.summary.async_tasks_cleaned = cleaned;
                state.decisions.push(governance_decision(
                    GovernanceTask::Daily,
                    "cleanup_async_tasks",
                    format!(
                        "Removed {cleaned} completed/failed async tasks older than {} hours",
                        Self::ASYNC_TASK_TTL_HOURS
                    ),
                    Some(if cleaned > 0 { 1.0 } else { 0.0 }),
                    vec![],
                    state.snapshot_before.as_deref(),
                ));
            }
            Err(err) => record_warning(
                GovernanceTask::Daily,
                None,
                "cleanup_async_tasks",
                &err,
                &mut state.warnings,
            ),
        }
    }

    async fn archive_stale_working_operation(
        &self,
        store: &dyn GovernanceStore,
        state: &mut ExecutionState,
    ) {
        match store.archive_stale_working(Self::STALE_WORKING_HOURS).await {
            Ok(per_user) => {
                let mut affected_users = Vec::new();
                for (user_id, count) in per_user {
                    if count > 0 {
                        store
                            .log_edit(
                                &user_id,
                                "governance:archive_working",
                                None,
                                Some(&format!("{{\"archived\":{count},\"threshold_hours\":{}}}", Self::STALE_WORKING_HOURS)),
                                &format!(
                                    "archived {count} stale working memories (>{}h)",
                                    Self::STALE_WORKING_HOURS
                                ),
                                state.snapshot_before.as_deref(),
                            )
                            .await;
                        affected_users.push(user_id);
                        state.summary.archived_working += count;
                        state.summary.total_cleaned += count;
                    }
                }
                state.decisions.push(governance_decision(
                    GovernanceTask::Hourly,
                    "archive_stale_working",
                    format!(
                        "Archived {} stale working memories across {} users",
                        state.summary.archived_working,
                        affected_users.len()
                    ),
                    Some(if state.summary.archived_working > 0 {
                        1.0
                    } else {
                        0.0
                    }),
                    vec![task_evidence(
                        GovernanceTask::Hourly,
                        &affected_users,
                        format!(
                            "Used stale-working threshold of {} hours",
                            Self::STALE_WORKING_HOURS
                        ),
                    )],
                    state.snapshot_before.as_deref(),
                ));
            }
            Err(err) => record_warning(
                GovernanceTask::Hourly,
                None,
                "archive_stale_working",
                &err,
                &mut state.warnings,
            ),
        }
    }

    async fn cleanup_stale_operation(
        &self,
        task: GovernanceTask,
        store: &dyn GovernanceStore,
        users: &[String],
        state: &mut ExecutionState,
    ) {
        let mut cleaned_users = Vec::new();
        for user_id in users {
            match store.cleanup_stale(user_id).await {
                Ok(cleaned) => {
                    state.summary.stale_cleaned += cleaned;
                    state.summary.total_cleaned += cleaned;
                    if cleaned > 0 {
                        cleaned_users.push(user_id.clone());
                        store
                            .log_edit(
                                user_id,
                                "governance:cleanup_stale",
                                None,
                                Some(&format!("{{\"cleaned_stale\":{cleaned}}}")),
                                &format!("cleaned {cleaned}"),
                                state.snapshot_before.as_deref(),
                            )
                            .await;
                    }
                }
                Err(err) => record_warning(
                    task,
                    Some(user_id),
                    "cleanup_stale",
                    &err,
                    &mut state.warnings,
                ),
            }
        }
        state.decisions.push(governance_decision(
            task,
            "cleanup_stale",
            format!(
                "Cleaned {} stale memories across {} users",
                state.summary.stale_cleaned,
                cleaned_users.len()
            ),
            Some(if state.summary.stale_cleaned > 0 {
                1.0
            } else {
                0.0
            }),
            vec![task_evidence(
                task,
                &cleaned_users,
                if task == GovernanceTask::Hourly {
                    "Per-user stale cleanup finished".to_string()
                } else {
                    "Daily stale-memory cleanup completed".to_string()
                },
            )],
            state.snapshot_before.as_deref(),
        ));
    }

    async fn quarantine_operation(
        &self,
        users: &[String],
        store: &dyn GovernanceStore,
        state: &mut ExecutionState,
    ) {
        let mut quarantined_users = Vec::new();
        for user_id in users {
            match store.quarantine_low_confidence(user_id).await {
                Ok(count) => {
                    state.summary.total_quarantined += count;
                    if count > 0 {
                        quarantined_users.push(user_id.clone());
                        store
                            .log_edit(
                                user_id,
                                "governance:quarantine",
                                None,
                                Some(&format!("{{\"quarantined\":{count}}}")),
                                &format!("quarantined {count}"),
                                state.snapshot_before.as_deref(),
                            )
                            .await;
                    }
                }
                Err(err) => record_warning(
                    GovernanceTask::Daily,
                    Some(user_id),
                    "quarantine",
                    &err,
                    &mut state.warnings,
                ),
            }
        }

        state.decisions.push(governance_decision(
            GovernanceTask::Daily,
            "quarantine",
            format!(
                "Quarantined {} low-confidence memories across {} users",
                state.summary.total_quarantined,
                quarantined_users.len()
            ),
            Some(if state.summary.total_quarantined > 0 {
                1.0
            } else {
                0.0
            }),
            vec![task_evidence(
                GovernanceTask::Daily,
                &quarantined_users,
                "Daily trust guardrail sweep completed".to_string(),
            )],
            state.snapshot_before.as_deref(),
        ));
    }

    async fn compress_redundant_operation(
        &self,
        users: &[String],
        store: &dyn GovernanceStore,
        state: &mut ExecutionState,
    ) {
        let mut compressed_users = Vec::new();
        for user_id in users {
            match store
                .compress_redundant(
                    user_id,
                    Self::REDUNDANCY_SIMILARITY_THRESHOLD,
                    Self::REDUNDANCY_WINDOW_DAYS,
                    Self::REDUNDANCY_MAX_PAIRS,
                )
                .await
            {
                Ok(count) => {
                    state.summary.redundant_compressed += count;
                    if count > 0 {
                        compressed_users.push(user_id.clone());
                        store
                            .log_edit(
                                user_id,
                                "governance:compress_redundant",
                                None,
                                Some(&format!("{{\"compressed\":{count}}}")),
                                &format!("compressed {count}"),
                                state.snapshot_before.as_deref(),
                            )
                            .await;
                    }
                }
                Err(err) => record_warning(
                    GovernanceTask::Daily,
                    Some(user_id),
                    "compress_redundant",
                    &err,
                    &mut state.warnings,
                ),
            }
        }

        state.decisions.push(governance_decision(
            GovernanceTask::Daily,
            "compress_redundant",
            format!(
                "Compressed {} redundant memories across {} users",
                state.summary.redundant_compressed,
                compressed_users.len()
            ),
            Some(if state.summary.redundant_compressed > 0 {
                1.0
            } else {
                0.0
            }),
            vec![task_evidence(
                GovernanceTask::Daily,
                &compressed_users,
                format!(
                    "Similarity threshold {:.2}, window {} days, max pairs {}",
                    Self::REDUNDANCY_SIMILARITY_THRESHOLD,
                    Self::REDUNDANCY_WINDOW_DAYS,
                    Self::REDUNDANCY_MAX_PAIRS
                ),
            )],
            state.snapshot_before.as_deref(),
        ));
    }

    async fn cleanup_orphaned_incrementals_operation(
        &self,
        users: &[String],
        store: &dyn GovernanceStore,
        state: &mut ExecutionState,
    ) {
        let mut orphan_cleanup_users = Vec::new();
        for user_id in users {
            match store
                .cleanup_orphaned_incrementals(user_id, Self::ORPHANED_INCREMENTALS_HOURS)
                .await
            {
                Ok(count) => {
                    state.summary.orphaned_incrementals_cleaned += count;
                    if count > 0 {
                        orphan_cleanup_users.push(user_id.clone());
                        store
                            .log_edit(
                                user_id,
                                "governance:cleanup_orphaned_incrementals",
                                None,
                                Some(&format!("{{\"cleaned_orphaned\":{count}}}")),
                                &format!("cleaned {count}"),
                                state.snapshot_before.as_deref(),
                            )
                            .await;
                    }
                }
                Err(err) => record_warning(
                    GovernanceTask::Daily,
                    Some(user_id),
                    "cleanup_orphaned_incrementals",
                    &err,
                    &mut state.warnings,
                ),
            }
        }

        state.decisions.push(governance_decision(
            GovernanceTask::Daily,
            "cleanup_orphaned_incrementals",
            format!(
                "Removed {} orphaned incrementals across {} users",
                state.summary.orphaned_incrementals_cleaned,
                orphan_cleanup_users.len()
            ),
            Some(if state.summary.orphaned_incrementals_cleaned > 0 {
                1.0
            } else {
                0.0
            }),
            vec![task_evidence(
                GovernanceTask::Daily,
                &orphan_cleanup_users,
                format!(
                    "Orphaned incrementals older than {} hours were targeted",
                    Self::ORPHANED_INCREMENTALS_HOURS
                ),
            )],
            state.snapshot_before.as_deref(),
        ));
    }

    async fn rebuild_vector_index_operation(
        &self,
        plan: &GovernancePlan,
        store: &dyn GovernanceStore,
        state: &mut ExecutionState,
    ) {
        match store.rebuild_vector_index(Self::VECTOR_INDEX_TABLE).await {
            Ok(value) => {
                state.summary.vector_index_rows = value;
                state.decisions.push(governance_decision(
                    GovernanceTask::Weekly,
                    "rebuild_vector_index",
                    format!(
                        "Rebuilt vector index for {} and touched {} rows",
                        Self::VECTOR_INDEX_TABLE,
                        value
                    ),
                    Some(if value > 0 { 1.0 } else { 0.0 }),
                    vec![task_evidence(
                        GovernanceTask::Weekly,
                        &plan.users,
                        format!("Rebuilt table {}", Self::VECTOR_INDEX_TABLE),
                    )],
                    state.snapshot_before.as_deref(),
                ));
            }
            Err(err) => record_warning(
                GovernanceTask::Weekly,
                None,
                "rebuild_vector_index",
                &err,
                &mut state.warnings,
            ),
        }
    }

    async fn cleanup_snapshots_operation(
        &self,
        plan: &GovernancePlan,
        store: &dyn GovernanceStore,
        state: &mut ExecutionState,
    ) {
        match store.cleanup_snapshots(Self::SNAPSHOTS_TO_KEEP).await {
            Ok(value) => {
                state.summary.snapshots_cleaned = value;
                state.decisions.push(governance_decision(
                    GovernanceTask::Weekly,
                    "cleanup_snapshots",
                    format!(
                        "Dropped {value} old milestone snapshots while keeping {}",
                        Self::SNAPSHOTS_TO_KEEP
                    ),
                    Some(if value > 0 { 1.0 } else { 0.0 }),
                    vec![task_evidence(
                        GovernanceTask::Weekly,
                        &plan.users,
                        "Weekly snapshot retention completed".to_string(),
                    )],
                    state.snapshot_before.as_deref(),
                ));
            }
            Err(err) => record_warning(
                GovernanceTask::Weekly,
                None,
                "cleanup_snapshots",
                &err,
                &mut state.warnings,
            ),
        }
    }

    async fn cleanup_orphan_branches_operation(
        &self,
        plan: &GovernancePlan,
        store: &dyn GovernanceStore,
        state: &mut ExecutionState,
    ) {
        match store.cleanup_orphan_branches().await {
            Ok(value) => {
                state.summary.orphan_branches_cleaned = value;
                state.decisions.push(governance_decision(
                    GovernanceTask::Weekly,
                    "cleanup_orphan_branches",
                    format!("Removed {value} orphaned sandbox branches"),
                    Some(if value > 0 { 1.0 } else { 0.0 }),
                    vec![task_evidence(
                        GovernanceTask::Weekly,
                        &plan.users,
                        "Weekly sandbox branch cleanup completed".to_string(),
                    )],
                    state.snapshot_before.as_deref(),
                ));
            }
            Err(err) => record_warning(
                GovernanceTask::Weekly,
                None,
                "cleanup_orphan_branches",
                &err,
                &mut state.warnings,
            ),
        }
    }

    async fn cleanup_orphan_stats_operation(
        &self,
        plan: &GovernancePlan,
        store: &dyn GovernanceStore,
        state: &mut ExecutionState,
    ) {
        match store.cleanup_orphan_stats().await {
            Ok(value) => {
                state.decisions.push(governance_decision(
                    GovernanceTask::Weekly,
                    "cleanup_orphan_stats",
                    format!("Removed {value} orphaned stats records"),
                    Some(if value > 0 { 1.0 } else { 0.0 }),
                    vec![task_evidence(
                        GovernanceTask::Weekly,
                        &plan.users,
                        "Weekly stats cleanup completed".to_string(),
                    )],
                    state.snapshot_before.as_deref(),
                ));
            }
            Err(err) => record_warning(
                GovernanceTask::Weekly,
                None,
                "cleanup_orphan_stats",
                &err,
                &mut state.warnings,
            ),
        }
    }

    async fn cleanup_edit_log_operation(
        &self,
        store: &dyn GovernanceStore,
        state: &mut ExecutionState,
    ) {
        match store.cleanup_edit_log(Self::EDIT_LOG_RETAIN_DAYS).await {
            Ok(deleted) => {
                state.decisions.push(governance_decision(
                    GovernanceTask::Weekly,
                    "cleanup_edit_log",
                    format!("Deleted {deleted} audit-log rows older than {} days", Self::EDIT_LOG_RETAIN_DAYS),
                    Some(if deleted > 0 { 1.0 } else { 0.0 }),
                    vec![],
                    state.snapshot_before.as_deref(),
                ));
            }
            Err(err) => record_warning(
                GovernanceTask::Weekly,
                None,
                "cleanup_edit_log",
                &err,
                &mut state.warnings,
            ),
        }
    }

    async fn cleanup_feedback_operation(
        &self,
        store: &dyn GovernanceStore,
        state: &mut ExecutionState,
    ) {
        match store.cleanup_feedback(Self::FEEDBACK_RETAIN_DAYS).await {
            Ok(deleted) => {
                state.decisions.push(governance_decision(
                    GovernanceTask::Weekly,
                    "cleanup_feedback",
                    format!("Deleted {deleted} feedback rows older than {} days", Self::FEEDBACK_RETAIN_DAYS),
                    Some(if deleted > 0 { 1.0 } else { 0.0 }),
                    vec![],
                    state.snapshot_before.as_deref(),
                ));
            }
            Err(err) => record_warning(
                GovernanceTask::Weekly,
                None,
                "cleanup_feedback",
                &err,
                &mut state.warnings,
            ),
        }
    }

    async fn tune_retrieval_params_operation(
        &self,
        users: &[String],
        store: &dyn GovernanceStore,
        state: &mut ExecutionState,
    ) {
        let mut tuned_count = 0i64;
        for user_id in users {
            match store.tune_user_retrieval_params(user_id).await {
                Ok(true) => tuned_count += 1,
                Ok(false) => {} // Not enough feedback, skip
                Err(err) => record_warning(
                    GovernanceTask::Daily,
                    Some(user_id),
                    "tune_retrieval_params",
                    &err,
                    &mut state.warnings,
                ),
            }
        }
        state.summary.users_tuned = tuned_count;
        if tuned_count > 0 {
            state.decisions.push(governance_decision(
                GovernanceTask::Daily,
                "tune_retrieval_params",
                format!("Auto-tuned retrieval parameters for {tuned_count} users"),
                Some(1.0),
                vec![],
                state.snapshot_before.as_deref(),
            ));
        }
    }

    async fn run_hourly(
        &self,
        store: &dyn GovernanceStore,
        plan: &GovernancePlan,
        state: &mut ExecutionState,
    ) {
        self.cleanup_tool_results_operation(plan, store, state)
            .await;
        self.archive_stale_working_operation(store, state).await;
        self.cleanup_stale_operation(GovernanceTask::Hourly, store, &plan.users, state)
            .await;
    }

    async fn run_daily(
        &self,
        store: &dyn GovernanceStore,
        plan: &GovernancePlan,
        state: &mut ExecutionState,
    ) {
        self.quarantine_operation(&plan.users, store, state).await;
        self.cleanup_stale_operation(GovernanceTask::Daily, store, &plan.users, state)
            .await;
        self.compress_redundant_operation(&plan.users, store, state)
            .await;
        self.cleanup_orphaned_incrementals_operation(&plan.users, store, state)
            .await;
        self.cleanup_async_tasks_operation(store, state).await;
        self.tune_retrieval_params_operation(&plan.users, store, state)
            .await;
    }

    async fn run_weekly(
        &self,
        store: &dyn GovernanceStore,
        plan: &GovernancePlan,
        state: &mut ExecutionState,
    ) {
        self.rebuild_vector_index_operation(plan, store, state)
            .await;
        self.cleanup_snapshots_operation(plan, store, state).await;
        self.cleanup_orphan_branches_operation(plan, store, state)
            .await;
        self.cleanup_orphan_stats_operation(plan, store, state)
            .await;
        self.cleanup_edit_log_operation(store, state).await;
        self.cleanup_feedback_operation(store, state).await;
    }
}

#[async_trait]
impl GovernanceStrategy for DefaultGovernanceStrategy {
    fn strategy_key(&self) -> &str {
        "governance:default:v1"
    }

    async fn plan(
        &self,
        store: &dyn GovernanceStore,
        task: GovernanceTask,
    ) -> Result<GovernancePlan, MemoriaError> {
        let users = match task {
            GovernanceTask::Hourly | GovernanceTask::Daily => store.list_active_users().await?,
            GovernanceTask::Weekly => match store.list_active_users().await {
                Ok(users) => users,
                Err(err) => {
                    error!(
                        task = GovernanceTask::Weekly.as_str(),
                        operation = "list_active_users",
                        %err,
                        "Failed to enumerate active users for governance summary"
                    );
                    Vec::new()
                }
            },
        };

        let action_names: &[&str] = match task {
            GovernanceTask::Hourly => &[
                "cleanup_tool_results",
                "archive_stale_working",
                "cleanup_stale",
            ],
            GovernanceTask::Daily => &[
                "quarantine_low_confidence",
                "cleanup_stale",
                "compress_redundant",
                "cleanup_orphaned_incrementals",
                "cleanup_async_tasks",
            ],
            GovernanceTask::Weekly => &[
                "rebuild_vector_index",
                "cleanup_snapshots",
                "cleanup_orphan_branches",
            ],
        };
        let actions = action_names
            .iter()
            .map(|action| StrategyDecision {
                action: (*action).to_string(),
                confidence: Some(1.0),
                rationale: format!(
                    "Planned {} governance action for {} active users",
                    action,
                    users.len()
                ),
                evidence: vec![task_evidence(
                    task,
                    &users,
                    "Generated by built-in governance planner".to_string(),
                )],
                rollback_hint: None,
            })
            .collect();

        let mut estimated_impact = HashMap::new();
        estimated_impact.insert("governance.target_users".to_string(), users.len() as f64);
        estimated_impact.insert(
            "governance.planned_actions".to_string(),
            action_names.len() as f64,
        );

        Ok(GovernancePlan {
            actions,
            estimated_impact,
            requires_approval: false,
            users,
        })
    }

    async fn execute(
        &self,
        store: &dyn GovernanceStore,
        task: GovernanceTask,
        plan: &GovernancePlan,
    ) -> Result<GovernanceExecution, MemoriaError> {
        let (snapshot_before, snapshot_warning) = store.create_safety_snapshot(task.as_str()).await;
        let mut state = ExecutionState {
            summary: GovernanceRunSummary {
                users_processed: plan.users.len(),
                ..GovernanceRunSummary::default()
            },
            decisions: Vec::new(),
            warnings: snapshot_warning.into_iter().collect(),
            snapshot_before,
        };

        match task {
            GovernanceTask::Hourly => self.run_hourly(store, plan, &mut state).await,
            GovernanceTask::Daily => self.run_daily(store, plan, &mut state).await,
            GovernanceTask::Weekly => self.run_weekly(store, plan, &mut state).await,
        }

        let mut metrics = plan.estimated_impact.clone();
        metrics.insert(
            "governance.users_processed".to_string(),
            state.summary.users_processed as f64,
        );
        metrics.insert(
            "governance.quarantined_count".to_string(),
            state.summary.total_quarantined as f64,
        );
        metrics.insert(
            "governance.cleaned_count".to_string(),
            state.summary.total_cleaned as f64,
        );
        metrics.insert(
            "governance.tool_results_cleaned".to_string(),
            state.summary.tool_results_cleaned as f64,
        );
        metrics.insert(
            "governance.archived_count".to_string(),
            state.summary.archived_working as f64,
        );
        metrics.insert(
            "governance.stale_cleaned".to_string(),
            state.summary.stale_cleaned as f64,
        );
        metrics.insert(
            "governance.redundant_compressed".to_string(),
            state.summary.redundant_compressed as f64,
        );
        metrics.insert(
            "governance.orphaned_incrementals_cleaned".to_string(),
            state.summary.orphaned_incrementals_cleaned as f64,
        );
        metrics.insert(
            "governance.vector_index_rows".to_string(),
            state.summary.vector_index_rows as f64,
        );
        metrics.insert(
            "governance.snapshots_cleaned".to_string(),
            state.summary.snapshots_cleaned as f64,
        );
        metrics.insert(
            "governance.orphan_branches_cleaned".to_string(),
            state.summary.orphan_branches_cleaned as f64,
        );
        metrics.insert(
            "governance.users_tuned".to_string(),
            state.summary.users_tuned as f64,
        );
        metrics.insert(
            "governance.snapshot_created".to_string(),
            if state.snapshot_before.is_some() {
                1.0
            } else {
                0.0
            },
        );

        let status = if state.warnings.is_empty() {
            StrategyStatus::Success
        } else {
            StrategyStatus::Degraded
        };

        Ok(GovernanceExecution {
            summary: state.summary,
            report: StrategyReport {
                status,
                decisions: state.decisions,
                metrics,
                warnings: state.warnings,
            },
        })
    }
}

fn governance_decision(
    task: GovernanceTask,
    action: &str,
    rationale: String,
    confidence: Option<f32>,
    evidence: Vec<StrategyEvidence>,
    snapshot_before: Option<&str>,
) -> StrategyDecision {
    let mut evidence = evidence;
    if evidence.is_empty() {
        evidence.push(task_evidence(
            task,
            &[],
            "No additional evidence recorded".to_string(),
        ));
    }
    StrategyDecision {
        action: action.to_string(),
        confidence,
        rationale,
        evidence,
        rollback_hint: snapshot_before
            .map(|snapshot| format!("Restore affected tables from snapshot {snapshot}")),
    }
}

fn task_evidence(task: GovernanceTask, users: &[String], summary: String) -> StrategyEvidence {
    let mut references = vec![format!("task:{}", task.as_str())];
    references.extend(users.iter().map(|user| format!("user:{user}")));
    StrategyEvidence {
        source: "governance.scheduler".to_string(),
        summary,
        score: Some(users.len() as f32),
        references,
    }
}

fn record_warning(
    task: GovernanceTask,
    user_id: Option<&str>,
    operation: &str,
    err: &MemoriaError,
    warnings: &mut Vec<String>,
) {
    if let Some(user_id) = user_id {
        error!(
            task = task.as_str(),
            user_id,
            operation,
            %err,
            "Governance operation failed"
        );
        warnings.push(format!(
            "{}:{}:{} failed: {}",
            task.as_str(),
            operation,
            user_id,
            err
        ));
    } else {
        error!(
            task = task.as_str(),
            operation,
            %err,
            "Governance operation failed"
        );
        warnings.push(format!("{}:{} failed: {}", task.as_str(), operation, err));
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};
    use std::sync::Mutex;

    use super::*;

    #[derive(Debug, Default)]
    struct RecordingState {
        calls: Vec<String>,
        log_entries: Vec<(String, String, String, Option<String>)>,
    }

    #[derive(Debug, Default)]
    struct RecordingStore {
        users: Vec<String>,
        cleanup_tool_results_result: i64,
        archive_stale_working_result: Vec<(String, i64)>,
        cleanup_stale_result: HashMap<String, i64>,
        quarantine_result: HashMap<String, i64>,
        compress_redundant_result: HashMap<String, i64>,
        cleanup_orphaned_incrementals_result: HashMap<String, i64>,
        rebuild_vector_index_result: i64,
        cleanup_snapshots_result: i64,
        cleanup_orphan_branches_result: i64,
        snapshot_result: Option<String>,
        snapshot_warning: Option<String>,
        failing_ops: HashSet<String>,
        state: Mutex<RecordingState>,
    }

    impl RecordingStore {
        fn record(&self, call: impl Into<String>) {
            self.state.lock().unwrap().calls.push(call.into());
        }

        fn fail_if_requested(&self, op: &str) -> Result<(), MemoriaError> {
            if self.failing_ops.contains(op) {
                return Err(MemoriaError::Database(format!("{op} failed")));
            }
            Ok(())
        }

        fn calls(&self) -> Vec<String> {
            self.state.lock().unwrap().calls.clone()
        }

        fn log_entries(&self) -> Vec<(String, String, String, Option<String>)> {
            self.state.lock().unwrap().log_entries.clone()
        }
    }

    async fn assert_governance_contract(
        strategy: &dyn GovernanceStrategy,
        store: &dyn GovernanceStore,
        task: GovernanceTask,
    ) {
        let plan = strategy
            .plan(store, task)
            .await
            .expect("strategy plan should succeed");
        let execution = strategy
            .execute(store, task, &plan)
            .await
            .expect("strategy execute should succeed");

        assert!(!strategy.strategy_key().is_empty());
        assert!(execution.summary.users_processed >= plan.users.len());
        assert!(matches!(
            execution.report.status,
            StrategyStatus::Success | StrategyStatus::Degraded
        ));
        assert!(execution
            .report
            .metrics
            .contains_key("governance.users_processed"));
        assert!(!execution.report.decisions.is_empty());
    }

    #[async_trait]
    impl GovernanceStore for RecordingStore {
        async fn list_active_users(&self) -> Result<Vec<String>, MemoriaError> {
            self.record("list_active_users");
            self.fail_if_requested("list_active_users")?;
            Ok(self.users.clone())
        }

        async fn cleanup_tool_results(&self, ttl_hours: i64) -> Result<i64, MemoriaError> {
            self.record(format!("cleanup_tool_results:{ttl_hours}"));
            self.fail_if_requested("cleanup_tool_results")?;
            Ok(self.cleanup_tool_results_result)
        }

        async fn cleanup_async_tasks(&self, ttl_hours: i64) -> Result<i64, MemoriaError> {
            self.record(format!("cleanup_async_tasks:{ttl_hours}"));
            self.fail_if_requested("cleanup_async_tasks")?;
            Ok(0)
        }

        async fn archive_stale_working(
            &self,
            stale_hours: i64,
        ) -> Result<Vec<(String, i64)>, MemoriaError> {
            self.record(format!("archive_stale_working:{stale_hours}"));
            self.fail_if_requested("archive_stale_working")?;
            Ok(self.archive_stale_working_result.clone())
        }

        async fn cleanup_stale(&self, user_id: &str) -> Result<i64, MemoriaError> {
            self.record(format!("cleanup_stale:{user_id}"));
            self.fail_if_requested(&format!("cleanup_stale:{user_id}"))?;
            Ok(*self.cleanup_stale_result.get(user_id).unwrap_or(&0))
        }

        async fn quarantine_low_confidence(&self, user_id: &str) -> Result<i64, MemoriaError> {
            self.record(format!("quarantine_low_confidence:{user_id}"));
            self.fail_if_requested(&format!("quarantine_low_confidence:{user_id}"))?;
            Ok(*self.quarantine_result.get(user_id).unwrap_or(&0))
        }

        async fn compress_redundant(
            &self,
            user_id: &str,
            similarity_threshold: f64,
            window_days: i64,
            max_pairs: usize,
        ) -> Result<i64, MemoriaError> {
            self.record(format!(
                "compress_redundant:{user_id}:{similarity_threshold}:{window_days}:{max_pairs}"
            ));
            self.fail_if_requested(&format!("compress_redundant:{user_id}"))?;
            Ok(*self.compress_redundant_result.get(user_id).unwrap_or(&0))
        }

        async fn cleanup_orphaned_incrementals(
            &self,
            user_id: &str,
            older_than_hours: i64,
        ) -> Result<i64, MemoriaError> {
            self.record(format!(
                "cleanup_orphaned_incrementals:{user_id}:{older_than_hours}"
            ));
            self.fail_if_requested(&format!("cleanup_orphaned_incrementals:{user_id}"))?;
            Ok(*self
                .cleanup_orphaned_incrementals_result
                .get(user_id)
                .unwrap_or(&0))
        }

        async fn rebuild_vector_index(&self, table: &str) -> Result<i64, MemoriaError> {
            self.record(format!("rebuild_vector_index:{table}"));
            self.fail_if_requested("rebuild_vector_index")?;
            Ok(self.rebuild_vector_index_result)
        }

        async fn cleanup_snapshots(&self, keep_last_n: usize) -> Result<i64, MemoriaError> {
            self.record(format!("cleanup_snapshots:{keep_last_n}"));
            self.fail_if_requested("cleanup_snapshots")?;
            Ok(self.cleanup_snapshots_result)
        }

        async fn cleanup_orphan_branches(&self) -> Result<i64, MemoriaError> {
            self.record("cleanup_orphan_branches");
            self.fail_if_requested("cleanup_orphan_branches")?;
            Ok(self.cleanup_orphan_branches_result)
        }

        async fn cleanup_orphan_stats(&self) -> Result<i64, MemoriaError> {
            self.record("cleanup_orphan_stats");
            self.fail_if_requested("cleanup_orphan_stats")?;
            Ok(0)
        }

        async fn cleanup_edit_log(&self, _: i64) -> Result<i64, MemoriaError> { Ok(0) }
        async fn cleanup_feedback(&self, _: i64) -> Result<i64, MemoriaError> { Ok(0) }

        async fn create_safety_snapshot(
            &self,
            operation: &str,
        ) -> (Option<String>, Option<String>) {
            self.record(format!("create_safety_snapshot:{operation}"));
            (self.snapshot_result.clone(), self.snapshot_warning.clone())
        }

        async fn log_edit(
            &self,
            user_id: &str,
            operation: &str,
            _memory_id: Option<&str>,
            _payload: Option<&str>,
            reason: &str,
            snapshot_before: Option<&str>,
        ) {
            self.record(format!(
                "log_edit:{user_id}:{operation}:{reason}:{}",
                snapshot_before.unwrap_or("none")
            ));
            self.state.lock().unwrap().log_entries.push((
                user_id.to_string(),
                operation.to_string(),
                reason.to_string(),
                snapshot_before.map(ToString::to_string),
            ));
        }
    }

    #[tokio::test]
    async fn hourly_runs_with_structured_report_and_snapshot() {
        let store = RecordingStore {
            users: vec!["u1".into(), "u2".into()],
            cleanup_tool_results_result: 4,
            archive_stale_working_result: vec![("u1".into(), 2), ("u2".into(), 1)],
            cleanup_stale_result: HashMap::from([("u1".into(), 3), ("u2".into(), 0)]),
            snapshot_result: Some("mem_snap_pre_hourly_deadbeef".into()),
            ..RecordingStore::default()
        };

        let execution = DefaultGovernanceStrategy
            .run(&store, GovernanceTask::Hourly)
            .await
            .expect("hourly governance should succeed");

        assert_eq!(
            execution.summary,
            GovernanceRunSummary {
                users_processed: 2,
                total_quarantined: 0,
                total_cleaned: 6,
                tool_results_cleaned: 4,
                async_tasks_cleaned: 0,
                archived_working: 3,
                stale_cleaned: 3,
                redundant_compressed: 0,
                orphaned_incrementals_cleaned: 0,
                vector_index_rows: 0,
                snapshots_cleaned: 0,
                orphan_branches_cleaned: 0,
                users_tuned: 0,
            }
        );
        assert_eq!(execution.report.status, StrategyStatus::Success);
        assert_eq!(execution.report.metrics["governance.snapshot_created"], 1.0);
        assert!(execution
            .report
            .decisions
            .iter()
            .all(|decision| decision.rollback_hint.is_some()));

        let logs = store.log_entries();
        assert!(logs.contains(&(
            "u1".into(),
            "governance:archive_working".into(),
            "archived 2 stale working memories (>24h)".into(),
            Some("mem_snap_pre_hourly_deadbeef".into()),
        )));
    }

    #[tokio::test]
    async fn daily_continues_after_per_user_operation_failure_and_marks_report_degraded() {
        let store = RecordingStore {
            users: vec!["u1".into()],
            quarantine_result: HashMap::from([("u1".into(), 2)]),
            cleanup_stale_result: HashMap::from([("u1".into(), 1)]),
            cleanup_orphaned_incrementals_result: HashMap::from([("u1".into(), 5)]),
            failing_ops: HashSet::from(["compress_redundant:u1".into()]),
            snapshot_warning: Some("snapshot quota low".into()),
            ..RecordingStore::default()
        };

        let execution = DefaultGovernanceStrategy
            .run(&store, GovernanceTask::Daily)
            .await
            .expect("daily governance should continue past non-fatal operation failures");

        assert_eq!(execution.summary.users_processed, 1);
        assert_eq!(execution.summary.total_quarantined, 2);
        assert_eq!(execution.summary.total_cleaned, 1);
        assert_eq!(execution.summary.redundant_compressed, 0);
        assert_eq!(execution.summary.orphaned_incrementals_cleaned, 5);
        assert_eq!(execution.report.status, StrategyStatus::Degraded);
        assert!(execution
            .report
            .warnings
            .iter()
            .any(|warning| warning.contains("compress_redundant")));
        assert!(execution
            .report
            .warnings
            .iter()
            .any(|warning| warning.contains("snapshot quota low")));
    }

    #[tokio::test]
    async fn weekly_runs_global_operations_once_even_with_multiple_users() {
        let store = RecordingStore {
            users: vec!["u1".into(), "u2".into()],
            rebuild_vector_index_result: 42,
            cleanup_snapshots_result: 3,
            cleanup_orphan_branches_result: 2,
            ..RecordingStore::default()
        };

        let execution = DefaultGovernanceStrategy
            .run(&store, GovernanceTask::Weekly)
            .await
            .expect("weekly governance should succeed");

        assert_eq!(execution.summary.users_processed, 2);
        assert_eq!(execution.summary.vector_index_rows, 42);
        assert_eq!(execution.summary.snapshots_cleaned, 3);
        assert_eq!(execution.summary.orphan_branches_cleaned, 2);

        let calls = store.calls();
        assert_eq!(
            calls
                .iter()
                .filter(|call| *call == "rebuild_vector_index:mem_memories")
                .count(),
            1
        );
        assert_eq!(
            calls
                .iter()
                .filter(|call| *call == "cleanup_snapshots:5")
                .count(),
            1
        );
        assert_eq!(
            calls
                .iter()
                .filter(|call| *call == "cleanup_orphan_branches")
                .count(),
            1
        );
    }

    #[derive(Default)]
    struct StaticContractStrategy;

    #[async_trait]
    impl GovernanceStrategy for StaticContractStrategy {
        fn strategy_key(&self) -> &str {
            "governance:contract:v1"
        }

        async fn plan(
            &self,
            store: &dyn GovernanceStore,
            _: GovernanceTask,
        ) -> Result<GovernancePlan, MemoriaError> {
            let users = store.list_active_users().await?;
            Ok(GovernancePlan {
                actions: vec![StrategyDecision {
                    action: "noop".into(),
                    confidence: Some(1.0),
                    rationale: "contract strategy plan".into(),
                    evidence: vec![StrategyEvidence {
                        source: "contract".into(),
                        summary: "static contract evidence".into(),
                        score: Some(1.0),
                        references: vec!["contract".into()],
                    }],
                    rollback_hint: None,
                }],
                estimated_impact: HashMap::from([(
                    "governance.target_users".into(),
                    users.len() as f64,
                )]),
                requires_approval: false,
                users,
            })
        }

        async fn execute(
            &self,
            _: &dyn GovernanceStore,
            _: GovernanceTask,
            plan: &GovernancePlan,
        ) -> Result<GovernanceExecution, MemoriaError> {
            Ok(GovernanceExecution {
                summary: GovernanceRunSummary {
                    users_processed: plan.users.len(),
                    ..GovernanceRunSummary::default()
                },
                report: StrategyReport {
                    status: StrategyStatus::Success,
                    decisions: plan.actions.clone(),
                    metrics: HashMap::from([(
                        "governance.users_processed".into(),
                        plan.users.len() as f64,
                    )]),
                    warnings: Vec::new(),
                },
            })
        }
    }

    #[tokio::test]
    async fn governance_contract_accepts_default_strategy() {
        let store = RecordingStore {
            users: vec!["u1".into()],
            snapshot_result: Some("mem_snap_pre_daily_contract".into()),
            quarantine_result: HashMap::from([("u1".into(), 1)]),
            ..RecordingStore::default()
        };

        assert_governance_contract(&DefaultGovernanceStrategy, &store, GovernanceTask::Daily).await;
    }

    #[tokio::test]
    async fn governance_contract_accepts_alternate_strategy() {
        let store = RecordingStore {
            users: vec!["u1".into(), "u2".into()],
            ..RecordingStore::default()
        };

        assert_governance_contract(&StaticContractStrategy, &store, GovernanceTask::Hourly).await;
    }

    /// Gap: tune failure for one user must not block other users' tuning.
    #[tokio::test]
    async fn daily_tune_failure_for_one_user_does_not_block_others() {
        struct TuneStore {
            users: Vec<String>,
            failing_user: String,
            tuned: Mutex<Vec<String>>,
        }

        #[async_trait]
        impl GovernanceStore for TuneStore {
            async fn list_active_users(&self) -> Result<Vec<String>, MemoriaError> {
                Ok(self.users.clone())
            }
            async fn cleanup_tool_results(&self, _: i64) -> Result<i64, MemoriaError> { Ok(0) }
            async fn cleanup_async_tasks(&self, _: i64) -> Result<i64, MemoriaError> { Ok(0) }
            async fn archive_stale_working(&self, _: i64) -> Result<Vec<(String, i64)>, MemoriaError> { Ok(vec![]) }
            async fn cleanup_stale(&self, _: &str) -> Result<i64, MemoriaError> { Ok(0) }
            async fn quarantine_low_confidence(&self, _: &str) -> Result<i64, MemoriaError> { Ok(0) }
            async fn compress_redundant(&self, _: &str, _: f64, _: i64, _: usize) -> Result<i64, MemoriaError> { Ok(0) }
            async fn cleanup_orphaned_incrementals(&self, _: &str, _: i64) -> Result<i64, MemoriaError> { Ok(0) }
            async fn rebuild_vector_index(&self, _: &str) -> Result<i64, MemoriaError> { Ok(0) }
            async fn cleanup_snapshots(&self, _: usize) -> Result<i64, MemoriaError> { Ok(0) }
            async fn cleanup_orphan_branches(&self) -> Result<i64, MemoriaError> { Ok(0) }
            async fn cleanup_orphan_stats(&self) -> Result<i64, MemoriaError> { Ok(0) }
            async fn cleanup_edit_log(&self, _: i64) -> Result<i64, MemoriaError> { Ok(0) }
        async fn cleanup_feedback(&self, _: i64) -> Result<i64, MemoriaError> { Ok(0) }
            async fn create_safety_snapshot(&self, _: &str) -> (Option<String>, Option<String>) { (None, None) }
            async fn log_edit(&self, _: &str, _: &str, _: Option<&str>, _: Option<&str>, _: &str, _: Option<&str>) {}

            async fn tune_user_retrieval_params(&self, user_id: &str) -> Result<bool, MemoriaError> {
                if user_id == self.failing_user {
                    return Err(MemoriaError::Database("tune exploded".into()));
                }
                self.tuned.lock().unwrap().push(user_id.to_string());
                Ok(true)
            }
        }

        let store = TuneStore {
            users: vec!["u1".into(), "u2".into(), "u3".into()],
            failing_user: "u2".into(),
            tuned: Mutex::new(vec![]),
        };

        let execution = DefaultGovernanceStrategy
            .run(&store, GovernanceTask::Daily)
            .await
            .expect("daily should not fail even if one user's tune fails");

        // u1 and u3 should have been tuned despite u2 failing
        let tuned = store.tuned.lock().unwrap().clone();
        assert!(tuned.contains(&"u1".to_string()), "u1 should be tuned");
        assert!(tuned.contains(&"u3".to_string()), "u3 should be tuned");
        assert!(!tuned.contains(&"u2".to_string()), "u2 should have failed");
        assert_eq!(execution.summary.users_tuned, 2);

        // Report should be degraded with a warning about u2
        assert_eq!(execution.report.status, StrategyStatus::Degraded);
        assert!(execution.report.warnings.iter().any(|w| w.contains("tune_retrieval_params") && w.contains("u2")));
    }
}
