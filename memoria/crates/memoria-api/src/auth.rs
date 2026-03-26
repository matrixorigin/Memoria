//! Bearer token auth extractor.
//! Validates Bearer token against master key OR API key (sk-... hashed lookup).
//! When authenticated via API key, user_id is resolved from the key's owner.
//!
//! `last_used_at` updates are batched: a background task flushes accumulated
//! key hashes every 5 seconds in a single UPDATE, avoiding per-request DB writes
//! that can exhaust the connection pool under load (see #62).

use axum::{
    extract::FromRequestParts,
    http::{request::Parts, StatusCode},
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use sqlx::Row;
use std::collections::HashSet;
use std::sync::Mutex;
use subtle::ConstantTimeEq;
use tracing::warn;

use crate::state::AppState;

pub struct AuthUser {
    pub user_id: String,
    pub is_master: bool,
}

impl AuthUser {
    pub fn require_master(&self) -> Result<(), (StatusCode, String)> {
        if !self.is_master {
            Err((StatusCode::FORBIDDEN, "Master key required".to_string()))
        } else {
            Ok(())
        }
    }
}

#[derive(Deserialize)]
struct UserQuery {
    user_id: Option<String>,
}

/// Batched `last_used_at` updater.
/// Collects key hashes in memory and flushes them in a single UPDATE periodically.
pub struct LastUsedBatcher {
    pending: Mutex<HashSet<String>>,
}

impl Default for LastUsedBatcher {
    fn default() -> Self {
        Self::new()
    }
}

impl LastUsedBatcher {
    pub fn new() -> Self {
        Self {
            pending: Mutex::new(HashSet::new()),
        }
    }

    /// Enqueue a key hash for deferred `last_used_at` update. Lock-free hot path.
    pub fn mark_used(&self, key_hash: String) {
        if let Ok(mut set) = self.pending.lock() {
            set.insert(key_hash);
        }
    }

    /// Drain pending hashes and flush to DB in a single batched UPDATE.
    /// Called by the background flush task.
    pub async fn flush(&self, pool: &sqlx::MySqlPool) {
        let hashes: Vec<String> = {
            let mut set = match self.pending.lock() {
                Ok(s) => s,
                Err(_) => return,
            };
            if set.is_empty() {
                return;
            }
            set.drain().collect()
        };

        // Batch UPDATE with IN clause — single round-trip regardless of batch size.
        // Cap at 500 per flush to keep the query reasonable.
        for chunk in hashes.chunks(500) {
            let placeholders: String = chunk.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            let sql = format!(
                "UPDATE mem_api_keys SET last_used_at = NOW(6) WHERE key_hash IN ({placeholders})"
            );
            let mut query = sqlx::query(&sql);
            for h in chunk {
                query = query.bind(h);
            }
            if let Err(e) = query.execute(pool).await {
                warn!(
                    "last_used_at batch flush failed ({} keys): {e}",
                    chunk.len()
                );
            }
        }
    }
}

/// Spawn the background flush loop. Call once at server startup.
pub fn spawn_last_used_flusher(
    batcher: std::sync::Arc<LastUsedBatcher>,
    pool: sqlx::MySqlPool,
    mut shutdown: tokio::sync::watch::Receiver<()>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
        interval.tick().await; // skip immediate
        loop {
            tokio::select! {
                _ = interval.tick() => {}
                _ = shutdown.changed() => {
                    batcher.flush(&pool).await;
                    break;
                }
            }
            batcher.flush(&pool).await;
        }
        tracing::debug!("last_used flusher exiting");
    })
}

// ── Tool usage tracking ───────────────────────────────────────────────────────

use chrono::{DateTime, Utc};

type ToolUsageMap = std::collections::HashMap<(String, String), (DateTime<Utc>, bool)>;

