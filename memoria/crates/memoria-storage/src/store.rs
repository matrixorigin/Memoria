use async_trait::async_trait;
use chrono::Utc;
use memoria_core::{interfaces::MemoryStore, MemoriaError, Memory, MemoryType, TrustTier};
use sqlx::{mysql::MySqlPool, Row};
use std::str::FromStr;

pub(crate) fn db_err(e: sqlx::Error) -> MemoriaError {
    if matches!(&e, sqlx::Error::PoolTimedOut) {
        tracing::error!("pool timed out while waiting for an open connection");
    }
    MemoriaError::Database(e.to_string())
}

/// Spawn a background task that periodically logs pool utilization.
/// Warns when idle connections drop below 10% of pool size.
/// Stops automatically when the pool is closed.
pub fn spawn_pool_monitor(pool: MySqlPool) {
    ::tokio::spawn(async move {
        let mut interval = ::tokio::time::interval(std::time::Duration::from_secs(30));
        interval.tick().await; // skip immediate
        loop {
            interval.tick().await;
            if pool.is_closed() {
                tracing::debug!("pool monitor stopping — pool closed");
                break;
            }
            let size = pool.size();
            let idle = pool.num_idle();
            let active = size - idle as u32;
            if idle == 0 {
                tracing::warn!(
                    pool_size = size,
                    pool_active = active,
                    pool_idle = idle,
                    "connection pool saturated — 0 idle connections"
                );
            } else if (idle as u32) < size / 10 + 1 {
                tracing::warn!(
                    pool_size = size,
                    pool_active = active,
                    pool_idle = idle,
                    "connection pool high utilization"
                );
            }
        }
    });
}

/// One entry for [`SqlMemoryStore::batch_log_edit`].
/// Fields: `(user_id, operation, memory_id, payload, reason, snapshot_before)`.
pub type EditLogEntry<'a> = (
    &'a str,
    &'a str,
    Option<&'a str>,
    Option<&'a str>,
    &'a str,
    Option<&'a str>,
);

/// Generate a UUID v7 (time-ordered) as a simple hex string.
fn uuid7_id() -> String {
    uuid::Uuid::now_v7().simple().to_string()
}

/// Sanitize a string for safe interpolation inside a SQL single-quoted literal.
/// Escapes `'`, `\`, and strips NUL bytes.
#[allow(dead_code)]
fn sanitize_sql_literal(s: &str) -> String {
    s.chars()
        .filter(|c| *c != '\0')
        .map(|c| match c {
            '\'' => ' ',
            '\\' => ' ',
            _ => c,
        })
        .collect()
}

