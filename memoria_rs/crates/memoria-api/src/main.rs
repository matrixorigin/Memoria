use memoria_api::{build_router, AppState};
use anyhow::Result;
use clap::Parser;
use memoria_embedding::{HttpEmbedder, LlmClient};
use memoria_git::GitForDataService;
use memoria_service::{Config, MemoryService};
use memoria_storage::SqlMemoryStore;
use sqlx::mysql::MySqlPool;
use std::sync::Arc;
use tower_http::trace::TraceLayer;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(version, about = "Memoria REST API server")]
struct Args {
    #[arg(long, env = "DATABASE_URL")]
    db_url: Option<String>,
    #[arg(long, env = "PORT", default_value = "8100")]
    port: u16,
    #[arg(long, env = "MASTER_KEY", default_value = "")]
    master_key: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let args = Args::parse();
    let mut cfg = Config::from_env();
    if let Some(v) = args.db_url { cfg.db_url = v; }

    tracing::info!(
        db_url = %cfg.db_url, port = args.port,
        has_llm = cfg.has_llm(), has_embedding = cfg.has_embedding(),
        "Starting Memoria API server"
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
    let state = AppState::new(service, git, args.master_key);

    let app = build_router(state).layer(TraceLayer::new_for_http());

    let addr = format!("0.0.0.0:{}", args.port);
    tracing::info!("Listening on {addr}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
