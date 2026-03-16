use memoria_service::MemoryService;
use std::sync::Arc;

#[derive(Clone)]
pub struct AppState {
    pub service: Arc<MemoryService>,
}
