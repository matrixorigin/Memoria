use std::sync::Arc;

use async_trait::async_trait;
use memoria_core::MemoriaError;
use serde::{Deserialize, Serialize};
use tonic::transport::{Channel, Endpoint};

use crate::governance::{
    GovernanceExecution, GovernancePlan, GovernanceStore, GovernanceStrategy, GovernanceTask,
};
use crate::plugin::governance_hook::{
    apply_execution_patch, apply_plan_patch, ExecuteHookContext, PlanHookContext,
    PluginExecutionPatch, PluginPlanPatch,
};
use crate::plugin::manifest::{PluginPackage, PluginRuntimeKind};
use crate::strategy_domain::StrategyStatus;

use super::rhai_runtime::PluginRuntime;

pub mod proto {
    tonic::include_proto!("memoria.plugin.v1");
}

#[derive(Clone)]
pub struct GrpcGovernanceStrategy {
    package: PluginPackage,
    delegate: Arc<dyn GovernanceStrategy>,
    endpoint: String,
    channel: Channel,
}

impl GrpcGovernanceStrategy {
    pub async fn connect(
        package: PluginPackage,
        endpoint: impl Into<String>,
        delegate: Arc<dyn GovernanceStrategy>,
    ) -> Result<Self, MemoriaError> {
        let endpoint = endpoint.into();
        let grpc = package.manifest.entry.grpc.as_ref().ok_or_else(|| {
            MemoriaError::Blocked("gRPC runtime selected but `entry.grpc` is missing".into())
        })?;
        if grpc.protocol != "grpc" {
            return Err(MemoriaError::Blocked(format!(
                "Unsupported gRPC protocol `{}`",
                grpc.protocol
            )));
        }
        let channel = Endpoint::from_shared(endpoint.clone())
            .map_err(|err| MemoriaError::Blocked(format!("Invalid gRPC endpoint: {err}")))?
            .connect()
            .await
            .map_err(|err| {
                MemoriaError::Blocked(format!("Failed to connect gRPC runtime: {err}"))
            })?;

        let strategy = Self {
            package,
            delegate,
            endpoint,
            channel,
        };
        strategy.handshake().await?;
        Ok(strategy)
    }

    async fn handshake(&self) -> Result<(), MemoriaError> {
        let mut client =
            proto::strategy_runtime_client::StrategyRuntimeClient::new(self.channel.clone());
        let response = client
            .handshake(proto::HandshakeRequest {
                api_version: self.package.manifest.api_version.clone(),
                capabilities: self.package.manifest.capabilities.clone(),
                runtime: "grpc".into(),
            })
            .await
            .map_err(|err| MemoriaError::Blocked(format!("gRPC handshake failed: {err}")))?
            .into_inner();
        if !response.accepted {
            return Err(MemoriaError::Blocked(format!(
                "gRPC runtime rejected handshake: {}",
                response.reason
            )));
        }
        Ok(())
    }

    async fn call_remote<T, C>(&self, task: GovernanceTask, context: C) -> Result<T, MemoriaError>
    where
        T: for<'de> Deserialize<'de>,
        C: Serialize,
    {
        let mut client =
            proto::strategy_runtime_client::StrategyRuntimeClient::new(self.channel.clone());
        let payload_json = serde_json::to_vec(&context)?;
        let response = client
            .execute_governance(proto::GovernanceRequest {
                strategy_key: self.package.plugin_key.clone(),
                task: task.as_str().into(),
                payload_json,
            })
            .await
            .map_err(|err| MemoriaError::Blocked(format!("gRPC governance call failed: {err}")))?
            .into_inner();
        if !response.error_code.is_empty() {
            return Err(MemoriaError::Blocked(format!(
                "gRPC governance error {}: {}",
                response.error_code, response.status
            )));
        }
        serde_json::from_slice(&response.report_json).map_err(|err| {
            MemoriaError::Internal(format!("Failed to decode gRPC governance payload: {err}"))
        })
    }

