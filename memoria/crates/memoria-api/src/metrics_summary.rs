//! Metrics rollup + refresh system for multi-db mode.
//!
//! Implements the two-layer model from `docs/metrics-rollup-refresh-design.md`:
//!
//! 1. **State table** (`mem_metrics_user_state`): tracks per-user refresh state
//!    with `dirty_mask + version` semantics so writes only mark dirty and the
//!    worker avoids clearing bits when writes race with refresh.
//!
//! 2. **Rollup table** (`mem_metrics_user_rollups`): stores all low-cardinality
//!    business metrics as `(user_id, family, bucket, value)` tuples, avoiding
//!    per-family tables and per-metric columns.
//!
//! Key invariants:
//! - The write path (`mark_user_dirty`) is a single-row upsert that OR-merges
//!   the pending mask and bumps `change_version`.
//! - The worker claims a batch, computes only the families indicated by the
//!   mask, flushes rollups per-family in bulk, then updates state with
//!   version-protected CAS to avoid clearing newly-arrived dirty bits.
//! - Connection pools are treated as precious: claim/flush are short
//!   transactions on the shared pool; user-DB reads use bounded concurrency
//!   on the global user pool.

use crate::state::CachedMetrics;
use chrono::Utc;
use memoria_core::MemoriaError;
use memoria_service::MemoryService;
use memoria_storage::SqlMemoryStore;
use sqlx::{MySqlPool, Row};
use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::{watch, Notify, RwLock};
use tracing::{info, warn};

// ── Configuration bounds ──────────────────────────────────────────────────────

const REFRESH_INTERVAL_MAX_SECS: u64 = 60;
const REFRESH_DEBOUNCE_MAX_MILLIS: u64 = 30_000;
const BATCH_SIZE_MAX: u32 = 512;
const MAX_CONCURRENCY_MAX: usize = 32;
/// Maximum rows per bulk INSERT chunk when flushing rollups.
const ROLLUP_FLUSH_CHUNK_ROWS: usize = 500;

// ── Dirty mask ────────────────────────────────────────────────────────────────

/// Bitmask indicating which metric families need refresh for a user.
///
/// Each bit maps to one or more families in the [`FAMILY_REGISTRY`].
/// The write path OR-merges the mask; the worker reads it to decide which
/// families to recompute.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DirtyMask(pub u64);

impl DirtyMask {
    pub const MEMORY: Self = Self(1 << 0);
    pub const FEEDBACK: Self = Self(1 << 1);
    pub const GRAPH: Self = Self(1 << 2);
    pub const SNAPSHOT: Self = Self(1 << 3);
    pub const BRANCH: Self = Self(1 << 4);
    pub const FULL: Self = Self((1 << 6) - 1); // bits 0..5

    pub fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }
}

impl std::ops::BitOr for DirtyMask {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

// ── Family registry ───────────────────────────────────────────────────────────

/// A metric family that can be materialized into the rollup table.
///
/// Each entry declares which dirty bit triggers it, whether it produces
/// labelled buckets, and what Prometheus metric name to render.
#[derive(Debug, Clone, Copy)]
struct FamilyDef {
    /// Key stored in the `family` column of `mem_metrics_user_rollups`.
    family: &'static str,
    /// Which dirty bit triggers recomputation of this family.
    trigger: DirtyMask,
    /// Whether this family produces labelled buckets (true) or a single
    /// `__total__` scalar (false).  Used by rendering and validation.
    #[allow(dead_code)]
    is_labelled: bool,
}

/// Authoritative list of all rollup families.  Adding a new business metric
/// means adding an entry here and implementing its computation in
/// [`compute_family_for_user`].
static FAMILY_REGISTRY: &[FamilyDef] = &[
    FamilyDef {
        family: "memory_total",
        trigger: DirtyMask::MEMORY,
        is_labelled: false,
    },
    FamilyDef {
        family: "memory_type",
        trigger: DirtyMask::MEMORY,
        is_labelled: true,
    },
    FamilyDef {
        family: "feedback_signal",
        trigger: DirtyMask::FEEDBACK,
        is_labelled: true,
    },
    FamilyDef {
        family: "graph_nodes_total",
        trigger: DirtyMask::GRAPH,
        is_labelled: false,
    },
    FamilyDef {
        family: "graph_edges_total",
        trigger: DirtyMask::GRAPH,
        is_labelled: false,
    },
    FamilyDef {
        family: "snapshots_total",
        trigger: DirtyMask::SNAPSHOT,
        is_labelled: false,
    },
    FamilyDef {
        family: "branches_extra_total",
        trigger: DirtyMask::BRANCH,
        is_labelled: false,
    },
];

const TOTAL_BUCKET: &str = "__total__";

// ── Public metrics types ──────────────────────────────────────────────────────

#[derive(Debug, Default, Clone)]
pub struct SummaryRefreshStats {
    pub inflight_users: u64,
    pub failures_total: u64,
    pub last_duration_secs: f64,
    pub last_success_age_secs: Option<u64>,
    pub last_failure_age_secs: Option<u64>,
}

/// Aggregated global metrics read from the rollup + state tables.
/// Used by the `/metrics` endpoint in multi-db mode.
#[derive(Debug, Default, Clone)]
pub struct GlobalSummaryMetrics {
    pub available: bool,
    pub total_users: i64,
    pub total_memories: i64,
    pub memory_counts: BTreeMap<String, i64>,
    pub feedback_counts: BTreeMap<String, i64>,
    pub graph_nodes_total: i64,
    pub graph_edges_total: i64,
    pub snapshots_total: i64,
    pub branches_extra_total: i64,
    pub ready_users_total: i64,
    pub dirty_users_total: i64,
    pub missing_users_total: i64,
    pub oldest_ready_refresh_age_secs: Option<i64>,
}

// ── Internal types ────────────────────────────────────────────────────────────

#[derive(Debug, Default)]
struct RefreshStats {
    inflight_users: AtomicU64,
    failures_total: AtomicU64,
    last_duration_ms: AtomicU64,
    last_success_epoch_secs: AtomicU64,
    last_failure_epoch_secs: AtomicU64,
}

/// A single rollup entry produced by computing a family for a user.
#[derive(Debug, Clone)]
struct RollupEntry {
    family: &'static str,
    bucket: String,
    value: i64,
}

/// A user claimed for refresh, with the snapshot of their state at claim time.
#[derive(Debug)]
struct ClaimedUser {
    user_id: String,
    db_name: String,
    claimed_version: u64,
    claimed_mask: u64,
}

// ── Manager ───────────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct MetricsSummaryManager {
    service: Arc<MemoryService>,
    shared_pool: MySqlPool,
    metrics_cache: Arc<RwLock<Option<CachedMetrics>>>,
    notify: Arc<Notify>,
    refresh_interval: Duration,
    refresh_debounce: Duration,
    batch_size: usize,
    max_concurrency: usize,
    stats: Arc<RefreshStats>,
}

