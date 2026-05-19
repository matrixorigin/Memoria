# Git-for-Data RFC — Rust Implementation Design

## 1. MatrixOne DDL Syntax

### Snapshots (account-level)
```sql
CREATE SNAPSHOT <name> FOR ACCOUNT
SHOW SNAPSHOTS
RESTORE ACCOUNT FROM SNAPSHOT <name>
DROP SNAPSHOT <name>

-- Time-travel read (non-destructive)
SELECT * FROM mem_memories {SNAPSHOT = '<name>'}
```

### Branches (table-level, zero-copy)
```sql
-- Create branch table (zero-copy clone of source)
data branch create table <branch_name> from mem_memories

-- Delete branch table
data branch delete table <db>.<branch_name>
```

Branch tables are regular tables with the same schema as `mem_memories`.
Branch name is an internally-generated UUID hex — never user input.

## 2. Rust Interface Design

```rust
pub struct GitForDataService {
    pool: MySqlPool,
    db_name: String,
}

impl GitForDataService {
    // ── Snapshots ──────────────────────────────────────────────────

    /// CREATE SNAPSHOT <name> FOR ACCOUNT
    pub async fn create_snapshot(&self, name: &str) -> Result<Snapshot, MemoriaError>;

    /// SHOW SNAPSHOTS
    pub async fn list_snapshots(&self) -> Result<Vec<Snapshot>, MemoriaError>;

    /// DROP SNAPSHOT <name>
    pub async fn drop_snapshot(&self, name: &str) -> Result<(), MemoriaError>;

    /// RESTORE ACCOUNT FROM SNAPSHOT <name>
    /// WARNING: destructive — all changes after snapshot are lost
    pub async fn restore_snapshot(&self, name: &str) -> Result<(), MemoriaError>;

    /// Lighter restore: DELETE + INSERT SELECT from snapshot (single table)
    pub async fn restore_table_from_snapshot(
        &self,
        table: &str,
        snapshot_name: &str,
    ) -> Result<(), MemoriaError>;

    // ── Branches ───────────────────────────────────────────────────

    /// data branch create table <uuid> from mem_memories
    pub async fn create_branch(&self, branch_name: &str) -> Result<(), MemoriaError>;

    /// data branch delete table <db>.<branch_name>
    pub async fn drop_branch(&self, branch_name: &str) -> Result<(), MemoriaError>;

    // ── Time-travel read ───────────────────────────────────────────

    /// SELECT * FROM mem_memories {SNAPSHOT = '<name>'} WHERE user_id = ?
    pub async fn read_at_snapshot(
        &self,
        snapshot_name: &str,
        user_id: &str,
    ) -> Result<Vec<Memory>, MemoriaError>;
}
```

## 3. Key Design Decisions

### 3.1 Identifier Safety
- Snapshot names and branch names must be validated before use in DDL
- Use `validate_identifier()` — alphanumeric + underscore only, no SQL injection
- Branch names are always internally generated UUID hex — never user input

### 3.2 Branch vs Snapshot
| | Snapshot | Branch |
|--|---------|--------|
| Scope | Account-level | Table-level |
| Cost | Zero-copy | Zero-copy |
| Restore | Destructive (whole account) | Non-destructive (single table) |
| Use case | Point-in-time backup | Isolated experiment |

### 3.3 sqlx Limitations
- `data branch create table` and `CREATE SNAPSHOT` are DDL — use `sqlx::query().execute()`
- `{SNAPSHOT = 'name'}` syntax in SELECT: sqlx may not support this natively
  → Use `sqlx::query(&format!(...))` with validated identifier (safe — not user input)
- `SHOW SNAPSHOTS` returns non-standard columns — parse with `Row::try_get()`

### 3.4 MCP Tool Mapping (Phase 5)
| MCP Tool | Git-for-Data Operation |
|---------|----------------------|
| `memory_snapshot` | `create_snapshot` |
| `memory_snapshots` | `list_snapshots` |
| `memory_rollback` | `restore_table_from_snapshot` |
| `memory_snapshot_delete` | `drop_snapshot` |
| `memory_branch` | `create_branch` + checkout |
| `memory_checkout` | switch active branch |
| `memory_merge` | INSERT SELECT from branch → main |
| `memory_diff` | compare branch vs main |
| `memory_branch_delete` | `drop_branch` |

## 4. Implementation Plan (Phase 5)

1. Add `memoria-git` crate
2. Implement `GitForDataService` with snapshot + branch ops
3. Add integration tests against real MatrixOne
4. Wire into `MemoryService`
5. Add 9 MCP tools (snapshot/branch/merge/rollback/diff)

## 5. Verified Against Real MatrixOne ✅

All 3 open questions confirmed:
1. `SHOW SNAPSHOTS` works — columns: SNAPSHOT_NAME, TIMESTAMP, SNAPSHOT_LEVEL, ACCOUNT_NAME, DATABASE_NAME, TABLE_NAME
2. `data branch create table <name> from mem_memories` works in memoria database
3. `CREATE SNAPSHOT <name> FOR ACCOUNT` + `SELECT ... {SNAPSHOT = '<name>'}` works

Ready to implement Phase 5.
