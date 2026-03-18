# SKILLS.md — Coding Agent Reference for Memoria

> This file helps AI coding agents understand the Memoria codebase quickly.
> It is NOT a user-facing doc — it's a machine-readable project map.

## Project Identity

Memoria is a persistent memory layer for AI agents with Git-level version control, built in Rust.
Storage backend: MatrixOne (distributed DB with native vector indexing).
Protocol: MCP (Model Context Protocol) over stdio/SSE.

## Workspace Layout

```
memoria/                          # Cargo workspace root
├── crates/
│   ├── memoria-core/             # Shared types: MemoriaError, Memory, MemoryType
│   ├── memoria-storage/          # SqlMemoryStore — all SQL queries, migrations
│   ├── memoria-service/          # Business logic
│   │   └── src/
│   │       ├── service.rs        # MemoryService — main entry point for all operations
│   │       ├── scheduler.rs      # GovernanceScheduler — periodic governance tasks
│   │       ├── config.rs         # Config struct (reads env vars)
│   │       ├── governance/       # GovernanceStrategy trait, DefaultGovernanceStrategy
│   │       └── plugin/           # Plugin system (see below)
│   ├── memoria-api/              # REST API (axum)
│   │   └── src/
│   │       ├── lib.rs            # build_router(), AppState
│   │       └── routes/           # Handler modules: memory, admin, governance, plugins
│   ├── memoria-mcp/              # MCP server (stdio/SSE transport)
│   ├── memoria-cli/              # CLI binary: memoria init, mcp, serve, plugin, ...
│   ├── memoria-embedding/        # Embedding + LLM client
│   └── memoria-git/              # Git-for-Data: snapshots, branches, merge (via MatrixOne)
├── Cargo.toml                    # Workspace definition + shared deps
└── build.rs                      # Proto compilation (tonic)
```

## Plugin System Architecture

```
plugin/
├── mod.rs              # Re-exports everything
├── manifest.rs         # PluginManifest, PluginPackage, load_plugin_package, signing verification
├── repository.rs       # Plugin repository: publish, review, score, binding rules, audit events
├── rhai_runtime.rs     # RhaiGovernanceStrategy — sandboxed Rhai script execution
├── grpc_runtime.rs     # GrpcGovernanceStrategy — remote gRPC plugin via tonic
├── governance_hook.rs  # GovernancePluginContractHarness — contract testing framework
└── templates/          # Rhai governance template (scaffolding)
```

### Plugin Lifecycle
1. `publish_plugin_package(store, dir, actor)` — verify signature, insert into `mem_plugin_packages`
2. `review_plugin_package(store, key, version, status, notes, actor)` — pending → active/rejected
3. `upsert_plugin_binding_rule(store, input)` — bind plugin to domain/subject with semver selector
4. `activate_plugin_binding(store, domain, binding, key, version, actor)` — activate specific version
5. `load_active_governance_plugin(store, binding, subject, delegate)` — scheduler loads at startup

### Dev Mode (skip signature + auto-approve)
- `publish_plugin_package_dev(store, dir, actor)` — uses `HostPluginPolicy::development()`
- `MEMORIA_GOVERNANCE_PLUGIN_DIR` env var — load from local filesystem, no DB needed
- `build_local_governance_strategy(package, delegate)` — create strategy from local PluginPackage

## Key Traits

```rust
// Core governance interface — all plugins implement this
trait GovernanceStrategy: Send + Sync {
    fn strategy_key(&self) -> &str;
    async fn plan(&self, store: &dyn GovernanceStore, task: GovernanceTask) -> Result<GovernancePlan>;
    async fn execute(&self, store: &dyn GovernanceStore, task: GovernanceTask, plan: &GovernancePlan) -> Result<GovernanceExecution>;
}

// Storage abstraction for governance operations
trait GovernanceStore: Send + Sync {
    async fn list_active_users(&self) -> Result<Vec<String>>;
    async fn cleanup_tool_results(&self, max_age_hours: i64) -> Result<i64>;
    async fn quarantine_low_confidence(&self, user: &str) -> Result<i64>;
    // ... more cleanup/maintenance methods
}
```

