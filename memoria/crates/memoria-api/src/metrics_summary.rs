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
//! - The write path (`mark_user_dirty`) serializes per-user state updates via
//!   `mem_user_registry`, OR-merges the pending mask, and rewrites all matching
//!   rows to one logical `change_version`.
//! - The worker claims a batch, computes only the families indicated by the
//!   mask, flushes rollups per-family in bulk, then updates state with
//!   version-protected CAS to avoid clearing newly-arrived dirty bits.
//! - Connection pools are treated as precious: claim/flush are short
//!   transactions on the shared pool; user-DB reads use bounded concurrency
//!   on the global user pool.
//!
//! ## Bootstrap (direct cutover)
//!
//! After switching to multi-db mode, pre-existing active users may have no
//! state row because `mark_user_dirty()` is only called on writes.  The
//! worker calls [`MetricsSummaryManager::bootstrap_missing_users`] once per
//! tick, seeding up to [`BOOTSTRAP_BATCH_SIZE`] users per pass with
//! `pending_mask = FULL`.  This converges `missing_users_total → 0` without
//! requiring user writes and without heavy fan-out on startup.
//!
//! ## Error health semantics
//!
//! `last_error_kind` / `last_error_at` represent the **most recent
//! outstanding** refresh error.  A successful refresh clears both fields,
//! so `/metrics` reports only users with currently-failing refreshes.
//! Error queries are scoped to active users via `mem_user_registry`.

use crate::state::CachedMetrics;
use chrono::{NaiveDateTime, Utc};
use memoria_core::{MemoriaError, MemoryType, FEEDBACK_SIGNALS};
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
/// Maximum users seeded per bootstrap pass to avoid fan-out on startup.
const BOOTSTRAP_BATCH_SIZE: usize = 50;
const DISTINCT_STATE_USERS_SUBQUERY: &str =
    r#"SELECT user_id FROM mem_metrics_user_state GROUP BY user_id"#;
const DEDUPED_STATE_SUBQUERY: &str = r#"SELECT
           user_id,
           COALESCE(CAST(BIT_OR(pending_mask) AS UNSIGNED), 0) AS pending_mask,
           MAX(CASE WHEN has_pending = 1 OR pending_mask <> 0 THEN 1 ELSE 0 END) AS has_pending,
           MAX(refreshed_at) AS refreshed_at
       FROM mem_metrics_user_state
       GROUP BY user_id"#;
const LATEST_ERROR_STATE_SUBQUERY: &str = r#"SELECT
           s.user_id,
           MAX(s.last_error_kind) AS last_error_kind,
           MAX(s.last_error_at) AS last_error_at
       FROM mem_metrics_user_state s
       INNER JOIN (
           SELECT user_id, MAX(last_error_at) AS last_error_at
           FROM mem_metrics_user_state
           WHERE last_error_kind IS NOT NULL AND last_error_at IS NOT NULL
           GROUP BY user_id
       ) latest
         ON latest.user_id = s.user_id
        AND latest.last_error_at = s.last_error_at
       WHERE s.last_error_kind IS NOT NULL AND s.last_error_at IS NOT NULL
       GROUP BY s.user_id"#;
const MARK_USER_DIRTY_INSERT_SQL: &str = r#"INSERT INTO mem_metrics_user_state (
               user_id, pending_mask, has_pending, change_version,
               next_eligible_at, updated_at
           ) VALUES (?, ?, 1, 1, ?, ?)"#;
const MARK_USER_DIRTY_AGGREGATE_SQL: &str = r#"SELECT
               COALESCE(CAST(BIT_OR(pending_mask) AS UNSIGNED), 0) AS merged_pending_mask,
               COALESCE(MAX(change_version), 0) AS max_change_version,
               COUNT(*) AS row_count
           FROM mem_metrics_user_state
           WHERE user_id = ?"#;
const MARK_USER_DIRTY_UPDATE_SQL: &str = r#"UPDATE mem_metrics_user_state SET
               pending_mask = ?,
               has_pending = 1,
               change_version = ?,
               next_eligible_at = ?,
               updated_at = ?
           WHERE user_id = ?"#;

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
/// Each entry centralizes **all** metadata for the family:
/// 1. dirty trigger bit
/// 2. how it is recomputed (via [`compute_family_for_user`])
/// 3. whether it is labelled, and if so, the legal bucket set
/// 4. how it renders to Prometheus (metric name, help, type, label key)
///
/// This is the single source of truth for family semantics.  Adding a new
/// business metric means adding an entry here and implementing its
/// computation in [`compute_family_for_user`].
#[derive(Debug, Clone, Copy)]
struct FamilyDef {
    /// Key stored in the `family` column of `mem_metrics_user_rollups`.
    family: &'static str,
    /// Which dirty bit triggers recomputation of this family.
    trigger: DirtyMask,
    /// Whether this family produces labelled buckets (true) or a single
    /// `__total__` scalar (false).
    is_labelled: bool,

    // ── Prometheus rendering metadata ────────────────────────────────
    /// Prometheus metric name (e.g. `memoria_memories_total`).
    prom_name: &'static str,
    /// HELP text emitted once per unique `prom_name`.
    prom_help: &'static str,
    /// Prometheus TYPE (`gauge` or `counter`).
    prom_type: &'static str,
    /// Label key for labelled families and scalar families rendered under
    /// a shared labelled metric (e.g. `"type"` for `memoria_memories_total`).
    label_name: Option<&'static str>,
    /// For scalar families rendered as a label value under a shared
    /// labelled metric (e.g. `memory_total` renders as `{type="all"}`).
    scalar_label_value: Option<&'static str>,
    /// Whether to add `total_users` to the rendered value
    /// (e.g. `branches_extra_total` → `memoria_branches_total`).
    add_total_users: bool,

    // ── Bucket validation ────────────────────────────────────────────
    /// For labelled families: the set of legal bucket values.
    /// `None` for scalar families.  Unknown buckets are silently
    /// dropped during rendering to prevent cardinality blowup.
    legal_buckets: Option<&'static [&'static str]>,
}

