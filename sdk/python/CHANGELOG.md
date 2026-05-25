# Changelog

## [1.0.0] - 2026-05-25

### Added
- Initial release of the Memoria Python SDK
- `MemoriaClient` (sync) and `AsyncMemoriaClient` (async) with identical interfaces
- Full memories resource: store, store_batch, retrieve, search, list, correct, correct_by_query,
  delete, purge, feedback
- observe endpoint for session memory extraction
- profile.me()
- snapshots: create, list, rollback, delete (single + bulk/prefix/date)
- branches: create, list, checkout, diff, diff_items, merge, delete, apply, pick
- governance: run, consolidate, reflect
- ping / health check
- Context-manager support (`with` / `async with`) for connection lifecycle
- Structured exception hierarchy: MemoriaAuthError, MemoriaForbiddenError,
  MemoriaNotFoundError, MemoriaUnprocessableError, MemoriaServerError, MemoriaConnectionError
- Exponential-backoff retry on 5xx and network errors (configurable max_retries)
- dataclasses response models — zero extra dependencies beyond httpx
