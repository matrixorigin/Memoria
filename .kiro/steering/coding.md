---
inclusion: always
---

# Coding Standards (Rust)

## Lint & Build Checks

All code changes must pass before committing:

```bash
make check   # runs: cargo check + cargo clippy -- -D warnings
```

**Before finishing any code task:**
- `make check` passes with zero errors and zero warnings
- `make test-unit` passes (no DB required)
- If touching DB-related code: `make test` passes (requires MatrixOne)

## Test Commands

```bash
make test-unit          # Unit tests only (no DB): -p memoria-core -p memoria-service -p memoria-mcp
make test               # All tests (needs DB, runs --test-threads=1)
make test-e2e           # API e2e tests only (needs DB)
make test-integration   # Storage integration tests only (needs DB)
```

## Key Patterns

### Crate structure
- `memoria-core` — shared types, `MemoriaError`
- `memoria-storage` — `SqlMemoryStore`, SQL queries, migrations
- `memoria-service` — business logic: `MemoryService`, governance, plugin system, scheduler
- `memoria-api` — REST API (axum): routes, auth, `AppState`
- `memoria-mcp` — MCP server (stdio/SSE transport)
- `memoria-cli` — CLI binary (`memoria` command)
- `memoria-embedding` — embedding + LLM client
- `memoria-git` — Git-for-Data (snapshots, branches, merge)

### Plugin system (`memoria-service/src/plugin/`)
- `manifest.rs` — `PluginManifest`, `PluginPackage`, signing verification, `load_plugin_package`
- `repository.rs` — publish/review/score/binding/audit CRUD against `mem_plugin_*` tables
- `rhai_runtime.rs` — `RhaiGovernanceStrategy` (sandboxed Rhai script execution)
- `grpc_runtime.rs` — `GrpcGovernanceStrategy` (remote gRPC plugin)
- `governance_hook.rs` — `GovernancePluginContractHarness` for contract testing

### Config
- `Config` struct reads from env vars. When adding a new field:
  1. Add to `Config` struct in `config.rs`
  2. Read from env in `Config::from_env()`
  3. Add `field: None` (or default) to ALL test `Config { .. }` constructors — check `scheduler.rs` tests AND `tests/plugin_repository.rs`

### Adding a new REST endpoint
1. Add handler in `memoria-api/src/routes/`
2. Register route in `memoria-api/src/lib.rs` `build_router()`
3. Add e2e test in `memoria-api/tests/api_e2e.rs` using `spawn_server()`

### Adding a new plugin repository feature
1. Add SQL + logic in `repository.rs`
2. Export from `plugin/mod.rs` → `lib.rs`
3. Add REST handler in `routes/plugins.rs`
4. Add CLI subcommand in `memoria-cli/src/main.rs`
5. Add e2e test in `api_e2e.rs`

## Common Clippy Issues

- `clippy::too_many_arguments` — use `#[allow(clippy::too_many_arguments)]` on the function if refactoring isn't practical
- `clippy::cmp_owned` — compare with `Path::new("x")` not `PathBuf::from("x")`
- `clippy::format_in_format_args` — inline format args into the outer `println!`/`format!` call
- Unused imports — remove them; `cargo check` alone won't catch what `clippy` catches

## Test Conventions

- Each e2e test spawns its own server via `spawn_server()` (random port, shared DB)
- Use `uuid::Uuid::new_v4().simple()` for unique names to avoid test interference
- Plugin tests: use `build_signed_plugin_files()` helper to create base64 file maps
- DB tests need `DATABASE_URL` env var (default: `mysql://root:111@localhost:6001/memoria`)