impl MetricsSummaryManager {
    pub fn new(
        service: Arc<MemoryService>,
        shared_pool: MySqlPool,
        metrics_cache: Arc<RwLock<Option<CachedMetrics>>>,
    ) -> Self {
        let refresh_interval = Duration::from_secs(
            std::env::var("MEMORIA_METRICS_SUMMARY_REFRESH_INTERVAL_SECS")
                .ok()
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(2)
                .clamp(1, REFRESH_INTERVAL_MAX_SECS),
        );
        let refresh_debounce = Duration::from_millis(
            std::env::var("MEMORIA_METRICS_SUMMARY_DEBOUNCE_MILLIS")
                .ok()
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(3_000)
                .clamp(0, REFRESH_DEBOUNCE_MAX_MILLIS),
        );
        let batch_size = std::env::var("MEMORIA_METRICS_SUMMARY_BATCH_SIZE")
            .ok()
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(64)
            .clamp(1, BATCH_SIZE_MAX) as usize;
        let max_concurrency = std::env::var("MEMORIA_METRICS_SUMMARY_MAX_CONCURRENCY")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(4)
            .clamp(1, MAX_CONCURRENCY_MAX);
        Self {
            service,
            shared_pool,
            metrics_cache,
            notify: Arc::new(Notify::new()),
            refresh_interval,
            refresh_debounce,
            batch_size,
            max_concurrency,
            stats: Arc::new(RefreshStats::default()),
        }
    }

    // ── Schema ────────────────────────────────────────────────────────────

    pub async fn ensure_schema(&self) -> Result<(), MemoriaError> {
        sqlx::query(
            r#"CREATE TABLE IF NOT EXISTS mem_metrics_user_state (
                user_id                 VARCHAR(64) PRIMARY KEY,
                pending_mask            BIGINT UNSIGNED NOT NULL DEFAULT 0,
                has_pending             TINYINT(1)      NOT NULL DEFAULT 0,
                change_version          BIGINT UNSIGNED NOT NULL DEFAULT 0,
                next_eligible_at        DATETIME(6)     NOT NULL,
                last_refreshed_version  BIGINT UNSIGNED NOT NULL DEFAULT 0,
                refreshed_at            DATETIME(6)     DEFAULT NULL,
                claim_token             VARCHAR(64)     DEFAULT NULL,
                claim_expires_at        DATETIME(6)     DEFAULT NULL,
                last_error_kind         VARCHAR(32)     DEFAULT NULL,
                last_error_at           DATETIME(6)     DEFAULT NULL,
                updated_at              DATETIME(6)     NOT NULL,
                INDEX idx_claimable (has_pending, next_eligible_at),
                INDEX idx_claim_expiry (claim_expires_at)
            )"#,
        )
        .execute(&self.shared_pool)
        .await
        .map_err(db_err)?;

