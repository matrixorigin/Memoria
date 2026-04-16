use chrono::{DateTime, Utc};
use memoria_core::{
    check_sensitivity,
    interfaces::{EmbeddingProvider, MemoryStore},
    MemoriaError, Memory, MemoryType, TrustTier,
};
use memoria_embedding::llm::ChatMessage;
use memoria_embedding::LlmClient;
use memoria_storage::{DbRouter, OwnedEditLogEntry, SqlMemoryStore};
use moka::sync::Cache;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::stats_reporter::{StatsEvent, StatsReporter};

/// Incremented when entity extraction jobs are dropped (queue full or channel closed).
pub static ENTITY_EXTRACTION_DROPS: AtomicU64 = AtomicU64::new(0);

#[inline]
fn round4(v: f64) -> f64 {
    (v * 10000.0).round() / 10000.0
}

/// Explain level — mirrors Python's ExplainLevel enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExplainLevel {
    #[default]
    None,
    Basic,
    Verbose,
    Analyze,
}

impl ExplainLevel {
    pub fn from_str_or_bool(s: &str) -> Self {
        match s {
            "true" | "basic" => Self::Basic,
            "verbose" => Self::Verbose,
            "analyze" => Self::Analyze,
            _ => Self::None,
        }
    }
    pub fn at_least(&self, min: ExplainLevel) -> bool {
        (*self as u8) >= (min as u8)
    }
}

/// Extra retrieval controls that must be threaded through the full retrieval pipeline.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RetrieveOptions {
    strict_session_id: Option<String>,
}

impl RetrieveOptions {
    /// `filter_session=true` takes precedence over `include_cross_session`.
    /// When neither is provided, retrieval remains cross-session.
    pub fn from_session_scope(
        session_id: Option<&str>,
        filter_session: Option<bool>,
        include_cross_session: Option<bool>,
    ) -> Self {
        let strict_session = filter_session == Some(true) || include_cross_session == Some(false);
        Self {
            strict_session_id: session_id.filter(|_| strict_session).map(str::to_string),
        }
    }

    pub fn strict_session_id(&self) -> Option<&str> {
        self.strict_session_id.as_deref()
    }

    pub fn is_strict_session(&self) -> bool {
        self.strict_session_id.is_some()
    }
}

/// Per-candidate scoring breakdown — answers "why is this memory ranked here?"
/// Only populated at Verbose/Analyze level.
#[derive(Debug, serde::Serialize)]
pub struct CandidateScore {
    pub memory_id: String,
    pub rank: usize,
    pub final_score: f64,
    pub vector_score: f64,
    pub keyword_score: f64,
    pub temporal_score: f64,
    pub confidence_score: f64,
}

/// Explain stats for retrieve/search — like SQL EXPLAIN ANALYZE.
#[derive(Debug, Default, serde::Serialize)]
pub struct RetrievalExplain {
    pub level: ExplainLevel,
    pub path: &'static str, // "vector", "fulltext", "graph", "graph+vector", "none"
    pub vector_attempted: bool,
    pub vector_hit: bool,
    pub fulltext_attempted: bool,
    pub fulltext_hit: bool,
    pub graph_attempted: bool,
    pub graph_hit: bool,
    pub graph_candidates: usize,
    pub result_count: usize,
    pub embedding_ms: f64,
    pub vector_ms: f64,
    pub fulltext_ms: f64,
    pub graph_ms: f64,
    pub total_ms: f64,
    /// Per-candidate scores (Verbose/Analyze only)
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub candidate_scores: Vec<CandidateScore>,
}

/// Result of a purge operation.
pub struct PurgeResult {
    pub purged: usize,
    /// Safety snapshot created before purge. None if snapshot creation failed.
    pub snapshot_name: Option<String>,
    /// Warning message if snapshot creation had issues (quota full, auto-cleanup, etc.)
    pub warning: Option<String>,
}

/// In-memory access counter that batches DB writes to avoid row-lock contention.
/// Accumulates counts in a DashMap and flushes every `FLUSH_INTERVAL`.
struct AccessCounter {
    pending: Arc<dashmap::DashMap<(String, String), AtomicU64>>,
    _shutdown: tokio::sync::watch::Sender<()>,
}

const ACCESS_FLUSH_INTERVAL: Duration = Duration::from_secs(5);

impl AccessCounter {
    fn new(store: Arc<SqlMemoryStore>) -> Self {
        let pending: Arc<dashmap::DashMap<(String, String), AtomicU64>> =
            Arc::new(dashmap::DashMap::new());
        let p = pending.clone();
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(());
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(ACCESS_FLUSH_INTERVAL) => {}
                    _ = shutdown_rx.changed() => {
                        // Final flush before exit
                        Self::flush(Arc::clone(&p), Arc::clone(&store)).await;
                        break;
                    }
                }
                Self::flush(Arc::clone(&p), Arc::clone(&store)).await;
            }
            tracing::debug!("access counter flusher exiting");
        });
        Self {
            pending,
            _shutdown: shutdown_tx,
        }
    }

    fn bump(&self, ids: &[(String, String)]) {
        for (user_id, id) in ids {
            self.pending
                .entry((user_id.clone(), id.clone()))
                .or_insert_with(|| AtomicU64::new(0))
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    async fn flush(
        pending: Arc<dashmap::DashMap<(String, String), AtomicU64>>,
        store: Arc<SqlMemoryStore>,
    ) {
        // Drain all entries
        let batch: Vec<((String, String), u64)> = pending
            .iter()
            .map(|e| (e.key().clone(), e.value().swap(0, Ordering::Relaxed)))
            .filter(|(_, n)| *n > 0)
            .collect();
        // Remove zeroed entries to avoid unbounded growth
        pending.retain(|_, v| v.load(Ordering::Relaxed) > 0);

        if batch.is_empty() {
            return;
        }
        if let Some(router) = store.db_router() {
            let mut by_user: std::collections::HashMap<String, Vec<(String, u64)>> =
                std::collections::HashMap::new();
            for ((user_id, memory_id), count) in batch {
                by_user.entry(user_id).or_default().push((memory_id, count));
            }
            for (user_id, user_batch) in by_user {
                match router.routed_store_for_user(&user_id) {
                    Ok(user_store) => {
                        if let Err(e) = user_store.bump_access_counts_batch(&user_batch).await {
                            tracing::warn!(user_id, error = %e, "access counter flush failed");
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            user_id,
                            error = %e,
                            "failed to route access counter flush to user store"
                        );
                    }
                }
            }
        } else {
            let batch: Vec<(String, u64)> = batch
                .into_iter()
                .map(|((_user_id, memory_id), count)| (memory_id, count))
                .collect();
            if let Err(e) = store.bump_access_counts_batch(&batch).await {
                tracing::warn!("access counter flush failed: {e}");
            }
        }
    }
}

// ── Async batched edit-log writer ─────────────────────────────────────────────

/// Trait for edit-log flush target — allows testing the batching logic without a real DB.
#[async_trait::async_trait]
trait EditLogFlusher: Send + Sync + 'static {
    async fn flush_batch(&self, entries: &[OwnedEditLogEntry]) -> Result<(), MemoriaError>;
}

#[async_trait::async_trait]
impl EditLogFlusher for SqlMemoryStore {
    async fn flush_batch(&self, entries: &[OwnedEditLogEntry]) -> Result<(), MemoriaError> {
        let store = self.clone();
        store.flush_edit_log_batch(entries).await
    }
}

/// In-memory flusher for tests — collects entries so tests can assert on them.
#[derive(Default)]
pub struct InMemoryFlusher {
    pub entries: Arc<std::sync::Mutex<Vec<OwnedEditLogEntry>>>,
}

#[async_trait::async_trait]
impl EditLogFlusher for InMemoryFlusher {
    async fn flush_batch(&self, entries: &[OwnedEditLogEntry]) -> Result<(), MemoriaError> {
        self.entries.lock().unwrap().extend(entries.iter().cloned());
        Ok(())
    }
}

/// Async batched edit-log writer. Collects entries via a bounded channel
/// and flushes as a multi-row batch every 2s or when 64 entries accumulate.
struct EditLogBuffer {
    tx: tokio::sync::mpsc::Sender<OwnedEditLogEntry>,
    flush_tx: tokio::sync::mpsc::Sender<tokio::sync::oneshot::Sender<()>>,
    handle: tokio::task::JoinHandle<()>,
}

const EDIT_LOG_FLUSH_INTERVAL: Duration = Duration::from_secs(2);
const EDIT_LOG_FLUSH_SIZE: usize = 64;
const EDIT_LOG_CHANNEL_CAP: usize = 4096;
const EDIT_LOG_DRAIN_TIMEOUT: Duration = Duration::from_secs(30);
const EDIT_LOG_RETRY_DELAY: Duration = Duration::from_millis(500);

impl EditLogBuffer {
    fn new<F: EditLogFlusher>(flusher: Arc<F>) -> Self {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<OwnedEditLogEntry>(EDIT_LOG_CHANNEL_CAP);
        let (flush_tx, mut flush_rx) =
            tokio::sync::mpsc::channel::<tokio::sync::oneshot::Sender<()>>(4);
        let handle = tokio::spawn(async move {
            let mut buf = Vec::with_capacity(EDIT_LOG_FLUSH_SIZE);
            loop {
                let deadline = tokio::time::sleep(EDIT_LOG_FLUSH_INTERVAL);
                tokio::pin!(deadline);
                loop {
                    tokio::select! {
                        entry = rx.recv() => {
                            match entry {
                                Some(e) => {
                                    buf.push(e);
                                    if buf.len() >= EDIT_LOG_FLUSH_SIZE {
                                        break;
                                    }
                                }
                                None => {
                                    if !buf.is_empty() {
                                        Self::flush_with_retry(flusher.as_ref(), &mut buf).await;
                                    }
                                    return;
                                }
                            }
                        }
                        ack = flush_rx.recv() => {
                            while let Ok(e) = rx.try_recv() { buf.push(e); }
                            if !buf.is_empty() {
                                Self::flush_with_retry(flusher.as_ref(), &mut buf).await;
                            }
                            match ack {
                                Some(tx) => { let _ = tx.send(()); }
                                None => continue, // flush_tx dropped; rx.recv() will drive shutdown
                            }
                            continue;
                        }
                        _ = &mut deadline => break,
                    }
                }
                if !buf.is_empty() {
                    Self::flush_with_retry(flusher.as_ref(), &mut buf).await;
                }
            }
        });
        Self {
            tx,
            flush_tx,
            handle,
        }
    }

