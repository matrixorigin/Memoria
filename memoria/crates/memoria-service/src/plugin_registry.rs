use std::collections::HashMap;
use std::sync::Arc;

use crate::governance::GovernanceStrategy;
use crate::graph_domains::{ConflictDetector, ConsolidationStrategy, TrustLifecycleStrategy};
use crate::plugin::{HostPluginPolicy, PluginManifest, RhaiGovernanceStrategy};
use crate::strategy::RetrievalStrategy;

type RetrievalFactory = Box<dyn Fn() -> Box<dyn RetrievalStrategy> + Send + Sync>;
type GovernanceFactory = Box<dyn Fn() -> Arc<dyn GovernanceStrategy> + Send + Sync>;
type ConflictFactory = Box<dyn Fn() -> Arc<dyn ConflictDetector> + Send + Sync>;
type ConsolidationFactory = Box<dyn Fn() -> Arc<dyn ConsolidationStrategy> + Send + Sync>;
type TrustFactory = Box<dyn Fn() -> Arc<dyn TrustLifecycleStrategy> + Send + Sync>;

#[derive(Debug, Clone)]
pub struct GovernancePluginMetadata {
    pub key: String,
    pub manifest: PluginManifest,
    pub package_root: std::path::PathBuf,
}

pub struct PluginRegistry {
    retrieval: HashMap<String, RetrievalFactory>,
    governance: HashMap<String, GovernanceFactory>,
    governance_metadata: HashMap<String, GovernancePluginMetadata>,
    conflict: HashMap<String, ConflictFactory>,
    consolidation: HashMap<String, ConsolidationFactory>,
    trust: HashMap<String, TrustFactory>,
}

impl PluginRegistry {
    pub fn new() -> Self {
        Self {
            retrieval: HashMap::new(),
            governance: HashMap::new(),
            governance_metadata: HashMap::new(),
            conflict: HashMap::new(),
            consolidation: HashMap::new(),
            trust: HashMap::new(),
        }
    }

    pub fn register_retrieval<F>(&mut self, key: impl Into<String>, factory: F)
    where
        F: Fn() -> Box<dyn RetrievalStrategy> + Send + Sync + 'static,
    {
        self.retrieval.insert(key.into(), Box::new(factory));
    }

    pub(crate) fn register_retrieval_factory(
        &mut self,
        key: impl Into<String>,
        factory: RetrievalFactory,
    ) {
        self.retrieval.insert(key.into(), factory);
    }

    pub fn register_governance<F>(&mut self, key: impl Into<String>, factory: F)
    where
        F: Fn() -> Arc<dyn GovernanceStrategy> + Send + Sync + 'static,
    {
        self.governance.insert(key.into(), Box::new(factory));
    }

    pub fn register_conflict_detector<F>(&mut self, key: impl Into<String>, factory: F)
    where
        F: Fn() -> Arc<dyn ConflictDetector> + Send + Sync + 'static,
    {
        self.conflict.insert(key.into(), Box::new(factory));
    }

    pub fn register_consolidation<F>(&mut self, key: impl Into<String>, factory: F)
    where
        F: Fn() -> Arc<dyn ConsolidationStrategy> + Send + Sync + 'static,
    {
        self.consolidation.insert(key.into(), Box::new(factory));
    }

    pub fn register_trust<F>(&mut self, key: impl Into<String>, factory: F)
    where
        F: Fn() -> Arc<dyn TrustLifecycleStrategy> + Send + Sync + 'static,
    {
        self.trust.insert(key.into(), Box::new(factory));
    }

    pub fn register_rhai_governance_plugin(
        &mut self,
        package_dir: impl AsRef<std::path::Path>,
        policy: HostPluginPolicy,
        delegate: Arc<dyn GovernanceStrategy>,
    ) -> Result<String, memoria_core::MemoriaError> {
        let strategy =
            RhaiGovernanceStrategy::load_from_dir(package_dir.as_ref(), &policy, delegate)?;
        let key = strategy.strategy_key().to_string();
        let metadata = GovernancePluginMetadata {
            key: key.clone(),
            manifest: strategy.manifest().clone(),
            package_root: strategy.package_root().to_path_buf(),
        };
        let strategy = Arc::new(strategy);
        self.governance
            .insert(key.clone(), Box::new(move || strategy.clone()));
        self.governance_metadata.insert(key.clone(), metadata);
        Ok(key)
    }

    pub fn governance_metadata(&self, key: &str) -> Option<&GovernancePluginMetadata> {
        self.governance_metadata.get(key)
    }

    pub fn list_governance_plugins(&self) -> Vec<GovernancePluginMetadata> {
        let mut plugins: Vec<_> = self.governance_metadata.values().cloned().collect();
        plugins.sort_by(|a, b| a.key.cmp(&b.key));
        plugins
    }

    pub fn create_retrieval(&self, key: &str) -> Option<Box<dyn RetrievalStrategy>> {
        self.retrieval.get(key).map(|factory| factory())
    }

    pub fn create_governance(&self, key: &str) -> Option<Arc<dyn GovernanceStrategy>> {
        self.governance.get(key).map(|factory| factory())
    }

    pub fn create_conflict_detector(&self, key: &str) -> Option<Arc<dyn ConflictDetector>> {
        self.conflict.get(key).map(|factory| factory())
    }

    pub fn create_consolidation(&self, key: &str) -> Option<Arc<dyn ConsolidationStrategy>> {
        self.consolidation.get(key).map(|factory| factory())
    }

