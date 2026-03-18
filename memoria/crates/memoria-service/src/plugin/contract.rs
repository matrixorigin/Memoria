use std::path::Path;
use std::sync::Arc;

use memoria_core::MemoriaError;

use crate::governance::{
    GovernanceExecution, GovernancePlan, GovernanceStore, GovernanceStrategy, GovernanceTask,
};

use super::{HostPluginPolicy, RhaiGovernanceStrategy};

pub struct GovernancePluginContractResult {
    pub strategy_key: String,
    pub plan: GovernancePlan,
    pub execution: GovernanceExecution,
}

pub struct GovernancePluginContractHarness {
    policy: HostPluginPolicy,
    delegate: Arc<dyn GovernanceStrategy>,
}

impl GovernancePluginContractHarness {
    pub fn new(policy: HostPluginPolicy, delegate: Arc<dyn GovernanceStrategy>) -> Self {
        Self { policy, delegate }
    }

    pub fn load_from_dir(
        &self,
        package_dir: impl AsRef<Path>,
    ) -> Result<RhaiGovernanceStrategy, MemoriaError> {
        RhaiGovernanceStrategy::load_from_dir(package_dir, &self.policy, self.delegate.clone())
    }

    pub async fn run_from_dir(
        &self,
        package_dir: impl AsRef<Path>,
        store: &dyn GovernanceStore,
        task: GovernanceTask,
    ) -> Result<GovernancePluginContractResult, MemoriaError> {
        let strategy = self.load_from_dir(package_dir)?;
        let strategy_key = strategy.strategy_key().to_string();
        let plan = strategy.plan(store, task).await?;
        let execution = strategy.execute(store, task, &plan).await?;

        Ok(GovernancePluginContractResult {
            strategy_key,
            plan,
            execution,
        })
    }
}
