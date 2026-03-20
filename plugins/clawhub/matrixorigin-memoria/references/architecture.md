# Architecture

Use this when navigating or modifying the Memoria Rust workspace.

## Workspace Layout

- `memoria-core`: shared types and errors
- `memoria-storage`: SQL storage and graph persistence
- `memoria-service`: business logic, governance, plugin runtime
- `memoria-api`: Axum REST API
- `memoria-mcp`: MCP server
- `memoria-cli`: CLI commands including `init`, `mcp`, `serve`, and plugin flows
- `memoria-embedding`: embedding and LLM clients
- `memoria-git`: snapshots, branches, merge, rollback

## Important Files

- `memoria-service/src/service.rs`
- `memoria-service/src/config.rs`
- `memoria-service/src/distributed.rs`
- `memoria-service/src/scheduler.rs`
- `memoria-service/src/plugin/`
- `memoria-api/src/lib.rs`
- `memoria-api/src/routes/`
- `memoria-mcp/src/tools.rs`

## Tables

- Core memory state: `mem_memories`, `mem_user_state`, `mem_branches`, `mem_snapshots`
- Graph state: `memory_graph_nodes`, `memory_graph_edges`, `mem_entities`
- Audit and feedback: `mem_edit_log`, `mem_retrieval_feedback`
- Plugin repository: `mem_plugin_packages`, `mem_plugin_signers`, `mem_plugin_bindings`
- Distributed runtime: `mem_distributed_locks`, `mem_async_tasks`

## Testing

```bash
make check
make test-unit
make test
make test-e2e
```

## Common Pitfalls

- New config fields require test constructor updates.
- Clippy warnings fail CI.
- Distributed tests need unique names and instance IDs.
- MatrixOne date-time columns should use `chrono::NaiveDateTime`.
