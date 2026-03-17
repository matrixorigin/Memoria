# Episodic Memory API

Episodic memory provides session-level summaries that capture what happened during a conversation or work session.

## Overview

- **What**: High-level summaries of sessions (topic/actions/outcomes, or bullet points)
- **When**: Generated manually via API, or auto-triggered during `observe_turn`
- **Storage**: Stored as `EPISODIC` memory type with structured metadata
- **Retrieval**: Automatically included in cross-session memory retrieval

## API Endpoints

### Generate Session Summary

```http
POST /v1/sessions/{session_id}/summary
Content-Type: application/json
Authorization: Bearer <token>

{
  "mode": "full",              // "full" (topic/action/outcome) or "lightweight" (3-5 bullet points)
  "sync": true,                // true = wait for result, false = async with task_id (default)
  "generate_embedding": true   // false = skip embedding (faster, but not retrievable)
}
```

**Modes:**
- `full`: Generates structured topic/action/outcome summary. No rate limit.
- `lightweight`: Generates 3-5 concise bullet points. Rate-limited to **3 per session**.

**Sync Response (sync=true, mode=full):**
```json
{
  "memory_id": "abc123...",
  "content": "Session Summary: Database optimization\n\nActions: ...\n\nOutcome: ...",
  "truncated": false,
  "mode": "full",
  "metadata": {
    "topic": "Database optimization",
    "action": "Analyzed queries, added indexes",
    "outcome": "Query time reduced 93%",
    "source_event_ids": ["mem1", "mem2"]
  }
}
```

**Sync Response (sync=true, mode=lightweight):**
```json
{
  "memory_id": "abc123...",
  "content": "Session Highlights:\n• Optimized DB queries\n• Added indexes\n• 93% faster",
  "truncated": false,
  "mode": "lightweight",
  "metadata": {
    "mode": "lightweight",
    "points": ["Optimized DB queries", "Added indexes", "93% faster"]
  }
}
```

**Async Response (sync=false):**
```json
{
  "task_id": "task_abc123",
  "truncated": false,
  "mode": "full"
}
```

### Poll Task Status

```http
GET /v1/tasks/{task_id}
Authorization: Bearer <token>
```

**Response:**
```json
{
  "task_id": "task_abc123",
  "status": "completed",  // "processing" | "completed" | "failed"
  "created_at": "2026-03-15T10:00:00Z",
  "updated_at": "2026-03-15T10:00:03Z",
  "result": { ... },
  "error": null
}
```

## Auto-Trigger (Phase 2)

When using `POST /v1/observe` with `session_id` and `turn_count`, Memoria can automatically generate lightweight summaries at configurable intervals.

Configure via environment variables:
- `MEMORIA_AUTO_TRIGGER_THRESHOLD=20` — trigger every N turns (0 = disabled, default: 20)
- `MEMORIA_MAX_LIGHTWEIGHT_PER_SESSION=3` — max auto-triggered summaries per session (default: 3)

Auto-triggered summaries are stored with `extra_metadata.auto_triggered=True`.

## LLM Requirement

Episodic memory generation (`POST /v1/sessions/{id}/summary`) **requires LLM configuration**. Without it, the endpoint returns HTTP 503:

```json
{"detail": "LLM not configured — episodic memory generation unavailable"}
```

Configure via environment variables:

```bash
export LLM_API_KEY="sk-..."
export LLM_BASE_URL="https://api.openai.com/v1"   # optional, defaults to OpenAI
export LLM_MODEL="gpt-4o-mini"                     # optional
```

Any OpenAI-compatible endpoint works (SiliconFlow, DeepSeek, Ollama, etc.).

## Limitations

- **Task storage**: In-memory (tasks lost on restart, use `sync=true` for critical sessions)
- **Multi-process**: Task status only visible on the process that created it
- **Lightweight rate limit**: Max 3 per session (configurable via `max_lightweight_per_session`)
- **Input limits**: `full` mode: max 200 messages / 16K tokens; `lightweight`: max 50 messages / 4K tokens

## Troubleshooting

**HTTP 503 "LLM not configured"**: Set `LLM_API_KEY` environment variable and restart the server

**"No memories found"**: Session has no memories, or wrong `session_id`

**"Rate limit exceeded"**: Lightweight mode limited to 3/session — use `mode="full"` instead

**Episodic memory not retrieved**: Ensure stored with `session_id=None` for cross-session retrieval
