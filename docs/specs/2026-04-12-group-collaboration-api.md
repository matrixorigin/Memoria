# Group Collaboration API Design

## Goals

- Add a small-team collaboration mode on top of the existing per-user Memoria architecture.
- Keep single-user behavior unchanged when a request uses a normal personal API key.
- Let a team share one physical Memoria database per group.
- Reuse the existing branch model for experimentation inside a group database.
- Protect `main` in group mode so it can only be changed through explicit selective apply (with a solo-owner bypass for initial seeding).
- Keep the first version operationally small and avoid adding new entities unless they are necessary.

## Non-Goals

- No field-level merge or three-way conflict resolution.
- No native branch merge in group mode (blocked by middleware).
- No per-edit attribution tracking — `author_id` records who created a memory, but subsequent corrections preserve the original author (corrector identity is in edit-log only).

## Background

The current system already has two important properties:

1. Per-user physical database routing exists as the base architecture.
2. Branches already exist as separate tables inside one database, with `main` backed by `mem_memories` and branch checkout implemented through `active_table()`.

This design keeps both properties intact. Group collaboration is added as a control-plane extension, not as a rewrite of the memory data model.

## Design Summary

The system supports two request modes:

- **Personal mode**: API key has no `group_id`. Requests continue to use the user's personal Memoria database exactly as they do today.
- **Group mode**: API key is bound to a `group_id`. Requests are routed to that group's shared physical database. In this mode, `main` is read-only and only selective apply may modify it.

## Data Model

### Shared Control-Plane Tables

Two tables in the shared metadata database:

```sql
CREATE TABLE mem_groups (
    group_id      VARCHAR(64)  PRIMARY KEY,
    group_name    VARCHAR(128) NOT NULL,
    db_name       VARCHAR(128) NOT NULL UNIQUE,
    owner_user_id VARCHAR(64)  NOT NULL,
    status        VARCHAR(20)  NOT NULL DEFAULT 'active',
    created_at    DATETIME(6)  NOT NULL,
    updated_at    DATETIME(6)  NOT NULL
);

CREATE TABLE mem_group_members (
    group_id     VARCHAR(64)  NOT NULL,
    user_id      VARCHAR(64)  NOT NULL,
    display_name VARCHAR(128),
    role         VARCHAR(20)  NOT NULL DEFAULT 'member',
    is_active    TINYINT(1)   NOT NULL DEFAULT 1,
    joined_at    DATETIME(6)  NOT NULL,
    removed_at   DATETIME(6)  DEFAULT NULL,
    PRIMARY KEY (group_id, user_id),
    KEY idx_gmem_user (user_id, is_active)
);
```

Field meanings:

- `group_id`: stable identifier, format `grp_<12-char hex>`.
- `group_name`: human-readable display name.
- `db_name`: physical database name for the shared group memory store, format `mem_grp_<12-char hex>`.
- `owner_user_id`: creator and manager of the group.
- `status`: lifecycle flag (`active` or `deleted`).

`mem_group_members` tracks membership with soft-delete support:

- `role`: `owner` for the group creator, `member` for invited users.
- `is_active`: `1` for current members, `0` for removed members (soft-delete).
- `removed_at`: timestamp when the member was removed (NULL if active).
- Re-adding a previously removed member reactivates the row (`is_active = 1`, `removed_at = NULL`).

### Memory Author Tracking

The `mem_memories` table (in each user/group database) includes an `author_id` column:

```sql
ALTER TABLE mem_memories ADD COLUMN author_id VARCHAR(64) DEFAULT NULL;
```

- **Group mode**: `author_id = auth.user_id` (the real human user who created the memory).
- **Personal mode**: `author_id = NULL` (not needed; `user_id` already identifies the author).
- On `correct()` operations, the original `author_id` is preserved (the corrector is not reassigned as author).

This is a separate concept from `user_id`, which in group mode is the `grp_xxx` scope ID used for database routing.

### API Key Extension

The existing `mem_api_keys` table is extended with:

```sql
group_id VARCHAR(64) DEFAULT NULL
```

Semantics:

- `group_id IS NULL`: personal key, use existing per-user database routing.
- `group_id IS NOT NULL`: group key, route to the shared group database.

