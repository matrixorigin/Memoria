# memoria_rs — Rust Rewrite

Rust workspace for Memoria. Python implementation in `../memoria/` is untouched.

## Quick start

```bash
cd memoria_rs

# Set up env
cp .env.example .env
# Edit .env with your DB URL

# Build (sqlx offline mode — no live DB needed for compilation)
SQLX_OFFLINE=true cargo build

# Run tests (unit only, no DB)
cargo test --lib

# Run tests with live DB
cargo test
```

## Phases

| Phase | Status | Deliverable |
|-------|--------|-------------|
| 1 | 🚧 IN PROGRESS | Core types + DB CRUD (sqlx offline) |
| 2 | ⏳ | HTTP embedding + REST API + 8 MCP tools |
| 3 | ⏳ | Git-for-Data RFC |
| 4 | ⏳ | Candle embedding + 22 MCP tools, binary < 15MB |
| 5 | ⏳ | Full Git-for-Data implementation |

## CRITICAL rules

- **NEVER** use `format!()` to build SQL — always sqlx parameter binding (`.bind(...)`)
- **ALWAYS** `SQLX_OFFLINE=true` — run `cargo sqlx prepare` after schema changes
- MemoryType must have exactly 6 variants
- TrustTier must have exactly 4 variants (T1–T4)
