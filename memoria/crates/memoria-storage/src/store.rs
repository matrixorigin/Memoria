use async_trait::async_trait;
use chrono::Utc;
use memoria_core::{interfaces::MemoryStore, Memory, MemoriaError, MemoryType, TrustTier};
use sqlx::{mysql::MySqlPool, Row};
use std::str::FromStr;

pub(crate) fn db_err(e: sqlx::Error) -> MemoriaError {
    MemoriaError::Database(e.to_string())
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
    format!("[{}]", v.iter().map(|f| f.to_string()).collect::<Vec<_>>().join(","))
}

fn mo_to_vec(s: &str) -> Result<Vec<f32>, MemoriaError> {
    let inner = s.trim_matches(|c| c == '[' || c == ']');
    if inner.is_empty() { return Ok(vec![]); }
    inner.split(',')
        .map(|x| x.trim().parse::<f32>()
            .map_err(|e| MemoriaError::Internal(format!("vec parse: {e}"))))
        .collect()
}

#[derive(Clone)]
pub struct SqlMemoryStore {
    pool: MySqlPool,
    embedding_dim: usize,
}

impl SqlMemoryStore {
    pub fn new(pool: MySqlPool, embedding_dim: usize) -> Self {
        Self { pool, embedding_dim }
    }

    pub fn pool(&self) -> &MySqlPool { &self.pool }

    pub fn graph_store(&self) -> crate::graph::GraphStore {
        crate::graph::GraphStore::new(self.pool.clone(), self.embedding_dim)
    }

