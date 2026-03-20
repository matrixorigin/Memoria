# API And MCP

Use this when implementing or calling Memoria endpoints and tools.

## Core Memory Endpoints

- `GET /v1/memories`
- `POST /v1/memories`
- `POST /v1/memories/batch`
- `POST /v1/memories/retrieve`
- `POST /v1/memories/search`
- `PUT /v1/memories/{id}/correct`
- `POST /v1/memories/correct`
- `DELETE /v1/memories/{id}`
- `POST /v1/memories/purge`
- `POST /v1/observe`
- `GET /v1/profiles/me`

## Snapshots And Branches

- `POST /v1/snapshots`
- `GET /v1/snapshots`
- `POST /v1/snapshots/{name}/rollback`
- `POST /v1/branches`
- `GET /v1/branches`
- `POST /v1/branches/{name}/checkout`
- `GET /v1/branches/{name}/diff`
- `POST /v1/branches/{name}/merge`

## Governance

- `POST /v1/governance`
- `POST /v1/consolidate`
- `POST /v1/reflect`
- `POST /v1/extract-entities`
- `POST /v1/extract-entities/link`
- `GET /v1/entities`

## Feedback And Retrieval Tuning

- `POST /v1/memories/{id}/feedback`
- `GET /v1/feedback/stats`
- `GET /v1/feedback/by-tier`
- `POST /v1/retrieval-params/tune`
- `GET /v1/retrieval-params`

## Sessions

- `POST /v1/sessions/{session_id}/summary`
- `GET /v1/tasks/{task_id}`

## Auth

- `POST /auth/keys`
- `GET /auth/keys`
- `GET /auth/keys/{id}`
- `PUT /auth/keys/{id}/rotate`
- `DELETE /auth/keys/{id}`

## Admin

- `GET /admin/stats`
- `GET /admin/users`
- `POST /admin/governance/{id}/trigger`
- `GET/POST /admin/plugins/signers`
- `POST /admin/plugins`
- `POST /admin/plugins/:key/:ver/review`

## Health

- `GET /health`
- `GET /health/instance`

## Common MCP Tool Surface

- `memory_store`, `memory_retrieve`, `memory_search`, `memory_list`
- `memory_profile`, `memory_correct`, `memory_purge`
- `memory_snapshot`, `memory_snapshots`, `memory_rollback`
- `memory_branch`, `memory_branches`, `memory_checkout`, `memory_merge`, `memory_diff`
- `memory_governance`, `memory_consolidate`, `memory_reflect`

## Important Notes

- `retrieve` is hybrid vector and full-text retrieval.
- `purge` creates a safety snapshot automatically.
- Reflection and entity extraction may require an LLM-capable configuration.
- Rate limits are per API key and return `429` on overflow.
