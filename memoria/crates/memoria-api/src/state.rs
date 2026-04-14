use crate::auth::{
    spawn_call_log_flusher, spawn_last_used_flusher, spawn_tool_usage_flusher, CallLogBatcher,
    LastUsedBatcher, ToolUsageBatcher,
};
use crate::metrics_summary::MetricsSummaryManager;
use crate::rate_limit::RateLimiter;
use memoria_core::MemoriaError;
use memoria_git::GitForDataService;
use memoria_service::{AsyncTaskStore, MemoryService};
use memoria_storage::store::spawn_pool_monitor;
use memoria_storage::PoolHealthSnapshot;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tracing::{info, warn};

/// Hard upper bounds to prevent misconfiguration.
const METRICS_CACHE_TTL_MAX_SECS: u64 = 300; // 5 min
const AUTH_POOL_MAX_CONNECTIONS_UPPER: u32 = 64;
const AUTH_POOL_ACQUIRE_TIMEOUT_MAX_SECS: u64 = 30;

pub struct CachedMetrics {
    pub body: Arc<String>,
    pub generated_at: Instant,
}

struct ApiKeyCacheEntry {
    user_id: String,
    cached_at: Instant,
}

#[derive(Clone)]
pub struct ApiKeyCache {
    ttl: Duration,
    inner: Arc<std::sync::RwLock<HashMap<String, ApiKeyCacheEntry>>>,
}

impl ApiKeyCache {
    pub fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            inner: Arc::new(std::sync::RwLock::new(HashMap::new())),
        }
    }

    pub fn get(&self, key_hash: &str) -> Option<String> {
        let now = Instant::now();
        if let Ok(cache) = self.inner.read() {
            if let Some(entry) = cache.get(key_hash) {
                if now.duration_since(entry.cached_at) < self.ttl {
                    return Some(entry.user_id.clone());
                }
            }
        }

        self.invalidate(key_hash);
        None
    }

    pub fn insert(&self, key_hash: String, user_id: String) {
        if let Ok(mut cache) = self.inner.write() {
            cache.insert(
                key_hash,
                ApiKeyCacheEntry {
                    user_id,
                    cached_at: Instant::now(),
                },
            );
        }
    }

    pub fn invalidate(&self, key_hash: &str) {
        if let Ok(mut cache) = self.inner.write() {
            cache.remove(key_hash);
        }
    }
}

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
    pub api_key_cache: ApiKeyCache,
    /// Dedicated connection pool for auth queries (isolated from business queries)
    pub auth_pool: Option<sqlx::MySqlPool>,
    /// Batched last_used_at updater
    pub last_used_batcher: Arc<LastUsedBatcher>,
    /// Batched per-user tool usage tracker (flushed every 10 min)
    pub tool_usage_batcher: Arc<ToolUsageBatcher>,
    /// Per-API-key rate limiter
    pub rate_limiter: RateLimiter,
    /// Short-lived cache for Prometheus output to avoid repeated full-table scans.
    pub metrics_cache: Arc<RwLock<Option<CachedMetrics>>>,
    pub metrics_cache_ttl: Duration,
    pub metrics_summary: Option<Arc<MetricsSummaryManager>>,
    /// Batched API call log writer (flushed every 5 s to mem_api_call_log).
    pub call_log_batcher: Arc<CallLogBatcher>,
    /// Shutdown signal + task handles for background flushers.
    /// Wrapped together so drain_flushers() can take ownership of the sender.
    flusher_state: Arc<std::sync::Mutex<FlusherState>>,
}

struct FlusherState {
    shutdown: Option<tokio::sync::watch::Sender<()>>,
    handles: Vec<tokio::task::JoinHandle<()>>,
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
        let metrics_cache_ttl = {
            let raw: u64 = std::env::var("MEMORIA_METRICS_CACHE_TTL_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(5);
            let clamped = raw.clamp(1, METRICS_CACHE_TTL_MAX_SECS);
            if clamped != raw {
                warn!(
                    raw_secs = raw,
                    clamped_secs = clamped,
                    max = METRICS_CACHE_TTL_MAX_SECS,
                    "MEMORIA_METRICS_CACHE_TTL_SECS clamped to bounds"
                );
            }
            Duration::from_secs(clamped)
        };
        Self {
            service,
            git,
            master_key,
            task_store,
            instance_id: "single".into(),
            api_key_cache: ApiKeyCache::new(Duration::from_secs(300)),
            auth_pool: None,
            last_used_batcher: Arc::new(LastUsedBatcher::new()),
            tool_usage_batcher: Arc::new(ToolUsageBatcher::new()),
            call_log_batcher: Arc::new(CallLogBatcher::new()),
            rate_limiter: crate::rate_limit::from_env(),
            metrics_cache: Arc::new(RwLock::new(None)),
            metrics_cache_ttl,
            metrics_summary: None,
            flusher_state: Arc::new(std::sync::Mutex::new(FlusherState {
                shutdown: None,
                handles: Vec::new(),
            })),
        }
    }

