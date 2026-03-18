use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use chrono::{NaiveDateTime, Utc};
use memoria_core::MemoriaError;
use memoria_storage::SqlMemoryStore;
use semver::Version;
use serde::{Deserialize, Serialize};
use sqlx::Row;
use tempfile::tempdir;

use crate::governance::GovernanceStrategy;

use super::{
    load_plugin_package, HostPluginPolicy, PluginManifest, RhaiGovernanceStrategy,
};

#[derive(Debug, Clone)]
pub struct PluginRepositoryEntry {
    pub plugin_key: String,
    pub version: String,
    pub domain: String,
    pub name: String,
    pub signer: String,
    pub status: String,
    pub published_at: NaiveDateTime,
    pub published_by: String,
}

#[derive(Debug, Clone)]
pub struct TrustedPluginSignerEntry {
    pub signer: String,
    pub algorithm: String,
    pub public_key: String,
    pub is_active: bool,
}

#[derive(Clone)]
pub struct ActiveGovernancePlugin {
    pub binding_key: String,
    pub plugin_key: String,
    pub version: String,
    pub strategy: RhaiGovernanceStrategy,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredPluginFile {
    path: String,
    content_base64: String,
}

fn db_err(err: sqlx::Error) -> MemoriaError {
    MemoriaError::Database(err.to_string())
}

fn current_memoria_version() -> Version {
    Version::parse(env!("CARGO_PKG_VERSION")).unwrap_or_else(|_| Version::new(0, 1, 0))
}

pub async fn upsert_trusted_plugin_signer(
    store: &SqlMemoryStore,
    signer: &str,
    public_key_base64: &str,
    created_by: &str,
) -> Result<(), MemoriaError> {
    let now = Utc::now().naive_utc();
    sqlx::query(
        "INSERT INTO mem_plugin_signers \
             (signer, algorithm, public_key, is_active, created_at, updated_at, created_by) \
         VALUES (?, 'ed25519', ?, 1, ?, ?, ?) \
         ON DUPLICATE KEY UPDATE public_key = VALUES(public_key), is_active = 1, updated_at = VALUES(updated_at), created_by = VALUES(created_by)"
    )
    .bind(signer)
    .bind(public_key_base64)
    .bind(now)
    .bind(now)
    .bind(created_by)
    .execute(store.pool())
    .await
    .map_err(db_err)?;
    Ok(())
}

pub async fn list_trusted_plugin_signers(
    store: &SqlMemoryStore,
) -> Result<Vec<TrustedPluginSignerEntry>, MemoriaError> {
    let rows = sqlx::query(
        "SELECT signer, algorithm, public_key, is_active FROM mem_plugin_signers ORDER BY signer"
    )
    .fetch_all(store.pool())
    .await
    .map_err(db_err)?;
    rows.into_iter()
        .map(|row| {
            Ok(TrustedPluginSignerEntry {
                signer: row.try_get("signer").map_err(db_err)?,
                algorithm: row.try_get("algorithm").map_err(db_err)?,
                public_key: row.try_get("public_key").map_err(db_err)?,
                is_active: row.try_get("is_active").map_err(db_err)?,
            })
        })
        .collect()
}

pub async fn list_plugin_repository_entries(
    store: &SqlMemoryStore,
    domain: Option<&str>,
) -> Result<Vec<PluginRepositoryEntry>, MemoriaError> {
    let rows = if let Some(domain) = domain {
        sqlx::query(
            "SELECT plugin_key, version, domain, name, signer, status, published_at, published_by \
             FROM mem_plugin_packages WHERE domain = ? ORDER BY published_at DESC"
        )
        .bind(domain)
        .fetch_all(store.pool())
        .await
    } else {
        sqlx::query(
            "SELECT plugin_key, version, domain, name, signer, status, published_at, published_by \
             FROM mem_plugin_packages ORDER BY published_at DESC"
        )
        .fetch_all(store.pool())
        .await
    }
    .map_err(db_err)?;

    rows.into_iter()
        .map(|row| {
            Ok(PluginRepositoryEntry {
                plugin_key: row.try_get("plugin_key").map_err(db_err)?,
                version: row.try_get("version").map_err(db_err)?,
                domain: row.try_get("domain").map_err(db_err)?,
                name: row.try_get("name").map_err(db_err)?,
                signer: row.try_get("signer").map_err(db_err)?,
                status: row.try_get("status").map_err(db_err)?,
                published_at: row.try_get("published_at").map_err(db_err)?,
                published_by: row.try_get("published_by").map_err(db_err)?,
            })
        })
        .collect()
}

pub async fn publish_plugin_package(
    store: &SqlMemoryStore,
    package_dir: impl AsRef<Path>,
    published_by: &str,
) -> Result<PluginRepositoryEntry, MemoriaError> {
    let package_dir = package_dir.as_ref();
    let policy = repository_policy(store).await?;
    let package = load_plugin_package(package_dir.to_path_buf(), &policy)?;
    let payload = capture_package_payload(package_dir)?;
    let payload_json = serde_json::to_string(&payload)?;
    let manifest_json = serde_json::to_string(&package.manifest)?;
    let domain = domain_from_manifest(&package.manifest)?;
    let now = Utc::now().naive_utc();

    sqlx::query(
        "INSERT INTO mem_plugin_packages \
             (plugin_key, version, domain, name, runtime, manifest_json, package_payload, sha256, signature, signer, status, published_at, published_by) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'active', ?, ?) \
         ON DUPLICATE KEY UPDATE \
             domain = VALUES(domain), name = VALUES(name), runtime = VALUES(runtime), manifest_json = VALUES(manifest_json), \
             package_payload = VALUES(package_payload), sha256 = VALUES(sha256), signature = VALUES(signature), \
             signer = VALUES(signer), status = 'active', published_at = VALUES(published_at), published_by = VALUES(published_by)"
    )
    .bind(&package.plugin_key)
    .bind(&package.manifest.version)
    .bind(&domain)
    .bind(&package.manifest.name)
    .bind(format!("{:?}", package.manifest.runtime).to_lowercase())
    .bind(manifest_json)
    .bind(payload_json)
    .bind(&package.manifest.integrity.sha256)
    .bind(&package.manifest.integrity.signature)
    .bind(&package.manifest.integrity.signer)
    .bind(now)
    .bind(published_by)
    .execute(store.pool())
    .await
    .map_err(db_err)?;

    Ok(PluginRepositoryEntry {
        plugin_key: package.plugin_key,
        version: package.manifest.version,
        domain,
        name: package.manifest.name,
        signer: package.manifest.integrity.signer,
        status: "active".into(),
        published_at: now,
        published_by: published_by.into(),
    })
}

pub async fn activate_plugin_binding(
    store: &SqlMemoryStore,
    domain: &str,
    binding_key: &str,
    plugin_key: &str,
    version: &str,
    updated_by: &str,
) -> Result<(), MemoriaError> {
    let package_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM mem_plugin_packages WHERE plugin_key = ? AND version = ? AND status = 'active'"
    )
    .bind(plugin_key)
    .bind(version)
    .fetch_one(store.pool())
    .await
    .map_err(db_err)?;
    if package_count == 0 {
        return Err(MemoriaError::Blocked(format!(
            "Plugin package {plugin_key}@{version} is not published"
        )));
    }

