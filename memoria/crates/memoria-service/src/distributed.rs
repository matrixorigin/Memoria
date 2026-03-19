//! Distributed coordination primitives: locks, task store, instance heartbeat.
//!
//! All implementations use the shared MatrixOne database — no external
//! dependencies (Redis, etcd, etc.).  A `NoopDistributedLock` is provided
//! for single-instance deployments so the scheduler code path is identical.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::time::Duration;

use memoria_core::MemoriaError;
use memoria_storage::SqlMemoryStore;

fn db_err(e: sqlx::Error) -> MemoriaError {
    MemoriaError::Database(e.to_string())
}

// ── Distributed Lock ──────────────────────────────────────────────────────────

/// Trait for distributed mutual exclusion.
///
/// Implementations must be safe for concurrent use from multiple OS processes
/// (not just tokio tasks).  The `holder` parameter is typically the instance ID.
#[async_trait]
pub trait DistributedLock: Send + Sync {
    /// Try to acquire `key`.  Returns `true` if this holder now owns the lock.
    async fn try_acquire(
        &self,
        key: &str,
        holder: &str,
        ttl: Duration,
    ) -> Result<bool, MemoriaError>;

    /// Extend the TTL of a lock already held by `holder`.  Returns `false` if
    /// the lock is not held (or was stolen).
    async fn renew(&self, key: &str, holder: &str, ttl: Duration) -> Result<bool, MemoriaError>;

    /// Release a lock.  No-op if not held by `holder`.
    async fn release(&self, key: &str, holder: &str) -> Result<(), MemoriaError>;
}

/// Always-acquire lock for single-instance mode.
pub struct NoopDistributedLock;

#[async_trait]
impl DistributedLock for NoopDistributedLock {
    async fn try_acquire(
        &self,
        _key: &str,
        _holder: &str,
        _ttl: Duration,
    ) -> Result<bool, MemoriaError> {
        Ok(true)
    }
    async fn renew(&self, _key: &str, _holder: &str, _ttl: Duration) -> Result<bool, MemoriaError> {
        Ok(true)
    }
    async fn release(&self, _key: &str, _holder: &str) -> Result<(), MemoriaError> {
        Ok(())
    }
}

/// DB-backed distributed lock using INSERT + expired-row cleanup.
///
/// Acquire strategy (no SELECT FOR UPDATE needed):
/// 1. DELETE expired rows for this key
/// 2. INSERT IGNORE — succeeds only if no row exists
/// 3. Check if the inserted row belongs to us
#[async_trait]
impl DistributedLock for SqlMemoryStore {
    async fn try_acquire(
        &self,
        key: &str,
        holder: &str,
        ttl: Duration,
    ) -> Result<bool, MemoriaError> {
        // Clean up expired lock
        sqlx::query("DELETE FROM mem_distributed_locks WHERE lock_key = ? AND expires_at < NOW()")
            .bind(key)
            .execute(self.pool())
            .await
            .map_err(db_err)?;

        // Try to insert (fails silently if another holder owns it)
        let result = sqlx::query(
            "INSERT IGNORE INTO mem_distributed_locks (lock_key, holder_id, acquired_at, expires_at) \
             VALUES (?, ?, NOW(), DATE_ADD(NOW(), INTERVAL ? SECOND))"
        )
        .bind(key)
        .bind(holder)
        .bind(ttl.as_secs() as i64)
        .execute(self.pool())
        .await
        .map_err(db_err)?;

        if result.rows_affected() > 0 {
            return Ok(true);
        }

        // Row exists — check if we already own it (re-entrant)
        let current_holder: Option<(String,)> = sqlx::query_as(
            "SELECT holder_id FROM mem_distributed_locks WHERE lock_key = ? AND holder_id = ? AND expires_at > NOW()"
        )
        .bind(key)
        .bind(holder)
        .fetch_optional(self.pool())
        .await
        .map_err(db_err)?;

        if current_holder.is_some() {
            // Re-entrant: refresh TTL
            sqlx::query(
                "UPDATE mem_distributed_locks SET expires_at = DATE_ADD(NOW(), INTERVAL ? SECOND) \
                 WHERE lock_key = ? AND holder_id = ?",
            )
            .bind(ttl.as_secs() as i64)
            .bind(key)
            .bind(holder)
            .execute(self.pool())
            .await
            .map_err(db_err)?;
            return Ok(true);
        }

        Ok(false)
    }

