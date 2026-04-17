use std::sync::Arc;

use memoria_core::interfaces::EmbeddingProvider;
use memoria_embedding::LlmClient;
use memoria_git::GitForDataService;
use memoria_service::MemoryService;
use memoria_storage::{DbRouter, SqlMemoryStore};
use memoria_test_utils::MultiDbTestContext;
use sqlx::MySqlPool;

pub struct ApiTestServer {
    pub base: String,
    pub client: reqwest::Client,
    context: Option<MultiDbTestContext>,
    shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
    server_handle: Option<tokio::task::JoinHandle<()>>,
}

impl ApiTestServer {
    pub fn service(&self) -> Arc<MemoryService> {
        self.context.as_ref().expect("context").service()
    }

    pub fn git(&self) -> Arc<GitForDataService> {
        self.context.as_ref().expect("context").git()
    }

    pub fn router(&self) -> Arc<DbRouter> {
        self.context.as_ref().expect("context").router()
    }

    pub fn shared_pool(&self) -> MySqlPool {
        self.context.as_ref().expect("context").shared_pool()
    }

    pub fn shared_store(&self) -> Arc<SqlMemoryStore> {
        self.context.as_ref().expect("context").shared_store()
    }

    pub fn shared_table(&self, table: &str) -> String {
        self.context.as_ref().expect("context").shared_table(table)
    }

    pub fn shared_db_url(&self) -> &str {
        self.context.as_ref().expect("context").shared_db_url()
    }

    pub async fn user_store(&self, user_id: &str) -> Arc<SqlMemoryStore> {
        self.context.as_ref().expect("context").user_store(user_id).await
    }

    pub async fn user_table(&self, user_id: &str, table: &str) -> String {
        self.context.as_ref().expect("context").user_table(user_id, table).await
    }

    pub async fn user_db_name(&self, user_id: &str) -> String {
        self.context.as_ref().expect("context").user_db_name(user_id).await
    }

    pub async fn user_db_pool(&self, user_id: &str) -> MySqlPool {
        self.context.as_ref().expect("context").user_db_pool(user_id).await
    }
}

impl Drop for ApiTestServer {
    fn drop(&mut self) {
        let shutdown_tx = self.shutdown_tx.take();
        let server_handle = self.server_handle.take();
        let context = self.context.take();

        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                if let Some(tx) = shutdown_tx {
                    let _ = tx.send(());
                }
                if let Some(server_handle) = server_handle {
                    let _ = server_handle.await;
                }
                drop(context);
            });
        }
    }
}

pub async fn spawn_api_server(
    db_name_prefix: &str,
    embedding_dim: usize,
    master_key: String,
    embedder: Option<Arc<dyn EmbeddingProvider>>,
    llm: Option<Arc<LlmClient>>,
    instance_id: Option<String>,
    init_auth_pool: bool,
) -> ApiTestServer {
    let context =
        MultiDbTestContext::new(&db_url(), db_name_prefix, embedding_dim, embedder, llm).await;
    let mut state = memoria_api::AppState::new(context.service(), context.git(), master_key);
    if let Some(instance_id) = instance_id {
        state = state.with_instance_id(instance_id);
    }
    if init_auth_pool {
        state = state
            .init_auth_pool(context.shared_db_url(), false)
            .await
            .expect("init auth pool");
    }

    let app = memoria_api::build_router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let port = listener.local_addr().expect("local addr").port();
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let server_handle = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = shutdown_rx.await;
            })
            .await
            .expect("serve api test app");
    });

    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("client");
    let base = format!("http://127.0.0.1:{port}");
    wait_for_server(&client, &base, &context.shared_pool()).await;

    ApiTestServer {
        base,
        client,
        context: Some(context),
        shutdown_tx: Some(shutdown_tx),
        server_handle: Some(server_handle),
    }
}

pub async fn wait_for_server(client: &reqwest::Client, base: &str, pool: &MySqlPool) {
    for _ in 0..20 {
        if client.get(format!("{base}/health")).send().await.is_ok() {
            break;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    }

    for _ in 0..20 {
        if sqlx::query("SELECT 1").execute(pool).await.is_ok() {
            return;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    }

    panic!("DB not ready after 1s");
}

fn db_url() -> String {
    std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "mysql://root:111@localhost:6001/memoria".to_string())
}
