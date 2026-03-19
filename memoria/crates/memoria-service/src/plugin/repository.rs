use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use chrono::{NaiveDateTime, Utc};
use memoria_core::MemoriaError;
use memoria_storage::SqlMemoryStore;
use semver::{Version, VersionReq};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use tempfile::tempdir;
use uuid::Uuid;

use crate::governance::GovernanceStrategy;

use super::{
    load_plugin_package, GrpcGovernanceStrategy, HostPluginPolicy, PluginManifest, PluginPackage,
    PluginRuntimeKind, RhaiGovernanceStrategy,
};

#[derive(Debug, Clone, Serialize)]
pub struct PluginRepositoryEntry {
    pub plugin_key: String,
    pub version: String,
    pub domain: String,
    pub name: String,
    pub runtime: String,
    pub signer: String,
    pub status: String,
    pub review_status: String,
    pub score: f64,
    pub published_at: NaiveDateTime,
    pub published_by: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct TrustedPluginSignerEntry {
    pub signer: String,
    pub algorithm: String,
    pub public_key: String,
    pub is_active: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct PluginCompatibilityEntry {
    pub plugin_key: String,
    pub version: String,
    pub domain: String,
    pub runtime: String,
    pub status: String,
    pub review_status: String,
    pub supported: bool,
    pub reason: String,
    pub compatibility: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct PluginBindingRule {
    pub rule_id: String,
    pub domain: String,
    pub binding_key: String,
    pub subject_key: String,
    pub priority: i64,
    pub plugin_key: String,
    pub selector_kind: String,
    pub selector_value: String,
    pub rollout_percent: i64,
    pub transport_endpoint: Option<String>,
    pub status: String,
    pub updated_at: NaiveDateTime,
    pub updated_by: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct PluginAuditEvent {
    pub event_type: String,
    pub status: String,
    pub message: String,
    pub plugin_key: Option<String>,
    pub version: Option<String>,
    pub binding_key: Option<String>,
    pub subject_key: Option<String>,
    pub actor: String,
    pub created_at: NaiveDateTime,
}

#[derive(Clone)]
pub struct ActiveGovernancePlugin {
    pub binding_key: String,
    pub subject_key: String,
    pub plugin_key: String,
    pub version: String,
    pub strategy: Arc<dyn GovernanceStrategy>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredPluginFile {
    path: String,
    content_base64: String,
}

#[derive(Debug, Clone)]
struct PackageRecord {
    plugin_key: String,
    version: String,
    runtime: PluginRuntimeKind,
    manifest: PluginManifest,
    payload: Vec<StoredPluginFile>,
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
    record_plugin_audit_event(
        store,
        AuditEventInput {
            domain: None,
            binding_key: None,
            subject_key: None,
            plugin_key: None,
            version: None,
            event_type: "signer.upserted",
            status: "success",
            message: format!("Trusted signer `{signer}` was upserted"),
            actor: created_by,
        },
    )
    .await?;
    Ok(())
}

pub async fn list_trusted_plugin_signers(
    store: &SqlMemoryStore,
) -> Result<Vec<TrustedPluginSignerEntry>, MemoriaError> {
    let rows = sqlx::query(
        "SELECT signer, algorithm, public_key, is_active FROM mem_plugin_signers ORDER BY signer",
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

pub async fn publish_plugin_package(
    store: &SqlMemoryStore,
    package_dir: impl AsRef<Path>,
    published_by: &str,
) -> Result<PluginRepositoryEntry, MemoriaError> {
    publish_plugin_package_inner(store, package_dir, published_by, false).await
}

/// Dev-mode publish: skips signature verification and auto-approves the package.
pub async fn publish_plugin_package_dev(
    store: &SqlMemoryStore,
    package_dir: impl AsRef<Path>,
    published_by: &str,
) -> Result<PluginRepositoryEntry, MemoriaError> {
    publish_plugin_package_inner(store, package_dir, published_by, true).await
}

async fn publish_plugin_package_inner(
    store: &SqlMemoryStore,
    package_dir: impl AsRef<Path>,
    published_by: &str,
    dev_mode: bool,
) -> Result<PluginRepositoryEntry, MemoriaError> {
    let package_dir = package_dir.as_ref();
    let policy = if dev_mode {
        HostPluginPolicy::development()
    } else {
        repository_policy(store).await?
    };
    let package = load_plugin_package(package_dir.to_path_buf(), &policy)?;
    let payload = capture_package_payload(package_dir)?;
    let payload_json = serde_json::to_string(&payload)?;
    let manifest_json = serde_json::to_string(&package.manifest)?;
    let domain = domain_from_manifest(&package.manifest)?;
    let now = Utc::now().naive_utc();

    if let Some(existing) = sqlx::query(
        "SELECT sha256, signature, signer, status, published_at, published_by \
         FROM mem_plugin_packages WHERE plugin_key = ? AND version = ?",
    )
    .bind(&package.plugin_key)
    .bind(&package.manifest.version)
    .fetch_optional(store.pool())
    .await
    .map_err(db_err)?
    {
        let existing_sha: String = existing.try_get("sha256").map_err(db_err)?;
        let existing_signature: String = existing.try_get("signature").map_err(db_err)?;
        let existing_signer: String = existing.try_get("signer").map_err(db_err)?;
        if existing_sha != package.manifest.integrity.sha256
            || existing_signature != package.manifest.integrity.signature
            || existing_signer != package.manifest.integrity.signer
        {
            return Err(MemoriaError::Blocked(format!(
                "Plugin package {}@{} already exists with different content; releases are immutable",
                package.plugin_key, package.manifest.version
            )));
        }
        return build_repository_entry(store, &package.plugin_key, &package.manifest.version).await;
    }

    let initial_status = if dev_mode { "active" } else { "pending" };
    let initial_review = if dev_mode { "active" } else { "pending" };
    let review_notes = if dev_mode {
        "Auto-approved (dev mode)"
    } else {
        "Awaiting review"
    };

    sqlx::query(
        "INSERT INTO mem_plugin_packages \
             (plugin_key, version, domain, name, runtime, manifest_json, package_payload, sha256, signature, signer, status, published_at, published_by) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
    )
    .bind(&package.plugin_key)
    .bind(&package.manifest.version)
    .bind(&domain)
    .bind(&package.manifest.name)
    .bind(runtime_label(package.manifest.runtime))
    .bind(manifest_json)
    .bind(payload_json)
    .bind(&package.manifest.integrity.sha256)
    .bind(&package.manifest.integrity.signature)
    .bind(&package.manifest.integrity.signer)
    .bind(initial_status)
    .bind(now)
    .bind(published_by)
    .execute(store.pool())
    .await
    .map_err(db_err)?;

    sqlx::query(
        "INSERT INTO mem_plugin_reviews \
             (plugin_key, version, review_status, score, review_notes, reviewed_at, reviewed_by) \
         VALUES (?, ?, ?, 0, ?, ?, ?) \
         ON DUPLICATE KEY UPDATE review_status = VALUES(review_status), review_notes = VALUES(review_notes), reviewed_at = VALUES(reviewed_at), reviewed_by = VALUES(reviewed_by)"
    )
    .bind(&package.plugin_key)
    .bind(&package.manifest.version)
    .bind(initial_review)
    .bind(review_notes)
    .bind(now)
    .bind(published_by)
    .execute(store.pool())
    .await
    .map_err(db_err)?;

    record_plugin_audit_event(
        store,
        AuditEventInput {
            domain: Some(&domain),
            binding_key: None,
            subject_key: None,
            plugin_key: Some(&package.plugin_key),
            version: Some(&package.manifest.version),
            event_type: if dev_mode {
                "package.published_dev"
            } else {
                "package.published"
            },
            status: initial_status,
            message: format!(
                "Published plugin package {}@{}{}",
                package.plugin_key,
                package.manifest.version,
                if dev_mode {
                    " (dev mode, auto-approved)"
                } else {
                    " and submitted it for review"
                }
            ),
            actor: published_by,
        },
    )
    .await?;

    build_repository_entry(store, &package.plugin_key, &package.manifest.version).await
}

pub async fn review_plugin_package(
    store: &SqlMemoryStore,
    plugin_key: &str,
    version: &str,
    status: &str,
    notes: Option<&str>,
    actor: &str,
) -> Result<(), MemoriaError> {
    validate_review_status(status)?;
    ensure_package_exists(store, plugin_key, version).await?;
    let now = Utc::now().naive_utc();
    sqlx::query("UPDATE mem_plugin_packages SET status = ? WHERE plugin_key = ? AND version = ?")
        .bind(status)
        .bind(plugin_key)
        .bind(version)
        .execute(store.pool())
        .await
        .map_err(db_err)?;
    sqlx::query(
        "INSERT INTO mem_plugin_reviews \
             (plugin_key, version, review_status, score, review_notes, reviewed_at, reviewed_by) \
         VALUES (?, ?, ?, 0, ?, ?, ?) \
         ON DUPLICATE KEY UPDATE review_status = VALUES(review_status), review_notes = VALUES(review_notes), reviewed_at = VALUES(reviewed_at), reviewed_by = VALUES(reviewed_by)"
    )
    .bind(plugin_key)
    .bind(version)
    .bind(status)
    .bind(notes.unwrap_or(""))
    .bind(now)
    .bind(actor)
    .execute(store.pool())
    .await
    .map_err(db_err)?;
    record_plugin_audit_event(
        store,
        AuditEventInput {
            domain: Some("governance"),
            binding_key: None,
            subject_key: None,
            plugin_key: Some(plugin_key),
            version: Some(version),
            event_type: "package.reviewed",
            status,
            message: notes
                .map(str::to_string)
                .unwrap_or_else(|| format!("Plugin {plugin_key}@{version} moved to {status}")),
            actor,
        },
    )
    .await
}

pub async fn score_plugin_package(
    store: &SqlMemoryStore,
    plugin_key: &str,
    version: &str,
    score: f64,
    notes: Option<&str>,
    actor: &str,
) -> Result<(), MemoriaError> {
    ensure_package_exists(store, plugin_key, version).await?;
    let now = Utc::now().naive_utc();
    sqlx::query(
        "INSERT INTO mem_plugin_reviews \
             (plugin_key, version, review_status, score, review_notes, reviewed_at, reviewed_by) \
         VALUES (?, ?, 'pending', ?, ?, ?, ?) \
         ON DUPLICATE KEY UPDATE score = VALUES(score), review_notes = VALUES(review_notes), reviewed_at = VALUES(reviewed_at), reviewed_by = VALUES(reviewed_by)"
    )
    .bind(plugin_key)
    .bind(version)
    .bind(score)
    .bind(notes.unwrap_or(""))
    .bind(now)
    .bind(actor)
    .execute(store.pool())
    .await
    .map_err(db_err)?;
    record_plugin_audit_event(
        store,
        AuditEventInput {
            domain: Some("governance"),
            binding_key: None,
            subject_key: None,
            plugin_key: Some(plugin_key),
            version: Some(version),
            event_type: "package.scored",
            status: "success",
            message: format!("Set score for {plugin_key}@{version} to {score}"),
            actor,
        },
    )
    .await
}

#[derive(Debug, Clone)]
pub struct BindingRuleInput<'a> {
    pub domain: &'a str,
    pub binding_key: &'a str,
    pub subject_key: &'a str,
    pub priority: i64,
    pub plugin_key: &'a str,
    pub selector_kind: &'a str,
    pub selector_value: &'a str,
    pub rollout_percent: i64,
    pub transport_endpoint: Option<&'a str>,
    pub actor: &'a str,
}

pub async fn activate_plugin_binding(
    store: &SqlMemoryStore,
    domain: &str,
    binding_key: &str,
    plugin_key: &str,
    version: &str,
    updated_by: &str,
) -> Result<(), MemoriaError> {
    upsert_plugin_binding_rule(
        store,
        BindingRuleInput {
            domain,
            binding_key,
            subject_key: "*",
            priority: 100,
            plugin_key,
            selector_kind: "exact",
            selector_value: version,
            rollout_percent: 100,
            transport_endpoint: None,
            actor: updated_by,
        },
    )
    .await
}

pub async fn upsert_plugin_binding_rule(
    store: &SqlMemoryStore,
    input: BindingRuleInput<'_>,
) -> Result<(), MemoriaError> {
    validate_selector_kind(input.selector_kind)?;
    validate_rollout(input.rollout_percent)?;
    if input.selector_kind == "exact" {
        ensure_package_is_active(store, input.plugin_key, input.selector_value).await?;
    } else {
        ensure_semver_target_exists(store, input.plugin_key, input.selector_value).await?;
    }

    let now = Utc::now().naive_utc();
    let rule_id = sqlx::query_scalar::<_, String>(
        "SELECT rule_id FROM mem_plugin_binding_rules \
         WHERE domain = ? AND binding_key = ? AND subject_key = ? AND priority = ?",
    )
    .bind(input.domain)
    .bind(input.binding_key)
    .bind(input.subject_key)
    .bind(input.priority)
    .fetch_optional(store.pool())
    .await
    .map_err(db_err)?
    .unwrap_or_else(|| Uuid::new_v4().to_string());

    sqlx::query(
        "INSERT INTO mem_plugin_binding_rules \
             (rule_id, domain, binding_key, subject_key, priority, plugin_key, selector_kind, selector_value, rollout_percent, transport_endpoint, status, updated_at, updated_by) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'active', ?, ?) \
         ON DUPLICATE KEY UPDATE \
             plugin_key = VALUES(plugin_key), selector_kind = VALUES(selector_kind), selector_value = VALUES(selector_value), \
             rollout_percent = VALUES(rollout_percent), transport_endpoint = VALUES(transport_endpoint), status = 'active', updated_at = VALUES(updated_at), updated_by = VALUES(updated_by)"
    )
    .bind(&rule_id)
    .bind(input.domain)
    .bind(input.binding_key)
    .bind(input.subject_key)
    .bind(input.priority)
    .bind(input.plugin_key)
    .bind(input.selector_kind)
    .bind(input.selector_value)
    .bind(input.rollout_percent)
    .bind(input.transport_endpoint.unwrap_or(""))
    .bind(now)
    .bind(input.actor)
    .execute(store.pool())
    .await
    .map_err(db_err)?;

    if input.selector_kind == "exact" && input.subject_key == "*" && input.priority == 100 {
        sqlx::query(
            "INSERT INTO mem_plugin_bindings (domain, binding_key, plugin_key, version, updated_at, updated_by) \
             VALUES (?, ?, ?, ?, ?, ?) \
             ON DUPLICATE KEY UPDATE plugin_key = VALUES(plugin_key), version = VALUES(version), updated_at = VALUES(updated_at), updated_by = VALUES(updated_by)"
        )
        .bind(input.domain)
        .bind(input.binding_key)
        .bind(input.plugin_key)
        .bind(input.selector_value)
        .bind(now)
        .bind(input.actor)
        .execute(store.pool())
        .await
        .map_err(db_err)?;
    }

    record_plugin_audit_event(
        store,
        AuditEventInput {
            domain: Some(input.domain),
            binding_key: Some(input.binding_key),
            subject_key: Some(input.subject_key),
            plugin_key: Some(input.plugin_key),
            version: if input.selector_kind == "exact" {
                Some(input.selector_value)
            } else {
                None
            },
            event_type: "binding.rule_upserted",
            status: "success",
            message: format!(
                "Updated binding {} for subject {} using {} {}",
                input.binding_key, input.subject_key, input.selector_kind, input.selector_value
            ),
            actor: input.actor,
        },
    )
    .await
}

pub async fn list_plugin_repository_entries(
    store: &SqlMemoryStore,
    domain: Option<&str>,
) -> Result<Vec<PluginRepositoryEntry>, MemoriaError> {
    let rows = if let Some(domain) = domain {
        sqlx::query(
            "SELECT p.plugin_key, p.version, p.domain, p.name, p.runtime, p.signer, p.status, p.published_at, p.published_by, \
                    COALESCE(r.review_status, 'pending') AS review_status, COALESCE(r.score, 0) AS score \
             FROM mem_plugin_packages p \
             LEFT JOIN mem_plugin_reviews r ON r.plugin_key = p.plugin_key AND r.version = p.version \
             WHERE p.domain = ? ORDER BY p.published_at DESC"
        )
        .bind(domain)
        .fetch_all(store.pool())
        .await
    } else {
        sqlx::query(
            "SELECT p.plugin_key, p.version, p.domain, p.name, p.runtime, p.signer, p.status, p.published_at, p.published_by, \
                    COALESCE(r.review_status, 'pending') AS review_status, COALESCE(r.score, 0) AS score \
             FROM mem_plugin_packages p \
             LEFT JOIN mem_plugin_reviews r ON r.plugin_key = p.plugin_key AND r.version = p.version \
             ORDER BY p.published_at DESC"
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
                runtime: row.try_get("runtime").map_err(db_err)?,
                signer: row.try_get("signer").map_err(db_err)?,
                status: row.try_get("status").map_err(db_err)?,
                review_status: row.try_get("review_status").map_err(db_err)?,
                score: row.try_get("score").map_err(db_err)?,
                published_at: row.try_get("published_at").map_err(db_err)?,
                published_by: row.try_get("published_by").map_err(db_err)?,
            })
        })
        .collect()
}

pub async fn list_plugin_compatibility_matrix(
    store: &SqlMemoryStore,
    domain: Option<&str>,
) -> Result<Vec<PluginCompatibilityEntry>, MemoriaError> {
    let policy = repository_policy(store).await?;
    let entries = list_plugin_repository_entries(store, domain).await?;
    let mut matrix = Vec::with_capacity(entries.len());
    for entry in entries {
        let manifest_json: String = sqlx::query_scalar(
            "SELECT manifest_json FROM mem_plugin_packages WHERE plugin_key = ? AND version = ?",
        )
        .bind(&entry.plugin_key)
        .bind(&entry.version)
        .fetch_one(store.pool())
        .await
        .map_err(db_err)?;
        let manifest: PluginManifest = serde_json::from_str(&manifest_json)?;
        let (supported, reason) = compatibility_status(&policy, &manifest);
        matrix.push(PluginCompatibilityEntry {
            plugin_key: entry.plugin_key,
            version: entry.version,
            domain: entry.domain,
            runtime: entry.runtime,
            status: entry.status,
            review_status: entry.review_status,
            supported,
            reason,
            compatibility: manifest.compatibility.memoria,
        });
    }
    Ok(matrix)
}

pub async fn get_plugin_audit_events(
    store: &SqlMemoryStore,
    domain: Option<&str>,
    plugin_key: Option<&str>,
    binding_key: Option<&str>,
    limit: usize,
) -> Result<Vec<PluginAuditEvent>, MemoriaError> {
    let mut query = String::from(
        "SELECT event_type, status, message, plugin_key, version, binding_key, subject_key, actor, created_at \
         FROM mem_plugin_audit_events WHERE 1=1"
    );
    let mut binds: Vec<String> = Vec::new();
    if let Some(domain) = domain {
        query.push_str(" AND domain = ?");
        binds.push(domain.to_string());
    }
    if let Some(plugin_key) = plugin_key {
        query.push_str(" AND plugin_key = ?");
        binds.push(plugin_key.to_string());
    }
    if let Some(binding_key) = binding_key {
        query.push_str(" AND binding_key = ?");
        binds.push(binding_key.to_string());
    }
    query.push_str(" ORDER BY created_at DESC LIMIT ?");
    let mut q = sqlx::query(&query);
    for bind in &binds {
        q = q.bind(bind);
    }
    let rows = q
        .bind(limit as i64)
        .fetch_all(store.pool())
        .await
        .map_err(db_err)?;
    rows.into_iter()
        .map(|row| {
            Ok(PluginAuditEvent {
                event_type: row.try_get("event_type").map_err(db_err)?,
                status: row.try_get("status").map_err(db_err)?,
                message: row.try_get("message").map_err(db_err)?,
                plugin_key: row.try_get("plugin_key").ok(),
                version: row.try_get("version").ok(),
                binding_key: row.try_get("binding_key").ok(),
                subject_key: row.try_get("subject_key").ok(),
                actor: row.try_get("actor").map_err(db_err)?,
                created_at: row.try_get("created_at").map_err(db_err)?,
            })
        })
        .collect()
}

pub async fn list_binding_rules(
    store: &SqlMemoryStore,
    domain: &str,
    binding_key: &str,
) -> Result<Vec<PluginBindingRule>, MemoriaError> {
    let rows = sqlx::query(
        "SELECT rule_id, domain, binding_key, subject_key, priority, plugin_key, selector_kind, selector_value, rollout_percent, transport_endpoint, status, updated_at, updated_by \
         FROM mem_plugin_binding_rules WHERE domain = ? AND binding_key = ? ORDER BY subject_key, priority ASC, updated_at DESC"
    )
    .bind(domain)
    .bind(binding_key)
    .fetch_all(store.pool())
    .await
    .map_err(db_err)?;
    rows.into_iter().map(binding_rule_from_row).collect()
}

pub async fn load_active_governance_plugin(
    store: &SqlMemoryStore,
    binding_key: &str,
    subject_key: &str,
    delegate: Arc<dyn GovernanceStrategy>,
) -> Result<Option<ActiveGovernancePlugin>, MemoriaError> {
    let rules = load_binding_rules(store, "governance", binding_key, subject_key).await?;
    if rules.is_empty() {
        return load_legacy_binding(store, binding_key, delegate).await;
    }

    for rule in rules {
        if !rollout_matches(&rule, subject_key) {
            continue;
        }
        if let Some(package) = resolve_package_for_rule(store, &rule).await? {
            let strategy =
                build_governance_strategy(store, &package, &rule, delegate.clone()).await?;
            record_plugin_audit_event(
                store,
                AuditEventInput {
                    domain: Some("governance"),
                    binding_key: Some(binding_key),
                    subject_key: Some(subject_key),
                    plugin_key: Some(&package.plugin_key),
                    version: Some(&package.version),
                    event_type: "binding.loaded",
                    status: "success",
                    message: format!(
                        "Loaded governance plugin {}@{} via binding {}",
                        package.plugin_key, package.version, binding_key
                    ),
                    actor: "runtime",
                },
            )
            .await?;
            return Ok(Some(ActiveGovernancePlugin {
                binding_key: binding_key.into(),
                subject_key: subject_key.into(),
                plugin_key: package.plugin_key,
                version: package.version,
                strategy,
            }));
        }
    }

    record_plugin_audit_event(
        store,
        AuditEventInput {
            domain: Some("governance"),
            binding_key: Some(binding_key),
            subject_key: Some(subject_key),
            plugin_key: None,
            version: None,
            event_type: "binding.load_failed",
            status: "failed",
            message: format!(
                "Binding {} did not resolve any active compatible package for subject {}",
                binding_key, subject_key
            ),
            actor: "runtime",
        },
    )
    .await?;
    Err(MemoriaError::Blocked(format!(
        "Binding {binding_key} did not resolve any active compatible package"
    )))
}

#[allow(clippy::too_many_arguments)]
pub async fn record_runtime_plugin_event(
    store: &SqlMemoryStore,
    domain: &str,
    binding_key: Option<&str>,
    subject_key: Option<&str>,
    plugin_key: Option<&str>,
    version: Option<&str>,
    event_type: &str,
    status: &str,
    message: &str,
) -> Result<(), MemoriaError> {
    record_plugin_audit_event(
        store,
        AuditEventInput {
            domain: Some(domain),
            binding_key,
            subject_key,
            plugin_key,
            version,
            event_type,
            status,
            message: message.to_string(),
            actor: "scheduler",
        },
    )
    .await
}

async fn build_repository_entry(
    store: &SqlMemoryStore,
    plugin_key: &str,
    version: &str,
) -> Result<PluginRepositoryEntry, MemoriaError> {
    let row = sqlx::query(
        "SELECT p.plugin_key, p.version, p.domain, p.name, p.runtime, p.signer, p.status, p.published_at, p.published_by, \
                COALESCE(r.review_status, 'pending') AS review_status, COALESCE(r.score, 0) AS score \
         FROM mem_plugin_packages p \
         LEFT JOIN mem_plugin_reviews r ON r.plugin_key = p.plugin_key AND r.version = p.version \
         WHERE p.plugin_key = ? AND p.version = ?"
    )
    .bind(plugin_key)
    .bind(version)
    .fetch_one(store.pool())
    .await
    .map_err(db_err)?;
    Ok(PluginRepositoryEntry {
        plugin_key: row.try_get("plugin_key").map_err(db_err)?,
        version: row.try_get("version").map_err(db_err)?,
        domain: row.try_get("domain").map_err(db_err)?,
        name: row.try_get("name").map_err(db_err)?,
        runtime: row.try_get("runtime").map_err(db_err)?,
        signer: row.try_get("signer").map_err(db_err)?,
        status: row.try_get("status").map_err(db_err)?,
        review_status: row.try_get("review_status").map_err(db_err)?,
        score: row.try_get("score").map_err(db_err)?,
        published_at: row.try_get("published_at").map_err(db_err)?,
        published_by: row.try_get("published_by").map_err(db_err)?,
    })
}

async fn load_legacy_binding(
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
    let package = load_package_record(store, &plugin_key, &version)
        .await?
        .ok_or_else(|| {
            MemoriaError::Blocked(format!(
                "Legacy binding {binding_key} points to missing package {plugin_key}@{version}"
            ))
        })?;
    let strategy = build_governance_strategy(
        store,
        &package,
        &PluginBindingRule {
            rule_id: "legacy".into(),
            domain: "governance".into(),
            binding_key: binding_key.into(),
            subject_key: "*".into(),
            priority: 100,
            plugin_key: plugin_key.clone(),
            selector_kind: "exact".into(),
            selector_value: version.clone(),
            rollout_percent: 100,
            transport_endpoint: None,
            status: "active".into(),
            updated_at: Utc::now().naive_utc(),
            updated_by: "legacy".into(),
        },
        delegate,
    )
    .await?;
    Ok(Some(ActiveGovernancePlugin {
        binding_key: binding_key.into(),
        subject_key: "system".into(),
        plugin_key,
        version,
        strategy,
    }))
}

async fn load_binding_rules(
    store: &SqlMemoryStore,
    domain: &str,
    binding_key: &str,
    subject_key: &str,
) -> Result<Vec<PluginBindingRule>, MemoriaError> {
    let rows = sqlx::query(
        "SELECT rule_id, domain, binding_key, subject_key, priority, plugin_key, selector_kind, selector_value, rollout_percent, transport_endpoint, status, updated_at, updated_by \
         FROM mem_plugin_binding_rules \
         WHERE domain = ? AND binding_key = ? AND status = 'active' AND (subject_key = ? OR subject_key = '*') \
         ORDER BY CASE WHEN subject_key = ? THEN 0 ELSE 1 END, priority ASC, updated_at DESC"
    )
    .bind(domain)
    .bind(binding_key)
    .bind(subject_key)
    .bind(subject_key)
    .fetch_all(store.pool())
    .await
    .map_err(db_err)?;
    rows.into_iter().map(binding_rule_from_row).collect()
}

fn binding_rule_from_row(row: sqlx::mysql::MySqlRow) -> Result<PluginBindingRule, MemoriaError> {
    let endpoint: String = row.try_get("transport_endpoint").map_err(db_err)?;
    Ok(PluginBindingRule {
        rule_id: row.try_get("rule_id").map_err(db_err)?,
        domain: row.try_get("domain").map_err(db_err)?,
        binding_key: row.try_get("binding_key").map_err(db_err)?,
        subject_key: row.try_get("subject_key").map_err(db_err)?,
        priority: row.try_get("priority").map_err(db_err)?,
        plugin_key: row.try_get("plugin_key").map_err(db_err)?,
        selector_kind: row.try_get("selector_kind").map_err(db_err)?,
        selector_value: row.try_get("selector_value").map_err(db_err)?,
        rollout_percent: row.try_get("rollout_percent").map_err(db_err)?,
        transport_endpoint: if endpoint.trim().is_empty() {
            None
        } else {
            Some(endpoint)
        },
        status: row.try_get("status").map_err(db_err)?,
        updated_at: row.try_get("updated_at").map_err(db_err)?,
        updated_by: row.try_get("updated_by").map_err(db_err)?,
    })
}

fn rollout_matches(rule: &PluginBindingRule, subject_key: &str) -> bool {
    if rule.rollout_percent >= 100 || subject_key.is_empty() {
        return true;
    }
    let mut hasher = DefaultHasher::new();
    rule.binding_key.hash(&mut hasher);
    rule.plugin_key.hash(&mut hasher);
    subject_key.hash(&mut hasher);
    (hasher.finish() % 100) < rule.rollout_percent as u64
}

async fn resolve_package_for_rule(
    store: &SqlMemoryStore,
    rule: &PluginBindingRule,
) -> Result<Option<PackageRecord>, MemoriaError> {
    match rule.selector_kind.as_str() {
        "exact" => load_package_record(store, &rule.plugin_key, &rule.selector_value).await,
        "semver" => {
            resolve_highest_matching_package(store, &rule.plugin_key, &rule.selector_value).await
        }
        other => Err(MemoriaError::Blocked(format!(
            "Unsupported selector kind `{other}`"
        ))),
    }
}

async fn load_package_record(
    store: &SqlMemoryStore,
    plugin_key: &str,
    version: &str,
) -> Result<Option<PackageRecord>, MemoriaError> {
    let row = sqlx::query(
        "SELECT version, runtime, manifest_json, package_payload \
         FROM mem_plugin_packages WHERE plugin_key = ? AND version = ? AND status = 'active'",
    )
    .bind(plugin_key)
    .bind(version)
    .fetch_optional(store.pool())
    .await
    .map_err(db_err)?;
    row.map(|row| package_record_from_row(plugin_key, row))
        .transpose()
}

async fn resolve_highest_matching_package(
    store: &SqlMemoryStore,
    plugin_key: &str,
    version_req: &str,
) -> Result<Option<PackageRecord>, MemoriaError> {
    let req = VersionReq::parse(version_req).map_err(|err| {
        MemoriaError::Blocked(format!("Invalid semver selector `{version_req}`: {err}"))
    })?;
    let rows = sqlx::query(
        "SELECT version, runtime, manifest_json, package_payload \
         FROM mem_plugin_packages WHERE plugin_key = ? AND status = 'active'",
    )
    .bind(plugin_key)
    .fetch_all(store.pool())
    .await
    .map_err(db_err)?;
    let mut candidates = Vec::new();
    for row in rows {
        let version: String = row.try_get("version").map_err(db_err)?;
        let parsed = Version::parse(&version).map_err(|err| {
            MemoriaError::Blocked(format!("Invalid stored plugin version `{version}`: {err}"))
        })?;
        if req.matches(&parsed) {
            candidates.push((parsed, package_record_from_row(plugin_key, row)?));
        }
    }
    candidates.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(candidates.pop().map(|(_, record)| record))
}

fn package_record_from_row(
    plugin_key: &str,
    row: sqlx::mysql::MySqlRow,
) -> Result<PackageRecord, MemoriaError> {
    let version: String = row.try_get("version").map_err(db_err)?;
    let runtime: String = row.try_get("runtime").map_err(db_err)?;
    let manifest_json: String = row.try_get("manifest_json").map_err(db_err)?;
    let payload_json: String = row.try_get("package_payload").map_err(db_err)?;
    let manifest: PluginManifest = serde_json::from_str(&manifest_json)?;
    let payload: Vec<StoredPluginFile> = serde_json::from_str(&payload_json)?;
    Ok(PackageRecord {
        plugin_key: plugin_key.into(),
        version,
        runtime: parse_runtime_kind(&runtime)?,
        manifest,
        payload,
    })
}

/// Build a governance strategy from a locally loaded plugin package (dev mode).
pub fn build_local_governance_strategy(
    package: &PluginPackage,
    delegate: Arc<dyn GovernanceStrategy>,
) -> Result<Arc<dyn GovernanceStrategy>, MemoriaError> {
    match package.manifest.runtime {
        PluginRuntimeKind::Rhai => {
            let script_source = fs::read_to_string(&package.script_path).map_err(|err| {
                MemoriaError::Blocked(format!(
                    "Failed to read local Rhai script {}: {err}",
                    package.script_path.display()
                ))
            })?;
            Ok(Arc::new(RhaiGovernanceStrategy::from_loaded_package(
                package.clone(),
                script_source,
                delegate,
            )?))
        }
        PluginRuntimeKind::Grpc => Err(MemoriaError::Blocked(
            "Local gRPC plugins not supported; use --endpoint with shared binding instead".into(),
        )),
    }
}

async fn build_governance_strategy(
    store: &SqlMemoryStore,
    package: &PackageRecord,
    rule: &PluginBindingRule,
    delegate: Arc<dyn GovernanceStrategy>,
) -> Result<Arc<dyn GovernanceStrategy>, MemoriaError> {
    let temp = materialize_payload(&package.payload)?;
    let policy = repository_policy(store).await?;
    let mut loaded = load_plugin_package(temp.path().to_path_buf(), &policy)?;
    match package.runtime {
        PluginRuntimeKind::Rhai => {
            let script_source = fs::read_to_string(&loaded.script_path).map_err(|err| {
                MemoriaError::Blocked(format!(
                    "Failed to read repository-backed Rhai script {}: {err}",
                    loaded.script_path.display()
                ))
            })?;
            let virtual_root = PathBuf::from(format!(
                "plugin-repo://{}/{}",
                package.plugin_key, package.version
            ));
            loaded.root_dir = virtual_root.clone();
            loaded.script_path = virtual_root.join(
                package
                    .manifest
                    .entry
                    .rhai
                    .as_ref()
                    .map(|entry| entry.script.as_str())
                    .unwrap_or("plugin.rhai"),
            );
            Ok(Arc::new(RhaiGovernanceStrategy::from_loaded_package(
                loaded,
                script_source,
                delegate,
            )?))
        }
        PluginRuntimeKind::Grpc => {
            let endpoint = rule.transport_endpoint.as_ref().ok_or_else(|| {
                MemoriaError::Blocked(format!(
                    "Binding {} requires a transport endpoint for gRPC plugin {}",
                    rule.binding_key, package.plugin_key
                ))
            })?;
            Ok(Arc::new(
                GrpcGovernanceStrategy::connect(loaded, endpoint.clone(), delegate).await?,
            ))
        }
    }
}

async fn repository_policy(store: &SqlMemoryStore) -> Result<HostPluginPolicy, MemoriaError> {
    let rows = sqlx::query("SELECT signer, public_key FROM mem_plugin_signers WHERE is_active > 0")
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

fn compatibility_status(policy: &HostPluginPolicy, manifest: &PluginManifest) -> (bool, String) {
    if !policy.supported_runtimes.contains(&manifest.runtime) {
        return (
            false,
            format!(
                "runtime {:?} is not supported by this host",
                manifest.runtime
            ),
        );
    }
    let req = match VersionReq::parse(&manifest.compatibility.memoria) {
        Ok(req) => req,
        Err(err) => {
            return (false, format!("invalid compatibility range: {err}"));
        }
    };
    if !req.matches(&policy.current_memoria_version) {
        return (
            false,
            format!(
                "host version {} is outside {}",
                policy.current_memoria_version, manifest.compatibility.memoria
            ),
        );
    }
    (true, "compatible".into())
}

fn runtime_label(kind: PluginRuntimeKind) -> &'static str {
    match kind {
        PluginRuntimeKind::Rhai => "rhai",
        PluginRuntimeKind::Grpc => "grpc",
    }
}

fn parse_runtime_kind(value: &str) -> Result<PluginRuntimeKind, MemoriaError> {
    match value {
        "rhai" => Ok(PluginRuntimeKind::Rhai),
        "grpc" => Ok(PluginRuntimeKind::Grpc),
        other => Err(MemoriaError::Blocked(format!(
            "Unsupported stored runtime `{other}`"
        ))),
    }
}

fn domain_from_manifest(manifest: &PluginManifest) -> Result<String, MemoriaError> {
    manifest
        .capabilities
        .first()
        .and_then(|cap| cap.split_once('.'))
        .map(|(domain, _)| domain.to_string())
        .ok_or_else(|| MemoriaError::Blocked("Plugin must declare at least one capability".into()))
}

fn validate_review_status(status: &str) -> Result<(), MemoriaError> {
    match status {
        "pending" | "active" | "rejected" | "disabled" | "taken_down" => Ok(()),
        other => Err(MemoriaError::Blocked(format!(
            "Unsupported review status `{other}`"
        ))),
    }
}

fn validate_selector_kind(kind: &str) -> Result<(), MemoriaError> {
    match kind {
        "exact" | "semver" => Ok(()),
        other => Err(MemoriaError::Blocked(format!(
            "Unsupported selector kind `{other}`"
        ))),
    }
}

fn validate_rollout(rollout_percent: i64) -> Result<(), MemoriaError> {
    if (0..=100).contains(&rollout_percent) && rollout_percent > 0 {
        Ok(())
    } else {
        Err(MemoriaError::Blocked(format!(
            "Rollout percent must be between 1 and 100, got {rollout_percent}"
        )))
    }
}

async fn ensure_package_exists(
    store: &SqlMemoryStore,
    plugin_key: &str,
    version: &str,
) -> Result<(), MemoriaError> {
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM mem_plugin_packages WHERE plugin_key = ? AND version = ?",
    )
    .bind(plugin_key)
    .bind(version)
    .fetch_one(store.pool())
    .await
    .map_err(db_err)?;
    if count == 0 {
        return Err(MemoriaError::Blocked(format!(
            "Plugin package {plugin_key}@{version} does not exist"
        )));
    }
    Ok(())
}

async fn ensure_package_is_active(
    store: &SqlMemoryStore,
    plugin_key: &str,
    version: &str,
) -> Result<(), MemoriaError> {
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM mem_plugin_packages WHERE plugin_key = ? AND version = ? AND status = 'active'"
    )
    .bind(plugin_key)
    .bind(version)
    .fetch_one(store.pool())
    .await
    .map_err(db_err)?;
    if count == 0 {
        return Err(MemoriaError::Blocked(format!(
            "Plugin package {plugin_key}@{version} is not active"
        )));
    }
    Ok(())
}

async fn ensure_semver_target_exists(
    store: &SqlMemoryStore,
    plugin_key: &str,
    version_req: &str,
) -> Result<(), MemoriaError> {
    let _ = VersionReq::parse(version_req).map_err(|err| {
        MemoriaError::Blocked(format!("Invalid semver selector `{version_req}`: {err}"))
    })?;
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM mem_plugin_packages WHERE plugin_key = ? AND status = 'active'",
    )
    .bind(plugin_key)
    .fetch_one(store.pool())
    .await
    .map_err(db_err)?;
    if count == 0 {
        return Err(MemoriaError::Blocked(format!(
            "No active plugin package found for {plugin_key}"
        )));
    }
    Ok(())
}

struct AuditEventInput<'a> {
    domain: Option<&'a str>,
    binding_key: Option<&'a str>,
    subject_key: Option<&'a str>,
    plugin_key: Option<&'a str>,
    version: Option<&'a str>,
    event_type: &'a str,
    status: &'a str,
    message: String,
    actor: &'a str,
}