    async fn renew(&self, key: &str, holder: &str, ttl: Duration) -> Result<bool, MemoriaError> {
        let result = sqlx::query(
            "UPDATE mem_distributed_locks SET expires_at = DATE_ADD(NOW(), INTERVAL ? SECOND) \
             WHERE lock_key = ? AND holder_id = ? AND expires_at > NOW()",
        )
        .bind(ttl.as_secs() as i64)
        .bind(key)
        .bind(holder)
        .execute(self.pool())
        .await
        .map_err(db_err)?;

        Ok(result.rows_affected() > 0)
    }

    async fn release(&self, key: &str, holder: &str) -> Result<(), MemoriaError> {
        sqlx::query("DELETE FROM mem_distributed_locks WHERE lock_key = ? AND holder_id = ?")
            .bind(key)
            .bind(holder)
            .execute(self.pool())
            .await
            .map_err(db_err)?;
        Ok(())
    }
}

// ── Async Task Store ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AsyncTask {
    pub task_id: String,
    pub instance_id: String,
    pub status: String,
    pub result: Option<serde_json::Value>,
    pub error: Option<serde_json::Value>,
    pub created_at: String,
    pub updated_at: String,
}

/// Trait for cross-instance async task tracking (e.g. episodic summary generation).
#[async_trait]
pub trait AsyncTaskStore: Send + Sync {
    async fn create_task(&self, task_id: &str, instance_id: &str) -> Result<(), MemoriaError>;
    async fn complete_task(
        &self,
        task_id: &str,
        result: serde_json::Value,
    ) -> Result<(), MemoriaError>;
    async fn fail_task(&self, task_id: &str, error: serde_json::Value) -> Result<(), MemoriaError>;
    async fn get_task(&self, task_id: &str) -> Result<Option<AsyncTask>, MemoriaError>;
}

#[async_trait]
impl AsyncTaskStore for SqlMemoryStore {
    async fn create_task(&self, task_id: &str, instance_id: &str) -> Result<(), MemoriaError> {
        sqlx::query(
            "INSERT INTO mem_async_tasks (task_id, instance_id, status, created_at, updated_at) \
             VALUES (?, ?, 'processing', NOW(), NOW())",
        )
        .bind(task_id)
        .bind(instance_id)
        .execute(self.pool())
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn complete_task(
        &self,
        task_id: &str,
        result: serde_json::Value,
    ) -> Result<(), MemoriaError> {
        sqlx::query(
            "UPDATE mem_async_tasks SET status = 'completed', result_json = ?, updated_at = NOW() \
             WHERE task_id = ?",
        )
        .bind(result)
        .bind(task_id)
        .execute(self.pool())
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn fail_task(&self, task_id: &str, error: serde_json::Value) -> Result<(), MemoriaError> {
        sqlx::query(
            "UPDATE mem_async_tasks SET status = 'failed', error_json = ?, updated_at = NOW() \
             WHERE task_id = ?",
        )
        .bind(error)
        .bind(task_id)
        .execute(self.pool())
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn get_task(&self, task_id: &str) -> Result<Option<AsyncTask>, MemoriaError> {
        let row = sqlx::query(
            "SELECT task_id, instance_id, status, result_json, error_json, \
                    created_at, updated_at \
             FROM mem_async_tasks WHERE task_id = ?",
        )
        .bind(task_id)
        .fetch_optional(self.pool())
        .await
        .map_err(db_err)?;

        Ok(row.map(|r| {
            use sqlx::Row;
            let created: chrono::NaiveDateTime = r.get("created_at");
            let updated: chrono::NaiveDateTime = r.get("updated_at");
            AsyncTask {
                task_id: r.get("task_id"),
                instance_id: r.get("instance_id"),
                status: r.get("status"),
                result: r.get::<Option<serde_json::Value>, _>("result_json"),
                error: r.get::<Option<serde_json::Value>, _>("error_json"),
                created_at: created.to_string(),
                updated_at: updated.to_string(),
            }
        }))
    }
}