/// In-memory cache of per-user tool access times, periodically flushed to DB.
/// On startup, rebuilt from `mem_tool_usage` so restarts don't lose data.
pub struct ToolUsageBatcher {
    /// (user_id, tool_name) → (last_used_at, dirty)
    entries: Mutex<ToolUsageMap>,
}

impl Default for ToolUsageBatcher {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolUsageBatcher {
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// Record a tool access. Cheap in-memory write.
    pub fn mark_used(&self, user_id: String, tool: String) {
        if let Ok(mut map) = self.entries.lock() {
            map.insert((user_id, tool), (Utc::now(), true));
        }
    }

    /// Query last access times for a user. Returns from memory, no DB hit.
    pub fn get_user_tool_usage(&self, user_id: &str) -> Vec<(String, DateTime<Utc>)> {
        let map = match self.entries.lock() {
            Ok(m) => m,
            Err(_) => return vec![],
        };
        map.iter()
            .filter(|((uid, _), _)| uid == user_id)
            .map(|((_, tool), (ts, _))| (tool.clone(), *ts))
            .collect()
    }

    /// Rebuild cache from DB. Call once at startup.
    pub async fn rebuild_from_db(&self, pool: &sqlx::MySqlPool) {
        let rows = match sqlx::query("SELECT user_id, tool_name, last_used_at FROM mem_tool_usage")
            .fetch_all(pool)
            .await
        {
            Ok(r) => r,
            Err(e) => {
                warn!("tool_usage rebuild failed: {e}");
                return;
            }
        };
        if let Ok(mut map) = self.entries.lock() {
            for row in &rows {
                let uid: String = row.get("user_id");
                let tool: String = row.get("tool_name");
                let ts: DateTime<Utc> = row.get("last_used_at");
                map.insert((uid, tool), (ts, false));
            }
        }
    }

    /// Flush dirty entries to DB.
    pub async fn flush(&self, pool: &sqlx::MySqlPool) {
        let dirty: Vec<(String, String, DateTime<Utc>)> = {
            let map = match self.entries.lock() {
                Ok(m) => m,
                Err(_) => return,
            };
            map.iter()
                .filter(|(_, (_, d))| *d)
                .map(|((uid, tool), (ts, _))| (uid.clone(), tool.clone(), *ts))
                .collect()
        };
        if dirty.is_empty() {
            return;
        }

        for chunk in dirty.chunks(500) {
            let placeholders: String = chunk
                .iter()
                .map(|_| "(?, ?, ?)")
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!(
                "INSERT INTO mem_tool_usage (user_id, tool_name, last_used_at) VALUES {placeholders} \
                 ON DUPLICATE KEY UPDATE last_used_at = VALUES(last_used_at)"
            );
            let mut query = sqlx::query(&sql);
            for (uid, tool, ts) in chunk {
                query = query.bind(uid).bind(tool).bind(ts);
            }
            if let Err(e) = query.execute(pool).await {
                warn!(
                    "tool_usage batch flush failed ({} entries): {e}",
                    chunk.len()
                );
                return; // keep dirty flags for retry on next cycle
            }
        }

        // Only clear dirty flags after all chunks succeed.
        if let Ok(mut map) = self.entries.lock() {
            for (uid, tool, _) in &dirty {
                if let Some((_, d)) = map.get_mut(&(uid.clone(), tool.clone())) {
                    *d = false;
                }
            }
        }
    }
}

/// Spawn the background tool-usage flush loop (10-minute interval).
pub fn spawn_tool_usage_flusher(
    batcher: std::sync::Arc<ToolUsageBatcher>,
    pool: sqlx::MySqlPool,
    mut shutdown: tokio::sync::watch::Receiver<()>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(10 * 60));
        interval.tick().await; // skip immediate
        loop {
            tokio::select! {
                _ = interval.tick() => {}
                _ = shutdown.changed() => {
                    batcher.flush(&pool).await;
                    break;
                }
            }
            batcher.flush(&pool).await;
        }
        tracing::debug!("tool_usage flusher exiting");
    })
}