    let now = Utc::now().naive_utc();
    sqlx::query(
        "INSERT INTO mem_plugin_bindings (domain, binding_key, plugin_key, version, updated_at, updated_by) \
         VALUES (?, ?, ?, ?, ?, ?) \
         ON DUPLICATE KEY UPDATE plugin_key = VALUES(plugin_key), version = VALUES(version), updated_at = VALUES(updated_at), updated_by = VALUES(updated_by)"
    )
    .bind(domain)
    .bind(binding_key)
    .bind(plugin_key)
    .bind(version)
    .bind(now)
    .bind(updated_by)
    .execute(store.pool())
    .await
    .map_err(db_err)?;
    Ok(())
}

pub async fn load_active_governance_plugin(
    store: &SqlMemoryStore,
    binding_key: &str,
    delegate: Arc<dyn GovernanceStrategy>,
) -> Result<Option<ActiveGovernancePlugin>, MemoriaError> {
    let Some(binding) = sqlx::query(
        "SELECT plugin_key, version FROM mem_plugin_bindings WHERE domain = 'governance' AND binding_key = ?"
    )
    .bind(binding_key)
    .fetch_optional(store.pool())
    .await
    .map_err(db_err)? else {
        return Ok(None);
    };

    let plugin_key: String = binding.try_get("plugin_key").map_err(db_err)?;
    let version: String = binding.try_get("version").map_err(db_err)?;
    let package_row = sqlx::query(
        "SELECT manifest_json, package_payload FROM mem_plugin_packages \
         WHERE plugin_key = ? AND version = ? AND status = 'active'"
    )
    .bind(&plugin_key)
    .bind(&version)
    .fetch_optional(store.pool())
    .await
    .map_err(db_err)?
    .ok_or_else(|| {
        MemoriaError::Blocked(format!(
            "Active governance binding {binding_key} points to missing package {plugin_key}@{version}"
        ))
    })?;

    let manifest_json: String = package_row.try_get("manifest_json").map_err(db_err)?;
    let payload_json: String = package_row.try_get("package_payload").map_err(db_err)?;
    let manifest: PluginManifest = serde_json::from_str(&manifest_json)?;
    let payload: Vec<StoredPluginFile> = serde_json::from_str(&payload_json)?;
    let temp = materialize_payload(&payload)?;
    let policy = repository_policy(store).await?;
    let mut package = load_plugin_package(temp.path().to_path_buf(), &policy)?;
    let script_source = fs::read_to_string(&package.script_path).map_err(|err| {
        MemoriaError::Blocked(format!(
            "Failed to read repository-backed Rhai script {}: {err}",
            package.script_path.display()
        ))
    })?;
    let virtual_root = PathBuf::from(format!(
        "plugin-repo://{}/{}",
        plugin_key,
        manifest.version
    ));
    package.root_dir = virtual_root.clone();
    package.script_path = virtual_root.join(
        manifest
            .entry
            .rhai
            .as_ref()
            .map(|entry| entry.script.as_str())
            .unwrap_or("plugin.rhai"),
    );

    let strategy = RhaiGovernanceStrategy::from_loaded_package(package, script_source, delegate)?;
    Ok(Some(ActiveGovernancePlugin {
        binding_key: binding_key.into(),
        plugin_key,
        version,
        strategy,
    }))
}

