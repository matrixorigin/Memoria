//! Pluggable retrieval strategy registry.
//! Mirrors Python's strategy/protocol.py + strategy/registry.py.
//!
//! Strategies are identified by "type:version" keys (e.g. "vector:v1", "activation:v1").
//! The registry maps keys to factory functions that produce boxed strategy instances.

use async_trait::async_trait;
use memoria_core::{MemoriaError, Memory};
use std::collections::HashMap;

use crate::plugin_registry::PluginRegistry;

/// A retrieval strategy — only responsible for retrieve().
#[async_trait]
pub trait RetrievalStrategy: Send + Sync {
    /// Unique key: "vector:v1", "activation:v1", etc.
    fn strategy_key(&self) -> &'static str;

    async fn retrieve(
        &self,
        user_id: &str,
        query: &str,
        query_embedding: Option<&[f32]>,
        top_k: i64,
    ) -> Result<Vec<Memory>, MemoriaError>;
}

type StrategyFactory = Box<dyn Fn() -> Box<dyn RetrievalStrategy> + Send + Sync>;

/// Registry of available retrieval strategies.
pub struct StrategyRegistry {
    entries: HashMap<String, StrategyFactory>,
}

impl StrategyRegistry {
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    /// Register a strategy factory under a key like "vector:v1".
    pub fn register<F>(&mut self, key: impl Into<String>, factory: F)
    where
        F: Fn() -> Box<dyn RetrievalStrategy> + Send + Sync + 'static,
    {
        self.entries.insert(key.into(), Box::new(factory));
    }

    /// Create a strategy instance by key.
    pub fn create(&self, key: &str) -> Option<Box<dyn RetrievalStrategy>> {
        self.entries.get(key).map(|f| f())
    }

    pub fn list_available(&self) -> Vec<String> {
        let mut keys: Vec<_> = self.entries.keys().cloned().collect();
        keys.sort();
        keys
    }

    /// Backward-compatible adapter into the Phase 1 multi-domain registry.
    pub fn into_plugin_registry(self) -> PluginRegistry {
        let mut registry = PluginRegistry::new();
        for (key, factory) in self.entries {
            registry.register_retrieval_factory(key, factory);
        }
        registry
    }
}

impl Default for StrategyRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MockStrategy {
        key: &'static str,
    }

    #[async_trait]
    impl RetrievalStrategy for MockStrategy {
        fn strategy_key(&self) -> &'static str {
            self.key
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

    #[test]
    fn test_registry_register_and_create() {
        let mut reg = StrategyRegistry::new();
        reg.register("vector:v1", || Box::new(MockStrategy { key: "vector:v1" }));
        reg.register("activation:v1", || {
            Box::new(MockStrategy {
                key: "activation:v1",
            })
        });

        let s = reg.create("vector:v1").expect("vector:v1 should exist");
        assert_eq!(s.strategy_key(), "vector:v1");

        let s2 = reg
            .create("activation:v1")
            .expect("activation:v1 should exist");
        assert_eq!(s2.strategy_key(), "activation:v1");

        assert!(reg.create("unknown:v1").is_none());

        assert_eq!(
            reg.list_available(),
            vec!["activation:v1".to_string(), "vector:v1".to_string()]
        );
    }

    #[test]
    fn test_registry_factory_creates_new_instances() {
        let mut reg = StrategyRegistry::new();
        reg.register("vector:v1", || Box::new(MockStrategy { key: "vector:v1" }));
        // Each call to create() returns a fresh instance
        let _a = reg.create("vector:v1").unwrap();
        let _b = reg.create("vector:v1").unwrap();
    }
}