        sqlx::query(
            r#"CREATE TABLE IF NOT EXISTS mem_metrics_user_rollups (
                user_id            VARCHAR(64)      NOT NULL,
                family             VARCHAR(32)      NOT NULL,
                bucket             VARCHAR(64)      NOT NULL,
                value              BIGINT           NOT NULL,
                refreshed_version  BIGINT UNSIGNED  NOT NULL,
                updated_at         DATETIME(6)      NOT NULL,
                PRIMARY KEY (user_id, family, bucket),
                INDEX idx_family_bucket (family, bucket),
                INDEX idx_user_family (user_id, family)
            )"#,
        )
        .execute(&self.shared_pool)
        .await
        .map_err(db_err)?;

        info!("Metrics rollup schema ensured (mem_metrics_user_state + mem_metrics_user_rollups)");
        Ok(())
    }

    // ── Public API ────────────────────────────────────────────────────────

    pub fn refresh_stats(&self) -> SummaryRefreshStats {
        SummaryRefreshStats {
            inflight_users: self.stats.inflight_users.load(Ordering::Relaxed),
            failures_total: self.stats.failures_total.load(Ordering::Relaxed),
            last_duration_secs: self.stats.last_duration_ms.load(Ordering::Relaxed) as f64 / 1000.0,
            last_success_age_secs: age_from_epoch(
                self.stats.last_success_epoch_secs.load(Ordering::Relaxed),
            ),
            last_failure_age_secs: age_from_epoch(
                self.stats.last_failure_epoch_secs.load(Ordering::Relaxed),
            ),
        }
    }

    /// Mark a user dirty with the given mask.  Single-row upsert, very fast.
    ///
    /// The mask is OR-merged with any existing pending_mask, change_version is
    /// bumped, and next_eligible_at is set to `now + debounce` for coalescing.
    pub async fn mark_user_dirty(
        &self,
        user_id: &str,
        mask: DirtyMask,
    ) -> Result<(), MemoriaError> {
        if user_id.trim().is_empty() || mask.is_empty() {
            return Ok(());
        }
        let now = Utc::now().naive_utc();
        let eligible_at =
            now + chrono::Duration::milliseconds(self.refresh_debounce.as_millis() as i64);
        sqlx::query(
            r#"INSERT INTO mem_metrics_user_state (
                   user_id, pending_mask, has_pending, change_version,
                   next_eligible_at, updated_at
               ) VALUES (?, ?, 1, 1, ?, ?)
               ON DUPLICATE KEY UPDATE
                   pending_mask     = pending_mask | VALUES(pending_mask),
                   has_pending      = 1,
                   change_version   = change_version + 1,
                   next_eligible_at = VALUES(next_eligible_at),
                   updated_at       = VALUES(updated_at)"#,
        )
        .bind(user_id)
        .bind(mask.0)
        .bind(eligible_at)
        .bind(now)
        .execute(&self.shared_pool)
        .await
        .map_err(db_err)?;
        self.notify.notify_one();
        Ok(())
    }

    /// Load aggregated global metrics from rollup + state tables.
    /// Called by `/metrics` in multi-db mode — reads only the shared DB.
    pub async fn load_global_metrics(&self) -> Result<GlobalSummaryMetrics, MemoriaError> {
        // Coverage: how many users are clean / dirty / missing state
        let coverage = sqlx::query(
            r#"SELECT
                   COUNT(*) AS total_users,
                   SUM(CASE WHEN s.user_id IS NULL THEN 1 ELSE 0 END) AS missing_users,
                   SUM(CASE WHEN s.user_id IS NOT NULL AND s.has_pending = 1 THEN 1 ELSE 0 END) AS dirty_users,
                   SUM(CASE WHEN s.user_id IS NOT NULL AND s.has_pending = 0 THEN 1 ELSE 0 END) AS ready_users
               FROM mem_user_registry r
               LEFT JOIN mem_metrics_user_state s ON s.user_id = r.user_id
               WHERE r.status = 'active'"#,
        )
        .fetch_one(&self.shared_pool)
        .await
        .map_err(db_err)?;

        // Scalar families from rollups
        let scalar_rows = sqlx::query(
            r#"SELECT r.family, COALESCE(SUM(r.value), 0) AS total
               FROM mem_metrics_user_rollups r
               INNER JOIN mem_user_registry u ON u.user_id = r.user_id
               WHERE u.status = 'active' AND r.bucket = '__total__'
               GROUP BY r.family"#,
        )
        .fetch_all(&self.shared_pool)
        .await
        .map_err(db_err)?;

        // Label families
        let memory_type_rows = sqlx::query(
            r#"SELECT r.bucket, COALESCE(SUM(r.value), 0) AS total
               FROM mem_metrics_user_rollups r
               INNER JOIN mem_user_registry u ON u.user_id = r.user_id
               WHERE u.status = 'active' AND r.family = 'memory_type'
               GROUP BY r.bucket"#,
        )
        .fetch_all(&self.shared_pool)
        .await
        .map_err(db_err)?;

        let feedback_rows = sqlx::query(
            r#"SELECT r.bucket, COALESCE(SUM(r.value), 0) AS total
               FROM mem_metrics_user_rollups r
               INNER JOIN mem_user_registry u ON u.user_id = r.user_id
               WHERE u.status = 'active' AND r.family = 'feedback_signal'
               GROUP BY r.bucket"#,
        )
        .fetch_all(&self.shared_pool)
        .await
        .map_err(db_err)?;

        // Oldest refresh age
        let oldest_refresh = sqlx::query(
            r#"SELECT
                   MAX(TIMESTAMPDIFF(SECOND, s.refreshed_at, UTC_TIMESTAMP())) AS oldest_age
               FROM mem_metrics_user_state s
               INNER JOIN mem_user_registry r ON r.user_id = s.user_id
               WHERE r.status = 'active'
                 AND s.has_pending = 0
                 AND s.refreshed_at IS NOT NULL"#,
        )
        .fetch_one(&self.shared_pool)
        .await
        .map_err(db_err)?;

        // Assemble
        let mut metrics = GlobalSummaryMetrics {
            available: true,
            total_users: optional_i64(&coverage, "total_users"),
            ready_users_total: optional_i64(&coverage, "ready_users"),
            dirty_users_total: optional_i64(&coverage, "dirty_users"),
            missing_users_total: optional_i64(&coverage, "missing_users"),
            oldest_ready_refresh_age_secs: oldest_refresh
                .try_get::<Option<i64>, _>("oldest_age")
                .map_err(db_err)?,
            ..GlobalSummaryMetrics::default()
        };

        for row in scalar_rows {
            let family: String = row.try_get("family").map_err(db_err)?;
            let total = optional_i64(&row, "total");
            match family.as_str() {
                "memory_total" => metrics.total_memories = total,
                "graph_nodes_total" => metrics.graph_nodes_total = total,
                "graph_edges_total" => metrics.graph_edges_total = total,
                "snapshots_total" => metrics.snapshots_total = total,
                "branches_extra_total" => metrics.branches_extra_total = total,
                _ => {}
            }
        }

        for row in memory_type_rows {
            let bucket: String = row.try_get("bucket").map_err(db_err)?;
            let total = optional_i64(&row, "total");
            metrics.memory_counts.insert(bucket, total);
        }

        for row in feedback_rows {
            let bucket: String = row.try_get("bucket").map_err(db_err)?;
            let total = optional_i64(&row, "total");
            metrics.feedback_counts.insert(bucket, total);
        }

        Ok(metrics)
    }

    // ── Background worker ─────────────────────────────────────────────────

    pub fn spawn(self: Arc<Self>, shutdown: watch::Receiver<()>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move { self.run(shutdown).await })
    }

    async fn run(self: Arc<Self>, mut shutdown: watch::Receiver<()>) {
        let mut interval = tokio::time::interval(self.refresh_interval);
        loop {
            tokio::select! {
                _ = interval.tick() => {}
                _ = self.notify.notified() => {}
                _ = shutdown.changed() => break,
            }
            if let Err(e) = self.refresh_until_quiet().await {
                self.stats.failures_total.fetch_add(1, Ordering::Relaxed);
                self.stats
                    .last_failure_epoch_secs
                    .store(now_epoch_secs(), Ordering::Relaxed);
                warn!(error = %e, "metrics rollup refresh loop failed");
            }
        }
    }

    async fn refresh_until_quiet(&self) -> Result<(), MemoriaError> {
        loop {
            let refreshed = self.refresh_batch().await?;
            if refreshed == 0 {
                return Ok(());
            }
        }
    }

    // ── Phase 1: Claim ────────────────────────────────────────────────────

    /// Claim a batch of eligible users for refresh.
    ///
    /// Uses an atomic UPDATE-then-SELECT-by-token pattern: a single UPDATE
    /// stamps our unique claim_token on eligible rows (MySQL UPDATE is
    /// row-locked and atomic), then we SELECT back only the rows bearing
    /// our token.  Two concurrent workers will never claim the same rows.
    async fn claim_batch(&self) -> Result<Vec<ClaimedUser>, MemoriaError> {
        let token = uuid::Uuid::new_v4().to_string();
        let now = Utc::now().naive_utc();
        let claim_expires = now + chrono::Duration::seconds(120);

        // Atomic claim: single UPDATE stamps our token on eligible rows.
        let result = sqlx::query(
            r#"UPDATE mem_metrics_user_state SET
                   claim_token = ?, claim_expires_at = ?, updated_at = ?
               WHERE has_pending = 1
                 AND next_eligible_at <= ?
                 AND (claim_expires_at IS NULL OR claim_expires_at < ?)
               ORDER BY next_eligible_at
               LIMIT ?"#,
        )
        .bind(&token)
        .bind(claim_expires)
        .bind(now)
        .bind(now)
        .bind(now)
        .bind(self.batch_size as i64)
        .execute(&self.shared_pool)
        .await
        .map_err(db_err)?;

        if result.rows_affected() == 0 {
            return Ok(Vec::new());
        }

        // Read back the rows we just claimed, by our unique token.
        let rows = sqlx::query(
            r#"SELECT s.user_id, r.db_name, s.change_version, s.pending_mask
               FROM mem_metrics_user_state s
               INNER JOIN mem_user_registry r ON r.user_id = s.user_id AND r.status = 'active'
               WHERE s.claim_token = ?"#,
        )
        .bind(&token)
        .fetch_all(&self.shared_pool)
        .await
        .map_err(db_err)?;

        let mut claimed = Vec::with_capacity(rows.len());
        for row in &rows {
            claimed.push(ClaimedUser {
                user_id: row.try_get("user_id").map_err(db_err)?,
                db_name: row.try_get("db_name").map_err(db_err)?,
                claimed_version: row
                    .try_get::<u64, _>("change_version")
                    .or_else(|_| row.try_get::<i64, _>("change_version").map(|v| v as u64))
                    .map_err(db_err)?,
                claimed_mask: row
                    .try_get::<u64, _>("pending_mask")
                    .or_else(|_| row.try_get::<i64, _>("pending_mask").map(|v| v as u64))
                    .map_err(db_err)?,
            });
        }

        Ok(claimed)
    }

    // ── Phase 2: Compute ──────────────────────────────────────────────────

    /// Compute rollup entries for a single user.  Only families triggered by
    /// the claimed mask are computed.  Runs on the global user pool.
    async fn compute_user_rollups(
        &self,
        user: &ClaimedUser,
    ) -> Result<Vec<RollupEntry>, MemoriaError> {
        let mask = DirtyMask(user.claimed_mask);
        let families_needed: Vec<&FamilyDef> = FAMILY_REGISTRY
            .iter()
            .filter(|f| mask.contains(f.trigger) || mask.contains(DirtyMask::FULL))
            .collect();

        if families_needed.is_empty() {
            return Ok(Vec::new());
        }

        let store = self.get_user_store(&user.user_id, &user.db_name).await?;
        let mut entries = Vec::new();

        for fam in families_needed {
            match compute_family_for_user(fam.family, &user.user_id, &store).await {
                Ok(mut fam_entries) => entries.append(&mut fam_entries),
                Err(e) => {
                    warn!(
                        user_id = user.user_id,
                        family = fam.family,
                        error = %e,
                        "failed to compute family for user"
                    );
                    return Err(e);
                }
            }
        }

        Ok(entries)
    }

    async fn get_user_store(
        &self,
        user_id: &str,
        db_name: &str,
    ) -> Result<SqlMemoryStore, MemoriaError> {
        let router =
            self.service.db_router.as_ref().ok_or_else(|| {
                MemoriaError::Internal("metrics rollup requires db router".into())
            })?;
        match router.routed_store_for_db_name(db_name) {
            Ok(store) => Ok(store),
            Err(_) => {
                // Fallback: ensure user store exists (may trigger provisioning)
                let store = self.service.user_sql_store(user_id).await?;
                Ok(store.as_ref().clone())
            }
        }
    }

    // ── Phase 3: Flush rollups ────────────────────────────────────────────

    /// Flush computed rollup entries for a batch of users.
    ///
    /// Deletion scope is **per-family**: for each family, only the users whose
    /// `claimed_mask` triggers that family have their old rows deleted.  This
    /// prevents cross-family data loss when different users refresh different
    /// families in the same batch.  Users who triggered a family but produced
    /// zero new rows still get their old rows deleted (correct: they now have
    /// zero of that family).
    ///
    /// Each inserted row carries the user's `claimed_version` as
    /// `refreshed_version`, preserving the audit/reconciliation semantics.
    async fn flush_rollups(
        &self,
        batch: &[(ClaimedUser, Vec<RollupEntry>)],
    ) -> Result<(), MemoriaError> {
        if batch.is_empty() {
            return Ok(());
        }

        // Group entries by family, carrying user_id + claimed_version.
        let mut by_family: BTreeMap<&str, Vec<(&str, u64, &RollupEntry)>> = BTreeMap::new();
        for (user, entries) in batch {
            for entry in entries {
                by_family.entry(entry.family).or_default().push((
                    &user.user_id,
                    user.claimed_version,
                    entry,
                ));
            }
        }

        // Collect every family triggered by at least one user in the batch.
        let all_triggered_families: Vec<&str> = FAMILY_REGISTRY
            .iter()
            .filter(|fam| {
                batch
                    .iter()
                    .any(|(u, _)| DirtyMask(u.claimed_mask).contains(fam.trigger))
            })
            .map(|fam| fam.family)
            .collect();

        let now = Utc::now().naive_utc();

        for family in all_triggered_families {
            // Per-family user set: only users whose mask triggers this family.
            let refreshing_user_ids = users_refreshing_family(batch, family);
            if refreshing_user_ids.is_empty() {
                continue;
            }

            // DELETE existing rows for this family + only the users refreshing it.
            let user_placeholders: String = refreshing_user_ids
                .iter()
                .map(|_| "?")
                .collect::<Vec<_>>()
                .join(",");
            let delete_sql = format!(
                "DELETE FROM mem_metrics_user_rollups WHERE family = ? AND user_id IN ({user_placeholders})"
            );
            let mut del_query = sqlx::query(&delete_sql).bind(family);
            for uid in &refreshing_user_ids {
                del_query = del_query.bind(*uid);
            }
            del_query.execute(&self.shared_pool).await.map_err(db_err)?;

            // Chunked INSERT for entries in this family (if any).
            if let Some(entries) = by_family.get(family) {
                for chunk in entries.chunks(ROLLUP_FLUSH_CHUNK_ROWS) {
                    let value_placeholders: String = chunk
                        .iter()
                        .map(|_| "(?, ?, ?, ?, ?, ?)")
                        .collect::<Vec<_>>()
                        .join(",");
                    let insert_sql = format!(
                        "INSERT INTO mem_metrics_user_rollups (user_id, family, bucket, value, refreshed_version, updated_at) VALUES {value_placeholders}"
                    );
                    let mut ins_query = sqlx::query(&insert_sql);
                    for (uid, version, entry) in chunk {
                        ins_query = ins_query
                            .bind(*uid)
                            .bind(entry.family)
                            .bind(&entry.bucket)
                            .bind(entry.value)
                            .bind(*version)
                            .bind(now);
                    }
                    ins_query.execute(&self.shared_pool).await.map_err(db_err)?;
                }
            }
        }

        Ok(())
    }

    // ── Phase 4: Flush state ──────────────────────────────────────────────

    /// Update state table for a batch of refreshed users.
    ///
    /// Uses a chunked batch UPDATE with per-user CASE expressions to
    /// materially reduce shared-pool round-trips vs the previous per-user
    /// loop.  Version-protected CAS semantics are preserved:
    /// - `pending_mask` bits are cleared only when `change_version` still
    ///   equals the `claimed_version` (no new writes arrived).
    /// - `has_pending` is derived from the post-update `pending_mask`
    ///   (MySQL evaluates SET left-to-right).
    /// - `claim_token` / `claim_expires_at` are always cleared.
    async fn flush_state(
        &self,
        batch: &[(ClaimedUser, Vec<RollupEntry>)],
    ) -> Result<(), MemoriaError> {
        if batch.is_empty() {
            return Ok(());
        }

        let now = Utc::now().naive_utc();
        const STATE_FLUSH_CHUNK: usize = 50;

        for chunk in batch.chunks(STATE_FLUSH_CHUNK) {
            let mut pending_cases = String::new();
            let mut version_cases = String::new();
            let user_placeholders: String = chunk.iter().map(|_| "?").collect::<Vec<_>>().join(",");

            for _ in chunk {
                pending_cases
                    .push_str("WHEN user_id = ? AND change_version = ? THEN pending_mask & ~? ");
                version_cases.push_str("WHEN user_id = ? THEN ? ");
            }

            let sql = format!(
                r#"UPDATE mem_metrics_user_state SET
                       pending_mask = CASE {pending_cases} ELSE pending_mask END,
                       has_pending = IF(pending_mask = 0, 0, 1),
                       last_refreshed_version = CASE {version_cases} ELSE last_refreshed_version END,
                       refreshed_at = ?,
                       claim_token = NULL,
                       claim_expires_at = NULL,
                       updated_at = ?
                   WHERE user_id IN ({user_placeholders})"#
            );

            let mut query = sqlx::query(&sql);
            // Bind pending_mask CASE params (user_id, claimed_version, claimed_mask)
            for (user, _) in chunk {
                query = query
                    .bind(&user.user_id)
                    .bind(user.claimed_version)
                    .bind(user.claimed_mask);
            }
            // Bind last_refreshed_version CASE params (user_id, claimed_version)
            for (user, _) in chunk {
                query = query.bind(&user.user_id).bind(user.claimed_version);
            }
            // Bind refreshed_at + updated_at
            query = query.bind(now).bind(now);
            // Bind IN clause user_ids
            for (user, _) in chunk {
                query = query.bind(&user.user_id);
            }

            query.execute(&self.shared_pool).await.map_err(db_err)?;
        }

        Ok(())
    }

    // ── Orchestration ─────────────────────────────────────────────────────

    /// Run one batch: claim → compute → flush rollups → flush state.
    async fn refresh_batch(&self) -> Result<usize, MemoriaError> {
        // Phase 1: Claim
        let claimed = self.claim_batch().await?;
        if claimed.is_empty() {
            self.stats.inflight_users.store(0, Ordering::Relaxed);
            return Ok(0);
        }

        let batch_len = claimed.len();
        self.stats
            .inflight_users
            .store(batch_len as u64, Ordering::Relaxed);
        let started = Instant::now();

        // Phase 2: Compute with bounded concurrency
        let mut results: Vec<(ClaimedUser, Vec<RollupEntry>)> = Vec::with_capacity(batch_len);
        let mut join_set = tokio::task::JoinSet::new();
        let mut queued = claimed.into_iter();
        let mut succeeded = 0usize;

        loop {
            // Fill up to max_concurrency
            while join_set.len() < self.max_concurrency {
                let Some(user) = queued.next() else { break };
                let mgr = self.clone();
                join_set.spawn(async move {
                    let entries = mgr.compute_user_rollups(&user).await;
                    (user, entries)
                });
            }

            if join_set.is_empty() {
                break;
            }

            match join_set.join_next().await {
                Some(Ok((user, Ok(entries)))) => {
                    results.push((user, entries));
                    succeeded += 1;
                }
                Some(Ok((user, Err(e)))) => {
                    self.stats.failures_total.fetch_add(1, Ordering::Relaxed);
                    self.stats
                        .last_failure_epoch_secs
                        .store(now_epoch_secs(), Ordering::Relaxed);
                    // Record error in state
                    self.record_user_error(&user.user_id, "compute_failed")
                        .await;
                    warn!(
                        user_id = user.user_id,
                        error = %e,
                        "metrics rollup compute failed"
                    );
                }
                Some(Err(e)) => {
                    self.stats.failures_total.fetch_add(1, Ordering::Relaxed);
                    self.stats
                        .last_failure_epoch_secs
                        .store(now_epoch_secs(), Ordering::Relaxed);
                    warn!(error = %e, "metrics rollup compute task panicked");
                }
                None => break,
            }
        }

        if !results.is_empty() {
            // Phase 3: Flush rollups
            self.flush_rollups(&results).await?;

            // Phase 4: Flush state
            self.flush_state(&results).await?;

            self.invalidate_metrics_cache().await;
        }

        self.stats
            .last_duration_ms
            .store(started.elapsed().as_millis() as u64, Ordering::Relaxed);
        self.stats.inflight_users.store(0, Ordering::Relaxed);
        if succeeded > 0 {
            self.stats
                .last_success_epoch_secs
                .store(now_epoch_secs(), Ordering::Relaxed);
        }
        Ok(succeeded)
    }

    async fn record_user_error(&self, user_id: &str, error_kind: &str) {
        let now = Utc::now().naive_utc();
        let _ = sqlx::query(
            r#"UPDATE mem_metrics_user_state SET
                   last_error_kind = ?,
                   last_error_at = ?,
                   claim_token = NULL,
                   claim_expires_at = NULL,
                   updated_at = ?
               WHERE user_id = ?"#,
        )
        .bind(error_kind)
        .bind(now)
        .bind(now)
        .bind(user_id)
        .execute(&self.shared_pool)
        .await;
    }

    async fn invalidate_metrics_cache(&self) {
        let mut cache = self.metrics_cache.write().await;
        *cache = None;
    }
}

