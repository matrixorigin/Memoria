use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StrategyStatus {
    Success,
    Rejected,
    Degraded,
    Failed,
}

impl StrategyStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Rejected => "rejected",
            Self::Degraded => "degraded",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct StrategyEvidence {
    pub source: String,
    pub summary: String,
    pub score: Option<f32>,
    pub references: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StrategyDecision {
    pub action: String,
    pub confidence: Option<f32>,
    pub rationale: String,
    pub evidence: Vec<StrategyEvidence>,
    pub rollback_hint: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StrategyReport {
    pub status: StrategyStatus,
    pub decisions: Vec<StrategyDecision>,
    pub metrics: HashMap<String, f64>,
    pub warnings: Vec<String>,
}

impl Default for StrategyReport {
    fn default() -> Self {
        Self {
            status: StrategyStatus::Success,
            decisions: Vec::new(),
            metrics: HashMap::new(),
            warnings: Vec::new(),
        }
    }
}
