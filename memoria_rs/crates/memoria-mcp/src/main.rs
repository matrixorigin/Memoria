mod server;
mod tools;
mod git_tools;

use anyhow::Result;
use clap::Parser;
use memoria_embedding::HttpEmbedder;
use memoria_git::GitForDataService;
use memoria_service::MemoryService;
use memoria_storage::SqlMemoryStore;
use sqlx::mysql::MySqlPool;
use std::sync::Arc;

#[derive(Parser)]
struct Args {
    #[arg(long, env = "DATABASE_URL", default_value = "mysql://root:111@localhost:6001/memoria_rs")]
    db_url: String,
    #[arg(long, env = "EMBEDDING_DIM", default_value = "1024")]
    embedding_dim: usize,
    #[arg(long, env = "EMBEDDING_BASE_URL", default_value = "")]
    embedding_base_url: String,
    #[arg(long, env = "EMBEDDING_API_KEY", default_value = "")]
    embedding_api_key: String,
    #[arg(long, env = "EMBEDDING_MODEL", default_value = "BAAI/bge-m3")]
    embedding_model: String,
    #[arg(long, default_value = "default")]
    user: String,
    #[arg(long, env = "DB_NAME", default_value = "memoria_rs")]
    db_name: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();

    let args = Args::parse();

    let store = SqlMemoryStore::connect(&args.db_url, args.embedding_dim).await?;
    store.migrate().await?;

    let pool = MySqlPool::connect(&args.db_url).await?;
    let git = Arc::new(GitForDataService::new(pool, &args.db_name));

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
    server::run_stdio(service, git, args.user).await
}
