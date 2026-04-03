use std::net::SocketAddr;
use std::sync::Arc;

use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use ed25519_dalek::{Signer, SigningKey};
use memoria_core::MemoriaError;
use memoria_service::{
    get_plugin_audit_events, grpc_proto, load_active_governance_plugin, publish_plugin_package,
    review_plugin_package, upsert_plugin_binding_rule, upsert_trusted_plugin_signer,
    BindingRuleInput, Config, GovernanceExecution, GovernancePlan, GovernanceRunSummary,
    GovernanceScheduler, GovernanceStore, GovernanceStrategy, GovernanceTask, MemoryService,
    StrategyReport,
};
use memoria_storage::SqlMemoryStore;
use serde_json::json;
use tempfile::tempdir;
use tonic::{transport::Server, Request, Response, Status};

fn db_url() -> String {
    std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "mysql://root:111@localhost:6001/memoria_test".to_string())
}

fn test_dim() -> usize {
    8
}

fn signing_key() -> SigningKey {
    SigningKey::from_bytes(&[7u8; 32])
}

fn unique_suffix() -> String {
    format!(
        "{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock should be valid")
            .as_nanos()
    )
}

fn write_signed_rhai_package(
    signer_name: &str,
    signer_key: &SigningKey,
    version: &str,
) -> Result<(tempfile::TempDir, String), Box<dyn std::error::Error>> {
    write_signed_rhai_package_named(signer_name, signer_key, version, None)
}

fn write_signed_rhai_package_named(
    signer_name: &str,
    signer_key: &SigningKey,
    version: &str,
    name_override: Option<&str>,
) -> Result<(tempfile::TempDir, String), Box<dyn std::error::Error>> {
    let dir = tempdir()?;
    let plugin_name = match name_override {
        Some(n) => n.to_string(),
        None => format!("memoria-governance-repo-{}", unique_suffix()),
    };
    std::fs::write(
        dir.path().join("policy.rhai"),
        r#"
            fn memoria_plugin(ctx) {
                if ctx["phase"] == "plan" {
                    return #{
                        requires_approval: true,
                        warnings: ["repository-loaded"]
                    };
                }
                return #{ warnings: ["repository-execute"] };
            }
        "#,
    )?;

    let manifest = json!({
        "name": plugin_name,
        "version": version,
        "api_version": "v1",
        "runtime": "rhai",
        "entry": {
            "rhai": {
                "script": "policy.rhai",
                "entrypoint": "memoria_plugin"
            },
            "grpc": null
        },
        "capabilities": ["governance.plan", "governance.execute"],
        "compatibility": { "memoria": ">=0.1.0-rc1 <0.2.0" },
        "permissions": {
            "network": false,
            "filesystem": false,
            "env": []
        },
        "limits": {
            "timeout_ms": 500,
            "max_memory_mb": 32,
            "max_output_bytes": 8192
        },
        "integrity": {
            "sha256": "",
            "signature": "",
            "signer": signer_name
        },
        "metadata": {
            "display_name": "Repository test plugin"
        }
    });
    std::fs::write(
        dir.path().join("manifest.json"),
        serde_json::to_vec_pretty(&manifest)?,
    )?;

    let sha256 = memoria_service::compute_package_sha256(dir.path())?;
    let signature = BASE64_STANDARD.encode(signer_key.sign(sha256.as_bytes()).to_bytes());
    let mut signed = manifest;
    signed["integrity"]["sha256"] = json!(sha256);
    signed["integrity"]["signature"] = json!(signature);
    std::fs::write(
        dir.path().join("manifest.json"),
        serde_json::to_vec_pretty(&signed)?,
    )?;

    Ok((dir, signed["name"].as_str().unwrap().to_string()))
}

