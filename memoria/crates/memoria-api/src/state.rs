use crate::auth::{LastUsedBatcher, spawn_last_used_flusher};
use memoria_git::GitForDataService;
use memoria_service::{AsyncTaskStore, MemoryService};
use moka::future::Cache;
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};

#[derive(Clone)]
pub struct AppState {
    pub service: Arc<MemoryService>,
    pub git: Arc<GitForDataService>,
    /// Master key for auth (empty = no auth)
    pub master_key: String,
    /// Cross-instance async task store (DB-backed when sql_store is available)
    pub task_store: Option<Arc<dyn AsyncTaskStore>>,
    /// Instance identifier for distributed coordination
    pub instance_id: String,
    /// API key hash -> user_id cache (TTL 5 min)
    pub api_key_cache: Cache<String, String>,
    /// Dedicated connection pool for auth queries (isolated from business queries)
    pub auth_pool: Option<sqlx::MySqlPool>,
    /// Batched last_used_at updater
    pub last_used_batcher: Arc<LastUsedBatcher>,
}

impl AppState {
    pub fn new(
        service: Arc<MemoryService>,
        git: Arc<GitForDataService>,
        master_key: String,
    ) -> Self {
        if master_key.is_empty() {
            warn!("MASTER_KEY is not set — running in open mode: all admin endpoints are unauthenticated");
        }
        let task_store: Option<Arc<dyn AsyncTaskStore>> = service
            .sql_store
            .as_ref()
            .map(|s| s.clone() as Arc<dyn AsyncTaskStore>);
        Self {
            service,
            git,
            master_key,
            task_store,
            instance_id: "single".into(),
            api_key_cache: Cache::builder()
                .max_capacity(10_000)
                .time_to_live(Duration::from_secs(300))
                .build(),
            auth_pool: None,
            last_used_batcher: Arc::new(LastUsedBatcher::new()),
        }
    }

    /// Create a dedicated auth pool and start the batched last_used_at flusher.
    /// Call after construction, before serving requests.
    pub async fn init_auth_pool(mut self, database_url: &str) -> Self {
        match sqlx::mysql::MySqlPoolOptions::new()
            .max_connections(2)
            .acquire_timeout(Duration::from_secs(2))
            .idle_timeout(Duration::from_secs(300))
            .connect(database_url)
            .await
        {
            Ok(pool) => {
                info!("Dedicated auth connection pool initialized (max_connections=2, acquire_timeout=2s)");
                // Start the batched last_used_at flusher using the auth pool
                spawn_last_used_flusher(self.last_used_batcher.clone(), pool.clone());
                self.auth_pool = Some(pool);
            }
            Err(e) => {
                warn!("Failed to create auth pool, falling back to main pool: {e}");
                // Still start the flusher using the main pool if available
                if let Some(sql) = &self.service.sql_store {
                    spawn_last_used_flusher(self.last_used_batcher.clone(), sql.pool().clone());
                }
            }
        }
        self
    }

    pub fn with_instance_id(mut self, instance_id: String) -> Self {
        self.instance_id = instance_id;
        self
    }
}
