use async_trait::async_trait;
use chrono::Utc;
use memoria_core::{interfaces::MemoryStore, Memory, MemoriaError, MemoryType, TrustTier};
use sqlx::{mysql::MySqlPool, Row};
use std::str::FromStr;

fn db_err(e: sqlx::Error) -> MemoriaError {
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

pub struct SqlMemoryStore {
    pool: MySqlPool,
    embedding_dim: usize,
}

impl SqlMemoryStore {
    pub fn new(pool: MySqlPool, embedding_dim: usize) -> Self {
        Self { pool, embedding_dim }
    }

    pub fn pool(&self) -> &MySqlPool { &self.pool }

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
                extra_metadata  JSON,
                is_active       TINYINT(1)   NOT NULL DEFAULT 1,
                superseded_by   VARCHAR(64),
                trust_tier      VARCHAR(10)  DEFAULT 'T3',
                initial_confidence FLOAT     DEFAULT 0.75,
                observed_at     DATETIME(6)  NOT NULL,
                created_at      DATETIME(6)  NOT NULL,
                updated_at      DATETIME(6),
                INDEX idx_user_active (user_id, is_active),
                INDEX idx_user_session (user_id, session_id),
                FULLTEXT INDEX ft_content (content) WITH PARSER ngram
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

    // ── Table-aware CRUD ──────────────────────────────────────────────────────

    pub async fn insert_into(&self, table: &str, memory: &Memory) -> Result<(), MemoriaError> {
        let now = Utc::now().naive_utc();
        let observed_at = memory.observed_at.map(|dt| dt.naive_utc()).unwrap_or(now);
        let created_at = memory.created_at.map(|dt| dt.naive_utc()).unwrap_or(now);
        let source_event_ids = serde_json::to_string(&memory.source_event_ids)?;
        let extra_metadata = memory.extra_metadata.as_ref().map(serde_json::to_string).transpose()?;
        let embedding = memory.embedding.as_deref().map(vec_to_mo);

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
        let extra_metadata = memory.extra_metadata.as_ref().map(serde_json::to_string).transpose()?;
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
        s.map(|v| serde_json::from_str(&v)).transpose()?
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