async fn record_plugin_audit_event(
    store: &SqlMemoryStore,
    input: AuditEventInput<'_>,
) -> Result<(), MemoriaError> {
    let now = Utc::now().naive_utc();
    sqlx::query(
        "INSERT INTO mem_plugin_audit_events \
             (event_id, domain, binding_key, subject_key, plugin_key, version, event_type, status, message, metadata_json, created_at, actor) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, '{}', ?, ?)"
    )
    .bind(Uuid::new_v4().to_string())
    .bind(input.domain.unwrap_or(""))
    .bind(input.binding_key.unwrap_or(""))
    .bind(input.subject_key.unwrap_or(""))
    .bind(input.plugin_key.unwrap_or(""))
    .bind(input.version.unwrap_or(""))
    .bind(input.event_type)
    .bind(input.status)
    .bind(input.message)
    .bind(now)
    .bind(input.actor)
    .execute(store.pool())
    .await
    .map_err(db_err)?;
    Ok(())
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
            .map_err(|err| {
                MemoriaError::Blocked(format!("Invalid package payload encoding: {err}"))
            })?;
        fs::write(&path, bytes).map_err(|err| {
            MemoriaError::Internal(format!(
                "Failed to materialize plugin repository file {}: {err}",
                path.display()
            ))
        })?;
    }
    Ok(dir)
}