// ── API call log tracking ─────────────────────────────────────────────────────

/// Request-scoped context shared between the call-log middleware and the AuthUser
/// extractor.  The middleware inserts this into request extensions before calling
/// `next`; the extractor fills in the resolved `user_id` so the middleware can
/// record the call after the handler returns.
#[derive(Clone, Default)]
pub struct CallLogContext(pub std::sync::Arc<Mutex<Option<String>>>);

/// RPC-level outcome metadata for `/mcp` calls.
/// Kept separate from the HTTP status so the two observability dimensions
/// (transport health vs. business logic errors) don't pollute each other.
pub struct RpcMeta {
    /// false when the JSON-RPC dispatch returned an error result.
    pub success: bool,
    /// JSON-RPC error code (e.g. -32601) when success = false; None otherwise.
    pub error_code: Option<i32>,
}

impl RpcMeta {
    pub fn ok() -> Self {
        Self { success: true, error_code: None }
    }
    pub fn err(code: i32) -> Self {
        Self { success: false, error_code: Some(code) }
    }
}

/// A single pending call log entry buffered in memory.
struct CallLogEntry {
    user_id: String,
    method: String,
    path: String,
    status_code: u16,
    latency_ms: u32,
    /// Always true for /v1/* REST calls.
    /// For /mcp JSON-RPC calls: false when the dispatch returned an error result.
    rpc_success: bool,
    /// JSON-RPC error code (e.g. -32601) when rpc_success = false; NULL otherwise.
    rpc_error_code: Option<i32>,
}

/// Accumulates call log entries in memory and flushes them in batches to DB.
pub struct CallLogBatcher {
    pending: Mutex<Vec<CallLogEntry>>,
}

impl Default for CallLogBatcher {
    fn default() -> Self {
        Self::new()
    }
}

impl CallLogBatcher {
    pub fn new() -> Self {
        Self {
            pending: Mutex::new(Vec::new()),
        }
    }

    /// Enqueue a REST call log entry (`/v1/*`). RPC fields default to success.
    pub fn record(
        &self,
        user_id: String,
        method: String,
        path: String,
        status_code: u16,
        latency_ms: u32,
    ) {
        self.record_rpc(user_id, method, path, status_code, latency_ms, RpcMeta::ok());
    }

    /// Enqueue a call log entry with explicit JSON-RPC success/error metadata.
    /// Use this for `/mcp` calls so that HTTP status (always 200 for JSON-RPC)
    /// and business-level error tracking are kept separate.
    pub fn record_rpc(
        &self,
        user_id: String,
        method: String,
        path: String,
        status_code: u16,
        latency_ms: u32,
        rpc: RpcMeta,
    ) {
        if let Ok(mut v) = self.pending.lock() {
            v.push(CallLogEntry {
                user_id,
                method,
                path,
                status_code,
                latency_ms,
                rpc_success: rpc.success,
                rpc_error_code: rpc.error_code,
            });
        }
    }

    /// Drain pending entries and write them to `mem_api_call_log` in chunks.
    pub async fn flush(&self, pool: &sqlx::MySqlPool) {
        let entries: Vec<CallLogEntry> = {
            let mut v = match self.pending.lock() {
                Ok(v) => v,
                Err(_) => return,
            };
            if v.is_empty() {
                return;
            }
            v.drain(..).collect()
        };

        for chunk in entries.chunks(200) {
            let placeholders: String = chunk
                .iter()
                .map(|_| "(?, ?, ?, ?, ?, ?, ?)")
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!(
                "INSERT INTO mem_api_call_log \
                 (user_id, method, path, status_code, latency_ms, rpc_success, rpc_error_code) \
                 VALUES {placeholders}"
            );
            let mut query = sqlx::query(&sql);
            for e in chunk {
                query = query
                    .bind(&e.user_id)
                    .bind(&e.method)
                    .bind(&e.path)
                    .bind(e.status_code as i16)
                    .bind(e.latency_ms as i32)
                    .bind(e.rpc_success as i8)
                    .bind(e.rpc_error_code);
            }
            if let Err(e) = query.execute(pool).await {
                warn!("call_log batch flush failed ({} entries): {e}", chunk.len());
            }
        }
    }
}

