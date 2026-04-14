use crate::store::spawn_pool_monitor;
use crate::{PoolHealthSnapshot, SqlMemoryStore};
use chrono::Utc;
use memoria_core::MemoriaError;
use moka::sync::Cache;
use sha2::{Digest, Sha256};
use sqlx::{mysql::MySqlPoolOptions, MySqlPool, Row};
use std::sync::Arc;
use tokio::sync::Semaphore;

fn db_err(e: sqlx::Error) -> MemoriaError {
    MemoriaError::Database(e.to_string())
}

fn is_duplicate_key_error(e: &sqlx::Error) -> bool {
    use sqlx::mysql::MySqlDatabaseError;

    e.as_database_error()
        .and_then(|de| de.as_error().downcast_ref::<MySqlDatabaseError>())
        .map(|me| me.number() == 1062)
        .unwrap_or(false)
}

const USER_DB_CACHE_MAX_CAPACITY: u64 = 10_000;
const USER_STORE_CACHE_MAX_CAPACITY: u64 = 10_000;
const USER_SCHEMA_CACHE_MAX_CAPACITY: u64 = 10_000;
const USER_STORE_CACHE_IDLE_SECS: u64 = 600;
const SHARED_POOL_MAX_CONNECTIONS: u32 = 8;
const SHARED_MAIN_POOL_MAX_CONNECTIONS: u32 = 8;
const GIT_POOL_MAX_CONNECTIONS: u32 = 4;
const GLOBAL_USER_POOL_MAX_CONNECTIONS: u32 = 80;
const USER_SCHEMA_INIT_POOL_MAX_CONNECTIONS: u32 = 12;
const POOL_MAX_CONNECTIONS_UPPER: u32 = 256;
const MERGED_SHARED_POOL_MAX_CONNECTIONS_ENV: &str = "MEMORIA_MERGED_SHARED_POOL_MAX_CONNECTIONS";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SharedPoolPlan {
    max_connections: u32,
    routing_component_max_connections: u32,
    shared_main_component_max_connections: u32,
    git_component_max_connections: u32,
    explicit_override: bool,
}

#[derive(Debug, Clone)]
pub struct UserDatabaseRecord {
    pub user_id: String,
    pub db_name: String,
    pub status: String,
}

#[derive(Clone)]
pub struct DbRouter {
    shared_pool: MySqlPool,
    shared_pool_max_connections: u32,
    /// Global pool for all per-user DB queries. Connections are switched to
    /// the correct database via `USE` (conn() pattern) or fully-qualified
    /// table names (qualified_table() pattern). `statement_cache_capacity=0`
    /// prevents cross-database prepared-statement pollution.
    global_user_pool: MySqlPool,
    global_user_pool_max_connections: u32,
    user_init_pool: MySqlPool,
    shared_db_url: String,
    shared_db_name: String,
    embedding_dim: usize,
    instance_id: String,
    user_db_cache: Cache<String, String>,
    user_schema_cache: Cache<String, bool>,
    user_store_cache: Cache<String, Arc<SqlMemoryStore>>,
    user_init_semaphore: Arc<Semaphore>,
}