Old keys remain fully valid without behavior change.

## Routing and Authentication

### AuthUser and scope_id

Upon authentication, the middleware constructs an `AuthUser` struct:

```rust
pub struct AuthUser {
    pub user_id: String,       // real human user
    pub scope_id: String,      // routing key: user_id (personal) or group_id (group)
    pub group_id: Option<String>,
    pub is_master: bool,
}
```

- In personal mode: `scope_id == user_id`.
- In group mode: `scope_id == group_id` (e.g. `grp_abc123`).

All downstream service calls use `scope_id` as the database routing key. The DB router detects `grp_` prefix and routes to the group's physical database via `mem_groups.db_name`.

### Membership Enforcement

Membership is validated at request entry with a 5-minute in-memory cache:

1. Load the API key record.
2. If `group_id` is present:
   - JOIN `mem_groups` with `mem_group_members` by `group_id`,
   - verify group exists and `status = 'active'`,
   - verify `user_id` is an active member (`is_active = 1`) in `mem_group_members`,
   - reject if any check fails.

If a user is removed from the group, their API keys are deactivated and the cache is proactively invalidated. As a defense-in-depth, the DB-level membership check on cache miss also rejects requests from non-members (authoritative gate).

### Key Issuance Rule

Group-scoped API keys may only be issued for users that are active members in `mem_group_members`. The group owner creates keys on behalf of members.

Key issuance is a separate explicit step from group creation.

## Per-Member Active Branch (ACTOR_USER_ID)

### Problem

In group mode, `scope_id` is the group ID (e.g. `grp_abc`). The storage layer uses this to key `mem_user_state` for active branch tracking. Without correction, all group members would share a single active branch pointer — Member A's checkout would overwrite Member B's state.

### Solution

A tokio task-local variable `ACTOR_USER_ID` carries the real human user ID through the request lifecycle:

```rust
tokio::task_local! {
    pub static ACTOR_USER_ID: String;
}
```

The `actor_scope_layer` middleware sets this for every group-scoped request. Storage functions that manage per-user state (`active_branch_name`, `set_active_branch`, `active_table`) read `ACTOR_USER_ID` to key on the real user, while still using `scope_id` (group ID) for shared resources like `mem_branches`.

This means:

- **Branch list**: shared across group (keyed by `scope_id`).
- **Branch data**: shared across group (keyed by `scope_id`).
- **Active branch pointer**: per-member (keyed by real `user_id` via `ACTOR_USER_ID`).
- **Cache keys**: per-member (prevents cross-member cache pollution).

In personal mode, the task-local is not set and the fallback is the passed `user_id`, so behavior is unchanged.

## Group Lifecycle

### Create Group

`POST /v1/groups`

1. Insert a row in `mem_groups`.
2. Create the physical database via `CREATE DATABASE IF NOT EXISTS`.
3. Bootstrap the Memoria schema in that database (via `migrate_user()`).
4. Insert membership rows into `mem_group_members` for the creator (role `owner`) and any initial members (role `member`).
5. Optional seed import: when `seed.db_name` is provided and matches the current key's accessible scope DB, copy active rows from the source database's `mem_memories` main table into the new group DB, rewriting `user_id` to the new `group_id`.

No API key is auto-issued; key creation is a separate step.

### Add Member

`POST /v1/groups/:group_id/members/:user_id`

- Owner only.
- Validates the target user is registered in `mem_user_registry` (must have created at least one personal key first).
- Inserts a new row into `mem_group_members` (or reactivates a previously removed member).

### Remove Member

`DELETE /v1/groups/:group_id/members/:user_id`

- Owner only.
- Cannot remove the owner themselves.
- Soft-deletes the member (`is_active = 0`, `removed_at = NOW()`) in `mem_group_members`.
- Deactivates all active group-bound API keys for that `(user_id, group_id)`.
- Invalidates the key cache for affected keys.

### Delete Group

`DELETE /v1/groups/:group_id`

- Owner only.
- Marks group `status = 'deleted'`.
- Deactivates all API keys bound to this group.
- Drops the physical group database.

## Middleware Guards

Three middleware layers enforce group-mode constraints at the router level, removing the need for per-handler guard calls:

### 1. Write Guard (`group_main_write_guard`)

