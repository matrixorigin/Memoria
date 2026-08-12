# Changelog

## [Unreleased]

### Added
- Sync and async `memories.fulltext_search()` for pure MatrixOne full-text search with
  exact scalar `extra_metadata_filter` and fixed-field SQL pre-filters, without vector or
  graph retrieval. Session filtering is strict and the endpoint is intentionally not exposed
  by MCP.

### Fixed
- `ping()` no longer wraps `MemoriaAuthError` / `MemoriaNotFoundError` and other API errors
  into `MemoriaConnectionError`; callers can now distinguish network failures from API errors.
- `_map_error`: empty response body no longer produces duplicate status code in the error
  message (e.g. `"HTTP 404: HTTP 404"` → `"HTTP 404: Not Found"`).

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
