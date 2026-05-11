use chrono::NaiveDateTime;
use memoria_core::MemoriaError;
use serde::{Deserialize, Serialize};
use sqlx::{mysql::MySqlPool, Row};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

fn db_err(e: sqlx::Error) -> MemoriaError {
    MemoriaError::Database(e.to_string())
}

/// Returns true when the error is MatrixOne's "txn need retry in rc mode, def changed"
/// (error code 20631). This is a transient conflict that arises when a DDL operation
/// runs concurrently with another DDL that modifies the same table's definition (e.g.
/// an ALTER TABLE migration overlapping with `data branch merge`). The caller should
/// back off briefly and retry the statement.
fn is_mo_retry_error(e: &sqlx::Error) -> bool {
    let msg = e.to_string();
    msg.contains("20631") || msg.contains("txn need retry")
}

/// Returns true when the error indicates a SQL parse / unsupported-syntax failure.
/// Used to detect that the connected MatrixOne build does not support `columns (...)`
/// in `data branch diff` so we can fall back gracefully (Option B runtime downgrade).
fn is_mo_syntax_error(e: &sqlx::Error) -> bool {
    let msg = e.to_string().to_lowercase();
    // MySQL/MatrixOne error 1064 = "You have an error in your SQL syntax".
    // Also match plain-text parse-error messages from newer MO builds.
    msg.contains("1064") || msg.contains("parse error") || msg.contains("syntax error")
}

