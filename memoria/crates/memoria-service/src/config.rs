/// Unified configuration for Memoria MCP server.
/// All settings read from environment variables (matching Python's config.py).
///
/// Environment variables (no prefix, matching README):
///   DATABASE_URL          — full MySQL URL
///   EMBEDDING_PROVIDER    — "openai" | "local" | "mock" (default: "mock")
///   EMBEDDING_MODEL       — e.g. "BAAI/bge-m3"
///   EMBEDDING_DIM         — integer, e.g. 1024
///   EMBEDDING_API_KEY     — API key for embedding service
///   EMBEDDING_BASE_URL    — base URL for embedding service
///   LLM_API_KEY           — OpenAI-compatible API key (optional)
///   LLM_BASE_URL          — LLM base URL (default: https://api.openai.com/v1)
///   LLM_MODEL             — LLM model name (default: gpt-4o-mini)
///   MEMORIA_USER          — default user ID (default: "default")
///   MEMORIA_DB_NAME       — database name for git-for-data (default: "memoria")

#[derive(Debug, Clone)]
pub struct Config {
    // Database
    pub db_url: String,
    pub db_name: String,

    // Embedding
    pub embedding_provider: String,
    pub embedding_model: String,
    pub embedding_dim: usize,
    pub embedding_api_key: String,
    pub embedding_base_url: String,

    // LLM (optional)
    pub llm_api_key: Option<String>,
    pub llm_base_url: String,
    pub llm_model: String,

    // Server
    pub user: String,
}

impl Config {
    /// Load from environment variables with sensible defaults.
    pub fn from_env() -> Self {
        let db_url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "mysql://root:111@localhost:6001/memoria".to_string());

        // Extract db_name from URL (last path segment) or from MEMORIA_DB_NAME
        let db_name = std::env::var("MEMORIA_DB_NAME").unwrap_or_else(|_| {
            db_url.rsplit('/').next().unwrap_or("memoria").to_string()
        });

        let embedding_dim = std::env::var("EMBEDDING_DIM")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(1024usize);

        let llm_api_key = std::env::var("LLM_API_KEY")
            .ok()
            .filter(|s| !s.is_empty());

        Self {
            db_url,
            db_name,
            embedding_provider: std::env::var("EMBEDDING_PROVIDER")
                .unwrap_or_else(|_| "mock".to_string()),
            embedding_model: std::env::var("EMBEDDING_MODEL")
                .unwrap_or_else(|_| "BAAI/bge-m3".to_string()),
            embedding_dim,
            embedding_api_key: std::env::var("EMBEDDING_API_KEY")
                .unwrap_or_default(),
            embedding_base_url: std::env::var("EMBEDDING_BASE_URL")
                .unwrap_or_default(),
            llm_api_key,
            llm_base_url: std::env::var("LLM_BASE_URL")
                .unwrap_or_else(|_| "https://api.openai.com/v1".to_string()),
            llm_model: std::env::var("LLM_MODEL")
                .unwrap_or_else(|_| "gpt-4o-mini".to_string()),
            user: std::env::var("MEMORIA_USER")
                .unwrap_or_else(|_| "default".to_string()),
        }
    }

    /// Returns true if LLM is configured.
    pub fn has_llm(&self) -> bool {
        self.llm_api_key.is_some()
    }

    /// Returns true if embedding is configured (non-mock provider with base URL, or local).
    pub fn has_embedding(&self) -> bool {
        if self.embedding_provider == "local" {
            return false; // local is handled separately, not via HttpEmbedder
        }
        self.embedding_provider != "mock" && !self.embedding_base_url.is_empty()
    }
}
