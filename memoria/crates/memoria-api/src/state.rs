use memoria_git::GitForDataService;
use memoria_service::MemoryService;
use std::sync::Arc;
use tokio::sync::Mutex;

pub type TaskStore = Arc<Mutex<std::collections::HashMap<String, crate::routes::sessions::TaskStatus>>>;

#[derive(Clone)]
pub struct AppState {
    pub service: Arc<MemoryService>,
    pub git: Arc<GitForDataService>,
    /// Master key for auth (empty = no auth)
    pub master_key: String,
    /// In-memory task store for async episodic generation
    pub tasks: TaskStore,
}

impl AppState {
    pub fn new(
        service: Arc<MemoryService>,
        git: Arc<GitForDataService>,
        master_key: String,
    ) -> Self {
        Self {
            service,
            git,
            master_key,
            tasks: Arc::new(Mutex::new(std::collections::HashMap::new())),
        }
    }
}