Applied to all write routes. If the request is group-scoped and the caller's active branch is `main`, returns **403 Forbidden**: "main is read-only in group mode; create or checkout a branch, then use selective apply."

**Solo-owner exception**: When a group has exactly one active member (the owner), writes to `main` are allowed. This avoids forcing branch ceremonies when a user initially seeds a group database alone before inviting collaborators.

Protected routes:
- `POST /v1/memories` (store)
- `POST /v1/memories/batch` (batch store)
- `POST /v1/memories/correct` (correct by query)
- `POST /v1/memories/purge` (purge)
- `PUT /v1/memories/:id/correct` (correct by id)
- `DELETE /v1/memories/:id` (delete)
- `POST /v1/observe` (observe turn)
- `POST /v1/sessions/:session_id/summary` (session summary)

### 2. Merge Guard (`group_merge_guard`)

Applied to `POST /v1/branches/:name/merge`. Blocks native branch merge entirely in group mode with **403 Forbidden**: "native branch merge is disabled in group mode; use selective apply instead."

### 3. Actor Scope Layer (`actor_scope_layer`)

Applied to all routes. Sets the `ACTOR_USER_ID` task-local for group-scoped requests, enabling per-member active branch tracking.

## Branch Workflow in Group Mode

The intended group workflow is:

1. Read `main` (always allowed).
2. Create a branch from `main`.
3. Checkout the branch (per-member, does not affect other members).
4. Perform experiments on the branch.
5. Diff the branch against latest `main`.
6. User reviews and selects records to apply.
7. System applies selected changes into `main` transactionally.

## Diff Model

### Comparison Base

Branch diff uses MatrixOne's native `data branch diff` command, not SQL JOINs.
The branch table **must** be created with `data branch create table` (not `CREATE TABLE LIKE`);
only native branches produce correct INSERT/UPDATE flags.

```sql
data branch diff {db}.{branch_table} against {db}.{main_table}
    columns (user_id, memory_id, content, memory_type, is_active, superseded_by, author_id)
    output limit {limit}
```

### Output Format

`data branch diff` returns rows with a leading **source column** (index 0) whose value is the
table name each row originates from (`br_<name>` for the branch, `mem_memories` for main).
The column header is dynamic (`diff {branch} against {main}`) and is not relied upon; the source
is parsed by column index.

### Source-Aware Classification

`classify_diff_rows(rows, branch_table)` first separates all rows by source:

- **Branch-side rows**: `source == branch_table`
- **Main-side rows**: `source == "mem_memories"`

Any `memory_id` that appears on **both** sides is classified as a **CONFLICT** (along with any `superseded_by` targets referenced by conflicting rows).
The remaining branch-only rows are classified by `flag`, `is_active`, and `superseded_by`:

| Scenario                       | flag   | is_active | superseded_by | Classification |
|--------------------------------|--------|-----------|---------------|----------------|
| New memory on branch           | INSERT | 1         | NULL          | **ADDED** |
| Created then deleted on branch | INSERT | 0         | NULL          | hidden |
| Created then corrected (old)   | INSERT | 0         | new_id        | hidden |
| Created then corrected (new)   | INSERT | 1         | NULL          | **ADDED** (paired → **UPDATED**) |
| Deleted main memory on branch  | UPDATE | 0         | NULL          | **REMOVED** |
| Corrected main memory (old)    | UPDATE | 0         | new_id        | paired into **UPDATED** |

**Pairing logic**: An UPDATE row (`is_active=0`, `superseded_by=new_id`) is paired with its corresponding INSERT row (`memory_id=new_id`, `is_active=1`). Together they form a single **UPDATED** entry containing both old and new content.

Main-only rows with `is_active = 1` are returned as **BEHIND_MAIN** (informational).

### Classification Output (5 categories)

| Category | Meaning |
|----------|---------|
| `added` | New memories created on the branch, not yet in main |
| `updated` | Correction pairs: old memory superseded by new on the branch |
| `removed` | Soft-deleted on the branch but still active in main |
| `conflicts` | Same `memory_id` appears on both sides (concurrent modification) |
| `behind_main` | Active memories in main that do not appear on this branch (added by others) |

### Conflict Structure