/// Spawn the background call-log flush loop (5-second interval).
pub fn spawn_call_log_flusher(
    batcher: std::sync::Arc<CallLogBatcher>,
    pool: sqlx::MySqlPool,
    mut shutdown: tokio::sync::watch::Receiver<()>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
        interval.tick().await; // skip immediate first tick
        loop {
            tokio::select! {
                _ = interval.tick() => {}
                _ = shutdown.changed() => {
                    batcher.flush(&pool).await;
                    break;
                }
            }
            batcher.flush(&pool).await;
        }
        tracing::debug!("call_log flusher exiting");
    })
}

#[axum::async_trait]
impl FromRequestParts<AppState> for AuthUser {
    type Rejection = (StatusCode, String);

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        // Extract optional tool/agent name for usage tracking (skip empty values).
        // Agents send X-Memoria-Tool with their name: cursor / kiro / claude / codex / openclaw.
        // Fall back to X-Tool-Name for backwards compatibility with older clients.
        // Any non-empty value is accepted — no whitelist, so new agents work automatically.
        let tool_name = parts
            .headers
            .get("X-Memoria-Tool")
            .or_else(|| parts.headers.get("X-Tool-Name"))
            .and_then(|v| v.to_str().ok())
            .filter(|v| !v.is_empty())
            .map(String::from);

        let bearer = parts
            .headers
            .get("Authorization")
            .and_then(|v| v.to_str().ok())
            .filter(|v| v.starts_with("Bearer "))
            .map(|v| &v[7..]);

        if let Some(token) = bearer {
            // 1) Master key — full access, fall through to X-User-Id extraction
            let master_match = !state.master_key.is_empty()
                && token.len() == state.master_key.len()
                && token.as_bytes().ct_eq(state.master_key.as_bytes()).into();
            if master_match {
                // fall through
            }
            // 2) API key — user_id resolved from DB, never master
            else if let Some(uid) = validate_api_key(token, state).await {
                if let Some(tool) = tool_name {
                    state.tool_usage_batcher.mark_used(uid.clone(), tool);
                }
                // Notify call-log middleware (if present) of the resolved user_id.
                // The middleware inserted CallLogContext into extensions before calling next;
                // we fill in the user_id so it can record the call after the handler returns.
                if let Some(ctx) = parts.extensions.get::<CallLogContext>() {
                    if let Ok(mut guard) = ctx.0.lock() {
                        *guard = Some(uid.clone());
                    }
                }
                return Ok(AuthUser {
                    user_id: uid,
                    is_master: false,
                });
            } else {
                crate::metrics::registry().security.auth_failures.inc();
                warn!(
                    token_prefix = &token[..token.len().min(8)],
                    "auth: invalid token"
                );
                return Err((StatusCode::UNAUTHORIZED, "Invalid token".to_string()));
            }
        } else if !state.master_key.is_empty() {
            // master_key is configured but caller sent no Bearer token
            crate::metrics::registry().security.auth_failures.inc();
            warn!("auth: missing Bearer token");
            return Err((StatusCode::UNAUTHORIZED, "Missing Bearer token".to_string()));
        }
        // Reached here: master key validated, or no-auth open mode (master_key not configured)

        let user_id = parts
            .headers
            .get("X-User-Id")
            .or_else(|| parts.headers.get("X-Impersonate-User"))
            .and_then(|v| v.to_str().ok())
            .map(String::from)
            .or_else(|| {
                let uri = parts.uri.query().unwrap_or("");
                serde_urlencoded::from_str::<UserQuery>(uri)
                    .ok()
                    .and_then(|q| q.user_id)
            })
            .unwrap_or_else(|| "default".to_string());

