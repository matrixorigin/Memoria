use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use memoria_core::MemoriaError;
use memoria_service::{
    GovernanceExecution, GovernancePlan, GovernancePluginContractHarness, GovernanceRunSummary,
    GovernanceStore, GovernanceStrategy, GovernanceTask, HostPluginPolicy, RhaiGovernanceStrategy,
    StrategyReport, StrategyStatus, GOVERNANCE_RHAI_TEMPLATE,
};

fn plugin_dir(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("data")
        .join("plugins")
        .join(name)
}

struct NoopStore;

#[async_trait]
impl GovernanceStore for NoopStore {
    async fn list_active_users(&self) -> Result<Vec<String>, MemoriaError> {
        Ok(vec!["u1".into(), "u2".into()])
    }
    async fn cleanup_tool_results(&self, _: i64) -> Result<i64, MemoriaError> {
        Ok(0)
    }
    async fn cleanup_async_tasks(&self, _: i64) -> Result<i64, MemoriaError> {
        Ok(0)
    }
    async fn archive_stale_working(&self, _: i64) -> Result<Vec<(String, i64)>, MemoriaError> {
        Ok(vec![])
    }
    async fn cleanup_stale(&self, _: &str) -> Result<i64, MemoriaError> {
        Ok(0)
    }
    async fn quarantine_low_confidence(&self, _: &str) -> Result<i64, MemoriaError> {
        Ok(0)
    }
    async fn compress_redundant(
        &self,
        _: &str,
        _: f64,
        _: i64,
        _: usize,
    ) -> Result<i64, MemoriaError> {
        Ok(0)
    }
    async fn cleanup_orphaned_incrementals(&self, _: &str, _: i64) -> Result<i64, MemoriaError> {
        Ok(0)
    }
    async fn rebuild_vector_index(&self, _: &str) -> Result<i64, MemoriaError> {
        Ok(0)
    }
    async fn cleanup_snapshots(&self, _: usize) -> Result<i64, MemoriaError> {
        Ok(0)
    }
    async fn cleanup_orphan_branches(&self) -> Result<i64, MemoriaError> {
        Ok(0)
    }
    async fn create_safety_snapshot(&self, _: &str) -> (Option<String>, Option<String>) {
        (None, None)
    }
    async fn log_edit(&self, _: &str, _: &str, _: Option<&str>, _: Option<&str>, _: &str, _: Option<&str>) {}
}

struct DelegateStrategy;

#[async_trait]
impl GovernanceStrategy for DelegateStrategy {
    fn strategy_key(&self) -> &str {
        "governance:delegate:v1"
    }

    async fn plan(
        &self,
        store: &dyn GovernanceStore,
        _: GovernanceTask,
    ) -> Result<GovernancePlan, MemoriaError> {
        Ok(GovernancePlan {
            actions: vec![],
            estimated_impact: HashMap::new(),
            requires_approval: false,
            users: store.list_active_users().await?,
        })
    }

    async fn execute(
        &self,
        _: &dyn GovernanceStore,
        _: GovernanceTask,
        _: &GovernancePlan,
    ) -> Result<GovernanceExecution, MemoriaError> {
        Ok(GovernanceExecution {
            summary: GovernanceRunSummary {
                users_processed: 2,
                ..GovernanceRunSummary::default()
            },
            report: StrategyReport::default(),
        })
    }
}

#[tokio::test]
async fn example_plugin_packages_load_and_validate() {
    let harness = GovernancePluginContractHarness::new(
        HostPluginPolicy::development(),
        Arc::new(DelegateStrategy),
    );
    let weekly = harness
        .load_from_dir(plugin_dir("governance-weekly-approval"))
        .unwrap();
    let audit = harness
        .load_from_dir(plugin_dir("governance-audit-note"))
        .unwrap();

    assert_eq!(weekly.strategy_key(), "governance:weekly-approval:v1");
    assert_eq!(audit.strategy_key(), "governance:audit-note:v1");
    assert!(GOVERNANCE_RHAI_TEMPLATE.contains("fn memoria_plugin"));
}

#[tokio::test]
async fn weekly_approval_plugin_contract() {
    let harness = GovernancePluginContractHarness::new(
        HostPluginPolicy::development(),
        Arc::new(DelegateStrategy),
    );
    let contract = harness
        .run_from_dir(
            plugin_dir("governance-weekly-approval"),
            &NoopStore,
            GovernanceTask::Weekly,
        )
        .await
        .unwrap();
    let strategy = RhaiGovernanceStrategy::load_from_dir(
        plugin_dir("governance-weekly-approval"),
        &HostPluginPolicy::development(),
        Arc::new(DelegateStrategy),
    )
    .unwrap();

    let plan = contract.plan;
    assert!(plan.requires_approval);
    assert_eq!(plan.actions.len(), 1);
    assert_eq!(plan.actions[0].action, "plugin:weekly-approval");
    assert_eq!(contract.strategy_key, strategy.strategy_key());

    let execution = contract.execution;
    assert_eq!(execution.report.status, StrategyStatus::Success);
    assert!(execution
        .report
        .warnings
        .iter()
        .any(|warning| warning.contains("Weekly approval plugin")));
    assert_eq!(
        execution
            .report
            .metrics
            .get("plugin.weekly_approval.executed"),
        Some(&1.0)
    );
}

#[tokio::test]
async fn audit_note_plugin_contract() {
    let harness = GovernancePluginContractHarness::new(
        HostPluginPolicy::development(),
        Arc::new(DelegateStrategy),
    );
    let contract = harness
        .run_from_dir(
            plugin_dir("governance-audit-note"),
            &NoopStore,
            GovernanceTask::Daily,
        )
        .await
        .unwrap();
    let plan = contract.plan;
    assert_eq!(plan.estimated_impact.get("plugin.daily_audit"), Some(&0.5));

    let execution = contract.execution;
    assert_eq!(execution.report.status, StrategyStatus::Success);
    assert_eq!(execution.report.decisions.len(), 1);
    assert_eq!(execution.report.decisions[0].action, "plugin:audit-note");
    assert_eq!(
        execution.report.metrics.get("plugin.audit_note.executed"),
        Some(&1.0)
    );
}