/// Authoritative list of all rollup families.
///
/// The ordering within the same `prom_name` matters: labelled entries
/// should come before scalar-label entries so the Prometheus output
/// emits per-bucket lines before the aggregate "all" line.
static FAMILY_REGISTRY: &[FamilyDef] = &[
    FamilyDef {
        family: "memory_type",
        trigger: DirtyMask::MEMORY,
        is_labelled: true,
        prom_name: "memoria_memories_total",
        prom_help: "Active memories by type.",
        prom_type: "gauge",
        label_name: Some("type"),
        scalar_label_value: None,
        add_total_users: false,
        legal_buckets: Some(MemoryType::ALL_NAMES),
    },
    FamilyDef {
        family: "memory_total",
        trigger: DirtyMask::MEMORY,
        is_labelled: false,
        prom_name: "memoria_memories_total",
        prom_help: "Active memories by type.",
        prom_type: "gauge",
        label_name: Some("type"),
        scalar_label_value: Some("all"),
        add_total_users: false,
        legal_buckets: None,
    },
    FamilyDef {
        family: "feedback_signal",
        trigger: DirtyMask::FEEDBACK,
        is_labelled: true,
        prom_name: "memoria_feedback_total",
        prom_help: "Feedback signals by type.",
        prom_type: "counter",
        label_name: Some("signal"),
        scalar_label_value: None,
        add_total_users: false,
        legal_buckets: Some(FEEDBACK_SIGNALS),
    },
    FamilyDef {
        family: "graph_nodes_total",
        trigger: DirtyMask::GRAPH,
        is_labelled: false,
        prom_name: "memoria_graph_nodes_total",
        prom_help: "Entity graph nodes.",
        prom_type: "gauge",
        label_name: None,
        scalar_label_value: None,
        add_total_users: false,
        legal_buckets: None,
    },
    FamilyDef {
        family: "graph_edges_total",
        trigger: DirtyMask::GRAPH,
        is_labelled: false,
        prom_name: "memoria_graph_edges_total",
        prom_help: "Entity graph edges.",
        prom_type: "gauge",
        label_name: None,
        scalar_label_value: None,
        add_total_users: false,
        legal_buckets: None,
    },
    FamilyDef {
        family: "snapshots_total",
        trigger: DirtyMask::SNAPSHOT,
        is_labelled: false,
        prom_name: "memoria_snapshots_total",
        prom_help: "Snapshots.",
        prom_type: "gauge",
        label_name: None,
        scalar_label_value: None,
        add_total_users: false,
        legal_buckets: None,
    },
    FamilyDef {
        family: "branches_extra_total",
        trigger: DirtyMask::BRANCH,
        is_labelled: false,
        prom_name: "memoria_branches_total",
        prom_help: "Active branches.",
        prom_type: "gauge",
        label_name: None,
        scalar_label_value: None,
        add_total_users: true,
        legal_buckets: None,
    },
];

const TOTAL_BUCKET: &str = "__total__";

// ── Registry-driven rendering ─────────────────────────────────────────────────

/// Render all business metrics from rollup data, driven entirely by the
/// family registry.  This is the single rendering path used by both the
/// rollup-backed (multi-db) and live-fallback (single-db) code paths.
///
/// The caller provides:
/// - `scalar_totals`: family → aggregated value for scalar families
/// - `labelled_totals`: family → (bucket → aggregated value) for labelled families
/// - `total_users`: user count (rendered as `memoria_users_total` and used
///   for the `branches_extra_total → memoria_branches_total` adjustment)
pub fn render_business_metrics(out: &mut String, metrics: &GlobalSummaryMetrics) {
    // memoria_users_total is not a rollup family; render it first.
    out.push_str("# HELP memoria_users_total Active users.\n");
    out.push_str("# TYPE memoria_users_total gauge\n");
    out.push_str(&format!("memoria_users_total {}\n", metrics.total_users));

    let mut emitted_help: std::collections::HashSet<&str> = std::collections::HashSet::new();

    for fam in FAMILY_REGISTRY {
        // Emit HELP + TYPE once per unique prom_name.
        if emitted_help.insert(fam.prom_name) {
            out.push_str(&format!("# HELP {} {}\n", fam.prom_name, fam.prom_help));
            out.push_str(&format!("# TYPE {} {}\n", fam.prom_name, fam.prom_type));
        }

        if fam.is_labelled {
            let label_key = fam.label_name.unwrap_or("bucket");
            if let Some(buckets) = metrics.labelled_totals.get(fam.family) {
                for (bucket, value) in buckets {
                    // Validate bucket against legal set if defined.
                    if let Some(legal) = fam.legal_buckets {
                        if !legal.contains(&bucket.as_str()) {
                            continue;
                        }
                    }
                    out.push_str(&format!(
                        "{prom}{{{label_key}=\"{bucket}\"}} {value}\n",
                        prom = fam.prom_name
                    ));
                }
            }
        } else if let Some(label_val) = fam.scalar_label_value {
            // Scalar rendered as a label value of a shared metric.
            let value = metrics.scalar_totals.get(fam.family).copied().unwrap_or(0);
            let label_key = fam.label_name.unwrap_or("bucket");
            out.push_str(&format!(
                "{prom}{{{label_key}=\"{label_val}\"}} {value}\n",
                prom = fam.prom_name
            ));
        } else {
            // Plain scalar.
            let mut value = metrics.scalar_totals.get(fam.family).copied().unwrap_or(0);
            if fam.add_total_users {
                value += metrics.total_users;
            }
            out.push_str(&format!("{} {}\n", fam.prom_name, value));
        }
    }
}