/// Sanitize a string for use inside MATCH ... AGAINST('...' IN BOOLEAN MODE).
/// Strips boolean-mode operators and SQL-injection characters.
fn sanitize_fulltext_query(s: &str) -> String {
    s.chars()
        .filter(|c| *c != '\0')
        .map(|c| match c {
            '\'' | '\\' | '"' | '+' | '-' | '<' | '>' | '(' | ')' | '~' | '*' | '@' => ' ',
            _ => c,
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Sanitize a string for use in a LIKE pattern (escapes `%`).
fn sanitize_like_pattern(s: &str) -> String {
    s.chars()
        .filter(|c| *c != '\0')
        .map(|c| match c {
            '\'' | '\\' => ' ',
            '%' => ' ',
            _ => c,
        })
        .collect()
}

fn vec_to_mo(v: &[f32]) -> String {
    format!(
        "[{}]",
        v.iter()
            .map(|f| f.to_string())
            .collect::<Vec<_>>()
            .join(",")
    )
}

fn mo_to_vec(s: &str) -> Result<Vec<f32>, MemoriaError> {
    let inner = s.trim_matches(|c| c == '[' || c == ']');
    if inner.is_empty() {
        return Ok(vec![]);
    }
    inner
        .split(',')
        .map(|x| {
            x.trim()
                .parse::<f32>()
                .map_err(|e| MemoriaError::Internal(format!("vec parse: {e}")))
        })
        .collect()
}

#[derive(Clone)]
pub struct SqlMemoryStore {
    pool: MySqlPool,
    embedding_dim: usize,
    instance_id: String,
    database_url: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SnapshotRegistration {
    pub name: String,
    pub snapshot_name: String,
    pub created_at: chrono::NaiveDateTime,
}

/// Aggregated feedback statistics for a user.
#[derive(Debug, Clone, serde::Serialize)]
pub struct FeedbackStats {
    pub total: i64,
    pub useful: i64,
    pub irrelevant: i64,
    pub outdated: i64,
    pub wrong: i64,
}

/// Feedback breakdown by trust tier.
#[derive(Debug, Clone, serde::Serialize)]
pub struct TierFeedback {
    pub tier: String,
    pub signal: String,
    pub count: i64,
}

/// Feedback counts for a single memory (denormalized, no JOIN needed).
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct MemoryFeedback {
    pub useful: i32,
    pub irrelevant: i32,
    pub outdated: i32,
    pub wrong: i32,
}

/// Per-user adaptive retrieval parameters.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct UserRetrievalParams {
    pub user_id: String,
    pub feedback_weight: f64,
    pub temporal_decay_hours: f64,
    pub confidence_weight: f64,
}

impl Default for UserRetrievalParams {
    fn default() -> Self {
        Self {
            user_id: String::new(),
            feedback_weight: 0.1,
            temporal_decay_hours: 168.0,
            confidence_weight: 0.1,
        }
    }
}

impl SqlMemoryStore {
    pub fn new(pool: MySqlPool, embedding_dim: usize, instance_id: String) -> Self {
        Self {
            pool,
            embedding_dim,
            instance_id,
            database_url: None,
        }
    }

    pub fn pool(&self) -> &MySqlPool {
        &self.pool
    }

    pub fn graph_store(&self) -> crate::graph::GraphStore {
        crate::graph::GraphStore::new(self.pool.clone(), self.embedding_dim)
    }

    pub async fn connect(
        database_url: &str,
        embedding_dim: usize,
        instance_id: String,
    ) -> Result<Self, MemoriaError> {
        // Auto-create database if it doesn't exist
        if let Some((base_url, db_name)) = database_url.rsplit_once('/') {
            if !db_name.is_empty() {
                let base_pool = sqlx::mysql::MySqlPoolOptions::new()
                    .max_connections(1)
                    .connect(base_url)
                    .await;
                if let Ok(base_pool) = base_pool {
                    let _ = sqlx::query(&format!("CREATE DATABASE IF NOT EXISTS {db_name}"))
                        .execute(&base_pool)
                        .await;
                }
            }
        }
        const DB_MAX_CONNECTIONS_UPPER: u32 = 512;
        let max_conns: u32 = std::env::var("DB_MAX_CONNECTIONS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(64)
            .clamp(1, DB_MAX_CONNECTIONS_UPPER);
        let max_lifetime_secs: u64 = std::env::var("DB_MAX_LIFETIME_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(3600);
        let pool = sqlx::mysql::MySqlPoolOptions::new()
            .max_connections(max_conns)
            .max_lifetime(std::time::Duration::from_secs(max_lifetime_secs))
            .idle_timeout(std::time::Duration::from_secs(300))
            .acquire_timeout(std::time::Duration::from_secs(10))
            .connect(database_url)
            .await
            .map_err(db_err)?;
        tracing::info!(
            max_connections = max_conns,
            max_lifetime_secs = max_lifetime_secs,
            "Main connection pool initialized"
        );
        spawn_pool_monitor(pool.clone());
        let mut store = Self::new(pool, embedding_dim, instance_id);
        store.database_url = Some(database_url.to_string());
        Ok(store)
    }

    /// Create a small isolated pool for background tasks (DDL, maintenance).
    /// Returns an error if no database_url was stored or if pool creation fails.
    pub async fn spawn_background_store(
        &self,
        max_connections: u32,
    ) -> Result<std::sync::Arc<Self>, MemoriaError> {
        let url = self.database_url.as_deref().ok_or_else(|| {
            MemoriaError::Internal("background pool requires database_url".into())
        })?;
        match sqlx::mysql::MySqlPoolOptions::new()
            .max_connections(max_connections)
            .max_lifetime(std::time::Duration::from_secs(3600))
            .idle_timeout(std::time::Duration::from_secs(300))
            .acquire_timeout(std::time::Duration::from_secs(30))
            .connect(url)
            .await
        {
            Ok(pool) => {
                tracing::info!(
                    max_connections = max_connections,
                    "Background connection pool initialized"
                );
                let mut s = Self::new(pool, self.embedding_dim, self.instance_id.clone());
                s.database_url = self.database_url.clone();
                Ok(std::sync::Arc::new(s))
            }
            Err(e) => Err(db_err(e)),
        }
    }

    pub async fn migrate(&self) -> Result<(), MemoriaError> {
        // mem_memories
        let sql = format!(
            r#"CREATE TABLE IF NOT EXISTS mem_memories (
                memory_id       VARCHAR(64)  PRIMARY KEY,
                user_id         VARCHAR(64)  NOT NULL,
                memory_type     VARCHAR(20)  NOT NULL,
                content         TEXT         NOT NULL,
                embedding       vecf32({dim}),
                session_id      VARCHAR(64),
                source_event_ids JSON        NOT NULL,
                extra_metadata  JSON, -- MO#23859: NULL avoided at bind level
                is_active       TINYINT(1)   NOT NULL DEFAULT 1,
                superseded_by   VARCHAR(64),
                trust_tier      VARCHAR(10)  DEFAULT 'T1',
                initial_confidence FLOAT     DEFAULT 0.95,
                observed_at     DATETIME(6)  NOT NULL,
                created_at      DATETIME(6)  NOT NULL,
                updated_at      DATETIME(6),
                INDEX idx_user_active (user_id, is_active, memory_type),
                INDEX idx_user_session (user_id, session_id),
                FULLTEXT INDEX ft_content (content) WITH PARSER ngram -- MO#23861: breaks on concurrent snapshot restore
            )"#,
            dim = self.embedding_dim
        );
        sqlx::query(&sql)
            .execute(&self.pool)
            .await
            .map_err(db_err)?;

        // mem_user_state — active branch per user
        sqlx::query(
            r#"CREATE TABLE IF NOT EXISTS mem_user_state (
                user_id       VARCHAR(64)  PRIMARY KEY,
                active_branch VARCHAR(100) NOT NULL DEFAULT 'main',
                updated_at    DATETIME(6)
            )"#,
        )
        .execute(&self.pool)
        .await
        .map_err(db_err)?;

        // mem_branches — branch registry
        sqlx::query(
            r#"CREATE TABLE IF NOT EXISTS mem_branches (
                id          VARCHAR(64)  PRIMARY KEY,
                user_id     VARCHAR(64)  NOT NULL,
                name        VARCHAR(100) NOT NULL,
                table_name  VARCHAR(100) NOT NULL,
                status      VARCHAR(20)  NOT NULL DEFAULT 'active',
                created_at  DATETIME(6)  NOT NULL,
                INDEX idx_user_name (user_id, name)
            )"#,
        )
        .execute(&self.pool)
        .await
        .map_err(db_err)?;

        // mem_snapshots — user snapshot registry
        sqlx::query(
            r#"CREATE TABLE IF NOT EXISTS mem_snapshots (
                id             VARCHAR(64)  PRIMARY KEY,
                user_id        VARCHAR(64)  NOT NULL,
                name           VARCHAR(100) NOT NULL,
                snapshot_name  VARCHAR(100) NOT NULL,
                status         VARCHAR(20)  NOT NULL DEFAULT 'active',
                created_at     DATETIME(6)  NOT NULL,
                INDEX idx_user_snapshot_name (user_id, name, status),
                INDEX idx_user_snapshot_internal (user_id, snapshot_name, status)
            )"#,
        )
        .execute(&self.pool)
        .await
        .map_err(db_err)?;

        // mem_governance_cooldown — per-user cooldown tracking
        sqlx::query(
            r#"CREATE TABLE IF NOT EXISTS mem_governance_cooldown (
                user_id     VARCHAR(64)  NOT NULL,
                operation   VARCHAR(32)  NOT NULL,
                last_run_at DATETIME(6)  NOT NULL,
                PRIMARY KEY (user_id, operation)
            )"#,
        )
        .execute(&self.pool)
        .await
        .map_err(db_err)?;

        sqlx::query(
            r#"CREATE TABLE IF NOT EXISTS mem_governance_runtime_state (
                strategy_key       VARCHAR(128) NOT NULL,
                task               VARCHAR(32)  NOT NULL,
                failure_count      INT          NOT NULL DEFAULT 0,
                circuit_open_until DATETIME(6)  DEFAULT NULL,
                updated_at         DATETIME(6)  NOT NULL,
                PRIMARY KEY (strategy_key, task)
            )"#,
        )
        .execute(&self.pool)
        .await
        .map_err(db_err)?;

        sqlx::query(
            r#"CREATE TABLE IF NOT EXISTS mem_plugin_signers (
                signer       VARCHAR(128) PRIMARY KEY,
                algorithm    VARCHAR(32)  NOT NULL,
                public_key   TEXT         NOT NULL,
                is_active    TINYINT(1)   NOT NULL DEFAULT 1,
                created_at   DATETIME(6)  NOT NULL,
                updated_at   DATETIME(6)  NOT NULL,
                created_by   VARCHAR(64)  NOT NULL
            )"#,
        )
        .execute(&self.pool)
        .await
        .map_err(db_err)?;

        sqlx::query(
            r#"CREATE TABLE IF NOT EXISTS mem_plugin_packages (
                plugin_key      VARCHAR(128) NOT NULL,
                version         VARCHAR(32)  NOT NULL,
                domain          VARCHAR(32)  NOT NULL,
                name            VARCHAR(128) NOT NULL,
                runtime         VARCHAR(32)  NOT NULL,
                manifest_json   TEXT         NOT NULL,
                package_payload LONGTEXT     NOT NULL,
                sha256          VARCHAR(128) NOT NULL,
                signature       TEXT         NOT NULL,
                signer          VARCHAR(128) NOT NULL,
                status          VARCHAR(16)  NOT NULL DEFAULT 'active',
                published_at    DATETIME(6)  NOT NULL,
                published_by    VARCHAR(64)  NOT NULL,
                PRIMARY KEY (plugin_key, version),
                INDEX idx_plugin_domain_status (domain, status),
                INDEX idx_plugin_signer (signer)
            )"#,
        )
        .execute(&self.pool)
        .await
        .map_err(db_err)?;

        sqlx::query(
            r#"CREATE TABLE IF NOT EXISTS mem_plugin_bindings (
                domain      VARCHAR(32)  NOT NULL,
                binding_key VARCHAR(64)  NOT NULL,
                plugin_key  VARCHAR(128) NOT NULL,
                version     VARCHAR(32)  NOT NULL,
                updated_at  DATETIME(6)  NOT NULL,
                updated_by  VARCHAR(64)  NOT NULL,
                PRIMARY KEY (domain, binding_key)
            )"#,
        )
        .execute(&self.pool)
        .await
        .map_err(db_err)?;

        sqlx::query(
            r#"CREATE TABLE IF NOT EXISTS mem_plugin_reviews (
                plugin_key    VARCHAR(128) NOT NULL,
                version       VARCHAR(32)  NOT NULL,
                review_status VARCHAR(16)  NOT NULL DEFAULT 'pending',
                score         DOUBLE       NOT NULL DEFAULT 0,
                review_notes  TEXT         NOT NULL,
                reviewed_at   DATETIME(6)  NOT NULL,
                reviewed_by   VARCHAR(64)  NOT NULL,
                PRIMARY KEY (plugin_key, version)
            )"#,
        )
        .execute(&self.pool)
        .await
        .map_err(db_err)?;

        sqlx::query(
            r#"CREATE TABLE IF NOT EXISTS mem_plugin_binding_rules (
                rule_id             VARCHAR(64)  PRIMARY KEY,
                domain              VARCHAR(32)  NOT NULL,
                binding_key         VARCHAR(64)  NOT NULL,
                subject_key         VARCHAR(128) NOT NULL,
                priority            BIGINT       NOT NULL DEFAULT 100,
                plugin_key          VARCHAR(128) NOT NULL,
                selector_kind       VARCHAR(16)  NOT NULL,
                selector_value      VARCHAR(64)  NOT NULL,
                rollout_percent     BIGINT       NOT NULL DEFAULT 100,
                transport_endpoint  TEXT         NOT NULL,
                status              VARCHAR(16)  NOT NULL DEFAULT 'active',
                updated_at          DATETIME(6)  NOT NULL,
                updated_by          VARCHAR(64)  NOT NULL,
                UNIQUE KEY uniq_binding_rule (domain, binding_key, subject_key, priority)
            )"#,
        )
        .execute(&self.pool)
        .await
        .map_err(db_err)?;

        sqlx::query(
            r#"CREATE TABLE IF NOT EXISTS mem_plugin_audit_events (
                event_id      VARCHAR(64)  PRIMARY KEY,
                domain        VARCHAR(32)  NOT NULL,
                binding_key   VARCHAR(64)  NOT NULL,
                subject_key   VARCHAR(128) NOT NULL,
                plugin_key    VARCHAR(128) NOT NULL,
                version       VARCHAR(32)  NOT NULL,
                event_type    VARCHAR(32)  NOT NULL,
                status        VARCHAR(16)  NOT NULL,
                message       TEXT         NOT NULL,
                metadata_json JSON         NOT NULL,
                created_at    DATETIME(6)  NOT NULL,
                actor         VARCHAR(64)  NOT NULL,
                INDEX idx_plugin_audit_lookup (domain, binding_key, plugin_key, created_at)
            )"#,
        )
        .execute(&self.pool)
        .await
        .map_err(db_err)?;

        // mem_entity_links — entity graph (lightweight, no graph tables)
        sqlx::query(
            r#"CREATE TABLE IF NOT EXISTS mem_entity_links (
                id          VARCHAR(64)  PRIMARY KEY,
                user_id     VARCHAR(64)  NOT NULL,
                memory_id   VARCHAR(64)  NOT NULL,
                entity_name VARCHAR(200) NOT NULL,
                entity_type VARCHAR(50)  NOT NULL DEFAULT 'concept',
                source      VARCHAR(20)  NOT NULL DEFAULT 'manual',
                created_at  DATETIME(6)  NOT NULL,
                INDEX idx_user_memory (user_id, memory_id),
                INDEX idx_user_entity (user_id, entity_name)
            )"#,
        )
        .execute(&self.pool)
        .await
        .map_err(db_err)?;

        // mem_memories_stats — access_count + feedback tracking (separated to reduce write contention)
        sqlx::query(
            r#"CREATE TABLE IF NOT EXISTS mem_memories_stats (
                memory_id        VARCHAR(64)  PRIMARY KEY,
                access_count     INT          NOT NULL DEFAULT 0,
                last_accessed_at DATETIME(6),
                feedback_useful  INT          NOT NULL DEFAULT 0,
                feedback_irrelevant INT       NOT NULL DEFAULT 0,
                feedback_outdated INT         NOT NULL DEFAULT 0,
                feedback_wrong   INT          NOT NULL DEFAULT 0,
                last_feedback_at DATETIME(6)
            )"#,
        )
        .execute(&self.pool)
        .await
        .map_err(db_err)?;

        // Migration: add feedback columns to existing mem_memories_stats
        let _ = sqlx::query(
            "ALTER TABLE mem_memories_stats ADD COLUMN feedback_useful INT NOT NULL DEFAULT 0",
        )
        .execute(&self.pool)
        .await;
        let _ = sqlx::query(
            "ALTER TABLE mem_memories_stats ADD COLUMN feedback_irrelevant INT NOT NULL DEFAULT 0",
        )
        .execute(&self.pool)
        .await;
        let _ = sqlx::query(
            "ALTER TABLE mem_memories_stats ADD COLUMN feedback_outdated INT NOT NULL DEFAULT 0",
        )
        .execute(&self.pool)
        .await;
        let _ = sqlx::query(
            "ALTER TABLE mem_memories_stats ADD COLUMN feedback_wrong INT NOT NULL DEFAULT 0",
        )
        .execute(&self.pool)
        .await;
        let _ =
            sqlx::query("ALTER TABLE mem_memories_stats ADD COLUMN last_feedback_at DATETIME(6)")
                .execute(&self.pool)
                .await;

        // mem_edit_log — append-only audit log for inject/correct/purge/governance
        sqlx::query(
            r#"CREATE TABLE IF NOT EXISTS mem_edit_log (
                edit_id         VARCHAR(64)  NOT NULL,
                user_id         VARCHAR(64)  NOT NULL,
                memory_id       VARCHAR(64)  DEFAULT NULL,
                operation       VARCHAR(64)  NOT NULL,
                payload         JSON         DEFAULT NULL,
                reason          TEXT         DEFAULT NULL,
                snapshot_before VARCHAR(64)  DEFAULT NULL,
                created_at      DATETIME(6)  NOT NULL DEFAULT NOW(),
                created_by      VARCHAR(64)  NOT NULL,
                INDEX idx_user_time (user_id, created_at),
                INDEX idx_memory_time (memory_id, created_at)
            ) CLUSTER BY (created_at, user_id)"#,
        )
        .execute(&self.pool)
        .await
        .map_err(db_err)?;

        // Migration: add memory_id column to existing mem_edit_log tables
        let _ =
            sqlx::query("ALTER TABLE mem_edit_log ADD COLUMN memory_id VARCHAR(64) DEFAULT NULL")
                .execute(&self.pool)
                .await;

        // Migration: add payload column to existing mem_edit_log tables
        let _ = sqlx::query("ALTER TABLE mem_edit_log ADD COLUMN payload JSON DEFAULT NULL")
            .execute(&self.pool)
            .await;

        // Graph tables
        self.graph_store().migrate().await?;

        // ── Distributed coordination tables ───────────────────────────────────

        // mem_distributed_locks — DB-based mutual exclusion for multi-instance
        sqlx::query(
            r#"CREATE TABLE IF NOT EXISTS mem_distributed_locks (
                lock_key    VARCHAR(128) PRIMARY KEY,
                holder_id   VARCHAR(128) NOT NULL,
                acquired_at DATETIME(6)  NOT NULL,
                expires_at  DATETIME(6)  NOT NULL,
                INDEX idx_lock_expires (expires_at)
            )"#,
        )
        .execute(&self.pool)
        .await
        .map_err(db_err)?;

        // mem_async_tasks — cross-instance async task tracking
        sqlx::query(
            r#"CREATE TABLE IF NOT EXISTS mem_async_tasks (
                task_id     VARCHAR(64)  PRIMARY KEY,
                instance_id VARCHAR(128) NOT NULL,
                user_id     VARCHAR(64)  NOT NULL DEFAULT '',
                status      VARCHAR(16)  NOT NULL DEFAULT 'processing',
                result_json JSON         DEFAULT NULL,
                error_json  JSON         DEFAULT NULL,
                created_at  DATETIME(6)  NOT NULL,
                updated_at  DATETIME(6)  NOT NULL,
                INDEX idx_task_status (status, created_at)
            )"#,
        )
        .execute(&self.pool)
        .await
        .map_err(db_err)?;

        // Backfill user_id column for existing deployments (ignore if already exists)
        let _ = sqlx::query(
            "ALTER TABLE mem_async_tasks ADD COLUMN user_id VARCHAR(64) NOT NULL DEFAULT '' AFTER instance_id",
        )
        .execute(&self.pool)
        .await;

        // Backfill extra_metadata column for existing deployments (ignore if already exists)
        let _ = sqlx::query(
            "ALTER TABLE mem_memories ADD COLUMN extra_metadata JSON AFTER source_event_ids",
        )
        .execute(&self.pool)
        .await;

        // Migrate idx_user_active to include memory_type (idempotent)
        let needs_upgrade: bool = sqlx::query_scalar(
            "SELECT COUNT(*) = 0 FROM information_schema.statistics \
             WHERE table_schema = DATABASE() AND table_name = 'mem_memories' \
             AND index_name = 'idx_user_active' AND column_name = 'memory_type'",
        )
        .fetch_one(&self.pool)
        .await
        .unwrap_or(false);
        if needs_upgrade {
            let _ = sqlx::query("ALTER TABLE mem_memories DROP INDEX idx_user_active")
                .execute(&self.pool)
                .await;
            let _ = sqlx::query("ALTER TABLE mem_memories ADD INDEX idx_user_active (user_id, is_active, memory_type)")
                .execute(&self.pool).await;
        }

        // Migration: mem_branches may lack table_name column (old schema)
        let has_table_name: bool = sqlx::query_scalar(
            "SELECT COUNT(*) > 0 FROM information_schema.columns \
             WHERE table_schema = DATABASE() AND table_name = 'mem_branches' AND column_name = 'table_name'"
        ).fetch_one(&self.pool).await.unwrap_or(false);
        if !has_table_name {
            let _ = sqlx::query(
                "ALTER TABLE mem_branches ADD COLUMN table_name VARCHAR(100) NOT NULL DEFAULT ''",
            )
            .execute(&self.pool)
            .await;
        }

        // Migration: mem_branches old schema used branch_id/branch_db — recreate with new schema
        let has_id: bool = sqlx::query_scalar(
            "SELECT COUNT(*) > 0 FROM information_schema.columns \
             WHERE table_schema = DATABASE() AND table_name = 'mem_branches' AND column_name = 'id'"
        ).fetch_one(&self.pool).await.unwrap_or(false);
        if !has_id {
            let _ = sqlx::query("DROP TABLE IF EXISTS mem_branches")
                .execute(&self.pool)
                .await;
            sqlx::query(
                r#"CREATE TABLE IF NOT EXISTS mem_branches (
                    id          VARCHAR(64)  PRIMARY KEY,
                    user_id     VARCHAR(64)  NOT NULL,
                    name        VARCHAR(100) NOT NULL,
                    table_name  VARCHAR(100) NOT NULL,
                    status      VARCHAR(20)  NOT NULL DEFAULT 'active',
                    created_at  DATETIME(6)  NOT NULL,
                    INDEX idx_user_name (user_id, name)
                )"#,
            )
            .execute(&self.pool)
            .await
            .map_err(db_err)?;
        }

        // mem_retrieval_feedback — explicit relevance feedback for adaptive tuning
        sqlx::query(
            r#"CREATE TABLE IF NOT EXISTS mem_retrieval_feedback (
                id          VARCHAR(64)  PRIMARY KEY,
                user_id     VARCHAR(64)  NOT NULL,
                memory_id   VARCHAR(64)  NOT NULL,
                signal      VARCHAR(16)  NOT NULL,
                context     TEXT         DEFAULT NULL,
                created_at  DATETIME(6)  NOT NULL,
                INDEX idx_feedback_user (user_id, created_at),
                INDEX idx_feedback_memory (memory_id)
            )"#,
        )
        .execute(&self.pool)
        .await
        .map_err(db_err)?;

        // mem_user_retrieval_params — per-user adaptive scoring parameters
        sqlx::query(
            r#"CREATE TABLE IF NOT EXISTS mem_user_retrieval_params (
                user_id              VARCHAR(64)  PRIMARY KEY,
                feedback_weight      DOUBLE       NOT NULL DEFAULT 0.1,
                temporal_decay_hours DOUBLE       NOT NULL DEFAULT 168.0,
                confidence_weight    DOUBLE       NOT NULL DEFAULT 0.1,
                updated_at           DATETIME(6)  NOT NULL
            )"#,
        )
        .execute(&self.pool)
        .await
        .map_err(db_err)?;

        // mem_tool_usage — per-user tool access timestamps (batched from API layer)
        sqlx::query(
            r#"CREATE TABLE IF NOT EXISTS mem_tool_usage (
                user_id      VARCHAR(64)  NOT NULL,
                tool_name    VARCHAR(128) NOT NULL,
                last_used_at DATETIME(6)  NOT NULL,
                PRIMARY KEY (user_id, tool_name)
            )"#,
        )
        .execute(&self.pool)
        .await
        .map_err(db_err)?;

        // mem_api_call_log — per-user API call statistics for the Monitor dashboard.
        // Records every authenticated /v1/* request: method, path, HTTP status,
        // latency, and timestamp.  Insertions are batched (every 5 s) to avoid
        // per-request DB pressure.
        sqlx::query(
            r#"CREATE TABLE IF NOT EXISTS mem_api_call_log (
                id           BIGINT       NOT NULL AUTO_INCREMENT,
                user_id      VARCHAR(64)  NOT NULL,
                method       VARCHAR(10)  NOT NULL DEFAULT '',
                path         VARCHAR(256) NOT NULL,
                status_code  SMALLINT     NOT NULL DEFAULT 0,
                latency_ms   INT          NOT NULL DEFAULT 0,
                called_at    DATETIME(6)  NOT NULL DEFAULT NOW(6),
                PRIMARY KEY (id),
                INDEX idx_user_called (user_id, called_at)
            )"#,
        )
        .execute(&self.pool)
        .await
        .map_err(db_err)?;

        // Migration: add `method` for tables created before this column existed.
        // MatrixOne / some MySQL forks do not reliably support
        // `ADD COLUMN IF NOT EXISTS ... AFTER ...`; use information_schema + plain ALTER.
        let has_method_col: bool = sqlx::query_scalar(
            "SELECT COUNT(*) > 0 FROM information_schema.columns \
             WHERE table_schema = DATABASE() AND table_name = 'mem_api_call_log' \
             AND column_name = 'method'",
        )
        .fetch_one(&self.pool)
        .await
        .unwrap_or(false);
        if !has_method_col {
            // No `AFTER` — better compatibility with MatrixOne; INSERT lists columns explicitly.
            let _ = sqlx::query(
                "ALTER TABLE mem_api_call_log ADD COLUMN method VARCHAR(10) NOT NULL DEFAULT ''",
            )
            .execute(&self.pool)
            .await;
        }

        Ok(())
    }

    // ── Audit log ─────────────────────────────────────────────────────────────

    /// Create a safety snapshot before destructive operations. Best-effort.
    /// If creation fails (e.g. quota exhausted), tries to drop the 10 oldest
    /// `pre_` safety snapshots and retries once.
    /// Returns `(snapshot_name_or_none, warning_message_or_none)`.
    pub async fn create_safety_snapshot(
        &self,
        operation: &str,
    ) -> (Option<String>, Option<String>) {
        let name = format!(
            "mem_snap_pre_{}_{}",
            operation,
            &uuid::Uuid::new_v4().simple().to_string()[..8]
        );
        let sql = format!("CREATE SNAPSHOT {name} FOR ACCOUNT sys");

        // First attempt
        if sqlx::raw_sql(&sql).execute(&self.pool).await.is_ok() {
            return (Some(name), None);
        }

        // Failed — try to reclaim space by dropping oldest pre_ snapshots
        let dropped = self.cleanup_oldest_safety_snapshots(10).await;
        if dropped > 0 {
            // Retry
            if sqlx::raw_sql(&sql).execute(&self.pool).await.is_ok() {
                return (Some(name), Some(format!(
                    "⚠️ Snapshot quota was full. Auto-deleted {dropped} oldest safety snapshots to make room. \
                     Consider running memory_snapshot_delete(prefix=\"pre_\") to free more space."
                )));
            }
        }

        // Still failed
        (None, Some(
            "⚠️ Safety snapshot could not be created (snapshot quota exhausted). \
             Purge proceeded without rollback protection. \
             Run memory_snapshot_delete(prefix=\"pre_\") or memory_snapshot_delete(older_than=\"...\") to free quota."
            .to_string()
        ))
    }

    /// Drop the N oldest `mem_snap_pre_` snapshots. Returns count dropped.
    async fn cleanup_oldest_safety_snapshots(&self, n: usize) -> usize {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT sname FROM mo_catalog.mo_snapshots \
             WHERE prefix_eq(sname, 'mem_snap_pre_') ORDER BY ts ASC",
        )
        .fetch_all(&self.pool)
        .await
        .unwrap_or_default();

        let mut dropped = 0;
        for (name,) in rows.iter().take(n) {
            if sqlx::raw_sql(&format!("DROP SNAPSHOT {name}"))
                .execute(&self.pool)
                .await
                .is_ok()
            {
                dropped += 1;
            }
        }
        dropped
    }

    /// Write an audit record to mem_edit_log. Best-effort — never fails the caller.
    /// For batch operations, call once per memory_id.
    pub async fn log_edit(
        &self,
        user_id: &str,
        operation: &str,
        memory_id: Option<&str>,
        payload: Option<&str>,
        reason: &str,
        snapshot_before: Option<&str>,
    ) {
        let edit_id = uuid7_id();
        let _ = sqlx::query(
            "INSERT INTO mem_edit_log (edit_id, user_id, memory_id, operation, payload, reason, snapshot_before, created_by) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)"
        )
        .bind(&edit_id)
        .bind(user_id)
        .bind(memory_id)
        .bind(operation)
        .bind(payload)
        .bind(reason)
        .bind(snapshot_before)
        .bind(user_id)
        .execute(&self.pool)
        .await;
    }

    // ── Branch state ──────────────────────────────────────────────────────────

    /// Returns the active table name for a user: "mem_memories" or branch table name.
    pub async fn active_table(&self, user_id: &str) -> Result<String, MemoriaError> {
        let row = sqlx::query("SELECT active_branch FROM mem_user_state WHERE user_id = ?")
            .bind(user_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(db_err)?;

        let branch = row
            .and_then(|r| r.try_get::<String, _>("active_branch").ok())
            .unwrap_or_else(|| "main".to_string());

        if branch == "main" {
            return Ok("mem_memories".to_string());
        }

        let branch_row = sqlx::query(
            "SELECT table_name FROM mem_branches WHERE user_id = ? AND name = ? AND status = 'active'"
        )
        .bind(user_id).bind(&branch)
        .fetch_optional(&self.pool).await.map_err(db_err)?;

        match branch_row {
            Some(r) => Ok(r.try_get::<String, _>("table_name").map_err(db_err)?),
            None => {
                self.set_active_branch(user_id, "main").await?;
                Ok("mem_memories".to_string())
            }
        }
    }

    pub async fn set_active_branch(&self, user_id: &str, branch: &str) -> Result<(), MemoriaError> {
        let now = Utc::now().naive_utc();
        sqlx::query(
            r#"INSERT INTO mem_user_state (user_id, active_branch, updated_at)
               VALUES (?, ?, ?)
               ON DUPLICATE KEY UPDATE active_branch = ?, updated_at = ?"#,
        )
        .bind(user_id)
        .bind(branch)
        .bind(now)
        .bind(branch)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    pub async fn register_branch(
        &self,
        user_id: &str,
        name: &str,
        table_name: &str,
    ) -> Result<(), MemoriaError> {
        let now = Utc::now().naive_utc();
        let id = uuid::Uuid::new_v4().simple().to_string();
        sqlx::query(
            r#"INSERT INTO mem_branches (id, user_id, name, table_name, status, created_at)
               VALUES (?, ?, ?, ?, 'active', ?)"#,
        )
        .bind(id)
        .bind(user_id)
        .bind(name)
        .bind(table_name)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    pub async fn deregister_branch(&self, user_id: &str, name: &str) -> Result<(), MemoriaError> {
        sqlx::query("UPDATE mem_branches SET status = 'deleted' WHERE user_id = ? AND name = ?")
            .bind(user_id)
            .bind(name)
            .execute(&self.pool)
            .await
            .map_err(db_err)?;
        Ok(())
    }

    pub async fn list_branches(
        &self,
        user_id: &str,
    ) -> Result<Vec<(String, String)>, MemoriaError> {
        let rows = sqlx::query(
            "SELECT name, table_name FROM mem_branches WHERE user_id = ? AND status = 'active'",
        )
        .bind(user_id)
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;
        rows.iter()
            .map(|r| {
                Ok((
                    r.try_get::<String, _>("name").map_err(db_err)?,
                    r.try_get::<String, _>("table_name").map_err(db_err)?,
                ))
            })
            .collect()
    }

    pub async fn register_snapshot(
        &self,
        user_id: &str,
        name: &str,
        snapshot_name: &str,
    ) -> Result<(), MemoriaError> {
        let now = Utc::now().naive_utc();
        let id = uuid::Uuid::new_v4().simple().to_string();
        sqlx::query(
            r#"INSERT INTO mem_snapshots (id, user_id, name, snapshot_name, status, created_at)
               VALUES (?, ?, ?, ?, 'active', ?)"#,
        )
        .bind(id)
        .bind(user_id)
        .bind(name)
        .bind(snapshot_name)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    pub async fn get_snapshot_registration(
        &self,
        user_id: &str,
        name: &str,
    ) -> Result<Option<SnapshotRegistration>, MemoriaError> {
        let row = sqlx::query(
            "SELECT name, snapshot_name, created_at \
             FROM mem_snapshots \
             WHERE user_id = ? AND name = ? AND status = 'active' \
             ORDER BY created_at DESC LIMIT 1",
        )
        .bind(user_id)
        .bind(name)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?;

        row.map(|r| {
            Ok(SnapshotRegistration {
                name: r.try_get("name").map_err(db_err)?,
                snapshot_name: r.try_get("snapshot_name").map_err(db_err)?,
                created_at: r.try_get("created_at").map_err(db_err)?,
            })
        })
        .transpose()
    }

    pub async fn get_snapshot_registration_by_internal(
        &self,
        user_id: &str,
        snapshot_name: &str,
    ) -> Result<Option<SnapshotRegistration>, MemoriaError> {
        let row = sqlx::query(
            "SELECT name, snapshot_name, created_at \
             FROM mem_snapshots \
             WHERE user_id = ? AND snapshot_name = ? AND status = 'active' \
             ORDER BY created_at DESC LIMIT 1",
        )
        .bind(user_id)
        .bind(snapshot_name)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?;

        row.map(|r| {
            Ok(SnapshotRegistration {
                name: r.try_get("name").map_err(db_err)?,
                snapshot_name: r.try_get("snapshot_name").map_err(db_err)?,
                created_at: r.try_get("created_at").map_err(db_err)?,
            })
        })
        .transpose()
    }

    pub async fn list_snapshot_registrations(
        &self,
        user_id: &str,
    ) -> Result<Vec<SnapshotRegistration>, MemoriaError> {
        let rows = sqlx::query(
            "SELECT name, snapshot_name, created_at \
             FROM mem_snapshots \
             WHERE user_id = ? AND status = 'active' \
             ORDER BY created_at DESC",
        )
        .bind(user_id)
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;

        rows.iter()
            .map(|r| {
                Ok(SnapshotRegistration {
                    name: r.try_get("name").map_err(db_err)?,
                    snapshot_name: r.try_get("snapshot_name").map_err(db_err)?,
                    created_at: r.try_get("created_at").map_err(db_err)?,
                })
            })
            .collect()
    }

    pub async fn deregister_snapshot(&self, user_id: &str, name: &str) -> Result<(), MemoriaError> {
        sqlx::query(
            "UPDATE mem_snapshots SET status = 'deleted' WHERE user_id = ? AND name = ? AND status = 'active'",
        )
        .bind(user_id)
        .bind(name)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    pub async fn deregister_snapshot_by_internal(
        &self,
        user_id: &str,
        snapshot_name: &str,
    ) -> Result<(), MemoriaError> {
        sqlx::query(
            "UPDATE mem_snapshots SET status = 'deleted' \
             WHERE user_id = ? AND snapshot_name = ? AND status = 'active'",
        )
        .bind(user_id)
        .bind(snapshot_name)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    // ── Governance ────────────────────────────────────────────────────────────

    /// Check cooldown. Returns Some(remaining_seconds) if still in cooldown, None if can run.
    pub async fn check_cooldown(
        &self,
        user_id: &str,
        operation: &str,
        cooldown_secs: i64,
    ) -> Result<Option<i64>, MemoriaError> {
        let row = sqlx::query(
            "SELECT TIMESTAMPDIFF(SECOND, last_run_at, NOW()) as elapsed \
             FROM mem_governance_cooldown WHERE user_id = ? AND operation = ?",
        )
        .bind(user_id)
        .bind(operation)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?;
        match row {
            None => Ok(None),
            Some(r) => {
                let elapsed: i64 = r.try_get("elapsed").unwrap_or(cooldown_secs + 1);
                if elapsed >= cooldown_secs {
                    Ok(None)
                } else {
                    Ok(Some(cooldown_secs - elapsed))
                }
            }
        }
    }

    pub async fn set_cooldown(&self, user_id: &str, operation: &str) -> Result<(), MemoriaError> {
        let now = Utc::now().naive_utc();
        sqlx::query(
            "INSERT INTO mem_governance_cooldown (user_id, operation, last_run_at) \
             VALUES (?, ?, ?) ON DUPLICATE KEY UPDATE last_run_at = ?",
        )
        .bind(user_id)
        .bind(operation)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    pub async fn check_governance_runtime_breaker(
        &self,
        strategy_key: &str,
        task: &str,
    ) -> Result<Option<i64>, MemoriaError> {
        let row = sqlx::query(
            "SELECT TIMESTAMPDIFF(SECOND, NOW(), circuit_open_until) AS remaining \
             FROM mem_governance_runtime_state \
             WHERE strategy_key = ? AND task = ? \
               AND circuit_open_until IS NOT NULL AND circuit_open_until > NOW()",
        )
        .bind(strategy_key)
        .bind(task)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?;

        Ok(row.and_then(|r| r.try_get::<i64, _>("remaining").ok()))
    }

    pub async fn record_governance_runtime_failure(
        &self,
        strategy_key: &str,
        task: &str,
        threshold: usize,
        cooldown_secs: i64,
    ) -> Result<Option<i64>, MemoriaError> {
        let open_on_insert = threshold <= 1;
        let initial_failures = if open_on_insert { 0 } else { 1 };
        sqlx::query(
            "INSERT INTO mem_governance_runtime_state \
                 (strategy_key, task, failure_count, circuit_open_until, updated_at) \
             VALUES (?, ?, ?, CASE WHEN ? THEN DATE_ADD(NOW(), INTERVAL ? SECOND) ELSE NULL END, NOW()) \
             ON DUPLICATE KEY UPDATE \
                 failure_count = CASE \
                     WHEN circuit_open_until IS NOT NULL AND circuit_open_until > NOW() THEN failure_count \
                     WHEN failure_count + 1 >= ? THEN 0 \
                     ELSE failure_count + 1 \
                 END, \
                 circuit_open_until = CASE \
                     WHEN circuit_open_until IS NOT NULL AND circuit_open_until > NOW() THEN circuit_open_until \
                     WHEN failure_count + 1 >= ? THEN DATE_ADD(NOW(), INTERVAL ? SECOND) \
                     ELSE NULL \
                 END, \
                 updated_at = NOW()"
        )
        .bind(strategy_key)
        .bind(task)
        .bind(initial_failures)
        .bind(open_on_insert)
        .bind(cooldown_secs)
        .bind(threshold as i64)
        .bind(threshold as i64)
        .bind(cooldown_secs)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;

        self.check_governance_runtime_breaker(strategy_key, task)
            .await
    }

    pub async fn clear_governance_runtime_breaker(
        &self,
        strategy_key: &str,
        task: &str,
    ) -> Result<(), MemoriaError> {
        sqlx::query(
            "INSERT INTO mem_governance_runtime_state \
                 (strategy_key, task, failure_count, circuit_open_until, updated_at) \
             VALUES (?, ?, 0, NULL, NOW()) \
             ON DUPLICATE KEY UPDATE failure_count = 0, circuit_open_until = NULL, updated_at = NOW()"
        )
        .bind(strategy_key)
        .bind(task)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    /// Quarantine memories whose effective confidence has decayed below threshold.
    /// effective_confidence = initial_confidence * EXP(-age_days / half_life)
    pub async fn quarantine_low_confidence(&self, user_id: &str) -> Result<i64, MemoriaError> {
        const THRESHOLD: f64 = 0.2;
        const BATCH: i64 = 500;
        let tiers: &[(&str, f64)] = &[("T1", 365.0), ("T2", 180.0), ("T3", 60.0), ("T4", 30.0)];
        let mut total = 0i64;
        for (tier, hl) in tiers {
            loop {
                let res = sqlx::query(&format!(
                    "UPDATE mem_memories SET is_active = 0, updated_at = NOW() \
                     WHERE user_id = ? AND is_active = 1 AND trust_tier = ? \
                       AND (initial_confidence * EXP(-TIMESTAMPDIFF(DAY, observed_at, NOW()) / {hl})) < {THRESHOLD} \
                     LIMIT {BATCH}"
                ))
                .bind(user_id).bind(tier)
                .execute(&self.pool).await.map_err(db_err)?;
                let n = res.rows_affected() as i64;
                total += n;
                if n < BATCH {
                    break;
                }
            }
        }
        Ok(total)
    }

    /// Delete inactive memories with very low initial_confidence (already superseded/stale).
    pub async fn cleanup_stale(&self, user_id: &str) -> Result<i64, MemoriaError> {
        const BATCH: u64 = 500;
        let mut total = 0i64;
        loop {
            let res = sqlx::query(
                "DELETE FROM mem_memories WHERE user_id = ? AND is_active = 0 AND initial_confidence < 0.1 LIMIT 500"
            )
            .bind(user_id).execute(&self.pool).await.map_err(db_err)?;
            let n = res.rows_affected();
            total += n as i64;
            if n < BATCH {
                break;
            }
        }
        Ok(total)
    }

    /// Delete expired tool_result memories (TTL = 72h by default).
    pub async fn cleanup_tool_results(&self, ttl_hours: i64) -> Result<i64, MemoriaError> {
        let mut total = 0i64;
        loop {
            let res = sqlx::query(
                "DELETE FROM mem_memories \
                 WHERE memory_type = 'tool_result' \
                   AND TIMESTAMPDIFF(HOUR, observed_at, NOW()) > ? \
                 LIMIT 5000",
            )
            .bind(ttl_hours)
            .execute(&self.pool)
            .await
            .map_err(db_err)?;
            let n = res.rows_affected() as i64;
            total += n;
            if n < 5000 {
                break;
            }
        }
        Ok(total)
    }

    /// Soft-delete working memories inactive for more than `stale_hours`.
    /// Returns per-user counts for audit logging.
    pub async fn archive_stale_working(
        &self,
        stale_hours: i64,
    ) -> Result<Vec<(String, i64)>, MemoriaError> {
        const BATCH: i64 = 500;

        // Collect affected users first (cheap DISTINCT query)
        let users: Vec<(String,)> = sqlx::query_as(
            "SELECT DISTINCT user_id FROM mem_memories \
             WHERE memory_type = 'working' AND is_active = 1 \
               AND TIMESTAMPDIFF(HOUR, observed_at, NOW()) > ?",
        )
        .bind(stale_hours)
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;

        if users.is_empty() {
            return Ok(vec![]);
        }

        // Batched UPDATE per user to avoid global lock
        let mut result = Vec::with_capacity(users.len());
        for (uid,) in users {
            let mut total = 0i64;
            loop {
                let res = sqlx::query(
                    "UPDATE mem_memories SET is_active = 0, updated_at = NOW() \
                     WHERE user_id = ? AND memory_type = 'working' AND is_active = 1 \
                       AND TIMESTAMPDIFF(HOUR, observed_at, NOW()) > ? \
                     LIMIT 500",
                )
                .bind(&uid)
                .bind(stale_hours)
                .execute(&self.pool)
                .await
                .map_err(db_err)?;
                let n = res.rows_affected() as i64;
                total += n;
                if n < BATCH {
                    break;
                }
            }
            if total > 0 {
                result.push((uid, total));
            }
        }
        Ok(result)
    }

    /// Deactivate near-duplicate memories (same user, same type, cosine sim > threshold).
    /// Uses L2² ≈ 2(1 - cos_sim) for normalized embeddings.
    /// Returns count of deactivated memories.
    pub async fn compress_redundant(
        &self,
        user_id: &str,
        similarity_threshold: f64,
        window_days: i64,
        max_pairs: usize,
    ) -> Result<i64, MemoriaError> {
        let rows: Vec<(String, String, chrono::NaiveDateTime, String)> = sqlx::query_as(
            "SELECT memory_id, memory_type, observed_at, embedding \
             FROM mem_memories \
             WHERE user_id = ? AND is_active = 1 AND embedding IS NOT NULL \
               AND TIMESTAMPDIFF(DAY, observed_at, NOW()) <= ? \
             ORDER BY memory_type, observed_at DESC",
        )
        .bind(user_id)
        .bind(window_days)
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;

        if rows.len() < 2 {
            return Ok(0);
        }

        let l2_sq_threshold = 2.0 * (1.0 - similarity_threshold);

        // Group by memory_type with flat embedding storage for cache locality
        struct Entry {
            id: String,
            ts: chrono::NaiveDateTime,
            emb_offset: usize,
            dim: usize,
        }
        let mut by_type: std::collections::HashMap<String, Vec<Entry>> = Default::default();
        let mut flat_embs: Vec<f32> = Vec::new();

        for (mid, mtype, ts, emb_str) in &rows {
            if let Ok(emb) = mo_to_vec(emb_str) {
                let offset = flat_embs.len();
                let dim = emb.len();
                flat_embs.extend_from_slice(&emb);
                by_type.entry(mtype.clone()).or_default().push(Entry {
                    id: mid.clone(),
                    ts: *ts,
                    emb_offset: offset,
                    dim,
                });
            }
        }

        let mut to_deactivate: Vec<(String, String)> = vec![];
        let mut deactivated_ids: std::collections::HashSet<String> = Default::default();
        let mut pairs_checked = 0;

        'outer: for group in by_type.values() {
            if group.len() < 2 {
                continue;
            }
            for i in 0..group.len() {
                if deactivated_ids.contains(&group[i].id) {
                    continue;
                }
                let emb_i = &flat_embs[group[i].emb_offset..group[i].emb_offset + group[i].dim];
                // Vectorized: compute L2² from i to all j > i
                for j in (i + 1)..group.len() {
                    if pairs_checked >= max_pairs {
                        break 'outer;
                    }
                    if deactivated_ids.contains(&group[j].id) {
                        continue;
                    }
                    pairs_checked += 1;
                    let emb_j = &flat_embs[group[j].emb_offset..group[j].emb_offset + group[j].dim];
                    let dist_sq: f32 = emb_i
                        .iter()
                        .zip(emb_j)
                        .map(|(a, b)| {
                            let d = a - b;
                            d * d
                        })
                        .sum();
                    if (dist_sq as f64) < l2_sq_threshold {
                        let (older, newer) = if group[i].ts >= group[j].ts {
                            (group[j].id.clone(), group[i].id.clone())
                        } else {
                            (group[i].id.clone(), group[j].id.clone())
                        };
                        deactivated_ids.insert(older.clone());
                        to_deactivate.push((older, newer));
                    }
                }
            }
        }

        if to_deactivate.is_empty() {
            return Ok(0);
        }

        // Batch UPDATE with CASE for superseded_by (preserves audit trail for all rows)
        // Split into chunks of 100 to avoid SQL statement size limits
        for chunk in to_deactivate.chunks(100) {
            let placeholders = chunk.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            let case_clauses = chunk
                .iter()
                .map(|_| "WHEN ? THEN ?")
                .collect::<Vec<_>>()
                .join(" ");
            let sql = format!(
                "UPDATE mem_memories SET is_active = 0, superseded_by = CASE memory_id {case_clauses} END, updated_at = NOW() WHERE memory_id IN ({placeholders})"
            );
            let mut q = sqlx::query(&sql);
            // Bind CASE values first
            for (older, newer) in chunk {
                q = q.bind(older).bind(newer);
            }
            // Then bind IN clause values
            for (older, _) in chunk {
                q = q.bind(older);
            }
            q.execute(&self.pool).await.map_err(db_err)?;
        }

        Ok(to_deactivate.len() as i64)
    }

    /// Rebuild IVF vector index for a table. lists = max(1, rows/50), capped at 1024.
    pub async fn rebuild_vector_index(&self, table: &str) -> Result<i64, MemoriaError> {
        Self::validate_table_name(table)?;
        let row: (i64,) = sqlx::query_as(&format!(
            "SELECT COUNT(*) FROM {table} WHERE embedding IS NOT NULL"
        ))
        .fetch_one(&self.pool)
        .await
        .map_err(db_err)?;
        let total_rows = row.0;
        if total_rows == 0 {
            return Ok(0);
        }
        let idx_name = format!("{table}_embedding_ivf");
        // IVF hurts recall on small datasets; only build when rows >= 500
        if total_rows < 500 {
            let _ = sqlx::raw_sql(&format!("DROP INDEX {idx_name} ON {table}"))
                .execute(&self.pool)
                .await;
            return Ok(total_rows);
        }
        let lists = (total_rows / 50).clamp(1, 1024);
        let _ = sqlx::raw_sql(&format!("DROP INDEX {idx_name} ON {table}"))
            .execute(&self.pool)
            .await;
        sqlx::raw_sql(&format!(
            "CREATE INDEX {idx_name} USING ivfflat ON {table}(embedding) LISTS {lists} op_type 'vector_l2_ops'"
        ))
        .execute(&self.pool).await.map_err(db_err)?;
        Ok(total_rows)
    }

    /// Deactivate orphaned incremental session summaries (session never closed, >24h old).
    pub async fn cleanup_orphaned_incrementals(
        &self,
        user_id: &str,
        older_than_hours: i64,
    ) -> Result<i64, MemoriaError> {
        let ids: Vec<(String,)> = sqlx::query_as(
            "SELECT inc.memory_id FROM mem_memories AS inc \
             WHERE inc.user_id = ? \
               AND inc.is_active = 1 \
               AND LOCATE('[session_summary:incremental]', inc.content) = 1 \
               AND inc.session_id IS NOT NULL \
               AND TIMESTAMPDIFF(HOUR, inc.observed_at, NOW()) > ? \
               AND NOT EXISTS ( \
                   SELECT 1 FROM mem_memories AS full_s \
                   WHERE full_s.user_id = ? \
                     AND full_s.is_active = 1 \
                     AND full_s.session_id IS NULL \
                     AND LOCATE('[session_summary]', full_s.content) = 1 \
                     AND full_s.observed_at > inc.observed_at \
               )",
        )
        .bind(user_id)
        .bind(older_than_hours)
        .bind(user_id)
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;

        if ids.is_empty() {
            return Ok(0);
        }
        for chunk in ids.chunks(500) {
            let placeholders = chunk.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            let sql = format!(
                "UPDATE mem_memories SET is_active = 0, updated_at = NOW() WHERE memory_id IN ({placeholders})"
            );
            let mut q = sqlx::query(&sql);
            for (id,) in chunk {
                q = q.bind(id);
            }
            q.execute(&self.pool).await.map_err(db_err)?;
        }
        Ok(ids.len() as i64)
    }

    /// Drop old milestone snapshots, keep last N (weekly).
    pub async fn cleanup_snapshots(&self, keep_last_n: usize) -> Result<i64, MemoriaError> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT sname FROM mo_catalog.mo_snapshots \
             WHERE prefix_eq(sname, 'mem_milestone_') ORDER BY ts DESC",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;

        if rows.len() <= keep_last_n {
            return Ok(0);
        }
        let mut dropped = 0i64;
        for (name,) in &rows[keep_last_n..] {
            let _ = sqlx::raw_sql(&format!("DROP SNAPSHOT {name}"))
                .execute(&self.pool)
                .await;
            dropped += 1;
        }
        Ok(dropped)
    }

    /// Clean up sandbox branches that were not properly dropped (weekly).
    pub async fn cleanup_orphan_branches(&self) -> Result<i64, MemoriaError> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT table_name FROM information_schema.tables \
             WHERE table_name LIKE 'memories_sandbox_%'",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;

        let (db_name,): (String,) = sqlx::query_as("SELECT DATABASE()")
            .fetch_one(&self.pool)
            .await
            .map_err(db_err)?;
        let mut cleaned = 0i64;
        for (table_name,) in rows {
            let _ = sqlx::raw_sql(&format!("DATA BRANCH DELETE TABLE {db_name}.{table_name}"))
                .execute(&self.pool)
                .await;
            cleaned += 1;
        }
        Ok(cleaned)
    }

    /// Bump access_count for retrieved memories (fire-and-forget style).
    pub async fn bump_access_counts(&self, memory_ids: &[String]) -> Result<(), MemoriaError> {
        if memory_ids.is_empty() {
            return Ok(());
        }
        for chunk in memory_ids.chunks(100) {
            let placeholders: Vec<&str> = chunk.iter().map(|_| "(?, 1, NOW())").collect();
            let sql = format!(
                "INSERT INTO mem_memories_stats (memory_id, access_count, last_accessed_at) VALUES {} \
                 ON DUPLICATE KEY UPDATE access_count = access_count + 1, last_accessed_at = NOW()",
                placeholders.join(", ")
            );
            let mut q = sqlx::query(&sql);
            for id in chunk {
                q = q.bind(id);
            }
            q.execute(&self.pool).await.map_err(db_err)?;
        }
        Ok(())
    }

    /// Batch bump with pre-aggregated counts (used by AccessCounter flush).
    pub async fn bump_access_counts_batch(
        &self,
        batch: &[(String, u64)],
    ) -> Result<(), MemoriaError> {
        if batch.is_empty() {
            return Ok(());
        }
        for chunk in batch.chunks(100) {
            let placeholders: Vec<String> =
                chunk.iter().map(|_| "(?, ?, NOW())".to_string()).collect();
            let sql = format!(
                "INSERT INTO mem_memories_stats (memory_id, access_count, last_accessed_at) VALUES {} \
                 ON DUPLICATE KEY UPDATE access_count = access_count + VALUES(access_count), last_accessed_at = NOW()",
                placeholders.join(", ")
            );
            let mut q = sqlx::query(&sql);
            for (id, count) in chunk {
                q = q.bind(id).bind(*count as i64);
            }
            q.execute(&self.pool).await.map_err(db_err)?;
        }
        Ok(())
    }

    /// Reset access_count to 0 for all memories of a user.
    pub async fn reset_access_counts(&self, user_id: &str) -> Result<i64, MemoriaError> {
        let result = sqlx::query(
            "UPDATE mem_memories_stats s \
             JOIN mem_memories m ON s.memory_id = m.memory_id \
             SET s.access_count = 0 \
             WHERE m.user_id = ?",
        )
        .bind(user_id)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(result.rows_affected() as i64)
    }

    /// Clean up orphaned stats records (stats without corresponding memory).
    pub async fn cleanup_orphan_stats(&self) -> Result<i64, MemoriaError> {
        let result = sqlx::query(
            "DELETE s FROM mem_memories_stats s \
             LEFT JOIN mem_memories m ON s.memory_id = m.memory_id \
             WHERE m.memory_id IS NULL",
        )
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(result.rows_affected() as i64)
    }

    /// Delete old audit-log rows older than `retain_days` days, in batches to avoid lock pressure.
    pub async fn cleanup_edit_log(&self, retain_days: i64) -> Result<i64, MemoriaError> {
        const BATCH: u64 = 1000;
        let mut total = 0i64;
        loop {
            let res = sqlx::query(
                "DELETE FROM mem_edit_log WHERE created_at < DATE_SUB(NOW(), INTERVAL ? DAY) LIMIT 1000"
            )
            .bind(retain_days)
            .execute(&self.pool).await.map_err(db_err)?;
            let n = res.rows_affected();
            total += n as i64;
            if n < BATCH {
                break;
            }
        }
        Ok(total)
    }

    /// Delete old feedback rows older than `retain_days` days, in batches.
    pub async fn cleanup_feedback(&self, retain_days: i64) -> Result<i64, MemoriaError> {
        const BATCH: u64 = 1000;
        let mut total = 0i64;
        loop {
            let res = sqlx::query(
                "DELETE FROM mem_retrieval_feedback WHERE created_at < DATE_SUB(NOW(), INTERVAL ? DAY) LIMIT 1000"
            )
            .bind(retain_days)
            .execute(&self.pool).await.map_err(db_err)?;
            let n = res.rows_affected();
            total += n as i64;
            if n < BATCH {
                break;
            }
        }
        Ok(total)
    }

    /// Validate table name to prevent SQL injection
    fn validate_table_name(table: &str) -> Result<(), MemoriaError> {
        // 只允许字母、数字、下划线
        if !table.chars().all(|c| c.is_alphanumeric() || c == '_') {
            return Err(MemoriaError::Validation(format!(
                "Invalid table name: {}",
                table
            )));
        }
        // 白名单验证（允许 mem_ 和 test_ 前缀）
        if !table.starts_with("mem_") && !table.starts_with("test_") {
            return Err(MemoriaError::Validation(format!(
                "Table not allowed for vector index operations: {}",
                table
            )));
        }
        Ok(())
    }

    /// Check if vector index needs rebuild and is not in cooldown.
    /// Returns (should_rebuild, current_row_count, cooldown_remaining_secs)
    pub async fn should_rebuild_vector_index(
        &self,
        table: &str,
    ) -> Result<(bool, i64, Option<i64>), MemoriaError> {
        Self::validate_table_name(table)?;
        let key = format!("vector_index_rebuild:{table}");

        // 1. 检查冷却
        let cooldown_check: Option<(chrono::NaiveDateTime,)> = sqlx::query_as(
            "SELECT circuit_open_until FROM mem_governance_runtime_state \
             WHERE strategy_key = ? AND task = 'rebuild'",
        )
        .bind(&key)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?;

        if let Some((until,)) = cooldown_check {
            let now = chrono::Utc::now().naive_utc();
            if until > now {
                let remaining = (until - now).num_seconds();
                return Ok((false, 0, Some(remaining)));
            }
        }

        // 2. 查当前行数（表可能不存在）
        let current_rows: i64 = sqlx::query_scalar(&format!(
            "SELECT COUNT(*) FROM {table} WHERE embedding IS NOT NULL"
        ))
        .fetch_one(&self.pool)
        .await
        .unwrap_or_default(); // 表不存在或查询失败，返回0

        // 3. 查上次重建时的行数
        let last_rows: Option<(i32,)> = sqlx::query_as(
            "SELECT failure_count FROM mem_governance_runtime_state \
             WHERE strategy_key = ? AND task = 'rebuild'",
        )
        .bind(&key)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?;

        let last_rows = last_rows.map(|(c,)| c as i64).unwrap_or(0);

        // 4. 判断是否需要重建
        let should_rebuild = if current_rows < 500 {
            false // 小数据集不需要 IVF
        } else if last_rows == 0 {
            true // 首次重建
        } else {
            let growth_ratio = (current_rows - last_rows) as f64 / last_rows as f64;
            growth_ratio > 0.2 // 增长超过 20%
        };

        Ok((should_rebuild, current_rows, None))
    }

    /// Record vector index rebuild and set adaptive cooldown.
    pub async fn record_vector_index_rebuild(
        &self,
        table: &str,
        row_count: i64,
        cooldown_secs: i64,
    ) -> Result<(), MemoriaError> {
        Self::validate_table_name(table)?;
        let key = format!("vector_index_rebuild:{table}");

        let cooldown_until =
            chrono::Utc::now().naive_utc() + chrono::Duration::seconds(cooldown_secs);

        sqlx::query(
            "INSERT INTO mem_governance_runtime_state \
             (strategy_key, task, failure_count, circuit_open_until, updated_at) \
             VALUES (?, 'rebuild', ?, ?, NOW()) \
             ON DUPLICATE KEY UPDATE \
             failure_count = VALUES(failure_count), \
             circuit_open_until = VALUES(circuit_open_until), \
             updated_at = NOW()",
        )
        .bind(&key)
        .bind(row_count as i32) // 复用 failure_count 字段存行数
        .bind(cooldown_until)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;

        Ok(())
    }

    /// Record vector index rebuild failure with exponential backoff.
    /// Returns the cooldown seconds applied.
    pub async fn record_vector_index_rebuild_failure(
        &self,
        table: &str,
    ) -> Result<i64, MemoriaError> {
        Self::validate_table_name(table)?;
        let key = format!("vector_index_rebuild:{table}");

        // 查询当前失败次数（存储在 failure_count 的负数）
        let current_failures: Option<(i32,)> = sqlx::query_as(
            "SELECT failure_count FROM mem_governance_runtime_state \
             WHERE strategy_key = ? AND task = 'rebuild' AND failure_count < 0",
        )
        .bind(&key)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?;

        let failure_count = current_failures.map(|(c,)| -c).unwrap_or(0) + 1;

        // 指数退避：5分钟 → 15分钟 → 1小时
        let cooldown_secs = match failure_count {
            1 => 300,  // 5分钟
            2 => 900,  // 15分钟
            _ => 3600, // 1小时
        };

        let cooldown_until =
            chrono::Utc::now().naive_utc() + chrono::Duration::seconds(cooldown_secs);

        sqlx::query(
            "INSERT INTO mem_governance_runtime_state \
             (strategy_key, task, failure_count, circuit_open_until, updated_at) \
             VALUES (?, 'rebuild', ?, ?, NOW()) \
             ON DUPLICATE KEY UPDATE \
             failure_count = VALUES(failure_count), \
             circuit_open_until = VALUES(circuit_open_until), \
             updated_at = NOW()",
        )
        .bind(&key)
        .bind(-(failure_count as i32)) // 负数表示失败次数
        .bind(cooldown_until)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;

        Ok(cooldown_secs)
    }

    /// Try to acquire a distributed lock (returns true if acquired).
    pub async fn try_acquire_lock(&self, key: &str, ttl_secs: i64) -> Result<bool, MemoriaError> {
        let expires_at = chrono::Utc::now().naive_utc() + chrono::Duration::seconds(ttl_secs);

        // 方案1：尝试更新过期的锁
        let update_result = sqlx::query(
            "UPDATE mem_distributed_locks \
             SET holder_id = ?, acquired_at = NOW(), expires_at = ? \
             WHERE lock_key = ? AND expires_at < NOW()",
        )
        .bind(&self.instance_id)
        .bind(expires_at)
        .bind(key)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;

        if update_result.rows_affected() > 0 {
            return Ok(true); // 成功更新过期锁
        }

        // 方案2：尝试插入新锁
        let insert_result = sqlx::query(
            "INSERT INTO mem_distributed_locks (lock_key, holder_id, acquired_at, expires_at) \
             VALUES (?, ?, NOW(), ?)",
        )
        .bind(key)
        .bind(&self.instance_id)
        .bind(expires_at)
        .execute(&self.pool)
        .await;

        match insert_result {
            Ok(_) => Ok(true), // 成功插入新锁
            Err(e) => {
                // 检查是否是主键冲突（锁已存在且未过期）
                let err_str = e.to_string();
                if err_str.contains("Duplicate") || err_str.contains("1062") {
                    // MatrixOne SI: the row may have been deleted by another
                    // connection but our snapshot still sees the old key.
                    // A fresh SELECT forces a snapshot refresh.
                    let exists: (i64,) = sqlx::query_as(
                        "SELECT COUNT(*) FROM mem_distributed_locks \
                         WHERE lock_key = ? AND expires_at >= NOW()",
                    )
                    .bind(key)
                    .fetch_one(&self.pool)
                    .await
                    .map_err(db_err)?;
                    if exists.0 > 0 {
                        return Ok(false); // lock genuinely held
                    }
                    // Row was deleted — retry INSERT with refreshed snapshot
                    let retry = sqlx::query(
                        "INSERT INTO mem_distributed_locks (lock_key, holder_id, acquired_at, expires_at) \
                         VALUES (?, ?, NOW(), ?)",
                    )
                    .bind(key)
                    .bind(&self.instance_id)
                    .bind(expires_at)
                    .execute(&self.pool)
                    .await;
                    match retry {
                        Ok(_) => Ok(true),
                        Err(e2) => {
                            let s = e2.to_string();
                            if s.contains("Duplicate") || s.contains("1062") {
                                Ok(false)
                            } else {
                                Err(db_err(e2))
                            }
                        }
                    }
                } else {
                    Err(db_err(e))
                }
            }
        }
    }

    /// Release a distributed lock.
    pub async fn release_lock(&self, key: &str) -> Result<(), MemoriaError> {
        sqlx::query("DELETE FROM mem_distributed_locks WHERE lock_key = ? AND holder_id = ?")
            .bind(key)
            .bind(&self.instance_id)
            .execute(&self.pool)
            .await
            .map_err(db_err)?;
        Ok(())
    }

    // ── Retrieval feedback ────────────────────────────────────────────────────

    /// Record explicit relevance feedback for a memory.
    /// signal: "useful" | "irrelevant" | "outdated" | "wrong"
    pub async fn record_feedback(
        &self,
        user_id: &str,
        memory_id: &str,
        signal: &str,
        context: Option<&str>,
    ) -> Result<String, MemoriaError> {
        // Validate signal
        if !["useful", "irrelevant", "outdated", "wrong"].contains(&signal) {
            return Err(MemoriaError::Validation(format!(
                "Invalid signal '{}'. Must be one of: useful, irrelevant, outdated, wrong",
                signal
            )));
        }
        // Verify memory exists and belongs to user
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM mem_memories WHERE memory_id = ? AND user_id = ?",
        )
        .bind(memory_id)
        .bind(user_id)
        .fetch_one(&self.pool)
        .await
        .map_err(db_err)?;
        if count == 0 {
            return Err(MemoriaError::NotFound(format!(
                "Memory {} not found or not owned by user",
                memory_id
            )));
        }

        let id = uuid::Uuid::new_v4().simple().to_string();
        sqlx::query(
            "INSERT INTO mem_retrieval_feedback (id, user_id, memory_id, signal, context, created_at) \
             VALUES (?, ?, ?, ?, ?, NOW())",
        )
        .bind(&id)
        .bind(user_id)
        .bind(memory_id)
        .bind(signal)
        .bind(context)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;

        // Update denormalized feedback counters in mem_memories_stats
        let col = match signal {
            "useful" => "feedback_useful",
            "irrelevant" => "feedback_irrelevant",
            "outdated" => "feedback_outdated",
            "wrong" => "feedback_wrong",
            _ => unreachable!(),
        };
        let sql = format!(
            "INSERT INTO mem_memories_stats (memory_id, {col}, last_feedback_at) VALUES (?, 1, NOW()) \
             ON DUPLICATE KEY UPDATE {col} = {col} + 1, last_feedback_at = NOW()"
        );
        sqlx::query(&sql)
            .bind(memory_id)
            .execute(&self.pool)
            .await
            .map_err(db_err)?;

        Ok(id)
    }

    /// Get feedback statistics for a user (for adaptive tuning analysis).
    pub async fn get_feedback_stats(&self, user_id: &str) -> Result<FeedbackStats, MemoriaError> {
        let row: (i64, i64, i64, i64, i64) = sqlx::query_as(
            "SELECT \
               COUNT(*) as total, \
               COALESCE(SUM(CASE WHEN signal = 'useful' THEN 1 ELSE 0 END), 0) as useful, \
               COALESCE(SUM(CASE WHEN signal = 'irrelevant' THEN 1 ELSE 0 END), 0) as irrelevant, \
               COALESCE(SUM(CASE WHEN signal = 'outdated' THEN 1 ELSE 0 END), 0) as outdated, \
               COALESCE(SUM(CASE WHEN signal = 'wrong' THEN 1 ELSE 0 END), 0) as wrong \
             FROM mem_retrieval_feedback WHERE user_id = ?",
        )
        .bind(user_id)
        .fetch_one(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(FeedbackStats {
            total: row.0,
            useful: row.1,
            irrelevant: row.2,
            outdated: row.3,
            wrong: row.4,
        })
    }

    /// Get feedback breakdown by trust tier (for adaptive tuning).
    pub async fn get_feedback_by_tier(
        &self,
        user_id: &str,
    ) -> Result<Vec<TierFeedback>, MemoriaError> {
        let rows: Vec<(String, String, i64)> = sqlx::query_as(
            "SELECT m.trust_tier, f.signal, COUNT(*) as cnt \
             FROM mem_retrieval_feedback f \
             JOIN mem_memories m ON f.memory_id = m.memory_id \
             WHERE f.user_id = ? \
             GROUP BY m.trust_tier, f.signal \
             ORDER BY m.trust_tier, f.signal",
        )
        .bind(user_id)
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(rows
            .into_iter()
            .map(|(tier, signal, count)| TierFeedback {
                tier,
                signal,
                count,
            })
            .collect())
    }

    /// Get feedback counts for a single memory (from denormalized stats, no JOIN).
    pub async fn get_memory_feedback(
        &self,
        memory_id: &str,
    ) -> Result<MemoryFeedback, MemoriaError> {
        let row: Option<(i32, i32, i32, i32)> = sqlx::query_as(
            "SELECT feedback_useful, feedback_irrelevant, feedback_outdated, feedback_wrong \
             FROM mem_memories_stats WHERE memory_id = ?",
        )
        .bind(memory_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?;

        Ok(row
            .map(|(useful, irrelevant, outdated, wrong)| MemoryFeedback {
                useful,
                irrelevant,
                outdated,
                wrong,
            })
            .unwrap_or_default())
    }

    /// Get feedback counts for multiple memories (batch, for retrieval scoring).
    pub async fn get_feedback_batch(
        &self,
        memory_ids: &[String],
    ) -> Result<std::collections::HashMap<String, MemoryFeedback>, MemoriaError> {
        let mut map = std::collections::HashMap::new();
        if memory_ids.is_empty() {
            return Ok(map);
        }
        for chunk in memory_ids.chunks(500) {
            let placeholders: Vec<&str> = chunk.iter().map(|_| "?").collect();
            let sql = format!(
                "SELECT memory_id, feedback_useful, feedback_irrelevant, feedback_outdated, feedback_wrong \
                 FROM mem_memories_stats WHERE memory_id IN ({})",
                placeholders.join(", ")
            );
            let mut q = sqlx::query(&sql);
            for id in chunk {
                q = q.bind(id);
            }
            let rows = q.fetch_all(&self.pool).await.map_err(db_err)?;
            for row in &rows {
                let id: String = row.try_get("memory_id").map_err(db_err)?;
                let useful: i32 = row.try_get("feedback_useful").unwrap_or(0);
                let irrelevant: i32 = row.try_get("feedback_irrelevant").unwrap_or(0);
                let outdated: i32 = row.try_get("feedback_outdated").unwrap_or(0);
                let wrong: i32 = row.try_get("feedback_wrong").unwrap_or(0);
                map.insert(
                    id,
                    MemoryFeedback {
                        useful,
                        irrelevant,
                        outdated,
                        wrong,
                    },
                );
            }
        }
        Ok(map)
    }

    // ── Per-User Retrieval Parameters ─────────────────────────────────────────

    /// Get user's retrieval parameters, or default if not set.
    pub async fn get_user_retrieval_params(
        &self,
        user_id: &str,
    ) -> Result<UserRetrievalParams, MemoriaError> {
        let row = sqlx::query(
            "SELECT feedback_weight, temporal_decay_hours, confidence_weight \
             FROM mem_user_retrieval_params WHERE user_id = ?",
        )
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?;

        match row {
            Some(r) => Ok(UserRetrievalParams {
                user_id: user_id.to_string(),
                feedback_weight: r.try_get("feedback_weight").unwrap_or(0.1),
                temporal_decay_hours: r.try_get("temporal_decay_hours").unwrap_or(168.0),
                confidence_weight: r.try_get("confidence_weight").unwrap_or(0.1),
            }),
            None => Ok(UserRetrievalParams {
                user_id: user_id.to_string(),
                ..Default::default()
            }),
        }
    }

    /// Update user's retrieval parameters.
    pub async fn set_user_retrieval_params(
        &self,
        params: &UserRetrievalParams,
    ) -> Result<(), MemoriaError> {
        let now = Utc::now().format("%Y-%m-%d %H:%M:%S%.6f").to_string();
        sqlx::query(
            "INSERT INTO mem_user_retrieval_params \
             (user_id, feedback_weight, temporal_decay_hours, confidence_weight, updated_at) \
             VALUES (?, ?, ?, ?, ?) \
             ON DUPLICATE KEY UPDATE \
             feedback_weight = VALUES(feedback_weight), \
             temporal_decay_hours = VALUES(temporal_decay_hours), \
             confidence_weight = VALUES(confidence_weight), \
             updated_at = VALUES(updated_at)",
        )
        .bind(&params.user_id)
        .bind(params.feedback_weight)
        .bind(params.temporal_decay_hours)
        .bind(params.confidence_weight)
        .bind(&now)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    /// Get access_count for a set of memory IDs.
    pub async fn get_access_counts(
        &self,
        memory_ids: &[String],
    ) -> Result<std::collections::HashMap<String, i32>, MemoriaError> {
        let mut map = std::collections::HashMap::new();
        if memory_ids.is_empty() {
            return Ok(map);
        }
        for chunk in memory_ids.chunks(500) {
            let placeholders: Vec<&str> = chunk.iter().map(|_| "?").collect();
            let sql = format!(
                "SELECT memory_id, access_count FROM mem_memories_stats WHERE memory_id IN ({})",
                placeholders.join(", ")
            );
            let mut q = sqlx::query(&sql);
            for id in chunk {
                q = q.bind(id);
            }
            let rows = q.fetch_all(&self.pool).await.map_err(db_err)?;
            for row in &rows {
                let id: String = row.try_get("memory_id").map_err(db_err)?;
                let count: i32 = row.try_get("access_count").map_err(db_err)?;
                map.insert(id, count);
            }
        }
        Ok(map)
    }

    /// Detect pollution: high supersede ratio in recent changes (threshold=0.3).
    pub async fn detect_pollution(
        &self,
        user_id: &str,
        since_hours: i64,
    ) -> Result<bool, MemoriaError> {
        let row: (i64, i64) = sqlx::query_as(
            "SELECT COUNT(*) as total_changes, \
             SUM(CASE WHEN superseded_by IS NOT NULL THEN 1 ELSE 0 END) as supersedes \
             FROM mem_memories \
             WHERE user_id = ? AND updated_at >= DATE_SUB(NOW(), INTERVAL ? HOUR)",
        )
        .bind(user_id)
        .bind(since_hours)
        .fetch_one(&self.pool)
        .await
        .map_err(db_err)?;
        let (total, supersedes) = row;
        if total == 0 {
            return Ok(false);
        }
        Ok(supersedes as f64 / total as f64 > 0.3)
    }

    /// Per-type stats: count, avg_confidence, contradiction_rate, avg_staleness_hours.
    pub async fn health_analyze(&self, user_id: &str) -> Result<serde_json::Value, MemoriaError> {
        let rows: Vec<(String, i64, f64, i64, f64)> = sqlx::query_as(
            "SELECT memory_type, COUNT(*) as total, AVG(initial_confidence) as avg_conf, \
             COUNT(CASE WHEN superseded_by IS NOT NULL THEN 1 END) as superseded, \
             AVG(TIMESTAMPDIFF(HOUR, observed_at, NOW())) as avg_stale_h \
             FROM mem_memories WHERE user_id = ? GROUP BY memory_type",
        )
        .bind(user_id)
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;

        let mut stats = serde_json::Map::new();
        for (mtype, total, avg_conf, superseded, avg_stale) in rows {
            let contradiction_rate = if total > 0 {
                superseded as f64 / total as f64
            } else {
                0.0
            };
            stats.insert(
                mtype,
                serde_json::json!({
                    "total": total,
                    "avg_confidence": avg_conf,
                    "contradiction_rate": contradiction_rate,
                    "avg_staleness_hours": avg_stale,
                }),
            );
        }
        Ok(serde_json::Value::Object(stats))
    }

    /// Storage stats: total, active, inactive, avg_content_size, oldest, newest.
    pub async fn health_storage_stats(
        &self,
        user_id: &str,
    ) -> Result<serde_json::Value, MemoriaError> {
        let row: (i64, i64, f64) = sqlx::query_as(
            "SELECT COUNT(*) as total, \
             SUM(CASE WHEN is_active = 1 THEN 1 ELSE 0 END) as active, \
             AVG(LENGTH(content)) as avg_content_size \
             FROM mem_memories WHERE user_id = ?",
        )
        .bind(user_id)
        .fetch_one(&self.pool)
        .await
        .map_err(db_err)?;

        Ok(serde_json::json!({
            "total": row.0,
            "active": row.1,
            "inactive": row.0 - row.1,
            "avg_content_size": row.2,
        }))
    }

    /// IVF capacity estimate: global vector count + growth rate + recommendation.
    pub async fn health_capacity(&self, user_id: &str) -> Result<serde_json::Value, MemoriaError> {
        const IVF_OPTIMAL: i64 = 50_000;
        const IVF_DEGRADED: i64 = 200_000;

        let (user_active,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM mem_memories WHERE user_id = ? AND is_active = 1")
                .bind(user_id)
                .fetch_one(&self.pool)
                .await
                .map_err(db_err)?;

        let (global_total,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM mem_memories WHERE is_active = 1")
                .fetch_one(&self.pool)
                .await
                .map_err(db_err)?;

        let (added_30d,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM mem_memories WHERE user_id = ? AND observed_at >= NOW() - INTERVAL 30 DAY"
        ).bind(user_id).fetch_one(&self.pool).await.map_err(db_err)?;

        let recommendation = if global_total > IVF_DEGRADED {
            "partition_required"
        } else if global_total > IVF_OPTIMAL {
            "monitor_query_latency"
        } else {
            "ok"
        };

        Ok(serde_json::json!({
            "user_active_memories": user_active,
            "global_vector_count": global_total,
            "monthly_growth_rate": added_30d,
            "ivf_thresholds": {"optimal": IVF_OPTIMAL, "degraded": IVF_DEGRADED},
            "recommendation": recommendation,
        }))
    }

    // ── Batch reads ─────────────────────────────────────────────────────────

    /// Fetch multiple memories by IDs. Returns map of memory_id → Memory.
    pub async fn get_by_ids(
        &self,
        ids: &[String],
    ) -> Result<std::collections::HashMap<String, Memory>, MemoriaError> {
        if ids.is_empty() {
            return Ok(Default::default());
        }
        let mut map = std::collections::HashMap::new();
        // Batch in chunks of 500 to avoid SQL length limits
        for chunk in ids.chunks(500) {
            let ph = chunk.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            let sql = format!(
                "SELECT memory_id, user_id, memory_type, content, \
                 embedding AS emb_str, session_id, \
                 CAST(source_event_ids AS CHAR) AS src_ids, \
                 CAST(extra_metadata AS CHAR) AS extra_meta, \
                 is_active, superseded_by, trust_tier, initial_confidence, \
                 observed_at, created_at, updated_at \
                 FROM mem_memories WHERE memory_id IN ({ph}) AND is_active = 1"
            );
            let mut q = sqlx::query(&sql);
            for id in chunk {
                q = q.bind(id);
            }
            let rows = q.fetch_all(&self.pool).await.map_err(db_err)?;
            for r in &rows {
                let m = row_to_memory(r)?;
                map.insert(m.memory_id.clone(), m);
            }
        }
        Ok(map)
    }

    // ── Table-aware CRUD ──────────────────────────────────────────────────────

    /// Find the nearest active memory by embedding distance.
    /// Returns (memory_id, content, l2_distance) if within threshold.
    ///
    /// NOTE: l2_threshold assumes normalized embeddings (unit vectors).
    /// For normalized vectors: L2 = sqrt(2 * (1 - cosine_similarity)).
    /// The IVF index uses vector_l2_ops, so this query benefits from the index.
    pub async fn find_near_duplicate(
        &self,
        table: &str,
        user_id: &str,
        embedding: &[f32],
        memory_type: &str,
        exclude_id: &str,
        l2_threshold: f64,
    ) -> Result<Option<(String, String, f64)>, MemoriaError> {
        // Same-type first, then cross-type fallback
        if let Some(hit) = self
            .find_near_duplicate_inner(
                table,
                user_id,
                embedding,
                Some(memory_type),
                exclude_id,
                l2_threshold,
            )
            .await?
        {
            return Ok(Some(hit));
        }
        self.find_near_duplicate_inner(table, user_id, embedding, None, exclude_id, l2_threshold)
            .await
    }

    async fn find_near_duplicate_inner(
        &self,
        table: &str,
        user_id: &str,
        embedding: &[f32],
        memory_type: Option<&str>,
        exclude_id: &str,
        l2_threshold: f64,
    ) -> Result<Option<(String, String, f64)>, MemoriaError> {
        let vec_literal = vec_to_mo(embedding);
        let type_filter = match memory_type {
            Some(_) => "AND memory_type = ? ",
            None => "",
        };
        let sql = format!(
            "SELECT memory_id, content, \
             l2_distance(embedding, '{vec_literal}') AS l2_dist \
             FROM {table} \
             WHERE user_id = ? AND is_active = 1 {type_filter}\
               AND embedding IS NOT NULL AND vector_dims(embedding) > 0 \
               AND memory_id != ? \
             ORDER BY l2_dist ASC LIMIT 1 by rank with option 'mode=post'"
        );
        let mut q = sqlx::query(&sql).bind(user_id);
        if let Some(mt) = memory_type {
            q = q.bind(mt);
        }
        let row = q
            .bind(exclude_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(db_err)?;
        if let Some(r) = row {
            let dist: f64 = r
                .try_get::<f64, _>("l2_dist")
                .or_else(|_| r.try_get::<f32, _>("l2_dist").map(|v| v as f64))
                .unwrap_or(f64::MAX);
            if dist <= l2_threshold {
                let mid: String = r.try_get("memory_id").map_err(db_err)?;
                let content: String = r.try_get("content").map_err(db_err)?;
                return Ok(Some((mid, content, dist)));
            }
        }
        Ok(None)
    }

    /// Mark a memory as superseded by another.
    /// Branch-aware soft-delete: deactivate a memory in the given table.
    pub async fn soft_delete_from(
        &self,
        table: &str,
        memory_id: &str,
    ) -> Result<(), MemoriaError> {
        let now = Utc::now().naive_utc();
        sqlx::query(&format!(
            "UPDATE {table} SET is_active = 0, updated_at = ? WHERE memory_id = ?"
        ))
        .bind(now)
        .bind(memory_id)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    /// Branch-aware get: fetch an active memory from the given table.
    pub async fn get_from(
        &self,
        table: &str,
        memory_id: &str,
    ) -> Result<Option<Memory>, MemoriaError> {
        let row = sqlx::query(&format!(
            "SELECT memory_id, user_id, memory_type, content, \
             embedding AS emb_str, session_id, \
             CAST(source_event_ids AS CHAR) AS src_ids, \
             CAST(extra_metadata AS CHAR) AS extra_meta, \
             is_active, superseded_by, trust_tier, initial_confidence, \
             observed_at, created_at, updated_at \
             FROM {table} WHERE memory_id = ? AND is_active = 1"
        ))
        .bind(memory_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?;
        row.map(|r| row_to_memory(&r)).transpose()
    }

    pub async fn supersede_memory(
        &self,
        table: &str,
        old_id: &str,
        new_id: &str,
    ) -> Result<(), MemoriaError> {
        sqlx::query(&format!(
            "UPDATE {table} SET is_active = 0, superseded_by = ?, updated_at = NOW() WHERE memory_id = ?"
        )).bind(new_id).bind(old_id)
        .execute(&self.pool).await.map_err(db_err)?;
        Ok(())
    }

    #[tracing::instrument(skip(self, memory), fields(memory_id = %memory.memory_id))]
    pub async fn insert_into(&self, table: &str, memory: &Memory) -> Result<(), MemoriaError> {
        let now = Utc::now().naive_utc();
        let observed_at = memory.observed_at.map(|dt| dt.naive_utc()).unwrap_or(now);
        let created_at = memory.created_at.map(|dt| dt.naive_utc()).unwrap_or(now);
        let source_event_ids = serde_json::to_string(&memory.source_event_ids)?;
        // Workaround: MO#23859 — PREPARE/EXECUTE corrupts NULL JSON on 2nd+ execution.
        let extra_metadata = memory
            .extra_metadata
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?
            .unwrap_or_else(|| "{}".to_string());
        let embedding = memory
            .embedding
            .as_deref()
            .filter(|v| !v.is_empty()) // Some([]) → None → SQL NULL
            .map(vec_to_mo);

        sqlx::query(&format!(
            r#"INSERT INTO {table}
               (memory_id, user_id, memory_type, content, embedding, session_id,
                source_event_ids, extra_metadata, is_active, superseded_by,
                trust_tier, initial_confidence, observed_at, created_at, updated_at)
               VALUES (?, ?, ?, ?, ?, ?, ?, ?, 1, ?, ?, ?, ?, ?, ?)"#
        ))
        .bind(&memory.memory_id)
        .bind(&memory.user_id)
        .bind(memory.memory_type.to_string())
        .bind(&memory.content)
        .bind(embedding)
        .bind(&memory.session_id)
        .bind(source_event_ids)
        .bind(extra_metadata)
        .bind(&memory.superseded_by)
        .bind(memory.trust_tier.to_string())
        .bind(memory.initial_confidence as f32)
        .bind(observed_at)
        .bind(created_at)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    /// Batch-insert multiple memories in a single multi-row INSERT statement.
    /// Falls back to single inserts if the batch is empty.
    pub async fn batch_insert_into(
        &self,
        table: &str,
        memories: &[&Memory],
    ) -> Result<(), MemoriaError> {
        if memories.is_empty() {
            return Ok(());
        }
        // Chunk to avoid oversized SQL statements
        for chunk in memories.chunks(50) {
            let placeholders = chunk
                .iter()
                .map(|_| "(?, ?, ?, ?, ?, ?, ?, ?, 1, ?, ?, ?, ?, ?, ?)")
                .collect::<Vec<_>>()
                .join(", ");
            let sql = format!(
                "INSERT INTO {table} \
                 (memory_id, user_id, memory_type, content, embedding, session_id, \
                  source_event_ids, extra_metadata, is_active, superseded_by, \
                  trust_tier, initial_confidence, observed_at, created_at, updated_at) \
                 VALUES {placeholders}"
            );
            let now = Utc::now().naive_utc();
            let mut q = sqlx::query(&sql);
            for m in chunk {
                let observed_at = m.observed_at.map(|dt| dt.naive_utc()).unwrap_or(now);
                let created_at = m.created_at.map(|dt| dt.naive_utc()).unwrap_or(now);
                let source_event_ids = serde_json::to_string(&m.source_event_ids)?;
                let extra_metadata = m
                    .extra_metadata
                    .as_ref()
                    .map(serde_json::to_string)
                    .transpose()?
                    .unwrap_or_else(|| "{}".to_string());
                let embedding = m
                    .embedding
                    .as_deref()
                    .filter(|v| !v.is_empty())
                    .map(vec_to_mo);
                q = q
                    .bind(m.memory_id.clone())
                    .bind(m.user_id.clone())
                    .bind(m.memory_type.to_string())
                    .bind(m.content.clone())
                    .bind(embedding)
                    .bind(m.session_id.clone())
                    .bind(source_event_ids)
                    .bind(extra_metadata)
                    .bind(m.superseded_by.clone())
                    .bind(m.trust_tier.to_string())
                    .bind(m.initial_confidence as f32)
                    .bind(observed_at)
                    .bind(created_at)
                    .bind(now);
            }
            q.execute(&self.pool).await.map_err(db_err)?;
        }
        Ok(())
    }

    /// Batch-insert multiple edit log entries in a single statement.
    pub async fn batch_log_edit(&self, entries: &[EditLogEntry<'_>]) {
        if entries.is_empty() {
            return;
        }
        for chunk in entries.chunks(50) {
            let placeholders = chunk
                .iter()
                .map(|_| "(?, ?, ?, ?, ?, ?, ?, ?)")
                .collect::<Vec<_>>()
                .join(", ");
            let sql = format!(
                "INSERT INTO mem_edit_log \
                 (edit_id, user_id, memory_id, operation, payload, reason, snapshot_before, created_by) \
                 VALUES {placeholders}"
            );
            let mut q = sqlx::query(&sql);
            for (user_id, operation, memory_id, payload, reason, snapshot_before) in chunk {
                q = q
                    .bind(uuid7_id())
                    .bind(*user_id)
                    .bind(*memory_id)
                    .bind(*operation)
                    .bind(*payload)
                    .bind(*reason)
                    .bind(*snapshot_before)
                    .bind(*user_id);
            }
            let _ = q.execute(&self.pool).await;
        }
    }

    pub async fn list_active_from(
        &self,
        table: &str,
        user_id: &str,
        limit: i64,
    ) -> Result<Vec<Memory>, MemoriaError> {
        let rows = sqlx::query(&format!(
            "SELECT memory_id, user_id, memory_type, content, \
             embedding AS emb_str, session_id, \
             CAST(source_event_ids AS CHAR) AS src_ids, \
             CAST(extra_metadata AS CHAR) AS extra_meta, \
             is_active, superseded_by, trust_tier, initial_confidence, \
             observed_at, created_at, updated_at \
             FROM {table} WHERE user_id = ? AND is_active = 1 \
             ORDER BY created_at DESC LIMIT ?"
        ))
        .bind(user_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;
        rows.iter().map(row_to_memory).collect()
    }

    /// Find memory IDs whose content contains `topic` (exact substring match).
    /// Uses fulltext boolean MUST with LIKE refinement. Requires topic >= 3 chars.
    pub async fn find_ids_by_topic(
        &self,
        table: &str,
        user_id: &str,
        topic: &str,
    ) -> Result<Vec<String>, MemoriaError> {
        // Require minimum length to avoid full table scan
        if topic.trim().len() < 3 {
            return Err(MemoriaError::Validation(
                "topic must be at least 3 characters".into(),
            ));
        }
        let ft_safe = sanitize_fulltext_query(topic);
        let like_safe = sanitize_like_pattern(topic);
        if ft_safe.is_empty() {
            return Ok(vec![]);
        }
        let ft_terms: String = ft_safe
            .split_whitespace()
            .map(|w| format!("+{w}"))
            .collect::<Vec<_>>()
            .join(" ");
        let sql = format!(
            "SELECT memory_id FROM {table} \
             WHERE user_id = ? AND is_active = 1 \
               AND MATCH(content) AGAINST('{ft_terms}' IN BOOLEAN MODE) \
               AND content LIKE ?"
        );
        let like_pat = format!("%{like_safe}%");
        let rows: Vec<(String,)> = sqlx::query_as(&sql)
            .bind(user_id)
            .bind(&like_pat)
            .fetch_all(&self.pool)
            .await
            .map_err(db_err)?;
        Ok(rows.into_iter().map(|r| r.0).collect())
    }

    #[tracing::instrument(skip(self))]
    pub async fn search_fulltext_from(
        &self,
        table: &str,
        user_id: &str,
        query: &str,
        limit: i64,
    ) -> Result<Vec<Memory>, MemoriaError> {
        let safe = sanitize_fulltext_query(query);
        if safe.is_empty() {
            return Ok(vec![]);
        }
        // Use OR semantics (no + prefix) — AND is too strict for natural language queries
        // because stopwords are removed from the index but +stopword still requires a match.
        let sql = format!(
            "SELECT memory_id, user_id, memory_type, content, \
             embedding AS emb_str, session_id, \
             CAST(source_event_ids AS CHAR) AS src_ids, \
             CAST(extra_metadata AS CHAR) AS extra_meta, \
             is_active, superseded_by, trust_tier, initial_confidence, \
             observed_at, created_at, updated_at, \
             MATCH(content) AGAINST('{safe}' IN BOOLEAN MODE) AS ft_score \
             FROM {table} \
             WHERE user_id = ? AND is_active = 1 \
               AND MATCH(content) AGAINST('{safe}' IN BOOLEAN MODE) \
             ORDER BY ft_score DESC LIMIT ?"
        );
        let rows = match sqlx::query(&sql)
            .bind(user_id)
            .bind(limit)
            .fetch_all(&self.pool)
            .await
        {
            Ok(rows) => rows,
            Err(e) => {
                // MatrixOne returns 20101 when the search string tokenizes to an empty pattern
                // (e.g. all stopwords, single chars, or unsupported Unicode). Treat as no results.
                let msg = e.to_string();
                if msg.contains("20101") && msg.contains("empty pattern") {
                    return Ok(vec![]);
                }
                return Err(db_err(e));
            }
        };
        rows.iter()
            .map(|r| {
                let mut m = row_to_memory(r)?;
                if let Ok(ft) = r.try_get::<f64, _>("ft_score") {
                    m.retrieval_score = Some(ft);
                } else if let Ok(ft) = r.try_get::<f32, _>("ft_score") {
                    m.retrieval_score = Some(ft as f64);
                }
                Ok(m)
            })
            .collect()
    }

    pub async fn search_vector_from(
        &self,
        table: &str,
        user_id: &str,
        embedding: &[f32],
        limit: i64,
    ) -> Result<Vec<Memory>, MemoriaError> {
        self.search_vector_from_filtered(table, user_id, embedding, limit, None)
            .await
    }

    /// Vector search with optional memory_type pre-filter to reduce scan set.
    pub async fn search_vector_from_filtered(
        &self,
        table: &str,
        user_id: &str,
        embedding: &[f32],
        limit: i64,
        memory_type: Option<&str>,
    ) -> Result<Vec<Memory>, MemoriaError> {
        let vec_literal = vec_to_mo(embedding);
        let type_clause = match memory_type {
            Some(mt) => format!(" AND memory_type = '{}'", sanitize_sql_literal(mt)),
            None => String::new(),
        };
        // MatrixOne bug workaround: prepared statement with l2_distance in ORDER BY returns 0 rows
        // Solution: inline all parameters instead of using bind()
        let sql = format!(
            "SELECT memory_id, user_id, memory_type, content, \
             session_id, \
             CAST(source_event_ids AS CHAR) AS src_ids, \
             CAST(extra_metadata AS CHAR) AS extra_meta, \
             is_active, superseded_by, trust_tier, initial_confidence, \
             observed_at, created_at, updated_at \
             FROM {table} \
             WHERE user_id = '{}' AND is_active = 1 AND embedding IS NOT NULL{type_clause} \
             ORDER BY l2_distance(embedding, '{vec_literal}') ASC \
             LIMIT {} by rank with option 'mode=post'",
            sanitize_sql_literal(user_id),
            limit
        );

        let rows = sqlx::query(&sql)
            .fetch_all(&self.pool)
            .await
            .map_err(db_err)?;

        rows.iter().map(row_to_memory).collect()
    }

    /// Hybrid search: vector + fulltext, merged with 4-dimension weighted scoring.
    /// Weights: vector=0.3, keyword=0.2, temporal=0.2, confidence=0.3 (matches Python "default")
    #[tracing::instrument(skip(self, embedding))]
    pub async fn search_hybrid_from(
        &self,
        table: &str,
        user_id: &str,
        embedding: &[f32],
        query: &str,
        limit: i64,
    ) -> Result<Vec<Memory>, MemoriaError> {
        let params = self
            .get_user_retrieval_params(user_id)
            .await
            .unwrap_or_default();
        let (mems, _) = self
            .search_hybrid_from_scored(
                table,
                user_id,
                embedding,
                query,
                limit,
                params.feedback_weight,
            )
            .await?;
        Ok(mems)
    }

    /// Like search_hybrid_from but also returns per-candidate score breakdown.
    /// scores: (memory_id, vec_score, kw_score, time_score, conf_score, final_score)
    pub async fn search_hybrid_from_scored(
        &self,
        table: &str,
        user_id: &str,
        embedding: &[f32],
        query: &str,
        limit: i64,
        feedback_weight: f64,
    ) -> Result<(Vec<Memory>, Vec<(String, f64, f64, f64, f64, f64)>), MemoriaError> {
        let fetch_k = (limit * 3).max(20);
        let vec_results = self
            .search_vector_from(table, user_id, embedding, fetch_k)
            .await?;
        let ft_results = self
            .search_fulltext_from(table, user_id, query, fetch_k)
            .await
            .unwrap_or_default();

        let ft_map: std::collections::HashMap<String, f64> = ft_results
            .iter()
            .filter_map(|m| m.retrieval_score.map(|s| (m.memory_id.clone(), s)))
            .collect();

        let mut seen: std::collections::HashSet<String> =
            vec_results.iter().map(|m| m.memory_id.clone()).collect();
        let mut candidates = vec_results;
        for m in ft_results {
            if seen.insert(m.memory_id.clone()) {
                candidates.push(m);
            }
        }

        let now = chrono::Utc::now();
        const DECAY_HOURS: f64 = 168.0;
        const W_VEC: f64 = 0.3;
        const W_KW: f64 = 0.2;
        const W_TIME: f64 = 0.2;
        const W_CONF: f64 = 0.3;

        // Per-tier half-life (days) — higher-trust memories decay slower.
        fn half_life_for(tier: &memoria_core::TrustTier) -> f64 {
            tier.default_half_life_days()
        }

        let mut score_breakdown: Vec<(String, f64, f64, f64, f64, f64)> = Vec::new();

        // Fetch access counts for frequency boost
        let ac_ids: Vec<String> = candidates.iter().map(|m| m.memory_id.clone()).collect();
        let ac_map = self.get_access_counts(&ac_ids).await.unwrap_or_default();

        // Fetch feedback for relevance adjustment
        let fb_map = self.get_feedback_batch(&ac_ids).await.unwrap_or_default();

        for m in &mut candidates {
            let vec_score = m.retrieval_score.unwrap_or(0.0);
            let raw_ft = ft_map.get(&m.memory_id).copied().unwrap_or(0.0);
            let kw_score = if raw_ft > 0.0 {
                raw_ft / (raw_ft + 1.0)
            } else {
                0.0
            };
            let (time_score, conf_score) = if let Some(obs) = m.observed_at {
                let age_hours = (now - obs).num_seconds() as f64 / 3600.0;
                let age_days = age_hours / 24.0;
                let ts = (-age_hours / DECAY_HOURS).max(-500.0).exp();
                let hl = half_life_for(&m.trust_tier);
                let cs = m.initial_confidence * (-age_days / hl).max(-500.0).exp();
                (ts, cs)
            } else {
                (0.0, m.initial_confidence)
            };
            let mut final_score =
                W_VEC * vec_score + W_KW * kw_score + W_TIME * time_score + W_CONF * conf_score;
            // Frequency boost: log(1 + access_count) — mild boost for frequently retrieved memories
            let ac = ac_map.get(&m.memory_id).copied().unwrap_or(0);
            if ac > 0 {
                final_score *= 1.0 + 0.1 * ((1 + ac) as f64).ln();
            }

            // Feedback adjustment: boost useful, penalize negative feedback
            if let Some(fb) = fb_map.get(&m.memory_id) {
                let positive = fb.useful as f64;
                let negative = (fb.irrelevant + fb.outdated + fb.wrong) as f64;
                // Net feedback score: positive boosts, negative penalizes
                // Formula: multiplier = 1 + feedback_weight * (useful - 0.5 * negative)
                let feedback_delta = positive - 0.5 * negative;
                if feedback_delta.abs() > 0.01 {
                    final_score *= (1.0 + feedback_weight * feedback_delta).clamp(0.5, 2.0);
                }
            }

            m.access_count = ac;
            m.retrieval_score = Some(final_score);
            score_breakdown.push((
                m.memory_id.clone(),
                vec_score,
                kw_score,
                time_score,
                conf_score,
                final_score,
            ));
        }

        // Drop memories whose effective confidence has decayed to near-zero.
        // This prevents long-expired facts from appearing in results.
        const MIN_CONF: f64 = 0.05;
        let live: std::collections::HashSet<String> = score_breakdown
            .iter()
            .filter(|(_, _, _, _, cs, _)| *cs >= MIN_CONF)
            .map(|(id, ..)| id.clone())
            .collect();
        candidates.retain(|m| live.contains(&m.memory_id));
        score_breakdown.retain(|(id, ..)| live.contains(id));

        candidates.sort_by(|a, b| {
            b.retrieval_score
                .partial_cmp(&a.retrieval_score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        // Re-sort score_breakdown to match candidate order
        let order: std::collections::HashMap<String, usize> = candidates
            .iter()
            .enumerate()
            .map(|(i, m)| (m.memory_id.clone(), i))
            .collect();
        score_breakdown.sort_by_key(|(id, ..)| order.get(id).copied().unwrap_or(usize::MAX));
        candidates.truncate(limit as usize);
        score_breakdown.truncate(limit as usize);
        Ok((candidates, score_breakdown))
    }

    // ── Entity links ──────────────────────────────────────────────────────────

    // TODO(perf): When mem_entity_links grows large, add indexes:
    //   - (user_id, memory_id) for get_linked_memory_ids
    //   - (user_id, entity_name, entity_type) for get_entity_names

    /// Returns memory_ids that already have entity links for a user.
    pub async fn get_linked_memory_ids(
        &self,
        user_id: &str,
    ) -> Result<std::collections::HashSet<String>, MemoriaError> {
        let rows = sqlx::query("SELECT DISTINCT memory_id FROM mem_entity_links WHERE user_id = ?")
            .bind(user_id)
            .fetch_all(&self.pool)
            .await
            .map_err(db_err)?;
        Ok(rows
            .iter()
            .filter_map(|r| r.try_get::<String, _>("memory_id").ok())
            .collect())
    }

    /// Returns all entity names for a user (for existing_entities list).
    pub async fn get_entity_names(
        &self,
        user_id: &str,
    ) -> Result<Vec<(String, String)>, MemoriaError> {
        let rows = sqlx::query(
            "SELECT DISTINCT entity_name, entity_type FROM mem_entity_links WHERE user_id = ? ORDER BY entity_name"
        )
        .bind(user_id)
        .fetch_all(&self.pool).await.map_err(db_err)?;
        Ok(rows
            .iter()
            .filter_map(|r| {
                let name = r.try_get::<String, _>("entity_name").ok()?;
                let etype = r.try_get::<String, _>("entity_type").ok()?;
                Some((name, etype))
            })
            .collect())
    }

    /// Insert entity links for a memory. Skips duplicates.
    pub async fn insert_entity_links(
        &self,
        user_id: &str,
        memory_id: &str,
        entities: &[(String, String)], // (name, type)
    ) -> Result<(usize, usize), MemoriaError> {
        if entities.is_empty() {
            return Ok((0, 0));
        }
        // Fetch existing entity names for this (user, memory) pair
        let existing: std::collections::HashSet<String> = {
            let rows = sqlx::query(
                "SELECT entity_name FROM mem_entity_links WHERE user_id = ? AND memory_id = ?",
            )
            .bind(user_id)
            .bind(memory_id)
            .fetch_all(&self.pool)
            .await
            .map_err(db_err)?;
            rows.iter()
                .filter_map(|r| r.try_get::<String, _>("entity_name").ok())
                .collect()
        };
        // Partition into new vs reused, dedup by lowercased name within the batch
        let mut seen = std::collections::HashSet::new();
        let mut to_insert: Vec<(String, &str)> = Vec::new(); // (name_lc, entity_type)
        let mut reused = 0usize;
        for (name, etype) in entities {
            let name_lc = name.to_lowercase();
            if existing.contains(&name_lc) || !seen.insert(name_lc.clone()) {
                reused += 1;
                continue;
            }
            to_insert.push((name_lc, etype.as_str()));
        }
        if to_insert.is_empty() {
            return Ok((0, reused));
        }
        let now = chrono::Utc::now().naive_utc();
        for chunk in to_insert.chunks(50) {
            let placeholders = chunk
                .iter()
                .map(|_| "(?, ?, ?, ?, ?, 'manual', ?)")
                .collect::<Vec<_>>()
                .join(", ");
            let sql = format!(
                "INSERT INTO mem_entity_links \
                 (id, user_id, memory_id, entity_name, entity_type, source, created_at) \
                 VALUES {placeholders}"
            );
            let mut q = sqlx::query(&sql);
            for (name_lc, etype) in chunk {
                let id = uuid::Uuid::new_v4().to_string().replace('-', "");
                q = q
                    .bind(id)
                    .bind(user_id)
                    .bind(memory_id)
                    .bind(name_lc.as_str())
                    .bind(*etype)
                    .bind(now);
            }
            q.execute(&self.pool).await.map_err(db_err)?;
        }
        Ok((to_insert.len(), reused))
    }
}

#[async_trait]
impl MemoryStore for SqlMemoryStore {
    async fn insert(&self, memory: &Memory) -> Result<(), MemoriaError> {
        self.insert_into("mem_memories", memory).await
    }

    async fn get(&self, memory_id: &str) -> Result<Option<Memory>, MemoriaError> {
        let row = sqlx::query(
            "SELECT memory_id, user_id, memory_type, content, \
             embedding AS emb_str, session_id, \
             CAST(source_event_ids AS CHAR) AS src_ids, \
             CAST(extra_metadata AS CHAR) AS extra_meta, \
             is_active, superseded_by, trust_tier, initial_confidence, \
             observed_at, created_at, updated_at \
             FROM mem_memories WHERE memory_id = ? AND is_active = 1",
        )
        .bind(memory_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?;
        row.map(|r| row_to_memory(&r)).transpose()
    }

    async fn update(&self, memory: &Memory) -> Result<(), MemoriaError> {
        let now = Utc::now().naive_utc();
        // Workaround: MO#23859 — PREPARE/EXECUTE corrupts NULL JSON on 2nd+ execution.
        let extra_metadata = memory
            .extra_metadata
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?
            .unwrap_or_else(|| "{}".to_string());
        sqlx::query(
            r#"UPDATE mem_memories
               SET content = ?, memory_type = ?, trust_tier = ?,
                   initial_confidence = ?, extra_metadata = ?,
                   superseded_by = ?, updated_at = ?
               WHERE memory_id = ?"#,
        )
        .bind(&memory.content)
        .bind(memory.memory_type.to_string())
        .bind(memory.trust_tier.to_string())
        .bind(memory.initial_confidence as f32)
        .bind(extra_metadata)
        .bind(&memory.superseded_by)
        .bind(now)
        .bind(&memory.memory_id)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn soft_delete(&self, memory_id: &str) -> Result<(), MemoriaError> {
        let now = Utc::now().naive_utc();
        sqlx::query("UPDATE mem_memories SET is_active = 0, updated_at = ? WHERE memory_id = ?")
            .bind(now)
            .bind(memory_id)
            .execute(&self.pool)
            .await
            .map_err(db_err)?;
        Ok(())
    }

    async fn list_active(&self, user_id: &str, limit: i64) -> Result<Vec<Memory>, MemoriaError> {
        self.list_active_from("mem_memories", user_id, limit).await
    }

    async fn search_fulltext(
        &self,
        user_id: &str,
        query: &str,
        limit: i64,
    ) -> Result<Vec<Memory>, MemoriaError> {
        self.search_fulltext_from("mem_memories", user_id, query, limit)
            .await
    }

    async fn search_vector(
        &self,
        user_id: &str,
        embedding: &[f32],
        limit: i64,
    ) -> Result<Vec<Memory>, MemoriaError> {
        self.search_vector_from("mem_memories", user_id, embedding, limit)
            .await
    }
}

fn row_to_memory(row: &sqlx::mysql::MySqlRow) -> Result<Memory, MemoriaError> {
    let memory_type_str: String = row.try_get("memory_type").map_err(db_err)?;
    let trust_tier_str: String = row.try_get("trust_tier").map_err(db_err)?;

    let source_event_ids: Vec<String> = {
        let s: String = row.try_get("src_ids").map_err(db_err)?;
        serde_json::from_str(&s)?
    };
    let extra_metadata = {
        let s: Option<String> = row.try_get("extra_meta").map_err(db_err)?;
        // Workaround: MO#23859 — we store "{}" instead of NULL; treat empty object as None.
        s.filter(|v| v != "{}")
            .map(|v| serde_json::from_str(&v))
            .transpose()?
    };
    let embedding: Option<Vec<f32>> = {
        // Try emb_str first (for compatibility with old queries that use CAST)
        if let Ok(Some(s)) = row.try_get::<Option<String>, _>("emb_str") {
            Some(mo_to_vec(&s)?)
        } else if let Ok(Some(s)) = row.try_get::<Option<String>, _>("embedding") {
            // Direct embedding column (MatrixOne returns vector as string)
            Some(mo_to_vec(&s)?)
        } else {
            // No embedding column in result set (e.g., vector search queries)
            None
        }
    };
    let observed_at = row
        .try_get::<chrono::NaiveDateTime, _>("observed_at")
        .ok()
        .map(|dt| dt.and_utc());
    let created_at = row
        .try_get::<chrono::NaiveDateTime, _>("created_at")
        .ok()
        .map(|dt| dt.and_utc());
    let updated_at = row
        .try_get::<chrono::NaiveDateTime, _>("updated_at")
        .ok()
        .map(|dt| dt.and_utc());

    Ok(Memory {
        memory_id: row.try_get("memory_id").map_err(db_err)?,
        user_id: row.try_get("user_id").map_err(db_err)?,
        memory_type: MemoryType::from_str(&memory_type_str)?,
        content: row.try_get("content").map_err(db_err)?,
        initial_confidence: row
            .try_get::<f32, _>("initial_confidence")
            .map_err(db_err)? as f64,
        embedding,
        source_event_ids,
        superseded_by: row.try_get("superseded_by").map_err(db_err)?,
        is_active: {
            let v: i8 = row.try_get("is_active").map_err(db_err)?;
            v != 0
        },
        access_count: 0,
        session_id: row.try_get("session_id").map_err(db_err)?,
        observed_at,
        created_at,
        updated_at,
        extra_metadata,
        trust_tier: TrustTier::from_str(&trust_tier_str)?,
        retrieval_score: None,
    })
}
