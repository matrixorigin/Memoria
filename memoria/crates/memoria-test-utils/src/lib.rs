//! Shared fake LLM server for E2E tests.
//!
//! Spawns a local HTTP server that responds to `/chat/completions` with
//! canned responses based on prompt content. Each test file can supply
//! extra `(prompt_contains, response_json)` pairs for domain-specific prompts.

use std::sync::{Arc, Once};
use std::time::{Duration, Instant};

use memoria_core::interfaces::EmbeddingProvider;
use memoria_embedding::LlmClient;
use memoria_git::GitForDataService;
use memoria_service::MemoryService;
use memoria_storage::{DbRouter, SqlMemoryStore};
use sqlx::mysql::{MySqlPool, MySqlPoolOptions};

/// A `(needle, response_body)` pair: if the prompt contains `needle`,
/// the fake server returns `response_body` as the assistant message content.
pub type PromptRule = (&'static str, serde_json::Value);

pub struct MultiDbTestContext {
    shared_db_url: String,
    router: Option<Arc<DbRouter>>,
    shared_pool: Option<MySqlPool>,
    store: Option<Arc<SqlMemoryStore>>,
    git: Option<Arc<GitForDataService>>,
    service: Option<Arc<MemoryService>>,
}

/// Default rules shared by all test suites (reflect + entity extraction).
pub fn default_rules() -> Vec<PromptRule> {
    vec![
        (
            "OUTPUT FORMAT (JSON array, 0-2 items)",
            serde_json::json!([{
                "type": "semantic",
                "content": "Prefer deterministic validation for reflection flows.",
                "confidence": 0.66,
                "evidence_summary": "Stored sessions focused on stable tool validation."
            }]),
        ),
        (
            "Extract named entities from the following text",
            serde_json::json!([
                {"name": "AlphaMesh", "type": "tech"},
                {"name": "DeltaFabric", "type": "tech"}
            ]),
        ),
    ]
}

/// Spawn a fake OpenAI-compatible LLM server.
///
/// `extra_rules` are checked before `default_rules`, so they can override.
/// Returns the `LlmClient` (with proxy disabled) and a shutdown handle.
/// Drop or send on the handle to stop the server.
pub async fn spawn_fake_llm(
    extra_rules: Vec<PromptRule>,
) -> (
    Arc<memoria_embedding::LlmClient>,
    tokio::sync::oneshot::Sender<()>,
) {
    // Merge: extra first (higher priority), then defaults.
    let rules: Vec<(String, String)> = extra_rules
        .iter()
        .chain(default_rules().iter())
        .map(|(needle, val)| (needle.to_string(), val.to_string()))
        .collect();
    let rules = Arc::new(rules);

    // Fallback response when no rule matches.
    let fallback = default_rules().last().unwrap().1.to_string();
    let fallback = Arc::new(fallback);

    let rules_c = Arc::clone(&rules);
    let fallback_c = Arc::clone(&fallback);

    let app = axum::Router::new().route(
        "/chat/completions",
        axum::routing::post(move |axum::Json(payload): axum::Json<serde_json::Value>| {
            let rules = Arc::clone(&rules_c);
            let fallback = Arc::clone(&fallback_c);
            async move {
                let prompt = payload["messages"]
                    .as_array()
                    .and_then(|msgs| msgs.last())
                    .and_then(|m| m["content"].as_str())
                    .unwrap_or("");
                let content = rules
                    .iter()
                    .find(|(needle, _)| prompt.contains(needle.as_str()))
                    .map(|(_, resp)| resp.clone())
                    .unwrap_or_else(|| fallback.as_str().to_string());
                axum::Json(serde_json::json!({
                    "choices": [{"message": {"content": content}}]
                }))
            }
        }),
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = shutdown_rx.await;
            })
            .await
            .unwrap();
    });
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

    let client = Arc::new(memoria_embedding::LlmClient::new_no_proxy(
        "fake-key".into(),
        format!("http://127.0.0.1:{port}"),
        "fake-model".into(),
    ));
    (client, shutdown_tx)
}

