use async_trait::async_trait;
use chrono::Utc;
use memoria_core::{interfaces::MemoryStore, Memory, MemoriaError, MemoryType, TrustTier};
use sqlx::{mysql::MySqlPool, Row};
use std::str::FromStr;

pub(crate) fn db_err(e: sqlx::Error) -> MemoriaError {
    MemoriaError::Database(e.to_string())
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
        let pool = MySqlPool::connect(database_url).await.map_err(db_err)?;
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
                trust_tier      VARCHAR(10)  DEFAULT 'T3',
                initial_confidence FLOAT     DEFAULT 0.75,
                observed_at     DATETIME(6)  NOT NULL,
                created_at      DATETIME(6)  NOT NULL,
                updated_at      DATETIME(6),
                INDEX idx_user_active (user_id, is_active),
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

        // Graph tables
        self.graph_store().migrate().await?;

        Ok(())
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
        let ph = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
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
        for id in ids {
            q = q.bind(id);
        }
        let rows = q.fetch_all(&self.pool).await.map_err(db_err)?;
        let mut map = std::collections::HashMap::new();
        for r in &rows {
            let m = row_to_memory(r)?;
            map.insert(m.memory_id.clone(), m);
        }
        Ok(map)
    }

    // ── Table-aware CRUD ──────────────────────────────────────────────────────

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

    pub async fn search_fulltext_from(&self, table: &str, user_id: &str, query: &str, limit: i64) -> Result<Vec<Memory>, MemoriaError> {
        let safe = query.replace('\'', "").replace('\\', "");
        let sql = format!(
            "SELECT memory_id, user_id, memory_type, content, \
             embedding AS emb_str, session_id, \
             CAST(source_event_ids AS CHAR) AS src_ids, \
             CAST(extra_metadata AS CHAR) AS extra_meta, \
             is_active, superseded_by, trust_tier, initial_confidence, \
             observed_at, created_at, updated_at, \
             MATCH(content) AGAINST('+{safe}' IN BOOLEAN MODE) AS ft_score \
             FROM {table} \
             WHERE user_id = ? AND is_active = 1 \
               AND MATCH(content) AGAINST('+{safe}' IN BOOLEAN MODE) \
             ORDER BY ft_score DESC LIMIT ?"
        );
        let rows = sqlx::query(&sql).bind(user_id).bind(limit)
            .fetch_all(&self.pool).await.map_err(db_err)?;
        rows.iter().map(row_to_memory).collect()
    }

    pub async fn search_vector_from(&self, table: &str, user_id: &str, embedding: &[f32], limit: i64) -> Result<Vec<Memory>, MemoriaError> {
        let vec_literal = vec_to_mo(embedding);
        let sql = format!(
            "SELECT memory_id, user_id, memory_type, content, \
             embedding AS emb_str, session_id, \
             CAST(source_event_ids AS CHAR) AS src_ids, \
             CAST(extra_metadata AS CHAR) AS extra_meta, \
             is_active, superseded_by, trust_tier, initial_confidence, \
             observed_at, created_at, updated_at \
             FROM {table} \
             WHERE user_id = ? AND is_active = 1 AND embedding IS NOT NULL \
             ORDER BY l2_distance(embedding, '{vec_literal}') ASC \
             LIMIT ?"
        );
        let rows = sqlx::query(&sql).bind(user_id).bind(limit)
            .fetch_all(&self.pool).await.map_err(db_err)?;
        rows.iter().map(row_to_memory).collect()
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
