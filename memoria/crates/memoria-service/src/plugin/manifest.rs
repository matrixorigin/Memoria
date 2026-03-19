use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use memoria_core::MemoriaError;
use semver::Version;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum PluginRuntimeKind {
    Rhai,
    Grpc,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginEntrypoint {
    pub rhai: Option<RhaiEntry>,
    pub grpc: Option<GrpcEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RhaiEntry {
    pub script: String,
    pub entrypoint: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GrpcEntry {
    pub service: String,
    pub protocol: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginCompatibility {
    pub memoria: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginPermissions {
    pub network: bool,
    pub filesystem: bool,
    #[serde(default)]
    pub env: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginLimits {
    pub timeout_ms: u64,
    pub max_memory_mb: u64,
    pub max_output_bytes: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginIntegrity {
    pub sha256: String,
    pub signature: String,
    pub signer: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct PluginMetadata {
    pub display_name: Option<String>,
    pub description: Option<String>,
    pub homepage: Option<String>,
    pub license: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginManifest {
    pub name: String,
    pub version: String,
    pub api_version: String,
    pub runtime: PluginRuntimeKind,
    pub entry: PluginEntrypoint,
    pub capabilities: Vec<String>,
    pub compatibility: PluginCompatibility,
    pub permissions: PluginPermissions,
    pub limits: PluginLimits,
    pub integrity: PluginIntegrity,
    #[serde(default)]
    pub metadata: PluginMetadata,
}

impl PluginManifest {
    pub fn has_capability(&self, capability: &str) -> bool {
        self.capabilities.iter().any(|item| item == capability)
    }

    pub fn plugin_key(&self) -> Result<String, MemoriaError> {
        let version = Version::parse(&self.version)
            .map_err(|err| MemoriaError::Blocked(format!("Invalid plugin version: {err}")))?;
        let domain = self
            .capabilities
            .first()
            .and_then(|cap| cap.split_once('.'))
            .map(|(domain, _)| domain)
            .ok_or_else(|| {
                MemoriaError::Blocked("Plugin must declare at least one capability".into())
            })?;

        let short_name = self
            .name
            .strip_prefix("memoria-")
            .unwrap_or(&self.name)
            .strip_prefix(&format!("{domain}-"))
            .unwrap_or(self.name.strip_prefix("memoria-").unwrap_or(&self.name));

        Ok(format!("{domain}:{short_name}:v{}", version.major))
    }
}

#[derive(Debug, Clone)]
pub struct PluginPackage {
    pub root_dir: PathBuf,
    pub manifest: PluginManifest,
    pub plugin_key: String,
    pub script_path: PathBuf,
    pub entrypoint: String,
}

#[derive(Debug, Clone)]
pub struct HostPluginPolicy {
    pub current_memoria_version: Version,
    pub supported_runtimes: BTreeSet<PluginRuntimeKind>,
    pub allowed_capabilities: BTreeSet<String>,
    pub allow_network: bool,
    pub allow_filesystem: bool,
    pub allowed_env: BTreeSet<String>,
    pub max_timeout_ms: u64,
    pub max_memory_mb: u64,
    pub max_output_bytes: usize,
    pub enforce_signatures: bool,
    pub trusted_signers: BTreeSet<String>,
    pub signer_public_keys: BTreeMap<String, String>,
}

impl HostPluginPolicy {
    pub fn development() -> Self {
        Self {
            current_memoria_version: Version::parse(env!("CARGO_PKG_VERSION"))
                .unwrap_or_else(|_| Version::new(0, 1, 0)),
            supported_runtimes: [PluginRuntimeKind::Rhai, PluginRuntimeKind::Grpc]
                .into_iter()
                .collect(),
            allowed_capabilities: [
                "retrieval.rerank",
                "retrieval.query_rewrite",
                "governance.plan",
                "governance.execute",
                "consolidation.merge",
                "trust.evaluate",
            ]
            .into_iter()
            .map(str::to_string)
            .collect(),
            allow_network: false,
            allow_filesystem: false,
            allowed_env: ["MEMORIA_PLUGIN_LOG_LEVEL"]
                .into_iter()
                .map(str::to_string)
                .collect(),
            max_timeout_ms: 1_500,
            max_memory_mb: 128,
            max_output_bytes: 256 * 1024,
            enforce_signatures: false,
            trusted_signers: BTreeSet::new(),
            signer_public_keys: BTreeMap::new(),
        }
    }
}

impl Default for HostPluginPolicy {
    fn default() -> Self {
        Self::development()
    }
}

pub fn load_plugin_package(
    root_dir: impl Into<PathBuf>,
    policy: &HostPluginPolicy,
) -> Result<PluginPackage, MemoriaError> {
    let root_dir = root_dir.into();
    let manifest_path = root_dir.join("manifest.json");
    let manifest_bytes = fs::read(&manifest_path).map_err(|err| {
        MemoriaError::Blocked(format!(
            "Failed to read plugin manifest {}: {err}",
            manifest_path.display()
        ))
    })?;
    let manifest: PluginManifest = serde_json::from_slice(&manifest_bytes)?;

    validate_manifest(&manifest, &root_dir, policy)?;

    let actual_sha256 = compute_package_sha256(&root_dir)?;
    if manifest.integrity.sha256 != actual_sha256 {
        return Err(MemoriaError::Blocked(format!(
            "Plugin integrity mismatch: expected {}, got {}",
            manifest.integrity.sha256, actual_sha256
        )));
    }

    if policy.enforce_signatures {
        if !policy.trusted_signers.contains(&manifest.integrity.signer) {
            return Err(MemoriaError::Blocked(format!(
                "Untrusted plugin signer: {}",
                manifest.integrity.signer
            )));
        }
        verify_manifest_signature(&manifest, policy)?;
    }

    let plugin_key = manifest.plugin_key()?;
    let (script_path, entrypoint) = match manifest.runtime {
        PluginRuntimeKind::Rhai => {
            let rhai = manifest.entry.rhai.clone().ok_or_else(|| {
                MemoriaError::Blocked("Rhai runtime selected but `entry.rhai` is missing".into())
            })?;
            let script_path = root_dir.join(&rhai.script);
            if !script_path.is_file() {
                return Err(MemoriaError::Blocked(format!(
                    "Rhai script not found: {}",
                    script_path.display()
                )));
            }
            (script_path, rhai.entrypoint)
        }
        PluginRuntimeKind::Grpc => {
            let grpc = manifest.entry.grpc.clone().ok_or_else(|| {
                MemoriaError::Blocked("gRPC runtime selected but `entry.grpc` is missing".into())
            })?;
            (root_dir.join("manifest.json"), grpc.service)
        }
    };
    Ok(PluginPackage {
        root_dir,
        manifest,
        plugin_key,
        script_path,
        entrypoint,
    })
}

fn validate_manifest(
    manifest: &PluginManifest,
    root_dir: &Path,
    policy: &HostPluginPolicy,
) -> Result<(), MemoriaError> {
    validate_name(&manifest.name)?;
    Version::parse(&manifest.version)
        .map_err(|err| MemoriaError::Blocked(format!("Invalid plugin version: {err}")))?;

    if manifest.api_version != "v1" {
        return Err(MemoriaError::Blocked(format!(
            "Unsupported plugin api_version: {}",
            manifest.api_version
        )));
    }

    if !policy.supported_runtimes.contains(&manifest.runtime) {
        return Err(MemoriaError::Blocked(format!(
            "Unsupported plugin runtime: {:?}",
            manifest.runtime
        )));
    }

    match manifest.runtime {
        PluginRuntimeKind::Rhai => {
            if manifest.entry.rhai.is_none() || manifest.entry.grpc.is_some() {
                return Err(MemoriaError::Blocked(
                    "Rhai plugin entry must define `entry.rhai` only".into(),
                ));
            }
        }
        PluginRuntimeKind::Grpc => {
            let grpc = manifest.entry.grpc.as_ref().ok_or_else(|| {
                MemoriaError::Blocked("gRPC plugin entry must define `entry.grpc`".into())
            })?;
            if manifest.entry.rhai.is_some() {
                return Err(MemoriaError::Blocked(
                    "gRPC plugin entry must define `entry.grpc` only".into(),
                ));
            }
            if grpc.protocol != "grpc" {
                return Err(MemoriaError::Blocked(format!(
                    "Unsupported gRPC protocol `{}`",
                    grpc.protocol
                )));
            }
            if grpc.service.trim().is_empty() {
                return Err(MemoriaError::Blocked(
                    "gRPC plugin entry service must not be empty".into(),
                ));
            }
        }
    }

    if manifest.capabilities.is_empty() {
        return Err(MemoriaError::Blocked(
            "Plugin must declare at least one capability".into(),
        ));
    }
    for capability in &manifest.capabilities {
        validate_capability(capability)?;
        if !policy.allowed_capabilities.contains(capability) {
            return Err(MemoriaError::Blocked(format!(
                "Capability `{capability}` is not allowed by host policy"
            )));
        }
    }

    if manifest.permissions.network && !policy.allow_network {
        return Err(MemoriaError::Blocked(
            "Plugin requested network access but host policy disallows it".into(),
        ));
    }
    if manifest.permissions.filesystem && !policy.allow_filesystem {
        return Err(MemoriaError::Blocked(
            "Plugin requested filesystem access but host policy disallows it".into(),
        ));
    }
    if let Some(env_name) = manifest
        .permissions
        .env
        .iter()
        .find(|name| !policy.allowed_env.contains(*name))
    {
        return Err(MemoriaError::Blocked(format!(
            "Plugin requested env permission `{env_name}` outside host policy"
        )));
    }

    if manifest.limits.timeout_ms == 0
        || manifest.limits.max_memory_mb == 0
        || manifest.limits.max_output_bytes == 0
    {
        return Err(MemoriaError::Blocked(
            "Plugin limits must all be positive".into(),
        ));
    }
    if manifest.limits.timeout_ms > policy.max_timeout_ms {
        return Err(MemoriaError::Blocked(format!(
            "Plugin timeout {}ms exceeds host limit {}ms",
            manifest.limits.timeout_ms, policy.max_timeout_ms
        )));
    }
    if manifest.limits.max_memory_mb > policy.max_memory_mb {
        return Err(MemoriaError::Blocked(format!(
            "Plugin memory limit {}MiB exceeds host limit {}MiB",
            manifest.limits.max_memory_mb, policy.max_memory_mb
        )));
    }
    if manifest.limits.max_output_bytes > policy.max_output_bytes {
        return Err(MemoriaError::Blocked(format!(
            "Plugin output limit {} exceeds host limit {}",
            manifest.limits.max_output_bytes, policy.max_output_bytes
        )));
    }

    if manifest.runtime == PluginRuntimeKind::Rhai {
        let script_path = root_dir.join(
            manifest
                .entry
                .rhai
                .as_ref()
                .map(|entry| entry.script.as_str())
                .unwrap_or_default(),
        );
        if !script_path.exists() {
            return Err(MemoriaError::Blocked(format!(
                "Plugin entry script does not exist: {}",
                script_path.display()
            )));
        }
    }

    Ok(())
}

fn validate_name(name: &str) -> Result<(), MemoriaError> {
    let valid = !name.is_empty()
        && (3..=128).contains(&name.len())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if valid {
        Ok(())
    } else {
        Err(MemoriaError::Blocked(format!(
            "Invalid plugin name `{name}`; expected [a-z0-9-]{{3,128}}"
        )))
    }
}

fn validate_capability(capability: &str) -> Result<(), MemoriaError> {
    let (domain, action) = capability.split_once('.').ok_or_else(|| {
        MemoriaError::Blocked(format!(
            "Invalid capability `{capability}`; expected <domain>.<action>"
        ))
    })?;
    let valid_domain = matches!(
        domain,
        "retrieval" | "governance" | "consolidation" | "trust"
    );
    let valid_action =
        !action.is_empty() && action.chars().all(|c| c.is_ascii_lowercase() || c == '_');
    if valid_domain && valid_action {
        Ok(())
    } else {
        Err(MemoriaError::Blocked(format!(
            "Invalid capability `{capability}`"
        )))
    }
}

pub fn verify_manifest_signature(
    manifest: &PluginManifest,
    policy: &HostPluginPolicy,
) -> Result<(), MemoriaError> {
    let Some(public_key) = policy.signer_public_keys.get(&manifest.integrity.signer) else {
        return Err(MemoriaError::Blocked(format!(
            "Missing trusted public key for signer: {}",
            manifest.integrity.signer
        )));
    };

    let public_key_bytes = BASE64_STANDARD.decode(public_key).map_err(|err| {
        MemoriaError::Blocked(format!("Invalid signer public key encoding: {err}"))
    })?;
    let public_key_bytes: [u8; 32] = public_key_bytes.try_into().map_err(|_| {
        MemoriaError::Blocked(
            "Signer public key must be a base64-encoded 32-byte Ed25519 key".into(),
        )
    })?;
    let verifying_key = VerifyingKey::from_bytes(&public_key_bytes)
        .map_err(|err| MemoriaError::Blocked(format!("Invalid Ed25519 public key: {err}")))?;

    let signature_bytes = BASE64_STANDARD
        .decode(&manifest.integrity.signature)
        .map_err(|err| MemoriaError::Blocked(format!("Invalid signature encoding: {err}")))?;
    let signature = Signature::from_slice(&signature_bytes)
        .map_err(|err| MemoriaError::Blocked(format!("Invalid Ed25519 signature: {err}")))?;

    verifying_key
        .verify(manifest.integrity.sha256.as_bytes(), &signature)
        .map_err(|err| {
            MemoriaError::Blocked(format!("Plugin signature verification failed: {err}"))
        })
}

pub fn compute_package_sha256(root_dir: &Path) -> Result<String, MemoriaError> {
    let mut files = Vec::new();
    collect_files(root_dir, root_dir, &mut files)?;
    files.sort();

    let mut package_hasher = Sha256::new();
    for relative in files {
        let file_path = root_dir.join(&relative);
        let bytes = if relative == Path::new("manifest.json") {
            normalized_manifest_bytes(&file_path)?
        } else {
            fs::read(&file_path).map_err(|err| {
                MemoriaError::Blocked(format!(
                    "Failed to read plugin file {}: {err}",
                    file_path.display()
                ))
            })?
        };
        let file_hash = Sha256::digest(&bytes);
        package_hasher.update(relative.to_string_lossy().as_bytes());
        package_hasher.update([0]);
        package_hasher.update(file_hash);
    }

    Ok(hex_encode(&package_hasher.finalize()))
}

fn collect_files(
    root_dir: &Path,
    current: &Path,
    files: &mut Vec<PathBuf>,
) -> Result<(), MemoriaError> {
    for entry in fs::read_dir(current).map_err(|err| {
        MemoriaError::Blocked(format!(
            "Failed to read plugin directory {}: {err}",
            current.display()
        ))
    })? {
        let entry = entry.map_err(|err| {
            MemoriaError::Blocked(format!(
                "Failed to enumerate plugin directory {}: {err}",
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
            files.push(relative.to_path_buf());
        }
    }
    Ok(())
}

fn normalized_manifest_bytes(path: &Path) -> Result<Vec<u8>, MemoriaError> {
    let bytes = fs::read(path).map_err(|err| {
        MemoriaError::Blocked(format!("Failed to read manifest {}: {err}", path.display()))
    })?;
    let mut value: Value = serde_json::from_slice(&bytes)?;
    let object = value
        .as_object_mut()
        .ok_or_else(|| MemoriaError::Blocked("Manifest root must be a JSON object".into()))?;
    object.remove("integrity");
    serde_json::to_vec(&sort_json_value(&value)).map_err(Into::into)
}

fn sort_json_value(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let sorted: BTreeMap<String, Value> = map
                .iter()
                .map(|(key, value)| (key.clone(), sort_json_value(value)))
                .collect();
            serde_json::to_value(sorted).unwrap_or(Value::Null)
        }
        Value::Array(items) => Value::Array(items.iter().map(sort_json_value).collect()),
        _ => value.clone(),
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
    use ed25519_dalek::{Signer, SigningKey};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_plugin_dir(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("memoria-plugin-{name}-{nonce}"))
    }

    fn write_test_package(
        dir: &Path,
        manifest_name: &str,
        capability: &str,
        script_body: &str,
    ) -> PluginManifest {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join("plugin.rhai"), script_body).unwrap();
        let mut manifest = PluginManifest {
            name: manifest_name.into(),
            version: "1.2.0".into(),
            api_version: "v1".into(),
            runtime: PluginRuntimeKind::Rhai,
            entry: PluginEntrypoint {
                rhai: Some(RhaiEntry {
                    script: "plugin.rhai".into(),
                    entrypoint: "memoria_plugin".into(),
                }),
                grpc: None,
            },
            capabilities: vec![capability.into()],
            compatibility: PluginCompatibility {
                memoria: ">=0.1.0-rc1 <0.2.0".into(),
            },
            permissions: PluginPermissions {
                network: false,
                filesystem: false,
                env: vec![],
            },
            limits: PluginLimits {
                timeout_ms: 800,
                max_memory_mb: 64,
                max_output_bytes: 16_384,
            },
            integrity: PluginIntegrity {
                sha256: String::new(),
                signature: "dev-signature".into(),
                signer: "dev-signer".into(),
            },
            metadata: PluginMetadata::default(),
        };
        let manifest_path = dir.join("manifest.json");
        fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();
        manifest.integrity.sha256 = compute_package_sha256(dir).unwrap();
        fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();
        manifest
    }

    #[test]
    fn loads_valid_rhai_package() {
        let dir = temp_plugin_dir("valid");
        write_test_package(
            &dir,
            "memoria-governance-stale-archive",
            "governance.plan",
            "fn memoria_plugin(ctx) { #{ requires_approval: false } }",
        );

        let package = load_plugin_package(&dir, &HostPluginPolicy::development()).unwrap();
        assert_eq!(package.plugin_key, "governance:stale-archive:v1");

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn rejects_invalid_capability() {
        let dir = temp_plugin_dir("invalid-capability");
        write_test_package(
            &dir,
            "memoria-governance-invalid",
            "governance.plan-now",
            "fn memoria_plugin(ctx) { #{} }",
        );

        let err = load_plugin_package(&dir, &HostPluginPolicy::development()).unwrap_err();
        assert!(err.to_string().contains("Invalid capability"));

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn accepts_supported_grpc_runtime() {
        let dir = temp_plugin_dir("grpc-runtime");
        let mut manifest = write_test_package(
            &dir,
            "memoria-governance-grpc-test",
            "governance.plan",
            "fn memoria_plugin(ctx) { #{} }",
        );
        manifest.runtime = PluginRuntimeKind::Grpc;
        manifest.entry.rhai = None;
        manifest.entry.grpc = Some(GrpcEntry {
            service: "memoria.plugin.v1.StrategyRuntime".into(),
            protocol: "grpc".into(),
        });
        // Write manifest without integrity first, then recompute hash
        manifest.integrity.sha256 = String::new();
        fs::write(
            dir.join("manifest.json"),
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();
        manifest.integrity.sha256 = compute_package_sha256(&dir).unwrap();
        fs::write(
            dir.join("manifest.json"),
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();

        let mut policy = HostPluginPolicy::development();
        policy.supported_runtimes.insert(PluginRuntimeKind::Grpc);
        let package = load_plugin_package(&dir, &policy).expect("grpc package should load");
        assert_eq!(package.plugin_key, "governance:grpc-test:v1");
        assert_eq!(package.entrypoint, "memoria.plugin.v1.StrategyRuntime");

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn rejects_untrusted_signer_when_signatures_enforced() {
        let dir = temp_plugin_dir("signatures");
        write_test_package(
            &dir,
            "memoria-governance-signature-test",
            "governance.plan",
            "fn memoria_plugin(ctx) { #{} }",
        );

        let mut policy = HostPluginPolicy::development();
        policy.enforce_signatures = true;
        let err = load_plugin_package(&dir, &policy).unwrap_err();
        assert!(err.to_string().contains("Untrusted plugin signer"));

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn accepts_trusted_ed25519_signature_when_signatures_enforced() {
        let dir = temp_plugin_dir("trusted-signature");
        let mut manifest = write_test_package(
            &dir,
            "memoria-governance-signed-test",
            "governance.plan",
            "fn memoria_plugin(ctx) { #{} }",
        );

        let signing_key = SigningKey::from_bytes(&[7u8; 32]);
        let public_key = BASE64_STANDARD.encode(signing_key.verifying_key().to_bytes());
        let signature = BASE64_STANDARD.encode(
            signing_key
                .sign(manifest.integrity.sha256.as_bytes())
                .to_bytes(),
        );
        manifest.integrity.signer = "trusted-signer".into();
        manifest.integrity.signature = signature;
        fs::write(
            dir.join("manifest.json"),
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();

        let mut policy = HostPluginPolicy::development();
        policy.enforce_signatures = true;
        policy.trusted_signers.insert("trusted-signer".into());
        policy
            .signer_public_keys
            .insert("trusted-signer".into(), public_key);

        let package =
            load_plugin_package(&dir, &policy).expect("trusted signed package should load");
        assert_eq!(package.plugin_key, "governance:signed-test:v1");

        let _ = fs::remove_dir_all(dir);
    }
}