    pub fn create_trust(&self, key: &str) -> Option<Arc<dyn TrustLifecycleStrategy>> {
        self.trust.get(key).map(|factory| factory())
    }

    pub fn list_retrieval(&self) -> Vec<String> {
        let mut keys: Vec<_> = self.retrieval.keys().cloned().collect();
        keys.sort();
        keys
    }

    pub fn list_governance(&self) -> Vec<String> {
        let mut keys: Vec<_> = self.governance.keys().cloned().collect();
        keys.sort();
        keys
    }

    pub fn list_conflict_detectors(&self) -> Vec<String> {
        let mut keys: Vec<_> = self.conflict.keys().cloned().collect();
        keys.sort();
        keys
    }

    pub fn list_consolidation(&self) -> Vec<String> {
        let mut keys: Vec<_> = self.consolidation.keys().cloned().collect();
        keys.sort();
        keys
    }

    pub fn list_trust(&self) -> Vec<String> {
        let mut keys: Vec<_> = self.trust.keys().cloned().collect();
        keys.sort();
        keys
    }
}

impl Default for PluginRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use memoria_core::{MemoriaError, Memory};
    use std::fs;
    use std::path::Path;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;
    use crate::governance::{
        GovernanceExecution, GovernancePlan, GovernanceRunSummary, GovernanceStore, GovernanceTask,
    };
    use crate::strategy_domain::StrategyReport;

    struct MockRetrieval;

    #[async_trait]
    impl RetrievalStrategy for MockRetrieval {
        fn strategy_key(&self) -> &'static str {
            "retrieval:test:v1"
        }

        async fn retrieve(
            &self,
            _: &str,
            _: &str,
            _: Option<&[f32]>,
            _: i64,
        ) -> Result<Vec<Memory>, MemoriaError> {
            Ok(vec![])
        }
    }

    #[derive(Default)]
    struct MockGovernance;

    #[async_trait]
    impl GovernanceStrategy for MockGovernance {
        fn strategy_key(&self) -> &str {
            "governance:test:v1"
        }

        async fn plan(
            &self,
            _: &dyn GovernanceStore,
            _: GovernanceTask,
        ) -> Result<GovernancePlan, MemoriaError> {
            Ok(GovernancePlan::default())
        }

        async fn execute(
            &self,
            _: &dyn GovernanceStore,
            _: GovernanceTask,
            _: &GovernancePlan,
        ) -> Result<GovernanceExecution, MemoriaError> {
            Ok(GovernanceExecution {
                summary: GovernanceRunSummary::default(),
                report: StrategyReport::default(),
            })
        }
    }

    #[test]
    fn registry_registers_and_creates_multiple_domains() {
        let mut registry = PluginRegistry::new();
        registry.register_retrieval("retrieval:test:v1", || Box::new(MockRetrieval));
        registry.register_governance("governance:test:v1", || Arc::new(MockGovernance));

        assert!(registry.create_retrieval("retrieval:test:v1").is_some());
        assert!(registry.create_governance("governance:test:v1").is_some());
        assert_eq!(
            registry.list_retrieval(),
            vec!["retrieval:test:v1".to_string()]
        );
        assert_eq!(
            registry.list_governance(),
            vec!["governance:test:v1".to_string()]
        );
    }

    fn temp_plugin_dir(name: &str) -> std::path::PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("memoria-plugin-registry-{name}-{nonce}"))
    }

    fn write_manifest(dir: &Path) {
        fs::create_dir_all(dir).unwrap();
        fs::write(
            dir.join("plugin.rhai"),
            r#"
                fn memoria_plugin(ctx) {
                    if ctx["phase"] == "plan" {
                        return #{ requires_approval: true };
                    }
                    return #{ "metrics": #{ "plugin.registry.loaded": 1.0 } };
                }
            "#,
        )
        .unwrap();
        let mut manifest = serde_json::json!({
            "name": "memoria-governance-registry-test",
            "version": "1.0.0",
            "api_version": "v1",
            "runtime": "rhai",
            "entry": { "rhai": { "script": "plugin.rhai", "entrypoint": "memoria_plugin" } },
            "capabilities": ["governance.plan", "governance.execute"],
            "compatibility": { "memoria": ">=0.1.0-rc1 <0.2.0" },
            "permissions": { "network": false, "filesystem": false, "env": [] },
            "limits": { "timeout_ms": 200, "max_memory_mb": 64, "max_output_bytes": 16384 },
            "integrity": { "sha256": "", "signature": "dev-signature", "signer": "dev-signer" },
            "metadata": { "display_name": "Registry Test Plugin" }
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

    #[test]
    fn registry_tracks_rhai_governance_metadata() {
        let dir = temp_plugin_dir("metadata");
        write_manifest(&dir);

        let mut registry = PluginRegistry::new();
        let key = registry
            .register_rhai_governance_plugin(
                &dir,
                HostPluginPolicy::development(),
                Arc::new(MockGovernance),
            )
            .unwrap();

        assert_eq!(key, "governance:registry-test:v1");
        assert_eq!(registry.list_governance(), vec![key.clone()]);
        assert!(registry.create_governance(&key).is_some());

        let metadata = registry.governance_metadata(&key).unwrap();
        assert_eq!(metadata.manifest.name, "memoria-governance-registry-test");
        assert_eq!(metadata.package_root, dir);
        assert_eq!(registry.list_governance_plugins().len(), 1);

        let _ = fs::remove_dir_all(&dir);
    }
}