async fn repository_policy(store: &SqlMemoryStore) -> Result<HostPluginPolicy, MemoriaError> {
    let rows = sqlx::query(
        "SELECT signer, public_key FROM mem_plugin_signers WHERE is_active > 0"
    )
    .fetch_all(store.pool())
    .await
    .map_err(db_err)?;
    let mut policy = HostPluginPolicy::development();
    policy.current_memoria_version = current_memoria_version();
    policy.enforce_signatures = true;
    policy.trusted_signers.clear();
    policy.signer_public_keys.clear();
    for row in rows {
        let signer: String = row.try_get("signer").map_err(db_err)?;
        let public_key: String = row.try_get("public_key").map_err(db_err)?;
        policy.trusted_signers.insert(signer.clone());
        policy.signer_public_keys.insert(signer, public_key);
    }
    Ok(policy)
}

fn domain_from_manifest(manifest: &PluginManifest) -> Result<String, MemoriaError> {
    manifest
        .capabilities
        .first()
        .and_then(|cap| cap.split_once('.'))
        .map(|(domain, _)| domain.to_string())
        .ok_or_else(|| MemoriaError::Blocked("Plugin must declare at least one capability".into()))
}

fn capture_package_payload(root_dir: &Path) -> Result<Vec<StoredPluginFile>, MemoriaError> {
    let mut files = Vec::new();
    collect_files(root_dir, root_dir, &mut files)?;
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(files)
}

fn collect_files(
    root_dir: &Path,
    current: &Path,
    files: &mut Vec<StoredPluginFile>,
) -> Result<(), MemoriaError> {
    for entry in fs::read_dir(current).map_err(|err| {
        MemoriaError::Blocked(format!(
            "Failed to read plugin repository package directory {}: {err}",
            current.display()
        ))
    })? {
        let entry = entry.map_err(|err| {
            MemoriaError::Blocked(format!(
                "Failed to enumerate plugin repository package directory {}: {err}",
                current.display()
            ))
        })?;
        let path = entry.path();
        if path.is_dir() {
            collect_files(root_dir, &path, files)?;
        } else if path.is_file() {
            let relative = path
                .strip_prefix(root_dir)
                .map_err(|err| MemoriaError::Internal(format!("strip_prefix failed: {err}")))?;
            let bytes = fs::read(&path).map_err(|err| {
                MemoriaError::Blocked(format!(
                    "Failed to read plugin file {}: {err}",
                    path.display()
                ))
            })?;
            files.push(StoredPluginFile {
                path: relative.to_string_lossy().to_string(),
                content_base64: BASE64_STANDARD.encode(bytes),
            });
        }
    }
    Ok(())
}

fn materialize_payload(payload: &[StoredPluginFile]) -> Result<tempfile::TempDir, MemoriaError> {
    let dir = tempdir().map_err(|err| MemoriaError::Internal(format!("tempdir failed: {err}")))?;
    for file in payload {
        let path = dir.path().join(&file.path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|err| {
                MemoriaError::Internal(format!(
                    "Failed to create plugin repository staging dir {}: {err}",
                    parent.display()
                ))
            })?;
        }
        let bytes = BASE64_STANDARD
            .decode(&file.content_base64)
            .map_err(|err| MemoriaError::Blocked(format!("Invalid package payload encoding: {err}")))?;
        fs::write(&path, bytes).map_err(|err| {
            MemoriaError::Internal(format!(
                "Failed to materialize plugin repository file {}: {err}",
                path.display()
            ))
        })?;
    }
    Ok(dir)
}
