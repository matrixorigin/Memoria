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
    /// Write MCP config + steering rules
    Init {
        /// AI tool to configure (required)
        #[arg(long, required = true, value_name = "kiro|cursor|claude")]
        tool: Vec<ToolName>,
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
    if let Some(v) = db_url { cfg.db_url = v; }

    tracing::info!(
        db_url = %cfg.db_url, port = port,
        has_llm = cfg.has_llm(), has_embedding = cfg.has_embedding(),
        "Starting Memoria API server"
    );

    let store = SqlMemoryStore::connect(&cfg.db_url, cfg.embedding_dim).await?;
    store.migrate().await?;

    let pool = MySqlPool::connect(&cfg.db_url).await?;
    let git = Arc::new(GitForDataService::new(pool, &cfg.db_name));

    let embedder = build_embedder(&cfg);
    let llm = build_llm(&cfg);

    let service = Arc::new(MemoryService::new_sql_with_llm(Arc::new(store), embedder, llm));
    let state = AppState::new(service, git, master_key);

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
    api_url: Option<String>, token: Option<String>,
    db_url: Option<String>, user: Option<String>,
    embedding_dim: Option<usize>, embedding_base_url: Option<String>,
    embedding_api_key: Option<String>, embedding_model: Option<String>,
    llm_api_key: Option<String>, llm_base_url: Option<String>,
    llm_model: Option<String>, db_name: Option<String>,
    transport: String, mcp_port: u16,
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
        let remote = memoria_mcp::remote::RemoteClient::new(
            api_url,
            token.as_deref(),
            user.clone(),
        );
        return memoria_mcp::run_stdio_remote(remote, user).await;
    }

    // Embedded mode
    let mut cfg = Config::from_env();
    if let Some(v) = db_url { cfg.db_url = v; }
    if let Some(v) = user { cfg.user = v; }
    if let Some(v) = embedding_dim { cfg.embedding_dim = v; }
    if let Some(v) = embedding_base_url { cfg.embedding_base_url = v; }
    if let Some(v) = embedding_api_key { cfg.embedding_api_key = v; }
    if let Some(v) = embedding_model { cfg.embedding_model = v; }
    if let Some(v) = llm_api_key { cfg.llm_api_key = Some(v); }
    if let Some(v) = llm_base_url { cfg.llm_base_url = v; }
    if let Some(v) = llm_model { cfg.llm_model = v; }
    if let Some(v) = db_name { cfg.db_name = v; }

    tracing::info!(
        db_url = %cfg.db_url,
        embedding_provider = %cfg.embedding_provider,
        has_llm = cfg.has_llm(),
        user = %cfg.user,
        "Starting Memoria MCP (embedded mode)"
    );

    let store = SqlMemoryStore::connect(&cfg.db_url, cfg.embedding_dim).await?;
    store.migrate().await?;

    let pool = MySqlPool::connect(&cfg.db_url).await?;
    let git = Arc::new(GitForDataService::new(pool, &cfg.db_name));

    let embedder = build_embedder(&cfg);
    let llm = build_llm(&cfg);

    let service = Arc::new(MemoryService::new_sql_with_llm(Arc::new(store), embedder, llm));
    Arc::new(memoria_service::GovernanceScheduler::new(service.clone())).start();

    if transport == "sse" {
        memoria_mcp::run_sse(service, git, cfg.user, mcp_port).await
    } else {
        memoria_mcp::run_stdio(service, git, cfg.user).await
    }
}

// ── Shared helpers ────────────────────────────────────────────────────────────

fn build_embedder(cfg: &memoria_service::Config) -> Option<Arc<dyn memoria_core::interfaces::EmbeddingProvider>> {
    use memoria_embedding::HttpEmbedder;

    if cfg.has_embedding() {
        Some(Arc::new(HttpEmbedder::new(
            &cfg.embedding_base_url, &cfg.embedding_api_key,
            &cfg.embedding_model, cfg.embedding_dim,
        )) as Arc<dyn memoria_core::interfaces::EmbeddingProvider>)
    } else if cfg.embedding_provider == "local" {
        #[cfg(feature = "local-embedding")]
        {
            let local = memoria_embedding::LocalEmbedder::new(&cfg.embedding_model)
                .expect("Failed to load local embedding model");
            Some(Arc::new(local) as Arc<dyn memoria_core::interfaces::EmbeddingProvider>)
        }
        #[cfg(not(feature = "local-embedding"))]
        {
            tracing::error!("EMBEDDING_PROVIDER=local but compiled without local-embedding feature");
            None
        }
    } else {
        None
    }
}