/// Wait until a MySQL/MatrixOne database is actually queryable, not just listening on TCP.
pub async fn wait_for_mysql_ready(db_url: &str, timeout: Duration) {
    let started = Instant::now();
    let mut last_err = None;
    let (base_url, db_name) = split_db_url(db_url).unwrap_or_else(|err| panic!("{err}"));

    while started.elapsed() < timeout {
        match sqlx::mysql::MySqlPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(Duration::from_secs(2))
            .connect(&base_url)
            .await
        {
            Ok(pool) => {
                let exists = sqlx::query_scalar::<_, i64>(
                    "SELECT COUNT(*) FROM INFORMATION_SCHEMA.SCHEMATA WHERE SCHEMA_NAME = ?",
                )
                .bind(&db_name)
                .fetch_one(&pool)
                .await;

                match exists {
                    Ok(0) => {
                        if let Err(err) = sqlx::raw_sql(&format!(
                            "CREATE DATABASE IF NOT EXISTS {}",
                            quote_ident(&db_name)
                        ))
                        .execute(&pool)
                        .await
                        {
                            last_err = Some(err.to_string());
                            pool.close().await;
                            tokio::time::sleep(Duration::from_millis(500)).await;
                            continue;
                        }
                    }
                    Ok(_) => {}
                    Err(err) => {
                        last_err = Some(err.to_string());
                        pool.close().await;
                        tokio::time::sleep(Duration::from_millis(500)).await;
                        continue;
                    }
                }
                pool.close().await;
            }
            Err(err) => {
                last_err = Some(err.to_string());
                tokio::time::sleep(Duration::from_millis(500)).await;
                continue;
            }
        }

        match sqlx::mysql::MySqlPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(Duration::from_secs(2))
            .connect(db_url)
            .await
        {
            Ok(pool) => {
                match sqlx::query("SELECT 1").execute(&pool).await {
                    Ok(_) => {
                        pool.close().await;
                        return;
                    }
                    Err(err) => last_err = Some(err.to_string()),
                }
                pool.close().await;
            }
            Err(err) => last_err = Some(err.to_string()),
        }

        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    panic!(
        "database did not become ready within {:?}: {}",
        timeout,
        last_err.unwrap_or_else(|| "unknown error".to_string())
    );
}

impl MultiDbTestContext {
    pub async fn new(
        base_db_url: &str,
        db_name_prefix: &str,
        embedding_dim: usize,
        embedder: Option<Arc<dyn EmbeddingProvider>>,
        llm: Option<Arc<LlmClient>>,
    ) -> Self {
        configure_multi_db_test_pools();
        let shared_db_url = unique_db_url(base_db_url, db_name_prefix);
        wait_for_mysql_ready(&shared_db_url, Duration::from_secs(30)).await;

        let router = Arc::new(
            DbRouter::connect(
                &shared_db_url,
                embedding_dim,
                uuid::Uuid::new_v4().to_string(),
            )
            .await
            .expect("connect multi-db router"),
        );
        let shared_pool = router.shared_pool().clone();
        let mut store = SqlMemoryStore::from_existing_pool(
            shared_pool.clone(),
            embedding_dim,
            uuid::Uuid::new_v4().to_string(),
            Some(shared_db_url.clone()),
            Some(router.shared_pool_max_connections()),
            "multi_db_test_shared_pool",
        );
        store.migrate_shared().await.expect("migrate shared db");
        store.set_db_router(router.clone());
        let store = Arc::new(store);
        let git = Arc::new(GitForDataService::new(
            shared_pool.clone(),
            router.shared_db_name().to_string(),
        ));
        let service = Arc::new(
            MemoryService::new_sql_with_llm_and_router(
                store.clone(),
                Some(router.clone()),
                embedder,
                llm,
            )
            .await,
        );

        Self {
            shared_db_url,
            router: Some(router),
            shared_pool: Some(shared_pool),
            store: Some(store),
            git: Some(git),
            service: Some(service),
        }
    }

    pub fn shared_db_url(&self) -> &str {
        &self.shared_db_url
    }

    pub fn router(&self) -> Arc<DbRouter> {
        self.router.as_ref().expect("router").clone()
    }

    pub fn shared_pool(&self) -> MySqlPool {
        self.shared_pool.as_ref().expect("shared pool").clone()
    }

    pub fn shared_store(&self) -> Arc<SqlMemoryStore> {
        self.store.as_ref().expect("shared store").clone()
    }

    pub fn git(&self) -> Arc<GitForDataService> {
        self.git.as_ref().expect("git service").clone()
    }

    pub fn service(&self) -> Arc<MemoryService> {
        self.service.as_ref().expect("memory service").clone()
    }

    pub fn shared_table(&self, table: &str) -> String {
        self.store.as_ref().expect("shared store").t(table)
    }

    pub async fn user_db_name(&self, user_id: &str) -> String {
        self.router
            .as_ref()
            .expect("router")
            .user_db_name(user_id)
            .await
            .expect("resolve user db")
    }

    pub async fn user_store(&self, user_id: &str) -> Arc<SqlMemoryStore> {
        self.service
            .as_ref()
            .expect("memory service")
            .user_sql_store(user_id)
            .await
            .expect("resolve user store")
    }

    pub async fn user_table(&self, user_id: &str, table: &str) -> String {
        self.user_store(user_id).await.t(table)
    }

    pub async fn user_db_pool(&self, user_id: &str) -> MySqlPool {
        let db_name = self.user_db_name(user_id).await;
        let user_db_url = self
            .router
            .as_ref()
            .expect("router")
            .user_db_url(&db_name)
            .expect("user db url");
        MySqlPoolOptions::new()
            .max_connections(1)
            .connect(&user_db_url)
            .await
            .expect("connect user db")
    }
}

fn configure_multi_db_test_pools() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        set_env_if_unset("MEMORIA_SHARED_POOL_MAX_CONNECTIONS", "1");
        set_env_if_unset("MEMORIA_SHARED_MAIN_POOL_MAX_CONNECTIONS", "1");
        set_env_if_unset("MEMORIA_GIT_POOL_MAX_CONNECTIONS", "1");
        set_env_if_unset("MEMORIA_MERGED_SHARED_POOL_MAX_CONNECTIONS", "3");
        set_env_if_unset("MEMORIA_GLOBAL_USER_POOL_MAX", "4");
        set_env_if_unset("MEMORIA_USER_SCHEMA_INIT_POOL_MAX_CONNECTIONS", "1");
        set_env_if_unset("MEMORIA_USER_SCHEMA_INIT_MAX_CONCURRENCY", "1");
    });
}

