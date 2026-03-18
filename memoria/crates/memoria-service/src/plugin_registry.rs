use std::collections::HashMap;
use std::sync::Arc;

use crate::governance::GovernanceStrategy;
use crate::graph_domains::{ConflictDetector, ConsolidationStrategy, TrustLifecycleStrategy};
use crate::strategy::RetrievalStrategy;

type RetrievalFactory = Box<dyn Fn() -> Box<dyn RetrievalStrategy> + Send + Sync>;
type GovernanceFactory = Box<dyn Fn() -> Arc<dyn GovernanceStrategy> + Send + Sync>;
type ConflictFactory = Box<dyn Fn() -> Arc<dyn ConflictDetector> + Send + Sync>;
type ConsolidationFactory = Box<dyn Fn() -> Arc<dyn ConsolidationStrategy> + Send + Sync>;
type TrustFactory = Box<dyn Fn() -> Arc<dyn TrustLifecycleStrategy> + Send + Sync>;

pub struct PluginRegistry {
    retrieval: HashMap<&'static str, RetrievalFactory>,
    governance: HashMap<&'static str, GovernanceFactory>,
    conflict: HashMap<&'static str, ConflictFactory>,
    consolidation: HashMap<&'static str, ConsolidationFactory>,
    trust: HashMap<&'static str, TrustFactory>,
}

impl PluginRegistry {
    pub fn new() -> Self {
        Self {
            retrieval: HashMap::new(),
            governance: HashMap::new(),
            conflict: HashMap::new(),
            consolidation: HashMap::new(),
            trust: HashMap::new(),
        }
    }

    pub fn register_retrieval<F>(&mut self, key: &'static str, factory: F)
    where
        F: Fn() -> Box<dyn RetrievalStrategy> + Send + Sync + 'static,
    {
        self.retrieval.insert(key, Box::new(factory));
    }

    pub(crate) fn register_retrieval_factory(
        &mut self,
        key: &'static str,
        factory: RetrievalFactory,
    ) {
        self.retrieval.insert(key, factory);
    }

    pub fn register_governance<F>(&mut self, key: &'static str, factory: F)
    where
        F: Fn() -> Arc<dyn GovernanceStrategy> + Send + Sync + 'static,
    {
        self.governance.insert(key, Box::new(factory));
    }

    pub fn register_conflict_detector<F>(&mut self, key: &'static str, factory: F)
    where
        F: Fn() -> Arc<dyn ConflictDetector> + Send + Sync + 'static,
    {
        self.conflict.insert(key, Box::new(factory));
    }

    pub fn register_consolidation<F>(&mut self, key: &'static str, factory: F)
    where
        F: Fn() -> Arc<dyn ConsolidationStrategy> + Send + Sync + 'static,
    {
        self.consolidation.insert(key, Box::new(factory));
    }

    pub fn register_trust<F>(&mut self, key: &'static str, factory: F)
    where
        F: Fn() -> Arc<dyn TrustLifecycleStrategy> + Send + Sync + 'static,
    {
        self.trust.insert(key, Box::new(factory));
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

    pub fn list_retrieval(&self) -> Vec<&'static str> {
        self.retrieval.keys().copied().collect()
    }

    pub fn list_governance(&self) -> Vec<&'static str> {
        self.governance.keys().copied().collect()
    }

    pub fn list_conflict_detectors(&self) -> Vec<&'static str> {
        self.conflict.keys().copied().collect()
    }

    pub fn list_consolidation(&self) -> Vec<&'static str> {
        self.consolidation.keys().copied().collect()
    }

    pub fn list_trust(&self) -> Vec<&'static str> {
        self.trust.keys().copied().collect()
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
    use memoria_core::{Memory, MemoriaError};

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
        fn strategy_key(&self) -> &'static str {
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
        assert_eq!(registry.list_retrieval(), vec!["retrieval:test:v1"]);
        assert_eq!(registry.list_governance(), vec!["governance:test:v1"]);
    }
}