impl DbRouter {
    pub async fn connect(
        shared_db_url: &str,
        embedding_dim: usize,
        instance_id: String,
    ) -> Result<Self, MemoriaError> {
        create_database_if_missing_from_url(shared_db_url).await?;
        let shared_plan = configured_shared_pool_plan();
        let pool = MySqlPoolOptions::new()
            .max_connections(shared_plan.max_connections)
            .max_lifetime(std::time::Duration::from_secs(3600))
            .idle_timeout(std::time::Duration::from_secs(300))
            .acquire_timeout(std::time::Duration::from_secs(10))
            .connect(shared_db_url)
            .await
            .map_err(db_err)?;
        tracing::info!(
            max_connections = shared_plan.max_connections,
            routing_component_max_connections = shared_plan.routing_component_max_connections,
            shared_main_component_max_connections =
                shared_plan.shared_main_component_max_connections,
            git_component_max_connections = shared_plan.git_component_max_connections,
            explicit_override = shared_plan.explicit_override,
            "Shared database pool initialized"
        );

        // Global pool for all per-user DB queries.
        // statement_cache_capacity=0 prevents prepared-statement cross-DB pollution.
        let global_max = configured_pool_max_connections(
            "MEMORIA_GLOBAL_USER_POOL_MAX",
            GLOBAL_USER_POOL_MAX_CONNECTIONS,
            POOL_MAX_CONNECTIONS_UPPER,
        );
        let global_url = append_url_param(shared_db_url, "statement_cache_capacity=0");
        let global_user_pool = MySqlPoolOptions::new()
            .max_connections(global_max)
            .min_connections(2)
            .max_lifetime(std::time::Duration::from_secs(3600))
            .idle_timeout(std::time::Duration::from_secs(300))
            .acquire_timeout(std::time::Duration::from_secs(15))
            .connect(&global_url)
            .await
            .map_err(db_err)?;
        tracing::info!(
            max_connections = global_max,
            "Global user pool initialized (statement_cache=0)"
        );
        spawn_pool_monitor(
            global_user_pool.clone(),
            Some(global_max),
            Arc::new(std::sync::Mutex::new(PoolHealthSnapshot::new(Some(
                global_max,
            )))),
            "global_user_pool",
        );
        let user_init_pool_max = configured_pool_max_connections(
            "MEMORIA_USER_SCHEMA_INIT_POOL_MAX_CONNECTIONS",
            USER_SCHEMA_INIT_POOL_MAX_CONNECTIONS,
            64,
        );
        let user_init_pool = MySqlPoolOptions::new()
            .max_connections(user_init_pool_max)
            .min_connections(0)
            .max_lifetime(std::time::Duration::from_secs(3600))
            .idle_timeout(std::time::Duration::from_secs(300))
            .acquire_timeout(std::time::Duration::from_secs(30))
            .connect(&global_url)
            .await
            .map_err(db_err)?;
        tracing::info!(
            max_connections = user_init_pool_max,
            "User schema init pool initialized (statement_cache=0)"
        );

        let shared_db_name = parse_db_name(shared_db_url)
            .ok_or_else(|| MemoriaError::Internal("invalid shared_db_url".into()))?;
        let user_init_max: usize = std::env::var("MEMORIA_USER_SCHEMA_INIT_MAX_CONCURRENCY")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(6)
            .clamp(1, 64);
        let router = Self {
            shared_pool: pool,
            shared_pool_max_connections: shared_plan.max_connections,
            global_user_pool,
            global_user_pool_max_connections: global_max,
            user_init_pool,
            shared_db_url: shared_db_url.to_string(),
            shared_db_name,
            embedding_dim,
            instance_id,
            user_db_cache: Cache::builder()
                .max_capacity(USER_DB_CACHE_MAX_CAPACITY)
                .time_to_live(std::time::Duration::from_secs(300))
                .build(),
            user_schema_cache: Cache::builder()
                .max_capacity(USER_SCHEMA_CACHE_MAX_CAPACITY)
                .build(),
            user_store_cache: Cache::builder()
                .max_capacity(USER_STORE_CACHE_MAX_CAPACITY)
                .time_to_idle(std::time::Duration::from_secs(USER_STORE_CACHE_IDLE_SECS))
                .build(),
            user_init_semaphore: Arc::new(Semaphore::new(user_init_max)),
        };
        router.ensure_user_registry_table().await?;
        Ok(router)
    }

    pub fn shared_pool(&self) -> &MySqlPool {
        &self.shared_pool
    }

    pub fn shared_pool_max_connections(&self) -> u32 {
        self.shared_pool_max_connections
    }

    pub fn global_user_pool(&self) -> &MySqlPool {
        &self.global_user_pool
    }

    pub fn global_user_pool_max_connections(&self) -> u32 {
        self.global_user_pool_max_connections
    }

    pub fn shared_db_name(&self) -> &str {
        &self.shared_db_name
    }

