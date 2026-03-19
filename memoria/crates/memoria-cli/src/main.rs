//! memoria — unified CLI for Memoria persistent memory service.
//!
//! Commands:
//!   memoria serve         — start REST API server
//!   memoria mcp           — start MCP server (embedded or remote mode)
//!   memoria init          — detect tools, write MCP config + steering rules
//!   memoria status        — show configuration status
//!   memoria update-rules  — update steering rules to latest version
//!   memoria benchmark     — run benchmark against a Memoria API server

mod benchmark;

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};
use std::path::{Path, PathBuf};
use std::sync::Arc;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const MCP_KEY: &str = "memoria";

// ── Embedded steering templates ───────────────────────────────────────────────

const KIRO_STEERING: &str = include_str!("../templates/kiro_steering.md");
const CURSOR_RULE: &str = include_str!("../templates/cursor_rule.md");
const CLAUDE_RULE: &str = include_str!("../templates/claude_rule.md");

// ── CLI ───────────────────────────────────────────────────────────────────────

#[derive(Clone, ValueEnum)]
enum ToolName {
    Kiro,
    Cursor,
    Claude,
}

impl std::fmt::Display for ToolName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ToolName::Kiro => write!(f, "kiro"),
            ToolName::Cursor => write!(f, "cursor"),
            ToolName::Claude => write!(f, "claude"),
        }
    }
}

#[derive(Parser)]
#[command(name = "memoria", version = VERSION, propagate_version = true, about = "Memoria — persistent memory for AI agents")]
struct Cli {
    /// Project directory (default: current)
    #[arg(long, default_value = ".")]
    dir: PathBuf,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start REST API server
    Serve {
        #[arg(long, env = "DATABASE_URL")]
        db_url: Option<String>,
        #[arg(long, env = "PORT", default_value = "8100")]
        port: u16,
        #[arg(long, env = "MASTER_KEY", default_value = "")]
        master_key: String,
    },
    /// Start MCP server (embedded or remote mode)
    Mcp {
        /// Remote Memoria API URL (remote mode)
        #[arg(long, env = "MEMORIA_API_URL")]
        api_url: Option<String>,
        /// Auth token for remote mode
        #[arg(long, env = "MEMORIA_TOKEN")]
        token: Option<String>,
        /// MySQL connection URL (embedded mode)
        #[arg(long, env = "DATABASE_URL")]
        db_url: Option<String>,
        /// Default user ID
        #[arg(long, env = "MEMORIA_USER")]
        user: Option<String>,
        /// Embedding dimension
        #[arg(long, env = "EMBEDDING_DIM")]
        embedding_dim: Option<usize>,
        /// Embedding base URL
        #[arg(long, env = "EMBEDDING_BASE_URL")]
        embedding_base_url: Option<String>,
        /// Embedding API key
        #[arg(long, env = "EMBEDDING_API_KEY")]
        embedding_api_key: Option<String>,
        /// Embedding model name
        #[arg(long, env = "EMBEDDING_MODEL")]
        embedding_model: Option<String>,
        /// LLM API key
        #[arg(long, env = "LLM_API_KEY")]
        llm_api_key: Option<String>,
        /// LLM base URL
        #[arg(long, env = "LLM_BASE_URL")]
        llm_base_url: Option<String>,
        /// LLM model name
        #[arg(long, env = "LLM_MODEL")]
        llm_model: Option<String>,
        /// Database name for git-for-data
        #[arg(long, env = "MEMORIA_DB_NAME")]
        db_name: Option<String>,
        /// Transport: stdio (default) or sse
        #[arg(long, default_value = "stdio")]
        transport: String,
        /// Port for SSE transport
        #[arg(long, env = "MCP_PORT", default_value = "8200")]
        mcp_port: u16,
    },
    /// Write MCP config + steering rules (-i for interactive wizard)
    Init {
        /// AI tool to configure
        #[arg(long, value_name = "kiro|cursor|claude")]
        tool: Vec<ToolName>,
        /// Interactive setup wizard
        #[arg(short = 'i', long)]
        interactive: bool,
        #[arg(long)]
        db_url: Option<String>,
        #[arg(long)]
        api_url: Option<String>,
        #[arg(long)]
        token: Option<String>,
        #[arg(long, default_value = "default")]
        user: String,
        #[arg(long)]
        force: bool,
        #[arg(long)]
        embedding_provider: Option<String>,
        #[arg(long)]
        embedding_model: Option<String>,
        #[arg(long)]
        embedding_dim: Option<String>,
        #[arg(long)]
        embedding_api_key: Option<String>,
        #[arg(long)]
        embedding_base_url: Option<String>,
    },
    /// Show MCP config and rule version status
    Status,
    /// Update steering rules to latest version
    UpdateRules,
    /// Run benchmark against a Memoria API server
    Benchmark {
        #[arg(long, default_value = "http://127.0.0.1:8100")]
        api_url: String,
        #[arg(long, default_value = "test-master-key-for-docker-compose")]
        token: String,
        #[arg(long, default_value = "core-v1")]
        dataset: String,
        #[arg(long)]
        out: Option<String>,
        #[arg(long)]
        validate_only: bool,
    },
    /// Manage shared plugin repository state
    Plugin {
        #[command(subcommand)]
        command: PluginCommands,
    },
}