fn write_signed_grpc_package(
    signer_name: &str,
    signer_key: &SigningKey,
    version: &str,
) -> Result<tempfile::TempDir, Box<dyn std::error::Error>> {
    let dir = tempdir()?;
    let suffix = unique_suffix();
    let manifest = json!({
        "name": format!("memoria-governance-grpc-repo-{suffix}"),
        "version": version,
        "api_version": "v1",
        "runtime": "grpc",
        "entry": {
            "rhai": null,
            "grpc": {
                "service": "memoria.plugin.v1.StrategyRuntime",
                "protocol": "grpc"
            }
        },
        "capabilities": ["governance.plan", "governance.execute"],
        "compatibility": { "memoria": ">=0.1.0-rc1 <0.2.0" },
        "permissions": {
            "network": false,
            "filesystem": false,
            "env": []
        },
        "limits": {
            "timeout_ms": 500,
            "max_memory_mb": 32,
            "max_output_bytes": 8192
        },
        "integrity": {
            "sha256": "",
            "signature": "",
            "signer": signer_name
        },
        "metadata": {
            "display_name": "Repository gRPC plugin"
        }
    });
    std::fs::write(
        dir.path().join("manifest.json"),
        serde_json::to_vec_pretty(&manifest)?,
    )?;
    let sha256 = memoria_service::compute_package_sha256(dir.path())?;
    let signature = BASE64_STANDARD.encode(signer_key.sign(sha256.as_bytes()).to_bytes());
    let mut signed = manifest;
    signed["integrity"]["sha256"] = json!(sha256);
    signed["integrity"]["signature"] = json!(signature);
    std::fs::write(
        dir.path().join("manifest.json"),
        serde_json::to_vec_pretty(&signed)?,
    )?;
    Ok(dir)
}

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
    async fn cleanup_orphaned_incrementals(&self, _: &str, _: i64) -> Result<i64, MemoriaError> {
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
    async fn cleanup_orphan_graph_data(&self) -> Result<i64, MemoriaError> {
        Ok(0)
    }
    async fn cleanup_edit_log(&self, _: i64) -> Result<i64, MemoriaError> {
        Ok(0)
    }
    async fn cleanup_feedback(&self, _: i64) -> Result<i64, MemoriaError> {
        Ok(0)
    }
    async fn create_safety_snapshot(&self, _: &str) -> (Option<String>, Option<String>) {
        (None, None)
    }
    async fn log_edit(
        &self,
        _: &str,
        _: &str,
        _: Option<&str>,
        _: Option<&str>,
        _: &str,
        _: Option<&str>,
    ) {
    }
}

#[derive(Default)]
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
            estimated_impact: std::collections::HashMap::new(),
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
struct MockGrpcRuntime;

#[tonic::async_trait]
impl grpc_proto::strategy_runtime_server::StrategyRuntime for MockGrpcRuntime {
    async fn handshake(
        &self,
        _: Request<grpc_proto::HandshakeRequest>,
    ) -> Result<Response<grpc_proto::HandshakeResponse>, Status> {
        Ok(Response::new(grpc_proto::HandshakeResponse {
            accepted: true,
            reason: String::new(),
            capabilities: vec!["governance.plan".into(), "governance.execute".into()],
        }))
    }

    async fn execute_governance(
        &self,
        request: Request<grpc_proto::GovernanceRequest>,
    ) -> Result<Response<grpc_proto::GovernanceResponse>, Status> {
        let payload: serde_json::Value = serde_json::from_slice(&request.get_ref().payload_json)
            .map_err(|err| Status::invalid_argument(format!("invalid payload: {err}")))?;
        let report_json = if payload["phase"] == "plan" {
            serde_json::to_vec(&json!({
                "requires_approval": true,
                "estimated_impact": { "grpc.plan": 1.0 }
            }))
            .unwrap()
        } else {
            serde_json::to_vec(&json!({
                "warnings": ["grpc-execute"],
                "metrics": { "grpc.execute": 1.0 }
            }))
            .unwrap()
        };
        Ok(Response::new(grpc_proto::GovernanceResponse {
            status: "ok".into(),
            report_json,
            error_code: String::new(),
            retryable: false,
        }))
    }
}

async fn spawn_grpc_runtime() -> (String, tokio::sync::oneshot::Sender<()>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    drop(listener);
    let (tx, rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        Server::builder()
            .add_service(
                grpc_proto::strategy_runtime_server::StrategyRuntimeServer::new(MockGrpcRuntime),
            )
            .serve_with_shutdown(addr, async move {
                let _ = rx.await;
            })
            .await
            .unwrap();
    });
    (format!("http://{addr}"), tx)
}