fn build_llm(cfg: &memoria_service::Config) -> Option<Arc<memoria_embedding::LlmClient>> {
    cfg.llm_api_key.as_ref().map(|key| {
        Arc::new(memoria_embedding::LlmClient::new(
            key.clone(), cfg.llm_base_url.clone(), cfg.llm_model.clone(),
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
        env.insert("EMBEDDING_PROVIDER".into(), embedding_provider.unwrap_or("").into());
        env.insert("EMBEDDING_BASE_URL".into(), embedding_base_url.unwrap_or("").into());
        env.insert("EMBEDDING_API_KEY".into(), embedding_api_key.unwrap_or("").into());
        env.insert("EMBEDDING_MODEL".into(), embedding_model.unwrap_or("").into());
        env.insert("EMBEDDING_DIM".into(), embedding_dim.unwrap_or("").into());
        env.insert("_README".into(), serde_json::Value::String(
            "EMBEDDING_*: required for semantic search. Use 'openai' provider with any OpenAI-compatible API (SiliconFlow, Ollama, etc). Empty values = not configured.".to_string()
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
    let content = content.replace("memoria-version: 0.1.0", &format!("memoria-version: {}", VERSION));
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
                    relative.display(), i, b
                );
            }
            _ => {
                return format!("  ⚠ {} (exists, skipped — use --force to overwrite)", relative.display());
            }
        }
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(path, &content).ok();
    let ver = regex_version(&content).map(|v| format!(" (v{})", v)).unwrap_or_default();
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
        write_mcp_json(&project_dir.join(".kiro/settings/mcp.json"), entry, project_dir),
        write_rule(&project_dir.join(".kiro/steering/memory.md"), KIRO_STEERING, force, project_dir),
    ]
}

fn configure_cursor(project_dir: &Path, entry: &serde_json::Value, force: bool) -> Vec<String> {
    vec![
        write_mcp_json(&project_dir.join(".cursor/mcp.json"), entry, project_dir),
        write_rule(&project_dir.join(".cursor/rules/memory.mdc"), CURSOR_RULE, force, project_dir),
    ]
}

fn configure_claude(project_dir: &Path, entry: &serde_json::Value, _force: bool) -> Vec<String> {
    let mut results = vec![
        write_mcp_json(&project_dir.join(".mcp.json"), entry, project_dir),
    ];
    let claude_rule = CLAUDE_RULE.replace("memoria-version: 0.1.0", &format!("memoria-version: {}", VERSION));
    let claude_md = project_dir.join("CLAUDE.md");
    if claude_md.exists() {
        let content = std::fs::read_to_string(&claude_md).unwrap_or_default();
        if !content.contains("memory_retrieve") {
            let mut f = std::fs::OpenOptions::new().append(true).open(&claude_md).unwrap();
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

#[allow(clippy::too_many_arguments)]
fn cmd_init(
    project_dir: &Path, tools: Vec<ToolName>,
    db_url: Option<String>, api_url: Option<String>, token: Option<String>,
    user: String, force: bool,
    embedding_provider: Option<String>, embedding_model: Option<String>,
    embedding_dim: Option<String>, embedding_api_key: Option<String>,
    embedding_base_url: Option<String>,
) {
    let entry = mcp_entry(
        db_url.as_deref(), api_url.as_deref(), token.as_deref(), &user,
        embedding_provider.as_deref(), embedding_model.as_deref(),
        embedding_dim.as_deref(), embedding_api_key.as_deref(),
        embedding_base_url.as_deref(),
    );

    for tool in &tools {
        println!("\n[{}]", tool);
        let results = match tool {
            ToolName::Kiro => configure_kiro(project_dir, &entry, force),
            ToolName::Cursor => configure_cursor(project_dir, &entry, force),
            ToolName::Claude => configure_claude(project_dir, &entry, force),
        };
        for r in results { println!("{}", r); }
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
        if mcp.exists() { println!("  ✓ {}", mcp_path); } else { println!("  ✗ {} (missing)", mcp_path); }
        let rule = project_dir.join(rule_path);
        if rule.exists() {
            let ver = installed_version(&rule).map(|v| format!(" (v{})", v)).unwrap_or_default();
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
            "kiro" => write_rule(&project_dir.join(".kiro/steering/memory.md"), KIRO_STEERING, true, project_dir),
            "cursor" => write_rule(&project_dir.join(".cursor/rules/memory.mdc"), CURSOR_RULE, true, project_dir),
            "claude" => {
                println!("  ⚠ CLAUDE.md — manual update recommended");
                continue;
            }
            _ => continue,
        };
        println!("{}", result);
    }
}

fn cmd_benchmark(api_url: &str, token: &str, dataset: &str, out: Option<&str>, validate_only: bool) {
    let dataset_path = {
        let p = Path::new(dataset);
        if p.exists() { p.to_path_buf() }
        else {
            let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
            let candidates = [
                manifest.join("../../../benchmarks/datasets").join(format!("{dataset}.json")),
                manifest.join("../../../memoria/datasets").join(format!("{dataset}.json")),
            ];
            candidates.into_iter().find(|c| c.exists()).unwrap_or_else(|| {
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
            for e in &errors { println!("  ❌ {e}"); }
            std::process::exit(1);
        }
        return;
    }

    let ds: benchmark::ScenarioDataset = serde_json::from_str(&content).unwrap_or_else(|e| {
        eprintln!("Failed to parse dataset: {e}");
        std::process::exit(1);
    });
    println!("Dataset: {} {} ({} scenarios)", ds.dataset_id, ds.version, ds.scenarios.len());

    let executor = benchmark::BenchmarkExecutor::new(api_url, token);
    let mut executions = std::collections::HashMap::new();

    for scenario in &ds.scenarios {
        print!("  Running {}...", scenario.scenario_id);
        let exec = executor.execute(scenario);
        let result = benchmark::score_scenario(scenario, &exec);
        let icon = match result.grade.as_str() { "S" | "A" => "✅", "B" => "⚠️", _ => "❌" };
        println!(" {icon} {:.1} ({})", result.total_score, result.grade);
        executions.insert(scenario.scenario_id.clone(), exec);
    }

    let report = benchmark::score_dataset(&ds, &executions);
    println!("\nOverall: {:.1} ({})", report.overall_score, report.overall_grade);
    if !report.by_difficulty.is_empty() {
        let mut items: Vec<_> = report.by_difficulty.iter().collect();
        items.sort_by_key(|(k, _)| k.to_string());
        print!("  By difficulty:");
        for (k, v) in &items { print!(" {k}={v:.1}"); }
        println!();
    }
    if !report.by_tag.is_empty() {
        let mut items: Vec<_> = report.by_tag.iter().collect();
        items.sort_by_key(|(k, _)| k.to_string());
        print!("  By tag:");
        for (k, v) in &items { print!(" {k}={v:.1}"); }
        println!();
    }

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
        Commands::Serve { db_url, port, master_key } => {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?
                .block_on(cmd_serve(db_url, port, master_key))?;
        }
        Commands::Mcp {
            api_url, token, db_url, user,
            embedding_dim, embedding_base_url, embedding_api_key, embedding_model,
            llm_api_key, llm_base_url, llm_model, db_name,
            transport, mcp_port,
        } => {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?
                .block_on(cmd_mcp(
                    api_url, token, db_url, user,
                    embedding_dim, embedding_base_url, embedding_api_key, embedding_model,
                    llm_api_key, llm_base_url, llm_model, db_name,
                    transport, mcp_port,
                ))?;
        }
        Commands::Init {
            tool, db_url, api_url, token, user, force,
            embedding_provider, embedding_model, embedding_dim,
            embedding_api_key, embedding_base_url,
        } => {
            cmd_init(
                &project_dir, tool, db_url, api_url, token, user, force,
                embedding_provider, embedding_model, embedding_dim,
                embedding_api_key, embedding_base_url,
            );
        }
        Commands::Status => cmd_status(&project_dir),
        Commands::UpdateRules => cmd_update_rules(&project_dir),
        Commands::Benchmark { api_url, token, dataset, out, validate_only } => {
            cmd_benchmark(&api_url, &token, &dataset, out.as_deref(), validate_only);
        }
    }
    Ok(())
}