#[derive(Subcommand)]
enum PluginCommands {
    /// Scaffold a new plugin project (manifest.json + template script)
    Init {
        /// Output directory (created if missing)
        #[arg(long, default_value = ".")]
        dir: PathBuf,
        /// Plugin name (e.g. "my-policy")
        #[arg(long)]
        name: String,
        /// Plugin domain capability (e.g. "governance.plan")
        #[arg(long, default_value = "governance.plan,governance.execute")]
        capabilities: String,
        /// Runtime: rhai or grpc
        #[arg(long, default_value = "rhai")]
        runtime: String,
    },
    /// Generate a dev-only ed25519 signing keypair
    DevKeygen {
        /// Output directory for key files
        #[arg(long, default_value = ".")]
        dir: PathBuf,
    },
    /// Register or update a trusted plugin signer
    SignerAdd {
        #[arg(long, env = "DATABASE_URL")]
        db_url: Option<String>,
        #[arg(long)]
        signer: String,
        #[arg(long)]
        public_key: String,
        #[arg(long, default_value = "cli")]
        actor: String,
    },
    /// List trusted plugin signers
    SignerList {
        #[arg(long, env = "DATABASE_URL")]
        db_url: Option<String>,
    },
    /// Publish a signed plugin package into the shared repository
    Publish {
        #[arg(long, env = "DATABASE_URL")]
        db_url: Option<String>,
        #[arg(long)]
        package_dir: PathBuf,
        #[arg(long, default_value = "cli")]
        actor: String,
        /// Skip signature verification and auto-approve
        #[arg(long)]
        dev_mode: bool,
    },
    /// List shared plugin packages
    List {
        #[arg(long, env = "DATABASE_URL")]
        db_url: Option<String>,
        #[arg(long)]
        domain: Option<String>,
    },
    /// Activate a shared plugin package for a runtime binding
    Activate {
        #[arg(long, env = "DATABASE_URL")]
        db_url: Option<String>,
        #[arg(long, default_value = "governance")]
        domain: String,
        #[arg(long, default_value = "default")]
        binding: String,
        #[arg(long, default_value = "*")]
        subject: String,
        #[arg(long, default_value_t = 100)]
        priority: i64,
        #[arg(long)]
        plugin_key: String,
        #[arg(long)]
        version: Option<String>,
        #[arg(long)]
        version_req: Option<String>,
        #[arg(long, default_value_t = 100)]
        rollout: i64,
        #[arg(long)]
        endpoint: Option<String>,
        #[arg(long, default_value = "cli")]
        actor: String,
    },
    /// Review or take down a published plugin package
    Review {
        #[arg(long, env = "DATABASE_URL")]
        db_url: Option<String>,
        #[arg(long)]
        plugin_key: String,
        #[arg(long)]
        version: String,
        #[arg(long)]
        status: String,
        #[arg(long)]
        notes: Option<String>,
        #[arg(long, default_value = "cli")]
        actor: String,
    },
    /// Set a plugin package score
    Score {
        #[arg(long, env = "DATABASE_URL")]
        db_url: Option<String>,
        #[arg(long)]
        plugin_key: String,
        #[arg(long)]
        version: String,
        #[arg(long)]
        score: f64,
        #[arg(long)]
        notes: Option<String>,
        #[arg(long, default_value = "cli")]
        actor: String,
    },
    /// Show compatibility matrix entries
    Matrix {
        #[arg(long, env = "DATABASE_URL")]
        db_url: Option<String>,
        #[arg(long)]
        domain: Option<String>,
    },
    /// Show audit events for shared plugins
    Events {
        #[arg(long, env = "DATABASE_URL")]
        db_url: Option<String>,
        #[arg(long)]
        domain: Option<String>,
        #[arg(long)]
        plugin_key: Option<String>,
        #[arg(long)]
        binding: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Show binding rules for one binding
    Rules {
        #[arg(long, env = "DATABASE_URL")]
        db_url: Option<String>,
        #[arg(long, default_value = "governance")]
        domain: String,
        #[arg(long, default_value = "default")]
        binding: String,
    },
}

// ── Serve (API server) ────────────────────────────────────────────────────────

async fn cmd_serve(db_url: Option<String>, port: u16, master_key: String) -> Result<()> {
    use memoria_api::{build_router, AppState};
    use memoria_git::GitForDataService;
    use memoria_service::{Config, MemoryService};
    use memoria_storage::SqlMemoryStore;
    use sqlx::mysql::MySqlPool;
    use tower_http::trace::TraceLayer;
    use tracing_subscriber::EnvFilter;

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let mut cfg = Config::from_env();
    if let Some(v) = db_url {
        cfg.db_url = v;
    }

    tracing::info!(
        db_url = %cfg.db_url, port = port,
        instance_id = %cfg.instance_id,
        has_llm = cfg.has_llm(), has_embedding = cfg.has_embedding(),
        governance_plugin_binding = %cfg.governance_plugin_binding,
        "Starting Memoria API server"
    );

    let store = SqlMemoryStore::connect(&cfg.db_url, cfg.embedding_dim).await?;
    store.migrate().await?;

    let pool = MySqlPool::connect(&cfg.db_url).await?;
    let git = Arc::new(GitForDataService::new(pool, &cfg.db_name));

    let embedder = build_embedder(&cfg);
    let llm = build_llm(&cfg);

    let service = Arc::new(MemoryService::new_sql_with_llm(
        Arc::new(store),
        embedder,
        llm,
    ));
    Arc::new(memoria_service::GovernanceScheduler::from_config(service.clone(), &cfg).await?)
        .start();
    let state = AppState::new(service, git, master_key).with_instance_id(cfg.instance_id.clone());

    let app = build_router(state).layer(TraceLayer::new_for_http());
    let addr = format!("0.0.0.0:{}", port);
    tracing::info!("Listening on {addr}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

// ── MCP server ────────────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
async fn cmd_mcp(
    api_url: Option<String>,
    token: Option<String>,
    db_url: Option<String>,
    user: Option<String>,
    embedding_dim: Option<usize>,
    embedding_base_url: Option<String>,
    embedding_api_key: Option<String>,
    embedding_model: Option<String>,
    llm_api_key: Option<String>,
    llm_base_url: Option<String>,
    llm_model: Option<String>,
    db_name: Option<String>,
    transport: String,
    mcp_port: u16,
) -> Result<()> {
    use memoria_git::GitForDataService;
    use memoria_service::{Config, MemoryService};
    use memoria_storage::SqlMemoryStore;
    use sqlx::mysql::MySqlPool;

    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();

    // Remote mode
    if let Some(api_url) = &api_url {
        let user = user.clone().unwrap_or_else(|| "default".to_string());
        tracing::info!(api_url = %api_url, user = %user, "Starting Memoria MCP (remote mode)");
        let remote =
            memoria_mcp::remote::RemoteClient::new(api_url, token.as_deref(), user.clone());
        return memoria_mcp::run_stdio_remote(remote, user).await;
    }

    // Embedded mode
    let mut cfg = Config::from_env();
    if let Some(v) = db_url {
        cfg.db_url = v;
    }
    if let Some(v) = user {
        cfg.user = v;
    }
    if let Some(v) = embedding_dim {
        cfg.embedding_dim = v;
    }
    if let Some(v) = embedding_base_url {
        cfg.embedding_base_url = v;
    }
    if let Some(v) = embedding_api_key {
        cfg.embedding_api_key = v;
    }
    if let Some(v) = embedding_model {
        cfg.embedding_model = v;
    }
    if let Some(v) = llm_api_key {
        cfg.llm_api_key = Some(v);
    }
    if let Some(v) = llm_base_url {
        cfg.llm_base_url = v;
    }
    if let Some(v) = llm_model {
        cfg.llm_model = v;
    }
    if let Some(v) = db_name {
        cfg.db_name = v;
    }

    tracing::info!(
        db_url = %cfg.db_url,
        embedding_provider = %cfg.embedding_provider,
        has_llm = cfg.has_llm(),
        governance_plugin_binding = %cfg.governance_plugin_binding,
        user = %cfg.user,
        "Starting Memoria MCP (embedded mode)"
    );

    let store = SqlMemoryStore::connect(&cfg.db_url, cfg.embedding_dim).await?;
    store.migrate().await?;

    let pool = MySqlPool::connect(&cfg.db_url).await?;
    let git = Arc::new(GitForDataService::new(pool, &cfg.db_name));

    let embedder = build_embedder(&cfg);
    let llm = build_llm(&cfg);

    let service = Arc::new(MemoryService::new_sql_with_llm(
        Arc::new(store),
        embedder,
        llm,
    ));
    Arc::new(memoria_service::GovernanceScheduler::from_config(service.clone(), &cfg).await?)
        .start();

    if transport == "sse" {
        memoria_mcp::run_sse(service, git, cfg.user, mcp_port).await
    } else {
        memoria_mcp::run_stdio(service, git, cfg.user).await
    }
}

async fn cmd_plugin(command: PluginCommands) -> Result<()> {
    use memoria_service::{
        get_plugin_audit_events, list_binding_rules, list_plugin_compatibility_matrix,
        list_plugin_repository_entries, list_trusted_plugin_signers, publish_plugin_package,
        publish_plugin_package_dev, review_plugin_package, score_plugin_package,
        upsert_plugin_binding_rule, upsert_trusted_plugin_signer, BindingRuleInput, Config,
    };
    use memoria_storage::SqlMemoryStore;

    // Commands that don't need a DB connection
    match &command {
        PluginCommands::Init {
            dir,
            name,
            capabilities,
            runtime,
        } => {
            return cmd_plugin_init(dir, name, capabilities, runtime);
        }
        PluginCommands::DevKeygen { dir } => {
            return cmd_plugin_dev_keygen(dir);
        }
        _ => {}
    }

    let cfg_db_url = match &command {
        PluginCommands::SignerAdd { db_url, .. }
        | PluginCommands::SignerList { db_url }
        | PluginCommands::Publish { db_url, .. }
        | PluginCommands::List { db_url, .. }
        | PluginCommands::Activate { db_url, .. }
        | PluginCommands::Review { db_url, .. }
        | PluginCommands::Score { db_url, .. }
        | PluginCommands::Matrix { db_url, .. }
        | PluginCommands::Events { db_url, .. }
        | PluginCommands::Rules { db_url, .. } => db_url.clone(),
        PluginCommands::Init { .. } | PluginCommands::DevKeygen { .. } => unreachable!(),
    };

    let mut cfg = Config::from_env();
    if let Some(db_url) = cfg_db_url {
        cfg.db_url = db_url;
    }
    let store = SqlMemoryStore::connect(&cfg.db_url, cfg.embedding_dim).await?;
    store.migrate().await?;

    match command {
        PluginCommands::SignerAdd {
            signer,
            public_key,
            actor,
            ..
        } => {
            upsert_trusted_plugin_signer(&store, &signer, &public_key, &actor).await?;
            println!("trusted signer upserted: {signer}");
        }
        PluginCommands::SignerList { .. } => {
            for signer in list_trusted_plugin_signers(&store).await? {
                println!(
                    "{}\t{}\tactive={}\t{}",
                    signer.signer, signer.algorithm, signer.is_active, signer.public_key
                );
            }
        }
        PluginCommands::Publish {
            package_dir,
            actor,
            dev_mode,
            ..
        } => {
            let published = if dev_mode {
                publish_plugin_package_dev(&store, &package_dir, &actor).await?
            } else {
                publish_plugin_package(&store, &package_dir, &actor).await?
            };
            println!(
                "published {}\t{}\t{}\t{}\tstatus={}{}",
                published.plugin_key,
                published.version,
                published.domain,
                published.signer,
                published.status,
                if dev_mode { " (dev mode)" } else { "" }
            );
        }
        PluginCommands::List { domain, .. } => {
            for entry in list_plugin_repository_entries(&store, domain.as_deref()).await? {
                println!(
                    "{}\t{}\t{}\t{}\treview={}\tscore={:.1}\t{}",
                    entry.plugin_key,
                    entry.version,
                    entry.domain,
                    entry.status,
                    entry.review_status,
                    entry.score,
                    entry.signer
                );
            }
        }
        PluginCommands::Activate {
            domain,
            binding,
            subject,
            priority,
            plugin_key,
            version,
            version_req,
            rollout,
            endpoint,
            actor,
            ..
        } => {
            let (selector_kind, selector_value) = match (version, version_req) {
                (Some(version), None) => ("exact", version),
                (None, Some(version_req)) => ("semver", version_req),
                _ => anyhow::bail!("Specify exactly one of --version or --version-req"),
            };
            upsert_plugin_binding_rule(
                &store,
                BindingRuleInput {
                    domain: &domain,
                    binding_key: &binding,
                    subject_key: &subject,
                    priority,
                    plugin_key: &plugin_key,
                    selector_kind,
                    selector_value: &selector_value,
                    rollout_percent: rollout,
                    transport_endpoint: endpoint.as_deref(),
                    actor: &actor,
                },
            )
            .await?;
            println!(
                "activated rule {}\t{}\tsubject={}\tpriority={}\t{}\t{}",
                domain, binding, subject, priority, selector_kind, selector_value
            );
        }
        PluginCommands::Review {
            plugin_key,
            version,
            status,
            notes,
            actor,
            ..
        } => {
            review_plugin_package(
                &store,
                &plugin_key,
                &version,
                &status,
                notes.as_deref(),
                &actor,
            )
            .await?;
            println!("reviewed {plugin_key}@{version} -> {status}");
        }
        PluginCommands::Score {
            plugin_key,
            version,
            score,
            notes,
            actor,
            ..
        } => {
            score_plugin_package(
                &store,
                &plugin_key,
                &version,
                score,
                notes.as_deref(),
                &actor,
            )
            .await?;
            println!("scored {plugin_key}@{version} -> {score}");
        }
        PluginCommands::Matrix { domain, .. } => {
            for entry in list_plugin_compatibility_matrix(&store, domain.as_deref()).await? {
                println!(
                    "{}\t{}\t{}\tstatus={}\treview={}\tsupported={}\t{}\t{}",
                    entry.plugin_key,
                    entry.version,
                    entry.runtime,
                    entry.status,
                    entry.review_status,
                    entry.supported,
                    entry.compatibility,
                    entry.reason
                );
            }
        }
        PluginCommands::Events {
            domain,
            plugin_key,
            binding,
            limit,
            ..
        } => {
            for event in get_plugin_audit_events(
                &store,
                domain.as_deref(),
                plugin_key.as_deref(),
                binding.as_deref(),
                limit,
            )
            .await?
            {
                println!(
                    "{}\t{}\tplugin={}\tversion={}\tbinding={}\tsubject={}\t{}\t{}",
                    event.created_at,
                    event.event_type,
                    event.plugin_key.unwrap_or_default(),
                    event.version.unwrap_or_default(),
                    event.binding_key.unwrap_or_default(),
                    event.subject_key.unwrap_or_default(),
                    event.status,
                    event.message
                );
            }
        }
        PluginCommands::Rules {
            domain, binding, ..
        } => {
            for rule in list_binding_rules(&store, &domain, &binding).await? {
                println!(
                    "{}\tsubject={}\tpriority={}\t{}\t{} {}\trollout={}\tendpoint={}",
                    rule.rule_id,
                    rule.subject_key,
                    rule.priority,
                    rule.plugin_key,
                    rule.selector_kind,
                    rule.selector_value,
                    rule.rollout_percent,
                    rule.transport_endpoint.unwrap_or_default()
                );
            }
        }
        PluginCommands::Init { .. } | PluginCommands::DevKeygen { .. } => unreachable!(),
    }
    Ok(())
}

// ── Plugin scaffolding ────────────────────────────────────────────────────────

fn cmd_plugin_init(dir: &Path, name: &str, capabilities: &str, runtime: &str) -> Result<()> {
    use memoria_service::{GOVERNANCE_RHAI_TEMPLATE, GOVERNANCE_RHAI_TEMPLATE_ENTRYPOINT};
    use serde_json::json;

    std::fs::create_dir_all(dir)?;
    let caps: Vec<&str> = capabilities.split(',').map(str::trim).collect();
    let full_name = if name.starts_with("memoria-") {
        name.to_string()
    } else {
        format!("memoria-{name}")
    };

    let script_file = "policy.rhai";
    let manifest = json!({
        "name": full_name,
        "version": "0.1.0",
        "api_version": "v1",
        "runtime": runtime,
        "entry": {
            "rhai": if runtime == "rhai" { json!({"script": script_file, "entrypoint": GOVERNANCE_RHAI_TEMPLATE_ENTRYPOINT}) } else { json!(null) },
            "grpc": if runtime == "grpc" { json!({"service": "memoria.plugin.v1.StrategyPlugin", "protocol": "grpc"}) } else { json!(null) }
        },
        "capabilities": caps,
        "compatibility": { "memoria": format!(">={}",  env!("CARGO_PKG_VERSION")) },
        "permissions": { "network": runtime == "grpc", "filesystem": false, "env": [] },
        "limits": { "timeout_ms": 500, "max_memory_mb": 32, "max_output_bytes": 8192 },
        "integrity": { "sha256": "", "signature": "", "signer": "" },
        "metadata": { "display_name": name }
    });

    std::fs::write(
        dir.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest)?,
    )?;

    if runtime == "rhai" {
        std::fs::write(dir.join(script_file), GOVERNANCE_RHAI_TEMPLATE)?;
    }

    println!("Plugin scaffolded in {}", dir.display());
    println!("  manifest.json");
    if runtime == "rhai" {
        println!("  {script_file}");
    }
    println!("\nNext steps:");
    println!("  1. Edit the script/manifest");
    println!("  2. memoria plugin dev-keygen --dir {}", dir.display());
    println!("  3. Sign and publish:");
    println!(
        "     memoria plugin publish --package-dir {} --dev-mode",
        dir.display()
    );
    Ok(())
}

fn cmd_plugin_dev_keygen(dir: &Path) -> Result<()> {
    use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
    use ed25519_dalek::SigningKey;

    std::fs::create_dir_all(dir)?;
    let mut secret = [0u8; 32];
    getrandom::getrandom(&mut secret)?;
    let key = SigningKey::from_bytes(&secret);
    let secret_b64 = BASE64.encode(key.to_bytes());
    let public_b64 = BASE64.encode(key.verifying_key().as_bytes());

    std::fs::write(dir.join("dev-signing-key.b64"), &secret_b64)?;
    std::fs::write(dir.join("dev-public-key.b64"), &public_b64)?;

    println!("Generated dev signing keypair in {}", dir.display());
    println!("  dev-signing-key.b64  (KEEP SECRET)");
    println!("  dev-public-key.b64");
    println!("\nTo register the signer:");
    println!("  memoria plugin signer-add --signer dev --public-key {public_b64}");
    println!("\n⚠️  Add dev-signing-key.b64 to .gitignore!");
    Ok(())
}

// ── Shared helpers ────────────────────────────────────────────────────────────

fn build_embedder(
    cfg: &memoria_service::Config,
) -> Option<Arc<dyn memoria_core::interfaces::EmbeddingProvider>> {
    use memoria_embedding::HttpEmbedder;

    if cfg.has_embedding() {
        Some(Arc::new(HttpEmbedder::new(
            &cfg.embedding_base_url,
            &cfg.embedding_api_key,
            &cfg.embedding_model,
            cfg.embedding_dim,
        ))
            as Arc<dyn memoria_core::interfaces::EmbeddingProvider>)
    } else if cfg.embedding_provider == "local" {
        #[cfg(feature = "local-embedding")]
        {
            let local = memoria_embedding::LocalEmbedder::new(&cfg.embedding_model)
                .expect("Failed to load local embedding model");
            Some(Arc::new(local) as Arc<dyn memoria_core::interfaces::EmbeddingProvider>)
        }
        #[cfg(not(feature = "local-embedding"))]
        {
            tracing::error!(
                "EMBEDDING_PROVIDER=local but compiled without local-embedding feature"
            );
            None
        }
    } else {
        None
    }
}

fn build_llm(cfg: &memoria_service::Config) -> Option<Arc<memoria_embedding::LlmClient>> {
    cfg.llm_api_key.as_ref().map(|key| {
        Arc::new(memoria_embedding::LlmClient::new(
            key.clone(),
            cfg.llm_base_url.clone(),
            cfg.llm_model.clone(),
        ))
    })
}

// ── Init / Status / UpdateRules (unchanged logic) ─────────────────────────────

#[allow(clippy::too_many_arguments)]
fn mcp_entry(
    db_url: Option<&str>,
    api_url: Option<&str>,
    token: Option<&str>,
    user: &str,
    embedding_provider: Option<&str>,
    embedding_model: Option<&str>,
    embedding_dim: Option<&str>,
    embedding_api_key: Option<&str>,
    embedding_base_url: Option<&str>,
) -> serde_json::Value {
    let mut args = vec![];
    let mut env = serde_json::Map::new();

    if let Some(url) = api_url {
        // Remote mode — embedding handled server-side
        args.push("--api-url".to_string());
        args.push(url.to_string());
        if let Some(t) = token {
            args.push("--token".to_string());
            args.push(t.to_string());
        }
    } else {
        // Embedded mode
        let url = db_url.unwrap_or("mysql://root:111@localhost:6001/memoria");
        args.push("--db-url".to_string());
        args.push(url.to_string());
        args.push("--user".to_string());
        args.push(user.to_string());

        // Always include all embedding env vars — empty string means "not configured, edit me"
        env.insert(
            "EMBEDDING_PROVIDER".into(),
            embedding_provider.unwrap_or("").into(),
        );
        env.insert(
            "EMBEDDING_BASE_URL".into(),
            embedding_base_url.unwrap_or("").into(),
        );
        env.insert(
            "EMBEDDING_API_KEY".into(),
            embedding_api_key.unwrap_or("").into(),
        );
        env.insert(
            "EMBEDDING_MODEL".into(),
            embedding_model.unwrap_or("").into(),
        );
        env.insert("EMBEDDING_DIM".into(), embedding_dim.unwrap_or("").into());
        env.insert("MEMORIA_GOVERNANCE_ENABLED".into(), "".into());
        env.insert("MEMORIA_GOVERNANCE_PLUGIN_BINDING".into(), "default".into());
        env.insert("_README".into(), serde_json::Value::String(
            "EMBEDDING_*: required for semantic search. Use 'openai' provider with any OpenAI-compatible API (SiliconFlow, Ollama, etc). MEMORIA_GOVERNANCE_PLUGIN_BINDING selects the shared repository binding resolved at startup.".to_string()
        ));
    }

    // Use subcommand: memoria mcp [args]
    let mut full_args = vec!["mcp".to_string()];
    full_args.extend(args);

    let mut entry = serde_json::json!({
        "command": "memoria",
        "args": full_args,
        "_version": VERSION,
    });
    if !env.is_empty() {
        entry["env"] = serde_json::Value::Object(env);
    }
    entry
}

fn which_cmd(name: &str) -> Option<String> {
    std::process::Command::new("which")
        .arg(name)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

fn detect_tools(project_dir: &Path) -> Vec<String> {
    let mut tools = vec![];
    if project_dir.join(".kiro").exists() || which_cmd("kiro").is_some() {
        tools.push("kiro".to_string());
    }
    if project_dir.join(".cursor").exists() || which_cmd("cursor").is_some() {
        tools.push("cursor".to_string());
    }
    if project_dir.join(".mcp.json").exists() || which_cmd("claude").is_some() {
        tools.push("claude".to_string());
    }
    tools
}

fn installed_version(path: &Path) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    regex_version(&content)
}

fn regex_version(content: &str) -> Option<String> {
    content
        .lines()
        .find(|l| l.contains("memoria-version:"))
        .and_then(|l| l.split("memoria-version:").nth(1))
        .map(|v| v.trim().trim_end_matches("-->").trim().to_string())
}

fn write_rule(path: &Path, content: &str, force: bool, project_dir: &Path) -> String {
    let relative = path.strip_prefix(project_dir).unwrap_or(path);
    // Replace template version placeholder with actual binary version
    let content = content.replace(
        "memoria-version: 0.1.0",
        &format!("memoria-version: {}", VERSION),
    );
    if path.exists() && !force {
        let installed = installed_version(path);
        let bundled = regex_version(&content);
        match (&installed, &bundled) {
            (Some(i), Some(b)) if i == b => {
                return format!("  ✓ {} (v{}, up to date)", relative.display(), i);
            }
            (Some(i), Some(b)) => {
                return format!(
                    "  ⚠ {} (v{} installed, v{} available — run update-rules or use --force)",
                    relative.display(),
                    i,
                    b
                );
            }
            _ => {
                return format!(
                    "  ⚠ {} (exists, skipped — use --force to overwrite)",
                    relative.display()
                );
            }
        }
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(path, &content).ok();
    let ver = regex_version(&content)
        .map(|v| format!(" (v{})", v))
        .unwrap_or_default();
    format!("  ✓ {}{}", relative.display(), ver)
}

fn write_mcp_json(path: &Path, entry: &serde_json::Value, project_dir: &Path) -> String {
    let relative = path.strip_prefix(project_dir).unwrap_or(path);
    let wrapper = serde_json::json!({ "mcpServers": { MCP_KEY: entry } });

    if path.exists() {
        if let Ok(content) = std::fs::read_to_string(path) {
            if let Ok(mut existing) = serde_json::from_str::<serde_json::Value>(&content) {
                existing["mcpServers"][MCP_KEY] = entry.clone();
                std::fs::write(path, serde_json::to_string_pretty(&existing).unwrap()).ok();
                return format!("  ✓ {} (updated memoria entry)", relative.display());
            }
        }
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(path, serde_json::to_string_pretty(&wrapper).unwrap()).ok();
    format!("  ✓ {} (created)", relative.display())
}

fn configure_kiro(project_dir: &Path, entry: &serde_json::Value, force: bool) -> Vec<String> {
    vec![
        write_mcp_json(
            &project_dir.join(".kiro/settings/mcp.json"),
            entry,
            project_dir,
        ),
        write_rule(
            &project_dir.join(".kiro/steering/memory.md"),
            KIRO_STEERING,
            force,
            project_dir,
        ),
    ]
}

fn configure_cursor(project_dir: &Path, entry: &serde_json::Value, force: bool) -> Vec<String> {
    vec![
        write_mcp_json(&project_dir.join(".cursor/mcp.json"), entry, project_dir),
        write_rule(
            &project_dir.join(".cursor/rules/memory.mdc"),
            CURSOR_RULE,
            force,
            project_dir,
        ),
    ]
}

fn configure_claude(project_dir: &Path, entry: &serde_json::Value, _force: bool) -> Vec<String> {
    let mut results = vec![write_mcp_json(
        &project_dir.join(".mcp.json"),
        entry,
        project_dir,
    )];
    let claude_rule = CLAUDE_RULE.replace(
        "memoria-version: 0.1.0",
        &format!("memoria-version: {}", VERSION),
    );
    let claude_md = project_dir.join("CLAUDE.md");
    if claude_md.exists() {
        let content = std::fs::read_to_string(&claude_md).unwrap_or_default();
        if !content.contains("memory_retrieve") {
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&claude_md)
                .unwrap();
            use std::io::Write;
            writeln!(f, "\n\n{}", claude_rule).ok();
            results.push("  ✓ CLAUDE.md (appended memory rules)".to_string());
        } else {
            results.push("  ✓ CLAUDE.md (already has memory rules)".to_string());
        }
    } else {
        std::fs::write(&claude_md, &claude_rule).ok();
        results.push("  ✓ CLAUDE.md (created)".to_string());
    }
    results
}

// ── Interactive wizard ─────────────────────────────────────────────────────────

/// Existing config parsed from mcp.json for use as defaults.
struct ExistingConfig {
    tools: Vec<ToolName>,
    db_host: String,
    db_port: String,
    db_user: String,
    db_pass: String,
    db_name: String,
    emb_provider: String,
    emb_base_url: String,
    emb_api_key: String,
    emb_model: String,
    emb_dim: String,
}

impl Default for ExistingConfig {
    fn default() -> Self {
        Self {
            tools: vec![],
            db_host: "localhost".into(),
            db_port: "6001".into(),
            db_user: "root".into(),
            db_pass: "111".into(),
            db_name: "memoria".into(),
            emb_provider: String::new(),
            emb_base_url: String::new(),
            emb_api_key: String::new(),
            emb_model: String::new(),
            emb_dim: String::new(),
        }
    }
}

fn load_existing_config(project_dir: &Path) -> ExistingConfig {
    let mut cfg = ExistingConfig::default();
    let candidates = [
        ("kiro", ".kiro/settings/mcp.json"),
        ("cursor", ".cursor/mcp.json"),
        ("claude", ".mcp.json"),
    ];
    let mut found_entry: Option<serde_json::Value> = None;
    for (tool, path) in &candidates {
        let full = project_dir.join(path);
        if let Ok(content) = std::fs::read_to_string(&full) {
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) {
                if json
                    .get("mcpServers")
                    .and_then(|s| s.get(MCP_KEY))
                    .is_some()
                {
                    match *tool {
                        "kiro" => cfg.tools.push(ToolName::Kiro),
                        "cursor" => cfg.tools.push(ToolName::Cursor),
                        "claude" => cfg.tools.push(ToolName::Claude),
                        _ => {}
                    }
                    if found_entry.is_none() {
                        found_entry = Some(json["mcpServers"][MCP_KEY].clone());
                    }
                }
            }
        }
    }
    if let Some(entry) = &found_entry {
        // Parse db_url from args: mysql://user:pass@host:port/db
        if let Some(args) = entry["args"].as_array() {
            for i in 0..args.len() {
                if args[i].as_str() == Some("--db-url") {
                    if let Some(url) = args.get(i + 1).and_then(|v| v.as_str()) {
                        if let Some(rest) = url.strip_prefix("mysql://") {
                            if let Some((userpass, hostdb)) = rest.split_once('@') {
                                let (u, p) = userpass.split_once(':').unwrap_or((userpass, ""));
                                cfg.db_user = u.to_string();
                                cfg.db_pass = p.to_string();
                                if let Some((hostport, db)) = hostdb.split_once('/') {
                                    cfg.db_name = db.to_string();
                                    let (h, port) =
                                        hostport.split_once(':').unwrap_or((hostport, "6001"));
                                    cfg.db_host = h.to_string();
                                    cfg.db_port = port.to_string();
                                }
                            }
                        }
                    }
                }
            }
        }
        if let Some(env) = entry["env"].as_object() {
            let get = |k: &str| {
                env.get(k)
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string()
            };
            cfg.emb_provider = get("EMBEDDING_PROVIDER");
            cfg.emb_base_url = get("EMBEDDING_BASE_URL");
            cfg.emb_api_key = get("EMBEDDING_API_KEY");
            cfg.emb_model = get("EMBEDDING_MODEL");
            cfg.emb_dim = get("EMBEDDING_DIM");
        }
    }
    cfg
}

fn mask_key(key: &str) -> String {
    if key.len() <= 9 {
        return "*".repeat(key.len());
    }
    format!("{}...{}", &key[..6], &key[key.len() - 3..])
}

fn check_db(db_url: &str) -> bool {
    use std::net::TcpStream;
    use std::time::Duration;
    // Parse host:port from mysql://user:pass@host:port/db
    let addr = db_url
        .strip_prefix("mysql://")
        .and_then(|s| s.split_once('@'))
        .and_then(|(_, hostdb)| hostdb.split_once('/'))
        .map(|(hostport, _)| hostport.to_string())
        .unwrap_or_default();
    if addr.is_empty() {
        println!("  ✗ Database: invalid URL");
        return false;
    }
    match TcpStream::connect_timeout(
        &addr.parse().unwrap_or_else(|_| {
            // Resolve manually for host:port format
            use std::net::ToSocketAddrs;
            addr.to_socket_addrs()
                .ok()
                .and_then(|mut a| a.next())
                .unwrap_or_else(|| ([127, 0, 0, 1], 6001).into())
        }),
        Duration::from_secs(3),
    ) {
        Ok(_) => {
            println!("  ✓ Database: {} reachable", addr);
            true
        }
        Err(e) => {
            println!("  ✗ Database: {} — {}", addr, e);
            false
        }
    }
}

fn check_embedding(base_url: &str, api_key: &str, model: &str) -> bool {
    if base_url.is_empty() {
        // OpenAI official — use default URL
        return check_embedding_request("https://api.openai.com/v1", api_key, model);
    }
    check_embedding_request(base_url, api_key, model)
}

fn check_embedding_request(base_url: &str, api_key: &str, model: &str) -> bool {
    let url = format!("{}/embeddings", base_url.trim_end_matches('/'));
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .unwrap();
    let mut req = client
        .post(&url)
        .header("Content-Type", "application/json")
        .body(format!(r#"{{"model":"{}","input":"test"}}"#, model));
    if !api_key.is_empty() {
        req = req.header("Authorization", format!("Bearer {}", api_key));
    }
    match req.send() {
        Ok(resp) if resp.status().is_success() => {
            println!("  ✓ Embedding: {} OK", base_url);
            true
        }
        Ok(resp) => {
            println!("  ✗ Embedding: {} — HTTP {}", base_url, resp.status());
            false
        }
        Err(e) => {
            println!("  ✗ Embedding: {} — {}", base_url, e);
            false
        }
    }
}

fn cmd_init_interactive(project_dir: &Path, force: bool) {
    cliclack::clear_screen().ok();
    cliclack::intro("🧠 Memoria Setup").ok();

    // ── Project directory ───────────────────────────────────────────
    let default_dir = project_dir.to_string_lossy().to_string();
    let project_input: String = cliclack::input("Project directory")
        .default_input(&default_dir)
        .validate_interactively(|input: &String| {
            if input.is_empty() {
                return Ok(());
            }
            let p = std::path::Path::new(input.as_str());
            let resolved = if p.is_absolute() {
                p.to_path_buf()
            } else {
                std::env::current_dir().unwrap_or_default().join(p)
            };
            if resolved.is_dir() {
                if !input.ends_with('/') {
                    return Ok(());
                }
                // Trailing slash — show subdirectories
                let mut subs: Vec<String> = std::fs::read_dir(&resolved)
                    .ok()
                    .map(|rd| {
                        rd.filter_map(|e| e.ok())
                            .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
                            .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
                            .map(|e| e.file_name().to_string_lossy().to_string())
                            .collect()
                    })
                    .unwrap_or_default();
                subs.sort();
                subs.truncate(8);
                if subs.is_empty() {
                    Ok(())
                } else {
                    Err(subs.join("  "))
                }
            } else {
                // Partial path — match siblings
                let parent = resolved.parent().unwrap_or(&resolved);
                let prefix = resolved
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                let mut matches: Vec<String> = std::fs::read_dir(parent)
                    .ok()
                    .map(|rd| {
                        rd.filter_map(|e| e.ok())
                            .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
                            .filter(|e| {
                                let name = e.file_name().to_string_lossy().to_string();
                                name.starts_with(&prefix) && !name.starts_with('.')
                            })
                            .map(|e| e.file_name().to_string_lossy().to_string())
                            .collect()
                    })
                    .unwrap_or_default();
                matches.sort();
                matches.truncate(8);
                if matches.is_empty() {
                    Err("(no match)".into())
                } else {
                    Err(matches.join("  "))
                }
            }
        })
        .interact()
        .unwrap_or_else(|_| default_dir.clone());
    let project_dir = std::path::Path::new(&project_input)
        .canonicalize()
        .unwrap_or_else(|_| std::path::PathBuf::from(&project_input));

    let existing = load_existing_config(&project_dir);

    // ── Step 1: AI Tool ─────────────────────────────────────────────
    let tool_defaults: Vec<usize> = if existing.tools.is_empty() {
        vec![0]
    } else {
        let mut v = vec![];
        if existing.tools.iter().any(|t| matches!(t, ToolName::Kiro)) {
            v.push(0);
        }
        if existing.tools.iter().any(|t| matches!(t, ToolName::Cursor)) {
            v.push(1);
        }
        if existing.tools.iter().any(|t| matches!(t, ToolName::Claude)) {
            v.push(2);
        }
        v
    };
    let tool_sel: Vec<usize> = match cliclack::multiselect("Which AI tools?")
        .item(0, "Kiro", "")
        .item(1, "Cursor", "")
        .item(2, "Claude Code", "")
        .initial_values(tool_defaults)
        .interact()
    {
        Ok(v) => v,
        Err(_) => {
            cliclack::outro_cancel("Cancelled").ok();
            return;
        }
    };
    let mut tools = vec![];
    if tool_sel.contains(&0) {
        tools.push(ToolName::Kiro);
    }
    if tool_sel.contains(&1) {
        tools.push(ToolName::Cursor);
    }
    if tool_sel.contains(&2) {
        tools.push(ToolName::Claude);
    }
    if tools.is_empty() {
        cliclack::outro_cancel("No tool selected").ok();
        return;
    }

    // ── Step 2: Database ────────────────────────────────────────────
    cliclack::note(
        "Database (MatrixOne)",
        "Configure your MatrixOne connection",
    )
    .ok();

    let db_host: String = cliclack::input("Host")
        .default_input(&existing.db_host)
        .interact()
        .unwrap_or_else(|_| existing.db_host.clone());
    let db_port: String = cliclack::input("Port")
        .default_input(&existing.db_port)
        .interact()
        .unwrap_or_else(|_| existing.db_port.clone());
    let db_user: String = cliclack::input("User")
        .default_input(&existing.db_user)
        .interact()
        .unwrap_or_else(|_| existing.db_user.clone());
    let db_pass: String = if existing.db_pass.is_empty() {
        cliclack::password("Password")
            .mask('▪')
            .interact()
            .unwrap_or_default()
    } else {
        let v: String = cliclack::password(format!("Password [{}]", mask_key(&existing.db_pass)))
            .mask('▪')
            .allow_empty()
            .interact()
            .unwrap_or_default();
        if v.is_empty() {
            existing.db_pass.clone()
        } else {
            v
        }
    };
    let db_name: String = cliclack::input("Database")
        .default_input(&existing.db_name)
        .interact()
        .unwrap_or_else(|_| existing.db_name.clone());
    let db_url = format!(
        "mysql://{}:{}@{}:{}/{}",
        db_user, db_pass, db_host, db_port, db_name
    );

    // ── Step 3: Embedding ───────────────────────────────────────────
    cliclack::note(
        "Embedding Service",
        "⚠ Dimension is locked on first startup. Choose a preset, then adjust any field.",
    )
    .ok();

    let emb_default: usize = match existing.emb_provider.as_str() {
        "openai" if existing.emb_base_url.contains("siliconflow") => 0,
        "openai" if existing.emb_base_url.contains("localhost:11434") => 2,
        "openai" if existing.emb_base_url.is_empty() => 1,
        "openai" => 3,
        _ => 0,
    };
    let emb_choice: usize = cliclack::select("Preset")
        .item(
            0,
            "SiliconFlow",
            "BAAI/bge-m3, 1024d — recommended, free tier",
        )
        .item(1, "OpenAI", "text-embedding-3-small, 1536d")
        .item(2, "Ollama", "nomic-embed-text, 768d — local")
        .item(3, "Custom", "enter all fields manually")
        .initial_value(emb_default)
        .interact()
        .unwrap_or(emb_default);

    let (pre_url, pre_model, pre_dim) = match emb_choice {
        0 => ("https://api.siliconflow.cn/v1", "BAAI/bge-m3", "1024"),
        1 => (
            "https://api.openai.com/v1",
            "text-embedding-3-small",
            "1536",
        ),
        2 => ("http://localhost:11434/v1", "nomic-embed-text", "768"),
        _ => ("", "", ""),
    };
    // Existing config wins over preset; preset fills blanks
    let def_url = if !existing.emb_base_url.is_empty() {
        &existing.emb_base_url
    } else {
        pre_url
    };
    let def_key = &existing.emb_api_key;
    let def_model = if !existing.emb_model.is_empty() {
        &existing.emb_model
    } else {
        pre_model
    };
    let def_dim = if !existing.emb_dim.is_empty() {
        &existing.emb_dim
    } else {
        pre_dim
    };

    let mut url_input = cliclack::input("Base URL").default_input(def_url);
    if def_url.is_empty() {
        url_input = url_input.placeholder("https://api.openai.com/v1");
    }
    let emb_base_url: String = url_input.interact().unwrap_or_else(|_| def_url.to_string());
    let emb_api_key: String = if def_key.is_empty() {
        cliclack::password("API Key")
            .mask('▪')
            .interact()
            .unwrap_or_default()
    } else {
        let v: String = cliclack::password(format!("API Key [{}]", mask_key(def_key)))
            .mask('▪')
            .allow_empty()
            .interact()
            .unwrap_or_default();
        if v.is_empty() {
            def_key.clone()
        } else {
            v
        }
    };
    let emb_model: String = cliclack::input("Model")
        .default_input(def_model)
        .interact()
        .unwrap_or_else(|_| def_model.to_string());
    let emb_dim: String = cliclack::input("Dimension")
        .default_input(def_dim)
        .interact()
        .unwrap_or_else(|_| def_dim.to_string());
    let emb_provider = "openai".to_string();

    // ── Summary ─────────────────────────────────────────────────────
    let tool_names: Vec<&str> = tools
        .iter()
        .map(|t| match t {
            ToolName::Kiro => "Kiro",
            ToolName::Cursor => "Cursor",
            ToolName::Claude => "Claude Code",
        })
        .collect();
    let emb_label = match emb_choice {
        0 => "SiliconFlow",
        1 => "OpenAI",
        2 => "Ollama",
        _ => "Custom",
    };

    cliclack::note(
        "Summary",
        format!(
            "Tools:     {}\nDatabase:  mysql://{}:***@{}:{}/{}\nEmbedding: {} / {} / {}d",
            tool_names.join(", "),
            db_user,
            db_host,
            db_port,
            db_name,
            emb_label,
            emb_model,
            emb_dim,
        ),
    )
    .ok();

    let proceed: bool = cliclack::confirm("Proceed?")
        .initial_value(true)
        .interact()
        .unwrap_or(false);
    if !proceed {
        cliclack::outro_cancel("Aborted").ok();
        return;
    }

    // ── Connectivity checks ─────────────────────────────────────────
    let spinner = cliclack::spinner();
    spinner.start("Checking database connection...");
    let db_ok = check_db(&db_url);
    if db_ok {
        spinner.stop("✔ Database reachable");
    } else {
        spinner.stop("✘ Database unreachable");
    }

    let spinner = cliclack::spinner();
    spinner.start("Checking embedding service...");
    let emb_ok = check_embedding(&emb_base_url, &emb_api_key, &emb_model);
    if emb_ok {
        spinner.stop("✔ Embedding service OK");
    } else {
        spinner.stop("✘ Embedding service unreachable");
    }

    if !db_ok || !emb_ok {
        let cont: bool = cliclack::confirm("Continue anyway?")
            .initial_value(false)
            .interact()
            .unwrap_or(false);
        if !cont {
            cliclack::outro_cancel("Aborted").ok();
            return;
        }
    }

    cmd_init(
        &project_dir,
        tools,
        Some(db_url),
        None,
        None,
        "default".into(),
        force,
        Some(emb_provider),
        Some(emb_model),
        Some(emb_dim),
        Some(emb_api_key),
        Some(emb_base_url),
    );

    cliclack::outro("You're all set! Restart your AI tool to activate Memoria.").ok();
}

#[allow(clippy::too_many_arguments)]
fn cmd_init(
    project_dir: &Path,
    tools: Vec<ToolName>,
    db_url: Option<String>,
    api_url: Option<String>,
    token: Option<String>,
    user: String,
    force: bool,
    embedding_provider: Option<String>,
    embedding_model: Option<String>,
    embedding_dim: Option<String>,
    embedding_api_key: Option<String>,
    embedding_base_url: Option<String>,
) {
    let entry = mcp_entry(
        db_url.as_deref(),
        api_url.as_deref(),
        token.as_deref(),
        &user,
        embedding_provider.as_deref(),
        embedding_model.as_deref(),
        embedding_dim.as_deref(),
        embedding_api_key.as_deref(),
        embedding_base_url.as_deref(),
    );

    for tool in &tools {
        println!("\n[{}]", tool);
        let results = match tool {
            ToolName::Kiro => configure_kiro(project_dir, &entry, force),
            ToolName::Cursor => configure_cursor(project_dir, &entry, force),
            ToolName::Claude => configure_claude(project_dir, &entry, force),
        };
        for r in results {
            println!("{}", r);
        }
    }

    // Post-init guidance
    if api_url.is_none() {
        // Embedded mode checks
        if embedding_provider.is_none() {
            #[cfg(feature = "local-embedding")]
            println!("\n💡 No --embedding-provider specified. Using local embedding (all-MiniLM-L6-v2, dim=384).\n   Model will be downloaded on first query (~30MB to ~/.cache/fastembed/).");
            #[cfg(not(feature = "local-embedding"))]
            println!("\n⚠️  No --embedding-provider specified and this binary was built WITHOUT local-embedding.\n   Edit the env block in the generated mcp.json to configure an embedding service,\n   or re-run with: memoria init --tool {} --embedding-provider openai --embedding-api-key sk-...", tools.iter().map(|t| t.to_string()).collect::<Vec<_>>().join(","));
        }
        println!("\n📝 Generated config includes all environment variables (empty = not configured).\n   Edit the env block in the mcp.json file to fill in your values.");
    }

    println!("\n📄 Steering rules teach your AI tool how to use memory effectively.\n   They are written alongside the MCP config and versioned with the binary.\n   After upgrading Memoria, run: memoria update-rules");
    println!("\n✅ Restart your AI tool to load the new configuration.");
}

fn cmd_status(project_dir: &Path) {
    println!("Memoria status ({})\n", project_dir.display());
    let tools = detect_tools(project_dir);
    if tools.is_empty() {
        println!("No AI tool configs found.");
    }
    for tool in &tools {
        let (mcp_path, rule_path) = match tool.as_str() {
            "kiro" => (".kiro/settings/mcp.json", ".kiro/steering/memory.md"),
            "cursor" => (".cursor/mcp.json", ".cursor/rules/memory.mdc"),
            "claude" => (".mcp.json", "CLAUDE.md"),
            _ => continue,
        };
        println!("[{}]", tool);
        let mcp = project_dir.join(mcp_path);
        if mcp.exists() {
            println!("  ✓ {}", mcp_path);
        } else {
            println!("  ✗ {} (missing)", mcp_path);
        }
        let rule = project_dir.join(rule_path);
        if rule.exists() {
            let ver = installed_version(&rule)
                .map(|v| format!(" (v{})", v))
                .unwrap_or_default();
            println!("  ✓ {}{}", rule_path, ver);
        } else {
            println!("  ✗ {} (missing)", rule_path);
        }
    }
    let bundled = VERSION;
    println!("\nBundled rule version: {}", bundled);
}

fn cmd_update_rules(project_dir: &Path) {
    let tools = detect_tools(project_dir);
    if tools.is_empty() {
        println!("No AI tool configs found. Run 'memoria init' first.");
        return;
    }
    for tool in &tools {
        println!("[{}]", tool);
        let result = match tool.as_str() {
            "kiro" => write_rule(
                &project_dir.join(".kiro/steering/memory.md"),
                KIRO_STEERING,
                true,
                project_dir,
            ),
            "cursor" => write_rule(
                &project_dir.join(".cursor/rules/memory.mdc"),
                CURSOR_RULE,
                true,
                project_dir,
            ),
            "claude" => {
                println!("  ⚠ CLAUDE.md — manual update recommended");
                continue;
            }
            _ => continue,
        };
        println!("{}", result);
    }
}

fn cmd_benchmark(
    api_url: &str,
    token: &str,
    dataset: &str,
    out: Option<&str>,
    validate_only: bool,
) {
    fn print_category_breakdown(
        heading: &str,
        values: &std::collections::HashMap<String, benchmark::CategoryBreakdown>,
    ) {
        if values.is_empty() {
            return;
        }
        let mut items: Vec<_> = values.iter().collect();
        items.sort_by(|a, b| a.0.cmp(b.0));
        println!("  {heading}:");
        for (_key, item) in items {
            println!(
                "    {}: {:.1} ({}) [{}]",
                item.label, item.score, item.grade, item.scenario_count
            );
        }
    }

    let dataset_path = {
        let p = Path::new(dataset);
        if p.exists() {
            p.to_path_buf()
        } else {
            let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
            let candidates = [
                manifest
                    .join("../../../benchmarks/datasets")
                    .join(format!("{dataset}.json")),
                manifest
                    .join("../../../memoria/datasets")
                    .join(format!("{dataset}.json")),
            ];
            candidates
                .into_iter()
                .find(|c| c.exists())
                .unwrap_or_else(|| {
                    eprintln!("Dataset not found: {dataset}");
                    eprintln!("Looked in: benchmarks/datasets/{dataset}.json");
                    std::process::exit(1);
                })
        }
    };

    let content = std::fs::read_to_string(&dataset_path).unwrap_or_else(|e| {
        eprintln!("Failed to read {}: {e}", dataset_path.display());
        std::process::exit(1);
    });

    if validate_only {
        let errors = benchmark::validate_dataset(&content);
        if errors.is_empty() {
            println!("✅ Dataset is valid.");
        } else {
            println!("Validation failed ({} errors):", errors.len());
            for e in &errors {
                println!("  ❌ {e}");
            }
            std::process::exit(1);
        }
        return;
    }

    let ds: benchmark::ScenarioDataset = serde_json::from_str(&content).unwrap_or_else(|e| {
        eprintln!("Failed to parse dataset: {e}");
        std::process::exit(1);
    });
    println!(
        "Dataset: {} {} ({} scenarios)",
        ds.dataset_id,
        ds.version,
        ds.scenarios.len()
    );

    let executor = benchmark::BenchmarkExecutor::new(api_url, token);
    let mut executions = std::collections::HashMap::new();

    for scenario in &ds.scenarios {
        print!("  Running {}...", scenario.scenario_id);
        let exec = executor.execute(scenario);
        let result = benchmark::score_scenario(scenario, &exec);
        let icon = match result.grade.as_str() {
            "S" | "A" => "✅",
            "B" => "⚠️",
            _ => "❌",
        };
        println!(" {icon} {:.1} ({})", result.total_score, result.grade);
        executions.insert(scenario.scenario_id.clone(), exec);
    }

    let report = benchmark::score_dataset(&ds, &executions);
    println!(
        "\nOverall: {:.1} ({})",
        report.overall_score, report.overall_grade
    );
    if !report.by_difficulty.is_empty() {
        let mut items: Vec<_> = report.by_difficulty.iter().collect();
        items.sort_by(|a, b| a.0.cmp(b.0));
        print!("  By difficulty:");
        for (k, v) in &items {
            print!(" {k}={v:.1}");
        }
        println!();
    }
    if !report.by_tag.is_empty() {
        let mut items: Vec<_> = report.by_tag.iter().collect();
        items.sort_by(|a, b| a.0.cmp(b.0));
        print!("  By tag:");
        for (k, v) in &items {
            print!(" {k}={v:.1}");
        }
        println!();
    }
    if !report.by_domain.is_empty() {
        let mut items: Vec<_> = report.by_domain.iter().collect();
        items.sort_by(|a, b| a.0.cmp(b.0));
        print!("  By domain:");
        for (k, v) in &items {
            print!(" {k}={v:.1}");
        }
        println!();
    }
    print_category_breakdown("By source family", &report.by_source_family);
    print_category_breakdown(
        "LongMemEval official categories",
        &report.by_longmemeval_category,
    );
    print_category_breakdown("BEAM official abilities", &report.by_beam_ability);

    if let Some(path) = out {
        let json = serde_json::to_string_pretty(&report).unwrap();
        std::fs::write(path, &json).unwrap_or_else(|e| eprintln!("Failed to write {path}: {e}"));
        println!("  Saved: {path}");
    }
}

// ── Main ──────────────────────────────────────────────────────────────────────

fn main() -> Result<()> {
    let cli = match Cli::try_parse() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("memoria {}", VERSION);
            e.exit();
        }
    };
    let project_dir = cli.dir.canonicalize().unwrap_or(cli.dir);

    match cli.command {
        Commands::Serve {
            db_url,
            port,
            master_key,
        } => {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?
                .block_on(cmd_serve(db_url, port, master_key))?;
        }
        Commands::Mcp {
            api_url,
            token,
            db_url,
            user,
            embedding_dim,
            embedding_base_url,
            embedding_api_key,
            embedding_model,
            llm_api_key,
            llm_base_url,
            llm_model,
            db_name,
            transport,
            mcp_port,
        } => {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?
                .block_on(cmd_mcp(
                    api_url,
                    token,
                    db_url,
                    user,
                    embedding_dim,
                    embedding_base_url,
                    embedding_api_key,
                    embedding_model,
                    llm_api_key,
                    llm_base_url,
                    llm_model,
                    db_name,
                    transport,
                    mcp_port,
                ))?;
        }
        Commands::Init {
            tool,
            interactive,
            db_url,
            api_url,
            token,
            user,
            force,
            embedding_provider,
            embedding_model,
            embedding_dim,
            embedding_api_key,
            embedding_base_url,
        } => {
            if interactive {
                cmd_init_interactive(&project_dir, force);
            } else if tool.is_empty() {
                eprintln!("error: --tool is required (or use -i for interactive wizard)");
                std::process::exit(1);
            } else {
                cmd_init(
                    &project_dir,
                    tool,
                    db_url,
                    api_url,
                    token,
                    user,
                    force,
                    embedding_provider,
                    embedding_model,
                    embedding_dim,
                    embedding_api_key,
                    embedding_base_url,
                );
            }
        }
        Commands::Status => cmd_status(&project_dir),
        Commands::UpdateRules => cmd_update_rules(&project_dir),
        Commands::Benchmark {
            api_url,
            token,
            dataset,
            out,
            validate_only,
        } => {
            cmd_benchmark(&api_url, &token, &dataset, out.as_deref(), validate_only);
        }
        Commands::Plugin { command } => {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?
                .block_on(cmd_plugin(command))?;
        }
    }
    Ok(())
}
