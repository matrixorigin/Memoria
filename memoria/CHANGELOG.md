# Changelog — Memoria Rust

## [Unreleased] — Phase 4 Complete

### Added

**Admin API** (`GET/DELETE /admin/*`, `POST /admin/governance/:id/trigger`)
- User listing, per-user stats, user deletion
- Trigger governance/consolidate per user on demand

**Sensitivity / PII Filter** (`crates/memoria-core/src/sensitivity.rs`)
- Three-tier classification: HIGH (block) / MEDIUM (redact) / LOW (pass)
- Patterns: AWS keys, private keys, bearer tokens, password assignments, email, phone, SSN, credit card
- Integrated into `MemoryService::store_memory()` — blocked content returns a friendly error

**Governance Scheduler** (`crates/memoria-service/src/scheduler.rs`)
- Hourly: governance sweep
- Daily: consolidation
- Weekly: reflection
- Off by default; enable with `MEMORIA_GOVERNANCE_ENABLED=true`

**Health Endpoints**
- `GET /v1/health/analyze` — detect contaminated/orphaned memories
- `GET /v1/health/storage` — row counts, embedding coverage, graph stats
- `GET /v1/health/capacity` — IVF index health, rebuild recommendations

**Sandbox Validation** (`MemoryService::validate_in_sandbox`)
- Zero-copy branch pre-validation before committing candidates
- Fail-open: validation errors never block writes

**EXPLAIN Support**
- `GET /v1/memories/retrieve?explain=true` and `GET /v1/memories/search?explain=true`
- Returns timing breakdown and retrieval path alongside results

**Typed Memory Pipeline** (`POST /v1/pipeline/run`)
- Sensitivity → Sandbox → Persist in a single request
- Per-candidate result: stored / rejected / redacted

### Changed

- `MemoryService::embed()` is now `pub` (needed by pipeline and explain)
- `RetrieveRequest` gains `explain: bool` field

### Fixed (MO Bug Workarounds)

- **MO#23859**: `extra_metadata` NULL binding corrupts ByteJson on 3rd+ prepared-statement execution — workaround: always bind `"{}"` instead of `None`
- **MO#23860**: snapshot restore write-write conflict under concurrent tests — workaround: serialize snapshot tests with `#[serial]`
- **MO#23861**: `FULLTEXT INDEX` DDL fails during concurrent snapshot restore — workaround: annotated, index retained

### Tests

- 212 tests, 0 failed (up from 197 at Phase 3.5)

---

## Phase 3.5 — Test Hardening

- All 14 snapshot e2e tests serialized (`#[serial]`)
- `Makefile` auto-loads `.env` for embedding config
- `git_ops.rs` setup uses full `migrate()` and reads `EMBEDDING_DIM` from env
- MO bug workarounds annotated with issue numbers

## Phase 3 — Graph Retrieval

- Spreading activation over entity graph
- NER extraction (regex + LLM candidates mode)
- Contradiction detection and consolidation
- `memory_reflect` with cluster synthesis

## Phase 2 — Git-for-Data

- Zero-copy snapshot / branch / merge via MatrixOne CoW
- Point-in-time rollback
- LCA-based diff with semantic classification (new / conflict / modified / removed)
- `memory_diff` preview before merge

## Phase 1 — Core

- `memoria-core`: types (`Memory`, `MemoryType`, `TrustTier`, `MemoriaError`)
- `memoria-storage`: `SqlMemoryStore` — CRUD, vector search, fulltext search, hybrid retrieval
- `memoria-service`: `MemoryService` — business logic layer
- `memoria`: Unified binary — `memoria serve` (REST API), `memoria mcp` (MCP stdio server), `memoria init/status/update-rules/benchmark` (CLI tools)
- `memoria-git`: snapshot/branch/merge service