    pub async fn connect(database_url: &str, embedding_dim: usize) -> Result<Self, MemoriaError> {
        // Auto-create database if it doesn't exist
        if let Some((base_url, db_name)) = database_url.rsplit_once('/') {
            if !db_name.is_empty() {
                let base_pool = sqlx::mysql::MySqlPoolOptions::new()
                    .max_connections(1)
                    .connect(base_url).await;
                if let Ok(base_pool) = base_pool {
                    let _ = sqlx::query(&format!("CREATE DATABASE IF NOT EXISTS {db_name}"))
                        .execute(&base_pool).await;
                }
            }
        }
        let pool = sqlx::mysql::MySqlPoolOptions::new()
            .max_connections(10)
            .idle_timeout(std::time::Duration::from_secs(300))
            .acquire_timeout(std::time::Duration::from_secs(10))
            .connect(database_url).await.map_err(db_err)?;
        Ok(Self::new(pool, embedding_dim))
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
        sqlx::query(&sql).execute(&self.pool).await.map_err(db_err)?;

        // mem_user_state — active branch per user
        sqlx::query(
            r#"CREATE TABLE IF NOT EXISTS mem_user_state (
                user_id       VARCHAR(64)  PRIMARY KEY,
                active_branch VARCHAR(100) NOT NULL DEFAULT 'main',
                updated_at    DATETIME(6)
            )"#,
        ).execute(&self.pool).await.map_err(db_err)?;

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
        ).execute(&self.pool).await.map_err(db_err)?;

        // mem_governance_cooldown — per-user cooldown tracking
        sqlx::query(
            r#"CREATE TABLE IF NOT EXISTS mem_governance_cooldown (
                user_id     VARCHAR(64)  NOT NULL,
                operation   VARCHAR(32)  NOT NULL,
                last_run_at DATETIME(6)  NOT NULL,
                PRIMARY KEY (user_id, operation)
            )"#,
        ).execute(&self.pool).await.map_err(db_err)?;

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
        ).execute(&self.pool).await.map_err(db_err)?;

        // mem_memories_stats — access_count tracking (separated to reduce write contention)
        sqlx::query(
            r#"CREATE TABLE IF NOT EXISTS mem_memories_stats (
                memory_id       VARCHAR(64)  PRIMARY KEY,
                access_count    INT          NOT NULL DEFAULT 0,
                last_accessed_at DATETIME(6)
            )"#,
        ).execute(&self.pool).await.map_err(db_err)?;

        // mem_edit_log — audit log for inject/correct/purge/governance operations
        sqlx::query(
            r#"CREATE TABLE IF NOT EXISTS mem_edit_log (
                edit_id         VARCHAR(64)  PRIMARY KEY,
                user_id         VARCHAR(64)  NOT NULL,
                operation       VARCHAR(64)  NOT NULL,
                target_ids      JSON         DEFAULT NULL,
                reason          TEXT         DEFAULT NULL,
                snapshot_before VARCHAR(64)  DEFAULT NULL,
                created_at      DATETIME(6)  NOT NULL DEFAULT NOW(),
                created_by      VARCHAR(64)  NOT NULL,
                INDEX idx_edit_user (user_id),
                INDEX idx_edit_operation (operation)
            )"#,
        ).execute(&self.pool).await.map_err(db_err)?;

        // Graph tables
        self.graph_store().migrate().await?;

        // Migrate idx_user_active to include memory_type (idempotent)
        let needs_upgrade: bool = sqlx::query_scalar(
            "SELECT COUNT(*) = 0 FROM information_schema.statistics \
             WHERE table_schema = DATABASE() AND table_name = 'mem_memories' \
             AND index_name = 'idx_user_active' AND column_name = 'memory_type'"
        ).fetch_one(&self.pool).await.unwrap_or(false);
        if needs_upgrade {
            let _ = sqlx::query("ALTER TABLE mem_memories DROP INDEX idx_user_active")
                .execute(&self.pool).await;
            let _ = sqlx::query("ALTER TABLE mem_memories ADD INDEX idx_user_active (user_id, is_active, memory_type)")
                .execute(&self.pool).await;
        }

        // Migration: mem_branches may lack table_name column (old schema)
        let has_table_name: bool = sqlx::query_scalar(
            "SELECT COUNT(*) > 0 FROM information_schema.columns \
             WHERE table_schema = DATABASE() AND table_name = 'mem_branches' AND column_name = 'table_name'"
        ).fetch_one(&self.pool).await.unwrap_or(false);
        if !has_table_name {
            let _ = sqlx::query("ALTER TABLE mem_branches ADD COLUMN table_name VARCHAR(100) NOT NULL DEFAULT ''")
                .execute(&self.pool).await;
        }

        // Migration: mem_branches old schema used branch_id/branch_db — recreate with new schema
        let has_id: bool = sqlx::query_scalar(
            "SELECT COUNT(*) > 0 FROM information_schema.columns \
             WHERE table_schema = DATABASE() AND table_name = 'mem_branches' AND column_name = 'id'"
        ).fetch_one(&self.pool).await.unwrap_or(false);
        if !has_id {
            let _ = sqlx::query("DROP TABLE IF EXISTS mem_branches").execute(&self.pool).await;
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
            ).execute(&self.pool).await.map_err(db_err)?;
        }

        Ok(())
    }

    // ── Audit log ─────────────────────────────────────────────────────────────

    /// Create a safety snapshot before destructive operations. Best-effort.
    /// If creation fails (e.g. quota exhausted), tries to drop the 10 oldest
    /// `pre_` safety snapshots and retries once.
    /// Returns `(snapshot_name_or_none, warning_message_or_none)`.
    pub async fn create_safety_snapshot(&self, operation: &str) -> (Option<String>, Option<String>) {
        let name = format!("mem_snap_pre_{}_{}", operation, &uuid::Uuid::new_v4().simple().to_string()[..8]);
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
             WHERE prefix_eq(sname, 'mem_snap_pre_') ORDER BY ts ASC"
        ).fetch_all(&self.pool).await.unwrap_or_default();

        let mut dropped = 0;
        for (name,) in rows.iter().take(n) {
            if sqlx::raw_sql(&format!("DROP SNAPSHOT {name}"))
                .execute(&self.pool).await.is_ok()
            {
                dropped += 1;
            }
        }
        dropped
    }

    /// Write an audit record to mem_edit_log. Best-effort — never fails the caller.
    pub async fn log_edit(
        &self,
        user_id: &str,
        operation: &str,
        target_ids: &[&str],
        reason: &str,
        snapshot_before: Option<&str>,
    ) {
        let tids = serde_json::to_string(target_ids).unwrap_or_else(|_| "[]".to_string());
        let edit_id = uuid::Uuid::new_v4().simple().to_string();
        let _ = sqlx::query(
            "INSERT INTO mem_edit_log (edit_id, user_id, operation, target_ids, reason, snapshot_before, created_by) \
             VALUES (?, ?, ?, ?, ?, ?, ?)"
        )
        .bind(&edit_id)
        .bind(user_id)
        .bind(operation)
        .bind(&tids)
        .bind(reason)
        .bind(snapshot_before)
        .bind(user_id)
        .execute(&self.pool)
        .await;
    }

    // ── Branch state ──────────────────────────────────────────────────────────

    /// Returns the active table name for a user: "mem_memories" or branch table name.
    pub async fn active_table(&self, user_id: &str) -> Result<String, MemoriaError> {
        let row = sqlx::query(
            "SELECT active_branch FROM mem_user_state WHERE user_id = ?"
        )
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
        .bind(user_id).bind(branch).bind(now).bind(branch).bind(now)
        .execute(&self.pool).await.map_err(db_err)?;
        Ok(())
    }

    pub async fn register_branch(&self, user_id: &str, name: &str, table_name: &str) -> Result<(), MemoriaError> {
        let now = Utc::now().naive_utc();
        let id = uuid::Uuid::new_v4().simple().to_string();
        sqlx::query(
            r#"INSERT INTO mem_branches (id, user_id, name, table_name, status, created_at)
               VALUES (?, ?, ?, ?, 'active', ?)"#,
        )
        .bind(id).bind(user_id).bind(name).bind(table_name).bind(now)
        .execute(&self.pool).await.map_err(db_err)?;
        Ok(())
    }

    pub async fn deregister_branch(&self, user_id: &str, name: &str) -> Result<(), MemoriaError> {
        sqlx::query(
            "UPDATE mem_branches SET status = 'deleted' WHERE user_id = ? AND name = ?"
        )
        .bind(user_id).bind(name)
        .execute(&self.pool).await.map_err(db_err)?;
        Ok(())
    }

    pub async fn list_branches(&self, user_id: &str) -> Result<Vec<(String, String)>, MemoriaError> {
        let rows = sqlx::query(
            "SELECT name, table_name FROM mem_branches WHERE user_id = ? AND status = 'active'"
        )
        .bind(user_id).fetch_all(&self.pool).await.map_err(db_err)?;
        rows.iter().map(|r| Ok((
            r.try_get::<String, _>("name").map_err(db_err)?,
            r.try_get::<String, _>("table_name").map_err(db_err)?,
        ))).collect()
    }

    // ── Governance ────────────────────────────────────────────────────────────

    /// Check cooldown. Returns Some(remaining_seconds) if still in cooldown, None if can run.
    pub async fn check_cooldown(&self, user_id: &str, operation: &str, cooldown_secs: i64) -> Result<Option<i64>, MemoriaError> {
        let row = sqlx::query(
            "SELECT TIMESTAMPDIFF(SECOND, last_run_at, NOW()) as elapsed \
             FROM mem_governance_cooldown WHERE user_id = ? AND operation = ?"
        )
        .bind(user_id).bind(operation)
        .fetch_optional(&self.pool).await.map_err(db_err)?;
        match row {
            None => Ok(None),
            Some(r) => {
                let elapsed: i64 = r.try_get("elapsed").unwrap_or(cooldown_secs + 1);
                if elapsed >= cooldown_secs { Ok(None) }
                else { Ok(Some(cooldown_secs - elapsed)) }
            }
        }
    }

    pub async fn set_cooldown(&self, user_id: &str, operation: &str) -> Result<(), MemoriaError> {
        let now = Utc::now().naive_utc();
        sqlx::query(
            "INSERT INTO mem_governance_cooldown (user_id, operation, last_run_at) \
             VALUES (?, ?, ?) ON DUPLICATE KEY UPDATE last_run_at = ?"
        )
        .bind(user_id).bind(operation).bind(now).bind(now)
        .execute(&self.pool).await.map_err(db_err)?;
        Ok(())
    }

    /// Quarantine memories whose effective confidence has decayed below threshold.
    /// effective_confidence = initial_confidence * EXP(-age_days / half_life)
    pub async fn quarantine_low_confidence(&self, user_id: &str) -> Result<i64, MemoriaError> {
        // Half-lives per tier (days): T1=365, T2=180, T3=60, T4=30
        // Quarantine threshold: 0.2
        const THRESHOLD: f64 = 0.2;
        let tiers: &[(&str, f64)] = &[("T1", 365.0), ("T2", 180.0), ("T3", 60.0), ("T4", 30.0)];
        let mut total = 0i64;
        for (tier, hl) in tiers {
            let res = sqlx::query(&format!(
                "UPDATE mem_memories SET is_active = 0, updated_at = NOW() \
                 WHERE user_id = ? AND is_active = 1 AND trust_tier = ? \
                   AND (initial_confidence * EXP(-TIMESTAMPDIFF(DAY, observed_at, NOW()) / {hl})) < {THRESHOLD}"
            ))
            .bind(user_id).bind(tier)
            .execute(&self.pool).await.map_err(db_err)?;
            total += res.rows_affected() as i64;
        }
        Ok(total)
    }

    /// Delete inactive memories with very low initial_confidence (already superseded/stale).
    pub async fn cleanup_stale(&self, user_id: &str) -> Result<i64, MemoriaError> {
        let res = sqlx::query(
            "DELETE FROM mem_memories WHERE user_id = ? AND is_active = 0 AND initial_confidence < 0.1"
        )
        .bind(user_id).execute(&self.pool).await.map_err(db_err)?;
        Ok(res.rows_affected() as i64)
    }

    /// Delete expired tool_result memories (TTL = 72h by default).
    pub async fn cleanup_tool_results(&self, ttl_hours: i64) -> Result<i64, MemoriaError> {
        let mut total = 0i64;
        loop {
            let res = sqlx::query(
                "DELETE FROM mem_memories \
                 WHERE memory_type = 'tool_result' \
                   AND TIMESTAMPDIFF(HOUR, observed_at, NOW()) > ? \
                 LIMIT 5000"
            )
            .bind(ttl_hours).execute(&self.pool).await.map_err(db_err)?;
            let n = res.rows_affected() as i64;
            total += n;
            if n < 5000 { break; }
        }
        Ok(total)
    }

    /// Soft-delete working memories inactive for more than `stale_hours`.
    /// Returns per-user counts for audit logging.
    pub async fn archive_stale_working(&self, stale_hours: i64) -> Result<Vec<(String, i64)>, MemoriaError> {
        // Fetch affected user_ids before updating
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT user_id FROM mem_memories \
             WHERE memory_type = 'working' AND is_active = 1 \
               AND TIMESTAMPDIFF(HOUR, observed_at, NOW()) > ?"
        )
        .bind(stale_hours).fetch_all(&self.pool).await.map_err(db_err)?;

        if rows.is_empty() {
            return Ok(vec![]);
        }

        sqlx::query(
            "UPDATE mem_memories SET is_active = 0, updated_at = NOW() \
             WHERE memory_type = 'working' AND is_active = 1 \
               AND TIMESTAMPDIFF(HOUR, observed_at, NOW()) > ?"
        )
        .bind(stale_hours).execute(&self.pool).await.map_err(db_err)?;

        let mut by_user: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
        for (uid,) in rows {
            *by_user.entry(uid).or_insert(0) += 1;
        }
        Ok(by_user.into_iter().collect())
    }

    /// Deactivate near-duplicate memories (same user, same type, cosine sim > threshold).
    /// Uses L2² ≈ 2(1 - cos_sim) for normalized embeddings.
    /// Returns count of deactivated memories.
    pub async fn compress_redundant(&self, user_id: &str, similarity_threshold: f64, window_days: i64, max_pairs: usize) -> Result<i64, MemoriaError> {
        let rows: Vec<(String, String, chrono::NaiveDateTime, String)> = sqlx::query_as(
            "SELECT memory_id, memory_type, observed_at, embedding \
             FROM mem_memories \
             WHERE user_id = ? AND is_active = 1 AND embedding IS NOT NULL \
               AND TIMESTAMPDIFF(DAY, observed_at, NOW()) <= ? \
             ORDER BY memory_type, observed_at DESC"
        )
        .bind(user_id).bind(window_days)
        .fetch_all(&self.pool).await.map_err(db_err)?;

        if rows.len() < 2 { return Ok(0); }

        let l2_sq_threshold = 2.0 * (1.0 - similarity_threshold);

        // Group by memory_type with flat embedding storage for cache locality
        struct Entry { id: String, ts: chrono::NaiveDateTime, emb_offset: usize, dim: usize }
        let mut by_type: std::collections::HashMap<String, Vec<Entry>> = Default::default();
        let mut flat_embs: Vec<f32> = Vec::new();

        for (mid, mtype, ts, emb_str) in &rows {
            if let Ok(emb) = mo_to_vec(emb_str) {
                let offset = flat_embs.len();
                let dim = emb.len();
                flat_embs.extend_from_slice(&emb);
                by_type.entry(mtype.clone()).or_default().push(Entry { id: mid.clone(), ts: *ts, emb_offset: offset, dim });
            }
        }

        let mut to_deactivate: Vec<(String, String)> = vec![];
        let mut deactivated_ids: std::collections::HashSet<String> = Default::default();
        let mut pairs_checked = 0;

        'outer: for group in by_type.values() {
            if group.len() < 2 { continue; }
            for i in 0..group.len() {
                if deactivated_ids.contains(&group[i].id) { continue; }
                let emb_i = &flat_embs[group[i].emb_offset..group[i].emb_offset + group[i].dim];
                // Vectorized: compute L2² from i to all j > i
                for j in (i+1)..group.len() {
                    if pairs_checked >= max_pairs { break 'outer; }
                    if deactivated_ids.contains(&group[j].id) { continue; }
                    pairs_checked += 1;
                    let emb_j = &flat_embs[group[j].emb_offset..group[j].emb_offset + group[j].dim];
                    let dist_sq: f32 = emb_i.iter().zip(emb_j)
                        .map(|(a, b)| { let d = a - b; d * d }).sum();
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

        if to_deactivate.is_empty() { return Ok(0); }

        for (older, newer) in &to_deactivate {
            sqlx::query(
                "UPDATE mem_memories SET is_active = 0, superseded_by = ?, updated_at = NOW() WHERE memory_id = ?"
            )
            .bind(newer).bind(older)
            .execute(&self.pool).await.map_err(db_err)?;
        }

        Ok(to_deactivate.len() as i64)
    }

    /// Rebuild IVF vector index for a table. lists = max(1, rows/50), capped at 1024.
    pub async fn rebuild_vector_index(&self, table: &str) -> Result<i64, MemoriaError> {
        let row: (i64,) = sqlx::query_as(&format!(
            "SELECT COUNT(*) FROM {table} WHERE embedding IS NOT NULL"
        )).fetch_one(&self.pool).await.map_err(db_err)?;
        let total_rows = row.0;
        if total_rows == 0 { return Ok(0); }
        let idx_name = format!("{table}_embedding_ivf");
        // IVF hurts recall on small datasets; only build when rows >= 500
        if total_rows < 500 {
            let _ = sqlx::raw_sql(&format!("DROP INDEX {idx_name} ON {table}"))
                .execute(&self.pool).await;
            return Ok(total_rows);
        }
        let lists = (total_rows / 50).clamp(1, 1024);
        let _ = sqlx::raw_sql(&format!("DROP INDEX {idx_name} ON {table}"))
            .execute(&self.pool).await;
        sqlx::raw_sql(&format!(
            "CREATE INDEX {idx_name} USING ivfflat ON {table}(embedding) LISTS {lists} op_type 'vector_l2_ops'"
        ))
        .execute(&self.pool).await.map_err(db_err)?;
        Ok(total_rows)
    }

    /// Deactivate orphaned incremental session summaries (session never closed, >24h old).
    pub async fn cleanup_orphaned_incrementals(&self, user_id: &str, older_than_hours: i64) -> Result<i64, MemoriaError> {
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
               )"
        ).bind(user_id).bind(older_than_hours).bind(user_id)
         .fetch_all(&self.pool).await.map_err(db_err)?;

        if ids.is_empty() { return Ok(0); }
        for chunk in ids.chunks(500) {
            let placeholders = chunk.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            let sql = format!(
                "UPDATE mem_memories SET is_active = 0, updated_at = NOW() WHERE memory_id IN ({placeholders})"
            );
            let mut q = sqlx::query(&sql);
            for (id,) in chunk { q = q.bind(id); }
            q.execute(&self.pool).await.map_err(db_err)?;
        }
        Ok(ids.len() as i64)
    }

    /// Drop old milestone snapshots, keep last N (weekly).
    pub async fn cleanup_snapshots(&self, keep_last_n: usize) -> Result<i64, MemoriaError> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT sname FROM mo_catalog.mo_snapshots \
             WHERE prefix_eq(sname, 'mem_milestone_') ORDER BY ts DESC"
        ).fetch_all(&self.pool).await.map_err(db_err)?;

        if rows.len() <= keep_last_n { return Ok(0); }
        let mut dropped = 0i64;
        for (name,) in &rows[keep_last_n..] {
            let _ = sqlx::raw_sql(&format!("DROP SNAPSHOT {name}"))
                .execute(&self.pool).await;
            dropped += 1;
        }
        Ok(dropped)
    }

    /// Clean up sandbox branches that were not properly dropped (weekly).
    pub async fn cleanup_orphan_branches(&self) -> Result<i64, MemoriaError> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT table_name FROM information_schema.tables \
             WHERE table_name LIKE 'memories_sandbox_%'"
        ).fetch_all(&self.pool).await.map_err(db_err)?;

        let (db_name,): (String,) = sqlx::query_as("SELECT DATABASE()")
            .fetch_one(&self.pool).await.map_err(db_err)?;
        let mut cleaned = 0i64;
        for (table_name,) in rows {
            let _ = sqlx::raw_sql(&format!("DATA BRANCH DELETE TABLE {db_name}.{table_name}"))
                .execute(&self.pool).await;
            cleaned += 1;
        }
        Ok(cleaned)
    }

    /// Bump access_count for retrieved memories (fire-and-forget style).
    pub async fn bump_access_counts(&self, memory_ids: &[String]) -> Result<(), MemoriaError> {
        if memory_ids.is_empty() { return Ok(()); }
        for chunk in memory_ids.chunks(100) {
            let placeholders: Vec<&str> = chunk.iter().map(|_| "(?, 1, NOW())").collect();
            let sql = format!(
                "INSERT INTO mem_memories_stats (memory_id, access_count, last_accessed_at) VALUES {} \
                 ON DUPLICATE KEY UPDATE access_count = access_count + 1, last_accessed_at = NOW()",
                placeholders.join(", ")
            );
            let mut q = sqlx::query(&sql);
            for id in chunk { q = q.bind(id); }
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
             WHERE m.user_id = ?"
        ).bind(user_id).execute(&self.pool).await.map_err(db_err)?;
        Ok(result.rows_affected() as i64)
    }

    /// Get access_count for a set of memory IDs.
    pub async fn get_access_counts(&self, memory_ids: &[String]) -> Result<std::collections::HashMap<String, i32>, MemoriaError> {
        let mut map = std::collections::HashMap::new();
        if memory_ids.is_empty() { return Ok(map); }
        for chunk in memory_ids.chunks(500) {
            let placeholders: Vec<&str> = chunk.iter().map(|_| "?").collect();
            let sql = format!(
                "SELECT memory_id, access_count FROM mem_memories_stats WHERE memory_id IN ({})",
                placeholders.join(", ")
            );
            let mut q = sqlx::query(&sql);
            for id in chunk { q = q.bind(id); }
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
    pub async fn detect_pollution(&self, user_id: &str, since_hours: i64) -> Result<bool, MemoriaError> {
        let row: (i64, i64) = sqlx::query_as(
            "SELECT COUNT(*) as total_changes, \
             SUM(CASE WHEN superseded_by IS NOT NULL THEN 1 ELSE 0 END) as supersedes \
             FROM mem_memories \
             WHERE user_id = ? AND updated_at >= DATE_SUB(NOW(), INTERVAL ? HOUR)"
        ).bind(user_id).bind(since_hours)
         .fetch_one(&self.pool).await.map_err(db_err)?;
        let (total, supersedes) = row;
        if total == 0 { return Ok(false); }
        Ok(supersedes as f64 / total as f64 > 0.3)
    }

    /// Per-type stats: count, avg_confidence, contradiction_rate, avg_staleness_hours.
    pub async fn health_analyze(&self, user_id: &str) -> Result<serde_json::Value, MemoriaError> {
        let rows: Vec<(String, i64, f64, i64, f64)> = sqlx::query_as(
            "SELECT memory_type, COUNT(*) as total, AVG(initial_confidence) as avg_conf, \
             COUNT(CASE WHEN superseded_by IS NOT NULL THEN 1 END) as superseded, \
             AVG(TIMESTAMPDIFF(HOUR, observed_at, NOW())) as avg_stale_h \
             FROM mem_memories WHERE user_id = ? GROUP BY memory_type"
        ).bind(user_id).fetch_all(&self.pool).await.map_err(db_err)?;

        let mut stats = serde_json::Map::new();
        for (mtype, total, avg_conf, superseded, avg_stale) in rows {
            let contradiction_rate = if total > 0 { superseded as f64 / total as f64 } else { 0.0 };
            stats.insert(mtype, serde_json::json!({
                "total": total,
                "avg_confidence": avg_conf,
                "contradiction_rate": contradiction_rate,
                "avg_staleness_hours": avg_stale,
            }));
        }
        Ok(serde_json::Value::Object(stats))
    }

    /// Storage stats: total, active, inactive, avg_content_size, oldest, newest.
    pub async fn health_storage_stats(&self, user_id: &str) -> Result<serde_json::Value, MemoriaError> {
        let row: (i64, i64, f64) = sqlx::query_as(
            "SELECT COUNT(*) as total, \
             SUM(CASE WHEN is_active = 1 THEN 1 ELSE 0 END) as active, \
             AVG(LENGTH(content)) as avg_content_size \
             FROM mem_memories WHERE user_id = ?"
        ).bind(user_id).fetch_one(&self.pool).await.map_err(db_err)?;

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

        let (user_active,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM mem_memories WHERE user_id = ? AND is_active = 1"
        ).bind(user_id).fetch_one(&self.pool).await.map_err(db_err)?;

        let (global_total,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM mem_memories WHERE is_active = 1"
        ).fetch_one(&self.pool).await.map_err(db_err)?;

        let (added_30d,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM mem_memories WHERE user_id = ? AND observed_at >= NOW() - INTERVAL 30 DAY"
        ).bind(user_id).fetch_one(&self.pool).await.map_err(db_err)?;

        let recommendation = if global_total > IVF_DEGRADED { "partition_required" }
            else if global_total > IVF_OPTIMAL { "monitor_query_latency" }
            else { "ok" };

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
    pub async fn get_by_ids(&self, ids: &[String]) -> Result<std::collections::HashMap<String, Memory>, MemoriaError> {
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
        &self, table: &str, user_id: &str, embedding: &[f32],
        memory_type: &str, exclude_id: &str, l2_threshold: f64,
    ) -> Result<Option<(String, String, f64)>, MemoriaError> {
        // Same-type first, then cross-type fallback
        if let Some(hit) = self.find_near_duplicate_inner(table, user_id, embedding, Some(memory_type), exclude_id, l2_threshold).await? {
            return Ok(Some(hit));
        }
        self.find_near_duplicate_inner(table, user_id, embedding, None, exclude_id, l2_threshold).await
    }

    async fn find_near_duplicate_inner(
        &self, table: &str, user_id: &str, embedding: &[f32],
        memory_type: Option<&str>, exclude_id: &str, l2_threshold: f64,
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
             ORDER BY l2_dist ASC LIMIT 1"
        );
        let mut q = sqlx::query(&sql).bind(user_id);
        if let Some(mt) = memory_type { q = q.bind(mt); }
        let row = q.bind(exclude_id).fetch_optional(&self.pool).await.map_err(db_err)?;
        if let Some(r) = row {
            let dist: f64 = r.try_get::<f64, _>("l2_dist")
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
    pub async fn supersede_memory(&self, table: &str, old_id: &str, new_id: &str) -> Result<(), MemoriaError> {
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
        let extra_metadata = memory.extra_metadata.as_ref()
            .map(serde_json::to_string).transpose()?
            .unwrap_or_else(|| "{}".to_string());
        let embedding = memory.embedding.as_deref()
            .filter(|v| !v.is_empty())  // Some([]) → None → SQL NULL
            .map(vec_to_mo);

        sqlx::query(&format!(
            r#"INSERT INTO {table}
               (memory_id, user_id, memory_type, content, embedding, session_id,
                source_event_ids, extra_metadata, is_active, superseded_by,
                trust_tier, initial_confidence, observed_at, created_at, updated_at)
               VALUES (?, ?, ?, ?, ?, ?, ?, ?, 1, ?, ?, ?, ?, ?, ?)"#
        ))
        .bind(&memory.memory_id).bind(&memory.user_id)
        .bind(memory.memory_type.to_string()).bind(&memory.content)
        .bind(embedding).bind(&memory.session_id)
        .bind(source_event_ids).bind(extra_metadata)
        .bind(&memory.superseded_by).bind(memory.trust_tier.to_string())
        .bind(memory.initial_confidence as f32)
        .bind(observed_at).bind(created_at).bind(now)
        .execute(&self.pool).await.map_err(db_err)?;
        Ok(())
    }

    pub async fn list_active_from(&self, table: &str, user_id: &str, limit: i64) -> Result<Vec<Memory>, MemoriaError> {
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
        .bind(user_id).bind(limit)
        .fetch_all(&self.pool).await.map_err(db_err)?;
        rows.iter().map(row_to_memory).collect()
    }

    /// Find memory IDs whose content contains `topic` (exact substring match).
    /// Uses fulltext boolean MUST first, falls back to LIKE.
    pub async fn find_ids_by_topic(&self, table: &str, user_id: &str, topic: &str) -> Result<Vec<String>, MemoriaError> {
        let ft_safe = sanitize_fulltext_query(topic);
        let like_safe = sanitize_like_pattern(topic);
        // Try fulltext boolean MUST (+word) first
        if !ft_safe.is_empty() {
            let ft_terms: String = ft_safe.split_whitespace().map(|w| format!("+{w}")).collect::<Vec<_>>().join(" ");
            let sql = format!(
                "SELECT memory_id FROM {table} \
                 WHERE user_id = ? AND is_active = 1 \
                   AND MATCH(content) AGAINST('{ft_terms}' IN BOOLEAN MODE) \
                   AND content LIKE ?"
            );
            let like_pat = format!("%{like_safe}%");
            let rows: Vec<(String,)> = sqlx::query_as(&sql)
                .bind(user_id).bind(&like_pat)
                .fetch_all(&self.pool).await
                .unwrap_or_default();
            if !rows.is_empty() {
                return Ok(rows.into_iter().map(|r| r.0).collect());
            }
        }
        // Fallback: LIKE only (handles short tokens that fulltext ignores)
        let like_pat = format!("%{like_safe}%");
        let sql2 = format!(
            "SELECT memory_id FROM {table} \
             WHERE user_id = ? AND is_active = 1 AND content LIKE ?"
        );
        let rows2: Vec<(String,)> = sqlx::query_as(&sql2)
            .bind(user_id).bind(&like_pat)
            .fetch_all(&self.pool).await.map_err(db_err)?;
        Ok(rows2.into_iter().map(|r| r.0).collect())
    }

    #[tracing::instrument(skip(self))]
    pub async fn search_fulltext_from(&self, table: &str, user_id: &str, query: &str, limit: i64) -> Result<Vec<Memory>, MemoriaError> {
        let safe = sanitize_fulltext_query(query);
        if safe.is_empty() { return Ok(vec![]); }
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
        let rows = sqlx::query(&sql).bind(user_id).bind(limit)
            .fetch_all(&self.pool).await.map_err(db_err)?;
        rows.iter().map(|r| {
            let mut m = row_to_memory(r)?;
            if let Ok(ft) = r.try_get::<f64, _>("ft_score") {
                m.retrieval_score = Some(ft);
            } else if let Ok(ft) = r.try_get::<f32, _>("ft_score") {
                m.retrieval_score = Some(ft as f64);
            }
            Ok(m)
        }).collect()
    }

    pub async fn search_vector_from(&self, table: &str, user_id: &str, embedding: &[f32], limit: i64) -> Result<Vec<Memory>, MemoriaError> {
        self.search_vector_from_filtered(table, user_id, embedding, limit, None).await
    }

    /// Vector search with optional memory_type pre-filter to reduce scan set.
    pub async fn search_vector_from_filtered(&self, table: &str, user_id: &str, embedding: &[f32], limit: i64, memory_type: Option<&str>) -> Result<Vec<Memory>, MemoriaError> {
        let vec_literal = vec_to_mo(embedding);
        let type_clause = match memory_type {
            Some(mt) => format!(" AND memory_type = '{}'", sanitize_sql_literal(mt)),
            None => String::new(),
        };
        let sql = format!(
            "SELECT memory_id, user_id, memory_type, content, \
             embedding AS emb_str, session_id, \
             CAST(source_event_ids AS CHAR) AS src_ids, \
             CAST(extra_metadata AS CHAR) AS extra_meta, \
             is_active, superseded_by, trust_tier, initial_confidence, \
             observed_at, created_at, updated_at, \
             l2_distance(embedding, '{vec_literal}') AS l2_dist \
             FROM {table} \
             WHERE user_id = ? AND is_active = 1 AND embedding IS NOT NULL{type_clause} \
             ORDER BY l2_dist ASC \
             LIMIT ?"
        );
        let rows = sqlx::query(&sql).bind(user_id).bind(limit)
            .fetch_all(&self.pool).await.map_err(db_err)?;
        rows.iter().map(|r| {
            let mut m = row_to_memory(r)?;
            if let Ok(dist) = r.try_get::<f64, _>("l2_dist") {
                m.retrieval_score = Some(1.0 / (1.0 + dist));
            }
            Ok(m)
        }).collect()
    }

    /// Hybrid search: vector + fulltext, merged with 4-dimension weighted scoring.
    /// Weights: vector=0.3, keyword=0.2, temporal=0.2, confidence=0.3 (matches Python "default")
    #[tracing::instrument(skip(self, embedding))]
    pub async fn search_hybrid_from(&self, table: &str, user_id: &str, embedding: &[f32], query: &str, limit: i64) -> Result<Vec<Memory>, MemoriaError> {
        let (mems, _) = self.search_hybrid_from_scored(table, user_id, embedding, query, limit).await?;
        Ok(mems)
    }

    /// Like search_hybrid_from but also returns per-candidate score breakdown.
    /// scores: (memory_id, vec_score, kw_score, time_score, conf_score, final_score)
    pub async fn search_hybrid_from_scored(
        &self, table: &str, user_id: &str, embedding: &[f32], query: &str, limit: i64,
    ) -> Result<(Vec<Memory>, Vec<(String, f64, f64, f64, f64, f64)>), MemoriaError> {
        let fetch_k = (limit * 3).max(20);
        let vec_results = self.search_vector_from(table, user_id, embedding, fetch_k).await?;
        let ft_results = self.search_fulltext_from(table, user_id, query, fetch_k).await.unwrap_or_default();

        let ft_map: std::collections::HashMap<String, f64> = ft_results.iter()
            .filter_map(|m| m.retrieval_score.map(|s| (m.memory_id.clone(), s)))
            .collect();

        let mut seen: std::collections::HashSet<String> = vec_results.iter().map(|m| m.memory_id.clone()).collect();
        let mut candidates = vec_results;
        for m in ft_results {
            if seen.insert(m.memory_id.clone()) { candidates.push(m); }
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

        for m in &mut candidates {
            let vec_score = m.retrieval_score.unwrap_or(0.0);
            let raw_ft = ft_map.get(&m.memory_id).copied().unwrap_or(0.0);
            let kw_score = if raw_ft > 0.0 { raw_ft / (raw_ft + 1.0) } else { 0.0 };
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
            let mut final_score = W_VEC * vec_score + W_KW * kw_score + W_TIME * time_score + W_CONF * conf_score;
            // Frequency boost: log(1 + access_count) — mild boost for frequently retrieved memories
            let ac = ac_map.get(&m.memory_id).copied().unwrap_or(0);
            if ac > 0 {
                final_score *= 1.0 + 0.1 * ((1 + ac) as f64).ln();
            }
            m.access_count = ac;
            m.retrieval_score = Some(final_score);
            score_breakdown.push((m.memory_id.clone(), vec_score, kw_score, time_score, conf_score, final_score));
        }

        // Drop memories whose effective confidence has decayed to near-zero.
        // This prevents long-expired facts from appearing in results.
        const MIN_CONF: f64 = 0.05;
        let live: std::collections::HashSet<String> = score_breakdown.iter()
            .filter(|(_, _, _, _, cs, _)| *cs >= MIN_CONF)
            .map(|(id, ..)| id.clone()).collect();
        candidates.retain(|m| live.contains(&m.memory_id));
        score_breakdown.retain(|(id, ..)| live.contains(id));

        candidates.sort_by(|a, b| b.retrieval_score.partial_cmp(&a.retrieval_score).unwrap_or(std::cmp::Ordering::Equal));
        // Re-sort score_breakdown to match candidate order
        let order: std::collections::HashMap<String, usize> = candidates.iter().enumerate()
            .map(|(i, m)| (m.memory_id.clone(), i)).collect();
        score_breakdown.sort_by_key(|(id, ..)| order.get(id).copied().unwrap_or(usize::MAX));
        candidates.truncate(limit as usize);
        score_breakdown.truncate(limit as usize);
        Ok((candidates, score_breakdown))
    }

    // ── Entity links ──────────────────────────────────────────────────────────

    /// Returns memory_ids that already have entity links for a user.
    pub async fn get_linked_memory_ids(&self, user_id: &str) -> Result<std::collections::HashSet<String>, MemoriaError> {
        let rows = sqlx::query(
            "SELECT DISTINCT memory_id FROM mem_entity_links WHERE user_id = ?"
        )
        .bind(user_id)
        .fetch_all(&self.pool).await.map_err(db_err)?;
        Ok(rows.iter().filter_map(|r| r.try_get::<String, _>("memory_id").ok()).collect())
    }

    /// Returns all entity names for a user (for existing_entities list).
    pub async fn get_entity_names(&self, user_id: &str) -> Result<Vec<(String, String)>, MemoriaError> {
        let rows = sqlx::query(
            "SELECT DISTINCT entity_name, entity_type FROM mem_entity_links WHERE user_id = ? ORDER BY entity_name"
        )
        .bind(user_id)
        .fetch_all(&self.pool).await.map_err(db_err)?;
        Ok(rows.iter().filter_map(|r| {
            let name = r.try_get::<String, _>("entity_name").ok()?;
            let etype = r.try_get::<String, _>("entity_type").ok()?;
            Some((name, etype))
        }).collect())
    }

    /// Insert entity links for a memory. Skips duplicates.
    pub async fn insert_entity_links(
        &self,
        user_id: &str,
        memory_id: &str,
        entities: &[(String, String)], // (name, type)
    ) -> Result<(usize, usize), MemoriaError> { // (created, reused)
        let existing: std::collections::HashSet<String> = {
            let rows = sqlx::query(
                "SELECT entity_name FROM mem_entity_links WHERE user_id = ? AND memory_id = ?"
            )
            .bind(user_id).bind(memory_id)
            .fetch_all(&self.pool).await.map_err(db_err)?;
            rows.iter().filter_map(|r| r.try_get::<String, _>("entity_name").ok()).collect()
        };
        let now = chrono::Utc::now().naive_utc();
        let mut created = 0usize;
        let mut reused = 0usize;
        for (name, etype) in entities {
            let name_lc = name.to_lowercase();
            if existing.contains(&name_lc) {
                reused += 1;
                continue;
            }
            let id = uuid::Uuid::new_v4().to_string().replace('-', "");
            sqlx::query(
                "INSERT INTO mem_entity_links (id, user_id, memory_id, entity_name, entity_type, source, created_at) \
                 VALUES (?, ?, ?, ?, ?, 'manual', ?)"
            )
            .bind(&id).bind(user_id).bind(memory_id).bind(&name_lc).bind(etype).bind(now)
            .execute(&self.pool).await.map_err(db_err)?;
            created += 1;
        }
        Ok((created, reused))
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
        .bind(memory_id).fetch_optional(&self.pool).await.map_err(db_err)?;
        row.map(|r| row_to_memory(&r)).transpose()
    }

    async fn update(&self, memory: &Memory) -> Result<(), MemoriaError> {
        let now = Utc::now().naive_utc();
        // Workaround: MO#23859 — PREPARE/EXECUTE corrupts NULL JSON on 2nd+ execution.
        let extra_metadata = memory.extra_metadata.as_ref()
            .map(serde_json::to_string).transpose()?
            .unwrap_or_else(|| "{}".to_string());
        sqlx::query(
            r#"UPDATE mem_memories
               SET content = ?, memory_type = ?, trust_tier = ?,
                   initial_confidence = ?, extra_metadata = ?,
                   superseded_by = ?, updated_at = ?
               WHERE memory_id = ?"#,
        )
        .bind(&memory.content).bind(memory.memory_type.to_string())
        .bind(memory.trust_tier.to_string()).bind(memory.initial_confidence as f32)
        .bind(extra_metadata).bind(&memory.superseded_by).bind(now)
        .bind(&memory.memory_id)
        .execute(&self.pool).await.map_err(db_err)?;
        Ok(())
    }

    async fn soft_delete(&self, memory_id: &str) -> Result<(), MemoriaError> {
        let now = Utc::now().naive_utc();
        sqlx::query(
            "UPDATE mem_memories SET is_active = 0, updated_at = ? WHERE memory_id = ?"
        )
        .bind(now).bind(memory_id)
        .execute(&self.pool).await.map_err(db_err)?;
        Ok(())
    }

    async fn list_active(&self, user_id: &str, limit: i64) -> Result<Vec<Memory>, MemoriaError> {
        self.list_active_from("mem_memories", user_id, limit).await
    }

    async fn search_fulltext(&self, user_id: &str, query: &str, limit: i64) -> Result<Vec<Memory>, MemoriaError> {
        self.search_fulltext_from("mem_memories", user_id, query, limit).await
    }

    async fn search_vector(&self, user_id: &str, embedding: &[f32], limit: i64) -> Result<Vec<Memory>, MemoriaError> {
        self.search_vector_from("mem_memories", user_id, embedding, limit).await
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
        s.filter(|v| v != "{}").map(|v| serde_json::from_str(&v)).transpose()?
    };
    let embedding: Option<Vec<f32>> = {
        let s: Option<String> = row.try_get("emb_str").map_err(db_err)?;
        s.map(|v| mo_to_vec(&v)).transpose()?
    };
    let observed_at = row.try_get::<chrono::NaiveDateTime, _>("observed_at").ok().map(|dt| dt.and_utc());
    let created_at = row.try_get::<chrono::NaiveDateTime, _>("created_at").ok().map(|dt| dt.and_utc());
    let updated_at = row.try_get::<chrono::NaiveDateTime, _>("updated_at").ok().map(|dt| dt.and_utc());

    Ok(Memory {
        memory_id: row.try_get("memory_id").map_err(db_err)?,
        user_id: row.try_get("user_id").map_err(db_err)?,
        memory_type: MemoryType::from_str(&memory_type_str)?,
        content: row.try_get("content").map_err(db_err)?,
        initial_confidence: row.try_get::<f32, _>("initial_confidence").map_err(db_err)? as f64,
        embedding,
        source_event_ids,
        superseded_by: row.try_get("superseded_by").map_err(db_err)?,
        is_active: { let v: i8 = row.try_get("is_active").map_err(db_err)?; v != 0 },
        access_count: 0,
        session_id: row.try_get("session_id").map_err(db_err)?,
        observed_at, created_at, updated_at, extra_metadata,
        trust_tier: TrustTier::from_str(&trust_tier_str)?,
        retrieval_score: None,
    })
}