A conflict carries both sides of the divergence:

```
DiffConflict {
  memory_id: String,
  branch_side: DiffConflictSide { content, is_active, superseded_by, superseded_by_content, author_id },
  main_side:   DiffConflictSide { content, is_active, superseded_by, superseded_by_content, author_id },
}
```

`superseded_by_content` is resolved from the same diff output — if a conflicting row has
`superseded_by` pointing to another row in the same diff, that row's content is inlined for
display without extra queries.

### Endpoint

`GET /v1/branches/:name/diff-items?limit=100`

Default limit is 100, max 500. Each category is independently truncated at the limit.

Response:

```json
{
  "branch": "exp-a",
  "against": "main",
  "added": [
    { "memory_id": "m1", "content": "new memory", "memory_type": "semantic", "author_id": "alice" }
  ],
  "updated": [
    {
      "memory_id": "new_id", "content": "corrected content", "memory_type": "semantic",
      "old_memory_id": "old_id", "old_content": "original content",
      "author_id": "alice"
    }
  ],
  "removed": [
    { "memory_id": "m3", "content": "deleted memory", "memory_type": "procedural", "author_id": "alice" }
  ],
  "conflicts": [
    {
      "memory_id": "mx",
      "branch": {
        "content": "alice's version", "is_active": 1,
        "superseded_by": null, "superseded_by_content": null,
        "author_id": "alice"
      },
      "main": {
        "content": "bob's version", "is_active": 1,
        "superseded_by": null, "superseded_by_content": null,
        "author_id": "bob"
      }
    }
  ],
  "behind_main": [
    { "memory_id": "m9", "content": "bob added this", "memory_type": "semantic", "author_id": "bob" }
  ]
}
```

- All items include `author_id` (the real human user who created the memory in group mode; `null` in personal mode).
- `conflicts` support **per-item accept-branch selection** during apply; unchecked conflicts implicitly keep main.
- `behind_main` is informational only — not selectable for apply.

## Selective Apply

### Endpoint

`POST /v1/branches/:name/apply`

Request:

```json
{
  "adds": ["m1"],
  "updates": [{"old_id": "old_m2", "new_id": "new_m2"}],
  "removes": ["m3"],
  "accept_branch_conflicts": ["mx"]
}
```

Response:

```json
{
  "applied_adds": ["m1"],
  "applied_updates": ["old_m2→new_m2"],
  "applied_removes": ["m3"],
  "applied_conflicts": ["mx"],
  "skipped_adds": [],
  "skipped_updates": [],
  "skipped_removes": [],
  "skipped_conflicts": []
}
```

### Apply Semantics

All operations run in a single database transaction. The core principle is **verbatim copy**: rows are copied from the branch table to main exactly as they are, preserving all fields including `updated_at`.

**Add**: For each selected add:
1. Verify the memory is active in the branch (`is_active = 1`) and absent from main.
2. `INSERT INTO main SELECT ... FROM branch WHERE memory_id = ? AND is_active = 1`.
3. Skip if main already has that `memory_id`.

**Update** (correction pair `{old_id, new_id}`): For each selected update:
1. Verify `old_id` exists in main (the original memory to be corrected).
2. Verify `old_id` exists in branch (superseded version, `is_active = 0`, `superseded_by = new_id`).
3. Verify `new_id` exists in branch and is active (`is_active = 1`).
4. `DELETE FROM main WHERE memory_id = old_id` (hard delete the original).
5. `INSERT INTO main SELECT ... FROM branch WHERE memory_id = old_id` (copy superseded row, preserving `is_active = 0` and `superseded_by`).
6. `INSERT INTO main SELECT ... FROM branch WHERE memory_id = new_id AND is_active = 1` (copy the corrected replacement).

**Remove**: For each selected remove:
1. Verify the memory is active in main (`is_active = 1`).
2. Verify the branch has the soft-deleted version (`is_active = 0`).
3. `DELETE FROM main WHERE memory_id = ?` (hard delete the active row).
4. `INSERT INTO main SELECT ... FROM branch WHERE memory_id = ? AND is_active = 0` (copy the soft-deleted version verbatim).
5. Skip if main is already inactive or branch doesn't have the soft-deleted version.