    /// Create a dedicated auth pool and start the batched last_used_at flusher.
    /// Call after construction, before serving requests.
    ///
    /// This is strict on purpose: if the auth pool cannot be created, startup fails
    /// rather than letting auth traffic spill into the main business pool.
    pub async fn init_auth_pool(mut self, database_url: &str) -> Result<Self, MemoriaError> {
        let auth_max_connections = {
            let raw: u32 = std::env::var("MEMORIA_AUTH_POOL_MAX_CONNECTIONS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(12);
            let clamped = raw.clamp(1, AUTH_POOL_MAX_CONNECTIONS_UPPER);
            if clamped != raw {
                warn!(
                    raw = raw,
                    clamped = clamped,
                    max = AUTH_POOL_MAX_CONNECTIONS_UPPER,
                    "MEMORIA_AUTH_POOL_MAX_CONNECTIONS clamped to bounds"
                );
            }
            clamped
        };
        let auth_acquire_timeout = {
            let raw: u64 = std::env::var("MEMORIA_AUTH_POOL_ACQUIRE_TIMEOUT_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(5);
            let clamped = raw.clamp(1, AUTH_POOL_ACQUIRE_TIMEOUT_MAX_SECS);
            if clamped != raw {
                warn!(
                    raw_secs = raw,
                    clamped_secs = clamped,
                    max = AUTH_POOL_ACQUIRE_TIMEOUT_MAX_SECS,
                    "MEMORIA_AUTH_POOL_ACQUIRE_TIMEOUT_SECS clamped to bounds"
                );
            }
            Duration::from_secs(clamped)
        };

        let pool = sqlx::mysql::MySqlPoolOptions::new()
            .max_connections(auth_max_connections)
            .max_lifetime(Duration::from_secs(3600))
            .acquire_timeout(auth_acquire_timeout)
            .idle_timeout(Duration::from_secs(300))
            .connect(database_url)
            .await
            .map_err(|e| {
                MemoriaError::Database(format!("failed to create dedicated auth pool: {e}"))
            })?;
        info!(
            max_connections = auth_max_connections,
            acquire_timeout_secs = auth_acquire_timeout.as_secs(),
            "Dedicated auth connection pool initialized"
        );
        spawn_pool_monitor(
            pool.clone(),
            Some(auth_max_connections),
            Arc::new(std::sync::Mutex::new(PoolHealthSnapshot::new(Some(
                auth_max_connections,
            )))),
            "auth_pool",
        );
        // Start the batched last_used_at flusher using the auth pool
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(());
        let h1 = spawn_last_used_flusher(
            self.last_used_batcher.clone(),
            pool.clone(),
            shutdown_rx.clone(),
        );
        let multi_db_mode = self
            .service
            .sql_store
            .as_ref()
            .and_then(|sql| sql.db_router())
            .is_some();

        // In multi-db mode, eager rebuild would fan out across every user DB and can
        // block startup for large tenants. Load per-user tool usage lazily instead.
        if multi_db_mode {
            info!("Skipping eager tool-usage rebuild in multi-db mode");
        } else {
            self.tool_usage_batcher.rebuild_from_db(&self.service).await;
        }
        let h2 = spawn_tool_usage_flusher(
            self.tool_usage_batcher.clone(),
            self.service.clone(),
            shutdown_rx.clone(),
        );
        // Start the call-log flush loop (writes mem_api_call_log every 5 s)
        let h3 = spawn_call_log_flusher(
            self.call_log_batcher.clone(),
            self.service.clone(),
            shutdown_rx.clone(),
        );

        let mut handles = vec![h1, h2, h3];
        if multi_db_mode {
            let manager = Arc::new(MetricsSummaryManager::new(
                self.service.clone(),
                pool.clone(),
                self.metrics_cache.clone(),
            ));
            manager.ensure_schema().await?;
            handles.push(manager.clone().spawn(shutdown_rx.clone()));
            self.metrics_summary = Some(manager);
            info!("Metrics summary refresher initialized for multi-db mode");
        }
        self.auth_pool = Some(pool);
        {
            let mut fs = self.flusher_state.lock().unwrap();
            fs.shutdown = Some(shutdown_tx);
            fs.handles = handles;
        }
        Ok(self)
    }

    pub fn with_instance_id(mut self, instance_id: String) -> Self {
        self.instance_id = instance_id;
        self
    }

    pub async fn mark_metrics_dirty(
        &self,
        user_id: &str,
        mask: crate::metrics_summary::DirtyMask,
    ) -> Result<(), MemoriaError> {
        if let Some(summary) = &self.metrics_summary {
            summary.mark_user_dirty(user_id, mask).await?;
        }
        Ok(())
    }

    /// Signal all background flushers to stop, wait for final flush to complete.
    /// Call during graceful shutdown before dropping the runtime.
    pub async fn drain_flushers(&self) {
        let handles = {
            let mut fs = self.flusher_state.lock().unwrap();
            // Drop sender → tasks receive shutdown signal and do final flush
            fs.shutdown.take();
            fs.handles.drain(..).collect::<Vec<_>>()
        };
        for h in handles {
            let _ = tokio::time::timeout(Duration::from_secs(5), h).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_metrics_cache_hit() {
        let cache: Arc<RwLock<Option<CachedMetrics>>> = Arc::new(RwLock::new(None));
        let ttl = Duration::from_secs(60);

        // Populate cache
        {
            let mut w = cache.write().await;
            *w = Some(CachedMetrics {
                body: Arc::new("cached_body".into()),
                generated_at: Instant::now(),
            });
        }

        // Read should hit
        let r = cache.read().await;
        let snapshot = r.as_ref().unwrap();
        assert!(snapshot.generated_at.elapsed() < ttl);
        assert_eq!(snapshot.body.as_ref(), "cached_body");
    }

    #[tokio::test]
    async fn test_metrics_cache_expiry() {
        let cache: Arc<RwLock<Option<CachedMetrics>>> = Arc::new(RwLock::new(None));
        let ttl = Duration::from_millis(50);

        {
            let mut w = cache.write().await;
            *w = Some(CachedMetrics {
                body: Arc::new("stale".into()),
                generated_at: Instant::now(),
            });
        }

        tokio::time::sleep(Duration::from_millis(60)).await;

        let r = cache.read().await;
        let snapshot = r.as_ref().unwrap();
        assert!(
            snapshot.generated_at.elapsed() >= ttl,
            "cache should be expired"
        );
    }

    #[tokio::test]
    async fn test_metrics_cache_concurrent_refresh() {
        let cache: Arc<RwLock<Option<CachedMetrics>>> = Arc::new(RwLock::new(None));
        let cache2 = cache.clone();

        // Two tasks race to populate the cache
        let (a, b) = tokio::join!(
            async {
                let mut w = cache.write().await;
                if w.is_none() {
                    *w = Some(CachedMetrics {
                        body: Arc::new("first".into()),
                        generated_at: Instant::now(),
                    });
                }
            },
            async {
                let mut w = cache2.write().await;
                if w.is_none() {
                    *w = Some(CachedMetrics {
                        body: Arc::new("second".into()),
                        generated_at: Instant::now(),
                    });
                }
            },
        );
        let _ = (a, b);

        // Exactly one writer should have populated it
        let r = cache.read().await;
        let body = r.as_ref().unwrap().body.as_ref();
        assert!(body == "first" || body == "second");
    }

    #[test]
    fn test_connection_pool_config() {
        // metrics_cache_ttl clamping
        assert_eq!(0u64.clamp(1, METRICS_CACHE_TTL_MAX_SECS), 1);
        assert_eq!(
            999u64.clamp(1, METRICS_CACHE_TTL_MAX_SECS),
            METRICS_CACHE_TTL_MAX_SECS
        );
        assert_eq!(5u64.clamp(1, METRICS_CACHE_TTL_MAX_SECS), 5);

        // auth pool max_connections clamping
        assert_eq!(0u32.clamp(1, AUTH_POOL_MAX_CONNECTIONS_UPPER), 1);
        assert_eq!(
            200u32.clamp(1, AUTH_POOL_MAX_CONNECTIONS_UPPER),
            AUTH_POOL_MAX_CONNECTIONS_UPPER
        );
        assert_eq!(8u32.clamp(1, AUTH_POOL_MAX_CONNECTIONS_UPPER), 8);

        // auth pool acquire_timeout clamping
        assert_eq!(0u64.clamp(1, AUTH_POOL_ACQUIRE_TIMEOUT_MAX_SECS), 1);
        assert_eq!(
            100u64.clamp(1, AUTH_POOL_ACQUIRE_TIMEOUT_MAX_SECS),
            AUTH_POOL_ACQUIRE_TIMEOUT_MAX_SECS
        );
        assert_eq!(5u64.clamp(1, AUTH_POOL_ACQUIRE_TIMEOUT_MAX_SECS), 5);
    }

    #[tokio::test]
    async fn test_metrics_endpoint_with_cache() {
        let cache: Arc<RwLock<Option<CachedMetrics>>> = Arc::new(RwLock::new(None));
        let ttl = Duration::from_secs(60);

        // Miss: cache empty
        assert!(cache.read().await.is_none());

        // Populate (simulates first collect)
        let body = Arc::new("# metrics\nmemoria_memories_total{type=\"all\"} 0\n".to_string());
        {
            let mut w = cache.write().await;
            *w = Some(CachedMetrics {
                body: body.clone(),
                generated_at: Instant::now(),
            });
        }

        // Hit: within TTL, same Arc pointer (no copy)
        let r = cache.read().await;
        let snapshot = r.as_ref().unwrap();
        assert!(snapshot.generated_at.elapsed() < ttl);
        assert!(Arc::ptr_eq(&snapshot.body, &body));
    }
}