/// Execute a DDL statement without prepared statement protocol.
/// MatrixOne does not support PREPARE for DDL (CREATE SNAPSHOT, data branch, etc.).
///
/// Automatically retries up to 3 times on MatrixOne error 20631
/// ("txn need retry in rc mode, def changed"), which is a transient conflict that can
/// occur when concurrent DDL operations (e.g. schema migrations adding columns to branch
/// tables) race with `data branch merge` or similar commands.
async fn exec_ddl(pool: &MySqlPool, sql: &str) -> Result<(), MemoriaError> {
    const MAX_ATTEMPTS: u32 = 3;
    let mut attempt = 0u32;
    loop {
        match sqlx::raw_sql(sql).execute(pool).await {
            Ok(_) => return Ok(()),
            Err(e) if attempt < MAX_ATTEMPTS - 1 && is_mo_retry_error(&e) => {
                attempt += 1;
                let backoff_ms = 50u64 * (1 << attempt); // 100 ms, 200 ms
                tracing::warn!(
                    attempt,
                    backoff_ms,
                    error = %e,
                    "exec_ddl: MatrixOne 20631 retry",
                );
                tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
            }
            Err(e) => return Err(db_err(e)),
        }
    }
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

fn quote_identifier(name: &str) -> String {
    format!("`{}`", name.replace('`', "``"))
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
    pub source: String, // branch table name or main table name
    pub flag: String,   // INSERT | UPDATE | DELETE
    pub memory_id: String,
    pub content: String,
    pub memory_type: String,
    pub is_active: i8,
    pub superseded_by: Option<String>,
    pub author_id: Option<String>,
}

/// Classified diff result produced by [`classify_diff_rows`].
///
/// ## Classification rules
///
/// Classification uses only `is_active` and `superseded_by`; the `flag` field is
/// intentionally ignored.  MatrixOne's `data branch diff` flag values are not stable
/// across versions:
///   * Older builds: flag=INSERT for newly-inserted branch rows, flag=UPDATE for modified/deleted.
///   * MatrixOne ≥ 3.0.11: flag=UPDATE for **all** diff rows regardless of DML operation.
///
/// Using flag for classification caused all newly-added branch memories to disappear
/// from diff results on MatrixOne ≥ 3.0.11 (they were `flag=UPDATE, is_active=1` which
/// matched neither the INSERT-processing nor the UPDATE-processing branch).
///
/// | Scenario                        | main | branch        | is_active | superseded_by | Category    |
/// |---------------------------------|------|---------------|-----------|---------------|-------------|
/// | New memory on branch            | ✗    | ✓ (active)    | 1         | NULL          | **ADDED**   |
/// | Created then corrected (old)    | ✗    | ✓ (inactive)  | 0         | new_id        | hidden (part of UPDATED pair) |
/// | Created then corrected (new)    | ✗    | ✓ (active)    | 1         | NULL          | **ADDED**   |
/// | Deleted main memory on branch   | ✓    | ✓ (inactive)  | 0         | NULL          | **REMOVED** |
/// | Created then deleted on branch  | ✗    | ✓ (inactive)  | 0         | NULL          | **REMOVED** (was hidden; see note below) |
/// | Corrected main memory (old)     | ✓    | ✓ (inactive)  | 0         | new_id        | **UPDATED** |
/// | Corrected main memory (new)     | ✗    | ✓ (active)    | 1         | NULL          | paired into **UPDATED** |
///
/// Note: "created then deleted on branch" rows initially land in `removed`, but
/// `resolve_ghost_removes` moves them to `ghost_removes` (hidden) after verifying
/// they are absent (or already inactive) in main — keeping `removed` aligned with
/// what `selective_apply` can actually merge.
#[derive(Debug, Clone, Default)]
pub struct ClassifiedDiff {
    pub added: Vec<DiffItem>,
    pub updated: Vec<DiffUpdatedPair>,
    /// Memories that are **active in main** (`is_active = 1`) and were deleted on
    /// this branch. Initially populated by `classify_diff_rows` for all branch-only
    /// inactive rows, then refined by `resolve_ghost_removes` which moves items that
    /// are absent (or already inactive) in main into `ghost_removes`.
    pub removed: Vec<DiffItem>,
    pub conflicts: Vec<DiffConflict>,
    /// Main-only changes: rows that exist on main but not on this branch
    /// (other users' merges that happened after this branch was created).
    pub behind_main: Vec<DiffItem>,
    /// Branch-only inactive rows with no superseded_by: the memory was
    /// created AND deleted entirely within this branch — it never existed
    /// in main. These are NOT shown as diffs; merging them is always a
    /// no-op (the selective_apply remove guard requires main to have the
    /// memory, which it doesn't).
    pub ghost_removes: Vec<DiffItem>,
}

/// A single diff item (used for ADDED and REMOVED categories).
#[derive(Debug, Clone)]
pub struct DiffItem {
    pub memory_id: String,
    pub content: String,
    pub memory_type: String,
    pub author_id: Option<String>,
}

/// A paired UPDATED item: old memory (on main) → new memory (on branch).
#[derive(Debug, Clone)]
pub struct DiffUpdatedPair {
    pub old_memory_id: String,
    pub old_content: String,
    pub new_memory_id: String,
    pub new_content: String,
    pub memory_type: String,
    pub author_id: Option<String>,
}

/// A conflict where branch and main diverged on the same memory_id.
#[derive(Debug, Clone)]
pub struct DiffConflict {
    pub memory_id: String,
    pub branch_side: DiffConflictSide,
    pub main_side: DiffConflictSide,
}

/// One side of a conflict.
#[derive(Debug, Clone)]
pub struct DiffConflictSide {
    pub content: String,
    pub is_active: i8,
    pub superseded_by: Option<String>,
    pub superseded_by_content: Option<String>,
    pub author_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ApplySelection {
    #[serde(default)]
    pub adds: Vec<String>,
    #[serde(default)]
    pub removes: Vec<String>,
    #[serde(default)]
    pub updates: Vec<ApplyUpdatePair>,
    #[serde(default)]
    pub accept_branch_conflicts: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApplyUpdatePair {
    pub old_id: String,
    pub new_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ApplyResult {
    pub applied_adds: Vec<String>,
    pub skipped_adds: Vec<String>,
    pub applied_updates: Vec<String>,
    pub skipped_updates: Vec<String>,
    pub applied_removes: Vec<String>,
    pub skipped_removes: Vec<String>,
    pub applied_conflicts: Vec<String>,
    pub skipped_conflicts: Vec<String>,
}

/// Classify raw `data branch diff` rows into ADDED / UPDATED / REMOVED / CONFLICTS.
///
/// Each DiffRow carries a `source` field (branch table name or main table name).
/// The `branch_table` parameter identifies which source is "ours".
///
/// Algorithm:
/// 1. Separate rows into branch-side and main-side by `source`.
/// 2. Find memory_ids that appear on BOTH sides → CONFLICT.
/// 3. Classify remaining branch-only rows using the original INSERT/UPDATE logic.
/// 4. Main-only rows are ignored (behind-main; informational only for now).
pub fn classify_diff_rows(rows: Vec<DiffRow>, branch_table: &str) -> ClassifiedDiff {
    use std::collections::{HashMap, HashSet};

    let mut result = ClassifiedDiff::default();

    // Step 1: Separate by source.
    // DiffRow.source was already normalized to the unqualified table name in diff_branch_rows,
    // so this comparison should be stable regardless of MatrixOne version.
    let mut branch_rows: Vec<&DiffRow> = Vec::new();
    let mut main_rows: Vec<&DiffRow> = Vec::new();
    for r in &rows {
        // MatrixOne returns identifiers in lowercase regardless of how they were created,
        // so use case-insensitive comparison to avoid misclassifying all branch rows as
        // main-side when the branch name contains uppercase letters.
        if r.source.eq_ignore_ascii_case(branch_table) {
            branch_rows.push(r);
        } else {
            main_rows.push(r);
        }
    }
    tracing::debug!(
        total = rows.len(),
        branch_side = branch_rows.len(),
        main_side = main_rows.len(),
        branch_table = %branch_table,
        "classify_diff_rows: source split"
    );
    for r in &branch_rows {
        tracing::debug!(
            memory_id = %r.memory_id,
            flag = %r.flag,
            is_active = r.is_active,
            superseded_by = ?r.superseded_by,
            "classify_diff_rows: branch row"
        );
    }

    // Step 2: Find conflicting memory_ids (present on both sides)
    let branch_mids: HashSet<&str> = branch_rows.iter().map(|r| r.memory_id.as_str()).collect();
    let main_mids: HashSet<&str> = main_rows.iter().map(|r| r.memory_id.as_str()).collect();
    let conflict_mids: HashSet<&str> = branch_mids.intersection(&main_mids).copied().collect();

    // Build conflict entries
    if !conflict_mids.is_empty() {
        // Build a content lookup for ALL rows so we can resolve superseded_by targets
        let all_by_mid: HashMap<(&str, &str), &DiffRow> = rows
            .iter()
            .map(|r| ((r.source.as_str(), r.memory_id.as_str()), r))
            .collect();

        let branch_by_mid: HashMap<&str, &DiffRow> = branch_rows
            .iter()
            .filter(|r| conflict_mids.contains(r.memory_id.as_str()))
            .map(|r| (r.memory_id.as_str(), *r))
            .collect();
        let main_by_mid: HashMap<&str, &DiffRow> = main_rows
            .iter()
            .filter(|r| conflict_mids.contains(r.memory_id.as_str()))
            .map(|r| (r.memory_id.as_str(), *r))
            .collect();

        let resolve_superseded = |row: &DiffRow, source: &str| -> Option<String> {
            row.superseded_by.as_ref().and_then(|sid| {
                if sid.is_empty() {
                    return None;
                }
                all_by_mid
                    .get(&(source, sid.as_str()))
                    .map(|r| r.content.clone())
            })
        };

        for mid in &conflict_mids {
            if let (Some(br), Some(mr)) = (branch_by_mid.get(mid), main_by_mid.get(mid)) {
                result.conflicts.push(DiffConflict {
                    memory_id: mid.to_string(),
                    branch_side: DiffConflictSide {
                        content: br.content.clone(),
                        is_active: br.is_active,
                        superseded_by: br.superseded_by.clone(),
                        superseded_by_content: resolve_superseded(br, branch_table),
                        author_id: br.author_id.clone(),
                    },
                    main_side: DiffConflictSide {
                        content: mr.content.clone(),
                        is_active: mr.is_active,
                        superseded_by: mr.superseded_by.clone(),
                        superseded_by_content: resolve_superseded(mr, &mr.source),
                        author_id: mr.author_id.clone(),
                    },
                });
            }
        }
    }

    // Step 3: Classify branch-only rows (exclude conflicting memory_ids)
    // Also exclude memory_ids that are the "new" side of a conflict correction pair
    // (if a conflicting old_id has superseded_by pointing to new_id, that new_id is also conflict-related)
    let mut conflict_related: HashSet<String> =
        conflict_mids.iter().map(|s| s.to_string()).collect();
    for r in &branch_rows {
        if conflict_mids.contains(r.memory_id.as_str()) {
            if let Some(ref new_id) = r.superseded_by {
                if !new_id.is_empty() {
                    conflict_related.insert(new_id.clone());
                }
            }
        }
    }

    let clean_branch: Vec<&DiffRow> = branch_rows
        .iter()
        .filter(|r| !conflict_related.contains(&r.memory_id))
        .copied()
        .collect();

    // FIXME(memoria-team): Two known scenarios can cause duplicate memory_id entries in
    // `result.added`, which surfaces in the dashboard as two visually identical INSERT rows
    // sharing the same checkbox state (selecting one selects the other) and an off-by-one
    // counter (e.g. "4 Changes" header but 5 rows rendered).
    //
    // Scenario A — superseded_map key collision:
    //   If two distinct UPDATE rows both carry the same `superseded_by` new_id value
    //   (a data-consistency anomaly), `HashMap::insert` silently overwrites the first entry.
    //   The INSERT row whose `memory_id` matches that new_id therefore finds *no* match in
    //   superseded_map on the first pass, falls through to `result.added`, and then the
    //   same INSERT may be re-encountered if the diff output contains duplicate raw rows.
    //
    // Scenario B — MatrixOne `data branch diff` returning duplicate raw rows:
    //   In certain MatrixOne versions/configurations the `data branch diff ... output limit N`
    //   statement can return the same physical row twice. Both copies have flag=INSERT,
    //   is_active=1, and the same memory_id, so both survive the `is_active == 0` filter and
    //   both enter `result.added`.
    //
    // Recommended fix: before the INSERT processing loop, deduplicate `clean_branch` by
    // (memory_id, flag) keeping the row with is_active=1 over is_active=0, and deduplicate
    // `superseded_map` construction by only inserting the first occurrence of each new_id.
    //
    // Workaround already applied on the memoria-website frontend (GitMemory.jsx): the
    // rendered diff list deduplicates allItems by rowKey so the UI stays consistent even
    // when the API returns duplicates.

    // Classification no longer depends on `flag` because MatrixOne versions differ
    // in what they return: older builds use flag=INSERT for newly inserted branch rows,
    // while newer builds (≥ 3.0.11) return flag=UPDATE for all diff rows regardless of
    // the actual DML operation. We therefore use only `is_active` and `superseded_by`
    // to drive the three-way classification.

    // Build superseded map: inactive branch rows that point to a replacement memory_id.
    let mut superseded_map: HashMap<String, &DiffRow> = HashMap::new();
    for r in &clean_branch {
        if r.is_active == 0 {
            if let Some(ref new_id) = r.superseded_by {
                if !new_id.is_empty() {
                    superseded_map.insert(new_id.clone(), r);
                }
            }
        }
    }

    // Active rows (is_active=1): ADDED, or the "new" side of an UPDATED pair.
    for r in &clean_branch {
        if r.is_active != 1 {
            continue;
        }
        if let Some(old_row) = superseded_map.remove(&r.memory_id) {
            result.updated.push(DiffUpdatedPair {
                old_memory_id: old_row.memory_id.clone(),
                old_content: old_row.content.clone(),
                new_memory_id: r.memory_id.clone(),
                new_content: r.content.clone(),
                memory_type: r.memory_type.clone(),
                author_id: r.author_id.clone(),
            });
        } else {
            result.added.push(DiffItem {
                memory_id: r.memory_id.clone(),
                content: r.content.clone(),
                memory_type: r.memory_type.clone(),
                author_id: r.author_id.clone(),
            });
        }
    }

    // Branch-only inactive rows without superseded_by.
    //
    // `data branch diff` only returns the branch-side row for deletions — it
    // does NOT return the main-side row — so every such row here is branch-only.
    // There are two distinct scenarios that look identical at this point:
    //
    //   A. "Deleted from main on branch": memory_id exists in main (active).
    //      → User-visible REMOVED; should propagate on merge.
    //
    //   B. "Created-then-deleted on branch": memory_id NEVER existed in main.
    //      → Ghost/no-op; hide from diff (merge would skip anyway).
    //
    // We cannot distinguish A from B without querying the main table. The
    // initial classification puts ALL of them into `removed`. Callers that have
    // DB access should call `resolve_ghost_removes` afterwards to move bucket-B
    // items from `removed` into `ghost_removes`.
    for r in &clean_branch {
        if r.is_active != 0 {
            continue;
        }
        let has_superseded = r
            .superseded_by
            .as_ref()
            .map(|s| !s.is_empty())
            .unwrap_or(false);
        if !has_superseded {
            result.removed.push(DiffItem {
                memory_id: r.memory_id.clone(),
                content: r.content.clone(),
                memory_type: r.memory_type.clone(),
                author_id: r.author_id.clone(),
            });
        }
    }

    // Step 4: Collect main-only rows as behind_main (exclude conflict-related)
    let mut main_conflict_related: HashSet<String> =
        conflict_mids.iter().map(|s| s.to_string()).collect();
    for r in &main_rows {
        if conflict_mids.contains(r.memory_id.as_str()) {
            if let Some(ref new_id) = r.superseded_by {
                if !new_id.is_empty() {
                    main_conflict_related.insert(new_id.clone());
                }
            }
        }
    }
    for r in &main_rows {
        if main_conflict_related.contains(&r.memory_id) {
            continue;
        }
        // Only show active main-only items (new additions by others)
        // and meaningful changes (corrections, deletions)
        if r.is_active == 1 {
            result.behind_main.push(DiffItem {
                memory_id: r.memory_id.clone(),
                content: r.content.clone(),
                memory_type: r.memory_type.clone(),
                author_id: r.author_id.clone(),
            });
        }
    }

    result
}

pub struct GitForDataService {
    pool: MySqlPool,
    db_name: String,
    /// Whether the connected MatrixOne build supports the `columns (...)` projection
    /// clause in `data branch diff`. Starts `true`; automatically flipped to `false`
    /// the first time a parse/unsupported-syntax error is observed, after which all
    /// diff calls in this process skip the projection (Option B runtime downgrade).
    ///
    /// Using `columns (...)` is strongly preferred: without it, MatrixOne ships every
    /// column of the source table over the wire — including embedding vectors and
    /// extra_metadata JSON — even though only 7 fields are read.
    supports_diff_columns: Arc<AtomicBool>,
}

impl GitForDataService {
    pub fn new(pool: MySqlPool, db_name: impl Into<String>) -> Self {
        Self {
            pool,
            db_name: db_name.into(),
            supports_diff_columns: Arc::new(AtomicBool::new(true)),
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
            &format!(
                "CREATE SNAPSHOT {safe} FOR DATABASE {}",
                quote_identifier(&self.db_name)
            ),
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
            .filter(|result| {
                result
                    .as_ref()
                    .ok()
                    .and_then(|snapshot| snapshot.database_name.as_ref())
                    .is_some_and(|db_name| db_name == &self.db_name)
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
        let db = quote_identifier(&self.db_name);
        let qualified_table = format!("{db}.{safe_table}");

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
        exec_ddl(&self.pool, &format!("DELETE FROM {qualified_table}")).await?;
        exec_ddl(
            &self.pool,
            &format!(
                "INSERT INTO {qualified_table} SELECT * FROM {qualified_table} {{SNAPSHOT = '{safe_snap}'}}"
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
        let db = quote_identifier(&self.db_name);
        exec_ddl(
            &self.pool,
            &format!("data branch create table {db}.{safe_branch} from {db}.{safe_source}"),
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
        let db = quote_identifier(&self.db_name);
        exec_ddl(
            &self.pool,
            &format!(
            "data branch create table {db}.{safe_branch} from {db}.{safe_source} {{snapshot = '{safe_snap}'}}"
        ),
        )
        .await
    }

    pub async fn drop_branch(&self, branch_name: &str) -> Result<(), MemoriaError> {
        let safe = validate_identifier(branch_name)?;
        let db = quote_identifier(&self.db_name);
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
        let db = quote_identifier(&self.db_name);
        exec_ddl(
            &self.pool,
            &format!(
                "data branch merge {db}.{safe_branch} into {db}.{safe_main} when conflict skip"
            ),
        )
        .await
    }

    /// LCA-based diff count for a specific user.
    /// Native `output count` is account-level, so we fetch rows and count in Rust.
    /// Counts only visible changes (ADDED + UPDATED + REMOVED) after classification.
    pub async fn diff_branch_count(
        &self,
        branch_table: &str,
        main_table: &str,
        user_id: &str,
    ) -> Result<i64, MemoriaError> {
        let rows = self
            .diff_branch_rows(branch_table, main_table, user_id, 5001)
            .await?;
        let classified = classify_diff_rows(rows, branch_table);
        let total = classified.added.len()
            + classified.updated.len()
            + classified.removed.len()
            + classified.conflicts.len()
            + classified.behind_main.len();
        Ok(total as i64)
    }

    /// Native LCA-based diff rows, filtered by user_id.
    ///
    /// `data branch diff` is account-level (no WHERE clause supported), so we fetch
    /// all rows and filter in Rust. Returns raw diff rows including source, is_active,
    /// superseded_by, and author_id.
    /// Use [`classify_diff_rows`] to get ADDED/UPDATED/REMOVED/CONFLICTS.
    ///
    /// ## Column projection
    /// The preferred query includes `columns (user_id, memory_id, ...)` to avoid
    /// shipping embedding vectors and extra_metadata JSON over the wire. If this MO
    /// build does not support that clause (parse/syntax error), the flag
    /// `supports_diff_columns` is set to `false` for the lifetime of the process and
    /// all subsequent calls fall back to the full-row query (Option B runtime downgrade).
    ///
    /// In both query forms, MatrixOne prepends an implicit source column (the table
    /// name the row originated from) as column index 0 in the result set, so ordinal
    /// access `r.try_get(0usize)` reliably retrieves it regardless of whether the
    /// projection clause is active.
    pub async fn diff_branch_rows(
        &self,
        branch_table: &str,
        main_table: &str,
        user_id: &str,
        limit: i64,
    ) -> Result<Vec<DiffRow>, MemoriaError> {
        let limit = limit.clamp(1, 5_000);
        let safe_branch = validate_identifier(branch_table)?;
        let safe_main = validate_identifier(main_table)?;
        let db = quote_identifier(&self.db_name);
        // Fetch more rows than requested to account for user_id filtering in Rust.
        let fetch_limit = limit * 10 + 100;

        // Projected query: only retrieve the 7 fields we actually read. Avoids shipping
        // embedding vectors and extra_metadata JSON over the wire on every diff call.
        const DIFF_COLS: &str =
            "columns (user_id, memory_id, content, memory_type, is_active, superseded_by, author_id)";

        let sql_projected = format!(
            "data branch diff {db}.{safe_branch} against {db}.{safe_main} \
             {DIFF_COLS} output limit {fetch_limit}"
        );
        let sql_full = format!(
            "data branch diff {db}.{safe_branch} against {db}.{safe_main} \
             output limit {fetch_limit}"
        );

        let rows = if self.supports_diff_columns.load(Ordering::Relaxed) {
            match sqlx::raw_sql(&sql_projected).fetch_all(&self.pool).await {
                Ok(rows) => rows,
                Err(e) if is_mo_syntax_error(&e) => {
                    // This MO build does not support columns (...) — disable for this
                    // process lifetime and retry with the full-row query.
                    self.supports_diff_columns.store(false, Ordering::Relaxed);
                    tracing::warn!(
                        error = %e,
                        "data branch diff: `columns (...)` unsupported on this MatrixOne \
                         build; falling back to full-row fetch. Consider upgrading to \
                         MatrixOne ≥ 3.0-dev or aligning your cloud build."
                    );
                    sqlx::raw_sql(&sql_full)
                        .fetch_all(&self.pool)
                        .await
                        .map_err(db_err)?
                }
                Err(e) => return Err(db_err(e)),
            }
        } else {
            sqlx::raw_sql(&sql_full)
                .fetch_all(&self.pool)
                .await
                .map_err(db_err)?
        };

        tracing::debug!(
            branch_table = %branch_table,
            main_table = %main_table,
            raw_row_count = rows.len(),
            "diff_branch_rows: raw rows from data branch diff"
        );

        // Log the first row's source column to diagnose qualified vs unqualified naming.
        if let Some(first) = rows.first() {
            let sample_source: String = first.try_get(0usize).unwrap_or_default();
            tracing::debug!(
                sample_source = %sample_source,
                branch_table = %branch_table,
                "diff_branch_rows: source column sample (col 0)"
            );
        }

        let mut result = Vec::new();
        for r in &rows {
            let uid: String = r.try_get("user_id").map_err(db_err)?;
            if uid != user_id {
                continue;
            }
            // Column index 0 is the implicit source column that MatrixOne prepends to
            // `data branch diff` output — it holds the originating table name and is
            // present in both the projected and full-row query forms.
            //
            // Depending on the MatrixOne version, column 0 may be qualified ("db.table")
            // or unqualified ("table"). We normalize to the unqualified form so that
            // classify_diff_rows can reliably compare against the bare table name.
            let source_raw: String = r.try_get(0usize).unwrap_or_default();
            let source = source_raw
                .rsplit_once('.')
                .map(|(_, t)| t.to_string())
                .unwrap_or(source_raw);
            result.push(DiffRow {
                source,
                flag: r.try_get("flag").map_err(db_err)?,
                memory_id: r.try_get("memory_id").map_err(db_err)?,
                content: r.try_get("content").map_err(db_err)?,
                memory_type: r
                    .try_get("memory_type")
                    .unwrap_or_else(|_| "semantic".into()),
                is_active: r.try_get("is_active").unwrap_or(1),
                superseded_by: {
                    let val: Option<String> = r.try_get("superseded_by").unwrap_or(None);
                    val.filter(|s| !s.is_empty())
                },
                author_id: {
                    let val: Option<String> = r.try_get("author_id").unwrap_or(None);
                    val.filter(|s| !s.is_empty())
                },
            });
            if result.len() >= limit as usize {
                break;
            }
        }
        tracing::debug!(
            filtered_row_count = result.len(),
            branch_table = %branch_table,
            "diff_branch_rows: rows after user_id filter"
        );
        Ok(result)
    }

    pub async fn selective_apply(
        &self,
        branch_table: &str,
        main_table: &str,
        user_id: &str,
        selection: ApplySelection,
    ) -> Result<ApplyResult, MemoriaError> {
        let branch_table = validate_identifier(branch_table)?.to_string();
        let main_table = validate_identifier(main_table)?.to_string();
        let db = quote_identifier(&self.db_name);
        let branch_table_ref = format!("{db}.{branch_table}");
        let main_table_ref = format!("{db}.{main_table}");

        let add_ids: Vec<String> = selection
            .adds
            .into_iter()
            .filter(|id| !id.is_empty())
            .collect();
        let update_pairs: Vec<ApplyUpdatePair> = selection
            .updates
            .into_iter()
            .filter(|p| !p.old_id.is_empty() && !p.new_id.is_empty())
            .collect();
        let remove_ids: Vec<String> = selection
            .removes
            .into_iter()
            .filter(|id| !id.is_empty())
            .collect();
        let conflict_ids: Vec<String> = selection
            .accept_branch_conflicts
            .into_iter()
            .filter(|id| !id.is_empty())
            .collect();

        let classified = if conflict_ids.is_empty() {
            None
        } else {
            // `data branch diff` cannot filter by memory_id, so fetch a generous window
            // and re-check that each requested item is still a current conflict.
            Some(classify_diff_rows(
                self.diff_branch_rows(&branch_table, &main_table, user_id, 5_000)
                    .await?,
                &branch_table,
            ))
        };
        let conflict_map: std::collections::HashMap<&str, &DiffConflict> = classified
            .as_ref()
            .map(|classified| {
                classified
                    .conflicts
                    .iter()
                    .map(|conflict| (conflict.memory_id.as_str(), conflict))
                    .collect()
            })
            .unwrap_or_default();

        let mut result = ApplyResult::default();
        let mut tx = self.pool.begin().await.map_err(db_err)?;

        if !add_ids.is_empty() {
            let placeholders = add_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            let branch_sql = format!(
                "SELECT memory_id FROM {branch_table_ref} WHERE user_id = ? AND is_active = 1 AND memory_id IN ({placeholders})"
            );
            let mut q = sqlx::query(&branch_sql).bind(user_id);
            for id in &add_ids {
                q = q.bind(id);
            }
            // Use pool directly for the read-only branch-table probe.
            // MatrixOne zero-copy branch tables may not be readable within an open
            // write transaction on the same connection, causing the branch check to
            // return empty and silently skip all adds.
            let branch_present: std::collections::HashSet<String> = q
                .fetch_all(&self.pool)
                .await
                .map_err(db_err)?
                .iter()
                .filter_map(|row| row.try_get::<String, _>("memory_id").ok())
                .collect();

            // Query main for ALL rows with these memory_ids (regardless of is_active),
            // to check for existing records (active or inactive) that would conflict.
            let main_sql = format!(
                "SELECT memory_id, is_active FROM {main_table_ref} WHERE user_id = ? AND memory_id IN ({placeholders})"
            );
            let mut q = sqlx::query(&main_sql).bind(user_id);
            for id in &add_ids {
                q = q.bind(id);
            }
            let main_rows = q.fetch_all(&self.pool).await.map_err(db_err)?;
            let main_present_all: std::collections::HashSet<String> = main_rows
                .iter()
                .filter_map(|row| row.try_get::<String, _>("memory_id").ok())
                .collect();
            let main_active: std::collections::HashSet<String> = main_rows
                .iter()
                .filter(|row| row.try_get::<i8, _>("is_active").unwrap_or(0) == 1)
                .filter_map(|row| row.try_get::<String, _>("memory_id").ok())
                .collect();

            tracing::debug!(
                add_ids = add_ids.len(),
                branch_present = branch_present.len(),
                main_present_all = main_present_all.len(),
                main_active = main_active.len(),
                "selective_apply: add check"
            );

            // Classify adds into three buckets:
            // - new_adds:     not in main at all → INSERT
            // - restore_adds: in main but inactive (is_active=0, e.g. previously deleted)
            //                 → DELETE stale row + INSERT branch version
            // - skip:         already active in main → no-op
            let mut new_adds: Vec<String> = Vec::new();
            let mut restore_adds: Vec<String> = Vec::new();
            for id in &add_ids {
                if !branch_present.contains(id) {
                    result.skipped_adds.push(id.clone());
                } else if main_active.contains(id) {
                    // Already active in main — already merged.
                    result.skipped_adds.push(id.clone());
                } else if main_present_all.contains(id) {
                    // Exists in main but inactive (deleted). Restore from branch.
                    restore_adds.push(id.clone());
                    result.applied_adds.push(id.clone());
                } else {
                    // Genuinely new — not in main at all.
                    new_adds.push(id.clone());
                    result.applied_adds.push(id.clone());
                }
            }

            // INSERT genuinely new memories
            if !new_adds.is_empty() {
                let ph = new_adds.iter().map(|_| "?").collect::<Vec<_>>().join(",");
                let insert_sql = format!(
                    "INSERT INTO {main_table_ref} \
                     (memory_id, user_id, memory_type, content, embedding, session_id, \
                      source_event_ids, extra_metadata, is_active, superseded_by, \
                      trust_tier, initial_confidence, observed_at, created_at, updated_at, author_id) \
                     SELECT memory_id, user_id, memory_type, content, embedding, session_id, \
                            source_event_ids, extra_metadata, is_active, superseded_by, \
                            trust_tier, initial_confidence, observed_at, created_at, updated_at, author_id \
                     FROM {branch_table_ref} WHERE user_id = ? AND is_active = 1 AND memory_id IN ({ph})"
                );
                let mut q = sqlx::query(&insert_sql).bind(user_id);
                for id in &new_adds {
                    q = q.bind(id);
                }
                q.execute(&mut *tx).await.map_err(db_err)?;
            }

            // Restore inactive-in-main memories: delete stale row, then insert branch version.
            // Cannot use INSERT...ON DUPLICATE KEY UPDATE because MatrixOne branch tables are
            // read-only views in some builds. Instead: DELETE + INSERT.
            if !restore_adds.is_empty() {
                let ph = restore_adds.iter().map(|_| "?").collect::<Vec<_>>().join(",");
                let delete_sql = format!(
                    "DELETE FROM {main_table_ref} WHERE user_id = ? AND memory_id IN ({ph})"
                );
                let mut q = sqlx::query(&delete_sql).bind(user_id);
                for id in &restore_adds {
                    q = q.bind(id);
                }
                q.execute(&mut *tx).await.map_err(db_err)?;

                let insert_sql = format!(
                    "INSERT INTO {main_table_ref} \
                     (memory_id, user_id, memory_type, content, embedding, session_id, \
                      source_event_ids, extra_metadata, is_active, superseded_by, \
                      trust_tier, initial_confidence, observed_at, created_at, updated_at, author_id) \
                     SELECT memory_id, user_id, memory_type, content, embedding, session_id, \
                            source_event_ids, extra_metadata, is_active, superseded_by, \
                            trust_tier, initial_confidence, observed_at, created_at, updated_at, author_id \
                     FROM {branch_table_ref} WHERE user_id = ? AND is_active = 1 AND memory_id IN ({ph})"
                );
                let mut q = sqlx::query(&insert_sql).bind(user_id);
                for id in &restore_adds {
                    q = q.bind(id);
                }
                q.execute(&mut *tx).await.map_err(db_err)?;
                tracing::info!(
                    count = restore_adds.len(),
                    "selective_apply: restored inactive-in-main memories from branch"
                );
            }
        }

        if !update_pairs.is_empty() {
            for pair in &update_pairs {
                let old_exists: i64 = sqlx::query_scalar(&format!(
                    "SELECT COUNT(*) FROM {main_table_ref} WHERE memory_id = ? AND user_id = ?"
                ))
                .bind(&pair.old_id)
                .bind(user_id)
                .fetch_one(&mut *tx)
                .await
                .map_err(db_err)?;

                // Read branch table via pool (not tx) to avoid MatrixOne zero-copy
                // visibility issues when reading branch tables inside an active transaction.
                let branch_old_exists: i64 = sqlx::query_scalar(&format!(
                    "SELECT COUNT(*) FROM {branch_table_ref} WHERE memory_id = ? AND user_id = ?"
                ))
                .bind(&pair.old_id)
                .bind(user_id)
                .fetch_one(&self.pool)
                .await
                .map_err(db_err)?;
                let branch_new_exists: i64 = sqlx::query_scalar(&format!(
                    "SELECT COUNT(*) FROM {branch_table_ref} WHERE memory_id = ? AND user_id = ? AND is_active = 1"
                ))
                .bind(&pair.new_id)
                .bind(user_id)
                .fetch_one(&self.pool)
                .await
                .map_err(db_err)?;

                if old_exists > 0 && branch_old_exists > 0 && branch_new_exists > 0 {
                    sqlx::query(&format!(
                        "DELETE FROM {main_table_ref} WHERE user_id = ? AND memory_id = ?"
                    ))
                    .bind(user_id)
                    .bind(&pair.old_id)
                    .execute(&mut *tx)
                    .await
                    .map_err(db_err)?;

                    sqlx::query(&format!(
                        "INSERT INTO {main_table_ref} \
                         (memory_id, user_id, memory_type, content, embedding, session_id, \
                          source_event_ids, extra_metadata, is_active, superseded_by, \
                          trust_tier, initial_confidence, observed_at, created_at, updated_at, author_id) \
                         SELECT memory_id, user_id, memory_type, content, embedding, session_id, \
                                source_event_ids, extra_metadata, is_active, superseded_by, \
                                trust_tier, initial_confidence, observed_at, created_at, updated_at, author_id \
                         FROM {branch_table_ref} WHERE memory_id = ? AND user_id = ?"
                    ))
                    .bind(&pair.old_id)
                    .bind(user_id)
                    .execute(&mut *tx)
                    .await
                    .map_err(db_err)?;

                    sqlx::query(&format!(
                        "INSERT INTO {main_table_ref} \
                         (memory_id, user_id, memory_type, content, embedding, session_id, \
                          source_event_ids, extra_metadata, is_active, superseded_by, \
                          trust_tier, initial_confidence, observed_at, created_at, updated_at, author_id) \
                         SELECT memory_id, user_id, memory_type, content, embedding, session_id, \
                                source_event_ids, extra_metadata, is_active, superseded_by, \
                                trust_tier, initial_confidence, observed_at, created_at, updated_at, author_id \
                         FROM {branch_table_ref} WHERE memory_id = ? AND user_id = ? AND is_active = 1"
                    ))
                    .bind(&pair.new_id)
                    .bind(user_id)
                    .execute(&mut *tx)
                    .await
                    .map_err(db_err)?;

                    result
                        .applied_updates
                        .push(format!("{}→{}", pair.old_id, pair.new_id));
                } else {
                    result
                        .skipped_updates
                        .push(format!("{}→{}", pair.old_id, pair.new_id));
                }
            }
        }

        if !remove_ids.is_empty() {
            let placeholders = remove_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            let main_sql = format!(
                "SELECT memory_id FROM {main_table_ref} WHERE user_id = ? AND is_active = 1 AND memory_id IN ({placeholders})"
            );
            let mut q = sqlx::query(&main_sql).bind(user_id);
            for id in &remove_ids {
                q = q.bind(id);
            }
            let main_present: std::collections::HashSet<String> = q
                .fetch_all(&mut *tx)
                .await
                .map_err(db_err)?
                .iter()
                .filter_map(|row| row.try_get::<String, _>("memory_id").ok())
                .collect();

            // Read branch table via pool (not tx) — same reason as update path above.
            let branch_sql = format!(
                "SELECT memory_id FROM {branch_table_ref} WHERE user_id = ? AND is_active = 0 AND memory_id IN ({placeholders})"
            );
            let mut q = sqlx::query(&branch_sql).bind(user_id);
            for id in &remove_ids {
                q = q.bind(id);
            }
            let branch_present: std::collections::HashSet<String> = q
                .fetch_all(&self.pool)
                .await
                .map_err(db_err)?
                .iter()
                .filter_map(|row| row.try_get::<String, _>("memory_id").ok())
                .collect();

            for id in &remove_ids {
                if main_present.contains(id) && branch_present.contains(id) {
                    result.applied_removes.push(id.clone());
                } else {
                    result.skipped_removes.push(id.clone());
                }
            }

            if !result.applied_removes.is_empty() {
                let ph = result
                    .applied_removes
                    .iter()
                    .map(|_| "?")
                    .collect::<Vec<_>>()
                    .join(",");
                let del_sql = format!(
                    "DELETE FROM {main_table_ref} WHERE user_id = ? AND memory_id IN ({ph})"
                );
                let mut q = sqlx::query(&del_sql).bind(user_id);
                for id in &result.applied_removes {
                    q = q.bind(id);
                }
                q.execute(&mut *tx).await.map_err(db_err)?;

                let ins_sql = format!(
                    "INSERT INTO {main_table_ref} \
                     (memory_id, user_id, memory_type, content, embedding, session_id, \
                      source_event_ids, extra_metadata, is_active, superseded_by, \
                      trust_tier, initial_confidence, observed_at, created_at, updated_at, author_id) \
                     SELECT memory_id, user_id, memory_type, content, embedding, session_id, \
                            source_event_ids, extra_metadata, is_active, superseded_by, \
                            trust_tier, initial_confidence, observed_at, created_at, updated_at, author_id \
                     FROM {branch_table_ref} WHERE user_id = ? AND is_active = 0 AND memory_id IN ({ph})"
                );
                let mut q = sqlx::query(&ins_sql).bind(user_id);
                for id in &result.applied_removes {
                    q = q.bind(id);
                }
                q.execute(&mut *tx).await.map_err(db_err)?;
            }
        }

        if !conflict_ids.is_empty() {
            for conflict_id in &conflict_ids {
                let Some(conflict) = conflict_map.get(conflict_id.as_str()) else {
                    result.skipped_conflicts.push(conflict_id.clone());
                    continue;
                };

                let mut branch_ids = vec![conflict.memory_id.clone()];
                if let Some(new_id) = conflict.branch_side.superseded_by.clone() {
                    branch_ids.push(new_id);
                }
                branch_ids.sort();
                branch_ids.dedup();

                let mut main_ids = vec![conflict.memory_id.clone()];
                if let Some(new_id) = conflict.main_side.superseded_by.clone() {
                    main_ids.push(new_id);
                }
                main_ids.sort();
                main_ids.dedup();

                let branch_ph = branch_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
                let branch_exists_sql = format!(
                    "SELECT memory_id FROM {branch_table_ref} WHERE user_id = ? AND memory_id IN ({branch_ph})"
                );
                let mut q = sqlx::query(&branch_exists_sql).bind(user_id);
                for id in &branch_ids {
                    q = q.bind(id);
                }
                // Read branch table via pool (not tx) — same reason as add/remove paths:
                // MatrixOne zero-copy branch tables may not be visible inside an open
                // write transaction on the same connection.
                let branch_present: std::collections::HashSet<String> = q
                    .fetch_all(&self.pool)
                    .await
                    .map_err(db_err)?
                    .iter()
                    .filter_map(|row| row.try_get::<String, _>("memory_id").ok())
                    .collect();
                if branch_present.len() != branch_ids.len() {
                    result.skipped_conflicts.push(conflict_id.clone());
                    continue;
                }

                let main_ph = main_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
                let main_exists_sql = format!(
                    "SELECT memory_id FROM {main_table_ref} WHERE user_id = ? AND memory_id IN ({main_ph})"
                );
                let mut q = sqlx::query(&main_exists_sql).bind(user_id);
                for id in &main_ids {
                    q = q.bind(id);
                }
                let main_present: std::collections::HashSet<String> = q
                    .fetch_all(&mut *tx)
                    .await
                    .map_err(db_err)?
                    .iter()
                    .filter_map(|row| row.try_get::<String, _>("memory_id").ok())
                    .collect();
                if main_present.len() != main_ids.len() {
                    result.skipped_conflicts.push(conflict_id.clone());
                    continue;
                }

                let del_sql = format!(
                    "DELETE FROM {main_table_ref} WHERE user_id = ? AND memory_id IN ({main_ph})"
                );
                let mut q = sqlx::query(&del_sql).bind(user_id);
                for id in &main_ids {
                    q = q.bind(id);
                }
                q.execute(&mut *tx).await.map_err(db_err)?;

                let ins_sql = format!(
                    "INSERT INTO {main_table_ref} \
                     (memory_id, user_id, memory_type, content, embedding, session_id, \
                      source_event_ids, extra_metadata, is_active, superseded_by, \
                      trust_tier, initial_confidence, observed_at, created_at, updated_at, author_id) \
                     SELECT memory_id, user_id, memory_type, content, embedding, session_id, \
                            source_event_ids, extra_metadata, is_active, superseded_by, \
                            trust_tier, initial_confidence, observed_at, created_at, updated_at, author_id \
                     FROM {branch_table_ref} WHERE user_id = ? AND memory_id IN ({branch_ph})"
                );
                let mut q = sqlx::query(&ins_sql).bind(user_id);
                for id in &branch_ids {
                    q = q.bind(id);
                }
                q.execute(&mut *tx).await.map_err(db_err)?;

                result.applied_conflicts.push(conflict_id.clone());
            }
        }

        tx.commit().await.map_err(db_err)?;
        Ok(result)
    }

    /// Refine a [`ClassifiedDiff`] by querying the main table to distinguish
    /// two cases that are indistinguishable from the raw `data branch diff` output:
    ///
    /// - **Real remove** (`memory_id` is **active** in main, `is_active = 1`): stays in `removed`.
    ///   This matches the guard in `selective_apply`'s remove path so that every item
    ///   shown as REMOVED in the UI is actually mergeable.
    /// - **Ghost remove** (`memory_id` never existed in main, or already inactive):
    ///   moved to `ghost_removes` (not shown to user).
    ///
    /// Must be called after [`classify_diff_rows`] when the caller has DB access.
    /// If `classified.removed` is empty the method is a no-op.
    pub async fn resolve_ghost_removes(
        &self,
        classified: &mut ClassifiedDiff,
        main_table: &str,
        user_id: &str,
    ) -> Result<(), MemoriaError> {
        if classified.removed.is_empty() {
            return Ok(());
        }
        let main_table = validate_identifier(main_table)?;
        let db = quote_identifier(&self.db_name);
        let main_table_ref = format!("{db}.{main_table}");
        let remove_ids: Vec<String> = classified
            .removed
            .iter()
            .map(|r| r.memory_id.clone())
            .collect();
        let ph = remove_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        // Use is_active = 1 to match the exact guard in selective_apply's remove path
        // (L1098: "is_active = 1"). A memory that is already inactive in main would
        // never pass that guard, so showing it as REMOVED in the diff is a false-positive:
        // the user sees a deletable item that the merge silently skips.
        let sql = format!(
            "SELECT memory_id FROM {main_table_ref} WHERE user_id = ? AND is_active = 1 AND memory_id IN ({ph})"
        );
        let mut q = sqlx::query_scalar(&sql).bind(user_id);
        for id in &remove_ids {
            q = q.bind(id);
        }
        let main_has: std::collections::HashSet<String> = q
            .fetch_all(&self.pool)
            .await
            .map_err(db_err)?
            .into_iter()
            .collect();

        let old_removed = std::mem::take(&mut classified.removed);
        for item in old_removed {
            if main_has.contains(&item.memory_id) {
                classified.removed.push(item);
            } else {
                classified.ghost_removes.push(item);
            }
        }
        tracing::debug!(
            real_removes = classified.removed.len(),
            ghost_removes = classified.ghost_removes.len(),
            "resolve_ghost_removes: filtered removed bucket"
        );
        Ok(())
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
