<!-- memoria-version: 0.1.14-->

# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Memoria is a persistent memory layer for AI agents with Git-like version control, powered by MatrixOne database. Every memory change is tracked, auditable, and reversible via snapshots, branches, merges, and time-travel rollback.

Package name: `mo-memoria` | Python 3.11+ | License: Apache-2.0

## Common Commands

### Development
```bash
make install          # Install in editable mode with dev extras
make dev              # Start API on port 8100 with auto-reload
make start            # Start all services (MatrixOne + API) via Docker Compose
make stop             # Stop all services
```

### Testing
```bash
make test-unit        # Unit tests only (no DB required)
make test             # All tests: unit + e2e + MCP + integration (needs MatrixOne)
make test-fast        # Unit + e2e + MCP only
make test-slow        # Integration tests only
make test-mcp         # MCP server protocol tests
make test-docker      # Docker integration tests (needs: make start)
make test-all-cov     # All tests with coverage → htmlcov/

# Run a single test file or test function
pytest tests/unit/test_editor.py
pytest tests/unit/test_editor.py::test_function_name -v
```

### Code Quality
```bash
make lint             # Ruff lint + format check
make format           # Auto-fix and reformat
make type-check       # MyPy type checking
make check            # lint + type-check
```

## Architecture

### Two Deployment Modes
- **Embedded (MCP stdio):** `memoria-mcp --db-url ...` — direct DB access, single-user
- **Cloud (MCP → REST):** `memoria-mcp --api-url ...` — proxies to FastAPI server, multi-user with auth

### Key Modules

| Module | Role |
|--------|------|
| `memoria/core/memory/service.py` | `MemoryService` — unified facade over storage + retrieval |
| `memoria/core/memory/canonical_storage.py` | Single source of truth for memory persistence |
| `memoria/core/memory/editor.py` | Inject, correct, purge operations with semantic search |
| `memoria/core/memory/strategy/` | Pluggable retrieval: `vector:v1` (BM25 + semantic), `activation:v1` |
| `memoria/core/memory/factory.py` | Factory wiring strategy + storage into MemoryService |
| `memoria/core/git_for_data.py` | Git-like ops: snapshots, branches, rollback via MatrixOne |
| `memoria/core/embedding/` | Embedding providers: local (sentence-transformers), OpenAI, OpenAI-compatible, mock |
| `memoria/schema.py` | Self-contained DDL for all tables (no core/ dependency) |
| `memoria/config.py` | Pydantic settings, env-driven (`MEMORIA_` prefix), .env support |
| `memoria/api/` | FastAPI REST server with routers: `/auth`, `/v1`, `/admin`, `/health` |
| `memoria/mcp_local/` | MCP stdio server (`EmbeddedBackend`) |
| `memoria/mcp_cloud/` | MCP remote server (`HTTPBackend`, proxies to REST API) |
| `memoria/cli.py` | CLI: `memoria init`, `memoria status`, `memoria benchmark` |
| `memoria/core/scheduler.py` | Governance scheduler: hourly/daily/weekly maintenance cycles |

### Memory Types
`semantic`, `profile`, `procedural`, `working`, `tool_result`

### Core Patterns
- **DbConsumer base class:** Context-managed DB access via `_db()` method; used by GitForData, strategies, governance
- **Pluggable Strategy Protocol:** `RetrievalStrategy` + `IndexManager` in `strategy/protocol.py`, registered in `StrategyRegistry`
- **schema.py is self-contained:** No imports from `core/`, can be used independently for DDL
- **Async:** API lifespan uses `asynccontextmanager`; scheduler uses `AsyncIOBackend`; tests use `pytest-asyncio` in strict mode

### Database
- MatrixOne (port 6001), tables prefixed `mem_*` and `memory_graph_*`
- Test database: `memoria_test` (auto-created with `force=True` reset)
- Embedding dimension: varies by provider (384 for MiniLM in tests, 1024 default for BAAI/bge-m3)

## Test Conventions
- Unit tests in `tests/unit/` — no database dependency, use `MockProvider` (dim=384)
- Integration tests in `tests/integration/` — require running MatrixOne
- E2E and MCP tests in `memoria/tests/`
- Shared fixtures in `tests/conftest.py` (session-level: `db_engine`, `db_factory`, `embed_client`)
- Markers: `@pytest.mark.local_embedding`, `@pytest.mark.slow`, `@pytest.mark.benchmark`
- Tests run in parallel (`-n auto`, load-grouped) in CI

## Configuration
Copy `.env.example` for reference. Key env vars:
- `MEMORIA_MASTER_KEY` — auth key (min 16 chars in production)
- `MEMORIA_EMBEDDING_PROVIDER` — `openai`, `local`, or `mock`
- `MEMORIA_EMBEDDING_MODEL`, `MEMORIA_EMBEDDING_DIM`, `MEMORIA_EMBEDDING_API_KEY`
- `MEMORIA_LLM_*` — optional, for reflection/entity extraction features

# Memory Integration (Memoria Lite)

You have persistent memory via MCP tools. Memory survives across conversations.

