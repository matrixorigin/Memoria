# API Reference

Base URL: `http://localhost:8100`

All endpoints require `Authorization: Bearer <api_key>` unless noted otherwise.

---

## Memory

### List Memories

```
GET /v1/memories?limit=50&cursor=...&memory_type=semantic
```

Cursor-based pagination. Returns memories ordered by `observed_at` descending.

| Param | Type | Default | Description |
|-------|------|---------|-------------|
| `limit` | int | 50 | Max items (1–500) |
| `cursor` | string | — | Cursor from previous response |
| `memory_type` | string | — | Filter by type |

Response:
```json
{
  "memories": [
    {
      "memory_id": "abc123",
      "content": "User prefers Python",
      "memory_type": "semantic",
      "confidence": 0.85,
      "observed_at": "2026-03-10 21:36:52.742020",
      "session_id": null
    }
  ],
  "next_cursor": "2026-03-10 21:36:52.742020|abc123"
}
```

### Store Memory

```
POST /v1/memories
```

```json
{
  "content": "User prefers Python over JavaScript",
  "memory_type": "semantic",
  "session_id": null,
  "source": "api"
}
```

| Field | Required | Default | Description |
|-------|----------|---------|-------------|
| `content` | Yes | — | Memory content (min 1 char) |
| `memory_type` | No | `semantic` | One of: `semantic`, `profile`, `procedural`, `working`, `tool_result` |
| `session_id` | No | — | Associate with a session |
| `source` | No | `api` | Source identifier |

### Batch Store

```
POST /v1/memories/batch
```

```json
{
  "memories": [
    {"content": "Fact 1"},
    {"content": "Fact 2", "memory_type": "profile"}
  ]
}
```

### Retrieve Memories

```
POST /v1/memories/retrieve
```

Hybrid retrieval: vector similarity + fulltext search, ranked by relevance.

```json
{
  "query": "programming language preference",
  "top_k": 10,
  "memory_types": ["semantic", "profile"],
  "session_id": null,
  "include_cross_session": true,
  "explain": "none"
}
```

| Field | Required | Default | Description |
|-------|----------|---------|-------------|
| `query` | Yes | — | Search query |
| `top_k` | No | 10 | Max results (1–100) |
| `memory_types` | No | all | Filter by types |
| `session_id` | No | — | Prioritize session memories |
| `include_cross_session` | No | true | Include other sessions |
| `explain` | No | false | `false` = no debug, `true` = show timing, `"verbose"` = detailed metrics, `"analyze"` = full diagnostics |

### Search Memories

```
POST /v1/memories/search
```

```json
{
  "query": "Python",
  "top_k": 10,
  "explain": "none"
}
```

| Field | Required | Default | Description |
|-------|----------|---------|-------------|
| `query` | Yes | — | Search query |
| `top_k` | No | 10 | Max results (1–100) |
| `explain` | No | false | `false` = no debug, `true` = show timing, `"verbose"` = detailed metrics |

Same as retrieve but without session prioritization.

#### Response Format

Both retrieve and search endpoints return:

```json
{
  "results": [
    {
      "memory_id": "abc123",
      "content": "User prefers Python",
      "memory_type": "semantic",
      "confidence": 0.85,
      "score": 0.92
    }
  ],
  "explain": {
    "version": "1.0",
    "level": "basic",
    "path": "graph+vector",
    "total_ms": 45.2,
    "metrics": {
      "results": 5
    }
  }
}
```

**Note**: The `explain` field is only present when the request includes `explain` parameter with a value other than "none". The content varies by level:
- `basic`: execution path, total time
- `verbose`: adds detailed metrics per phase
- `analyze`: includes internal diagnostics

The `explain` field is only present when `explain` parameter is not "none".

### Correct Memory

```
PUT /v1/memories/{memory_id}/correct
```

```json
{
  "new_content": "User now prefers TypeScript",
  "reason": "User changed preference"
}
```

### Correct Memory by Query

```
POST /v1/memories/correct
```

```json
{
  "query": "programming language preference",
  "new_content": "User now prefers TypeScript",
  "reason": "User changed preference"
}
```

