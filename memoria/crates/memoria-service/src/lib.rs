pub mod config;
pub mod governance;
pub mod graph_domains;
pub mod pipeline;
pub mod plugin;
pub mod plugin_registry;
pub mod scheduler;
pub mod service;
pub mod strategy;
pub mod strategy_domain;
pub use config::Config;
pub use governance::{
    DefaultGovernanceStrategy, GovernanceExecution, GovernancePlan, GovernanceRunSummary,
    GovernanceStore, GovernanceStrategy, GovernanceTask,
};
pub use graph_domains::{
    Conflict, ConflictDetector, ConflictInput, ConsolidationInput, ConsolidationStrategy,
    DefaultConflictDetector, DefaultConsolidationStrategy, DefaultTrustLifecycleStrategy,
    GraphDomainStore, TrustEvaluationInput, TrustLifecycleStrategy,
};
pub use memoria_core::MemoriaError;
pub use pipeline::{MemoryPipeline, PipelineResult};
pub use plugin::{
    activate_plugin_binding, compute_package_sha256, list_plugin_repository_entries,
    list_trusted_plugin_signers, load_active_governance_plugin, publish_plugin_package,
    upsert_trusted_plugin_signer, ActiveGovernancePlugin, GovernancePluginContractHarness,
    GovernancePluginContractResult, HostPluginPolicy, PluginCompatibility, PluginManifest,
    PluginPackage, PluginRepositoryEntry, PluginRuntime, RhaiGovernanceStrategy,
    TrustedPluginSignerEntry, GOVERNANCE_RHAI_TEMPLATE, GOVERNANCE_RHAI_TEMPLATE_ENTRYPOINT,
};
pub use plugin_registry::{GovernancePluginMetadata, PluginRegistry};
pub use scheduler::GovernanceScheduler;
pub use service::{CandidateScore, ExplainLevel, MemoryService, PurgeResult, RetrievalExplain};
pub use strategy::{RetrievalStrategy, StrategyRegistry};
pub use strategy_domain::{StrategyDecision, StrategyEvidence, StrategyReport, StrategyStatus};
