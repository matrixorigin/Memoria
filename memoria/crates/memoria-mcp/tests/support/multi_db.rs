use std::sync::Arc;

use memoria_core::interfaces::EmbeddingProvider;
use memoria_embedding::LlmClient;
use memoria_git::GitForDataService;
use memoria_service::MemoryService;
use memoria_storage::{DbRouter, SqlMemoryStore};
use memoria_test_utils::MultiDbTestContext;
use sqlx::MySqlPool;

pub struct McpTestContext {
    inner: MultiDbTestContext,
}

impl McpTestContext {
    pub fn service(&self) -> Arc<MemoryService> {
        self.inner.service()
    }

    pub fn git(&self) -> Arc<GitForDataService> {
        self.inner.git()
    }

    pub fn router(&self) -> Arc<DbRouter> {
        self.inner.router()
    }

    pub fn shared_pool(&self) -> MySqlPool {
        self.inner.shared_pool()
    }

    pub fn shared_store(&self) -> Arc<SqlMemoryStore> {
        self.inner.shared_store()
    }

    pub async fn user_store(&self, user_id: &str) -> Arc<SqlMemoryStore> {
        self.inner.user_store(user_id).await
    }

    pub async fn user_table(&self, user_id: &str, table: &str) -> String {
        self.inner.user_table(user_id, table).await
    }

    pub async fn user_db_name(&self, user_id: &str) -> String {
        self.inner.user_db_name(user_id).await
    }

    pub async fn user_db_pool(&self, user_id: &str) -> MySqlPool {
        self.inner.user_db_pool(user_id).await
    }
}

pub async fn setup_mcp_context(
    db_name_prefix: &str,
    embedding_dim: usize,
    embedder: Option<Arc<dyn EmbeddingProvider>>,
    llm: Option<Arc<LlmClient>>,
) -> McpTestContext {
    McpTestContext {
        inner: MultiDbTestContext::new(
            &db_url(),
            db_name_prefix,
            embedding_dim,
            embedder,
            llm,
        )
        .await,
    }
}

fn db_url() -> String {
    std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "mysql://root:111@localhost:6001/memoria".to_string())
}
