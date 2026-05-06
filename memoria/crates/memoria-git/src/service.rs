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
/// Classification rules (based on MatrixOne `data branch diff` output):
///
/// | Scenario                        | main | branch        | flag   | is_active | superseded_by | Category    |
/// |---------------------------------|------|---------------|--------|-----------|---------------|-------------|
/// | New memory on branch            | ✗    | ✓ (active)    | INSERT | 1         | NULL          | **ADDED**   |
/// | Created then deleted on branch  | ✗    | ✓ (inactive)  | INSERT | 0         | NULL          | hidden      |
/// | Created then corrected (old)    | ✗    | ✓ (inactive)  | INSERT | 0         | new_id        | hidden      |
/// | Created then corrected (new)    | ✗    | ✓ (active)    | INSERT | 1         | NULL          | **ADDED**   |
/// | Deleted main memory on branch   | ✓    | ✓ (inactive)  | UPDATE | 0         | NULL          | **REMOVED** |
/// | Corrected main memory (old)     | ✓    | ✓ (inactive)  | UPDATE | 0         | new_id        | **UPDATED** |
/// | Corrected main memory (new)     | ✗    | ✓ (active)    | INSERT | 1         | NULL          | paired into **UPDATED** |
#[derive(Debug, Clone, Default)]
pub struct ClassifiedDiff {
    pub added: Vec<DiffItem>,
    pub updated: Vec<DiffUpdatedPair>,
    pub removed: Vec<DiffItem>,
    pub conflicts: Vec<DiffConflict>,
    /// Main-only changes: rows that exist on main but not on this branch
    /// (other users' merges that happened after this branch was created).
    pub behind_main: Vec<DiffItem>,
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

    // Step 1: Separate by source
    let mut branch_rows: Vec<&DiffRow> = Vec::new();
    let mut main_rows: Vec<&DiffRow> = Vec::new();
    for r in &rows {
        if r.source == branch_table {
            branch_rows.push(r);
        } else {
            main_rows.push(r);
        }
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

    // Build superseded map from clean branch rows
    let mut superseded_map: HashMap<String, &DiffRow> = HashMap::new();
    for r in &clean_branch {
        if r.flag == "UPDATE" && r.is_active == 0 {
            if let Some(ref new_id) = r.superseded_by {
                if !new_id.is_empty() {
                    superseded_map.insert(new_id.clone(), r);
                }
            }
        }
    }

    // Process INSERT rows
    for r in &clean_branch {
        if r.flag != "INSERT" || r.is_active == 0 {
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

    // Process UPDATE rows — is_active=0 without superseded_by → REMOVED
    for r in &clean_branch {
        if r.flag != "UPDATE" || r.is_active != 0 {
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
    pub async fn diff_branch_rows(
        &self,
        branch_table: &str,
        main_table: &str,
        user_id: &str,
        limit: i64,
    ) -> Result<Vec<DiffRow>, MemoriaError> {
        let safe_branch = validate_identifier(branch_table)?;
        let safe_main = validate_identifier(main_table)?;
        let db = quote_identifier(&self.db_name);
        // Fetch more than limit to account for filtering
        let fetch_limit = limit * 10 + 100;
        let sql = format!(
            "data branch diff {db}.{safe_branch} against {db}.{safe_main} \
             columns (user_id, memory_id, content, memory_type, is_active, superseded_by, author_id) \
             output limit {fetch_limit}"
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
            // First column (index 0) is the source table name
            let source: String = r.try_get(0usize).unwrap_or_default();
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
            let branch_present: std::collections::HashSet<String> = q
                .fetch_all(&mut *tx)
                .await
                .map_err(db_err)?
                .iter()
                .filter_map(|row| row.try_get::<String, _>("memory_id").ok())
                .collect();

            let main_sql = format!(
                "SELECT memory_id FROM {main_table_ref} WHERE user_id = ? AND memory_id IN ({placeholders})"
            );
            let mut q = sqlx::query(&main_sql).bind(user_id);
            for id in &add_ids {
                q = q.bind(id);
            }
            let main_present: std::collections::HashSet<String> = q
                .fetch_all(&mut *tx)
                .await
                .map_err(db_err)?
                .iter()
                .filter_map(|row| row.try_get::<String, _>("memory_id").ok())
                .collect();

            for id in &add_ids {
                if branch_present.contains(id) && !main_present.contains(id) {
                    result.applied_adds.push(id.clone());
                } else {
                    result.skipped_adds.push(id.clone());
                }
            }

            if !result.applied_adds.is_empty() {
                let ph = result
                    .applied_adds
                    .iter()
                    .map(|_| "?")
                    .collect::<Vec<_>>()
                    .join(",");
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
                for id in &result.applied_adds {
                    q = q.bind(id);
                }
                q.execute(&mut *tx).await.map_err(db_err)?;
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

                let branch_old_exists: i64 = sqlx::query_scalar(&format!(
                    "SELECT COUNT(*) FROM {branch_table_ref} WHERE memory_id = ? AND user_id = ?"
                ))
                .bind(&pair.old_id)
                .bind(user_id)
                .fetch_one(&mut *tx)
                .await
                .map_err(db_err)?;
                let branch_new_exists: i64 = sqlx::query_scalar(&format!(
                    "SELECT COUNT(*) FROM {branch_table_ref} WHERE memory_id = ? AND user_id = ? AND is_active = 1"
                ))
                .bind(&pair.new_id)
                .bind(user_id)
                .fetch_one(&mut *tx)
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

            let branch_sql = format!(
                "SELECT memory_id FROM {branch_table_ref} WHERE user_id = ? AND is_active = 0 AND memory_id IN ({placeholders})"
            );
            let mut q = sqlx::query(&branch_sql).bind(user_id);
            for id in &remove_ids {
                q = q.bind(id);
            }
            let branch_present: std::collections::HashSet<String> = q
                .fetch_all(&mut *tx)
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
                let branch_present: std::collections::HashSet<String> = q
                    .fetch_all(&mut *tx)
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
