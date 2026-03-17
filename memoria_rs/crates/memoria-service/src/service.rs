use memoria_core::{
    check_sensitivity,
    interfaces::{EmbeddingProvider, MemoryStore},
    Memory, MemoriaError, MemoryType, TrustTier,
};
use memoria_embedding::LlmClient;
use memoria_storage::SqlMemoryStore;
use std::sync::Arc;
use uuid::Uuid;
use chrono::{DateTime, Utc};

#[inline]
fn round4(v: f64) -> f64 { (v * 10000.0).round() / 10000.0 }

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
    pub path: &'static str,           // "vector", "fulltext", "graph", "graph+vector", "none"
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

pub struct MemoryService {
    /// Trait-based store for generic ops (used by tests with MockStore)
    pub store: Arc<dyn MemoryStore>,
    /// Concrete store for branch-aware ops (None in tests)
    pub sql_store: Option<Arc<SqlMemoryStore>>,
    pub embedder: Option<Arc<dyn EmbeddingProvider>>,
    /// LLM client for reflect/extract (None if LLM_API_KEY not set)
    pub llm: Option<Arc<LlmClient>>,
}

impl MemoryService {
    /// Production constructor — uses SqlMemoryStore for branch support
    pub fn new_sql(store: Arc<SqlMemoryStore>, embedder: Option<Arc<dyn EmbeddingProvider>>) -> Self {
        let llm = LlmClient::from_env().map(Arc::new);
        Self {
            store: store.clone(),
            sql_store: Some(store),
            embedder,
            llm,
        }
    }

    /// Production constructor with explicit LLM client.
    pub fn new_sql_with_llm(
        store: Arc<SqlMemoryStore>,
        embedder: Option<Arc<dyn EmbeddingProvider>>,
        llm: Option<Arc<LlmClient>>,
    ) -> Self {
        Self {
            store: store.clone(),
            sql_store: Some(store),
            embedder,
            llm,
        }
    }

    /// Test constructor — any MemoryStore, no branch support
    pub fn new(store: Arc<dyn MemoryStore>, embedder: Option<Arc<dyn EmbeddingProvider>>) -> Self {
        Self { store, sql_store: None, embedder, llm: None }
    }

