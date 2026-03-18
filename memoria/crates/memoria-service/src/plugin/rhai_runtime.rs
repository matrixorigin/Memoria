use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use memoria_core::MemoriaError;
use rhai::serde::{from_dynamic, to_dynamic};
use rhai::{Dynamic, Engine, Map, Scope};
use serde::{Deserialize, Serialize};
use tokio::time::{timeout, Duration};

use crate::governance::{
    GovernanceExecution, GovernancePlan, GovernanceRunSummary, GovernanceStore, GovernanceStrategy,
    GovernanceTask,
};
use crate::plugin::manifest::{
    load_plugin_package, HostPluginPolicy, PluginPackage, PluginRuntimeKind,
};
use crate::strategy_domain::{StrategyDecision, StrategyEvidence, StrategyReport, StrategyStatus};

pub trait PluginRuntime: Send + Sync {
    fn runtime_kind(&self) -> PluginRuntimeKind;
}

#[derive(Clone)]
pub struct RhaiGovernanceStrategy {
    package: PluginPackage,
    delegate: Arc<dyn GovernanceStrategy>,
    script_source: String,
}

impl RhaiGovernanceStrategy {
    pub fn load_from_dir(
        package_dir: impl AsRef<Path>,
        policy: &HostPluginPolicy,
        delegate: Arc<dyn GovernanceStrategy>,
    ) -> Result<Self, MemoriaError> {
        let package = load_plugin_package(package_dir.as_ref().to_path_buf(), policy)?;
        let script_source = fs::read_to_string(&package.script_path).map_err(|err| {
            MemoriaError::Blocked(format!(
                "Failed to read Rhai script {}: {err}",
                package.script_path.display()
            ))
        })?;

        Self::from_loaded_package(package, script_source, delegate)
    }

    pub fn from_loaded_package(
        package: PluginPackage,
        script_source: String,
        delegate: Arc<dyn GovernanceStrategy>,
    ) -> Result<Self, MemoriaError> {
        if !package.manifest.has_capability("governance.plan")
            && !package.manifest.has_capability("governance.execute")
        {
            return Err(MemoriaError::Blocked(
                "Rhai governance plugin must declare governance.plan and/or governance.execute"
                    .into(),
            ));
        }

        Ok(Self {
            package,
            delegate,
            script_source,
        })
    }

    async fn call_plugin<T, C>(&self, context: C) -> Result<T, MemoriaError>
    where
        T: for<'de> Deserialize<'de> + Send + 'static,
        C: Serialize + Send + 'static,
    {
        let script_source = self.script_source.clone();
        let entrypoint = self.package.entrypoint.clone();
        let timeout_ms = self.package.manifest.limits.timeout_ms;
        let max_output_bytes = self.package.manifest.limits.max_output_bytes;
        let max_memory_mb = self.package.manifest.limits.max_memory_mb;

        let handle = tokio::task::spawn_blocking(move || {
            let mut engine = Engine::new();
            configure_engine_limits(&mut engine, timeout_ms, max_memory_mb, max_output_bytes);
            register_helpers(&mut engine);
            let started_at = Instant::now();
            let printed_bytes = Arc::new(AtomicUsize::new(0));
            let printed_bytes_hook = printed_bytes.clone();
            engine.on_print(move |s| {
                printed_bytes_hook.fetch_add(s.len(), Ordering::Relaxed);
            });
            engine.on_progress(move |_| {
                if printed_bytes.load(Ordering::Relaxed) > max_output_bytes {
                    return Some("plugin output exceeded configured limit".into());
                }
                if started_at.elapsed() > Duration::from_millis(timeout_ms) {
                    return Some("plugin execution exceeded configured timeout".into());
                }
                None
            });
            let ast = engine
                .compile(&script_source)
                .map_err(|err| MemoriaError::Blocked(format!("Rhai compile failed: {err}")))?;
            let input = to_dynamic(context).map_err(|err| {
                MemoriaError::Internal(format!("Rhai input serialization failed: {err}"))
            })?;
            let result: Dynamic = engine
                .call_fn(&mut Scope::new(), &ast, &entrypoint, (input,))
                .map_err(|err| MemoriaError::Blocked(format!("Rhai execution failed: {err}")))?;
            let json: serde_json::Value = from_dynamic(&result).map_err(|err| {
                MemoriaError::Internal(format!("Rhai output deserialization failed: {err}"))
            })?;
            let encoded = serde_json::to_vec(&json)?;
            if encoded.len() > max_output_bytes {
                return Err(MemoriaError::Blocked(format!(
                    "Plugin output exceeded {} bytes",
                    max_output_bytes
                )));
            }
            serde_json::from_slice(&encoded).map_err(|err| {
                MemoriaError::Internal(format!("Plugin output decode failed: {err}"))
            })
        });

        timeout(Duration::from_millis(timeout_ms), handle)
            .await
            .map_err(|_| {
                MemoriaError::Blocked(format!("Rhai plugin timed out after {}ms", timeout_ms))
            })?
            .map_err(|err| MemoriaError::Internal(format!("Plugin task join failed: {err}")))?
    }