    pub fn shared_db_url(&self) -> &str {
        &self.shared_db_url
    }

    pub fn user_db_name_for_id(user_id: &str) -> String {
        let digest = Sha256::digest(user_id.as_bytes());
        let mut hex = String::with_capacity(16);
        for b in digest.iter().take(8) {
            use std::fmt::Write as _;
            let _ = write!(&mut hex, "{b:02x}");
        }
        format!("mem_u_{hex}")
    }

    pub fn user_db_url(&self, db_name: &str) -> Result<String, MemoriaError> {
        replace_db_name(&self.shared_db_url, db_name)
            .ok_or_else(|| MemoriaError::Internal("invalid shared_db_url".into()))
    }

    pub fn embedding_dim(&self) -> usize {
        self.embedding_dim
    }

    /// Register (or re-activate) a user→db mapping in mem_user_registry.
    pub async fn register_user_db(&self, user_id: &str, db_name: &str) -> Result<(), MemoriaError> {
        let now = Utc::now().naive_utc();
        let insert = sqlx::query(
            r#"INSERT INTO mem_user_registry (user_id, db_name, status, created_at, updated_at)
               VALUES (?, ?, 'active', ?, ?)"#,
        )
        .bind(user_id)
        .bind(db_name)
        .bind(now)
        .bind(now)
        .execute(&self.shared_pool)
        .await;

        match insert {
            Ok(_) => Ok(()),
            Err(e) if is_duplicate_key_error(&e) => {
                sqlx::query(
                    "UPDATE mem_user_registry SET db_name = ?, status = 'active', updated_at = ? WHERE user_id = ?",
                )
                .bind(db_name)
                .bind(now)
                .bind(user_id)
                .execute(&self.shared_pool)
                .await
                .map_err(db_err)?;
                Ok(())
            }
            Err(e) => Err(db_err(e)),
        }
    }

    pub async fn list_registered_users(&self) -> Result<Vec<UserDatabaseRecord>, MemoriaError> {
        let rows =
            sqlx::query("SELECT user_id, db_name, status FROM mem_user_registry ORDER BY user_id")
                .fetch_all(&self.shared_pool)
                .await
                .map_err(db_err)?;
        rows.iter()
            .map(|row| {
                Ok(UserDatabaseRecord {
                    user_id: row.try_get("user_id").map_err(db_err)?,
                    db_name: row.try_get("db_name").map_err(db_err)?,
                    status: row.try_get("status").map_err(db_err)?,
                })
            })
            .collect()
    }

    pub async fn list_active_users(&self) -> Result<Vec<String>, MemoriaError> {
        let rows = sqlx::query("SELECT user_id FROM mem_user_registry WHERE status = 'active'")
            .fetch_all(&self.shared_pool)
            .await
            .map_err(db_err)?;
        rows.iter()
            .map(|row| row.try_get("user_id").map_err(db_err))
            .collect()
    }

    pub async fn user_db_name(&self, user_id: &str) -> Result<String, MemoriaError> {
        if let Some(cached) = self.user_db_cache.get(user_id) {
            return Ok(cached);
        }

        // Avoid moka single-flight here: its init future makes upstream callers
        // fail Send bounds under axum handlers and tokio::spawn.
        let shared_pool = self.shared_pool.clone();
        let user_id_owned = user_id.to_string();
        let row = sqlx::query(
            "SELECT db_name FROM mem_user_registry WHERE user_id = ? AND status = 'active'",
        )
        .bind(&user_id_owned)
        .fetch_optional(&shared_pool)
        .await
        .map_err(db_err)?;
        let db_name = match row {
            Some(row) => row.try_get("db_name").map_err(db_err)?,
            None => provision_user_db_with_pool(&shared_pool, &user_id_owned).await?,
        };
        self.user_db_cache.insert(user_id_owned, db_name.clone());
        Ok(db_name)
    }