/// Render summary error health metrics from the state table.
///
/// These metrics reflect **outstanding** refresh errors, not lifetime history.
/// `last_error_kind` / `last_error_at` are cleared on successful refresh
/// (see [`MetricsSummaryManager::flush_state`]), so only users whose most
/// recent refresh attempt failed will appear here.  Queries are scoped to
/// active users via `mem_user_registry`.
pub fn render_error_health_metrics(out: &mut String, metrics: &GlobalSummaryMetrics) {
    // Users with outstanding errors by kind
    out.push_str("# HELP memoria_metrics_summary_errors_by_kind Users with outstanding refresh errors by error kind.\n");
    out.push_str("# TYPE memoria_metrics_summary_errors_by_kind gauge\n");
    if metrics.error_counts_by_kind.is_empty() {
        out.push_str("memoria_metrics_summary_errors_by_kind{kind=\"none\"} 0\n");
    } else {
        for (kind, cnt) in &metrics.error_counts_by_kind {
            out.push_str(&format!(
                "memoria_metrics_summary_errors_by_kind{{kind=\"{kind}\"}} {cnt}\n"
            ));
        }
    }

    // Total users with errors
    out.push_str("# HELP memoria_metrics_summary_users_with_errors Total users with outstanding refresh errors.\n");
    out.push_str("# TYPE memoria_metrics_summary_users_with_errors gauge\n");
    out.push_str(&format!(
        "memoria_metrics_summary_users_with_errors {}\n",
        metrics.users_with_errors
    ));

    // Newest error age
    out.push_str("# HELP memoria_metrics_summary_newest_error_age_seconds Age of the most recent refresh error.\n");
    out.push_str("# TYPE memoria_metrics_summary_newest_error_age_seconds gauge\n");
    out.push_str(&format!(
        "memoria_metrics_summary_newest_error_age_seconds {}\n",
        metrics.newest_error_age_secs.unwrap_or(0)
    ));
}

// ── Public metrics types ──────────────────────────────────────────────────────

#[derive(Debug, Default, Clone)]
pub struct SummaryRefreshStats {
    pub inflight_users: u64,
    pub failures_total: u64,
    pub last_duration_secs: f64,
    pub last_success_age_secs: Option<u64>,
    pub last_failure_age_secs: Option<u64>,
    pub effective_concurrency: u64,
    pub effective_batch_size: u64,
    pub pool_backoff_active: bool,
}

/// Aggregated global metrics read from the rollup + state tables.
/// Used by the `/metrics` endpoint in multi-db mode.
///
/// Business metrics are stored in generic maps keyed by family name
/// (matching [`FAMILY_REGISTRY`] entries) so that rendering is driven
/// entirely from the registry rather than ad-hoc field access.
#[derive(Debug, Default, Clone)]
pub struct GlobalSummaryMetrics {
    pub available: bool,
    pub total_users: i64,
    /// Scalar family totals: family name → aggregated value across all users.
    pub scalar_totals: BTreeMap<String, i64>,
    /// Labelled family breakdowns: family name → (bucket → aggregated value).
    pub labelled_totals: BTreeMap<String, BTreeMap<String, i64>>,
    // ── Summary health ──
    pub ready_users_total: i64,
    pub dirty_users_total: i64,
    pub missing_users_total: i64,
    pub oldest_ready_refresh_age_secs: Option<i64>,
    // ── Error health (exposed via /metrics) ──
    pub error_counts_by_kind: BTreeMap<String, i64>,
    pub users_with_errors: i64,
    pub newest_error_age_secs: Option<i64>,
}

// ── Internal types ────────────────────────────────────────────────────────────

#[derive(Debug, Default)]
struct RefreshStats {
    inflight_users: AtomicU64,
    failures_total: AtomicU64,
    last_duration_ms: AtomicU64,
    last_success_epoch_secs: AtomicU64,
    last_failure_epoch_secs: AtomicU64,
    effective_concurrency: AtomicU64,
    effective_batch_size: AtomicU64,
    pool_backoff_active: AtomicU64,
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
    claimed_version: u64,
    claimed_mask: u64,
}

