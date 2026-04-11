//! Push-based operational metrics reporter.
//!
//! Collects aggregate counters (memories, edits, API calls) and writes them
//! into the shared `memoria_srv` database for the admin dashboard.
//!
//! Design constraints:
//! - **Best-effort**: channel is bounded; events are silently dropped when full.
//! - **Non-blocking**: `report()` is synchronous and never waits for a DB write.
//! - **No user content**: only aggregate counters and category labels are stored.

use sqlx::MySqlPool;
use std::collections::HashMap;
use tokio::sync::mpsc;
use tracing::warn;

const CHANNEL_CAP: usize = 1_000;
const FLUSH_INTERVAL_MS: u64 = 2_000;
const BATCH_FLUSH_SIZE: usize = 200;

/// Every write-path event that should be reflected in the shared stats tables.
#[derive(Debug)]
pub enum StatsEvent {
    /// A new memory was stored (active).
    MemoryStored {
        user_id: String,
        memory_type: String,
        trust_tier: String,
    },
    /// An existing memory was deactivated (soft-deleted / superseded).
    MemoryDeactivated { user_id: String },
    /// A previously deactivated memory was re-activated.
    MemoryActivated { user_id: String },
    /// An edit-log entry was written (any operation).
    EditLogged {
        user_id: String,
        operation: String,
    },
    /// One or more new entities were upserted for a user.
    EntitiesUpserted { user_id: String, count: u64 },
    /// An API or MCP call completed (used for devops/MCP tab).
    ApiCallLogged {
        user_id: String,
        path: String,
        is_mcp: bool,
        is_success: bool,
    },
}

/// Lightweight handle that enqueues stats events into a background writer.
#[derive(Clone)]
pub struct StatsReporter {
    tx: mpsc::Sender<StatsEvent>,
}

impl StatsReporter {
    /// Construct a new reporter that writes to `shared_pool`.
    /// Spawns a background tokio task; call from an async context.
    pub fn new(shared_pool: MySqlPool) -> Self {
        let (tx, rx) = mpsc::channel(CHANNEL_CAP);
        tokio::spawn(run_writer(rx, shared_pool));
        Self { tx }
    }

    /// Enqueue an event. Never blocks; silently drops when the channel is full.
    pub fn report(&self, event: StatsEvent) {
        let _ = self.tx.try_send(event);
    }
}

// ── Background writer ─────────────────────────────────────────────────────────

async fn run_writer(mut rx: mpsc::Receiver<StatsEvent>, pool: MySqlPool) {
    let mut buf: Vec<StatsEvent> = Vec::with_capacity(BATCH_FLUSH_SIZE);
    loop {
        // Collect up to BATCH_FLUSH_SIZE events or wait up to FLUSH_INTERVAL_MS.
        let deadline = tokio::time::sleep(tokio::time::Duration::from_millis(FLUSH_INTERVAL_MS));
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                event = rx.recv() => {
                    match event {
                        Some(e) => {
                            buf.push(e);
                            if buf.len() >= BATCH_FLUSH_SIZE { break; }
                        }
                        None => {
                            // Channel closed (shutdown) — do a final flush.
                            if !buf.is_empty() {
                                flush_batch(&buf, &pool).await;
                                buf.clear();
                            }
                            return;
                        }
                    }
                }
                _ = &mut deadline => break,
            }
        }
        if !buf.is_empty() {
            flush_batch(&buf, &pool).await;
            buf.clear();
        }
    }
}

// ── Batch aggregation & SQL flush ─────────────────────────────────────────────

#[derive(Default)]
struct UserStatsDelta {
    total_memories: i64,
    active_memories: i64,
    inactive_memories: i64,
    total_entities: i64,
    total_edits: i64,
}

#[derive(Default)]
struct ApiStatsDelta {
    total_calls: i64,
    mcp_calls: i64,
    mcp_errors: i64,
    first_mcp_call: Option<chrono::NaiveDateTime>,
}

