# memoria — Rust workspace

Rust workspace for Memoria. Single unified binary `memoria` with subcommands.

## Quick start

```bash
cd memoria

# Set up env
cp .env.example .env
# Edit .env with your DB URL

# Build
SQLX_OFFLINE=true cargo build -p memoria-cli

# Run the binary
./target/debug/memoria --help
./target/debug/memoria serve          # REST API server
./target/debug/memoria mcp --help     # MCP server
./target/debug/memoria init           # Configure AI tools
```

## Crates

| Crate | Description |
|-------|-------------|
| `memoria-cli` | Unified binary entry point (`memoria serve`, `memoria mcp`, `memoria init`, etc.) |
| `memoria-api` | Axum REST API (lib) |
| `memoria-mcp` | MCP stdio/SSE server (lib) |
| `memoria-service` | Business logic, governance, scheduling |
| `memoria-storage` | sqlx-based MatrixOne storage |
| `memoria-embedding` | HTTP + local (fastembed) embedding providers |
| `memoria-core` | Shared types and interfaces |
| `memoria-git` | Git-for-Data (snapshots, branches, merge) |

## CRITICAL rules

- **NEVER** use `format!()` to build SQL — always sqlx parameter binding (`.bind(...)`)
- **ALWAYS** `SQLX_OFFLINE=true` — run `cargo sqlx prepare` after schema changes
- MemoryType must have exactly 6 variants
- TrustTier must have exactly 4 variants (T1–T4)