#[tokio::test]
async fn repository_requires_review_before_activation_and_startup_load() {
    let instance_id = uuid::Uuid::new_v4().to_string();
    let store = SqlMemoryStore::connect(&db_url(), test_dim(), instance_id)
        .await
        .unwrap();
    store.migrate().await.unwrap();

    let signer_name = format!("repo-signer-{}", unique_suffix());
    let signer_key = signing_key();
    let public_key = BASE64_STANDARD.encode(signer_key.verifying_key().as_bytes());
    upsert_trusted_plugin_signer(&store, &signer_name, &public_key, "test")
        .await
        .unwrap();

    let (package_dir, plugin_name) =
        write_signed_rhai_package(&signer_name, &signer_key, "1.0.0").unwrap();
    let published = publish_plugin_package(&store, package_dir.path(), "test")
        .await
        .unwrap();
    assert_eq!(published.status, "pending");

    let err = memoria_service::activate_plugin_binding(
        &store,
        "governance",
        "default",
        &published.plugin_key,
        &published.version,
        "test",
    )
    .await
    .expect_err("pending package should not activate");
    assert!(err.to_string().contains("is not active"));

    review_plugin_package(
        &store,
        &published.plugin_key,
        &published.version,
        "active",
        Some("approved"),
        "reviewer",
    )
    .await
    .unwrap();
    memoria_service::activate_plugin_binding(
        &store,
        "governance",
        "default",
        &published.plugin_key,
        &published.version,
        "test",
    )
    .await
    .unwrap();

    let loaded = load_active_governance_plugin(
        &store,
        "default",
        "system",
        Arc::new(memoria_service::DefaultGovernanceStrategy),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(loaded.plugin_key, published.plugin_key);
    let events = get_plugin_audit_events(&store, None, Some(&published.plugin_key), None, 10)
        .await
        .unwrap();
    assert!(events
        .iter()
        .any(|event| event.event_type == "package.published"));
    assert!(events
        .iter()
        .any(|event| event.event_type == "binding.loaded"));

    let sql_store = Arc::new(store);
    let service = Arc::new(MemoryService::new_sql_with_llm(sql_store, None, None).await);
    let config = Config {
        db_url: db_url(),
        db_name: "memoria_test".into(),
        shared_db_url: db_url().replace("/memoria_test", "/memoria_shared"),
        multi_db: false,
        embedding_provider: "mock".into(),
        embedding_model: "mock".into(),
        embedding_dim: test_dim(),
        embedding_api_key: String::new(),
        embedding_base_url: String::new(),
        embedding_endpoints: vec![],
        llm_api_key: None,
        llm_base_url: "https://api.openai.com/v1".into(),
        llm_model: "gpt-4o-mini".into(),
        user: "default".into(),
        governance_plugin_binding: "default".into(),
        governance_plugin_subject: "system".into(),
        governance_plugin_dir: None,
        instance_id: "test-instance".into(),
        lock_ttl_secs: 120,
    };
    let scheduler = GovernanceScheduler::from_config(service, &config)
        .await
        .unwrap();
    assert_eq!(scheduler.strategy_key(), published.plugin_key);
    assert!(plugin_name.contains("memoria-governance-repo-"));
}

#[tokio::test]
async fn repository_binding_rules_support_semver_selection_and_subject_freeze() {
    let instance_id = uuid::Uuid::new_v4().to_string();
    let store = SqlMemoryStore::connect(&db_url(), test_dim(), instance_id)
        .await
        .unwrap();
    store.migrate().await.unwrap();

    let signer_name = format!("repo-signer-{}", unique_suffix());
    let signer_key = signing_key();
    let public_key = BASE64_STANDARD.encode(signer_key.verifying_key().as_bytes());
    upsert_trusted_plugin_signer(&store, &signer_name, &public_key, "test")
        .await
        .unwrap();

    let shared_name = format!("memoria-governance-repo-{}", unique_suffix());

    let (v1_dir, _) =
        write_signed_rhai_package_named(&signer_name, &signer_key, "1.0.0", Some(&shared_name))
            .unwrap();
    let package_v1 = publish_plugin_package(&store, v1_dir.path(), "test")
        .await
        .unwrap();
    review_plugin_package(
        &store,
        &package_v1.plugin_key,
        &package_v1.version,
        "active",
        Some("approve v1"),
        "reviewer",
    )
    .await
    .unwrap();

    let (v11_dir, _) =
        write_signed_rhai_package_named(&signer_name, &signer_key, "1.1.0", Some(&shared_name))
            .unwrap();
    let package_v11 = publish_plugin_package(&store, v11_dir.path(), "test")
        .await
        .unwrap();
    review_plugin_package(
        &store,
        &package_v11.plugin_key,
        &package_v11.version,
        "active",
        Some("approve v1.1"),
        "reviewer",
    )
    .await
    .unwrap();
    assert_eq!(package_v1.plugin_key, package_v11.plugin_key);

    upsert_plugin_binding_rule(
        &store,
        BindingRuleInput {
            domain: "governance",
            binding_key: "semver",
            subject_key: "*",
            priority: 100,
            plugin_key: &package_v1.plugin_key,
            selector_kind: "semver",
            selector_value: "^1.0",
            rollout_percent: 100,
            transport_endpoint: None,
            actor: "test",
        },
    )
    .await
    .unwrap();
    upsert_plugin_binding_rule(
        &store,
        BindingRuleInput {
            domain: "governance",
            binding_key: "semver",
            subject_key: "tenant-freeze",
            priority: 10,
            plugin_key: &package_v1.plugin_key,
            selector_kind: "exact",
            selector_value: "1.0.0",
            rollout_percent: 100,
            transport_endpoint: None,
            actor: "test",
        },
    )
    .await
    .unwrap();

    let global = load_active_governance_plugin(
        &store,
        "semver",
        "tenant-default",
        Arc::new(DelegateStrategy),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(global.version, "1.1.0");

    let frozen = load_active_governance_plugin(
        &store,
        "semver",
        "tenant-freeze",
        Arc::new(DelegateStrategy),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(frozen.version, "1.0.0");
}

#[tokio::test]
async fn repository_loads_grpc_plugin_from_shared_binding() {
    let instance_id = uuid::Uuid::new_v4().to_string();
    let store = SqlMemoryStore::connect(&db_url(), test_dim(), instance_id)
        .await
        .unwrap();
    store.migrate().await.unwrap();

    let signer_name = format!("repo-signer-{}", unique_suffix());
    let signer_key = signing_key();
    let public_key = BASE64_STANDARD.encode(signer_key.verifying_key().as_bytes());
    upsert_trusted_plugin_signer(&store, &signer_name, &public_key, "test")
        .await
        .unwrap();

    let package_dir = write_signed_grpc_package(&signer_name, &signer_key, "1.0.0").unwrap();
    let published = publish_plugin_package(&store, package_dir.path(), "test")
        .await
        .unwrap();
    review_plugin_package(
        &store,
        &published.plugin_key,
        &published.version,
        "active",
        Some("approve grpc"),
        "reviewer",
    )
    .await
    .unwrap();

    let (endpoint, shutdown) = spawn_grpc_runtime().await;
    upsert_plugin_binding_rule(
        &store,
        BindingRuleInput {
            domain: "governance",
            binding_key: "grpc",
            subject_key: "*",
            priority: 100,
            plugin_key: &published.plugin_key,
            selector_kind: "exact",
            selector_value: &published.version,
            rollout_percent: 100,
            transport_endpoint: Some(&endpoint),
            actor: "test",
        },
    )
    .await
    .unwrap();

    let loaded =
        load_active_governance_plugin(&store, "grpc", "system", Arc::new(DelegateStrategy))
            .await
            .unwrap()
            .unwrap();

    let plan = loaded
        .strategy
        .plan(&NoopStore, GovernanceTask::Daily)
        .await
        .unwrap();
    assert!(plan.requires_approval);
    let execution = loaded
        .strategy
        .execute(&NoopStore, GovernanceTask::Daily, &plan)
        .await
        .unwrap();
    assert_eq!(execution.report.metrics.get("grpc.execute"), Some(&1.0));

    let _ = shutdown.send(());
}
