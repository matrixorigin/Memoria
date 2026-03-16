use memoria_service::Config;
use anyhow::Result;
use clap::Parser;
use memoria_embedding::{HttpEmbedder, LlmClient};
use memoria_git::GitForDataService;
use memoria_service::MemoryService;
use memoria_storage::SqlMemoryStore;
use sqlx::mysql::MySqlPool;
use std::sync::Arc;

/// Memoria MCP server.
///
/// Embedded mode (default): connects directly to MatrixOne DB.
/// Remote mode (--api-url): proxies all calls to a Memoria REST API server.
#[derive(Parser)]
#[command(version, about)]
struct Args {
    /// Remote Memoria API URL (remote mode). If set, --token is used for auth.
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
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();

    let args = Args::parse();

    // Remote mode
    if let Some(api_url) = &args.api_url {
        let user = args.user.clone().unwrap_or_else(|| "default".to_string());
        tracing::info!(api_url = %api_url, user = %user, "Starting Memoria MCP (remote mode)");
        let remote = memoria_mcp::remote::RemoteClient::new(
            api_url,
            args.token.as_deref(),
            user.clone(),
        );
        return memoria_mcp::run_stdio_remote(remote, user).await;
    }

    // Embedded mode
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
        has_llm = cfg.has_llm(),
        user = %cfg.user,
        "Starting Memoria MCP (embedded mode)"
    );

    let store = SqlMemoryStore::connect(&cfg.db_url, cfg.embedding_dim).await?;
    store.migrate().await?;

    let pool = MySqlPool::connect(&cfg.db_url).await?;
    let git = Arc::new(GitForDataService::new(pool, &cfg.db_name));

    let embedder = if cfg.has_embedding() {
        Some(Arc::new(HttpEmbedder::new(
            &cfg.embedding_base_url, &cfg.embedding_api_key,
            &cfg.embedding_model, cfg.embedding_dim,
        )) as Arc<dyn memoria_core::interfaces::EmbeddingProvider>)
    } else { None };

    let llm = cfg.llm_api_key.as_ref().map(|key| {
        Arc::new(LlmClient::new(key.clone(), cfg.llm_base_url.clone(), cfg.llm_model.clone()))
    });

    let service = Arc::new(MemoryService::new_sql_with_llm(Arc::new(store), embedder, llm));
    memoria_mcp::run_stdio(service, git, cfg.user).await
}