## Database Tables (MatrixOne)

Core: `mem_memories`, `mem_snapshots`, `mem_branches`, `memory_graph_nodes`, `memory_graph_edges`
Auth: `mem_api_keys`
Plugin: `mem_plugin_packages`, `mem_plugin_signers`, `mem_plugin_binding_rules`, `mem_plugin_reviews`, `mem_plugin_compatibility`, `mem_plugin_audit_events`

## Config Pattern

`Config` struct in `config.rs` reads from env vars. Every field maps to an env var:

| Field | Env Var | Default |
|-------|---------|---------|
| `db_url` | `DATABASE_URL` | `mysql://root:111@localhost:6001/memoria` |
| `embedding_provider` | `EMBEDDING_PROVIDER` | `local` |
| `governance_plugin_dir` | `MEMORIA_GOVERNANCE_PLUGIN_DIR` | None |
| `governance_plugin_binding` | `MEMORIA_GOVERNANCE_PLUGIN_BINDING` | `default` |
| `governance_plugin_subject` | `MEMORIA_GOVERNANCE_PLUGIN_SUBJECT` | `system` |

**When adding a new Config field:** update ALL test `Config { .. }` constructors (in `scheduler.rs` tests and `tests/plugin_repository.rs`).

## REST API Structure

| Prefix | Module | Auth | Purpose |
|--------|--------|------|---------|
| `/v1/memories` | `routes/memory.rs` | Bearer | CRUD, search, retrieve |
| `/v1/snapshots`, `/v1/branches` | `routes/memory.rs` | Bearer | Git-for-Data |
| `/v1/governance` | `routes/governance.rs` | Bearer | Trigger governance |
| `/admin/users`, `/admin/stats` | `routes/admin.rs` | Bearer | Admin ops |
| `/admin/plugins/*` | `routes/plugins.rs` | Bearer | Plugin repository |
| `/health` | `routes/admin.rs` | None | Health checks |

## CLI Commands

```
memoria init --tool <kiro|cursor|claude>    # Generate MCP config + steering rules
memoria mcp --db-url <url>                  # Start MCP server (embedded mode)
memoria mcp --api-url <url> --token <key>   # Start MCP server (remote mode)
memoria serve                               # Start REST API server
memoria plugin init --dir <dir>             # Scaffold plugin project
memoria plugin publish --package-dir <dir>  # Publish plugin to repository
memoria plugin dev-keygen --dir <dir>       # Generate ed25519 dev signing keypair
memoria plugin list|review|score|activate|rules|matrix|events  # Plugin management
```

## Testing

```bash
make check          # cargo check + clippy -D warnings (MUST pass before commit)
make test-unit      # Unit tests, no DB: memoria-core, memoria-service, memoria-mcp
make test           # All tests (needs MatrixOne running)
make test-e2e       # API e2e tests only
```

### Test patterns
- E2e tests: `spawn_server()` → random port, shared DB, reqwest client
- Plugin tests: `build_signed_plugin_files()` helper creates base64 file maps
- Unique names: `uuid::Uuid::new_v4().simple()` to avoid test interference
- DB tests need `DATABASE_URL` env var

## Common Pitfalls

1. **Adding Config fields** — must update ALL test constructors or tests won't compile
2. **Clippy strictness** — `-D warnings` means ANY warning is a build failure
3. **`PathBuf::from("x")` in comparisons** — use `Path::new("x")` to avoid `cmp_owned`
4. **`format!()` inside `println!()`** — inline the args directly
5. **Plugin exports** — new public items need re-export chain: `repository.rs` → `plugin/mod.rs` → `lib.rs`
6. **Shared DB in tests** — e2e tests run in parallel against same DB; use unique names for isolation