Finds the best-matching memory via semantic search and corrects it. Response includes `matched_memory_id` and `matched_content` showing which memory was found.
```

### Delete Memory

```
DELETE /v1/memories/{memory_id}?reason=outdated
```

### Bulk Purge

```
POST /v1/memories/purge
```

```json
{
  "memory_ids": ["id1", "id2"],
  "memory_types": ["working"],
  "before": "2026-01-01T00:00:00",
  "reason": "cleanup"
}
```

All fields are optional. Combine to narrow scope.

Response:

```json
{
  "purged": 2,
  "snapshot_name": "mem_snap_pre_purge_a1b2c3d4",
  "warning": null
}
```

A safety snapshot is automatically created before purge. Use `POST /v1/snapshots/{snapshot_name}/rollback` to undo. `snapshot_name` and `warning` are omitted from the response when null.

If snapshot quota is exhausted, the system auto-deletes the 10 oldest safety snapshots and retries. If still unable to create a snapshot, `warning` contains a message and `snapshot_name` is omitted — the purge still proceeds.

### Observe Turn

```
POST /v1/observe
```

Auto-extract memories from conversation messages.

```json
{
  "messages": [
    {"role": "user", "content": "I prefer dark mode"},
    {"role": "assistant", "content": "Noted, I'll remember that."}
  ],
  "source_event_ids": []
}
```

### Get Profile

```
GET /v1/profiles/me
GET /v1/profiles/{user_id}
```

Returns memory profile with stats:
```json
{
  "user_id": "alice",
  "profile": { ... },
  "stats": {
    "total": 42,
    "by_type": {"semantic": 30, "profile": 8, "procedural": 4},
    "avg_confidence": 0.82,
    "oldest": "2026-01-15T10:00:00",
    "newest": "2026-03-10T21:36:52"
  }
}
```

---

## Snapshots

### Create Snapshot

```
POST /v1/snapshots
```

```json
{
  "name": "before-migration",
  "description": "Snapshot before DB migration"
}
```

Creates a read-only point-in-time snapshot using MatrixOne native snapshots. Max 100 per user.

### List Snapshots

```
GET /v1/snapshots
```

### Get Snapshot Detail

```
GET /v1/snapshots/{name}?detail=brief&limit=50&offset=0
```

| Param | Type | Default | Description |
|-------|------|---------|-------------|
| `detail` | string | `brief` | `brief` (80 char), `normal` (200), `full` (2000 + confidence) |
| `limit` | int | 50 | Max memories (1–500) |
| `offset` | int | 0 | Pagination offset |

Response includes `by_type` distribution and `has_more` flag.

### Delete Snapshot

```
DELETE /v1/snapshots/{name}
```

### Diff Snapshot vs Current

```
GET /v1/snapshots/{name}/diff?limit=50
```

Returns added/removed memories since the snapshot was taken.

```json
{
  "snapshot_count": 40,
  "current_count": 45,
  "added_count": 8,
  "removed_count": 3,
  "unchanged_count": 37,
  "added": [{"memory_id": "...", "memory_type": "semantic", "content": "..."}],
  "removed": [{"memory_id": "...", "memory_type": "semantic", "content": "..."}]
}
```

---

## Governance

### Consolidate

```
POST /v1/consolidate?force=false
```

Detect contradicting memories, fix orphaned graph nodes. 30-minute cooldown per user. Pass `force=true` to skip cooldown.

### Reflect

```
POST /v1/reflect?force=false
```

Analyze memory clusters and synthesize high-level insights. 2-hour cooldown.

**LLM behavior:** Requires LLM configuration (`LLM_API_KEY`). Without LLM, returns HTTP 200 with a degraded response:
```json
{
  "insights": 0,
  "skipped": 0,
  "note": "LLM not configured — reflect unavailable. Set LLM_API_KEY to enable."
}
```

### Extract Entities

```
POST /v1/extract-entities
```

LLM entity extraction for unlinked memories. Builds entity graph nodes and entity_link edges. Idempotent — skips already-linked memories.

**LLM behavior:** Requires LLM configuration (`LLM_API_KEY`). Without LLM, returns HTTP 200 with a degraded response:
```json
{
  "total_memories": 0,
  "entities_found": 0,
  "edges_created": 0,
  "error": "LLM not configured — set LLM_API_KEY to enable."
}
```

Use `/v1/reflect/candidates` and `/v1/extract-entities/candidates` for LLM-free alternatives that return raw data for you to process.

---

## Auth

### Create API Key (Admin)

```
POST /auth/keys
Authorization: Bearer MASTER_KEY
```

```json
{
  "user_id": "alice",
  "name": "alice-laptop",
  "expires_at": "2027-01-01T00:00:00"
}
```

`expires_at` is optional. Auto-creates the user if new. Returns `raw_key` (shown only once).

### List API Keys

```
GET /auth/keys
```

Returns all active keys for the authenticated user. Fields: `key_id`, `user_id`, `name`, `key_prefix`, `created_at`, `expires_at`, `last_used_at`.

### Get API Key

```
GET /auth/keys/{key_id}
```

Returns full details for a single key. `raw_key` is never returned after creation.

### Rotate API Key

```
PUT /auth/keys/{key_id}/rotate
```

Atomically revokes the old key and issues a new one with the same `name`, `user_id`, and `expires_at`. Returns the new key with `raw_key` (shown only once).

### Revoke API Key

```
DELETE /auth/keys/{key_id}
```

---

## Admin

All admin endpoints require `Authorization: Bearer MASTER_KEY`.

### System Stats

```
GET /admin/stats
```

```json
{"total_users": 15, "total_memories": 4200, "total_snapshots": 30}
```

### List Users

```
GET /admin/users?cursor=...&limit=100
```

### User Stats

```
GET /admin/users/{user_id}/stats
```

```json
{"user_id": "alice", "memory_count": 42, "snapshot_count": 3, "api_key_count": 2}
```

### List User's API Keys (Admin)

```
GET /admin/users/{user_id}/keys
```

Returns all active keys for a user with full fields (`expires_at`, `last_used_at`, etc.).

### Revoke All User's API Keys (Admin)

```
DELETE /admin/users/{user_id}/keys
```

```json
{"user_id": "alice", "revoked": 3}
```

### Deactivate User

```
DELETE /admin/users/{user_id}
```

Deactivates user and revokes all API keys.

### Trigger Governance

```
POST /admin/governance/{user_id}/trigger?op=governance
```

| `op` | Description |
|------|-------------|
| `governance` | Run full governance cycle (decay, quarantine, cleanup) |
| `consolidate` | Detect contradictions |
| `reflect` | Synthesize insights (requires LLM) |

---

## Health

```
GET /health
```

No auth required.

```json
{"status": "ok", "database": "connected"}
```

---

## Rate Limits

Per API key, sliding window.

| Operation | Limit |
|-----------|-------|
| `POST /v1/memories` | 300/min |
| `POST /v1/memories/batch` | 60/min |
| `PUT /v1/memories/*/correct` | 120/min |
| `POST /v1/memories/correct` | 120/min |
| `DELETE /v1/memories/*` | 120/min |
| `POST /v1/memories/purge` | 30/min |
| `POST /v1/memories/retrieve` | 300/min |
| `POST /v1/memories/search` | 300/min |
| `POST /v1/observe` | 120/min |
| `GET /v1/snapshots*` | 120/min |
| `POST /v1/snapshots` | 30/min |
| `DELETE /v1/snapshots/*` | 30/min |
| `POST /v1/consolidate` | 10/min |
| `POST /v1/reflect` | 10/min |

Exceeding the limit returns `429 Too Many Requests`.

---

## Audit Trail

Every mutation (inject, correct, purge, governance) is logged to the `mem_edit_log` table with:

| Column | Description |
|--------|-------------|
| `edit_id` | Unique ID |
| `user_id` | User who performed the operation |
| `operation` | `inject`, `correct`, `purge`, `governance:quarantine`, `governance:cleanup_stale`, `governance:archive_working`, `governance:compress_redundant`, `governance:cleanup_orphaned_incrementals` |
| `target_ids` | JSON array of affected memory IDs |
| `reason` | Human-readable reason |
| `snapshot_before` | Safety snapshot name (purge operations only) |
| `created_at` | Timestamp |
| `created_by` | Same as user_id |

Purge operations automatically create a safety snapshot (`mem_snap_pre_purge_*`) before deleting memories. This enables rollback via `POST /v1/snapshots/{name}/rollback`.

---

## Error Responses

All errors follow this format:

```json
{"detail": "Error message"}
```

| Status | Meaning |
|--------|---------|
| 400 | Bad request (validation error) |
| 401 | Missing or invalid API key |
| 403 | Not authorized (e.g., accessing another user's resource) |
| 404 | Resource not found |
| 409 | Conflict (e.g., snapshot name already exists) |
| 429 | Rate limit exceeded |
| 500 | Internal server error |