    pub async fn user_store(&self, user_id: &str) -> Result<Arc<SqlMemoryStore>, MemoriaError> {
        if let Some(cached) = self.user_store_cache.get(user_id) {
            return Ok(cached);
        }

        // Avoid moka single-flight here: its init future makes upstream callers
        // fail Send bounds under axum handlers and tokio::spawn.
        let shared_pool = self.shared_pool.clone();
        let global_user_pool = self.global_user_pool.clone();
        let user_init_pool = self.user_init_pool.clone();
        let shared_db_url = self.shared_db_url.clone();
        let embedding_dim = self.embedding_dim;
        let instance_id = self.instance_id.clone();
        let user_db_cache = self.user_db_cache.clone();
        let user_schema_cache = self.user_schema_cache.clone();
        let user_init_semaphore = self.user_init_semaphore.clone();
        let user_id_owned = user_id.to_string();
        let existing = sqlx::query(
            "SELECT db_name FROM mem_user_registry WHERE user_id = ? AND status = 'active'",
        )
        .bind(&user_id_owned)
        .fetch_optional(&shared_pool)
        .await
        .map_err(db_err)?;
        let (db_name, needs_init) = match existing {
            Some(row) => (row.try_get("db_name").map_err(db_err)?, false),
            None => (
                provision_user_db_with_pool(&shared_pool, &user_id_owned).await?,
                true,
            ),
        };
        if user_schema_cache.get(&user_id_owned).is_none() {
            let _permit = user_init_semaphore
                .acquire_owned()
                .await
                .map_err(|_| MemoriaError::Internal("user schema init semaphore closed".into()))?;
            if user_schema_cache.get(&user_id_owned).is_none() {
                let init_store = build_routed_store(
                    user_init_pool.clone(),
                    &shared_db_url,
                    embedding_dim,
                    &instance_id,
                    &db_name,
                )?;
                let init_result = init_store.migrate_user().await;
                if let Err(err) = init_result {
                    if needs_init {
                        let _ = sqlx::query(
                            "DELETE FROM mem_user_registry WHERE user_id = ? AND db_name = ?",
                        )
                        .bind(&user_id_owned)
                        .bind(&db_name)
                        .execute(&shared_pool)
                        .await;
                    }
                    return Err(err);
                }
                user_schema_cache.insert(user_id_owned.clone(), true);
            }
        }
        user_db_cache.insert(user_id_owned.clone(), db_name.clone());
        let store = Arc::new(build_routed_store(
            global_user_pool,
            &shared_db_url,
            embedding_dim,
            &instance_id,
            &db_name,
        )?);
        self.user_store_cache.insert(user_id_owned, store.clone());
        Ok(store)
    }

    pub fn routed_store_for_user(&self, user_id: &str) -> Result<SqlMemoryStore, MemoriaError> {
        let db_name = Self::user_db_name_for_id(user_id);
        self.routed_store_for_db_name(&db_name)
    }

    pub fn routed_store_for_db_name(&self, db_name: &str) -> Result<SqlMemoryStore, MemoriaError> {
        build_routed_store(
            self.global_user_pool.clone(),
            &self.shared_db_url,
            self.embedding_dim,
            &self.instance_id,
            db_name,
        )
    }

    pub async fn invalidate_user(&self, user_id: &str) {
        self.user_db_cache.invalidate(user_id);
        self.user_schema_cache.invalidate(user_id);
        self.user_store_cache.invalidate(user_id);
    }

    async fn ensure_user_registry_table(&self) -> Result<(), MemoriaError> {
        sqlx::query(
            r#"CREATE TABLE IF NOT EXISTS mem_user_registry (
                user_id     VARCHAR(64)  PRIMARY KEY,
                db_name     VARCHAR(128) NOT NULL UNIQUE,
                status      VARCHAR(20)  NOT NULL DEFAULT 'active',
                created_at  DATETIME(6)  NOT NULL,
                updated_at  DATETIME(6)  NOT NULL,
                INDEX idx_status (status)
            )"#,
        )
        .execute(&self.shared_pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }
}

