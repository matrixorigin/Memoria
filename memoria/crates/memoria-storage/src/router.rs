use crate::SqlMemoryStore;
use chrono::Utc;
use memoria_core::MemoriaError;
use moka::future::Cache;
use sha2::{Digest, Sha256};
use sqlx::{mysql::MySqlPoolOptions, MySqlPool, Row};
use std::sync::Arc;

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
const USER_STORE_CACHE_MAX_CAPACITY: u64 = 128;
const USER_STORE_CACHE_IDLE_SECS: u64 = 600;

#[derive(Debug, Clone)]
pub struct UserDatabaseRecord {
    pub user_id: String,
    pub db_name: String,
    pub status: String,
}

#[derive(Clone)]
pub struct DbRouter {
    shared_pool: MySqlPool,
    shared_db_url: String,
    shared_db_name: String,
    embedding_dim: usize,
    instance_id: String,
    user_db_cache: Cache<String, String>,
    user_store_cache: Cache<String, Arc<SqlMemoryStore>>,
}

impl DbRouter {
    pub async fn connect(
        shared_db_url: &str,
        embedding_dim: usize,
        instance_id: String,
    ) -> Result<Self, MemoriaError> {
        create_database_if_missing(shared_db_url).await?;
        let pool = MySqlPoolOptions::new()
            .max_connections(16)
            .max_lifetime(std::time::Duration::from_secs(3600))
            .idle_timeout(std::time::Duration::from_secs(300))
            .acquire_timeout(std::time::Duration::from_secs(10))
            .connect(shared_db_url)
            .await
            .map_err(db_err)?;
        let shared_db_name = parse_db_name(shared_db_url)
            .ok_or_else(|| MemoriaError::Internal("invalid shared_db_url".into()))?;
        let router = Self {
            shared_pool: pool,
            shared_db_url: shared_db_url.to_string(),
            shared_db_name,
            embedding_dim,
            instance_id,
            user_db_cache: Cache::builder()
                .max_capacity(USER_DB_CACHE_MAX_CAPACITY)
                .time_to_live(std::time::Duration::from_secs(300))
                .build(),
            user_store_cache: Cache::builder()
                .max_capacity(USER_STORE_CACHE_MAX_CAPACITY)
                .time_to_idle(std::time::Duration::from_secs(USER_STORE_CACHE_IDLE_SECS))
                .build(),
        };
        router.ensure_user_registry_table().await?;
        Ok(router)
    }

    pub fn shared_pool(&self) -> &MySqlPool {
        &self.shared_pool
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
        self.user_db_cache
            .try_get_with_by_ref(user_id, async {
                let row = sqlx::query(
                    "SELECT db_name FROM mem_user_registry WHERE user_id = ? AND status = 'active'",
                )
                .bind(user_id)
                .fetch_optional(&self.shared_pool)
                .await
                .map_err(db_err)?;
                match row {
                    Some(row) => row.try_get("db_name").map_err(db_err),
                    None => self.provision_user_db(user_id).await,
                }
            })
            .await
            .map_err(|err: Arc<MemoriaError>| (*err).clone())
    }

    pub async fn user_store(&self, user_id: &str) -> Result<Arc<SqlMemoryStore>, MemoriaError> {
        self.user_store_cache
            .try_get_with_by_ref(user_id, async {
                let db_name = self.user_db_name(user_id).await?;
                let db_url = self.user_db_url(&db_name)?;
                let store = Arc::new(
                    SqlMemoryStore::connect_routed(
                        &db_url,
                        self.embedding_dim,
                        self.instance_id.clone(),
                    )
                    .await?,
                );
                store.migrate_user().await?;
                Ok(store)
            })
            .await
            .map_err(|err: Arc<MemoriaError>| (*err).clone())
    }

    pub async fn invalidate_user(&self, user_id: &str) {
        self.user_db_cache.invalidate(user_id).await;
        self.user_store_cache.invalidate(user_id).await;
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

    async fn provision_user_db(&self, user_id: &str) -> Result<String, MemoriaError> {
        let db_name = Self::user_db_name_for_id(user_id);
        let db_url = self.user_db_url(&db_name)?;
        create_database_if_missing(&db_url).await?;
        let now = Utc::now().naive_utc();
        let insert = sqlx::query(
            r#"INSERT INTO mem_user_registry (user_id, db_name, status, created_at, updated_at)
               VALUES (?, ?, 'active', ?, ?)"#,
        )
        .bind(user_id)
        .bind(&db_name)
        .bind(now)
        .bind(now)
        .execute(&self.shared_pool)
        .await;

        match insert {
            Ok(_) => Ok(db_name),
            Err(e) if is_duplicate_key_error(&e) => {
                let existing =
                    sqlx::query("SELECT db_name FROM mem_user_registry WHERE user_id = ?")
                        .bind(user_id)
                        .fetch_optional(&self.shared_pool)
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
                .execute(&self.shared_pool)
                .await
                .map_err(db_err)?;

                Ok(existing_db_name)
            }
            Err(e) => Err(db_err(e)),
        }
    }
}

async fn create_database_if_missing(database_url: &str) -> Result<(), MemoriaError> {
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
    sqlx::raw_sql(&format!(
        "CREATE DATABASE IF NOT EXISTS {}",
        quote_ident(db_name)
    ))
    .execute(&base_pool)
    .await
    .map_err(db_err)?;
    Ok(())
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