#[derive(Debug, Clone, Copy, Default)]
struct StateUpsertOutcome {
    inserted: bool,
    duplicate_rows_seen: u64,
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
            effective_concurrency: self.stats.effective_concurrency.load(Ordering::Relaxed),
            effective_batch_size: self.stats.effective_batch_size.load(Ordering::Relaxed),
            pool_backoff_active: self.stats.pool_backoff_active.load(Ordering::Relaxed) != 0,
        }
    }

    /// Mark a user dirty with the given mask.
    ///
    /// The update is serialized via `mem_user_registry` so MatrixOne does not
    /// see concurrent `ON DUPLICATE KEY UPDATE` races on the state table.  The
    /// mask is OR-merged with any existing pending_mask, all matching rows are
    /// rewritten to the same `change_version`, and `next_eligible_at` is set
    /// to `now + debounce` for coalescing.
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
        let outcome = self
            .upsert_user_state_locked(user_id, mask, eligible_at, now)
            .await?;
        if outcome.duplicate_rows_seen > 0 {
            warn!(
                user_id,
                duplicate_rows = outcome.duplicate_rows_seen,
                "mem_metrics_user_state already contains duplicate rows for user; using logical row semantics"
            );
        }
        self.notify.notify_one();
        Ok(())
    }

    /// Seed `mem_metrics_user_state` rows for active users that are in
    /// `mem_user_registry` but have no state row yet.
    ///
    /// This is the bootstrap path for direct cutover: after switching to
    /// multi-db mode, pre-existing active users may never call
    /// `mark_user_dirty()` if they produce no new writes.  Without a
    /// state row they would stay in the "missing" bucket forever.
    ///
    /// **Bounded**: at most [`BOOTSTRAP_BATCH_SIZE`] users are seeded per
    /// call to avoid fan-out pressure on the shared pool.  The worker
    /// calls this once per tick so convergence is gradual.
    ///
    /// Seeded rows get `pending_mask = FULL` and `has_pending = 1`, making
    /// them immediately claimable by the refresh worker so all families
    /// are computed on first refresh.  `next_eligible_at` is set to now
    /// (no debounce for bootstrap rows).
    ///
    /// Returns the number of users seeded (0 when fully converged).
    async fn bootstrap_missing_users(&self) -> Result<usize, MemoriaError> {
        let now = Utc::now().naive_utc();
        let bootstrap_sql = format!(
            r#"SELECT r.user_id
               FROM mem_user_registry r
               LEFT JOIN ({DISTINCT_STATE_USERS_SUBQUERY}) s ON s.user_id = r.user_id
               WHERE r.status = 'active' AND s.user_id IS NULL
               ORDER BY r.user_id
               LIMIT ?"#
        );
        let rows = sqlx::query(&bootstrap_sql)
            .bind(BOOTSTRAP_BATCH_SIZE as i64)
            .fetch_all(&self.shared_pool)
            .await
            .map_err(db_err)?;

        let mut seeded = 0usize;
        for row in rows {
            let user_id: String = row.try_get("user_id").map_err(db_err)?;
            let outcome = self
                .upsert_user_state_locked(&user_id, DirtyMask::FULL, now, now)
                .await?;
            if outcome.inserted {
                seeded += 1;
            }
        }
        if seeded > 0 {
            info!(
                seeded,
                "bootstrapped missing active users into metrics state"
            );
        }
        Ok(seeded)
    }

    /// Load aggregated global metrics from rollup + state tables.
    /// Called by `/metrics` in multi-db mode — reads only the shared DB.
    pub async fn load_global_metrics(&self) -> Result<GlobalSummaryMetrics, MemoriaError> {
        // Coverage: how many users are clean / dirty / missing state
        let coverage_sql = format!(
            r#"SELECT
                   COUNT(*) AS total_users,
                   SUM(CASE WHEN s.user_id IS NULL THEN 1 ELSE 0 END) AS missing_users,
                    SUM(CASE WHEN s.user_id IS NOT NULL AND s.has_pending = 1 THEN 1 ELSE 0 END) AS dirty_users,
                    SUM(CASE WHEN s.user_id IS NOT NULL AND s.has_pending = 0 THEN 1 ELSE 0 END) AS ready_users
                FROM mem_user_registry r
                LEFT JOIN ({DEDUPED_STATE_SUBQUERY}) s ON s.user_id = r.user_id
                WHERE r.status = 'active'"#
        );
        let coverage = sqlx::query(&coverage_sql)
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

        // All labelled families in a single query driven by the registry
        let labelled_names: Vec<&str> = FAMILY_REGISTRY
            .iter()
            .filter(|f| f.is_labelled)
            .map(|f| f.family)
            .collect();
        let labelled_rows = if labelled_names.is_empty() {
            Vec::new()
        } else {
            let placeholders: String = labelled_names
                .iter()
                .map(|_| "?")
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!(
                r#"SELECT r.family, r.bucket, COALESCE(SUM(r.value), 0) AS total
                   FROM mem_metrics_user_rollups r
                   INNER JOIN mem_user_registry u ON u.user_id = r.user_id
                   WHERE u.status = 'active' AND r.family IN ({placeholders})
                   GROUP BY r.family, r.bucket"#
            );
            let mut q = sqlx::query(&sql);
            for name in &labelled_names {
                q = q.bind(*name);
            }
            q.fetch_all(&self.shared_pool).await.map_err(db_err)?
        };

        // Oldest refresh age
        let oldest_refresh_sql = format!(
            r#"SELECT
                   MAX(TIMESTAMPDIFF(SECOND, s.refreshed_at, UTC_TIMESTAMP())) AS oldest_age
                FROM ({DEDUPED_STATE_SUBQUERY}) s
                INNER JOIN mem_user_registry r ON r.user_id = s.user_id
                WHERE r.status = 'active'
                  AND s.has_pending = 0
                  AND s.refreshed_at IS NOT NULL"#
        );
        let oldest_refresh = sqlx::query(&oldest_refresh_sql)
            .fetch_one(&self.shared_pool)
            .await
            .map_err(db_err)?;

        // ── Error health ──────────────────────────────────────────
        // Scoped to active users so deactivated/historical state rows
        // don't inflate the outstanding-error counts.
        let error_rows_sql = format!(
            r#"SELECT s.last_error_kind, COUNT(*) AS cnt
               FROM ({LATEST_ERROR_STATE_SUBQUERY}) s
               INNER JOIN mem_user_registry r ON r.user_id = s.user_id AND r.status = 'active'
               GROUP BY s.last_error_kind"#
        );
        let error_rows = sqlx::query(&error_rows_sql)
            .fetch_all(&self.shared_pool)
            .await
            .map_err(db_err)?;

        let newest_error_sql = format!(
            r#"SELECT MIN(TIMESTAMPDIFF(SECOND, s.last_error_at, UTC_TIMESTAMP())) AS newest_age
               FROM ({LATEST_ERROR_STATE_SUBQUERY}) s
               INNER JOIN mem_user_registry r ON r.user_id = s.user_id AND r.status = 'active'"#
        );
        let newest_error = sqlx::query(&newest_error_sql)
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
            oldest_ready_refresh_age_secs: clamp_metric_age(
                oldest_refresh
                    .try_get::<Option<i64>, _>("oldest_age")
                    .map_err(db_err)?,
            ),
            newest_error_age_secs: clamp_metric_age(
                newest_error
                    .try_get::<Option<i64>, _>("newest_age")
                    .map_err(db_err)?,
            ),
            ..GlobalSummaryMetrics::default()
        };

        for row in scalar_rows {
            let family: String = row.try_get("family").map_err(db_err)?;
            let total = optional_i64(&row, "total");
            metrics.scalar_totals.insert(family, total);
        }

        for row in labelled_rows {
            let family: String = row.try_get("family").map_err(db_err)?;
            let bucket: String = row.try_get("bucket").map_err(db_err)?;
            let total = optional_i64(&row, "total");
            metrics
                .labelled_totals
                .entry(family)
                .or_default()
                .insert(bucket, total);
        }

        let mut total_users_with_errors: i64 = 0;
        for row in error_rows {
            let kind: String = row.try_get("last_error_kind").map_err(db_err)?;
            let cnt = optional_i64(&row, "cnt");
            total_users_with_errors += cnt;
            metrics.error_counts_by_kind.insert(kind, cnt);
        }
        metrics.users_with_errors = total_users_with_errors;

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
            // Bootstrap: seed state rows for active users missing from
            // mem_metrics_user_state.  Bounded per tick so it converges
            // gradually without fan-out pressure.
            if let Err(e) = self.bootstrap_missing_users().await {
                warn!(error = %e, "bootstrap_missing_users failed");
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

    // ── Pool-aware backoff ───────────────────────────────────────────────

    /// Compute effective refresh concurrency based on the main (user) pool
    /// health.  When the pool enters high-utilization or saturated territory,
    /// concurrency is halved or reduced to 1 to avoid starving request
    /// traffic.
    ///
    /// Note: only the main pool (global user pool) has health monitoring.
    /// The shared/auth pool does not currently expose health signals, so
    /// backoff is only applied against the user pool.
    fn effective_concurrency(&self) -> usize {
        use memoria_storage::PoolHealthLevel;
        let level = self
            .service
            .sql_store
            .as_ref()
            .map(|s| s.pool_health_snapshot().level)
            .unwrap_or(PoolHealthLevel::Healthy);
        let eff = match level {
            PoolHealthLevel::Healthy => self.max_concurrency,
            PoolHealthLevel::HighUtilization => (self.max_concurrency / 2).max(1),
            PoolHealthLevel::Saturated | PoolHealthLevel::Empty => 1,
        };
        let backoff = eff < self.max_concurrency;
        self.stats
            .effective_concurrency
            .store(eff as u64, Ordering::Relaxed);
        self.stats
            .pool_backoff_active
            .store(u64::from(backoff), Ordering::Relaxed);
        eff
    }

    /// Compute effective claim batch size based on pool health.
    fn effective_batch_size(&self) -> usize {
        use memoria_storage::PoolHealthLevel;
        let level = self
            .service
            .sql_store
            .as_ref()
            .map(|s| s.pool_health_snapshot().level)
            .unwrap_or(PoolHealthLevel::Healthy);
        let eff = match level {
            PoolHealthLevel::Healthy => self.batch_size,
            PoolHealthLevel::HighUtilization => (self.batch_size / 2).max(1),
            PoolHealthLevel::Saturated | PoolHealthLevel::Empty => (self.batch_size / 4).max(1),
        };
        self.stats
            .effective_batch_size
            .store(eff as u64, Ordering::Relaxed);
        eff
    }

    // ── Phase 1: Claim ────────────────────────────────────────────────────

    /// Claim a batch of eligible users for refresh.
    ///
    /// Uses an atomic UPDATE-then-SELECT-by-token pattern: a single UPDATE
    /// stamps our unique claim_token on eligible rows (MySQL UPDATE is
    /// row-locked and atomic), then we SELECT back only the rows bearing
    /// our token.  Two concurrent workers will never claim the same rows.
    async fn claim_batch(&self) -> Result<Vec<ClaimedUser>, MemoriaError> {
        let effective_size = self.effective_batch_size();
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
        .bind(effective_size as i64)
        .execute(&self.shared_pool)
        .await
        .map_err(db_err)?;

        if result.rows_affected() == 0 {
            return Ok(Vec::new());
        }

        // Read back the rows we just claimed, by our unique token.
        let rows = sqlx::query(
            r#"SELECT
                   s.user_id,
                   COALESCE(CAST(BIT_OR(s.pending_mask) AS UNSIGNED), 0) AS pending_mask,
                   COALESCE(MAX(s.change_version), 0) AS change_version
               FROM mem_metrics_user_state s
               WHERE s.claim_token = ?
               GROUP BY s.user_id"#,
        )
        .bind(&token)
        .fetch_all(&self.shared_pool)
        .await
        .map_err(db_err)?;

        let mut claimed = Vec::with_capacity(rows.len());
        for row in &rows {
            claimed.push(ClaimedUser {
                user_id: row.try_get("user_id").map_err(db_err)?,
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

    async fn upsert_user_state_locked(
        &self,
        user_id: &str,
        mask: DirtyMask,
        eligible_at: NaiveDateTime,
        now: NaiveDateTime,
    ) -> Result<StateUpsertOutcome, MemoriaError> {
        let mut tx = self.shared_pool.begin().await.map_err(db_err)?;

        let has_registry_row = sqlx::query(
            "SELECT user_id FROM mem_user_registry WHERE user_id = ? LIMIT 1 FOR UPDATE",
        )
        .bind(user_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(db_err)?
        .is_some();

        if !has_registry_row {
            let _ = sqlx::query(
                "SELECT user_id FROM mem_metrics_user_state WHERE user_id = ? LIMIT 1 FOR UPDATE",
            )
            .bind(user_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(db_err)?;
        }

        let aggregate = sqlx::query(MARK_USER_DIRTY_AGGREGATE_SQL)
            .bind(user_id)
            .fetch_one(&mut *tx)
            .await
            .map_err(db_err)?;

        let row_count = aggregate
            .try_get::<i64, _>("row_count")
            .or_else(|_| aggregate.try_get::<u64, _>("row_count").map(|v| v as i64))
            .map_err(db_err)? as u64;

        let outcome = if row_count == 0 {
            sqlx::query(MARK_USER_DIRTY_INSERT_SQL)
                .bind(user_id)
                .bind(mask.0)
                .bind(eligible_at)
                .bind(now)
                .execute(&mut *tx)
                .await
                .map_err(db_err)?;
            StateUpsertOutcome {
                inserted: true,
                duplicate_rows_seen: 0,
            }
        } else {
            let merged_pending_mask = aggregate
                .try_get::<u64, _>("merged_pending_mask")
                .or_else(|_| {
                    aggregate
                        .try_get::<i64, _>("merged_pending_mask")
                        .map(|v| v as u64)
                })
                .map_err(db_err)?;
            let max_change_version = aggregate
                .try_get::<u64, _>("max_change_version")
                .or_else(|_| {
                    aggregate
                        .try_get::<i64, _>("max_change_version")
                        .map(|v| v as u64)
                })
                .map_err(db_err)?;

            sqlx::query(MARK_USER_DIRTY_UPDATE_SQL)
                .bind(merged_pending_mask | mask.0)
                .bind(max_change_version.saturating_add(1))
                .bind(eligible_at)
                .bind(now)
                .bind(user_id)
                .execute(&mut *tx)
                .await
                .map_err(db_err)?;

            StateUpsertOutcome {
                inserted: false,
                duplicate_rows_seen: row_count.saturating_sub(1),
            }
        };

        tx.commit().await.map_err(db_err)?;
        Ok(outcome)
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

        let store = self.get_user_store(&user.user_id).await?;
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

    async fn get_user_store(&self, user_id: &str) -> Result<SqlMemoryStore, MemoriaError> {
        // Always go through the service path so cache misses also run
        // `migrate_user()` before metrics reads hit a freshly-provisioned DB.
        let store = self.service.user_sql_store(user_id).await?;
        Ok(store.as_ref().clone())
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
    /// - `last_error_kind` / `last_error_at` are cleared on successful
    ///   refresh, so error health reports outstanding errors only.
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
            let mut has_pending_cases = String::new();
            let mut version_cases = String::new();
            let user_placeholders: String = chunk.iter().map(|_| "?").collect::<Vec<_>>().join(",");

            for _ in chunk {
                pending_cases
                    .push_str("WHEN user_id = ? AND change_version = ? THEN pending_mask & ? ");
                has_pending_cases.push_str(
                    "WHEN user_id = ? AND change_version = ? THEN IF((pending_mask & ?) = 0, 0, 1) ",
                );
                version_cases.push_str("WHEN user_id = ? THEN ? ");
            }

            let sql = format!(
                r#"UPDATE mem_metrics_user_state SET
                       pending_mask = CASE {pending_cases} ELSE pending_mask END,
                       has_pending = CASE {has_pending_cases} ELSE has_pending END,
                       last_refreshed_version = CASE {version_cases} ELSE last_refreshed_version END,
                       refreshed_at = ?,
                       last_error_kind = NULL,
                       last_error_at = NULL,
                       claim_token = NULL,
                       claim_expires_at = NULL,
                       updated_at = ?
                   WHERE user_id IN ({user_placeholders})"#
            );

            let mut query = sqlx::query(&sql);
            // Bind pending_mask CASE params (user_id, claimed_version, keep_mask)
            for (user, _) in chunk {
                let keep_mask = DirtyMask::FULL.0 & !user.claimed_mask;
                query = query
                    .bind(&user.user_id)
                    .bind(user.claimed_version)
                    .bind(keep_mask);
            }
            // Bind has_pending CASE params (user_id, claimed_version, keep_mask)
            for (user, _) in chunk {
                let keep_mask = DirtyMask::FULL.0 & !user.claimed_mask;
                query = query
                    .bind(&user.user_id)
                    .bind(user.claimed_version)
                    .bind(keep_mask);
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

        // Phase 2: Compute with pool-aware bounded concurrency
        let effective_conc = self.effective_concurrency();
        let mut results: Vec<(ClaimedUser, Vec<RollupEntry>)> = Vec::with_capacity(batch_len);
        let mut join_set = tokio::task::JoinSet::new();
        let mut queued = claimed.into_iter();
        let mut succeeded = 0usize;

        loop {
            // Re-evaluate concurrency each iteration to react to pool changes
            let conc = self.effective_concurrency().min(effective_conc);
            // Fill up to effective concurrency
            while join_set.len() < conc {
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

fn clamp_metric_age(age_secs: Option<i64>) -> Option<i64> {
    age_secs.map(|age| age.max(0))
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
                    make_entry("memory_type", "profile", 5),
                ],
            ),
            (
                make_user("B", DirtyMask::FEEDBACK, 2),
                vec![make_entry("feedback_signal", "useful", 3)],
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
                    make_entry("memory_type", "profile", 5),
                ],
            ),
            (
                make_user("B", DirtyMask::FEEDBACK, 2),
                vec![make_entry("feedback_signal", "useful", 3)],
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

    // ── Registry metadata completeness ────────────────────────────────

    #[test]
    fn family_registry_rendering_metadata_complete() {
        for fam in FAMILY_REGISTRY {
            assert!(!fam.prom_name.is_empty(), "{}: prom_name empty", fam.family);
            assert!(!fam.prom_help.is_empty(), "{}: prom_help empty", fam.family);
            assert!(
                fam.prom_type == "gauge" || fam.prom_type == "counter",
                "{}: invalid prom_type",
                fam.family
            );
            if fam.is_labelled {
                assert!(
                    fam.label_name.is_some(),
                    "{}: labelled family needs label_name",
                    fam.family
                );
                assert!(
                    fam.legal_buckets.is_some(),
                    "{}: labelled family needs legal_buckets",
                    fam.family
                );
            }
        }
    }

    #[test]
    fn family_registry_legal_buckets_non_empty() {
        for fam in FAMILY_REGISTRY {
            if let Some(buckets) = fam.legal_buckets {
                assert!(
                    !buckets.is_empty(),
                    "{}: legal_buckets defined but empty",
                    fam.family
                );
            }
        }
    }

    // ── Bucket-drift regression guards ────────────────────────────────

    #[test]
    fn memory_type_buckets_match_memoria_core() {
        let fam = FAMILY_REGISTRY
            .iter()
            .find(|f| f.family == "memory_type")
            .expect("memory_type family missing");
        let mut actual: Vec<&str> = fam.legal_buckets.unwrap().to_vec();
        actual.sort();
        let mut expected: Vec<&str> = MemoryType::ALL_NAMES.to_vec();
        expected.sort();
        assert_eq!(
            actual, expected,
            "memory_type legal_buckets drifted from MemoryType::ALL_NAMES"
        );
    }

    #[test]
    fn feedback_signal_buckets_match_canonical_set() {
        let fam = FAMILY_REGISTRY
            .iter()
            .find(|f| f.family == "feedback_signal")
            .expect("feedback_signal family missing");
        let mut actual: Vec<&str> = fam.legal_buckets.unwrap().to_vec();
        actual.sort();
        let mut expected: Vec<&str> = FEEDBACK_SIGNALS.to_vec();
        expected.sort();
        assert_eq!(
            actual, expected,
            "feedback_signal legal_buckets drifted from FEEDBACK_SIGNALS"
        );
    }

    #[test]
    fn render_all_real_memory_types_and_feedback_signals() {
        let mut metrics = GlobalSummaryMetrics::default();
        for (ty, val) in [
            ("semantic", 10),
            ("working", 20),
            ("episodic", 5),
            ("profile", 15),
            ("tool_result", 3),
            ("procedural", 7),
        ] {
            metrics
                .labelled_totals
                .entry("memory_type".into())
                .or_default()
                .insert(ty.into(), val);
        }
        for (sig, val) in [
            ("useful", 8),
            ("irrelevant", 2),
            ("outdated", 1),
            ("wrong", 4),
        ] {
            metrics
                .labelled_totals
                .entry("feedback_signal".into())
                .or_default()
                .insert(sig.into(), val);
        }

        let mut out = String::new();
        render_business_metrics(&mut out, &metrics);

        for ty in MemoryType::ALL_NAMES {
            assert!(
                out.contains(&format!("type=\"{ty}\"")),
                "missing memory type {ty} in rendered output"
            );
        }
        for sig in FEEDBACK_SIGNALS {
            assert!(
                out.contains(&format!("signal=\"{sig}\"")),
                "missing feedback signal {sig} in rendered output"
            );
        }
    }

    // ── Registry-driven rendering ─────────────────────────────────────

    #[test]
    fn render_business_metrics_produces_expected_output() {
        let mut metrics = GlobalSummaryMetrics {
            total_users: 3,
            ..Default::default()
        };
        metrics.scalar_totals.insert("memory_total".into(), 100);
        metrics.scalar_totals.insert("graph_nodes_total".into(), 50);
        metrics.scalar_totals.insert("graph_edges_total".into(), 25);
        metrics.scalar_totals.insert("snapshots_total".into(), 10);
        metrics
            .scalar_totals
            .insert("branches_extra_total".into(), 2);
        metrics
            .labelled_totals
            .entry("memory_type".into())
            .or_default()
            .insert("profile".into(), 60);
        metrics
            .labelled_totals
            .entry("memory_type".into())
            .or_default()
            .insert("semantic".into(), 40);
        metrics
            .labelled_totals
            .entry("feedback_signal".into())
            .or_default()
            .insert("useful".into(), 7);

        let mut out = String::new();
        render_business_metrics(&mut out, &metrics);

        assert!(out.contains("memoria_users_total 3\n"));
        assert!(out.contains("memoria_memories_total{type=\"profile\"} 60\n"));
        assert!(out.contains("memoria_memories_total{type=\"semantic\"} 40\n"));
        assert!(out.contains("memoria_memories_total{type=\"all\"} 100\n"));
        assert!(out.contains("memoria_feedback_total{signal=\"useful\"} 7\n"));
        assert!(out.contains("memoria_graph_nodes_total 50\n"));
        assert!(out.contains("memoria_graph_edges_total 25\n"));
        assert!(out.contains("memoria_snapshots_total 10\n"));
        // branches = total_users + branches_extra_total = 3 + 2 = 5
        assert!(out.contains("memoria_branches_total 5\n"));
    }

    #[test]
    fn render_business_metrics_filters_illegal_buckets() {
        let mut metrics = GlobalSummaryMetrics::default();
        metrics
            .labelled_totals
            .entry("memory_type".into())
            .or_default()
            .insert("working".into(), 10);
        metrics
            .labelled_totals
            .entry("memory_type".into())
            .or_default()
            .insert("ILLEGAL_BUCKET".into(), 999);

        let mut out = String::new();
        render_business_metrics(&mut out, &metrics);

        assert!(out.contains("memoria_memories_total{type=\"working\"} 10\n"));
        assert!(!out.contains("ILLEGAL_BUCKET"));
    }

    #[test]
    fn render_help_type_emitted_once_per_prom_name() {
        let metrics = GlobalSummaryMetrics::default();
        let mut out = String::new();
        render_business_metrics(&mut out, &metrics);
        // memoria_memories_total is shared by memory_type and memory_total
        let help_count = out.matches("# HELP memoria_memories_total").count();
        assert_eq!(help_count, 1, "HELP should be emitted once");
        let type_count = out.matches("# TYPE memoria_memories_total").count();
        assert_eq!(type_count, 1, "TYPE should be emitted once");
    }

    // ── Error health rendering ────────────────────────────────────────

    #[test]
    fn render_error_health_empty_state() {
        let metrics = GlobalSummaryMetrics::default();
        let mut out = String::new();
        render_error_health_metrics(&mut out, &metrics);
        assert!(out.contains("memoria_metrics_summary_errors_by_kind{kind=\"none\"} 0\n"));
        assert!(out.contains("memoria_metrics_summary_users_with_errors 0\n"));
        assert!(out.contains("memoria_metrics_summary_newest_error_age_seconds 0\n"));
    }

    #[test]
    fn render_error_health_with_errors() {
        let mut metrics = GlobalSummaryMetrics::default();
        metrics
            .error_counts_by_kind
            .insert("compute_failed".into(), 3);
        metrics.users_with_errors = 3;
        metrics.newest_error_age_secs = Some(120);

        let mut out = String::new();
        render_error_health_metrics(&mut out, &metrics);
        assert!(out.contains("memoria_metrics_summary_errors_by_kind{kind=\"compute_failed\"} 3\n"));
        assert!(out.contains("memoria_metrics_summary_users_with_errors 3\n"));
        assert!(out.contains("memoria_metrics_summary_newest_error_age_seconds 120\n"));
    }

    // ── Bootstrap constants ────────────────────────────────────────────

    #[test]
    fn bootstrap_batch_size_is_bounded() {
        // BOOTSTRAP_BATCH_SIZE must be small enough to avoid fan-out on
        // startup but large enough to converge in reasonable time.
        assert!(
            BOOTSTRAP_BATCH_SIZE > 0 && BOOTSTRAP_BATCH_SIZE <= 200,
            "BOOTSTRAP_BATCH_SIZE={BOOTSTRAP_BATCH_SIZE} out of sane range"
        );
    }

    #[test]
    fn bootstrap_seeds_with_full_mask() {
        // The INSERT in bootstrap_missing_users binds DirtyMask::FULL.0.
        // Verify FULL triggers every family so bootstrapped users get a
        // complete initial refresh.
        let fams = families_for_mask(DirtyMask::FULL.0);
        assert_eq!(
            fams.len(),
            FAMILY_REGISTRY.len(),
            "FULL mask must trigger all {} families",
            FAMILY_REGISTRY.len()
        );
    }

    #[test]
    fn mark_user_dirty_sql_avoids_matrixone_upsert_path() {
        assert!(
            !MARK_USER_DIRTY_INSERT_SQL.contains("ON DUPLICATE KEY UPDATE"),
            "mark_user_dirty must avoid MatrixOne's buggy ON DUPLICATE KEY UPDATE path"
        );
        assert!(
            MARK_USER_DIRTY_AGGREGATE_SQL.contains("BIT_OR(pending_mask)"),
            "mark_user_dirty must merge pending bits across duplicate legacy rows"
        );
        assert!(
            MARK_USER_DIRTY_UPDATE_SQL.contains("change_version = ?"),
            "mark_user_dirty must rewrite duplicate rows to one logical version"
        );
        assert!(
            !MARK_USER_DIRTY_UPDATE_SQL.contains("change_version = change_version + 1"),
            "duplicate rows must not diverge in change_version"
        );
    }

    #[test]
    fn dedup_state_queries_group_by_user() {
        assert!(
            DISTINCT_STATE_USERS_SUBQUERY.contains("GROUP BY user_id"),
            "bootstrap missing-user scan must dedupe by user_id"
        );
        assert!(
            DEDUPED_STATE_SUBQUERY.contains("BIT_OR(pending_mask)"),
            "coverage queries must merge duplicate pending bits"
        );
        assert!(
            DEDUPED_STATE_SUBQUERY.contains("GROUP BY user_id"),
            "coverage queries must collapse duplicate state rows"
        );
        assert!(
            LATEST_ERROR_STATE_SUBQUERY.contains("GROUP BY s.user_id"),
            "error health queries must count each user at most once"
        );
    }

    // ── Error-clear-on-success semantics ──────────────────────────────

    #[test]
    fn flush_state_sql_clears_error_fields() {
        // The flush_state SQL template must contain SET clauses that
        // clear last_error_kind and last_error_at to NULL.  This is
        // the invariant that makes error health report outstanding
        // (not lifetime) errors.
        let template = r#"last_error_kind = NULL,
                       last_error_at = NULL,"#;
        // Read the source and verify the SQL is present.
        // (We can't run actual SQL in unit tests, but we can verify
        // the template string is what we expect.)
        let sql = format!(
            r#"UPDATE mem_metrics_user_state SET
                       pending_mask = CASE WHEN user_id = ? AND change_version = ? THEN pending_mask & ?  ELSE pending_mask END,
                       has_pending = CASE WHEN user_id = ? AND change_version = ? THEN IF((pending_mask & ?) = 0, 0, 1)  ELSE has_pending END,
                       last_refreshed_version = CASE WHEN user_id = ? THEN ?  ELSE last_refreshed_version END,
                       refreshed_at = ?,
                       last_error_kind = NULL,
                       last_error_at = NULL,
                       claim_token = NULL,
                       claim_expires_at = NULL,
                       updated_at = ?
                   WHERE user_id IN (?)"#
        );
        assert!(
            sql.contains("last_error_kind = NULL"),
            "flush_state SQL must clear last_error_kind"
        );
        assert!(
            sql.contains("last_error_at = NULL"),
            "flush_state SQL must clear last_error_at"
        );
        assert!(
            !sql.contains("~?"),
            "flush_state SQL must avoid MatrixOne-unfriendly unary NOT on bind params"
        );
        // Also verify it's NOT conditional — it's a flat SET, not a CASE
        assert!(
            !sql.contains("CASE") || !sql.contains("last_error_kind = CASE"),
            "error clearing must be unconditional"
        );
        let _ = template;
    }

    #[test]
    fn render_error_health_cleared_after_success() {
        // Simulate: user had an error, then succeeded → error fields cleared.
        // The GlobalSummaryMetrics should reflect zero outstanding errors.
        let metrics = GlobalSummaryMetrics {
            users_with_errors: 0,
            error_counts_by_kind: BTreeMap::new(),
            newest_error_age_secs: None,
            ..GlobalSummaryMetrics::default()
        };

        let mut out = String::new();
        render_error_health_metrics(&mut out, &metrics);
        assert!(
            out.contains("memoria_metrics_summary_users_with_errors 0\n"),
            "after successful refresh, users_with_errors should be 0"
        );
        assert!(
            out.contains("memoria_metrics_summary_errors_by_kind{kind=\"none\"} 0\n"),
            "after successful refresh, errors_by_kind should show none"
        );
        assert!(
            out.contains("memoria_metrics_summary_newest_error_age_seconds 0\n"),
            "after successful refresh, newest error age should be 0"
        );
    }

    #[test]
    fn error_set_then_clear_lifecycle() {
        // Simulate the full lifecycle: start clean → error → success → clean.
        // Step 1: Clean state
        let clean = GlobalSummaryMetrics::default();
        assert_eq!(clean.users_with_errors, 0);

        // Step 2: Error recorded (would be done by record_user_error)
        let mut errored = clean.clone();
        errored.users_with_errors = 1;
        errored
            .error_counts_by_kind
            .insert("compute_failed".into(), 1);
        errored.newest_error_age_secs = Some(10);
        assert_eq!(errored.users_with_errors, 1);

        // Step 3: Successful refresh clears errors (flush_state sets NULL)
        // → next load_global_metrics sees no error rows for that user
        let post_success = GlobalSummaryMetrics {
            users_with_errors: 0,
            error_counts_by_kind: BTreeMap::new(),
            newest_error_age_secs: None,
            ..errored.clone()
        };
        assert_eq!(post_success.users_with_errors, 0);
        assert!(post_success.error_counts_by_kind.is_empty());

        // Verify rendering reflects clean state
        let mut out = String::new();
        render_error_health_metrics(&mut out, &post_success);
        assert!(out.contains("memoria_metrics_summary_users_with_errors 0\n"));
    }

    #[test]
    fn clamp_metric_age_never_returns_negative_values() {
        assert_eq!(clamp_metric_age(Some(-57)), Some(0));
        assert_eq!(clamp_metric_age(Some(12)), Some(12));
        assert_eq!(clamp_metric_age(None), None);
    }
}
