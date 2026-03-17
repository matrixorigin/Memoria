pub mod http;
pub mod llm;
pub mod local;

pub use http::HttpEmbedder;
pub use llm::{LlmClient, ChatMessage};

#[cfg(feature = "local-embedding")]
pub use local::LocalEmbedder;