        if let Some(tool) = tool_name {
            state.tool_usage_batcher.mark_used(user_id.clone(), tool);
        }

        if let Some(ctx) = parts.extensions.get::<CallLogContext>() {
            if let Ok(mut guard) = ctx.0.lock() {
                *guard = Some(user_id.clone());
            }
        }

        Ok(AuthUser {
            user_id,
            is_master: true,
        })
    }
}

/// Hash the raw API key and look it up in mem_api_keys.
/// Returns Some(user_id) if valid, None otherwise.
///
/// Uses a dedicated auth connection pool so that auth validation is never
/// blocked by slow business queries on the main pool.
/// `last_used_at` is updated via batched writes (see [`LastUsedBatcher`]).
async fn validate_api_key(token: &str, state: &AppState) -> Option<String> {
    state.service.sql_store.as_ref()?;
    let key_hash = format!("{:x}", Sha256::digest(token.as_bytes()));

    // Rate limit check (before cache, to count all attempts)
    if !state.rate_limiter.allow(&key_hash).await {
        crate::metrics::registry().security.auth_failures.inc();
        return None;
    }

    // Check cache first — no DB hit at all
    if let Some(user_id) = state.api_key_cache.get(&key_hash).await {
        // Still enqueue last_used_at update (batched, no DB pressure)
        state.last_used_batcher.mark_used(key_hash);
        return Some(user_id);
    }

    let Some(pool) = state.auth_pool.as_ref() else {
        warn!("validate_api_key: dedicated auth pool unavailable");
        return None;
    };

    let row = sqlx::query(
        "SELECT user_id FROM mem_api_keys \
         WHERE key_hash = ? AND is_active = 1 \
         AND (expires_at IS NULL OR expires_at > NOW(6))",
    )
    .bind(&key_hash)
    .fetch_optional(pool)
    .await
    .map_err(|e| warn!("validate_api_key: DB query failed: {e}"))
    .ok()??;

    let user_id: String = row.try_get("user_id").ok()?;

    // Cache the result (TTL 5 min)
    state
        .api_key_cache
        .insert(key_hash.clone(), user_id.clone())
        .await;

    // Enqueue batched last_used_at update — zero DB pressure on hot path
    state.last_used_batcher.mark_used(key_hash);

    Some(user_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tool_usage_mark_and_query() {
        let b = ToolUsageBatcher::new();
        b.mark_used("alice".into(), "memory_store".into());
        b.mark_used("alice".into(), "memory_retrieve".into());
        b.mark_used("bob".into(), "memory_store".into());

        let alice = b.get_user_tool_usage("alice");
        assert_eq!(alice.len(), 2);
        let tools: Vec<&str> = alice.iter().map(|(t, _)| t.as_str()).collect();
        assert!(tools.contains(&"memory_store"));
        assert!(tools.contains(&"memory_retrieve"));

        let bob = b.get_user_tool_usage("bob");
        assert_eq!(bob.len(), 1);
        assert_eq!(bob[0].0, "memory_store");

        assert!(b.get_user_tool_usage("nobody").is_empty());
    }

    #[test]
    fn test_tool_usage_overwrite_updates_time() {
        let b = ToolUsageBatcher::new();
        b.mark_used("alice".into(), "memory_store".into());
        let t1 = b.get_user_tool_usage("alice")[0].1;

        std::thread::sleep(std::time::Duration::from_millis(10));
        b.mark_used("alice".into(), "memory_store".into());
        let t2 = b.get_user_tool_usage("alice")[0].1;

        assert!(t2 > t1);
    }

    #[test]
    fn test_tool_usage_empty_tool_not_stored() {
        // Simulates what the AuthUser extractor does: filter(|v| !v.is_empty())
        let raw = "";
        let tool_name = Some(raw).filter(|v| !v.is_empty()).map(String::from);
        assert!(tool_name.is_none());
    }
}