    async fn flush_with_retry(flusher: &dyn EditLogFlusher, buf: &mut Vec<OwnedEditLogEntry>) {
        if let Err(e) = flusher.flush_batch(buf).await {
            tracing::warn!(
                "edit log flush failed, retrying in {:?}: {e}",
                EDIT_LOG_RETRY_DELAY
            );
            tokio::time::sleep(EDIT_LOG_RETRY_DELAY).await;
            if let Err(e2) = flusher.flush_batch(buf).await {
                tracing::error!(
                    "edit log flush retry failed, dropping {} entries: {e2}",
                    buf.len()
                );
            }
        }
        buf.clear();
    }

    fn send(
        &self,
        user_id: &str,
        operation: &str,
        memory_id: Option<&str>,
        payload: Option<&str>,
        reason: &str,
        snapshot_before: Option<&str>,
    ) {
        let entry = OwnedEditLogEntry {
            edit_id: uuid::Uuid::now_v7().simple().to_string(),
            user_id: user_id.to_string(),
            operation: operation.to_string(),
            memory_id: memory_id.map(String::from),
            payload: payload.map(String::from),
            reason: reason.to_string(),
            snapshot_before: snapshot_before.map(String::from),
        };
        match self.tx.try_send(entry) {
            Ok(()) => {}
            Err(tokio::sync::mpsc::error::TrySendError::Full(entry)) => {
                tracing::warn!(
                    user_id = %entry.user_id,
                    operation = %entry.operation,
                    memory_id = ?entry.memory_id,
                    "edit log channel full, dropping entry"
                );
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(entry)) => {
                tracing::warn!(
                    user_id = %entry.user_id,
                    operation = %entry.operation,
                    memory_id = ?entry.memory_id,
                    "edit log channel closed, dropping entry"
                );
            }
        }
    }

    /// Drain: drop sender to stop new writes, await background task flush, with timeout.
    async fn drain(self, timeout: Duration) -> bool {
        drop(self.tx);
        drop(self.flush_tx);
        match tokio::time::timeout(timeout, self.handle).await {
            Ok(Ok(())) => {
                tracing::info!("edit log drained");
                true
            }
            Ok(Err(e)) => {
                tracing::error!("edit log drain task panicked: {e}");
                false
            }
            Err(_) => {
                tracing::error!("edit log drain timed out after {timeout:?}");
                false
            }
        }
    }

    /// Get a clone of the entry sender for wiring into SqlMemoryStore.
    fn entry_sender(&self) -> tokio::sync::mpsc::Sender<OwnedEditLogEntry> {
        self.tx.clone()
    }
}

pub struct MemoryService {
    /// Trait-based store for generic ops (used by tests with MockStore)
    pub store: Arc<dyn MemoryStore>,
    /// Concrete store for branch-aware ops (None in tests)
    pub sql_store: Option<Arc<SqlMemoryStore>>,
    /// Optional multi-DB router. When present, per-user operations should resolve
    /// a dedicated user store instead of using `sql_store` directly.
    pub db_router: Option<Arc<DbRouter>>,
    pub embedder: Option<Arc<dyn EmbeddingProvider>>,
    /// LLM client for reflect/extract (None if LLM_API_KEY not set)
    pub llm: Option<Arc<LlmClient>>,
    /// Async entity extraction queue (None when sql_store is absent)
    entity_tx: Option<tokio::sync::mpsc::Sender<EntityJob>>,
    /// Batched access counter (None in tests)
    access_counter: Option<AccessCounter>,
    /// Async batched edit-log writer
    edit_log: std::sync::Mutex<Option<EditLogBuffer>>,
    /// Per-user feedback_weight cache (TTL 5 min)
    feedback_weight_cache: Cache<String, f64>,
    /// Vector index monitor (None in tests)
    vector_monitor: Option<Arc<crate::vector_index_monitor::VectorIndexMonitor>>,
    /// Isolated pool for graph retrieval (spreading activation, entity recall)
    /// to avoid starving the main pool during heavy retrieve queries.
    graph_pool: Option<Arc<SqlMemoryStore>>,
    /// Shared operational metrics reporter (None = disabled, zero overhead).
    /// Wrapped in OnceLock so the entity background worker can also access it
    /// after construction via `with_stats()`.
    stats: Arc<OnceLock<Arc<StatsReporter>>>,
}

/// A pending entity-extraction job pushed from the write path.
struct EntityJob {
    user_id: String,
    memory_id: String,
    content: String,
}

impl MemoryService {
    /// Production constructor — uses SqlMemoryStore for branch support
    pub async fn new_sql(
        store: Arc<SqlMemoryStore>,
        embedder: Option<Arc<dyn EmbeddingProvider>>,
    ) -> Self {
        let llm = LlmClient::from_env().map(Arc::new);
        Self::new_sql_with_llm(store, embedder, llm).await
    }

    /// Production constructor with explicit LLM client.
    pub async fn new_sql_with_llm(
        store: Arc<SqlMemoryStore>,
        embedder: Option<Arc<dyn EmbeddingProvider>>,
        llm: Option<Arc<LlmClient>>,
    ) -> Self {
        Self::new_sql_with_llm_and_router(store, None, embedder, llm).await
    }

    /// Production constructor with explicit multi-DB router.
    pub async fn new_sql_with_llm_and_router(
        store: Arc<SqlMemoryStore>,
        db_router: Option<Arc<DbRouter>>,
        embedder: Option<Arc<dyn EmbeddingProvider>>,
        llm: Option<Arc<LlmClient>>,
    ) -> Self {
        // Create edit-log buffer early so background stores inherit the sender
        let edit_log = EditLogBuffer::new(store.clone());
        store.set_edit_log_tx(edit_log.entry_sender());

        // Stats cell shared between MemoryService and background entity workers.
        // Populated later via with_stats(); workers check it on each batch.
        let stats_cell: Arc<OnceLock<Arc<StatsReporter>>> = Arc::new(OnceLock::new());

        let entity_queue_size: usize = std::env::var("ENTITY_QUEUE_SIZE")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(512)
            .clamp(64, 8192);
        let vector_monitor = if db_router.is_some() {
            info!("vector rebuild worker disabled in multi-db mode");
            None
        } else {
            match store.spawn_background_store(2).await {
                Ok(rebuild_store) => {
                    let (rebuild_tx, rebuild_rx) = tokio::sync::mpsc::channel(4);
                    crate::vector_index_monitor::init_coarse_clock();
                    let vector_monitor =
                        Arc::new(crate::vector_index_monitor::VectorIndexMonitor::new(
                            "mem_memories".to_string(),
                            rebuild_tx,
                        ));
                    let worker =
                        crate::rebuild_worker::RebuildWorker::new(rebuild_store, rebuild_rx);
                    tokio::spawn(async move { worker.run().await });
                    Some(vector_monitor)
                }
                Err(e) => {
                    error!(
                        error = %e,
                        "rebuild worker disabled because isolated pool initialization failed"
                    );
                    None
                }
            }
        };

        let entity_pool_size_raw: Option<u32> = std::env::var("ENTITY_POOL_SIZE")
            .ok()
            .and_then(|s| s.parse().ok());
        let entity_tx = if db_router.is_some() {
            if entity_pool_size_raw == Some(0) {
                info!("Entity extraction disabled in multi-db mode (ENTITY_POOL_SIZE=0)");
                None
            } else {
                if let Some(raw) = entity_pool_size_raw {
                    info!(
                        entity_pool_size = raw,
                        "ENTITY_POOL_SIZE ignored in multi-db mode; entity extraction uses routed user stores"
                    );
                } else {
                    info!("Entity extraction uses routed user stores in multi-db mode");
                }
                let (entity_tx, entity_rx) = tokio::sync::mpsc::channel(entity_queue_size);
                Self::spawn_entity_worker(
                    entity_rx,
                    store.clone(),
                    llm.clone(),
                    stats_cell.clone(),
                );
                tracing::info!(entity_queue_size, "entity extraction enabled");
                Some(entity_tx)
            }
        } else {
            let entity_pool_size = entity_pool_size_raw.unwrap_or(8);
            if entity_pool_size == 0 {
                info!("Entity extraction disabled; ENTITY_POOL_SIZE=0");
                None
            } else {
                let entity_pool_size = entity_pool_size.clamp(2, 32);
                match store.spawn_background_store(entity_pool_size).await {
                    Ok(entity_store) => {
                        let (entity_tx, entity_rx) = tokio::sync::mpsc::channel(entity_queue_size);
                        Self::spawn_entity_worker(
                            entity_rx,
                            entity_store,
                            llm.clone(),
                            stats_cell.clone(),
                        );
                        tracing::info!(
                            entity_queue_size,
                            entity_pool_size,
                            "entity extraction enabled"
                        );
                        Some(entity_tx)
                    }
                    Err(e) => {
                        error!(
                            error = %e,
                            "entity extraction disabled because isolated pool initialization failed"
                        );
                        None
                    }
                }
            }
        };

        // Isolated pool for graph retrieval (spreading activation, entity recall).
        // Keeps graph queries from competing with the main pool during retrieve.
        let graph_pool_size_raw: Option<u32> = std::env::var("GRAPH_POOL_SIZE")
            .ok()
            .and_then(|s| s.parse().ok());
        let graph_pool = if db_router.is_some() {
            if let Some(raw) = graph_pool_size_raw.filter(|raw| *raw > 0) {
                info!(
                    graph_pool_size = raw,
                    "GRAPH_POOL_SIZE ignored in multi-db mode; graph queries use the global user pool"
                );
            } else {
                info!("Graph retrieval isolated pool skipped in multi-db mode");
            }
            None
        } else {
            let graph_pool_size = graph_pool_size_raw.unwrap_or(8);
            if graph_pool_size == 0 {
                info!("Graph retrieval isolated pool disabled; graph queries will use main pool");
                None
            } else {
                let graph_pool_size = graph_pool_size.min(32);
                match store.spawn_background_store(graph_pool_size).await {
                    Ok(gp) => {
                        info!(graph_pool_size, "Graph retrieval using isolated pool");
                        Some(gp)
                    }
                    Err(e) => {
                        warn!(
                            error = %e,
                            graph_pool_size,
                            "graph isolated pool failed, graph queries will use main pool"
                        );
                        None
                    }
                }
            }
        };

        Self {
            store: store.clone(),
            sql_store: Some(store.clone()),
            db_router,
            embedder,
            llm: llm.clone(),
            entity_tx,
            // Use graph pool for access counter flushes to keep writes off the main pool
            access_counter: Some(AccessCounter::new(
                graph_pool
                    .as_ref()
                    .map(Arc::clone)
                    .unwrap_or_else(|| store.clone()),
            )),
            edit_log: std::sync::Mutex::new(Some(edit_log)),
            feedback_weight_cache: Cache::builder()
                .max_capacity(10_000)
                .time_to_live(Duration::from_secs(300))
                .build(),
            vector_monitor,
            graph_pool,
            stats: stats_cell,
        }
    }

