mod contract;
mod governance_hook;
mod grpc_runtime;
mod manifest;
mod repository;
mod rhai_runtime;
mod template;

pub use contract::{GovernancePluginContractHarness, GovernancePluginContractResult};
pub use grpc_runtime::{proto as grpc_proto, GrpcGovernanceStrategy};
pub use manifest::{
    compute_package_sha256, load_plugin_package, HostPluginPolicy, PluginCompatibility,
    PluginEntrypoint, PluginIntegrity, PluginLimits, PluginManifest, PluginMetadata, PluginPackage,
    PluginPermissions, PluginRuntimeKind,
};
pub use repository::{
    activate_plugin_binding, build_local_governance_strategy, get_plugin_audit_events,
    list_binding_rules, list_plugin_compatibility_matrix, list_plugin_repository_entries,
    list_trusted_plugin_signers, load_active_governance_plugin, publish_plugin_package,
    publish_plugin_package_dev, record_runtime_plugin_event, review_plugin_package,
    score_plugin_package, upsert_plugin_binding_rule, upsert_trusted_plugin_signer,
    ActiveGovernancePlugin, BindingRuleInput, PluginAuditEvent, PluginBindingRule,
    PluginCompatibilityEntry, PluginRepositoryEntry, TrustedPluginSignerEntry,
};
pub use rhai_runtime::{PluginRuntime, RhaiGovernanceStrategy};
pub use template::{GOVERNANCE_RHAI_TEMPLATE, GOVERNANCE_RHAI_TEMPLATE_ENTRYPOINT};
