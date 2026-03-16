mod config;
mod server;
mod tools;
mod git_tools;

use anyhow::Result;
use clap::Parser;
use config::Config;
use memoria_embedding::{HttpEmbedder, LlmClient};
use memoria_git::GitForDataService;
use memoria_service::MemoryService;
use memoria_storage::SqlMemoryStore;
use sqlx::mysql::MySqlPool;
use std::sync::Arc;

/// Memoria MCP server — embedded mode (direct DB connection).
///
/// All settings can be provided via CLI flags or environment variables.
/// See Config for the full list of supported environment variables.
#[derive(Parser)]
#[command(version, about)]
struct Args {
    /// MySQL connection URL
    #[arg(long, env = "DATABASE_URL")]
    db_url: Option<String>,

    /// Default user ID
    #[arg(long, env = "MEMORIA_USER")]
    user: Option<String>,

    /// Embedding dimension (overrides EMBEDDING_DIM env var)
    #[arg(long, env = "EMBEDDING_DIM")]
    embedding_dim: Option<usize>,

    /// Embedding base URL (overrides EMBEDDING_BASE_URL env var)
    #[arg(long, env = "EMBEDDING_BASE_URL")]
    embedding_base_url: Option<String>,

    /// Embedding API key (overrides EMBEDDING_API_KEY env var)
    #[arg(long, env = "EMBEDDING_API_KEY")]
    embedding_api_key: Option<String>,

    /// Embedding model name (overrides EMBEDDING_MODEL env var)
    #[arg(long, env = "EMBEDDING_MODEL")]
    embedding_model: Option<String>,

    /// LLM API key for reflect/extract (overrides LLM_API_KEY env var)
    #[arg(long, env = "LLM_API_KEY")]
    llm_api_key: Option<String>,

    /// LLM base URL (overrides LLM_BASE_URL env var)
    #[arg(long, env = "LLM_BASE_URL")]
    llm_base_url: Option<String>,

    /// LLM model name (overrides LLM_MODEL env var)
    #[arg(long, env = "LLM_MODEL")]
    llm_model: Option<String>,

    /// Database name for git-for-data operations
    #[arg(long, env = "MEMORIA_DB_NAME")]
    db_name: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();

    let args = Args::parse();

    // Build config: CLI args override env vars
    let mut cfg = Config::from_env();
    if let Some(v) = args.db_url { cfg.db_url = v; }
    if let Some(v) = args.user { cfg.user = v; }
    if let Some(v) = args.embedding_dim { cfg.embedding_dim = v; }
    if let Some(v) = args.embedding_base_url { cfg.embedding_base_url = v; }
    if let Some(v) = args.embedding_api_key { cfg.embedding_api_key = v; }
    if let Some(v) = args.embedding_model { cfg.embedding_model = v; }
    if let Some(v) = args.llm_api_key { cfg.llm_api_key = Some(v); }
    if let Some(v) = args.llm_base_url { cfg.llm_base_url = v; }
    if let Some(v) = args.llm_model { cfg.llm_model = v; }
    if let Some(v) = args.db_name { cfg.db_name = v; }

    tracing::info!(
        db_url = %cfg.db_url,
        embedding_provider = %cfg.embedding_provider,
        embedding_dim = cfg.embedding_dim,
        has_llm = cfg.has_llm(),
        user = %cfg.user,
        "Starting Memoria MCP server"
    );

    let store = SqlMemoryStore::connect(&cfg.db_url, cfg.embedding_dim).await?;
    store.migrate().await?;

    let pool = MySqlPool::connect(&cfg.db_url).await?;
    let git = Arc::new(GitForDataService::new(pool, &cfg.db_name));

    let embedder = if cfg.has_embedding() {
        Some(Arc::new(HttpEmbedder::new(
            &cfg.embedding_base_url,
            &cfg.embedding_api_key,
            &cfg.embedding_model,
            cfg.embedding_dim,
        )) as Arc<dyn memoria_core::interfaces::EmbeddingProvider>)
    } else {
        if cfg.embedding_provider != "mock" {
            tracing::warn!(
                "Embedding provider '{}' configured but EMBEDDING_BASE_URL is empty — \
                 falling back to keyword-only search",
                cfg.embedding_provider
            );
        }
        None
    };

    let llm = cfg.llm_api_key.as_ref().map(|key| {
        Arc::new(LlmClient::new(
            key.clone(),
            cfg.llm_base_url.clone(),
            cfg.llm_model.clone(),
        ))
    });

    if llm.is_some() {
        tracing::info!(model = %cfg.llm_model, "LLM configured — reflect/extract auto mode enabled");
    } else {
        tracing::info!("No LLM configured — reflect/extract will use candidates mode");
    }

    let service = Arc::new(MemoryService::new_sql_with_llm(Arc::new(store), embedder, llm));
    server::run_stdio(service, git, cfg.user).await
}
