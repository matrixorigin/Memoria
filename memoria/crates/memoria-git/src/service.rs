use chrono::NaiveDateTime;
use memoria_core::MemoriaError;
use serde::{Deserialize, Serialize};
use sqlx::{mysql::MySqlPool, Row};

fn db_err(e: sqlx::Error) -> MemoriaError {
    MemoriaError::Database(e.to_string())
}

/// Execute a DDL statement without prepared statement protocol.
/// MatrixOne does not support PREPARE for DDL (CREATE SNAPSHOT, data branch, etc.)
async fn exec_ddl(pool: &MySqlPool, sql: &str) -> Result<(), MemoriaError> {
    sqlx::raw_sql(sql).execute(pool).await.map_err(db_err)?;
    Ok(())
}

/// Validate identifier — alphanumeric + underscore only, prevents SQL injection in DDL.
fn validate_identifier(name: &str) -> Result<&str, MemoriaError> {
    if name.chars().all(|c| c.is_alphanumeric() || c == '_') && !name.is_empty() {
        Ok(name)
    } else {
        Err(MemoriaError::Internal(format!(
            "Invalid identifier: {name:?} — only alphanumeric and underscore allowed"
        )))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub snapshot_name: String,
    pub timestamp: Option<NaiveDateTime>,
    pub snapshot_level: String,
    pub account_name: String,
    pub database_name: Option<String>,
    pub table_name: Option<String>,
}

#[derive(Debug, Clone)]
pub struct DiffRow {
    pub flag: String, // INSERT | UPDATE | DELETE
    pub memory_id: String,
    pub content: String,
    pub memory_type: String,
}

pub struct GitForDataService {
    pool: MySqlPool,
    db_name: String,
}

impl GitForDataService {
    pub fn new(pool: MySqlPool, db_name: impl Into<String>) -> Self {
        Self {
            pool,
            db_name: db_name.into(),
        }
    }

    pub fn pool(&self) -> &MySqlPool {
        &self.pool
    }

    // ── Snapshots ─────────────────────────────────────────────────────────────

    pub async fn create_snapshot(&self, name: &str) -> Result<Snapshot, MemoriaError> {
        let safe = validate_identifier(name)?;
        exec_ddl(
            &self.pool,
            &format!("CREATE SNAPSHOT {safe} FOR ACCOUNT sys"),
        )
        .await?;
        self.get_snapshot(name).await?.ok_or_else(|| {
            MemoriaError::Internal(format!("Snapshot {name} not found after creation"))
        })
    }

    pub async fn list_snapshots(&self) -> Result<Vec<Snapshot>, MemoriaError> {
        let rows = sqlx::query("SHOW SNAPSHOTS")
            .fetch_all(&self.pool)
            .await
            .map_err(db_err)?;
        rows.iter()
            .map(|r| {
                // Try NaiveDateTime directly first, then fall back to string parsing
                let timestamp = r.try_get::<NaiveDateTime, _>("TIMESTAMP").ok().or_else(|| {
                    r.try_get::<String, _>("TIMESTAMP").ok().and_then(|s| {
                        NaiveDateTime::parse_from_str(&s, "%Y-%m-%d %H:%M:%S%.f")
                            .ok()
                            .or_else(|| NaiveDateTime::parse_from_str(&s, "%Y-%m-%d %H:%M:%S").ok())
                    })
                });
                Ok(Snapshot {
                    snapshot_name: r.try_get("SNAPSHOT_NAME").map_err(db_err)?,
                    timestamp,
                    snapshot_level: r.try_get("SNAPSHOT_LEVEL").map_err(db_err)?,
                    account_name: r.try_get("ACCOUNT_NAME").map_err(db_err)?,
                    database_name: r.try_get("DATABASE_NAME").ok(),
                    table_name: r.try_get("TABLE_NAME").ok(),
                })
            })
            .collect()
    }

    pub async fn get_snapshot(&self, name: &str) -> Result<Option<Snapshot>, MemoriaError> {
        let snaps = self.list_snapshots().await?;
        Ok(snaps.into_iter().find(|s| s.snapshot_name == name))
    }

    pub async fn drop_snapshot(&self, name: &str) -> Result<(), MemoriaError> {
        let safe = validate_identifier(name)?;
        exec_ddl(&self.pool, &format!("DROP SNAPSHOT {safe}")).await
    }

    /// Restore a single table from snapshot (non-destructive alternative to full account restore).
    /// DELETE current rows + INSERT SELECT from snapshot.
    /// Workaround for MO#23860: retry on w-w conflict.
    pub async fn restore_table_from_snapshot(
        &self,
        table: &str,
        snapshot_name: &str,
    ) -> Result<(), MemoriaError> {
        let safe_table = validate_identifier(table)?;
        let safe_snap = validate_identifier(snapshot_name)?;

        // Verify snapshot exists
        self.get_snapshot(snapshot_name)
            .await?
            .ok_or_else(|| MemoriaError::NotFound(format!("Snapshot {snapshot_name}")))?;

        // MO#23860: concurrent snapshot restore causes w-w conflict
        // MO#23861: concurrent snapshot restore loses FULLTEXT INDEX secondary tables
        // Callers must serialize snapshot operations until these are fixed.
        //
        // Note: ideally this would be transactional, but MatrixOne does not
        // support {SNAPSHOT = '...'} syntax inside transactions. The DELETE+INSERT
        // is non-atomic; callers should create a safety snapshot before rollback.
        exec_ddl(&self.pool, &format!("DELETE FROM {safe_table}")).await?;
        exec_ddl(
            &self.pool,
            &format!(
                "INSERT INTO {safe_table} SELECT * FROM {safe_table} {{SNAPSHOT = '{safe_snap}'}}"
            ),
        )
        .await?;

        Ok(())
    }

    // ── Branches ──────────────────────────────────────────────────────────────

    /// Zero-copy branch of a table. branch_name must be internally generated (UUID hex).
    pub async fn create_branch(
        &self,
        branch_name: &str,
        source_table: &str,
    ) -> Result<(), MemoriaError> {
        let safe_branch = validate_identifier(branch_name)?;
        let safe_source = validate_identifier(source_table)?;
        exec_ddl(
            &self.pool,
            &format!("data branch create table {safe_branch} from {safe_source}"),
        )
        .await
    }

    /// Create a branch from a snapshot: branch table contains the snapshot's data.
    /// MatrixOne syntax: data branch create table <branch> from <source> {snapshot = '<snap>'}
    pub async fn create_branch_from_snapshot(
        &self,
        branch_name: &str,
        source_table: &str,
        snapshot_name: &str,
    ) -> Result<(), MemoriaError> {
        let safe_branch = validate_identifier(branch_name)?;
        let safe_source = validate_identifier(source_table)?;
        let safe_snap = validate_identifier(snapshot_name)?;
        exec_ddl(
            &self.pool,
            &format!(
            "data branch create table {safe_branch} from {safe_source} {{snapshot = '{safe_snap}'}}"
        ),
        )
        .await
    }

    pub async fn drop_branch(&self, branch_name: &str) -> Result<(), MemoriaError> {
        let safe = validate_identifier(branch_name)?;
        let db = &self.db_name;
        exec_ddl(&self.pool, &format!("data branch delete table {db}.{safe}")).await
    }

    /// Native branch merge: `data branch merge {branch} into {main} when conflict skip`.
    /// Inserts rows from branch that don't exist in main (by PK). Account-level but
    /// user_id isolation is natural — branch only contains the user's new rows.
    pub async fn merge_branch(
        &self,
        branch_table: &str,
        main_table: &str,
    ) -> Result<(), MemoriaError> {
        let safe_branch = validate_identifier(branch_table)?;
        let safe_main = validate_identifier(main_table)?;
        exec_ddl(
            &self.pool,
            &format!("data branch merge {safe_branch} into {safe_main} when conflict skip"),
        )
        .await
    }

    /// LCA-based diff count for a specific user.
    /// Native `output count` is account-level, so we fetch rows and count in Rust.
    pub async fn diff_branch_count(
        &self,
        branch_table: &str,
        main_table: &str,
        user_id: &str,
    ) -> Result<i64, MemoriaError> {
        // Fetch a large batch and count user's rows
        // For safety limit purposes, 5000 is the max we care about
        let rows = self
            .diff_branch_rows(branch_table, main_table, user_id, 5001)
            .await?;
        Ok(rows.len() as i64)
    }

    /// Native LCA-based diff rows, filtered by user_id.
    ///
    /// `data branch diff` is account-level (no WHERE clause supported), so we fetch
    /// all rows and filter in Rust. This avoids exposing other users' memories.
    pub async fn diff_branch_rows(
        &self,
        branch_table: &str,
        main_table: &str,
        user_id: &str,
        limit: i64,
    ) -> Result<Vec<DiffRow>, MemoriaError> {
        let safe_branch = validate_identifier(branch_table)?;
        let safe_main = validate_identifier(main_table)?;
        // Fetch more than limit to account for filtering
        let fetch_limit = limit * 10 + 100;
        let sql = format!(
            "data branch diff {safe_branch} against {safe_main} output limit {fetch_limit}"
        );
        let rows = sqlx::raw_sql(&sql)
            .fetch_all(&self.pool)
            .await
            .map_err(db_err)?;
        let mut result = Vec::new();
        for r in &rows {
            let uid: String = r.try_get("user_id").map_err(db_err)?;
            if uid != user_id {
                continue;
            }
            result.push(DiffRow {
                flag: r.try_get("flag").map_err(db_err)?,
                memory_id: r.try_get("memory_id").map_err(db_err)?,
                content: r.try_get("content").map_err(db_err)?,
                memory_type: r.try_get("memory_type").map_err(db_err)?,
            });
            if result.len() >= limit as usize {
                break;
            }
        }
        Ok(result)
    }

    /// Count rows in a table at a given snapshot (for diff/validation).
    pub async fn count_at_snapshot(
        &self,
        table: &str,
        snapshot_name: &str,
        user_id: &str,
    ) -> Result<i64, MemoriaError> {
        let safe_table = validate_identifier(table)?;
        let safe_snap = validate_identifier(snapshot_name)?;
        let row = sqlx::query(&format!(
            "SELECT COUNT(*) AS cnt FROM {safe_table} {{SNAPSHOT = '{safe_snap}'}} WHERE user_id = ?"
        ))
        .bind(user_id)
        .fetch_one(&self.pool)
        .await
        .map_err(db_err)?;
        row.try_get::<i64, _>("cnt").map_err(db_err)
    }
}