fn user_db_url_from_shared(shared_db_url: &str, db_name: &str) -> Result<String, MemoriaError> {
    replace_db_name(shared_db_url, db_name)
        .ok_or_else(|| MemoriaError::Internal("invalid shared_db_url".into()))
}

fn build_routed_store(
    global_user_pool: MySqlPool,
    shared_db_url: &str,
    embedding_dim: usize,
    instance_id: &str,
    db_name: &str,
) -> Result<SqlMemoryStore, MemoriaError> {
    let db_url = user_db_url_from_shared(shared_db_url, db_name)?;
    let mut store = SqlMemoryStore::new(global_user_pool, embedding_dim, instance_id.to_string());
    store.set_db_name(db_name.to_string());
    store.set_database_url(db_url);
    Ok(store)
}

async fn provision_user_db_with_pool(
    shared_pool: &MySqlPool,
    user_id: &str,
) -> Result<String, MemoriaError> {
    let db_name = DbRouter::user_db_name_for_id(user_id);
    create_database_if_missing(shared_pool, &db_name).await?;
    let now = Utc::now().naive_utc();
    let insert = sqlx::query(
        r#"INSERT INTO mem_user_registry (user_id, db_name, status, created_at, updated_at)
           VALUES (?, ?, 'active', ?, ?)"#,
    )
    .bind(user_id)
    .bind(&db_name)
    .bind(now)
    .bind(now)
    .execute(shared_pool)
    .await;

    match insert {
        Ok(_) => Ok(db_name),
        Err(e) if is_duplicate_key_error(&e) => {
            let existing = sqlx::query("SELECT db_name FROM mem_user_registry WHERE user_id = ?")
                .bind(user_id)
                .fetch_optional(shared_pool)
                .await
                .map_err(db_err)?;

            let Some(existing) = existing else {
                return Err(MemoriaError::Internal(format!(
                    "user db registration collision for {user_id} -> {db_name}"
                )));
            };

            let existing_db_name: String = existing.try_get("db_name").map_err(db_err)?;
            if existing_db_name != db_name {
                return Err(MemoriaError::Internal(format!(
                    "user {user_id} already registered to {existing_db_name}, expected {db_name}"
                )));
            }

            sqlx::query(
                "UPDATE mem_user_registry SET status = 'active', updated_at = ? WHERE user_id = ?",
            )
            .bind(now)
            .bind(user_id)
            .execute(shared_pool)
            .await
            .map_err(db_err)?;

            Ok(existing_db_name)
        }
        Err(e) => Err(db_err(e)),
    }
}

async fn create_database_if_missing(
    shared_pool: &MySqlPool,
    db_name: &str,
) -> Result<(), MemoriaError> {
    sqlx::raw_sql(&format!(
        "CREATE DATABASE IF NOT EXISTS {}",
        quote_ident(db_name)
    ))
    .execute(shared_pool)
    .await
    .map_err(db_err)?;
    Ok(())
}

async fn create_database_if_missing_from_url(database_url: &str) -> Result<(), MemoriaError> {
    let Some((base_url, db_name, _suffix)) = split_database_url(database_url) else {
        return Err(MemoriaError::Internal(
            "database_url missing db name".into(),
        ));
    };
    let base_pool = MySqlPoolOptions::new()
        .max_connections(1)
        .connect(base_url)
        .await
        .map_err(db_err)?;
    create_database_if_missing(&base_pool, db_name).await
}

fn parse_db_name(database_url: &str) -> Option<String> {
    split_database_url(database_url).map(|(_, db_name, _)| db_name.to_string())
}

fn replace_db_name(database_url: &str, db_name: &str) -> Option<String> {
    let (base, _, suffix) = split_database_url(database_url)?;
    Some(format!("{base}/{db_name}{suffix}"))
}

fn split_database_url(database_url: &str) -> Option<(&str, &str, &str)> {
    let suffix_start = database_url.find(['?', '#']).unwrap_or(database_url.len());
    let (without_suffix, suffix) = database_url.split_at(suffix_start);
    let (base, db_name) = without_suffix.rsplit_once('/')?;
    if db_name.is_empty() {
        return None;
    }
    Some((base, db_name, suffix))
}