    /// Lightweight constructor — no background workers.
    /// If `sql_store` is provided, edit-log writes go to the real DB via async batching.
    /// If `sql_store` is None, edit-log entries are silently dropped.
    pub fn new(
        store: Arc<dyn MemoryStore>,
        embedder: Option<Arc<dyn EmbeddingProvider>>,
        sql_store: Option<Arc<SqlMemoryStore>>,
    ) -> Self {
        let edit_log = match &sql_store {
            Some(s) => {
                let buf = EditLogBuffer::new(Arc::clone(s));
                s.set_edit_log_tx(buf.entry_sender());
                buf
            }
            None => {
                let flusher = Arc::new(InMemoryFlusher::default());
                EditLogBuffer::new(flusher)
            }
        };
        Self {
            store: store.clone(),
            sql_store,
            db_router: None,
            embedder,
            llm: None,
            entity_tx: None,
            access_counter: None,
            edit_log: std::sync::Mutex::new(Some(edit_log)),
            feedback_weight_cache: Cache::builder()
                .max_capacity(10_000)
                .time_to_live(Duration::from_secs(300))
                .build(),
            vector_monitor: None,
            graph_pool: None,
            stats: Arc::new(OnceLock::new()),
        }
    }

    /// Test constructor — like `new()` but returns an in-memory entry collector
    /// so tests can assert on edit-log writes without a real DB.
    #[doc(hidden)]
    pub fn new_with_test_entries(
        store: Arc<dyn MemoryStore>,
        embedder: Option<Arc<dyn EmbeddingProvider>>,
    ) -> (Self, Arc<std::sync::Mutex<Vec<OwnedEditLogEntry>>>) {
        let flusher = Arc::new(InMemoryFlusher::default());
        let entries = Arc::clone(&flusher.entries);
        let edit_log = EditLogBuffer::new(flusher);
        (
            Self {
                store,
                sql_store: None,
                db_router: None,
                embedder,
                llm: None,
                entity_tx: None,
                access_counter: None,
                edit_log: std::sync::Mutex::new(Some(edit_log)),
                feedback_weight_cache: Cache::builder()
                    .max_capacity(10_000)
                    .time_to_live(Duration::from_secs(300))
                    .build(),
                vector_monitor: None,
                graph_pool: None,
                stats: Arc::new(OnceLock::new()),
            },
            entries,
        )
    }

    /// Attach a `StatsReporter` after construction.
    /// Must be called before the service receives any requests.
    /// No-op if called more than once (OnceLock semantics).
    pub fn with_stats(self, reporter: Arc<StatsReporter>) -> Self {
        let _ = self.stats.set(reporter);
        self
    }

    /// Initialize the stats reporter from an external caller (e.g., AppState::init_auth_pool).
    /// Safe to call concurrently; OnceLock guarantees only the first call wins.
    pub fn init_stats(&self, reporter: Arc<StatsReporter>) {
        let _ = self.stats.set(reporter);
    }

    #[inline]
    fn report(&self, event: StatsEvent) {
        if let Some(r) = self.stats.get() {
            r.report(event);
        }
    }

    /// Force-flush all buffered edit-log entries to the store.
    pub async fn flush_edit_log(&self) {
        let flush_tx = {
            let guard = self.edit_log.lock().unwrap();
            guard.as_ref().map(|buf| buf.flush_tx.clone())
        };
        if let Some(tx) = flush_tx {
            let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
            if tx.send(ack_tx).await.is_ok() {
                let _ = ack_rx.await;
            }
        }
    }

    /// Drain edit-log: stop accepting new writes, flush remaining entries, with timeout.
    /// After this call, `send_edit_log` becomes a no-op and `SqlMemoryStore::log_edit`
    /// falls back to direct INSERT. Returns false if drain failed (timeout or panic).
    pub async fn drain_edit_log(&self) -> bool {
        // 1. Clear sender in SqlMemoryStore (and all background pool clones sharing the Arc)
        //    so governance scheduler's log_edit falls back to direct INSERT.
        if let Some(sql) = &self.sql_store {
            sql.clear_edit_log_tx();
        }
        // 2. Take the buffer (makes send_edit_log a no-op) and drain it.
        let buf = self.edit_log.lock().unwrap().take();
        match buf {
            Some(buf) => buf.drain(EDIT_LOG_DRAIN_TIMEOUT).await,
            None => true,
        }
    }

    /// Non-blocking: enqueue an edit-log entry for async batched write.
    pub fn send_edit_log(
        &self,
        user_id: &str,
        operation: &str,
        memory_id: Option<&str>,
        payload: Option<&str>,
        reason: &str,
        snapshot_before: Option<&str>,
    ) {
        if let Some(buf) = self.edit_log.lock().unwrap().as_ref() {
            buf.send(
                user_id,
                operation,
                memory_id,
                payload,
                reason,
                snapshot_before,
            );
        }
        self.report(StatsEvent::EditLogged {
            user_id: user_id.to_string(),
            operation: operation.to_string(),
        });
    }

    /// Best-effort cleanup of graph node + entity links for a deactivated memory.
    /// Used by correct, purge, and dedup-supersede paths.
    async fn cleanup_entity_data_for_memory(sql: &SqlMemoryStore, memory_id: &str, op: &str) {
        let graph = sql.graph_store();
        let (r1, r2, r3) = tokio::join!(
            graph.deactivate_by_memory_id(memory_id),
            graph.delete_memory_entity_links(memory_id),
            sql.delete_entity_links_by_memory_id(memory_id),
        );
        if let Err(e) = r1 {
            tracing::warn!(memory_id, error = %e, "{op}: failed to deactivate graph node");
        }
        if let Err(e) = r2 {
            tracing::warn!(memory_id, error = %e, "{op}: failed to delete entity links");
        }
        if let Err(e) = r3 {
            tracing::warn!(memory_id, error = %e, "{op}: failed to delete legacy entity links");
        }
    }

