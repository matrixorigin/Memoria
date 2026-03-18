use std::collections::HashMap;

use memoria_core::MemoriaError;
use serde::{Deserialize, Serialize};

use crate::governance::{
    GovernanceExecution, GovernancePlan, GovernanceRunSummary, GovernanceTask,
};
use crate::strategy_domain::{StrategyDecision, StrategyEvidence, StrategyReport, StrategyStatus};

#[derive(Debug, Serialize)]
pub(crate) struct PlanHookContext {
    phase: &'static str,
    task: &'static str,
    strategy_key: String,
    base_plan: SerializablePlan,
}

impl PlanHookContext {
    pub(crate) fn new(strategy_key: &str, task: GovernanceTask, plan: &GovernancePlan) -> Self {
        Self {
            phase: "plan",
            task: task.as_str(),
            strategy_key: strategy_key.to_string(),
            base_plan: SerializablePlan::from(plan),
        }
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct ExecuteHookContext {
    phase: &'static str,
    task: &'static str,
    strategy_key: String,
    plan: SerializablePlan,
    summary: SerializableSummary,
    report: SerializableReport,
}

impl ExecuteHookContext {
    pub(crate) fn new(
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
pub(crate) struct SerializablePlan {
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
pub(crate) struct SerializableSummary {
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
pub(crate) struct SerializableReport {
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
pub(crate) struct SerializableDecision {
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
pub(crate) struct SerializableEvidence {
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
pub(crate) struct PluginPlanPatch {
    pub(crate) requires_approval: Option<bool>,
    pub(crate) actions: Option<Vec<PluginDecision>>,
    pub(crate) estimated_impact: Option<HashMap<String, f64>>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct PluginExecutionPatch {
    pub(crate) status: Option<String>,
    pub(crate) decisions: Option<Vec<PluginDecision>>,
    pub(crate) metrics: Option<HashMap<String, f64>>,
    pub(crate) warnings: Option<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct PluginDecision {
    pub(crate) action: String,
    pub(crate) rationale: String,
    #[serde(default)]
    pub(crate) confidence: Option<f32>,
    #[serde(default)]
    pub(crate) evidence: Vec<PluginEvidence>,
    #[serde(default)]
    pub(crate) rollback_hint: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct PluginEvidence {
    pub(crate) source: String,
    pub(crate) summary: String,
    #[serde(default)]
    pub(crate) score: Option<f32>,
    #[serde(default)]
    pub(crate) references: Vec<String>,
}

pub(crate) fn apply_plan_patch(mut plan: GovernancePlan, patch: PluginPlanPatch) -> GovernancePlan {
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

pub(crate) fn apply_execution_patch(
    execution: &mut GovernanceExecution,
    patch: PluginExecutionPatch,
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