// ── Family computation ────────────────────────────────────────────────────────

/// Compute rollup entries for a single family from a user's DB.
async fn compute_family_for_user(
    family: &'static str,
    user_id: &str,
    store: &SqlMemoryStore,
) -> Result<Vec<RollupEntry>, MemoriaError> {
    match family {
        "memory_total" => {
            let total = sqlx::query_scalar::<_, i64>(&format!(
                "SELECT COUNT(*) FROM {} WHERE is_active > 0",
                store.t("mem_memories")
            ))
            .fetch_one(store.pool())
            .await
            .map_err(db_err)?;
            Ok(vec![RollupEntry {
                family,
                bucket: TOTAL_BUCKET.to_string(),
                value: total,
            }])
        }
        "memory_type" => {
            let rows = sqlx::query(&format!(
                "SELECT memory_type, COUNT(*) AS cnt FROM {} WHERE is_active > 0 GROUP BY memory_type",
                store.t("mem_memories")
            ))
            .fetch_all(store.pool())
            .await
            .map_err(db_err)?;
            rows.iter()
                .map(|row| {
                    Ok(RollupEntry {
                        family,
                        bucket: row.try_get::<String, _>("memory_type").map_err(db_err)?,
                        value: optional_i64(row, "cnt"),
                    })
                })
                .collect()
        }
        "feedback_signal" => {
            let rows = sqlx::query(&format!(
                "SELECT signal, COUNT(*) AS cnt FROM {} GROUP BY signal",
                store.t("mem_retrieval_feedback")
            ))
            .fetch_all(store.pool())
            .await
            .map_err(db_err)?;
            rows.iter()
                .map(|row| {
                    Ok(RollupEntry {
                        family,
                        bucket: row.try_get::<String, _>("signal").map_err(db_err)?,
                        value: optional_i64(row, "cnt"),
                    })
                })
                .collect()
        }
        "graph_nodes_total" => {
            let total = sqlx::query_scalar::<_, i64>(&format!(
                "SELECT COUNT(*) FROM {} WHERE is_active = 1",
                store.t("memory_graph_nodes")
            ))
            .fetch_one(store.pool())
            .await
            .map_err(db_err)?;
            Ok(vec![RollupEntry {
                family,
                bucket: TOTAL_BUCKET.to_string(),
                value: total,
            }])
        }
        "graph_edges_total" => {
            let total = sqlx::query_scalar::<_, i64>(&format!(
                "SELECT COUNT(*) FROM {}",
                store.t("memory_graph_edges")
            ))
            .fetch_one(store.pool())
            .await
            .map_err(db_err)?;
            Ok(vec![RollupEntry {
                family,
                bucket: TOTAL_BUCKET.to_string(),
                value: total,
            }])
        }
        "snapshots_total" => {
            let total = sqlx::query_scalar::<_, i64>(&format!(
                "SELECT COUNT(*) FROM {} WHERE user_id = ? AND status = 'active'",
                store.t("mem_snapshots")
            ))
            .bind(user_id)
            .fetch_one(store.pool())
            .await
            .map_err(db_err)?;
            Ok(vec![RollupEntry {
                family,
                bucket: TOTAL_BUCKET.to_string(),
                value: total,
            }])
        }
        "branches_extra_total" => {
            let total = sqlx::query_scalar::<_, i64>(&format!(
                "SELECT COUNT(*) FROM {} WHERE user_id = ? AND status = 'active'",
                store.t("mem_branches")
            ))
            .bind(user_id)
            .fetch_one(store.pool())
            .await
            .map_err(db_err)?;
            Ok(vec![RollupEntry {
                family,
                bucket: TOTAL_BUCKET.to_string(),
                value: total,
            }])
        }
        _ => Ok(Vec::new()),
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn db_err(e: impl std::fmt::Display) -> MemoriaError {
    MemoriaError::Database(e.to_string())
}

fn optional_i64(row: &sqlx::mysql::MySqlRow, column: &str) -> i64 {
    row.try_get::<Option<i64>, _>(column)
        .ok()
        .flatten()
        .unwrap_or(0)
}

fn now_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn age_from_epoch(epoch_secs: u64) -> Option<u64> {
    if epoch_secs == 0 {
        return None;
    }
    Some(now_epoch_secs().saturating_sub(epoch_secs))
}

/// Returns user IDs from the batch whose `claimed_mask` triggers the given
/// family.  This determines the per-family DELETE scope: only these users'
/// rows should be deleted for the given family, regardless of whether they
/// produced any new entries.
fn users_refreshing_family<'a>(
    batch: &'a [(ClaimedUser, Vec<RollupEntry>)],
    family: &str,
) -> Vec<&'a str> {
    let trigger = match FAMILY_REGISTRY.iter().find(|f| f.family == family) {
        Some(fam) => fam.trigger,
        None => return Vec::new(),
    };
    batch
        .iter()
        .filter(|(user, _)| {
            let mask = DirtyMask(user.claimed_mask);
            mask.contains(trigger) || mask.contains(DirtyMask::FULL)
        })
        .map(|(user, _)| user.user_id.as_str())
        .collect()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Returns the family names that would be triggered by the given dirty mask.
    fn families_for_mask(mask: u64) -> Vec<&'static str> {
        let m = DirtyMask(mask);
        FAMILY_REGISTRY
            .iter()
            .filter(|f| m.contains(f.trigger) || m.contains(DirtyMask::FULL))
            .map(|f| f.family)
            .collect()
    }

    #[test]
    fn dirty_mask_or_merges() {
        let m = DirtyMask::MEMORY | DirtyMask::GRAPH;
        assert!(m.contains(DirtyMask::MEMORY));
        assert!(m.contains(DirtyMask::GRAPH));
        assert!(!m.contains(DirtyMask::FEEDBACK));
    }

    #[test]
    fn dirty_mask_full_contains_all() {
        assert!(DirtyMask::FULL.contains(DirtyMask::MEMORY));
        assert!(DirtyMask::FULL.contains(DirtyMask::FEEDBACK));
        assert!(DirtyMask::FULL.contains(DirtyMask::GRAPH));
        assert!(DirtyMask::FULL.contains(DirtyMask::SNAPSHOT));
        assert!(DirtyMask::FULL.contains(DirtyMask::BRANCH));
    }

    #[test]
    fn dirty_mask_empty() {
        assert!(DirtyMask(0).is_empty());
        assert!(!DirtyMask::MEMORY.is_empty());
    }

    #[test]
    fn family_registry_covers_all_bits() {
        // Every non-FULL dirty bit should trigger at least one family.
        for mask in [
            DirtyMask::MEMORY,
            DirtyMask::FEEDBACK,
            DirtyMask::GRAPH,
            DirtyMask::SNAPSHOT,
            DirtyMask::BRANCH,
        ] {
            let triggered: Vec<_> = FAMILY_REGISTRY
                .iter()
                .filter(|f| mask.contains(f.trigger))
                .collect();
            assert!(
                !triggered.is_empty(),
                "no family triggered by mask {:?}",
                mask
            );
        }
    }

    #[test]
    fn family_registry_has_expected_families() {
        let names: Vec<&str> = FAMILY_REGISTRY.iter().map(|f| f.family).collect();
        assert!(names.contains(&"memory_total"));
        assert!(names.contains(&"memory_type"));
        assert!(names.contains(&"feedback_signal"));
        assert!(names.contains(&"graph_nodes_total"));
        assert!(names.contains(&"graph_edges_total"));
        assert!(names.contains(&"snapshots_total"));
        assert!(names.contains(&"branches_extra_total"));
    }

    #[test]
    fn labelled_families_are_correct() {
        for fam in FAMILY_REGISTRY {
            match fam.family {
                "memory_type" | "feedback_signal" => assert!(fam.is_labelled),
                _ => assert!(!fam.is_labelled),
            }
        }
    }

    #[test]
    fn age_from_epoch_zero_is_none() {
        assert_eq!(age_from_epoch(0), None);
    }

    #[test]
    fn age_from_epoch_recent_is_some() {
        let recent = now_epoch_secs().saturating_sub(5);
        let age = age_from_epoch(recent);
        assert!(age.is_some());
        assert!(age.unwrap() <= 10); // allow some slack
    }

    // ── Per-family deletion scope (issue #2) ──────────────────────────────

    fn make_user(id: &str, mask: DirtyMask, version: u64) -> ClaimedUser {
        ClaimedUser {
            user_id: id.to_string(),
            db_name: format!("db_{id}"),
            claimed_version: version,
            claimed_mask: mask.0,
        }
    }

    fn make_entry(family: &'static str, bucket: &str, value: i64) -> RollupEntry {
        RollupEntry {
            family,
            bucket: bucket.to_string(),
            value,
        }
    }

    #[test]
    fn families_for_mask_memory_only() {
        let fams = families_for_mask(DirtyMask::MEMORY.0);
        assert!(fams.contains(&"memory_total"));
        assert!(fams.contains(&"memory_type"));
        assert!(!fams.contains(&"feedback_signal"));
        assert!(!fams.contains(&"graph_nodes_total"));
    }

    #[test]
    fn families_for_mask_full_covers_all() {
        let fams = families_for_mask(DirtyMask::FULL.0);
        for reg in FAMILY_REGISTRY {
            assert!(
                fams.contains(&reg.family),
                "FULL mask should trigger {}",
                reg.family
            );
        }
    }

    #[test]
    fn families_for_mask_empty_yields_none() {
        let fams = families_for_mask(0);
        assert!(fams.is_empty());
    }

    #[test]
    fn users_refreshing_family_per_family_scope() {
        // User A: MEMORY mask → refreshes memory_total, memory_type
        // User B: FEEDBACK mask → refreshes feedback_signal only
        let batch: Vec<(ClaimedUser, Vec<RollupEntry>)> = vec![
            (
                make_user("A", DirtyMask::MEMORY, 1),
                vec![
                    make_entry("memory_total", "__total__", 10),
                    make_entry("memory_type", "core", 5),
                ],
            ),
            (
                make_user("B", DirtyMask::FEEDBACK, 2),
                vec![make_entry("feedback_signal", "positive", 3)],
            ),
        ];

        // memory_type is triggered by MEMORY → only user A
        assert_eq!(users_refreshing_family(&batch, "memory_type"), vec!["A"]);
        // feedback_signal is triggered by FEEDBACK → only user B
        assert_eq!(
            users_refreshing_family(&batch, "feedback_signal"),
            vec!["B"]
        );
        // memory_total is triggered by MEMORY → only user A
        assert_eq!(users_refreshing_family(&batch, "memory_total"), vec!["A"]);
        // graph_nodes_total is triggered by GRAPH → neither
        assert!(users_refreshing_family(&batch, "graph_nodes_total").is_empty());
    }

    #[test]
    fn users_refreshing_family_includes_zero_entry_users() {
        // User A has MEMORY mask but produces NO entries.
        // Their old rows should still be deleted (they now have zero of those families).
        let batch: Vec<(ClaimedUser, Vec<RollupEntry>)> =
            vec![(make_user("A", DirtyMask::MEMORY, 1), vec![])];

        let users = users_refreshing_family(&batch, "memory_type");
        assert_eq!(users, vec!["A"]);
        let users = users_refreshing_family(&batch, "memory_total");
        assert_eq!(users, vec!["A"]);
        // Should not appear for families they did not trigger
        assert!(users_refreshing_family(&batch, "feedback_signal").is_empty());
    }

    #[test]
    fn users_refreshing_family_full_mask_triggers_all() {
        let batch: Vec<(ClaimedUser, Vec<RollupEntry>)> =
            vec![(make_user("X", DirtyMask::FULL, 10), vec![])];

        for fam in FAMILY_REGISTRY {
            let users = users_refreshing_family(&batch, fam.family);
            assert_eq!(
                users,
                vec!["X"],
                "FULL mask user should appear for {}",
                fam.family
            );
        }
    }

    // ── Refreshed-version propagation (issue #3) ──────────────────────────

    #[test]
    fn flush_data_carries_claimed_version() {
        let batch: Vec<(ClaimedUser, Vec<RollupEntry>)> = vec![
            (
                make_user("A", DirtyMask::MEMORY, 42),
                vec![make_entry("memory_total", "__total__", 10)],
            ),
            (
                make_user("B", DirtyMask::FEEDBACK, 99),
                vec![make_entry("feedback_signal", "pos", 1)],
            ),
        ];

        // Group by family with version, matching the flush_rollups logic
        let mut by_family: BTreeMap<&str, Vec<(&str, u64, &RollupEntry)>> = BTreeMap::new();
        for (user, entries) in &batch {
            for entry in entries {
                by_family.entry(entry.family).or_default().push((
                    &user.user_id,
                    user.claimed_version,
                    entry,
                ));
            }
        }

        // memory_total → version 42 for user A
        let mem = by_family.get("memory_total").unwrap();
        assert_eq!(mem.len(), 1);
        assert_eq!(mem[0].0, "A");
        assert_eq!(mem[0].1, 42);

        // feedback_signal → version 99 for user B
        let fb = by_family.get("feedback_signal").unwrap();
        assert_eq!(fb.len(), 1);
        assert_eq!(fb[0].0, "B");
        assert_eq!(fb[0].1, 99);
    }

    #[test]
    fn flush_data_version_not_zero() {
        // Regression: the old code bound 0u64 for refreshed_version.
        // Verify that claimed_version propagates (and is non-zero when set).
        let batch: Vec<(ClaimedUser, Vec<RollupEntry>)> = vec![(
            make_user("U", DirtyMask::MEMORY, 7),
            vec![make_entry("memory_total", "__total__", 5)],
        )];

        let mut by_family: BTreeMap<&str, Vec<(&str, u64, &RollupEntry)>> = BTreeMap::new();
        for (user, entries) in &batch {
            for entry in entries {
                by_family.entry(entry.family).or_default().push((
                    &user.user_id,
                    user.claimed_version,
                    entry,
                ));
            }
        }

        for (_, entries) in &by_family {
            for &(_, version, _) in entries {
                assert_ne!(version, 0, "refreshed_version must not be hardcoded to 0");
            }
        }
    }

    // ── Cross-family regression scenario (issue #2 end-to-end) ────────────

    #[test]
    fn cross_family_deletion_does_not_leak() {
        // Scenario from the review: user A refreshes MEMORY, user B refreshes
        // FEEDBACK only.  The memory_type DELETE must NOT include user B.
        let batch: Vec<(ClaimedUser, Vec<RollupEntry>)> = vec![
            (
                make_user("A", DirtyMask::MEMORY, 1),
                vec![
                    make_entry("memory_total", "__total__", 10),
                    make_entry("memory_type", "core", 5),
                ],
            ),
            (
                make_user("B", DirtyMask::FEEDBACK, 2),
                vec![make_entry("feedback_signal", "positive", 3)],
            ),
        ];

        // For each MEMORY family, only A should be in the delete set
        for fam_name in ["memory_total", "memory_type"] {
            let users = users_refreshing_family(&batch, fam_name);
            assert!(
                !users.contains(&"B"),
                "user B must NOT be in the delete set for {fam_name}"
            );
            assert!(
                users.contains(&"A"),
                "user A must be in the delete set for {fam_name}"
            );
        }

        // For the FEEDBACK family, only B should be in the delete set
        let fb_users = users_refreshing_family(&batch, "feedback_signal");
        assert!(
            !fb_users.contains(&"A"),
            "user A must NOT be in the delete set for feedback_signal"
        );
        assert!(fb_users.contains(&"B"));
    }
}
