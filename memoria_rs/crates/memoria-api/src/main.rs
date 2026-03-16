mod routes;
mod state;

use anyhow::Result;
use axum::{routing::{get, post, delete}, Router};
use clap::Parser;
use memoria_embedding::HttpEmbedder;
use memoria_service::MemoryService;
use memoria_storage::SqlMemoryStore;
use state::AppState;
use std::sync::Arc;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
struct Args {
    #[arg(long, env = "DATABASE_URL", default_value = "mysql://root:111@localhost:6001/memoria_rs")]
    db_url: String,
    #[arg(long, env = "PORT", default_value = "8100")]
    port: u16,
    #[arg(long, env = "EMBEDDING_DIM", default_value = "1024")]
    embedding_dim: usize,
    #[arg(long, env = "EMBEDDING_BASE_URL", default_value = "")]
    embedding_base_url: String,
    #[arg(long, env = "EMBEDDING_API_KEY", default_value = "")]
    embedding_api_key: String,
    #[arg(long, env = "EMBEDDING_MODEL", default_value = "BAAI/bge-m3")]
    embedding_model: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let args = Args::parse();

    let store = SqlMemoryStore::connect(&args.db_url, args.embedding_dim).await?;
    store.migrate().await?;

    let embedder = if !args.embedding_base_url.is_empty() {
        Some(Arc::new(HttpEmbedder::new(
            &args.embedding_base_url,
            &args.embedding_api_key,
            &args.embedding_model,
            args.embedding_dim,
        )) as Arc<dyn memoria_core::interfaces::EmbeddingProvider>)
    } else {
        None
    };

    let service = Arc::new(MemoryService::new_sql(Arc::new(store), embedder));
    let state = AppState { service };

    let app = Router::new()
        .route("/health", get(routes::health))
        .route("/v1/memories", post(routes::store_memory))
        .route("/v1/memories/retrieve", post(routes::retrieve))
        .route("/v1/memories/search", post(routes::search))
        .route("/v1/memories/:id", get(routes::get_memory))
        .route("/v1/memories/:id", post(routes::correct_memory))
        .route("/v1/memories/:id", delete(routes::purge_memory))
        .with_state(state);

    let addr = format!("0.0.0.0:{}", args.port);
    tracing::info!("memoria-api listening on {addr}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
