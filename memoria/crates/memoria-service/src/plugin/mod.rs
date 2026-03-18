mod contract;
mod manifest;
mod repository;
mod rhai_runtime;
mod template;

pub use contract::{GovernancePluginContractHarness, GovernancePluginContractResult};
pub use manifest::{
    compute_package_sha256, load_plugin_package, HostPluginPolicy, PluginCompatibility,
    PluginEntrypoint, PluginIntegrity, PluginLimits, PluginManifest, PluginMetadata, PluginPackage,
    PluginPermissions, PluginRuntimeKind,
};
pub use repository::{
    activate_plugin_binding, list_plugin_repository_entries, load_active_governance_plugin,
    list_trusted_plugin_signers, publish_plugin_package, upsert_trusted_plugin_signer,
    ActiveGovernancePlugin, PluginRepositoryEntry, TrustedPluginSignerEntry,
};
pub use rhai_runtime::{PluginRuntime, RhaiGovernanceStrategy};
pub use template::{GOVERNANCE_RHAI_TEMPLATE, GOVERNANCE_RHAI_TEMPLATE_ENTRYPOINT};