**Accept Branch Conflict**: For each selected conflict `memory_id`:
1. Re-run diff classification and verify the item is still a current conflict.
2. Build the **main conflict subgraph**: `memory_id` plus `main.superseded_by` (if any).
3. Build the **branch conflict subgraph**: `memory_id` plus `branch.superseded_by` (if any).
4. `DELETE FROM main WHERE memory_id IN (main subgraph)` to remove the current main-side resolution.
5. `INSERT INTO main SELECT ... FROM branch WHERE memory_id IN (branch subgraph)` to copy the branch-side resolution verbatim.
6. Unselected conflicts are treated as **accept main** (no-op on main; the branch still remains divergent until changed separately).

### Logging

Apply actions are logged via the existing edit-log mechanism with a `branch_apply` action type. The log payload includes:

- `actor_user_id` (real human user, not scope_id)
- `group_id`
- `source_branch` (branch name)
- Applied and skipped counts for each category

## API Reference

### Group Management

| Method | Path | Auth | Description |
|--------|------|------|-------------|
| POST | `/v1/groups` | Any user | Create group (caller becomes owner) |
| GET | `/v1/groups` | Any user | List groups where caller is a member |
| GET | `/v1/groups/:group_id` | Member | Get group details |
| POST | `/v1/groups/:group_id/members/:user_id` | Owner | Add member |
| DELETE | `/v1/groups/:group_id/members/:user_id` | Owner | Remove member + revoke keys |
| DELETE | `/v1/groups/:group_id` | Owner | Delete group + drop DB |

### Key Management

| Method | Path | Auth | Description |
|--------|------|------|-------------|
| POST | `/auth/keys` | User or Owner | Create key (with optional `group_id`) |
| GET | `/auth/keys` | Any user | List caller's keys |
| GET | `/auth/keys/:id` | Owner of key | Get key details |
| PUT | `/auth/keys/:id/rotate` | Owner of key | Rotate key (new secret, same metadata) |
| DELETE | `/auth/keys/:id` | Owner of key | Revoke key |

### Branch Diff & Apply

| Method | Path | Auth | Description |
|--------|------|------|-------------|
| GET | `/v1/branches/:name/diff-items` | Member | 5-category structured diff (added/updated/removed/conflicts/behind_main) |
| POST | `/v1/branches/:name/apply` | Member | Selective apply to main |

## Compatibility Notes

This design is intentionally conservative:

- Personal mode remains unchanged.
- Existing branch storage remains unchanged.
- Existing memory row schema remains unchanged (only nullable `author_id` column added).
- Existing soft-delete semantics remain unchanged.
- Group management adds two tables (`mem_groups`, `mem_group_members`) and one nullable column (`group_id` on `mem_api_keys`).
- MatrixOne compatibility: qualified table names used instead of `USE` statements.

## Risks and Deferred Work

### Conflict Detection (Partial)

Concurrent-modification conflicts are detected at diff time via the `source` column in
`data branch diff` output. Any `memory_id` appearing on both branch and main sides is
surfaced as a `conflict` entry in the diff response.

Conflict **resolution** is not yet automated — the caller must decide which side wins and
apply changes manually. Options being considered:

- "Accept branch" — implemented as a per-conflict apply option that replaces the main-side conflict subgraph with the branch-side subgraph.
- "Accept main" — currently a no-op on main when the conflict is left unchecked.
- "Manual merge" — user edits the content in a separate operation before applying.

### No Dedicated Merge Session Table

Audit data is stored in edit logs. If the feature later needs browsing, retries, or historical merge inspection, a dedicated merge-session entity can be introduced.

### Snapshot/Rollback Safety in Groups

Snapshot and rollback operations in a group database affect all members' data. No per-member isolation exists for these operations. Restricting snapshot/rollback to group owners is a potential future improvement.

### Group DB Not in User Registry

Group databases (`mem_grp_*`) are not registered in `mem_user_registry`. This means governance and stats operations do not traverse group databases. Adding group DB registration is a future improvement.

## Test UI (not shipped)

A local HTML+JS dev UI exists at `tools/group-collab-ui/` (git-excluded, for local verification only). It provides a proxy server and a diff panel that renders all 5 diff categories with author badges and conflict side-by-side comparison.
