use memoria_git::GitForDataService;
use memoria_service::MemoryService;
use std::sync::Arc;

#[derive(Clone)]
pub struct AppState {
    pub service: Arc<MemoryService>,
    pub git: Arc<GitForDataService>,
    /// Master key for auth (empty = no auth)
    pub master_key: String,
}
