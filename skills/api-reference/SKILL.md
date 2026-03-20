---
name: api-reference
description: Memoria REST API endpoints, request/response formats, auth, rate limits. Use when calling or implementing API endpoints.
---

## Memory CRUD

### List: `GET /v1/memories?limit=50&cursor=...&memory_type=semantic`

Response: `{ "items": [...], "next_cursor": "..." }`

### Store: `POST /v1/memories`

```json
{ "content": "...", "memory_type": "semantic", "session_id": null }
```
Returns `201` with `MemoryResponse`.

Types: `semantic` (default), `profile`, `procedural`, `working`, `tool_result`

### Batch Store: `POST /v1/memories/batch`

```json
{ "memories": [{ "content": "..." }, { "content": "...", "memory_type": "profile" }] }
```

### Retrieve: `POST /v1/memories/retrieve`

Hybrid vector + fulltext search, ranked by relevance.

```json
{ "query": "...", "top_k": 10, "memory_types": ["semantic"], "session_id": null, "explain": false }
```

`explain`: `false` | `true` (timing) | `"verbose"` (detailed) | `"analyze"` (full diagnostics)

### Search: `POST /v1/memories/search`

```json
{ "query": "...", "top_k": 10, "explain": false }
```

Same as retrieve but without session prioritization.

### Correct by ID: `PUT /v1/memories/{id}/correct`

```json
{ "new_content": "...", "reason": "..." }
```

### Correct by Query: `POST /v1/memories/correct`

```json
{ "query": "...", "new_content": "...", "reason": "..." }
```

Finds best match via semantic search, corrects it. Response includes `matched_memory_id`.

### Delete: `DELETE /v1/memories/{id}?reason=...`

### Bulk Purge: `POST /v1/memories/purge`

```json
{ "memory_ids": ["id1"], "memory_types": ["working"], "before": "2026-01-01T00:00:00", "reason": "..." }
```

All fields optional. Auto-creates safety snapshot. Response: `{ "purged": N, "snapshot_name": "..." }`

### Observe: `POST /v1/observe`

```json
{ "messages": [{ "role": "user", "content": "..." }] }
```

### Profile: `GET /v1/profiles/me`

## Snapshots

| Endpoint | Description |
|----------|-------------|
| `POST /v1/snapshots` | Create: `{ "name": "...", "description": "..." }` |
| `GET /v1/snapshots` | List all |
| `GET /v1/snapshots/{name}?detail=brief&limit=50&offset=0` | Detail (`brief`/`normal`/`full`) |
| `DELETE /v1/snapshots/{name}` | Delete |
| `GET /v1/snapshots/{name}/diff?limit=50` | Diff vs current state |
| `POST /v1/snapshots/{name}/rollback` | Restore to snapshot |

## Branches

| Endpoint | Description |
|----------|-------------|
| `POST /v1/branches` | Create: `{ "name": "..." }` |
| `GET /v1/branches` | List all |
| `POST /v1/branches/{name}/checkout` | Switch to branch |
| `GET /v1/branches/{name}/diff` | Preview changes vs main |
| `POST /v1/branches/{name}/merge` | Merge into main: `{ "strategy": "append" }` |
| `DELETE /v1/branches/{name}` | Delete |

## Governance

| Endpoint | Cooldown | Description |
|----------|----------|-------------|
| `POST /v1/governance?force=false` | 1 hour | Quarantine low-confidence, cleanup stale |
| `POST /v1/consolidate?force=false` | 30 min | Detect contradictions, fix orphans |
| `POST /v1/reflect?force=false` | 2 hours | Synthesize insights (needs LLM) |
| `POST /v1/extract-entities` | — | Extract entities, build graph (needs LLM) |
| `POST /v1/extract-entities/link` | — | Manually link entities to memories |
| `GET /v1/entities` | — | List user's entities |

LLM-free alternatives: `POST /v1/reflect/candidates`, `POST /v1/extract-entities/candidates` — return raw data for the calling agent to process.

## Feedback & Adaptive Retrieval

Feedback signals improve retrieval ranking over time. The system learns which memories are useful/irrelevant for each user.

### How It Works

1. User retrieves memories via `memory_retrieve` or `memory_search`
2. Agent uses memories to answer questions
3. Agent calls `memory_feedback` with signal based on outcome
4. System adjusts `feedback_weight` parameter (auto-tuned daily by governance)
5. Future retrievals rank memories higher/lower based on accumulated feedback

**Quantified Impact**: With default `feedback_weight=0.1`, a memory with 3 `useful` signals scores ~1.3x higher; one with 2 `wrong` signals scores ~0.9x lower. At `feedback_weight=0.3`, these become ~1.9x and ~0.7x respectively.

