use chrono::{DateTime, Utc};
use memoria_core::{
    check_sensitivity,
    interfaces::{EmbeddingProvider, MemoryStore},
    MemoriaError, Memory, MemoryType, TrustTier,
};
use memoria_embedding::llm::ChatMessage;
use memoria_embedding::LlmClient;
use memoria_storage::SqlMemoryStore;
use memoria_storage::EditLogEntry;
use moka::future::Cache;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tracing::{info, warn};
use uuid::Uuid;

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
    pending: Arc<dashmap::DashMap<String, AtomicU64>>,
}

const ACCESS_FLUSH_INTERVAL: Duration = Duration::from_secs(5);

impl AccessCounter {
    fn new(store: Arc<SqlMemoryStore>) -> Self {
        let pending: Arc<dashmap::DashMap<String, AtomicU64>> = Arc::new(dashmap::DashMap::new());
        let p = pending.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(ACCESS_FLUSH_INTERVAL).await;
                Self::flush(&p, &store).await;
            }
        });
        Self { pending }
    }

    fn bump(&self, ids: &[String]) {
        for id in ids {
            self.pending
                .entry(id.clone())
                .or_insert_with(|| AtomicU64::new(0))
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    async fn flush(
        pending: &dashmap::DashMap<String, AtomicU64>,
        store: &SqlMemoryStore,
    ) {
        // Drain all entries
        let batch: Vec<(String, u64)> = pending
            .iter()
            .map(|e| (e.key().clone(), e.value().swap(0, Ordering::Relaxed)))
            .filter(|(_, n)| *n > 0)
            .collect();
        // Remove zeroed entries to avoid unbounded growth
        pending.retain(|_, v| v.load(Ordering::Relaxed) > 0);

        if batch.is_empty() {
            return;
        }
        if let Err(e) = store.bump_access_counts_batch(&batch).await {
            tracing::warn!("access counter flush failed: {e}");
        }
    }
}

pub struct MemoryService {
    /// Trait-based store for generic ops (used by tests with MockStore)
    pub store: Arc<dyn MemoryStore>,
    /// Concrete store for branch-aware ops (None in tests)
    pub sql_store: Option<Arc<SqlMemoryStore>>,
    pub embedder: Option<Arc<dyn EmbeddingProvider>>,
    /// LLM client for reflect/extract (None if LLM_API_KEY not set)
    pub llm: Option<Arc<LlmClient>>,
    /// Async entity extraction queue (None when sql_store is absent)
    entity_tx: Option<tokio::sync::mpsc::UnboundedSender<EntityJob>>,
    /// Batched access counter (None in tests)
    access_counter: Option<AccessCounter>,
    /// Per-user feedback_weight cache (TTL 5 min)
    feedback_weight_cache: Cache<String, f64>,
    /// Vector index monitor (None in tests)
    vector_monitor: Option<Arc<crate::vector_index_monitor::VectorIndexMonitor>>,
}

/// A pending entity-extraction job pushed from the write path.
struct EntityJob {
    user_id: String,
    memory_id: String,
    content: String,
}

impl MemoryService {
    /// Production constructor — uses SqlMemoryStore for branch support
    pub fn new_sql(
        store: Arc<SqlMemoryStore>,
        embedder: Option<Arc<dyn EmbeddingProvider>>,
    ) -> Self {
        let llm = LlmClient::from_env().map(Arc::new);
        Self::new_sql_with_llm(store, embedder, llm)
    }

    /// Production constructor with explicit LLM client.
    pub fn new_sql_with_llm(
        store: Arc<SqlMemoryStore>,
        embedder: Option<Arc<dyn EmbeddingProvider>>,
        llm: Option<Arc<LlmClient>>,
    ) -> Self {
        let (entity_tx, entity_rx) = tokio::sync::mpsc::unbounded_channel();
        
        // 启动 vector index monitor + rebuild worker
        let (rebuild_tx, rebuild_rx) = tokio::sync::mpsc::unbounded_channel();
        crate::vector_index_monitor::init_coarse_clock();
        let vector_monitor = Arc::new(crate::vector_index_monitor::VectorIndexMonitor::new(
            "mem_memories".to_string(),
            rebuild_tx,
        ));
        let worker = crate::rebuild_worker::RebuildWorker::new(store.clone(), rebuild_rx);
        tokio::spawn(async move { worker.run().await });
        
        let svc = Self {
            store: store.clone(),
            sql_store: Some(store.clone()),
            embedder,
            llm: llm.clone(),
            entity_tx: Some(entity_tx),
            access_counter: Some(AccessCounter::new(store.clone())),
            feedback_weight_cache: Cache::builder()
                .max_capacity(10_000)
                .time_to_live(Duration::from_secs(300))
                .build(),
            vector_monitor: Some(vector_monitor),
        };
        Self::spawn_entity_worker(entity_rx, store, llm);
        svc
    }

    /// Test constructor — any MemoryStore, no branch support
    pub fn new(store: Arc<dyn MemoryStore>, embedder: Option<Arc<dyn EmbeddingProvider>>) -> Self {
        Self {
            store,
            sql_store: None,
            embedder,
            llm: None,
            entity_tx: None,
            access_counter: None,
            feedback_weight_cache: Cache::builder()
                .max_capacity(10_000)
                .time_to_live(Duration::from_secs(300))
                .build(),
            vector_monitor: None,
        }
    }

    /// Enqueue a memory for async entity extraction (non-blocking).
    fn enqueue_entity_extraction(&self, user_id: &str, memory_id: &str, content: &str) {
        if let Some(tx) = &self.entity_tx {
            let _ = tx.send(EntityJob {
                user_id: user_id.to_string(),
                memory_id: memory_id.to_string(),
                content: content.to_string(),
            });
        }
    }

    /// Minimum content length to consider LLM entity extraction.
    const ENTITY_LLM_MIN_CONTENT_LEN: usize = 80;
    /// If regex extraction yields fewer entities than this, try LLM.
    const ENTITY_LLM_THRESHOLD: usize = 2;

    /// Spawn background task that drains the entity extraction queue.
    fn spawn_entity_worker(
        mut rx: tokio::sync::mpsc::UnboundedReceiver<EntityJob>,
        store: Arc<SqlMemoryStore>,
        llm: Option<Arc<LlmClient>>,
    ) {
        tokio::spawn(async move {
            while let Some(job) = rx.recv().await {
                let graph = store.graph_store();
                // 1. Regex extraction
                let entities = memoria_storage::extract_entities(&job.content);
                let mut links: Vec<(String, String, &str)> = Vec::new();
                for ent in &entities {
                    if let Ok((eid, _)) = graph
                        .upsert_entity(&job.user_id, &ent.name, &ent.display, &ent.entity_type)
                        .await
                    {
                        links.push((job.memory_id.clone(), eid, "regex"));
                    }
                }
                if !links.is_empty() {
                    let refs: Vec<(&str, &str, &str)> = links
                        .iter()
                        .map(|(m, e, s)| (m.as_str(), e.as_str(), *s))
                        .collect();
                    let _ = graph
                        .batch_upsert_memory_entity_links(&job.user_id, &refs)
                        .await;
                }
                // 2. Hybrid: if regex found few entities and content is long, try LLM
                if entities.len() < Self::ENTITY_LLM_THRESHOLD
                    && job.content.len() >= Self::ENTITY_LLM_MIN_CONTENT_LEN
                {
                    if let Some(ref llm) = llm {
                        Self::llm_extract_entities(
                            llm,
                            &graph,
                            &job.user_id,
                            &job.memory_id,
                            &job.content,
                        )
                        .await;
                    }
                }
            }
        });
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
        let mut links: Vec<(String, String, &str)> = Vec::new();
        for item in &items {
            let name = item["name"].as_str().unwrap_or("").trim().to_lowercase();
            if name.is_empty() {
                continue;
            }
            let display = item["name"].as_str().unwrap_or("").trim().to_string();
            let etype = item["type"].as_str().unwrap_or("concept").to_string();
            if let Ok((eid, _)) = graph.upsert_entity(user_id, &name, &display, &etype).await {
                links.push((memory_id.to_string(), eid, "llm"));
            }
        }
        if !links.is_empty() {
            let refs: Vec<(&str, &str, &str)> = links
                .iter()
                .map(|(m, e, s)| (m.as_str(), e.as_str(), *s))
                .collect();
            let _ = graph
                .batch_upsert_memory_entity_links(user_id, &refs)
                .await;
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
            memory_id: Uuid::new_v4().simple().to_string(),
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
        if let Some(sql) = &self.sql_store {
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
                        sql.supersede_memory(&table, &old_id, &memory.memory_id)
                            .await?;
                        let payload = serde_json::json!({"content": &memory.content, "type": memory.memory_type.to_string()}).to_string();
                        sql.log_edit(user_id, "inject", Some(&memory.memory_id), Some(&payload), "store_memory:supersede", None).await;
                        self.enqueue_entity_extraction(user_id, &memory.memory_id, &memory.content);
                        if t0.elapsed().as_secs() >= 1 { tracing::warn!(embed_ms = t_embed.as_millis() as u64, dedup_ms = t_dedup.as_millis() as u64, insert_ms = t_insert.as_millis() as u64, total_ms = t0.elapsed().as_millis() as u64, "store_memory slow (supersede)"); };
                        return Ok(memory);
                    }
                    // Same content — skip storing duplicate
                    if t0.elapsed().as_secs() >= 1 { tracing::warn!(embed_ms = t_embed.as_millis() as u64, dedup_ms = t_dedup.as_millis() as u64, total_ms = t0.elapsed().as_millis() as u64, "store_memory slow (skip dup)"); };
                    return Ok(memory);
                }
                let t_dedup = t1.elapsed();
                let t2 = std::time::Instant::now();
                sql.insert_into(&table, &memory).await?;
                let t_insert = t2.elapsed();
                let payload = serde_json::json!({"content": &memory.content, "type": memory.memory_type.to_string()}).to_string();
                sql.log_edit(user_id, "inject", Some(&memory.memory_id), Some(&payload), "store_memory", None).await;
                self.enqueue_entity_extraction(user_id, &memory.memory_id, &memory.content);
                if t0.elapsed().as_secs() >= 1 { tracing::warn!(embed_ms = t_embed.as_millis() as u64, dedup_ms = t_dedup.as_millis() as u64, insert_ms = t_insert.as_millis() as u64, total_ms = t0.elapsed().as_millis() as u64, "store_memory slow"); };
            } else {
                sql.insert_into(&table, &memory).await?;
                let payload = serde_json::json!({"content": &memory.content, "type": memory.memory_type.to_string()}).to_string();
                sql.log_edit(user_id, "inject", Some(&memory.memory_id), Some(&payload), "store_memory", None).await;
                self.enqueue_entity_extraction(user_id, &memory.memory_id, &memory.content);
                if t0.elapsed().as_secs() >= 1 { tracing::warn!(embed_ms = t_embed.as_millis() as u64, total_ms = t0.elapsed().as_millis() as u64, "store_memory slow (no embedding)"); };
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
        let (mems, _) = self
            .retrieve_inner(user_id, query, top_k, ExplainLevel::None)
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
            .retrieve_inner(user_id, query, top_k, ExplainLevel::Basic)
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
        let start = std::time::Instant::now();
        let (mems, explain) = self.retrieve_inner(user_id, query, top_k, level).await?;
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
            let ids: Vec<String> = mems.iter().map(|m| m.memory_id.clone()).collect();
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
    ) -> Result<(Vec<Memory>, RetrievalExplain), MemoriaError> {
        let total_start = std::time::Instant::now();
        let mut explain = RetrievalExplain {
            level,
            ..Default::default()
        };

        if let Some(sql) = &self.sql_store {
            let table = sql.active_table(user_id).await?;
            // Load per-user feedback_weight lazily — only when needed for scoring
            // (avoids extra DB query when fulltext fallback has no feedback to apply)

            // Phase 0: embed query
            let p0_start = std::time::Instant::now();
            let emb = self.embed(query).await.unwrap_or(None);
            explain.embedding_ms = p0_start.elapsed().as_secs_f64() * 1000.0;

            // Phase 1: graph retrieval (activation-based)
            if let Some(ref embedding) = emb {
                explain.graph_attempted = true;
                let g_start = std::time::Instant::now();
                let graph_store = sql.graph_store();
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
                        let (vec_results, scores) = if level.at_least(ExplainLevel::Verbose) {
                            let fw = self.get_feedback_weight(user_id).await;
                            sql.search_hybrid_from_scored(&table, user_id, embedding, query, top_k, fw)
                                .await?
                        } else {
                            (
                                sql.search_hybrid_from(&table, user_id, embedding, query, top_k)
                                    .await?,
                                vec![],
                            )
                        };
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

            // Phase 2: vector search (fallback)
            if let Some(ref embedding) = emb {
                explain.vector_attempted = true;
                let vs_start = std::time::Instant::now();
                let (results, scores) = if level.at_least(ExplainLevel::Verbose) {
                    let fw = self.get_feedback_weight(user_id).await;
                    sql.search_hybrid_from_scored(&table, user_id, embedding, query, top_k, fw)
                        .await?
                } else {
                    (
                        sql.search_hybrid_from(&table, user_id, embedding, query, top_k)
                            .await?,
                        vec![],
                    )
                };
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
                .search_fulltext_from(&table, user_id, query, top_k)
                .await?;
            explain.fulltext_ms = ft_start.elapsed().as_secs_f64() * 1000.0;
            explain.fulltext_hit = !results.is_empty();

            // Apply feedback adjustment to fulltext results
            if !results.is_empty() {
                let ids: Vec<String> = results.iter().map(|m| m.memory_id.clone()).collect();
                if let Ok(fb_map) = sql.get_feedback_batch(&ids).await {
                    let feedback_weight = self.get_feedback_weight(user_id).await;
                    for m in &mut results {
                        if let Some(fb) = fb_map.get(&m.memory_id) {
                            let positive = fb.useful as f64;
                            let negative = (fb.irrelevant + fb.outdated + fb.wrong) as f64;
                            let feedback_delta = positive - 0.5 * negative;
                            if feedback_delta.abs() > 0.01 {
                                if let Some(score) = m.retrieval_score.as_mut() {
                                    *score *= (1.0 + feedback_weight * feedback_delta).clamp(0.5, 2.0);
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
        self.retrieve_inner(user_id, query, top_k, ExplainLevel::Basic)
            .await
    }

    pub async fn search_explain_level(
        &self,
        user_id: &str,
        query: &str,
        top_k: i64,
        level: ExplainLevel,
    ) -> Result<(Vec<Memory>, RetrievalExplain), MemoriaError> {
        self.retrieve_inner(user_id, query, top_k, level).await
    }

    pub async fn correct(
        &self,
        memory_id: &str,
        new_content: &str,
    ) -> Result<Memory, MemoriaError> {
        let old = self
            .store
            .get(memory_id)
            .await?
            .ok_or_else(|| MemoriaError::NotFound(memory_id.to_string()))?;

        // Create new memory with corrected content (proper superseded_by chain)
        let new_id = Uuid::new_v4().simple().to_string();
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

        // Store new memory
        self.store.insert(&new_mem).await?;

        let user_id = old.user_id.clone();
        // Deactivate old and link to new via superseded_by
        // Only update superseded_by — avoid touching content to skip fulltext index rebuild
        self.store.soft_delete(memory_id).await?;
        if let Some(sql) = &self.sql_store {
            sqlx::query(
                "UPDATE mem_memories SET superseded_by = ?, updated_at = NOW() WHERE memory_id = ?"
            )
            .bind(&new_mem.memory_id)
            .bind(memory_id)
            .execute(sql.pool())
            .await
            .map_err(|e| MemoriaError::Database(e.to_string()))?;
        } else {
            let mut old_updated = old;
            old_updated.superseded_by = Some(new_mem.memory_id.clone());
            self.store.update(&old_updated).await?;
        }

        if let Some(sql) = &self.sql_store {
            let payload = serde_json::json!({
                "new_content": new_content,
                "new_memory_id": &new_mem.memory_id,
            }).to_string();
            sql.log_edit(&user_id, "correct", Some(memory_id), Some(&payload), "", None).await;
        }

        Ok(new_mem)
    }

    pub async fn purge(&self, user_id: &str, memory_id: &str) -> Result<PurgeResult, MemoriaError> {
        let (snap, warning) = if let Some(sql) = &self.sql_store {
            let (s, w) = sql.create_safety_snapshot("purge").await;
            sql.log_edit(user_id, "purge", Some(memory_id), None, "", s.as_deref()).await;
            (s, w)
        } else {
            (None, None)
        };
        self.store.soft_delete(memory_id).await?;
        Ok(PurgeResult {
            purged: 1,
            snapshot_name: snap,
            warning,
        })
    }

    /// Purge multiple memories by IDs with a single audit log entry.
    pub async fn purge_batch(
        &self,
        user_id: &str,
        ids: &[&str],
    ) -> Result<PurgeResult, MemoriaError> {
        let (snap, warning) = if let Some(sql) = &self.sql_store {
            sql.create_safety_snapshot("purge").await
        } else {
            (None, None)
        };
        for id in ids {
            self.store.soft_delete(id).await?;
        }
        if let Some(sql) = &self.sql_store {
            let entries: Vec<EditLogEntry<'_>> = ids
                .iter()
                .map(|id| (user_id, "purge", Some(*id), None, "", snap.as_deref()))
                .collect();
            sql.batch_log_edit(&entries).await;
        }
        Ok(PurgeResult {
            purged: ids.len(),
            snapshot_name: snap,
            warning,
        })
    }

    /// Purge memories whose content contains `topic` (exact text match).
    pub async fn purge_by_topic(
        &self,
        user_id: &str,
        topic: &str,
    ) -> Result<PurgeResult, MemoriaError> {
        if let Some(sql) = &self.sql_store {
            let (snap, warning) = sql.create_safety_snapshot("purge").await;
            let table = sql.active_table(user_id).await?;
            let ids = sql.find_ids_by_topic(&table, user_id, topic).await?;
            for id in &ids {
                self.store.soft_delete(id).await?;
                let _ = sql.graph_store().deactivate_by_memory_id(id).await;
            }
            let reason = format!("topic:{topic}");
            let entries: Vec<EditLogEntry<'_>> = ids
                .iter()
                .map(|id| (user_id, "purge", Some(id.as_str()), None, reason.as_str(), snap.as_deref()))
                .collect();
            sql.batch_log_edit(&entries).await;
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

    pub async fn list_active(
        &self,
        user_id: &str,
        limit: i64,
    ) -> Result<Vec<Memory>, MemoriaError> {
        if let Some(sql) = &self.sql_store {
            let table = sql.active_table(user_id).await?;
            return sql.list_active_from(&table, user_id, limit).await;
        }
        self.store.list_active(user_id, limit).await
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

    pub async fn embed_batch(&self, texts: &[String]) -> Result<Option<Vec<Vec<f32>>>, MemoriaError> {
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
    pub async fn get_feedback_weight(&self, user_id: &str) -> f64 {
        if let Some(fw) = self.feedback_weight_cache.get(user_id).await {
            return fw;
        }
        let fw = if let Some(sql) = &self.sql_store {
            sql.get_user_retrieval_params(user_id).await
                .map(|p| p.feedback_weight)
                .unwrap_or(0.1)
        } else {
            0.1
        };
        self.feedback_weight_cache.insert(user_id.to_string(), fw).await;
        fw
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
                memory_id: Uuid::new_v4().simple().to_string(),
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
        if let Some(sql) = &self.sql_store {
            let table = sql.active_table(user_id).await?;
            let refs: Vec<&Memory> = results.iter().collect();
            sql.batch_insert_into(&table, &refs).await?;
            let payloads: Vec<String> = results
                .iter()
                .map(|m| serde_json::json!({"content": &m.content, "type": m.memory_type.to_string()}).to_string())
                .collect();
            let log_entries: Vec<EditLogEntry<'_>> =
                results
                    .iter()
                    .zip(payloads.iter())
                    .map(|(m, p)| (user_id, "inject", Some(m.memory_id.as_str()), Some(p.as_str()), "store_batch", None))
                    .collect();
            sql.batch_log_edit(&log_entries).await;
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
                memory_id: Uuid::new_v4().simple().to_string(),
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
                    memory_id: Uuid::new_v4().simple().to_string(),
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

        if let Some(sql) = &self.sql_store {
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
                        self.enqueue_entity_extraction(user_id, &mem.memory_id, &mem.content);
                        return Ok(mem);
                    }
                    return Ok(mem); // exact dup — skip
                }
            }
            sql.insert_into(&table, &mem).await?;
            self.enqueue_entity_extraction(user_id, &mem.memory_id, &mem.content);
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
        let sql = self
            .sql_store
            .as_ref()
            .ok_or_else(|| MemoriaError::Internal("Feedback requires SQL store".into()))?;
        sql.record_feedback(user_id, memory_id, signal, context)
            .await
    }

    /// Get feedback statistics for a user.
    pub async fn get_feedback_stats(
        &self,
        user_id: &str,
    ) -> Result<memoria_storage::FeedbackStats, MemoriaError> {
        let sql = self
            .sql_store
            .as_ref()
            .ok_or_else(|| MemoriaError::Internal("Feedback requires SQL store".into()))?;
        sql.get_feedback_stats(user_id).await
    }

    /// Get feedback breakdown by trust tier.
    pub async fn get_feedback_by_tier(
        &self,
        user_id: &str,
    ) -> Result<Vec<memoria_storage::TierFeedback>, MemoriaError> {
        let sql = self
            .sql_store
            .as_ref()
            .ok_or_else(|| MemoriaError::Internal("Feedback requires SQL store".into()))?;
        sql.get_feedback_by_tier(user_id).await
    }
}
