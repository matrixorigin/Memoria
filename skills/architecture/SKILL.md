---
name: architecture
description: Memoria codebase structure, workspace layout, key traits, database tables, config patterns, and testing conventions. Use when navigating or modifying Memoria code.
---

## Workspace Layout

```
memoria/                          # Cargo workspace root
├── crates/
│   ├── memoria-core/             # Shared types: MemoriaError, Memory, MemoryType
│   ├── memoria-storage/          # SqlMemoryStore — all SQL queries, migrations
│   ├── memoria-service/          # Business logic
│   │   └── src/
│   │       ├── service.rs        # MemoryService — main entry point
│   │       ├── scheduler.rs      # GovernanceScheduler — periodic tasks + leader election
│   │       ├── config.rs         # Config struct (reads env vars)
│   │       ├── distributed.rs    # DistributedLock, AsyncTaskStore traits + SQL impls
│   │       ├── governance/       # GovernanceStrategy trait, DefaultGovernanceStrategy
│   │       └── plugin/           # Plugin system (manifest, repository, rhai, grpc)
│   ├── memoria-api/              # REST API (axum)
│   │   └── src/
│   │       ├── lib.rs            # build_router(), route registration
│   │       ├── state.rs          # AppState (service, task_store, instance_id)
│   │       └── routes/           # memory, admin, governance, plugins, sessions
│   ├── memoria-mcp/              # MCP server (stdio/SSE transport)
│   ├── memoria-cli/              # CLI binary: init, mcp, serve, plugin, benchmark
│   ├── memoria-embedding/        # Embedding + LLM client
│   └── memoria-git/              # Git-for-Data: snapshots, branches, merge
├── Cargo.toml                    # Workspace deps
└── build.rs                      # Proto compilation (tonic)
```

## Key Traits

```rust
trait GovernanceStrategy: Send + Sync {
    fn strategy_key(&self) -> &str;
    async fn plan(&self, store, task) -> Result<GovernancePlan>;
    async fn execute(&self, store, task, plan) -> Result<GovernanceExecution>;
}

trait GovernanceStore: Send + Sync {
    async fn list_active_users(&self) -> Result<Vec<String>>;
    async fn quarantine_low_confidence(&self, user) -> Result<i64>;
    // ... cleanup/maintenance methods
}

trait DistributedLock: Send + Sync {
    async fn try_acquire(&self, key, holder, ttl) -> Result<bool>;
    async fn renew(&self, key, holder, ttl) -> Result<bool>;
    async fn release(&self, key, holder) -> Result<()>;
}

trait AsyncTaskStore: Send + Sync {
    async fn create_task(&self, task) -> Result<()>;
    async fn complete_task(&self, task_id, result) -> Result<()>;
    async fn fail_task(&self, task_id, error) -> Result<()>;
    async fn get_task(&self, task_id) -> Result<Option<AsyncTask>>;
}
```

## Database Tables

| Group | Tables |
|-------|--------|
| Core | `mem_memories`, `mem_user_state`, `mem_branches`, `mem_snapshots` |
| Graph | `memory_graph_nodes`, `memory_graph_edges`, `mem_entities`, `mem_memory_entity_links`, `mem_entity_links` |
| Audit | `mem_edit_log`, `mem_retrieval_feedback`, `mem_memories_stats` |
| Governance | `mem_governance_cooldown`, `mem_governance_runtime_state`, `mem_user_retrieval_params` |
| Auth | `mem_api_keys` |
| Plugin | `mem_plugin_packages`, `mem_plugin_signers`, `mem_plugin_bindings`, `mem_plugin_binding_rules`, `mem_plugin_reviews`, `mem_plugin_audit_events` |
| Distributed | `mem_distributed_locks`, `mem_async_tasks` |

## Config

`Config` struct in `config.rs` reads from env vars:

| Field | Env Var | Default |
|-------|---------|---------|
| `db_url` | `DATABASE_URL` | `mysql://root:111@localhost:6001/memoria` |
| `embedding_provider` | `EMBEDDING_PROVIDER` | `local` |
| `instance_id` | `MEMORIA_INSTANCE_ID` | Random UUID |
| `lock_ttl_secs` | `MEMORIA_LOCK_TTL_SECS` | `120` |
| `governance_plugin_dir` | `MEMORIA_GOVERNANCE_PLUGIN_DIR` | None |
| `governance_plugin_binding` | `MEMORIA_GOVERNANCE_PLUGIN_BINDING` | `default` |
| `governance_plugin_subject` | `MEMORIA_GOVERNANCE_PLUGIN_SUBJECT` | `system` |

**When adding a Config field:** update ALL test `Config { .. }` constructors — check `scheduler.rs` tests AND `tests/plugin_repository.rs`.

## REST API Structure

| Prefix | Module | Auth | Purpose |
|--------|--------|------|---------|
| `/v1/memories` | `routes/memory.rs` | Bearer | CRUD, search, retrieve |
| `/v1/snapshots`, `/v1/branches` | `routes/memory.rs` | Bearer | Git-for-Data |
| `/v1/sessions` | `routes/sessions.rs` | Bearer | Episodic memory, async tasks |
| `/v1/governance` | `routes/governance.rs` | Bearer | Trigger governance |
| `/admin/*` | `routes/admin.rs` | Master | Admin ops |
| `/admin/plugins/*` | `routes/plugins.rs` | Master | Plugin repository |
| `/health` | `routes/admin.rs` | None | Liveness probe |
| `/health/instance` | `routes/memory.rs` | None | Readiness probe (returns instance_id) |

## Adding a New REST Endpoint

1. Add handler in `memoria-api/src/routes/`
2. Register route in `memoria-api/src/lib.rs` `build_router()`
3. Add e2e test in `memoria-api/tests/api_e2e.rs` using `spawn_server()`

## Plugin System Files

```
plugin/
├── mod.rs              # Re-exports
├── manifest.rs         # PluginManifest, PluginPackage, signing verification
├── repository.rs       # Publish, review, score, binding rules, audit
├── rhai_runtime.rs     # RhaiGovernanceStrategy (sandboxed Rhai)
├── grpc_runtime.rs     # GrpcGovernanceStrategy (remote gRPC)
├── governance_hook.rs  # Contract testing harness
└── templates/          # Rhai governance template
```

## Distributed Components

| Component | File | Purpose |
|-----------|------|---------|
| `DistributedLock` trait | `distributed.rs` | Lock abstraction |
| `NoopDistributedLock` | `distributed.rs` | Single-instance no-op |
| `SqlMemoryStore` lock impl | `distributed.rs` | INSERT-based DB lock with TTL |
| `AsyncTaskStore` trait | `distributed.rs` | Cross-instance task visibility |
| `GovernanceScheduler` | `scheduler.rs` | Leader election + heartbeat |
| `AppState` | `state.rs` | Holds instance_id + DB task store |

## Testing

```bash
make check          # cargo check + clippy -D warnings (MUST pass)
make test-unit      # Unit tests (no DB): memoria-core, memoria-service, memoria-mcp
make test           # All tests (needs MatrixOne, --test-threads=1)
make test-e2e       # API e2e tests only
```

Patterns:
- E2e: `spawn_server()` → random port, shared DB, reqwest client
- Distributed e2e: `spawn_server_with_instance()` → custom instance ID
- Plugin: `build_signed_plugin_files()` helper for base64 file maps
- Unique names: `uuid::Uuid::new_v4().simple()` for test isolation
- DB tests need `DATABASE_URL` env var

## Common Pitfalls

1. Adding Config fields → must update ALL test constructors
2. Clippy `-D warnings` → any warning = build failure
3. `PathBuf::from("x")` in comparisons → use `Path::new("x")`
4. `format!()` inside `println!()` → inline args directly
5. Plugin exports → re-export chain: `repository.rs` → `plugin/mod.rs` → `lib.rs`
6. Shared DB in tests → use unique names for isolation
7. `GovernanceScheduler` constructors → `#[allow(clippy::too_many_arguments)]`
8. MatrixOne DATETIME columns → use `chrono::NaiveDateTime`, not `String`