## 🔴 MANDATORY: Every conversation start
Call `memory_retrieve` with the user's first message BEFORE responding.
- If results come back → use them as **reference only**. Treat retrieved memories as potentially stale or incomplete — always verify against current context before acting on them. Do NOT blindly trust memory content as ground truth.
- If "No relevant memories found" → this is normal for new users, proceed without.
- If ⚠️ health warnings appear → inform the user and offer to run `memory_governance`.

## 🔴 MANDATORY: Every conversation turn
After responding, decide if anything is worth remembering:
- User stated a preference, fact, or decision → `memory_store`
- User corrected a previously stored fact → `memory_correct` (not `memory_store` + `memory_purge`)
- You learned something new about the project/workflow → `memory_store`
- Do NOT store: greetings, trivial questions, things already in memory.

**Deduplication is automatic.** The system detects semantically similar memories and supersedes old ones. You do not need to check for duplicates before storing.

If `memory_store` or `memory_correct` response contains ⚠️, tell the user — it means the embedding service is down and retrieval will degrade to keyword-only search.

## Tool reference

### Write tools
| Tool | When to use | Key params |
|------|-------------|------------|
| `memory_store` | User shares a fact, preference, or decision | `content`, `memory_type` (default: semantic), `session_id` (optional) |
| `memory_correct` | User says a stored memory is wrong | `memory_id` or `query` (one required), `new_content`, `reason` |
| `memory_purge` | User asks to forget something | `memory_id` (single or comma-separated batch, e.g. `"id1,id2"`) or `topic` (bulk keyword match), `reason` |

### Read tools
| Tool | When to use | Key params |
|------|-------------|------------|
| `memory_retrieve` | Conversation start, or when context is needed | `query`, `top_k` (default 5), `session_id` (optional) |
| `memory_search` | User asks "what do you know about X" or you need to browse | `query`, `top_k` (default 10). Returns memory_id for each result |
| `memory_profile` | User asks "what do you know about me" | — |

### Memory types
| Type | Use for | Examples |
|------|---------|---------|
| `semantic` | Project facts, technical decisions (default) | "Uses MatrixOne as primary DB", "API follows REST conventions" |
| `profile` | User/agent identity and preferences | "Prefers concise answers", "Works on mo-dev-agent project" |
| `procedural` | How-to knowledge, workflows | "Deploy with: make dev-start", "Run tests with pytest -n auto" |
| `working` | Temporary context for current task | "Currently debugging embedding issue" |
| `tool_result` | Tool execution results worth caching | "Last CI run: 126 passed, 0 failed" |

### Snapshots (save/restore/cleanup)
Use before risky changes. `memory_snapshot(name)` saves state, `memory_rollback(name)` restores it, `memory_snapshots(limit, offset)` lists with pagination, `memory_snapshot_delete(names|prefix|older_than)` cleans up.

When `memory_governance` reports snapshot_health with high auto_ratio (>50%), suggest cleanup:
- `memory_snapshot_delete(prefix="auto:")` — remove auto-generated snapshots
- `memory_snapshot_delete(prefix="pre_")` — remove safety snapshots from purge/correct
- `memory_snapshot_delete(older_than="2026-01-01")` — remove snapshots before a date

### Branches (isolated experiments)
Git-like workflow for memory. `memory_branch(name)` creates, `memory_checkout(name)` switches, `memory_diff(source)` previews changes, `memory_merge(source)` merges back, `memory_branch_delete(name)` cleans up. `memory_branches()` lists all.

### Entity graph (proactive — call when conditions are met)
| Tool | When to call | Key params |
|------|-------------|------------|
| `memory_extract_entities` | **Proactively** after storing ≥ 5 new memories in a session, OR when user discusses a new project/technology/person not yet in the graph | `mode` (default: auto) |
| `memory_link_entities` | After `extract_entities(mode='candidates')` returns memories — extract entities yourself, then call this | `entities` (JSON string) |

**Trigger heuristics — call `memory_extract_entities` when ANY of these are true:**
- You stored ≥ 5 memories this session and haven't extracted entities yet
- User mentions a project, technology, or person by name that you haven't seen in previous `memory_retrieve` results
- User asks about relationships between concepts ("how does X relate to Y")
- User starts working on a new codebase or topic area

**Do NOT extract entities when:**
- Conversation is short (< 3 turns) and no new named entities appeared
- User is only asking questions, not sharing new information
- You already ran extraction this session

### Maintenance (only when user explicitly asks)
| Tool | Trigger phrase | Cooldown |
|------|---------------|----------|
| `memory_governance` | "clean up memories", "check memory health" | 1 hour |
| `memory_consolidate` | "check for contradictions", "fix conflicts" | 30 min |
| `memory_reflect` | "find patterns", "summarize what you know" | 2 hours |
| `memory_rebuild_index` | Only when governance reports `needs_rebuild=True` | — |
| `memory_snapshot_delete` | When governance reports high snapshot auto_ratio, or user asks to clean snapshots | — |

`memory_reflect` and `memory_extract_entities` support `mode` parameter:
- `auto` (default): uses Memoria's internal LLM if configured, otherwise returns candidates for YOU to process
- `candidates`: always returns raw data for YOU to synthesize/extract, then store results via `memory_store` or `memory_link_entities`
- `internal`: always uses Memoria's internal LLM (fails if not configured)