    fn supports(&self, capability: &str) -> bool {
        self.package.manifest.has_capability(capability)
    }

    pub fn manifest(&self) -> &crate::plugin::PluginManifest {
        &self.package.manifest
    }

    pub fn package_root(&self) -> &Path {
        &self.package.root_dir
    }
}

impl PluginRuntime for RhaiGovernanceStrategy {
    fn runtime_kind(&self) -> PluginRuntimeKind {
        PluginRuntimeKind::Rhai
    }
}

#[async_trait]
impl GovernanceStrategy for RhaiGovernanceStrategy {
    fn strategy_key(&self) -> &str {
        &self.package.plugin_key
    }

    async fn plan(
        &self,
        store: &dyn GovernanceStore,
        task: GovernanceTask,
    ) -> Result<GovernancePlan, MemoriaError> {
        let base_plan = self.delegate.plan(store, task).await?;
        if !self.supports("governance.plan") {
            return Ok(base_plan);
        }

        let patch: RhaiPlanPatch = self
            .call_plugin::<RhaiPlanPatch, _>(PlanHookContext::new(
                self.strategy_key(),
                task,
                &base_plan,
            ))
            .await?;
        Ok(apply_plan_patch(base_plan, patch))
    }

    async fn execute(
        &self,
        store: &dyn GovernanceStore,
        task: GovernanceTask,
        plan: &GovernancePlan,
    ) -> Result<GovernanceExecution, MemoriaError> {
        let mut execution = self.delegate.execute(store, task, plan).await?;
        if !self.supports("governance.execute") {
            return Ok(execution);
        }

        match self
            .call_plugin::<RhaiExecutionPatch, _>(ExecuteHookContext::new(
                self.strategy_key(),
                task,
                plan,
                &execution,
            ))
            .await
        {
            Ok(patch) => {
                apply_execution_patch(&mut execution, patch)?;
            }
            Err(err) => {
                execution.report.status = StrategyStatus::Degraded;
                execution.report.warnings.push(format!(
                    "Plugin execution hook degraded and builtin result was retained: {err}"
                ));
                execution
                    .report
                    .metrics
                    .insert("plugin.runtime.degraded".into(), 1.0);
            }
        }

        Ok(execution)
    }
}

#[derive(Debug, Serialize)]
struct PlanHookContext {
    phase: &'static str,
    task: &'static str,
    strategy_key: String,
    base_plan: SerializablePlan,
}

impl PlanHookContext {
    fn new(strategy_key: &str, task: GovernanceTask, plan: &GovernancePlan) -> Self {
        Self {
            phase: "plan",
            task: task.as_str(),
            strategy_key: strategy_key.to_string(),
            base_plan: SerializablePlan::from(plan),
        }
    }
}

#[derive(Debug, Serialize)]
struct ExecuteHookContext {
    phase: &'static str,
    task: &'static str,
    strategy_key: String,
    plan: SerializablePlan,
    summary: SerializableSummary,
    report: SerializableReport,
}

impl ExecuteHookContext {
    fn new(
        strategy_key: &str,
        task: GovernanceTask,
        plan: &GovernancePlan,
        execution: &GovernanceExecution,
    ) -> Self {
        Self {
            phase: "execute",
            task: task.as_str(),
            strategy_key: strategy_key.to_string(),
            plan: SerializablePlan::from(plan),
            summary: SerializableSummary::from(&execution.summary),
            report: SerializableReport::from(&execution.report),
        }
    }
}

#[derive(Debug, Serialize)]
struct SerializablePlan {
    actions: Vec<SerializableDecision>,
    estimated_impact: HashMap<String, f64>,
    requires_approval: bool,
    users: Vec<String>,
}