    /// Enqueue a memory for async entity extraction.
    /// Non-blocking: drops the job immediately if the queue is full.
    async fn enqueue_entity_extraction(&self, user_id: &str, memory_id: &str, content: &str) {
        if let Some(tx) = &self.entity_tx {
            let job = EntityJob {
                user_id: user_id.to_string(),
                memory_id: memory_id.to_string(),
                content: content.to_string(),
            };
            match tx.try_send(job) {
                Ok(()) => {}
                Err(tokio::sync::mpsc::error::TrySendError::Full(_job)) => {
                    // Drop immediately — never block user request for entity extraction
                    ENTITY_EXTRACTION_DROPS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    tracing::warn!(
                        memory_id,
                        queue_len = tx.max_capacity(),
                        "entity extraction queue full, job dropped — entities may be missing"
                    );
                }
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_job)) => {
                    ENTITY_EXTRACTION_DROPS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    tracing::error!(
                        memory_id,
                        "entity extraction channel closed — entities will be missing"
                    );
                }
            }
        }
    }

    /// Minimum content length to consider LLM entity extraction.
    const ENTITY_LLM_MIN_CONTENT_LEN: usize = 80;
    /// If regex extraction yields fewer entities than this, try LLM.
    const ENTITY_LLM_THRESHOLD: usize = 2;

    /// Spawn background task that drains the entity extraction queue.
    /// Max jobs to drain per micro-batch (prevents one worker from hogging the channel).
    const ENTITY_BATCH_LIMIT: usize = 64;

    fn spawn_entity_worker(
        rx: tokio::sync::mpsc::Receiver<EntityJob>,
        store: Arc<SqlMemoryStore>,
        llm: Option<Arc<LlmClient>>,
        stats_cell: Arc<OnceLock<Arc<StatsReporter>>>,
    ) {
        let worker_count: usize = std::env::var("ENTITY_WORKER_COUNT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(4)
            .clamp(1, 16);
        let permits = Arc::new(tokio::sync::Semaphore::new(worker_count));
        tokio::spawn(async move {
            let mut rx = rx;
            loop {
                let Some(first) = rx.recv().await else {
                    break;
                };
                let mut batch = Vec::with_capacity(Self::ENTITY_BATCH_LIMIT);
                batch.push(first);
                while batch.len() < Self::ENTITY_BATCH_LIMIT {
                    match rx.try_recv() {
                        Ok(job) => batch.push(job),
                        Err(_) => break,
                    }
                }

                let batch_len = batch.len();
                let mut by_user: std::collections::HashMap<String, Vec<EntityJob>> =
                    std::collections::HashMap::new();
                for job in batch {
                    by_user.entry(job.user_id.clone()).or_default().push(job);
                }
                let user_count = by_user.len();

                for (user_id, jobs) in by_user {
                    let Ok(permit) = permits.clone().acquire_owned().await else {
                        tracing::debug!("entity worker semaphore closed");
                        return;
                    };
                    let store = Arc::clone(&store);
                    let llm = llm.clone();
                    let stats = stats_cell.get().cloned();
                    tokio::spawn(async move {
                        let _permit = permit;
                        Self::process_entity_batch(store, llm, stats, user_id, jobs).await;
                    });
                }

                if batch_len > 1 {
                    tracing::debug!(batch_len, users = user_count, "micro-batch dispatched");
                }
            }
            tracing::debug!("entity dispatcher exiting");
        });
        tracing::info!(worker_count, "entity extraction dispatcher started");
    }

    /// Process a batch of jobs for the same user: merge all regex entities into
    /// one bulk upsert, then run LLM extraction individually for qualifying jobs.
    async fn process_entity_batch(
        store: Arc<SqlMemoryStore>,
        llm: Option<Arc<LlmClient>>,
        stats: Option<Arc<StatsReporter>>,
        user_id: String,
        jobs: Vec<EntityJob>,
    ) {
        // Sanity check: all jobs must belong to the same user
        if cfg!(debug_assertions) {
            debug_assert!(
                jobs.iter().all(|j| j.user_id == user_id),
                "all jobs must belong to the same user_id"
            );
        } else if let Some(bad) = jobs.iter().find(|j| j.user_id != user_id) {
            tracing::error!(
                expected = user_id,
                actual = bad.user_id,
                memory_id = bad.memory_id,
                "entity batch contains mismatched user_id — skipping batch"
            );
            return;
        }
        let user_store = if let Some(router) = store.db_router() {
            match router.routed_store_for_user(&user_id) {
                Ok(user_store) => Some(user_store),
                Err(e) => {
                    tracing::warn!(
                        user_id,
                        error = %e,
                        "failed to route entity extraction to user store"
                    );
                    return;
                }
            }
        } else {
            None
        };
        let graph = user_store.as_ref().unwrap_or(store.as_ref()).graph_store();

        // 1. Regex extraction — collect entities with their memory_id inline
        struct ExtractedEntity {
            memory_id: String,
            name: String,
            display: String,
            entity_type: String,
        }
        let mut extracted: Vec<ExtractedEntity> = Vec::new();
        let mut job_entity_counts: Vec<usize> = Vec::new(); // count per job for LLM decision

        for job in &jobs {
            let entities = memoria_storage::extract_entities(&job.content);
            job_entity_counts.push(entities.len());
            for ent in entities {
                extracted.push(ExtractedEntity {
                    memory_id: job.memory_id.clone(),
                    name: ent.name,
                    display: ent.display,
                    entity_type: ent.entity_type,
                });
            }
        }

        // Batch upsert all entities in one call (deduplicate by name to avoid
        // wasted UUIDs in INSERT IGNORE — links are built from extracted, not refs)
        if !extracted.is_empty() {
            let mut seen = std::collections::HashSet::new();
            let refs: Vec<(&str, &str, &str)> = extracted
                .iter()
                .filter(|e| seen.insert(e.name.as_str()))
                .map(|e| (e.name.as_str(), e.display.as_str(), e.entity_type.as_str()))
                .collect();
            let refs_len = refs.len() as u64;
            if let Ok(resolved) = graph.batch_upsert_entities(&user_id, &refs).await {
                // Report approximate new-entity count (best-effort; refs_len may overcount
                // if some entities already existed, which is acceptable for monitoring).
                if refs_len > 0 {
                    if let Some(r) = &stats {
                        r.report(StatsEvent::EntitiesUpserted {
                            user_id: user_id.clone(),
                            count: refs_len,
                        });
                    }
                }
                // Build name→entity_id map
                let id_map: std::collections::HashMap<&str, &str> = resolved
                    .iter()
                    .map(|(n, eid)| (n.as_str(), eid.as_str()))
                    .collect();
                // Build links using the inline memory_id
                let links: Vec<(&str, &str, &str)> = extracted
                    .iter()
                    .filter_map(|e| {
                        id_map
                            .get(e.name.as_str())
                            .map(|eid| (e.memory_id.as_str(), *eid, "regex"))
                    })
                    .collect();
                if !links.is_empty() {
                    let _ = graph
                        .batch_upsert_memory_entity_links(&user_id, &links)
                        .await;
                }
            }
        }

        // 2. LLM extraction for qualifying jobs (few regex entities + long content)
        if let Some(llm) = llm.as_ref() {
            for (job, &entity_count) in jobs.iter().zip(&job_entity_counts) {
                if entity_count < Self::ENTITY_LLM_THRESHOLD
                    && job.content.len() >= Self::ENTITY_LLM_MIN_CONTENT_LEN
                {
                    Self::llm_extract_entities(llm, &graph, &user_id, &job.memory_id, &job.content)
                        .await;
                }
            }
        }
    }

    /// Run LLM entity extraction for a single memory and link results.
    async fn llm_extract_entities(
        llm: &LlmClient,
        graph: &memoria_storage::graph::GraphStore,
        user_id: &str,
        memory_id: &str,
        content: &str,
    ) {
        let prompt = format!(
            "Extract named entities from the following text. Return a JSON array of objects.\n\
             Each object: {{\"name\": \"canonical name\", \"type\": \"tech|person|repo|project|concept\"}}\n\
             Rules: only specific named entities, max 10, deduplicate.\n\nText:\n{}\n\nJSON array:",
            memoria_core::truncate_utf8(content, 2000)
        );
        let msgs = vec![ChatMessage {
            role: "user".into(),
            content: prompt,
        }];
        let raw = match llm.chat(&msgs, 0.0, Some(300)).await {
            Ok(r) => r,
            Err(e) => {
                warn!(error = %e, "entity worker LLM extraction failed");
                return;
            }
        };
        let start = raw.find('[').unwrap_or(raw.len());
        let end = raw.rfind(']').map(|i| i + 1).unwrap_or(raw.len());
        if start >= end {
            return;
        }
        let items: Vec<serde_json::Value> = match serde_json::from_str(&raw[start..end]) {
            Ok(v) => v,
            Err(_) => return,
        };
        // Collect parsed entities for batch upsert
        let mut batch: Vec<(String, String, String)> = Vec::new();
        for item in &items {
            let name = item["name"].as_str().unwrap_or("").trim().to_lowercase();
            if name.is_empty() {
                continue;
            }
            let display = item["name"].as_str().unwrap_or("").trim().to_string();
            let etype = item["type"].as_str().unwrap_or("concept").to_string();
            batch.push((name, display, etype));
        }
        if !batch.is_empty() {
            let refs: Vec<(&str, &str, &str)> = batch
                .iter()
                .map(|(n, d, t)| (n.as_str(), d.as_str(), t.as_str()))
                .collect();
            if let Ok(resolved) = graph.batch_upsert_entities(user_id, &refs).await {
                let links: Vec<(&str, &str, &str)> = resolved
                    .iter()
                    .map(|(_name, eid)| (memory_id, eid.as_str(), "llm"))
                    .collect();
                let _ = graph
                    .batch_upsert_memory_entity_links(user_id, &links)
                    .await;
            }
        }
    }

    #[allow(dead_code)]
    async fn active_table(&self, user_id: &str) -> String {
        match &self.sql_store {
            Some(s) => s
                .active_table(user_id)
                .await
                .unwrap_or_else(|_| "mem_memories".to_string()),
            None => "mem_memories".to_string(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    #[tracing::instrument(skip(self, content), fields(user_id))]
    pub async fn store_memory(
        &self,
        user_id: &str,
        content: &str,
        memory_type: MemoryType,
        session_id: Option<String>,
        trust_tier: Option<TrustTier>,
        observed_at: Option<DateTime<Utc>>,
        initial_confidence: Option<f64>,
    ) -> Result<Memory, MemoriaError> {
        let t0 = std::time::Instant::now();
        // Sensitivity check — block HIGH tier, redact MEDIUM tier
        let sensitivity = check_sensitivity(content);
        if sensitivity.blocked {
            return Err(MemoriaError::Blocked(format!(
                "Memory blocked: contains sensitive content ({})",
                sensitivity.matched_labels.join(", ")
            )));
        }
        let content = sensitivity.redacted_content.as_deref().unwrap_or(content);

        let effective_tier = trust_tier.unwrap_or(TrustTier::T1Verified);
        let embedding = self.embed(content).await?;
        let t_embed = t0.elapsed();
        let memory = Memory {
            memory_id: Uuid::now_v7().simple().to_string(),
            user_id: user_id.to_string(),
            memory_type,
            content: content.to_string(),
            initial_confidence: initial_confidence
                .unwrap_or_else(|| effective_tier.initial_confidence()),
            embedding,
            source_event_ids: vec![],
            superseded_by: None,
            is_active: true,
            access_count: 0,
            session_id,
            observed_at: Some(observed_at.unwrap_or_else(Utc::now)),
            created_at: None,
            updated_at: None,
            extra_metadata: None,
            trust_tier: effective_tier,
            retrieval_score: None,
        };
        // Dedup: if embedding exists, check for near-duplicate and supersede
        // TODO(concurrency): race between dedup check and insert can create duplicates
        if self.sql_store.is_some() {
            let sql = self.user_sql_store(user_id).await?;
            let table = sql.active_table(user_id).await?;
            if let Some(ref emb) = memory.embedding {
                // L2 threshold from cosine similarity 0.95: sqrt(2*(1-0.95)) ≈ 0.3162
                // Only supersede near-identical memories, not contradictions.
                // Assumes normalized embeddings (bge-m3, text-embedding-3-* all output unit vectors).
                let l2_threshold = 0.3162;
                let mtype = memory.memory_type.to_string();
                let t1 = std::time::Instant::now();
                if let Ok(Some((old_id, old_content, _dist))) = sql
                    .find_near_duplicate(
                        &table,
                        user_id,
                        emb,
                        &mtype,
                        &memory.memory_id,
                        l2_threshold,
                    )
                    .await
                {
                    let t_dedup = t1.elapsed();
                    if old_content.trim() != memory.content.trim() {
                        let t2 = std::time::Instant::now();
                        sql.insert_into(&table, &memory).await?;
                        let t_insert = t2.elapsed();
                        // Supersede + cleanup are independent — run in parallel
                        let (sup_res, _) = tokio::join!(
                            sql.supersede_memory(&table, &old_id, &memory.memory_id),
                            Self::cleanup_entity_data_for_memory(
                                sql.as_ref(),
                                &old_id,
                                "supersede",
                            ),
                        );
                        sup_res?;
                        let payload = serde_json::json!({"content": &memory.content, "type": memory.memory_type.to_string()}).to_string();
                        self.send_edit_log(
                            user_id,
                            "inject",
                            Some(&memory.memory_id),
                            Some(&payload),
                            "store_memory:supersede",
                            None,
                        );
                        self.report(StatsEvent::MemoryStored {
                            user_id: user_id.to_string(),
                            memory_type: memory.memory_type.to_string(),
                            trust_tier: memory.trust_tier.to_string(),
                        });
                        self.report(StatsEvent::MemoryDeactivated {
                            user_id: user_id.to_string(),
                        });
                        self.enqueue_entity_extraction(user_id, &memory.memory_id, &memory.content)
                            .await;
                        if t0.elapsed().as_secs() >= 1 {
                            tracing::warn!(
                                embed_ms = t_embed.as_millis() as u64,
                                dedup_ms = t_dedup.as_millis() as u64,
                                insert_ms = t_insert.as_millis() as u64,
                                total_ms = t0.elapsed().as_millis() as u64,
                                "store_memory slow (supersede)"
                            );
                        };
                        return Ok(memory);
                    }
                    // Same content — skip storing duplicate
                    if t0.elapsed().as_secs() >= 1 {
                        tracing::warn!(
                            embed_ms = t_embed.as_millis() as u64,
                            dedup_ms = t_dedup.as_millis() as u64,
                            total_ms = t0.elapsed().as_millis() as u64,
                            "store_memory slow (skip dup)"
                        );
                    };
                    return Ok(memory);
                }
                let t_dedup = t1.elapsed();
                let t2 = std::time::Instant::now();
                sql.insert_into(&table, &memory).await?;
                let t_insert = t2.elapsed();
                let payload = serde_json::json!({"content": &memory.content, "type": memory.memory_type.to_string()}).to_string();
                self.send_edit_log(
                    user_id,
                    "inject",
                    Some(&memory.memory_id),
                    Some(&payload),
                    "store_memory",
                    None,
                );
                self.report(StatsEvent::MemoryStored {
                    user_id: user_id.to_string(),
                    memory_type: memory.memory_type.to_string(),
                    trust_tier: memory.trust_tier.to_string(),
                });
                self.enqueue_entity_extraction(user_id, &memory.memory_id, &memory.content)
                    .await;
                if t0.elapsed().as_secs() >= 1 {
                    tracing::warn!(
                        embed_ms = t_embed.as_millis() as u64,
                        dedup_ms = t_dedup.as_millis() as u64,
                        insert_ms = t_insert.as_millis() as u64,
                        total_ms = t0.elapsed().as_millis() as u64,
                        "store_memory slow"
                    );
                };
            } else {
                sql.insert_into(&table, &memory).await?;
                let payload = serde_json::json!({"content": &memory.content, "type": memory.memory_type.to_string()}).to_string();
                self.send_edit_log(
                    user_id,
                    "inject",
                    Some(&memory.memory_id),
                    Some(&payload),
                    "store_memory",
                    None,
                );
                self.report(StatsEvent::MemoryStored {
                    user_id: user_id.to_string(),
                    memory_type: memory.memory_type.to_string(),
                    trust_tier: memory.trust_tier.to_string(),
                });
                self.enqueue_entity_extraction(user_id, &memory.memory_id, &memory.content)
                    .await;
                if t0.elapsed().as_secs() >= 1 {
                    tracing::warn!(
                        embed_ms = t_embed.as_millis() as u64,
                        total_ms = t0.elapsed().as_millis() as u64,
                        "store_memory slow (no embedding)"
                    );
                };
            }
        } else {
            self.store.insert(&memory).await?;
        }
        Ok(memory)
    }

    /// Validate candidate memories in a zero-copy branch before committing.
    /// Returns true if branch retrieval score >= main (or if validation fails — fail open).
    /// The branch is always dropped after validation.
    pub async fn validate_in_sandbox(
        &self,
        user_id: &str,
        candidates: &[Memory],
        query: &str,
        git: &memoria_git::GitForDataService,
    ) -> bool {
        let sql = match &self.sql_store {
            Some(s) => s,
            None => return true, // no SQL store — skip sandbox
        };
        if candidates.is_empty() {
            return true;
        }

        let branch = format!("mem_sandbox_{}", &Uuid::new_v4().simple().to_string()[..16]);

        // Create branch (zero-copy of mem_memories)
        if git.create_branch(&branch, "mem_memories").await.is_err() {
            return true; // fail open
        }

        let result = async {
            // Insert candidates into branch
            for m in candidates {
                sql.insert_into(&branch, m).await?;
            }
            // Score main vs branch (top-5 fulltext score as proxy)
            let main_results = sql
                .search_fulltext_from("mem_memories", user_id, query, 5)
                .await
                .unwrap_or_default();
            let branch_results = sql
                .search_fulltext_from(&branch, user_id, query, 5)
                .await
                .unwrap_or_default();

            let score = |mems: &[Memory]| -> f64 {
                if mems.is_empty() {
                    return 0.0;
                }
                mems.iter()
                    .map(|m| m.retrieval_score.unwrap_or(0.5))
                    .sum::<f64>()
                    / mems.len() as f64
            };
            Ok::<bool, MemoriaError>(score(&branch_results) >= score(&main_results))
        }
        .await;

        // Always drop branch
        let _ = git.drop_branch(&branch).await;

        result.unwrap_or(true) // fail open on error
    }

    pub async fn retrieve(
        &self,
        user_id: &str,
        query: &str,
        top_k: i64,
    ) -> Result<Vec<Memory>, MemoriaError> {
        self.retrieve_with_options(user_id, query, top_k, &RetrieveOptions::default())
            .await
    }

    pub async fn retrieve_with_options(
        &self,
        user_id: &str,
        query: &str,
        top_k: i64,
        options: &RetrieveOptions,
    ) -> Result<Vec<Memory>, MemoriaError> {
        let (mems, _) = self
            .retrieve_inner(user_id, query, top_k, ExplainLevel::None, options)
            .await?;
        self.bump_access_counts(&mems);
        Ok(mems)
    }

    /// Retrieve with explain stats at the given level.
    pub async fn retrieve_explain(
        &self,
        user_id: &str,
        query: &str,
        top_k: i64,
    ) -> Result<(Vec<Memory>, RetrievalExplain), MemoriaError> {
        let (mems, explain) = self
            .retrieve_inner(
                user_id,
                query,
                top_k,
                ExplainLevel::Basic,
                &RetrieveOptions::default(),
            )
            .await?;
        self.bump_access_counts(&mems);
        Ok((mems, explain))
    }

    /// Retrieve with explicit explain level (none/basic/verbose/analyze).
    pub async fn retrieve_explain_level(
        &self,
        user_id: &str,
        query: &str,
        top_k: i64,
        level: ExplainLevel,
    ) -> Result<(Vec<Memory>, RetrievalExplain), MemoriaError> {
        self.retrieve_explain_level_with_options(
            user_id,
            query,
            top_k,
            level,
            &RetrieveOptions::default(),
        )
        .await
    }

    pub async fn retrieve_explain_level_with_options(
        &self,
        user_id: &str,
        query: &str,
        top_k: i64,
        level: ExplainLevel,
        options: &RetrieveOptions,
    ) -> Result<(Vec<Memory>, RetrievalExplain), MemoriaError> {
        let start = std::time::Instant::now();
        let (mems, explain) = self
            .retrieve_inner(user_id, query, top_k, level, options)
            .await?;
        self.bump_access_counts(&mems);

        // 记录查询到 vector monitor（轻量级，无阻塞）
        if let Some(monitor) = &self.vector_monitor {
            let elapsed_ms = start.elapsed().as_millis() as u64;
            monitor.record_query(elapsed_ms, mems.len());
        }

        Ok((mems, explain))
    }

    /// Fire-and-forget bump of access counts for retrieved memories.
    fn bump_access_counts(&self, mems: &[Memory]) {
        if let Some(counter) = &self.access_counter {
            let ids: Vec<(String, String)> = mems
                .iter()
                .map(|m| (m.user_id.clone(), m.memory_id.clone()))
                .collect();
            counter.bump(&ids);
        }
    }

    #[tracing::instrument(skip(self), fields(user_id, top_k))]
    async fn retrieve_inner(
        &self,
        user_id: &str,
        query: &str,
        top_k: i64,
        level: ExplainLevel,
        options: &RetrieveOptions,
    ) -> Result<(Vec<Memory>, RetrievalExplain), MemoriaError> {
        let total_start = std::time::Instant::now();
        let mut explain = RetrievalExplain {
            level,
            ..Default::default()
        };

        if self.sql_store.is_some() {
            let sql = self.user_sql_store(user_id).await?;
            let table = sql.active_table(user_id).await?;
            let strict_session_id = options.strict_session_id();
            // Load per-user feedback_weight lazily — only when needed for scoring
            // (avoids extra DB query when fulltext fallback has no feedback to apply)

            // Phase 0: embed query
            let p0_start = std::time::Instant::now();
            let emb = self.embed(query).await.unwrap_or(None);
            explain.embedding_ms = p0_start.elapsed().as_secs_f64() * 1000.0;

            // Phase 1: graph retrieval (activation-based)
            if strict_session_id.is_none() {
                if let Some(ref embedding) = emb {
                    explain.graph_attempted = true;
                    let g_start = std::time::Instant::now();
                    // Use isolated graph pool to avoid starving main pool
                    let graph_sql = if self.db_router.is_some() {
                        sql.as_ref()
                    } else {
                        self.graph_pool.as_deref().unwrap_or(sql.as_ref())
                    };
                    let graph_store = graph_sql.graph_store();
                    let retriever = memoria_storage::graph::ActivationRetriever::new(&graph_store);
                    match retriever
                        .retrieve(user_id, query, embedding, top_k, None)
                        .await
                    {
                        Ok(scored_nodes) if !scored_nodes.is_empty() => {
                            explain.graph_ms = g_start.elapsed().as_secs_f64() * 1000.0;
                            explain.graph_hit = true;
                            explain.graph_candidates = scored_nodes.len();

                            // Convert graph nodes to Memory objects via batch fetch
                            let memory_ids: Vec<String> = scored_nodes
                                .iter()
                                .filter_map(|(n, _)| n.memory_id.clone())
                                .collect();
                            let tabular = if !memory_ids.is_empty() {
                                sql.get_by_ids(&memory_ids).await.unwrap_or_default()
                            } else {
                                Default::default()
                            };

                            let mut graph_memories: Vec<Memory> = Vec::new();
                            let mut seen = std::collections::HashSet::new();
                            for (node, score) in &scored_nodes {
                                if let Some(ref mid) = node.memory_id {
                                    if seen.insert(mid.clone()) {
                                        if let Some(mut mem) = tabular.get(mid).cloned() {
                                            mem.retrieval_score = Some(*score as f64);
                                            graph_memories.push(mem);
                                        }
                                    }
                                }
                            }

                            if graph_memories.len() as i64 >= top_k {
                                graph_memories.truncate(top_k as usize);
                                explain.path = "graph";
                                explain.result_count = graph_memories.len();
                                explain.total_ms = total_start.elapsed().as_secs_f64() * 1000.0;
                                return Ok((graph_memories, explain));
                            }

                            // Graph insufficient — supplement with hybrid
                            explain.vector_attempted = true;
                            let vs_start = std::time::Instant::now();
                            // Always use _scored directly with cached feedback_weight
                            // to avoid redundant get_user_retrieval_params query
                            let fw = self.get_feedback_weight(user_id).await?;
                            let (vec_results, scores) = sql
                                .search_hybrid_from_scored(
                                    &table, user_id, embedding, query, top_k, fw,
                                )
                                .await?;
                            explain.vector_ms = vs_start.elapsed().as_secs_f64() * 1000.0;
                            explain.vector_hit = !vec_results.is_empty();

                            // Merge: dedup (keep higher score), sort by score
                            for m in vec_results {
                                if seen.insert(m.memory_id.clone()) {
                                    graph_memories.push(m);
                                } else {
                                    // Memory exists from graph — use higher score
                                    if let Some(existing) = graph_memories
                                        .iter_mut()
                                        .find(|g| g.memory_id == m.memory_id)
                                    {
                                        if m.retrieval_score > existing.retrieval_score {
                                            existing.retrieval_score = m.retrieval_score;
                                        }
                                    }
                                }
                            }

                            graph_memories.sort_by(|a, b| {
                                b.retrieval_score
                                    .partial_cmp(&a.retrieval_score)
                                    .unwrap_or(std::cmp::Ordering::Equal)
                            });
                            graph_memories.truncate(top_k as usize);

                            if level.at_least(ExplainLevel::Verbose) {
                                explain.candidate_scores = scores
                                    .into_iter()
                                    .enumerate()
                                    .map(|(i, (id, vs, ks, ts, cs, fs))| CandidateScore {
                                        memory_id: id,
                                        rank: i + 1,
                                        final_score: round4(fs),
                                        vector_score: round4(vs),
                                        keyword_score: round4(ks),
                                        temporal_score: round4(ts),
                                        confidence_score: round4(cs),
                                    })
                                    .collect();
                            }
                            explain.path = "graph+vector";
                            explain.result_count = graph_memories.len();
                            explain.total_ms = total_start.elapsed().as_secs_f64() * 1000.0;
                            return Ok((graph_memories, explain));
                        }
                        Ok(_) => {
                            explain.graph_ms = g_start.elapsed().as_secs_f64() * 1000.0;
                            // Graph returned nothing — fall through to vector
                        }
                        Err(_) => {
                            explain.graph_ms = g_start.elapsed().as_secs_f64() * 1000.0;
                            // Graph failed — fall through to vector
                        }
                    }
                }
            }

            // Phase 2: vector search (fallback)
            if let Some(ref embedding) = emb {
                explain.vector_attempted = true;
                let vs_start = std::time::Instant::now();
                let fw = self.get_feedback_weight(user_id).await?;
                let (results, scores) = sql
                    .search_hybrid_from_scored_scoped(
                        &table,
                        user_id,
                        embedding,
                        query,
                        top_k,
                        fw,
                        strict_session_id,
                    )
                    .await?;
                explain.vector_ms = vs_start.elapsed().as_secs_f64() * 1000.0;
                if !results.is_empty() {
                    explain.vector_hit = true;
                    if level.at_least(ExplainLevel::Verbose) {
                        explain.candidate_scores = scores
                            .into_iter()
                            .enumerate()
                            .map(|(i, (id, vs, ks, ts, cs, fs))| CandidateScore {
                                memory_id: id,
                                rank: i + 1,
                                final_score: round4(fs),
                                vector_score: round4(vs),
                                keyword_score: round4(ks),
                                temporal_score: round4(ts),
                                confidence_score: round4(cs),
                            })
                            .collect();
                    }
                    explain.path = "hybrid";
                    explain.result_count = results.len();
                    explain.total_ms = total_start.elapsed().as_secs_f64() * 1000.0;
                    return Ok((results, explain));
                }
            }

            // Phase 3: fulltext fallback
            explain.fulltext_attempted = true;
            let ft_start = std::time::Instant::now();
            let mut results = sql
                .search_fulltext_from_scoped(&table, user_id, query, top_k, strict_session_id)
                .await?;
            explain.fulltext_ms = ft_start.elapsed().as_secs_f64() * 1000.0;
            explain.fulltext_hit = !results.is_empty();

            // Apply feedback adjustment to fulltext results
            if !results.is_empty() {
                let ids: Vec<String> = results.iter().map(|m| m.memory_id.clone()).collect();
                if let Ok(fb_map) = sql.get_feedback_batch(&ids).await {
                    let feedback_weight = self.get_feedback_weight(user_id).await?;
                    for m in &mut results {
                        if let Some(fb) = fb_map.get(&m.memory_id) {
                            let positive = fb.useful as f64;
                            let negative = (fb.irrelevant + fb.outdated + fb.wrong) as f64;
                            let feedback_delta = positive - 0.5 * negative;
                            if feedback_delta.abs() > 0.01 {
                                if let Some(score) = m.retrieval_score.as_mut() {
                                    *score *=
                                        (1.0 + feedback_weight * feedback_delta).clamp(0.5, 2.0);
                                }
                            }
                        }
                    }
                    // Re-sort after feedback adjustment
                    results.sort_by(|a, b| {
                        b.retrieval_score
                            .partial_cmp(&a.retrieval_score)
                            .unwrap_or(std::cmp::Ordering::Equal)
                    });
                }
            }

            explain.path = if explain.fulltext_hit {
                "fulltext"
            } else {
                "none"
            };
            if level.at_least(ExplainLevel::Verbose) {
                explain.candidate_scores = results
                    .iter()
                    .enumerate()
                    .map(|(i, m)| {
                        let fs = m.retrieval_score.unwrap_or(0.0);
                        CandidateScore {
                            memory_id: m.memory_id.clone(),
                            rank: i + 1,
                            final_score: round4(fs),
                            vector_score: 0.0,
                            keyword_score: round4(fs),
                            temporal_score: 0.0,
                            confidence_score: 0.0,
                        }
                    })
                    .collect();
            }
            explain.result_count = results.len();
            explain.total_ms = total_start.elapsed().as_secs_f64() * 1000.0;
            return Ok((results, explain));
        }

        // Fallback for tests (no sql_store)
        if let Some(emb) = self.embed(query).await.unwrap_or(None) {
            explain.vector_attempted = true;
            let results = self.store.search_vector(user_id, &emb, top_k).await?;
            if !results.is_empty() {
                explain.vector_hit = true;
                explain.path = "vector";
                explain.result_count = results.len();
                explain.total_ms = total_start.elapsed().as_secs_f64() * 1000.0;
                return Ok((results, explain));
            }
        }
        explain.fulltext_attempted = true;
        let results = self.store.search_fulltext(user_id, query, top_k).await?;
        explain.fulltext_hit = !results.is_empty();
        explain.path = if explain.fulltext_hit {
            "fulltext"
        } else {
            "none"
        };
        explain.result_count = results.len();
        explain.total_ms = total_start.elapsed().as_secs_f64() * 1000.0;
        Ok((results, explain))
    }

    pub async fn search(
        &self,
        user_id: &str,
        query: &str,
        top_k: i64,
    ) -> Result<Vec<Memory>, MemoriaError> {
        self.retrieve(user_id, query, top_k).await
    }

    pub async fn search_explain(
        &self,
        user_id: &str,
        query: &str,
        top_k: i64,
    ) -> Result<(Vec<Memory>, RetrievalExplain), MemoriaError> {
        self.retrieve_inner(
            user_id,
            query,
            top_k,
            ExplainLevel::Basic,
            &RetrieveOptions::default(),
        )
        .await
    }

    pub async fn search_explain_level(
        &self,
        user_id: &str,
        query: &str,
        top_k: i64,
        level: ExplainLevel,
    ) -> Result<(Vec<Memory>, RetrievalExplain), MemoriaError> {
        self.retrieve_inner(user_id, query, top_k, level, &RetrieveOptions::default())
            .await
    }

    // TODO(concurrency): concurrent correct on same memory_id can create duplicate
    // new memories. Consider SELECT FOR UPDATE or optimistic locking.
    pub async fn correct(
        &self,
        user_id: &str,
        memory_id: &str,
        new_content: &str,
    ) -> Result<Memory, MemoriaError> {
        // Sensitivity check — same as store_memory
        let sensitivity = check_sensitivity(new_content);
        if sensitivity.blocked {
            return Err(MemoriaError::Blocked(format!(
                "Memory blocked: contains sensitive content ({})",
                sensitivity.matched_labels.join(", ")
            )));
        }
        let new_content = sensitivity
            .redacted_content
            .as_deref()
            .unwrap_or(new_content);

        // Branch-aware: resolve table and fetch old memory from correct table
        if self.sql_store.is_some() {
            let sql = self.user_sql_store(user_id).await?;
            let table = sql.active_table(user_id).await?;
            let old = sql
                .get_from(&table, memory_id)
                .await?
                .ok_or_else(|| MemoriaError::NotFound(memory_id.to_string()))?;

            let new_id = Uuid::now_v7().simple().to_string();
            let new_mem = Memory {
                memory_id: new_id,
                user_id: old.user_id.clone(),
                content: new_content.to_string(),
                memory_type: old.memory_type.clone(),
                trust_tier: TrustTier::T2Curated,
                initial_confidence: old.initial_confidence,
                embedding: self.embed(new_content).await?,
                session_id: old.session_id.clone(),
                source_event_ids: vec![format!("correct:{}", memory_id)],
                extra_metadata: None,
                observed_at: Some(Utc::now()),
                created_at: Some(Utc::now()),
                updated_at: None,
                superseded_by: None,
                is_active: true,
                access_count: 0,
                retrieval_score: None,
            };

            sql.insert_into(&table, &new_mem).await?;
            // Supersede + cleanup are independent — run in parallel
            let (sup_res, _) = tokio::join!(
                sql.supersede_memory(&table, memory_id, &new_mem.memory_id),
                Self::cleanup_entity_data_for_memory(sql.as_ref(), memory_id, "correct"),
            );
            sup_res?;

            self.report(StatsEvent::MemoryStored {
                user_id: user_id.to_string(),
                memory_type: new_mem.memory_type.to_string(),
                trust_tier: new_mem.trust_tier.to_string(),
            });
            self.report(StatsEvent::MemoryDeactivated {
                user_id: user_id.to_string(),
            });

            let payload = serde_json::json!({
                "new_content": new_content,
                "new_memory_id": &new_mem.memory_id,
            })
            .to_string();
            self.send_edit_log(
                user_id,
                "correct",
                Some(memory_id),
                Some(&payload),
                "",
                None,
            );

            self.enqueue_entity_extraction(user_id, &new_mem.memory_id, new_content)
                .await;

            Ok(new_mem)
        } else {
            // Non-SQL fallback (tests with MockStore)
            let old = self
                .store
                .get(memory_id)
                .await?
                .ok_or_else(|| MemoriaError::NotFound(memory_id.to_string()))?;

            let new_id = Uuid::now_v7().simple().to_string();
            let new_mem = Memory {
                memory_id: new_id,
                user_id: old.user_id.clone(),
                content: new_content.to_string(),
                memory_type: old.memory_type.clone(),
                trust_tier: TrustTier::T2Curated,
                initial_confidence: old.initial_confidence,
                embedding: self.embed(new_content).await?,
                session_id: old.session_id.clone(),
                source_event_ids: vec![format!("correct:{}", memory_id)],
                extra_metadata: None,
                observed_at: Some(Utc::now()),
                created_at: Some(Utc::now()),
                updated_at: None,
                superseded_by: None,
                is_active: true,
                access_count: 0,
                retrieval_score: None,
            };

            self.store.insert(&new_mem).await?;
            self.store.soft_delete(memory_id).await?;
            let mut old_updated = old;
            old_updated.superseded_by = Some(new_mem.memory_id.clone());
            self.store.update(&old_updated).await?;
            Ok(new_mem)
        }
    }

    pub async fn purge(&self, user_id: &str, memory_id: &str) -> Result<PurgeResult, MemoriaError> {
        if self.sql_store.is_some() {
            let sql = self.user_sql_store(user_id).await?;
            let (snap, warning) = sql.create_safety_snapshot("purge").await;
            self.send_edit_log(user_id, "purge", Some(memory_id), None, "", snap.as_deref());
            let table = sql.active_table(user_id).await?;
            let deactivated = sql.soft_delete_from(&table, memory_id).await?;
            if deactivated > 0 {
                self.report(StatsEvent::MemoryDeactivated {
                    user_id: user_id.to_string(),
                });
            }
            // Graph + entity link cleanup (best-effort, governance fallback covers crash)
            Self::cleanup_entity_data_for_memory(sql.as_ref(), memory_id, "purge").await;
            Ok(PurgeResult {
                purged: 1,
                snapshot_name: snap,
                warning,
            })
        } else {
            self.store.soft_delete(memory_id).await?;
            Ok(PurgeResult {
                purged: 1,
                snapshot_name: None,
                warning: None,
            })
        }
    }

    /// Purge multiple memories by IDs with a single audit log entry.
    pub async fn purge_batch(
        &self,
        user_id: &str,
        ids: &[&str],
    ) -> Result<PurgeResult, MemoriaError> {
        if self.sql_store.is_some() {
            let sql = self.user_sql_store(user_id).await?;
            let (snap, warning) = sql.create_safety_snapshot("purge").await;
            let table = sql.active_table(user_id).await?;
            for id in ids {
                let deactivated = sql.soft_delete_from(&table, id).await?;
                self.send_edit_log(user_id, "purge", Some(id), None, "", snap.as_deref());
                if deactivated > 0 {
                    self.report(StatsEvent::MemoryDeactivated {
                        user_id: user_id.to_string(),
                    });
                }
                Self::cleanup_entity_data_for_memory(sql.as_ref(), id, "purge_batch").await;
            }
            Ok(PurgeResult {
                purged: ids.len(),
                snapshot_name: snap,
                warning,
            })
        } else {
            for id in ids {
                self.store.soft_delete(id).await?;
            }
            Ok(PurgeResult {
                purged: ids.len(),
                snapshot_name: None,
                warning: None,
            })
        }
    }

    /// Purge memories whose content contains `topic` (exact text match).
    pub async fn purge_by_topic(
        &self,
        user_id: &str,
        topic: &str,
    ) -> Result<PurgeResult, MemoriaError> {
        if self.sql_store.is_some() {
            let sql = self.user_sql_store(user_id).await?;
            let (snap, warning) = sql.create_safety_snapshot("purge").await;
            let table = sql.active_table(user_id).await?;
            let ids = sql.find_ids_by_topic(&table, user_id, topic).await?;
            if !ids.is_empty() {
                // Batch soft-delete in chunks
                sql.soft_delete_batch_from(&table, &ids).await?;
                // Batch entity cleanup
                sql.cleanup_entity_data_batch(&ids).await;
            }
            let reason = format!("topic:{topic}");
            for id in &ids {
                self.send_edit_log(
                    user_id,
                    "purge",
                    Some(id.as_str()),
                    None,
                    &reason,
                    snap.as_deref(),
                );
            }
            Ok(PurgeResult {
                purged: ids.len(),
                snapshot_name: snap,
                warning,
            })
        } else {
            Ok(PurgeResult {
                purged: 0,
                snapshot_name: None,
                warning: None,
            })
        }
    }

    pub async fn get(&self, memory_id: &str) -> Result<Option<Memory>, MemoriaError> {
        self.store.get(memory_id).await
    }

    /// Branch-aware get: resolves the user's active table before lookup.
    /// Use this when you have a user_id and need to find memories on branches.
    pub async fn get_for_user(
        &self,
        user_id: &str,
        memory_id: &str,
    ) -> Result<Option<Memory>, MemoriaError> {
        if self.sql_store.is_some() {
            let sql = self.user_sql_store(user_id).await?;
            let table = sql.active_table(user_id).await?;
            return sql.get_from(&table, memory_id).await;
        }
        self.store.get(memory_id).await
    }

    pub async fn list_active(
        &self,
        user_id: &str,
        limit: i64,
    ) -> Result<Vec<Memory>, MemoriaError> {
        if self.sql_store.is_some() {
            let sql = self.user_sql_store(user_id).await?;
            let table = sql.active_table(user_id).await?;
            return sql
                .list_active_lite(&table, user_id, limit, None, None)
                .await;
        }
        self.store.list_active(user_id, limit).await
    }

    /// Paginated list with optional memory_type filter and cursor-based keyset pagination.
    pub async fn list_active_paged(
        &self,
        user_id: &str,
        limit: i64,
        memory_type: Option<&str>,
        cursor: Option<&str>,
    ) -> Result<Vec<Memory>, MemoriaError> {
        if self.sql_store.is_some() {
            let sql = self.user_sql_store(user_id).await?;
            let table = sql.active_table(user_id).await?;
            return sql
                .list_active_lite(&table, user_id, limit, memory_type, cursor)
                .await;
        }
        // Fallback: trait path — no SQL store means no server-side filter/cursor.
        // Production always uses SQL store; this path is for trait-only test doubles.
        let mut mems = self.store.list_active(user_id, limit).await?;
        if let Some(mt) = memory_type {
            mems.retain(|m| m.memory_type.to_string() == mt);
        }
        if let Some(cursor_id) = cursor {
            mems.retain(|m| m.memory_id.as_str() < cursor_id);
        }
        Ok(mems)
    }

    pub async fn embed(&self, text: &str) -> Result<Option<Vec<f32>>, MemoriaError> {
        match self.embedder.as_ref() {
            None => Ok(None),
            Some(e) => match e.embed(text).await {
                Ok(v) => Ok(Some(v)),
                Err(err) => {
                    tracing::warn!(error = %err, "embedding failed");
                    Err(MemoriaError::Embedding(err.to_string()))
                }
            },
        }
    }

    pub async fn embed_batch(
        &self,
        texts: &[String],
    ) -> Result<Option<Vec<Vec<f32>>>, MemoriaError> {
        match self.embedder.as_ref() {
            None => Ok(None),
            Some(e) => match e.embed_batch(texts).await {
                Ok(v) => Ok(Some(v)),
                Err(err) => {
                    tracing::warn!(error = %err, "batch embedding failed");
                    Err(MemoriaError::Embedding(err.to_string()))
                }
            },
        }
    }

    /// Get per-user feedback_weight with caching (TTL 5 min).
    pub async fn get_feedback_weight(&self, user_id: &str) -> Result<f64, MemoriaError> {
        if let Some(fw) = self.feedback_weight_cache.get(user_id) {
            return Ok(fw);
        }
        let fw = if self.sql_store.is_some() {
            let sql = self.user_sql_store(user_id).await?;
            sql.get_user_retrieval_params(user_id)
                .await
                .map(|p| p.feedback_weight)
                .unwrap_or(0.1)
        } else {
            0.1
        };
        self.feedback_weight_cache.insert(user_id.to_string(), fw);
        Ok(fw)
    }

    /// Batch store with single embedding API call for all memories.
    pub async fn store_batch(
        &self,
        user_id: &str,
        items: Vec<(String, MemoryType, Option<String>, Option<TrustTier>)>,
    ) -> Result<Vec<Memory>, MemoriaError> {
        if items.is_empty() {
            return Ok(vec![]);
        }

        // Sensitivity check + collect contents
        let mut contents = Vec::with_capacity(items.len());
        let mut checked_items = Vec::with_capacity(items.len());
        for (content, mt, session_id, tier) in items {
            let sensitivity = check_sensitivity(&content);
            if sensitivity.blocked {
                return Err(MemoriaError::Blocked(format!(
                    "Memory blocked: contains sensitive content ({})",
                    sensitivity.matched_labels.join(", ")
                )));
            }
            let final_content = sensitivity.redacted_content.unwrap_or(content);
            contents.push(final_content.clone());
            checked_items.push((final_content, mt, session_id, tier));
        }

        // Batch embed
        let embeddings = self.embed_batch(&contents).await?;

        let mut results = Vec::with_capacity(checked_items.len());
        for (i, (content, mt, session_id, tier)) in checked_items.into_iter().enumerate() {
            let effective_tier = tier.unwrap_or(TrustTier::T1Verified);
            let embedding = embeddings.as_ref().map(|v| v[i].clone());
            let memory = Memory {
                memory_id: Uuid::now_v7().simple().to_string(),
                user_id: user_id.to_string(),
                memory_type: mt,
                content,
                initial_confidence: effective_tier.initial_confidence(),
                embedding,
                source_event_ids: vec![],
                superseded_by: None,
                is_active: true,
                access_count: 0,
                session_id,
                observed_at: Some(Utc::now()),
                created_at: None,
                updated_at: None,
                extra_metadata: None,
                trust_tier: effective_tier,
                retrieval_score: None,
            };
            results.push(memory);
        }
        if self.sql_store.is_some() {
            let sql = self.user_sql_store(user_id).await?;
            let table = sql.active_table(user_id).await?;
            let refs: Vec<&Memory> = results.iter().collect();
            sql.batch_insert_into(&table, &refs).await?;
            let payloads: Vec<String> = results
                .iter()
                .map(|m| {
                    serde_json::json!({"content": &m.content, "type": m.memory_type.to_string()})
                        .to_string()
                })
                .collect();
            for (m, p) in results.iter().zip(payloads.iter()) {
                self.send_edit_log(
                    user_id,
                    "inject",
                    Some(&m.memory_id),
                    Some(p),
                    "store_batch",
                    None,
                );
            }
        } else {
            for m in &results {
                self.store.insert(m).await?;
            }
        }
        Ok(results)
    }

    // ── TypedObserver: LLM-based memory extraction ──────────────────────────

    /// Extract and persist memories from a conversation turn.
    /// When LLM is configured, uses structured extraction (type, content, confidence).
    /// Falls back to storing raw assistant/user messages as semantic memories.
    pub async fn observe_turn(
        &self,
        user_id: &str,
        messages: &[serde_json::Value],
        session_id: Option<String>,
    ) -> Result<(Vec<Memory>, bool), MemoriaError> {
        let has_llm = self.llm.is_some();

        let candidates = if let Some(llm) = &self.llm {
            match self.extract_via_llm(llm, messages).await {
                Ok(ref items) if !items.is_empty() => {
                    info!(count = items.len(), "LLM extracted memory candidates");
                    self.build_candidates(user_id, items, session_id.clone())
                        .await
                }
                Ok(_) => vec![],
                Err(e) => {
                    warn!(error = %e, "LLM extraction failed, falling back to raw storage");
                    self.raw_candidates(user_id, messages, session_id.clone())
                }
            }
        } else {
            self.raw_candidates(user_id, messages, session_id.clone())
        };

        let mut stored = Vec::with_capacity(candidates.len());
        for mem in candidates {
            match self.persist_with_dedup(user_id, mem).await {
                Ok(m) => stored.push(m),
                Err(MemoriaError::Blocked(_)) => continue,
                Err(e) => return Err(e),
            }
        }
        info!(
            user_id,
            count = stored.len(),
            llm = has_llm,
            "observe_turn complete"
        );
        Ok((stored, has_llm))
    }

    /// Build Memory objects from LLM-extracted items.
    async fn build_candidates(
        &self,
        user_id: &str,
        items: &[serde_json::Value],
        session_id: Option<String>,
    ) -> Vec<Memory> {
        let now = Utc::now();
        let mut result = Vec::new();
        for item in items {
            let content = match item["content"].as_str() {
                Some(s) if !s.trim().is_empty() => s.trim(),
                _ => continue,
            };
            let sensitivity = check_sensitivity(content);
            if sensitivity.blocked {
                continue;
            }
            let content = sensitivity.redacted_content.as_deref().unwrap_or(content);

            let mtype = match item["type"].as_str().unwrap_or("semantic") {
                "profile" => MemoryType::Profile,
                "procedural" => MemoryType::Procedural,
                "episodic" => MemoryType::Episodic,
                _ => MemoryType::Semantic,
            };
            let confidence = item["confidence"]
                .as_f64()
                .map(|c| c.clamp(0.0, 1.0))
                .unwrap_or(0.7);

            result.push(Memory {
                memory_id: Uuid::now_v7().simple().to_string(),
                user_id: user_id.to_string(),
                memory_type: mtype,
                content: content.to_string(),
                initial_confidence: confidence,
                embedding: match self.embed(content).await {
                    Ok(e) => e,
                    Err(_) => continue,
                },
                source_event_ids: vec![],
                superseded_by: None,
                is_active: true,
                access_count: 0,
                session_id: session_id.clone(),
                observed_at: Some(now),
                created_at: None,
                updated_at: None,
                extra_metadata: None,
                trust_tier: TrustTier::T3Inferred,
                retrieval_score: None,
            });
        }
        result
    }

    /// Fallback: store raw assistant/user messages as semantic memories.
    fn raw_candidates(
        &self,
        user_id: &str,
        messages: &[serde_json::Value],
        session_id: Option<String>,
    ) -> Vec<Memory> {
        let now = Utc::now();
        messages
            .iter()
            .enumerate()
            .filter_map(|(i, msg)| {
                let role = msg["role"].as_str().unwrap_or("");
                let content = msg["content"].as_str().unwrap_or("").trim();
                if content.is_empty() || (role != "assistant" && role != "user") {
                    return None;
                }
                Some(Memory {
                    memory_id: Uuid::now_v7().simple().to_string(),
                    user_id: user_id.to_string(),
                    memory_type: MemoryType::Semantic,
                    content: content.to_string(),
                    initial_confidence: 0.7,
                    embedding: None, // will be embedded in persist_with_dedup
                    source_event_ids: vec![],
                    superseded_by: None,
                    is_active: true,
                    access_count: 0,
                    session_id: session_id.clone(),
                    observed_at: Some(now + chrono::Duration::milliseconds(i as i64)),
                    created_at: None,
                    updated_at: None,
                    extra_metadata: None,
                    trust_tier: TrustTier::T1Verified,
                    retrieval_score: None,
                })
            })
            .collect()
    }

    /// Persist a memory with dedup (near-duplicate detection + supersede).
    async fn persist_with_dedup(
        &self,
        user_id: &str,
        mut mem: Memory,
    ) -> Result<Memory, MemoriaError> {
        let sensitivity = check_sensitivity(&mem.content);
        if sensitivity.blocked {
            return Err(MemoriaError::Blocked(
                "blocked by sensitivity filter".into(),
            ));
        }
        if let Some(redacted) = &sensitivity.redacted_content {
            mem.content = redacted.clone();
        }
        if mem.embedding.is_none() {
            mem.embedding = self.embed(&mem.content).await?;
        }

        if self.sql_store.is_some() {
            let sql = self.user_sql_store(user_id).await?;
            let table = sql.active_table(user_id).await?;
            if let Some(ref emb) = mem.embedding {
                let l2_threshold = 0.3162;
                let mtype = mem.memory_type.to_string();
                if let Ok(Some((old_id, old_content, _))) = sql
                    .find_near_duplicate(&table, user_id, emb, &mtype, &mem.memory_id, l2_threshold)
                    .await
                {
                    if old_content.trim() != mem.content.trim() {
                        sql.insert_into(&table, &mem).await?;
                        sql.supersede_memory(&table, &old_id, &mem.memory_id)
                            .await?;
                        info!(old_id, new_id = %mem.memory_id, "superseded near-duplicate");
                        self.enqueue_entity_extraction(user_id, &mem.memory_id, &mem.content)
                            .await;
                        return Ok(mem);
                    }
                    return Ok(mem); // exact dup — skip
                }
            }
            sql.insert_into(&table, &mem).await?;
            self.enqueue_entity_extraction(user_id, &mem.memory_id, &mem.content)
                .await;
        } else {
            self.store.insert(&mem).await?;
        }
        Ok(mem)
    }

    const MAX_EXTRACT_MESSAGES: usize = 20;
    const MAX_EXTRACT_CHARS: usize = 6000;

    async fn extract_via_llm(
        &self,
        llm: &LlmClient,
        messages: &[serde_json::Value],
    ) -> Result<Vec<serde_json::Value>, MemoriaError> {
        let recent = if messages.len() > Self::MAX_EXTRACT_MESSAGES {
            &messages[messages.len() - Self::MAX_EXTRACT_MESSAGES..]
        } else {
            messages
        };
        let mut conv_text = String::new();
        for m in recent {
            let role = m["role"].as_str().unwrap_or("unknown");
            let content = m["content"].as_str().unwrap_or("");
            let truncated: String = content.chars().take(500).collect();
            if !truncated.is_empty() {
                conv_text.push_str(&format!("[{role}]: {truncated}\n"));
            }
        }
        // Trim to last MAX_EXTRACT_CHARS
        if conv_text.len() > Self::MAX_EXTRACT_CHARS {
            let start = conv_text.len() - Self::MAX_EXTRACT_CHARS;
            conv_text = conv_text[start..].to_string();
        }

        let result = llm
            .chat(
                &[
                    ChatMessage {
                        role: "system".into(),
                        content: OBSERVER_EXTRACTION_PROMPT.into(),
                    },
                    ChatMessage {
                        role: "user".into(),
                        content: conv_text,
                    },
                ],
                0.0,
                Some(2048),
            )
            .await
            .map_err(|e| MemoriaError::Internal(format!("LLM extraction: {e}")))?;

        parse_json_array(&result)
    }

    pub async fn user_sql_store(&self, user_id: &str) -> Result<Arc<SqlMemoryStore>, MemoriaError> {
        if let Some(router) = &self.db_router {
            router.user_store(user_id).await
        } else {
            self.sql_store
                .clone()
                .ok_or_else(|| MemoriaError::Internal("SQL store required".into()))
        }
    }

    pub fn shared_sql_store(&self) -> Result<Arc<SqlMemoryStore>, MemoriaError> {
        self.sql_store
            .clone()
            .ok_or_else(|| MemoriaError::Internal("SQL store required".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::mysql::MySqlPoolOptions;

    #[tokio::test]
    async fn test_new_sql_with_llm_disables_background_workers_without_database_url() {
        let pool = MySqlPoolOptions::new()
            .connect_lazy("mysql://root:root@localhost:3306/memoria")
            .expect("lazy mysql pool");
        let store = Arc::new(SqlMemoryStore::new(pool, 1024, "test-instance".to_string()));

        let service = MemoryService::new_sql_with_llm(store, None, None).await;

        assert!(service.entity_tx.is_none());
        assert!(service.vector_monitor.is_none());
    }
}

/// Parse a JSON array from LLM output, tolerating markdown fences.
fn parse_json_array(s: &str) -> Result<Vec<serde_json::Value>, MemoriaError> {
    let trimmed = s.trim();
    // Strip markdown code fences
    let json_str = if trimmed.starts_with("```") {
        let inner = trimmed
            .trim_start_matches("```json")
            .trim_start_matches("```");
        inner.trim_end_matches("```").trim()
    } else {
        trimmed
    };
    let arr: Vec<serde_json::Value> = serde_json::from_str(json_str)?;
    Ok(arr)
}

const OBSERVER_EXTRACTION_PROMPT: &str = r#"Extract structured memories from this conversation turn.
Return a JSON array ONLY, no other text. Each item:
{"type": "profile|semantic|procedural|episodic",
 "content": "concise factual statement",
 "confidence": 0.0-1.0}

Types (choose the MOST SPECIFIC type):
- profile: user identity, preferences, environment, habits, tools, language, role.
- semantic: general knowledge or facts NOT about the user themselves.
- procedural: repeated action patterns the user follows.
- episodic: what the user DID or ASKED ABOUT — activities, tasks, topics explored.

Confidence guide:
- 1.0: user explicitly stated
- 0.7: strongly implied by context
- 0.4: weakly inferred

Do NOT extract: greetings, pure meta-conversation.
If nothing worth remembering, return [].
"#;

// ── Feedback methods ──────────────────────────────────────────────────────────

impl MemoryService {
    /// Record explicit relevance feedback for a memory.
    /// signal: "useful" | "irrelevant" | "outdated" | "wrong"
    pub async fn record_feedback(
        &self,
        user_id: &str,
        memory_id: &str,
        signal: &str,
        context: Option<&str>,
    ) -> Result<String, MemoriaError> {
        let sql = self.user_sql_store(user_id).await?;
        sql.record_feedback(user_id, memory_id, signal, context)
            .await
    }

    /// Get feedback statistics for a user.
    pub async fn get_feedback_stats(
        &self,
        user_id: &str,
    ) -> Result<memoria_storage::FeedbackStats, MemoriaError> {
        let sql = self.user_sql_store(user_id).await?;
        sql.get_feedback_stats(user_id).await
    }

    /// Get feedback breakdown by trust tier.
    pub async fn get_feedback_by_tier(
        &self,
        user_id: &str,
    ) -> Result<Vec<memoria_storage::TierFeedback>, MemoriaError> {
        let sql = self.user_sql_store(user_id).await?;
        sql.get_feedback_by_tier(user_id).await
    }
}