    #[allow(dead_code)]
    async fn active_table(&self, user_id: &str) -> String {
        match &self.sql_store {
            Some(s) => s.active_table(user_id).await.unwrap_or_else(|_| "mem_memories".to_string()),
            None => "mem_memories".to_string(),
        }
    }

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
        let embedding = self.embed(content).await;
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
                if let Ok(Some((old_id, old_content, _dist))) = sql
                    .find_near_duplicate(&table, user_id, emb, &mtype, &memory.memory_id, l2_threshold)
                    .await
                {
                    if old_content.trim() != memory.content.trim() {
                        sql.insert_into(&table, &memory).await?;
                        sql.supersede_memory(&table, &old_id, &memory.memory_id).await?;
                        return Ok(memory);
                    }
                    // Same content — skip storing duplicate
                    return Ok(memory);
                }
            }
            sql.insert_into(&table, &memory).await?;
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
        if candidates.is_empty() { return true; }

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
            let main_results = sql.search_fulltext_from("mem_memories", user_id, query, 5).await.unwrap_or_default();
            let branch_results = sql.search_fulltext_from(&branch, user_id, query, 5).await.unwrap_or_default();

            let score = |mems: &[Memory]| -> f64 {
                if mems.is_empty() { return 0.0; }
                mems.iter().map(|m| m.retrieval_score.unwrap_or(0.5)).sum::<f64>() / mems.len() as f64
            };
            Ok::<bool, MemoriaError>(score(&branch_results) >= score(&main_results))
        }.await;

        // Always drop branch
        let _ = git.drop_branch(&branch).await;

        result.unwrap_or(true) // fail open on error
    }

    pub async fn retrieve(&self, user_id: &str, query: &str, top_k: i64) -> Result<Vec<Memory>, MemoriaError> {
        self.retrieve_inner(user_id, query, top_k, ExplainLevel::None).await.map(|(mems, _)| mems)
    }

    /// Retrieve with explain stats at the given level.
    pub async fn retrieve_explain(&self, user_id: &str, query: &str, top_k: i64) -> Result<(Vec<Memory>, RetrievalExplain), MemoriaError> {
        self.retrieve_inner(user_id, query, top_k, ExplainLevel::Basic).await
    }

    /// Retrieve with explicit explain level (none/basic/verbose/analyze).
    pub async fn retrieve_explain_level(&self, user_id: &str, query: &str, top_k: i64, level: ExplainLevel) -> Result<(Vec<Memory>, RetrievalExplain), MemoriaError> {
        self.retrieve_inner(user_id, query, top_k, level).await
    }

    async fn retrieve_inner(&self, user_id: &str, query: &str, top_k: i64, level: ExplainLevel) -> Result<(Vec<Memory>, RetrievalExplain), MemoriaError> {
        let total_start = std::time::Instant::now();
        let mut explain = RetrievalExplain { level, ..Default::default() };

        if let Some(sql) = &self.sql_store {
            let table = sql.active_table(user_id).await?;

            // Phase 0: embed query
            let p0_start = std::time::Instant::now();
            let emb = self.embed(query).await;
            explain.embedding_ms = p0_start.elapsed().as_secs_f64() * 1000.0;

            // Phase 1: graph retrieval (activation-based)
            if let Some(ref embedding) = emb {
                explain.graph_attempted = true;
                let g_start = std::time::Instant::now();
                let graph_store = sql.graph_store();
                let retriever = memoria_storage::graph::ActivationRetriever::new(&graph_store);
                match retriever.retrieve(user_id, query, embedding, top_k, None).await {
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
                            sql.search_hybrid_from_scored(&table, user_id, embedding, query, top_k).await?
                        } else {
                            (sql.search_hybrid_from(&table, user_id, embedding, query, top_k).await?, vec![])
                        };
                        explain.vector_ms = vs_start.elapsed().as_secs_f64() * 1000.0;
                        explain.vector_hit = !vec_results.is_empty();

                        // Merge: dedup, sort by score
                        for m in vec_results {
                            if seen.insert(m.memory_id.clone()) {
                                graph_memories.push(m);
                            }
                        }
                        graph_memories.sort_by(|a, b| {
                            b.retrieval_score
                                .partial_cmp(&a.retrieval_score)
                                .unwrap_or(std::cmp::Ordering::Equal)
                        });
                        graph_memories.truncate(top_k as usize);

                        if level.at_least(ExplainLevel::Verbose) {
                            explain.candidate_scores = scores.into_iter().enumerate()
                                .map(|(i, (id, vs, ks, ts, cs, fs))| CandidateScore {
                                    memory_id: id, rank: i + 1,
                                    final_score: round4(fs), vector_score: round4(vs),
                                    keyword_score: round4(ks), temporal_score: round4(ts),
                                    confidence_score: round4(cs),
                                }).collect();
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
                    sql.search_hybrid_from_scored(&table, user_id, embedding, query, top_k).await?
                } else {
                    (sql.search_hybrid_from(&table, user_id, embedding, query, top_k).await?, vec![])
                };
                explain.vector_ms = vs_start.elapsed().as_secs_f64() * 1000.0;
                if !results.is_empty() {
                    explain.vector_hit = true;
                    if level.at_least(ExplainLevel::Verbose) {
                        explain.candidate_scores = scores.into_iter().enumerate()
                            .map(|(i, (id, vs, ks, ts, cs, fs))| CandidateScore {
                                memory_id: id, rank: i + 1,
                                final_score: round4(fs), vector_score: round4(vs),
                                keyword_score: round4(ks), temporal_score: round4(ts),
                                confidence_score: round4(cs),
                            }).collect();
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
            let results = sql.search_fulltext_from(&table, user_id, query, top_k).await?;
            explain.fulltext_ms = ft_start.elapsed().as_secs_f64() * 1000.0;
            explain.fulltext_hit = !results.is_empty();
            explain.path = if explain.fulltext_hit { "fulltext" } else { "none" };
            explain.result_count = results.len();
            explain.total_ms = total_start.elapsed().as_secs_f64() * 1000.0;
            return Ok((results, explain));
        }

        // Fallback for tests (no sql_store)
        if let Some(emb) = self.embed(query).await {
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
        explain.path = if explain.fulltext_hit { "fulltext" } else { "none" };
        explain.result_count = results.len();
        explain.total_ms = total_start.elapsed().as_secs_f64() * 1000.0;
        Ok((results, explain))
    }

    pub async fn search(&self, user_id: &str, query: &str, top_k: i64) -> Result<Vec<Memory>, MemoriaError> {
        self.retrieve(user_id, query, top_k).await
    }

    pub async fn search_explain(&self, user_id: &str, query: &str, top_k: i64) -> Result<(Vec<Memory>, RetrievalExplain), MemoriaError> {
        self.retrieve_inner(user_id, query, top_k, ExplainLevel::Basic).await
    }

    pub async fn search_explain_level(&self, user_id: &str, query: &str, top_k: i64, level: ExplainLevel) -> Result<(Vec<Memory>, RetrievalExplain), MemoriaError> {
        self.retrieve_inner(user_id, query, top_k, level).await
    }

    pub async fn correct(&self, memory_id: &str, new_content: &str) -> Result<Memory, MemoriaError> {
        let old = self.store.get(memory_id).await?
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
            embedding: self.embed(new_content).await,
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

        // Deactivate old and link to new via superseded_by
        self.store.soft_delete(memory_id).await?;
        let mut old_updated = old;
        old_updated.superseded_by = Some(new_mem.memory_id.clone());
        self.store.update(&old_updated).await?;

        Ok(new_mem)
    }

    pub async fn purge(&self, memory_id: &str) -> Result<(), MemoriaError> {
        self.store.soft_delete(memory_id).await
    }

    /// Purge memories whose content contains `topic` (exact text match).
    pub async fn purge_by_topic(&self, user_id: &str, topic: &str) -> Result<usize, MemoriaError> {
        if let Some(sql) = &self.sql_store {
            let table = sql.active_table(user_id).await?;
            let ids = sql.find_ids_by_topic(&table, user_id, topic).await?;
            for id in &ids {
                self.store.soft_delete(id).await?;
                let _ = sql.graph_store().deactivate_by_memory_id(id).await;
            }
            Ok(ids.len())
        } else {
            Ok(0)
        }
    }

    pub async fn get(&self, memory_id: &str) -> Result<Option<Memory>, MemoriaError> {
        self.store.get(memory_id).await
    }

    pub async fn list_active(&self, user_id: &str, limit: i64) -> Result<Vec<Memory>, MemoriaError> {
        if let Some(sql) = &self.sql_store {
            let table = sql.active_table(user_id).await?;
            return sql.list_active_from(&table, user_id, limit).await;
        }
        self.store.list_active(user_id, limit).await
    }

    pub async fn embed(&self, text: &str) -> Option<Vec<f32>> {
        self.embedder.as_ref()?.embed(text).await.ok()
    }
}