impl From<&GovernancePlan> for SerializablePlan {
    fn from(value: &GovernancePlan) -> Self {
        Self {
            actions: value
                .actions
                .iter()
                .map(SerializableDecision::from)
                .collect(),
            estimated_impact: value.estimated_impact.clone(),
            requires_approval: value.requires_approval,
            users: value.users.clone(),
        }
    }
}

#[derive(Debug, Serialize)]
struct SerializableSummary {
    users_processed: usize,
    total_quarantined: i64,
    total_cleaned: i64,
    tool_results_cleaned: i64,
    archived_working: i64,
    stale_cleaned: i64,
    redundant_compressed: i64,
    orphaned_incrementals_cleaned: i64,
    vector_index_rows: i64,
    snapshots_cleaned: i64,
    orphan_branches_cleaned: i64,
}

impl From<&GovernanceRunSummary> for SerializableSummary {
    fn from(value: &GovernanceRunSummary) -> Self {
        Self {
            users_processed: value.users_processed,
            total_quarantined: value.total_quarantined,
            total_cleaned: value.total_cleaned,
            tool_results_cleaned: value.tool_results_cleaned,
            archived_working: value.archived_working,
            stale_cleaned: value.stale_cleaned,
            redundant_compressed: value.redundant_compressed,
            orphaned_incrementals_cleaned: value.orphaned_incrementals_cleaned,
            vector_index_rows: value.vector_index_rows,
            snapshots_cleaned: value.snapshots_cleaned,
            orphan_branches_cleaned: value.orphan_branches_cleaned,
        }
    }
}

#[derive(Debug, Serialize)]
struct SerializableReport {
    status: &'static str,
    decisions: Vec<SerializableDecision>,
    metrics: HashMap<String, f64>,
    warnings: Vec<String>,
}

impl From<&StrategyReport> for SerializableReport {
    fn from(value: &StrategyReport) -> Self {
        Self {
            status: value.status.as_str(),
            decisions: value
                .decisions
                .iter()
                .map(SerializableDecision::from)
                .collect(),
            metrics: value.metrics.clone(),
            warnings: value.warnings.clone(),
        }
    }
}

#[derive(Debug, Serialize)]
struct SerializableDecision {
    action: String,
    confidence: Option<f32>,
    rationale: String,
    evidence: Vec<SerializableEvidence>,
    rollback_hint: Option<String>,
}

impl From<&StrategyDecision> for SerializableDecision {
    fn from(value: &StrategyDecision) -> Self {
        Self {
            action: value.action.clone(),
            confidence: value.confidence,
            rationale: value.rationale.clone(),
            evidence: value
                .evidence
                .iter()
                .map(SerializableEvidence::from)
                .collect(),
            rollback_hint: value.rollback_hint.clone(),
        }
    }
}

#[derive(Debug, Serialize)]
struct SerializableEvidence {
    source: String,
    summary: String,
    score: Option<f32>,
    references: Vec<String>,
}

