---
name: setup
description: Install Memoria and configure MCP for AI tools (Kiro, Cursor, Claude Code, Codex, Gemini CLI). Decision tree for Cloud vs self-hosted mode, database, embedding provider. Use when helping users set up Memoria.
---

## Decision Tree

Follow these steps in order. Steps marked "Self-Hosted only" can be skipped for Memoria Cloud users.

### Step 1: Memoria Cloud or Self-Hosted?

Ask: "Use Memoria Cloud, or run your own instance?"

- **Memoria Cloud (recommended)** → sign up at [thememoria.ai](https://thememoria.ai/auth), get API URL + token, then proceed to Step 2
- **Self-hosted** → proceed to Step 2

### Step 2: Which AI tool?

Ask: "Which AI tool are you using — Kiro, Cursor, Claude Code, Codex, or Gemini CLI?"

The `--tool` flag value and config files generated per tool:

| Tool | `--tool` value | Config files |
|------|---------------|--------------|
| Kiro | `kiro` | `.kiro/settings/mcp.json` + `.kiro/steering/memory.md` |
| Cursor | `cursor` | `.cursor/mcp.json` + `.cursor/rules/memory.mdc` |
| Claude Code | `claude` | `.mcp.json` + `CLAUDE.md` |
| Codex | `codex` | `~/.codex/config.toml` + `AGENTS.md` |
| Gemini CLI | `gemini` | `.gemini/settings.json` + `GEMINI.md` |

### Step 3: Database (Self-Hosted only)

Skip this step if user chose Memoria Cloud in Step 1.

Ask: "Do you have a MatrixOne database running?"

- Already have one → get connection URL (format: `mysql+pymysql://<user>:<pass>@<host>:<port>/<db>`)
- No → run `docker compose up -d` in the Memoria repo root (wait 30-60s for first start)

### Step 4: Embedding provider (Self-Hosted only)

Skip this step if user chose Memoria Cloud in Step 1.

⚠️ **Hard to reverse.** Embedding dimension is locked into schema on first startup.

Ask: "Do you have an OpenAI-compatible embedding endpoint?"

- Yes → collect: base URL, API key, model, dimension
- No → suggest SiliconFlow (free tier) or Ollama. Local embedding requires `--features local-embedding` build.

### Step 5: Install Memoria CLI

The `memoria` binary is required for all modes — it serves as the MCP bridge between the AI tool and the Memoria server.

One-line install (recommended):

```bash
curl -sSL https://raw.githubusercontent.com/matrixorigin/Memoria/main/scripts/install.sh | bash
```

Or download manually from [GitHub Releases](https://github.com/matrixorigin/Memoria/releases).

Platform-specific manual install:

```bash
# Linux x86_64
curl -LO https://github.com/matrixorigin/Memoria/releases/latest/download/memoria-x86_64-unknown-linux-musl.tar.gz
tar xzf memoria-x86_64-unknown-linux-musl.tar.gz && sudo mv memoria /usr/local/bin/

# macOS Apple Silicon
curl -LO https://github.com/matrixorigin/Memoria/releases/latest/download/memoria-aarch64-apple-darwin.tar.gz
tar xzf memoria-aarch64-apple-darwin.tar.gz && sudo mv memoria /usr/local/bin/

# From source (required for local embedding)
cd Memoria/memoria && cargo build --release -p memoria-cli
# With local embedding: cargo build --release -p memoria-cli --features local-embedding
sudo cp target/release/memoria /usr/local/bin/
```

Verify: `memoria --version`

### Step 6: Configure

### Memoria Cloud (Remote Mode)

Sign up at [thememoria.ai](https://thememoria.ai/auth) — after login you will receive the API URL and token.

```bash
cd <user-project>
memoria init --tool <tool> --api-url '<API URL from thememoria.ai>' --token '<your token>'
```

Replace `<tool>` with the value from Step 2 (e.g., `kiro`, `cursor`, `claude`, `codex`, `gemini`).

### Self-Hosted: Local Docker

```bash
docker compose up -d                    # Start MatrixOne
docker ps --filter name=matrixone       # Verify (wait 30-60s)
cd <user-project>
memoria init --tool <tool>              # + embedding flags below
```

### Self-Hosted: Existing DB

```bash
cd <user-project>
memoria init --tool <tool> --db-url 'mysql+pymysql://<user>:<pass>@<host>:<port>/<db>'
```

### Embedding Flags (Self-Hosted only)

```bash
# Local (default, no flags)
memoria init --tool <tool>

# OpenAI-compatible
memoria init --tool <tool> \
  --embedding-provider openai \
  --embedding-base-url https://api.siliconflow.cn/v1 \
  --embedding-api-key sk-... \
  --embedding-model BAAI/bge-m3 \
  --embedding-dim 1024
```

### Step 7: Verify

After running `memoria init`, tell user to:

1. Restart their AI tool
2. Ask the AI: *"Do you have memory tools available?"*
3. Or run: `memoria status`

Expected: `memory_retrieve("test")` → "No relevant memories found".

## Post-Setup

```bash
memoria rules --force   # After upgrading Memoria binary, re-sync steering rules
```

## MCP Server Modes (Reference)

These are the underlying commands that `memoria init` configures. Users normally don't need to run them directly.

```bash
# Embedded mode (direct DB connection, self-hosted)
memoria mcp --db-url "mysql+pymysql://root:111@localhost:6001/memoria" --user alice

# Remote mode (proxy to Memoria API server, Cloud or self-hosted API)
memoria mcp --api-url "<API URL>" --token "<token>"

# SSE transport (alternative to default stdio)
memoria mcp --transport sse
```

## Troubleshooting

| Problem | Fix |
|---------|-----|
| MatrixOne won't start | `docker logs memoria-matrixone` |
| Port 6001 in use | Change `MO_PORT` in `.env` |
| Can't connect to DB | Wait 30-60s on first start |
| Docker permission denied | `sudo usermod -aG docker $USER && newgrp docker` |
| Docker not available | Use [Memoria Cloud](https://thememoria.ai/auth) instead (no Docker needed) |
| First query slow | Normal with local embedding (~3-5s). Use `openai` provider for faster response |
| `local-embedding` not compiled | Use OpenAI-compatible service, or build from source with `--features local-embedding` |
| AI tool doesn't see memory tools | 1. Run `which memoria` to verify CLI installed 2. Restart AI tool 3. Test MCP server directly |