fn quote_ident(name: &str) -> String {
    format!("`{}`", name.replace('`', "``"))
}

fn append_url_param(url: &str, param: &str) -> String {
    if url.contains('?') {
        format!("{url}&{param}")
    } else {
        format!("{url}?{param}")
    }
}

fn configured_shared_pool_plan() -> SharedPoolPlan {
    let routing_component_max_connections = configured_pool_max_connections(
        "MEMORIA_SHARED_POOL_MAX_CONNECTIONS",
        SHARED_POOL_MAX_CONNECTIONS,
        POOL_MAX_CONNECTIONS_UPPER,
    );
    let shared_main_component_max_connections = configured_pool_max_connections(
        "MEMORIA_SHARED_MAIN_POOL_MAX_CONNECTIONS",
        SHARED_MAIN_POOL_MAX_CONNECTIONS,
        POOL_MAX_CONNECTIONS_UPPER,
    );
    let git_component_max_connections = configured_pool_max_connections(
        "MEMORIA_GIT_POOL_MAX_CONNECTIONS",
        GIT_POOL_MAX_CONNECTIONS,
        64,
    );
    let explicit_override = std::env::var(MERGED_SHARED_POOL_MAX_CONNECTIONS_ENV)
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .map(|max_connections| max_connections.clamp(1, POOL_MAX_CONNECTIONS_UPPER));
    let max_connections = explicit_override.unwrap_or_else(|| {
        routing_component_max_connections
            .saturating_add(shared_main_component_max_connections)
            .saturating_add(git_component_max_connections)
            .clamp(1, POOL_MAX_CONNECTIONS_UPPER)
    });
    SharedPoolPlan {
        max_connections,
        routing_component_max_connections,
        shared_main_component_max_connections,
        git_component_max_connections,
        explicit_override: explicit_override.is_some(),
    }
}

fn configured_pool_max_connections(env_name: &str, default: u32, upper: u32) -> u32 {
    std::env::var(env_name)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
        .clamp(1, upper)
}

#[cfg(test)]
mod tests {
    use super::{
        configured_pool_max_connections, configured_shared_pool_plan, SharedPoolPlan,
        GIT_POOL_MAX_CONNECTIONS, GLOBAL_USER_POOL_MAX_CONNECTIONS,
        MERGED_SHARED_POOL_MAX_CONNECTIONS_ENV, POOL_MAX_CONNECTIONS_UPPER,
        SHARED_MAIN_POOL_MAX_CONNECTIONS, SHARED_POOL_MAX_CONNECTIONS,
        USER_SCHEMA_INIT_POOL_MAX_CONNECTIONS,
    };
    use std::sync::{Mutex, OnceLock};

    static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    fn with_env<F: FnOnce()>(vars: &[(&str, Option<&str>)], f: F) {
        let _lock = ENV_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        struct EnvGuard(Vec<(String, Option<std::ffi::OsString>)>);

        impl Drop for EnvGuard {
            fn drop(&mut self) {
                for (key, old) in &self.0 {
                    match old {
                        Some(value) => unsafe { std::env::set_var(key, value) },
                        None => unsafe { std::env::remove_var(key) },
                    }
                }
            }
        }

        let _restore = EnvGuard(
            vars.iter()
                .map(|(key, value)| {
                    let old = std::env::var_os(key);
                    match value {
                        Some(value) => unsafe { std::env::set_var(key, value) },
                        None => unsafe { std::env::remove_var(key) },
                    }
                    (key.to_string(), old)
                })
                .collect(),
        );

        f();
    }

