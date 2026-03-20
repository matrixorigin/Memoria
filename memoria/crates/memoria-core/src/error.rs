use thiserror::Error;

#[derive(Debug, Error)]
pub enum MemoriaError {
    #[error("Invalid memory type: {0}")]
    InvalidMemoryType(String),

    #[error("Invalid trust tier: {0}")]
    InvalidTrustTier(String),

    #[error("Memory not found: {0}")]
    NotFound(String),

    #[error("Database error: {0}")]
    Database(String),

    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("Embedding error: {0}")]
    Embedding(String),

    #[error("Internal error: {0}")]
    Internal(String),

    #[error("Validation error: {0}")]
    Validation(String),

    #[error("Blocked: {0}")]
    Blocked(String),
}