    pub fn manifest(&self) -> &crate::plugin::PluginManifest {
        &self.package.manifest
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }
}

impl PluginRuntime for GrpcGovernanceStrategy {
    fn runtime_kind(&self) -> PluginRuntimeKind {
        PluginRuntimeKind::Grpc
    }
}

#[async_trait]
impl GovernanceStrategy for GrpcGovernanceStrategy {
    fn strategy_key(&self) -> &str {
        &self.package.plugin_key
    }

    async fn plan(
        &self,
        store: &dyn GovernanceStore,
        task: GovernanceTask,
    ) -> Result<GovernancePlan, MemoriaError> {
        let base_plan = self.delegate.plan(store, task).await?;
        if !self.package.manifest.has_capability("governance.plan") {
            return Ok(base_plan);
        }
        let patch: PluginPlanPatch = self
            .call_remote(
                task,
                PlanHookContext::new(self.strategy_key(), task, &base_plan),
            )
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
        if !self.package.manifest.has_capability("governance.execute") {
            return Ok(execution);
        }
        match self
            .call_remote::<PluginExecutionPatch, _>(
                task,
                ExecuteHookContext::new(self.strategy_key(), task, plan, &execution),
            )
            .await
        {
            Ok(patch) => apply_execution_patch(&mut execution, patch)?,
            Err(err) => {
                execution.report.status = StrategyStatus::Degraded;
                execution.report.warnings.push(format!(
                    "gRPC execution hook degraded and builtin result was retained: {err}"
                ));
                execution
                    .report
                    .metrics
                    .insert("plugin.runtime.grpc.degraded".into(), 1.0);
            }
        }
        Ok(execution)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::net::SocketAddr;

    use tokio::sync::oneshot;
    use tonic::{transport::Server, Request, Response, Status};

    use super::*;
    use crate::governance::{GovernanceRunSummary, GovernanceStore};
    use crate::plugin::manifest::{
        GrpcEntry, PluginCompatibility, PluginEntrypoint, PluginIntegrity, PluginLimits,
        PluginManifest, PluginMetadata, PluginPackage, PluginPermissions,
    };
    use crate::strategy_domain::StrategyReport;

    struct NoopStore;

    #[async_trait]
    impl GovernanceStore for NoopStore {
        async fn list_active_users(&self) -> Result<Vec<String>, MemoriaError> {
            Ok(vec!["u1".into()])
        }
        async fn cleanup_tool_results(&self, _: i64) -> Result<i64, MemoriaError> {
            Ok(0)
        }
        async fn cleanup_async_tasks(&self, _: i64) -> Result<i64, MemoriaError> {
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
        async fn cleanup_orphan_stats(&self) -> Result<i64, MemoriaError> {
            Ok(0)
        }
        async fn cleanup_edit_log(&self, _: i64) -> Result<i64, MemoriaError> { Ok(0) }
        async fn cleanup_feedback(&self, _: i64) -> Result<i64, MemoriaError> { Ok(0) }
        async fn create_safety_snapshot(&self, _: &str) -> (Option<String>, Option<String>) {
            (None, None)
        }
        async fn log_edit(&self, _: &str, _: &str, _: Option<&str>, _: Option<&str>, _: &str, _: Option<&str>) {}
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
                    users_processed: 1,
                    ..GovernanceRunSummary::default()
                },
                report: StrategyReport::default(),
            })
        }
    }

    #[derive(Default)]
    struct MockRuntime;

    #[tonic::async_trait]
    impl proto::strategy_runtime_server::StrategyRuntime for MockRuntime {
        async fn handshake(
            &self,
            _: Request<proto::HandshakeRequest>,
        ) -> Result<Response<proto::HandshakeResponse>, Status> {
            Ok(Response::new(proto::HandshakeResponse {
                accepted: true,
                reason: String::new(),
                capabilities: vec!["governance.plan".into(), "governance.execute".into()],
            }))
        }

        async fn execute_governance(
            &self,
            request: Request<proto::GovernanceRequest>,
        ) -> Result<Response<proto::GovernanceResponse>, Status> {
            let payload: serde_json::Value =
                serde_json::from_slice(&request.get_ref().payload_json)
                    .map_err(|err| Status::invalid_argument(format!("invalid payload: {err}")))?;
            let phase = payload["phase"].as_str().unwrap_or_default();
            let report_json = if phase == "plan" {
                serde_json::to_vec(&serde_json::json!({
                    "requires_approval": true,
                    "estimated_impact": { "grpc.plan": 1.0 }
                }))
                .unwrap()
            } else {
                serde_json::to_vec(&serde_json::json!({
                    "warnings": ["grpc-execute"],
                    "metrics": { "grpc.execute": 1.0 }
                }))
                .unwrap()
            };
            Ok(Response::new(proto::GovernanceResponse {
                status: "ok".into(),
                report_json,
                error_code: String::new(),
                retryable: false,
            }))
        }
    }

    async fn spawn_runtime() -> (String, oneshot::Sender<()>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr: SocketAddr = listener.local_addr().unwrap();
        drop(listener);
        let (tx, rx) = oneshot::channel();
        tokio::spawn(async move {
            Server::builder()
                .add_service(proto::strategy_runtime_server::StrategyRuntimeServer::new(
                    MockRuntime,
                ))
                .serve_with_shutdown(addr, async move {
                    let _ = rx.await;
                })
                .await
                .unwrap();
        });
        // Wait for the server to bind and start accepting connections.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        (format!("http://{addr}"), tx)
    }

    fn grpc_package() -> PluginPackage {
        let manifest = PluginManifest {
            name: "memoria-governance-grpc-test".into(),
            version: "1.0.0".into(),
            api_version: "v1".into(),
            runtime: PluginRuntimeKind::Grpc,
            entry: PluginEntrypoint {
                rhai: None,
                grpc: Some(GrpcEntry {
                    service: "memoria.plugin.v1.StrategyRuntime".into(),
                    protocol: "grpc".into(),
                }),
            },
            capabilities: vec!["governance.plan".into(), "governance.execute".into()],
            compatibility: PluginCompatibility {
                memoria: ">=0.1.0-rc1 <0.2.0".into(),
            },
            permissions: PluginPermissions {
                network: false,
                filesystem: false,
                env: vec![],
            },
            limits: PluginLimits {
                timeout_ms: 500,
                max_memory_mb: 64,
                max_output_bytes: 16384,
            },
            integrity: PluginIntegrity {
                sha256: "test".into(),
                signature: "test".into(),
                signer: "test".into(),
            },
            metadata: PluginMetadata::default(),
        };
        PluginPackage {
            root_dir: std::env::temp_dir(),
            plugin_key: manifest.plugin_key().unwrap(),
            script_path: std::env::temp_dir().join("unused"),
            entrypoint: "memoria_plugin".into(),
            manifest,
        }
    }

    #[tokio::test]
    async fn grpc_governance_strategy_calls_remote_runtime() {
        let (endpoint, shutdown) = spawn_runtime().await;
        let strategy =
            GrpcGovernanceStrategy::connect(grpc_package(), endpoint, Arc::new(DelegateStrategy))
                .await
                .unwrap();

        let plan = strategy
            .plan(&NoopStore, GovernanceTask::Daily)
            .await
            .unwrap();
        assert!(plan.requires_approval);
        assert_eq!(plan.estimated_impact.get("grpc.plan"), Some(&1.0));

        let execution = strategy
            .execute(&NoopStore, GovernanceTask::Daily, &plan)
            .await
            .unwrap();
        assert!(execution
            .report
            .warnings
            .iter()
            .any(|warning| warning == "grpc-execute"));
        assert_eq!(execution.report.metrics.get("grpc.execute"), Some(&1.0));

        let _ = shutdown.send(());
    }
}