### Record Feedback: `POST /v1/memories/{id}/feedback`

```json
{ "signal": "useful", "context": "helped answer the question" }
```

Signals: `useful`, `irrelevant`, `outdated`, `wrong`

Returns `201` with `{ "feedback_id": "..." }`

**Errors**:
- `404`: Memory not found
- `422`: Invalid signal value

### Get Stats: `GET /v1/feedback/stats`

Returns aggregated feedback counts:
```json
{ "useful": 42, "irrelevant": 5, "outdated": 3, "wrong": 1 }
```

### Get by Tier: `GET /v1/feedback/by-tier`

Returns feedback breakdown by trust tier (T1-T4).

### Tune Parameters: `POST /v1/retrieval-params/tune`

Manually adjust retrieval scoring weights:
```json
{ "feedback_weight": 0.15 }
```

`feedback_weight`: 0.01–0.5 (default 0.1). Higher = feedback has more impact on ranking.

Auto-tuning: `POST /v1/retrieval-params/tune` with empty body triggers automatic tuning based on accumulated feedback.

**Errors**:
- `422`: `feedback_weight` out of range
- `200` with `"message"`: Not enough feedback (requires ≥10 signals)

### Get Parameters: `GET /v1/retrieval-params`

Returns current retrieval parameters for the user.

### Related Tools

| Tool | Relationship |
|------|-------------|
| `memory_retrieve` / `memory_search` | Feedback affects their ranking results |
| `memory_correct` | Use instead of `wrong` feedback when content needs fixing |
| `memory_purge` | Use instead of `outdated` feedback when memory should be deleted |
| `memory_governance` | Auto-tunes `feedback_weight` daily based on feedback patterns |

## Episodic Memory

### Generate Summary: `POST /v1/sessions/{session_id}/summary`

```json
{ "mode": "full", "sync": true, "generate_embedding": true }
```

Modes: `full` (topic/action/outcome) | `lightweight` (3-5 bullets, max 3/session)

Requires LLM (`LLM_API_KEY`). Returns 503 without it.

### Poll Task: `GET /v1/tasks/{task_id}`

Response: `{ "task_id": "...", "status": "completed|processing|failed", "result": {...} }`

## Auth

| Endpoint | Auth | Description |
|----------|------|-------------|
| `POST /auth/keys` | Master | Create API key: `{ "user_id": "...", "name": "..." }` |
| `GET /auth/keys` | Bearer | List my keys |
| `GET /auth/keys/{id}` | Bearer | Get key detail |
| `PUT /auth/keys/{id}/rotate` | Bearer | Rotate (revoke old, issue new) |
| `DELETE /auth/keys/{id}` | Bearer | Revoke |

## Admin (Master key required)

| Endpoint | Description |
|----------|-------------|
| `GET /admin/stats` | System stats |
| `GET /admin/users?cursor=...&limit=100` | List users |
| `GET /admin/users/{id}/stats` | User stats |
| `GET /admin/users/{id}/keys` | User's API keys |
| `DELETE /admin/users/{id}/keys` | Revoke all user keys |
| `DELETE /admin/users/{id}` | Deactivate user |
| `POST /admin/governance/{id}/trigger?op=governance` | Trigger governance (`governance`/`consolidate`/`reflect`) |

## Plugin Admin (Master key required)

| Endpoint | Description |
|----------|-------------|
| `GET/POST /admin/plugins/signers` | List/add trusted signers |
| `POST /admin/plugins` | Publish plugin (base64 files) |
| `POST /admin/plugins/:key/:ver/review` | Review: `{ "status": "active" }` |
| `POST /admin/plugins/:key/:ver/score` | Score: `{ "score": 4.5 }` |
| `GET/POST /admin/plugins/domains/:d/bindings` | List/create binding rules |
| `POST /admin/plugins/domains/:d/activate` | Activate binding |
| `GET /admin/plugins` | List packages |
| `GET /admin/plugins/matrix` | Compatibility matrix |
| `GET /admin/plugins/events` | Audit events |

## Health (no auth)

| Endpoint | Response | Use |
|----------|----------|-----|
| `GET /health` | `"ok"` | Liveness probe |
| `GET /health/instance` | `{ "status": "ok", "instance_id": "..." }` | Readiness probe |

## Rate Limits

Per API key, sliding window. Key limits: store 300/min, retrieve 300/min, batch 60/min, purge 30/min, consolidate/reflect 10/min. Returns `429` when exceeded.

## Error Format

```json
{ "detail": "Error message" }
```

Status codes: 400 (validation), 401 (auth), 403 (forbidden), 404 (not found), 409 (conflict), 429 (rate limit), 500 (internal).