impl From<&StrategyEvidence> for SerializableEvidence {
    fn from(value: &StrategyEvidence) -> Self {
        Self {
            source: value.source.clone(),
            summary: value.summary.clone(),
            score: value.score,
            references: value.references.clone(),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct RhaiPlanPatch {
    requires_approval: Option<bool>,
    actions: Option<Vec<PluginDecision>>,
    estimated_impact: Option<HashMap<String, f64>>,
}

#[derive(Debug, Default, Deserialize)]
struct RhaiExecutionPatch {
    status: Option<String>,
    decisions: Option<Vec<PluginDecision>>,
    metrics: Option<HashMap<String, f64>>,
    warnings: Option<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize)]
struct PluginDecision {
    action: String,
    rationale: String,
    #[serde(default)]
    confidence: Option<f32>,
    #[serde(default)]
    evidence: Vec<PluginEvidence>,
    #[serde(default)]
    rollback_hint: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct PluginEvidence {
    source: String,
    summary: String,
    #[serde(default)]
    score: Option<f32>,
    #[serde(default)]
    references: Vec<String>,
}

fn apply_plan_patch(mut plan: GovernancePlan, patch: RhaiPlanPatch) -> GovernancePlan {
    if let Some(requires_approval) = patch.requires_approval {
        plan.requires_approval = requires_approval;
    }
    if let Some(actions) = patch.actions {
        plan.actions = actions.into_iter().map(Into::into).collect();
    }
    if let Some(estimated_impact) = patch.estimated_impact {
        plan.estimated_impact = estimated_impact;
    }
    plan
}

fn apply_execution_patch(
    execution: &mut GovernanceExecution,
    patch: RhaiExecutionPatch,
) -> Result<(), MemoriaError> {
    if let Some(status) = patch.status {
        execution.report.status = match status.as_str() {
            "success" => StrategyStatus::Success,
            "rejected" => StrategyStatus::Rejected,
            "degraded" => StrategyStatus::Degraded,
            "failed" => StrategyStatus::Failed,
            other => {
                return Err(MemoriaError::Blocked(format!(
                    "Unsupported plugin status `{other}`"
                )))
            }
        };
    }
    if let Some(decisions) = patch.decisions {
        execution.report.decisions = decisions.into_iter().map(Into::into).collect();
    }
    if let Some(metrics) = patch.metrics {
        execution.report.metrics.extend(metrics);
    }
    if let Some(warnings) = patch.warnings {
        execution.report.warnings.extend(warnings);
    }
    Ok(())
}

fn register_helpers(engine: &mut Engine) {
    engine.register_fn("decision", |action: &str, rationale: &str| {
        let mut map = Map::new();
        map.insert("action".into(), action.into());
        map.insert("rationale".into(), rationale.into());
        map
    });
    engine.register_fn(
        "decision",
        |action: &str, rationale: &str, confidence: rhai::FLOAT| {
            let mut map = Map::new();
            map.insert("action".into(), action.into());
            map.insert("rationale".into(), rationale.into());
            map.insert("confidence".into(), confidence.into());
            map
        },
    );
    engine.register_fn("evidence", |source: &str, summary: &str| {
        let mut map = Map::new();
        map.insert("source".into(), source.into());
        map.insert("summary".into(), summary.into());
        map
    });
}

fn configure_engine_limits(
    engine: &mut Engine,
    timeout_ms: u64,
    max_memory_mb: u64,
    max_output_bytes: usize,
) {
    let memory_budget = (max_memory_mb as usize).saturating_mul(1024 * 1024);
    let max_operations = timeout_ms.saturating_mul(10_000).max(50_000);
    let max_array_size = (memory_budget / 4096).clamp(16, 4096);
    let max_map_size = (memory_budget / 8192).clamp(8, 2048);
    let max_string_size = max_output_bytes.max(1024);

    engine
        .set_max_operations(max_operations)
        .set_max_variables(256)
        .set_max_call_levels(32)
        .set_max_expr_depths(64, 32)
        .set_max_string_size(max_string_size)
        .set_max_array_size(max_array_size)
        .set_max_map_size(max_map_size);
}

impl From<PluginDecision> for StrategyDecision {
    fn from(value: PluginDecision) -> Self {
        Self {
            action: value.action,
            confidence: value.confidence,
            rationale: value.rationale,
            evidence: value.evidence.into_iter().map(Into::into).collect(),
            rollback_hint: value.rollback_hint,
        }
    }
}

impl From<PluginEvidence> for StrategyEvidence {
    fn from(value: PluginEvidence) -> Self {
        Self {
            source: value.source,
            summary: value.summary,
            score: value.score,
            references: value.references,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::governance::{GovernanceRunSummary, GovernanceStore};
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct NoopStore;

    #[async_trait]
    impl GovernanceStore for NoopStore {
        async fn list_active_users(&self) -> Result<Vec<String>, MemoriaError> {
            Ok(vec!["u1".into(), "u2".into()])
        }
        async fn cleanup_tool_results(&self, _: i64) -> Result<i64, MemoriaError> {
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
        async fn cleanup_orphaned_incrementals(
            &self,
            _: &str,
            _: i64,
        ) -> Result<i64, MemoriaError> {
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
        async fn log_edit(&self, _: &str, _: &str, _: &[&str], _: &str, _: Option<&str>) {}
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

    fn temp_plugin_dir(name: &str) -> std::path::PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("memoria-rhai-plugin-{name}-{nonce}"))
    }

    fn write_manifest(dir: &Path, script: &str) {
        write_manifest_with_limits(dir, script, 500, 16384);
    }

    fn write_manifest_with_limits(
        dir: &Path,
        script: &str,
        timeout_ms: u64,
        max_output_bytes: usize,
    ) {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join("plugin.rhai"), script).unwrap();
        let mut manifest = serde_json::json!({
            "name": "memoria-governance-rhai-test",
            "version": "1.0.0",
            "api_version": "v1",
            "runtime": "rhai",
            "entry": { "rhai": { "script": "plugin.rhai", "entrypoint": "memoria_plugin" } },
            "capabilities": ["governance.plan", "governance.execute"],
            "compatibility": { "memoria": ">=0.1.0-rc1 <0.2.0" },
            "permissions": { "network": false, "filesystem": false, "env": [] },
            "limits": { "timeout_ms": timeout_ms, "max_memory_mb": 64, "max_output_bytes": max_output_bytes },
            "integrity": { "sha256": "", "signature": "dev-signature", "signer": "dev-signer" },
            "metadata": { "display_name": "Rhai Test Plugin" }
        });
        fs::write(
            dir.join("manifest.json"),
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();
        let sha = crate::plugin::manifest::compute_package_sha256(dir).unwrap();
        manifest["integrity"]["sha256"] = serde_json::Value::String(sha);
        fs::write(
            dir.join("manifest.json"),
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();
    }

    #[tokio::test]
    async fn rhai_governance_plan_patch_updates_plan() {
        let dir = temp_plugin_dir("plan");
        write_manifest(
            &dir,
            r#"
                fn memoria_plugin(ctx) {
                    if ctx["phase"] == "plan" {
                        return #{
                            requires_approval: true,
                            actions: [ decision("plugin:approval", "Rhai requested review", 0.9) ],
                            estimated_impact: #{ "plugin.review_required": 1.0 }
                        };
                    }
                    return #{};
                }
            "#,
        );

        let strategy = RhaiGovernanceStrategy::load_from_dir(
            &dir,
            &HostPluginPolicy::development(),
            Arc::new(DelegateStrategy),
        )
        .unwrap();
        let plan = strategy
            .plan(&NoopStore, GovernanceTask::Weekly)
            .await
            .unwrap();
        assert!(plan.requires_approval);
        assert_eq!(plan.actions.len(), 1);
        assert_eq!(plan.actions[0].action, "plugin:approval");
        assert_eq!(
            plan.estimated_impact.get("plugin.review_required"),
            Some(&1.0)
        );

        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn rhai_governance_execute_failure_degrades_report() {
        let dir = temp_plugin_dir("execute");
        write_manifest(
            &dir,
            r#"
                fn memoria_plugin(ctx) {
                    if ctx["phase"] == "execute" {
                        throw("boom");
                    }
                    return #{};
                }
            "#,
        );

        let strategy = RhaiGovernanceStrategy::load_from_dir(
            &dir,
            &HostPluginPolicy::development(),
            Arc::new(DelegateStrategy),
        )
        .unwrap();
        let plan = strategy
            .plan(&NoopStore, GovernanceTask::Daily)
            .await
            .unwrap();
        let execution = strategy
            .execute(&NoopStore, GovernanceTask::Daily, &plan)
            .await
            .unwrap();
        assert_eq!(execution.report.status, StrategyStatus::Degraded);
        assert!(execution
            .report
            .warnings
            .iter()
            .any(|warning| warning.contains("Plugin execution hook degraded")));
        assert_eq!(
            execution.report.metrics.get("plugin.runtime.degraded"),
            Some(&1.0)
        );

        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn rhai_governance_plan_rejects_busy_loop_with_engine_limits() {
        let dir = temp_plugin_dir("busy-loop");
        write_manifest_with_limits(
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
            16384,
        );

        let strategy = RhaiGovernanceStrategy::load_from_dir(
            &dir,
            &HostPluginPolicy::development(),
            Arc::new(DelegateStrategy),
        )
        .unwrap();
        let err = strategy
            .plan(&NoopStore, GovernanceTask::Daily)
            .await
            .expect_err("busy loop should be rejected");
        assert!(err.to_string().contains("Rhai execution failed"));

        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn rhai_governance_plan_rejects_output_over_limit() {
        let dir = temp_plugin_dir("output");
        write_manifest_with_limits(
            &dir,
            r#"
                fn memoria_plugin(ctx) {
                    if ctx["phase"] == "plan" {
                        return #{
                            warnings: ["1234567890123456789012345678901234567890"],
                            requires_approval: true
                        };
                    }
                    return #{};
                }
            "#,
            100,
            16,
        );

        let strategy = RhaiGovernanceStrategy::load_from_dir(
            &dir,
            &HostPluginPolicy::development(),
            Arc::new(DelegateStrategy),
        )
        .unwrap();
        let err = strategy
            .plan(&NoopStore, GovernanceTask::Weekly)
            .await
            .expect_err("oversized output should be rejected");
        assert!(err.to_string().contains("Plugin output exceeded"));

        let _ = fs::remove_dir_all(dir);
    }
}
