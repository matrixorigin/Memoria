pub mod http;
pub mod llm;
pub mod local;
pub mod mock;

pub use http::HttpEmbedder;
pub use llm::{ChatMessage, LlmClient};
pub use mock::MockEmbedder;

#[cfg(feature = "local-embedding")]
pub use local::LocalEmbedder;
