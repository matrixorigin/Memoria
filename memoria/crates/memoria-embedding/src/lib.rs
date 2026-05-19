pub mod http;
pub mod llm;
pub mod local;
pub mod mock;
pub mod round_robin;

pub use http::HttpEmbedder;
pub use llm::{ChatMessage, LlmClient};
pub use mock::MockEmbedder;
pub use round_robin::RoundRobinEmbedder;

#[cfg(feature = "local-embedding")]
pub use local::LocalEmbedder;