fn set_env_if_unset(name: &str, value: &str) {
    if std::env::var_os(name).is_none() {
        unsafe {
            std::env::set_var(name, value);
        }
    }
}

impl Drop for MultiDbTestContext {
    fn drop(&mut self) {
        let Some(router) = self.router.take() else {
            return;
        };
        let Some(shared_pool) = self.shared_pool.take() else {
            return;
        };
        let shared_db_url = self.shared_db_url.clone();
        let global_user_pool = router.global_user_pool().clone();
        let store = self.store.take();
        let git = self.git.take();
        let service = self.service.take();

        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let db_names = sqlx::query_scalar::<_, String>(
                    "SELECT DISTINCT db_name FROM mem_user_registry WHERE status = 'active'",
                )
                .fetch_all(&shared_pool)
                .await
                .unwrap_or_default();

                drop(service);
                drop(git);
                drop(store);
                drop(router);

                shared_pool.close().await;
                global_user_pool.close().await;
                cleanup_databases(&shared_db_url, &db_names).await;
            });
        }
    }
}

pub fn unique_db_url(base_db_url: &str, db_name_prefix: &str) -> String {
    let suffix = &uuid::Uuid::new_v4().simple().to_string()[..8];
    replace_db_name(base_db_url, &format!("{db_name_prefix}_{suffix}"))
}

fn split_db_url(db_url: &str) -> Result<(String, String), String> {
    let (base_url, db_name) = db_url
        .rsplit_once('/')
        .ok_or_else(|| "database URL is missing a database name".to_string())?;
    let db_name = db_name.split(['?', '#']).next().unwrap_or(db_name).trim();
    if db_name.is_empty() {
        return Err("database URL is missing a database name".to_string());
    }
    Ok((base_url.to_string(), db_name.to_string()))
}

fn quote_ident(name: &str) -> String {
    format!("`{}`", name.replace('`', "``"))
}

pub fn replace_db_name(database_url: &str, db_name: &str) -> String {
    let suffix_start = database_url.find(['?', '#']).unwrap_or(database_url.len());
    let (without_suffix, suffix) = database_url.split_at(suffix_start);
    let (base, _) = without_suffix
        .rsplit_once('/')
        .expect("database url must include db name");
    format!("{base}/{db_name}{suffix}")
}

async fn cleanup_databases(shared_db_url: &str, user_db_names: &[String]) {
    let (base_url, shared_db_name) = split_db_url(shared_db_url).expect("split shared db url");
    let admin_pool = MySqlPoolOptions::new()
        .max_connections(1)
        .connect(&base_url)
        .await
        .expect("connect admin pool");

    for db_name in user_db_names {
        sqlx::raw_sql(&format!("DROP DATABASE IF EXISTS {}", quote_ident(db_name)))
            .execute(&admin_pool)
            .await
            .expect("drop user db");
    }
    sqlx::raw_sql(&format!(
        "DROP DATABASE IF EXISTS {}",
        quote_ident(&shared_db_name)
    ))
    .execute(&admin_pool)
    .await
    .expect("drop shared db");
    admin_pool.close().await;
}