async fn flush_batch(events: &[StatsEvent], pool: &MySqlPool) {
    // Aggregate all events in-memory before touching the DB.
    let mut user_stats: HashMap<String, UserStatsDelta> = HashMap::new();
    let mut api_stats: HashMap<String, ApiStatsDelta> = HashMap::new();
    // (user_id, metric, dim_key) → delta_count
    let mut metric_detail: HashMap<(String, String, String), i64> = HashMap::new();
    // (date, metric) → delta_count
    let mut daily: HashMap<(chrono::NaiveDate, String), i64> = HashMap::new();
    // path → delta_count (MCP paths)
    let mut mcp_paths: HashMap<String, i64> = HashMap::new();

    let today = chrono::Utc::now().naive_utc().date();

    for event in events {
        match event {
            StatsEvent::MemoryStored {
                user_id,
                memory_type,
                trust_tier,
            } => {
                let u = user_stats.entry(user_id.clone()).or_default();
                u.total_memories += 1;
                u.active_memories += 1;
                *metric_detail
                    .entry((user_id.clone(), "memory_type".into(), memory_type.clone()))
                    .or_default() += 1;
                *metric_detail
                    .entry((user_id.clone(), "trust_tier".into(), trust_tier.clone()))
                    .or_default() += 1;
                *daily
                    .entry((today, "memory_stored".into()))
                    .or_default() += 1;
            }
            StatsEvent::MemoryDeactivated { user_id } => {
                let u = user_stats.entry(user_id.clone()).or_default();
                u.active_memories -= 1;
                u.inactive_memories += 1;
            }
            StatsEvent::MemoryActivated { user_id } => {
                let u = user_stats.entry(user_id.clone()).or_default();
                u.active_memories += 1;
                u.inactive_memories -= 1;
            }
            StatsEvent::EditLogged { user_id, operation } => {
                let u = user_stats.entry(user_id.clone()).or_default();
                u.total_edits += 1;
                *metric_detail
                    .entry((user_id.clone(), "edit_op".into(), operation.clone()))
                    .or_default() += 1;
            }
            StatsEvent::EntitiesUpserted { user_id, count } => {
                let u = user_stats.entry(user_id.clone()).or_default();
                u.total_entities += *count as i64;
            }
            StatsEvent::ApiCallLogged {
                user_id,
                path,
                is_mcp,
                is_success,
            } => {
                let a = api_stats.entry(user_id.clone()).or_default();
                a.total_calls += 1;
                if *is_mcp {
                    a.mcp_calls += 1;
                    if !is_success {
                        a.mcp_errors += 1;
                    }
                    if a.first_mcp_call.is_none() {
                        a.first_mcp_call = Some(chrono::Utc::now().naive_utc());
                    }
                    *mcp_paths.entry(path.clone()).or_default() += 1;
                    *daily
                        .entry((today, "mcp_calls".into()))
                        .or_default() += 1;
                }
                *daily
                    .entry((today, "api_calls".into()))
                    .or_default() += 1;
            }
        }
    }

    // Capture a single timestamp for all updated_at columns in this flush.
    let now = chrono::Utc::now().naive_utc();

    // ── Write srv_user_stats (one multi-row INSERT per flush) ────────────────
    if !user_stats.is_empty() {
        let mut qb: sqlx::QueryBuilder<sqlx::MySql> = sqlx::QueryBuilder::new(
            "INSERT INTO srv_user_stats \
             (user_id, total_memories, active_memories, inactive_memories, \
              total_entities, total_edits, updated_at) ",
        );
        qb.push_values(user_stats.iter(), |mut b, (user_id, delta)| {
            b.push_bind(user_id)
                .push_bind(delta.total_memories)
                .push_bind(delta.active_memories.max(0))
                .push_bind(delta.inactive_memories.max(0))
                .push_bind(delta.total_entities)
                .push_bind(delta.total_edits)
                .push_bind(now);
        });
        qb.push(
            " ON DUPLICATE KEY UPDATE \
               total_memories    = total_memories    + VALUES(total_memories), \
               active_memories   = GREATEST(0, active_memories   + VALUES(active_memories)), \
               inactive_memories = GREATEST(0, inactive_memories + VALUES(inactive_memories)), \
               total_entities    = total_entities    + VALUES(total_entities), \
               total_edits       = total_edits       + VALUES(total_edits), \
               updated_at        = VALUES(updated_at)",
        );
        if let Err(e) = qb.build().execute(pool).await {
            warn!(rows = user_stats.len(), error = %e, "stats_reporter: srv_user_stats write failed");
        }
    }

    // ── Write srv_user_metric_detail (one multi-row INSERT per flush) ────────
    if !metric_detail.is_empty() {
        let mut qb: sqlx::QueryBuilder<sqlx::MySql> = sqlx::QueryBuilder::new(
            "INSERT INTO srv_user_metric_detail (user_id, metric, dim_key, cnt) ",
        );
        qb.push_values(
            metric_detail.iter(),
            |mut b, ((user_id, metric, dim_key), cnt)| {
                b.push_bind(user_id)
                    .push_bind(metric)
                    .push_bind(dim_key)
                    .push_bind(*cnt);
            },
        );
        qb.push(" ON DUPLICATE KEY UPDATE cnt = cnt + VALUES(cnt)");
        if let Err(e) = qb.build().execute(pool).await {
            warn!(rows = metric_detail.len(), error = %e, "stats_reporter: srv_user_metric_detail write failed");
        }
    }

    // ── Write srv_daily_stats (one multi-row INSERT per flush) ───────────────
    if !daily.is_empty() {
        let mut qb: sqlx::QueryBuilder<sqlx::MySql> =
            sqlx::QueryBuilder::new("INSERT INTO srv_daily_stats (dt, metric, cnt) ");
        qb.push_values(daily.iter(), |mut b, ((dt, metric), cnt)| {
            b.push_bind(dt).push_bind(metric).push_bind(*cnt);
        });
        qb.push(" ON DUPLICATE KEY UPDATE cnt = cnt + VALUES(cnt)");
        if let Err(e) = qb.build().execute(pool).await {
            warn!(rows = daily.len(), error = %e, "stats_reporter: srv_daily_stats write failed");
        }
    }

    // ── Write srv_user_api_stats (one multi-row INSERT per flush) ────────────
    if !api_stats.is_empty() {
        let mut qb: sqlx::QueryBuilder<sqlx::MySql> = sqlx::QueryBuilder::new(
            "INSERT INTO srv_user_api_stats \
             (user_id, total_calls, mcp_calls, mcp_errors, first_mcp_call, updated_at) ",
        );
        qb.push_values(api_stats.iter(), |mut b, (user_id, delta)| {
            b.push_bind(user_id)
                .push_bind(delta.total_calls)
                .push_bind(delta.mcp_calls)
                .push_bind(delta.mcp_errors)
                .push_bind(delta.first_mcp_call)
                .push_bind(now);
        });
        qb.push(
            " ON DUPLICATE KEY UPDATE \
               total_calls    = total_calls  + VALUES(total_calls), \
               mcp_calls      = mcp_calls    + VALUES(mcp_calls), \
               mcp_errors     = mcp_errors   + VALUES(mcp_errors), \
               first_mcp_call = COALESCE(first_mcp_call, VALUES(first_mcp_call)), \
               updated_at     = VALUES(updated_at)",
        );
        if let Err(e) = qb.build().execute(pool).await {
            warn!(rows = api_stats.len(), error = %e, "stats_reporter: srv_user_api_stats write failed");
        }
    }

    // ── Write srv_mcp_path_stats (one multi-row INSERT per flush) ────────────
    if !mcp_paths.is_empty() {
        let mut qb: sqlx::QueryBuilder<sqlx::MySql> =
            sqlx::QueryBuilder::new("INSERT INTO srv_mcp_path_stats (path, cnt) ");
        qb.push_values(mcp_paths.iter(), |mut b, (path, cnt)| {
            b.push_bind(path).push_bind(*cnt);
        });
        qb.push(" ON DUPLICATE KEY UPDATE cnt = cnt + VALUES(cnt)");
        if let Err(e) = qb.build().execute(pool).await {
            warn!(rows = mcp_paths.len(), error = %e, "stats_reporter: srv_mcp_path_stats write failed");
        }
    }

    tracing::debug!(
        events = events.len(),
        users = user_stats.len(),
        "stats_reporter: batch flushed"
    );
}
