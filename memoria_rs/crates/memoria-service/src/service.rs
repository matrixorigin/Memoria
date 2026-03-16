use memoria_core::{
    interfaces::{EmbeddingProvider, MemoryStore},
    Memory, MemoriaError, MemoryType, TrustTier,
};
use memoria_embedding::LlmClient;
use memoria_storage::SqlMemoryStore;
use std::sync::Arc;
use uuid::Uuid;
use chrono::Utc;

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
    ) -> Result<Memory, MemoriaError> {
        let embedding = self.embed(content).await;
        let memory = Memory {
            memory_id: Uuid::new_v4().simple().to_string(),
            user_id: user_id.to_string(),
            memory_type,
            content: content.to_string(),
            initial_confidence: trust_tier.as_ref().map(|t| t.initial_confidence()).unwrap_or(0.75),
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
            trust_tier: trust_tier.unwrap_or_default(),
            retrieval_score: None,
        };
        // Write to active branch table if sql_store available, else use trait
        if let Some(sql) = &self.sql_store {
            let table = sql.active_table(user_id).await?;
            sql.insert_into(&table, &memory).await?;
        } else {
            self.store.insert(&memory).await?;
        }
        Ok(memory)
    }

    pub async fn retrieve(&self, user_id: &str, query: &str, top_k: i64) -> Result<Vec<Memory>, MemoriaError> {
        if let Some(sql) = &self.sql_store {
            let table = sql.active_table(user_id).await?;
            if let Some(emb) = self.embed(query).await {
                let results = sql.search_vector_from(&table, user_id, &emb, top_k).await?;
                if !results.is_empty() { return Ok(results); }
            }
            return sql.search_fulltext_from(&table, user_id, query, top_k).await;
        }
        // Fallback for tests
        if let Some(emb) = self.embed(query).await {
            let results = self.store.search_vector(user_id, &emb, top_k).await?;
            if !results.is_empty() { return Ok(results); }
        }
        self.store.search_fulltext(user_id, query, top_k).await
    }

    pub async fn search(&self, user_id: &str, query: &str, top_k: i64) -> Result<Vec<Memory>, MemoriaError> {
        self.retrieve(user_id, query, top_k).await
    }

    pub async fn correct(&self, memory_id: &str, new_content: &str) -> Result<Memory, MemoriaError> {
        let mut memory = self.store.get(memory_id).await?
            .ok_or_else(|| MemoriaError::NotFound(memory_id.to_string()))?;
        memory.content = new_content.to_string();
        memory.embedding = self.embed(new_content).await;
        self.store.update(&memory).await?;
        Ok(memory)
    }

    pub async fn purge(&self, memory_id: &str) -> Result<(), MemoriaError> {
        self.store.soft_delete(memory_id).await
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

    async fn embed(&self, text: &str) -> Option<Vec<f32>> {
        self.embedder.as_ref()?.embed(text).await.ok()
    }
}
