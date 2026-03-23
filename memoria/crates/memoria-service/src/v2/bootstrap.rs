use std::sync::Arc;

use memoria_embedding::LlmClient;
use memoria_storage::SqlMemoryStore;

pub(crate) fn start_background_runtime(store: Arc<SqlMemoryStore>, llm: Option<Arc<LlmClient>>) {
    super::worker::spawn_v2_job_worker(store, llm);
}