    #[test]
    fn shared_pool_plan_defaults_merge_legacy_components() {
        with_env(
            &[
                ("MEMORIA_SHARED_POOL_MAX_CONNECTIONS", None),
                ("MEMORIA_SHARED_MAIN_POOL_MAX_CONNECTIONS", None),
                ("MEMORIA_GIT_POOL_MAX_CONNECTIONS", None),
                (MERGED_SHARED_POOL_MAX_CONNECTIONS_ENV, None),
            ],
            || {
                assert_eq!(
                    configured_shared_pool_plan(),
                    SharedPoolPlan {
                        max_connections: SHARED_POOL_MAX_CONNECTIONS
                            + SHARED_MAIN_POOL_MAX_CONNECTIONS
                            + GIT_POOL_MAX_CONNECTIONS,
                        routing_component_max_connections: SHARED_POOL_MAX_CONNECTIONS,
                        shared_main_component_max_connections: SHARED_MAIN_POOL_MAX_CONNECTIONS,
                        git_component_max_connections: GIT_POOL_MAX_CONNECTIONS,
                        explicit_override: false,
                    }
                );
            },
        );
    }

    #[test]
    fn shared_pool_plan_honors_explicit_merged_override() {
        with_env(
            &[
                ("MEMORIA_SHARED_POOL_MAX_CONNECTIONS", Some("20")),
                ("MEMORIA_SHARED_MAIN_POOL_MAX_CONNECTIONS", Some("10")),
                ("MEMORIA_GIT_POOL_MAX_CONNECTIONS", Some("6")),
                (MERGED_SHARED_POOL_MAX_CONNECTIONS_ENV, Some("40")),
            ],
            || {
                assert_eq!(
                    configured_shared_pool_plan(),
                    SharedPoolPlan {
                        max_connections: 40,
                        routing_component_max_connections: 20,
                        shared_main_component_max_connections: 10,
                        git_component_max_connections: 6,
                        explicit_override: true,
                    }
                );
            },
        );
    }

    #[test]
    fn shared_pool_plan_clamps_explicit_merged_override() {
        with_env(
            &[
                ("MEMORIA_SHARED_POOL_MAX_CONNECTIONS", Some("20")),
                ("MEMORIA_SHARED_MAIN_POOL_MAX_CONNECTIONS", Some("10")),
                ("MEMORIA_GIT_POOL_MAX_CONNECTIONS", Some("6")),
                (MERGED_SHARED_POOL_MAX_CONNECTIONS_ENV, Some("0")),
            ],
            || {
                assert_eq!(
                    configured_shared_pool_plan(),
                    SharedPoolPlan {
                        max_connections: 1,
                        routing_component_max_connections: 20,
                        shared_main_component_max_connections: 10,
                        git_component_max_connections: 6,
                        explicit_override: true,
                    }
                );
            },
        );
    }

    #[test]
    fn user_pool_defaults_match_budget_split() {
        with_env(
            &[
                ("MEMORIA_GLOBAL_USER_POOL_MAX", None),
                ("MEMORIA_USER_SCHEMA_INIT_POOL_MAX_CONNECTIONS", None),
            ],
            || {
                assert_eq!(
                    configured_pool_max_connections(
                        "MEMORIA_GLOBAL_USER_POOL_MAX",
                        GLOBAL_USER_POOL_MAX_CONNECTIONS,
                        POOL_MAX_CONNECTIONS_UPPER,
                    ),
                    GLOBAL_USER_POOL_MAX_CONNECTIONS
                );
                assert_eq!(
                    configured_pool_max_connections(
                        "MEMORIA_USER_SCHEMA_INIT_POOL_MAX_CONNECTIONS",
                        USER_SCHEMA_INIT_POOL_MAX_CONNECTIONS,
                        64,
                    ),
                    USER_SCHEMA_INIT_POOL_MAX_CONNECTIONS
                );
            },
        );
    }

    #[test]
    fn user_schema_init_concurrency_defaults_to_six() {
        with_env(
            &[("MEMORIA_USER_SCHEMA_INIT_MAX_CONCURRENCY", None)],
            || {
                let user_init_max: usize =
                    std::env::var("MEMORIA_USER_SCHEMA_INIT_MAX_CONCURRENCY")
                        .ok()
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(6)
                        .clamp(1, 64);
                assert_eq!(user_init_max, 6);
            },
        );
    }
}
