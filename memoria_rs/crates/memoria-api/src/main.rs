use memoria_api::{routes, AppState};
use anyhow::Result;
use axum::{
    routing::{delete, get, post, put},
    Router,
};
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
        db_url = %cfg.db_url,
        port = args.port,
        has_llm = cfg.has_llm(),
        has_embedding = cfg.has_embedding(),
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

    let app = Router::new()
        // Health
        .route("/health", get(routes::memory::health))
        // Memory CRUD
        .route("/v1/memories", get(routes::memory::list_memories))
        .route("/v1/memories", post(routes::memory::store_memory))
        .route("/v1/memories/batch", post(routes::memory::batch_store))
        .route("/v1/memories/retrieve", post(routes::memory::retrieve))
        .route("/v1/memories/search", post(routes::memory::search))
        .route("/v1/memories/correct", post(routes::memory::correct_by_query))
        .route("/v1/memories/purge", post(routes::memory::purge_memories))
        .route("/v1/memories/:id", get(routes::memory::get_memory))
        .route("/v1/memories/:id/correct", put(routes::memory::correct_memory))
        .route("/v1/memories/:id", delete(routes::memory::delete_memory))
        .route("/v1/profiles", get(routes::memory::get_profile))
        // Governance
        .route("/v1/governance", post(routes::governance::governance))
        .route("/v1/consolidate", post(routes::governance::consolidate))
        .route("/v1/reflect", post(routes::governance::reflect))
        .route("/v1/extract-entities", post(routes::governance::extract_entities))
        .route("/v1/extract-entities/link", post(routes::governance::link_entities))
        .route("/v1/entities", get(routes::governance::get_entities))
        // Snapshots
        .route("/v1/snapshots", get(routes::snapshots::list_snapshots))
        .route("/v1/snapshots", post(routes::snapshots::create_snapshot))
        .route("/v1/snapshots/delete", post(routes::snapshots::delete_snapshot_bulk))
        .route("/v1/snapshots/:name", delete(routes::snapshots::delete_snapshot))
        .route("/v1/snapshots/:name/rollback", post(routes::snapshots::rollback))
        // Branches
        .route("/v1/branches", get(routes::snapshots::list_branches))
        .route("/v1/branches", post(routes::snapshots::create_branch))
        .route("/v1/branches/:name/checkout", post(routes::snapshots::checkout_branch))
        .route("/v1/branches/:name/merge", post(routes::snapshots::merge_branch))
        .route("/v1/branches/:name/diff", get(routes::snapshots::diff_branch))
        .route("/v1/branches/:name", delete(routes::snapshots::delete_branch))
        // Sessions (episodic memory)
        .route("/v1/sessions/:session_id/summary", post(routes::sessions::create_session_summary))
        .route("/v1/tasks/:task_id", get(routes::sessions::get_task_status))
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let addr = format!("0.0.0.0:{}", args.port);
    tracing::info!("Listening on {addr}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
