use std::sync::Arc;

use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use ed25519_dalek::{Signer, SigningKey};
use memoria_service::{
    activate_plugin_binding, compute_package_sha256, load_active_governance_plugin,
    publish_plugin_package, upsert_trusted_plugin_signer, Config, GovernanceScheduler,
    MemoryService,
};
use memoria_storage::SqlMemoryStore;
use serde_json::json;
use tempfile::tempdir;

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

fn write_signed_package(
    signer_name: &str,
    signer_key: &SigningKey,
) -> Result<(tempfile::TempDir, String, String), Box<dyn std::error::Error>> {
    let dir = tempdir()?;
    let suffix = unique_suffix();
    let plugin_name = format!("memoria-governance-repo-{suffix}");
    let version = "1.0.0".to_string();
    std::fs::write(
        dir.path().join("policy.rhai"),
        r#"
            fn memoria_plugin(ctx) {
                if ctx.contains("plan") {
                    let plan = ctx["plan"];
                    if !plan.contains("warnings") {
                        plan["warnings"] = [];
                    }
                    plan["warnings"].push("repository-loaded");
                    return plan;
                }
                return ctx["report"];
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
        "capabilities": ["governance.plan"],
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

    let sha256 = compute_package_sha256(dir.path())?;
    let signature = BASE64_STANDARD.encode(signer_key.sign(sha256.as_bytes()).to_bytes());
    let mut signed = manifest;
    signed["integrity"]["sha256"] = json!(sha256);
    signed["integrity"]["signature"] = json!(signature);
    std::fs::write(
        dir.path().join("manifest.json"),
        serde_json::to_vec_pretty(&signed)?,
    )?;

    Ok((dir, signed["name"].as_str().unwrap().to_string(), version))
}

#[tokio::test]
async fn shared_plugin_repository_publishes_activates_and_loads_governance_plugins() {
    let store = SqlMemoryStore::connect(&db_url(), test_dim())
        .await
        .expect("connect");
    store.migrate().await.expect("migrate");

    let signer_name = format!("repo-signer-{}", unique_suffix());
    let signer_key = signing_key();
    let public_key = BASE64_STANDARD.encode(signer_key.verifying_key().as_bytes());
    upsert_trusted_plugin_signer(&store, &signer_name, &public_key, "test")
        .await
        .expect("register signer");

    let (package_dir, plugin_name, version) =
        write_signed_package(&signer_name, &signer_key).expect("signed package");
    let published = publish_plugin_package(&store, package_dir.path(), "test")
        .await
        .expect("publish package");
    assert_eq!(published.version, version);
    assert_eq!(published.domain, "governance");

    let binding = format!("binding-{}", unique_suffix());
    activate_plugin_binding(
        &store,
        "governance",
        &binding,
        &published.plugin_key,
        &published.version,
        "test",
    )
    .await
    .expect("activate binding");

    let loaded = load_active_governance_plugin(
        &store,
        &binding,
        Arc::new(memoria_service::DefaultGovernanceStrategy),
    )
    .await
    .expect("load active plugin")
    .expect("binding should resolve");
    assert_eq!(loaded.plugin_key, published.plugin_key);
    assert_eq!(loaded.version, published.version);

    let service = Arc::new(MemoryService::new_sql_with_llm(
        Arc::new(store),
        None,
        None,
    ));
    let config = Config {
        db_url: db_url(),
        db_name: "memoria_test".into(),
        embedding_provider: "mock".into(),
        embedding_model: "mock".into(),
        embedding_dim: test_dim(),
        embedding_api_key: String::new(),
        embedding_base_url: String::new(),
        llm_api_key: None,
        llm_base_url: "https://api.openai.com/v1".into(),
        llm_model: "gpt-4o-mini".into(),
        user: "default".into(),
        governance_plugin_binding: binding.clone(),
    };
    let scheduler = GovernanceScheduler::from_config(service, &config)
        .await
        .expect("scheduler from shared config");
    assert_eq!(scheduler.strategy_key(), published.plugin_key);
    assert!(plugin_name.contains("memoria-governance-repo-"));
}
