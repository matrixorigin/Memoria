use memoria_git::GitForDataService;
use memoria_service::{AsyncTaskStore, MemoryService};
use moka::future::Cache;
use std::sync::Arc;
use std::time::Duration;
use tracing::warn;

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
        }
    }

    pub fn with_instance_id(mut self, instance_id: String) -> Self {
        self.instance_id = instance_id;
        self
    }
}
