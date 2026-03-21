---
name: setup
description: Install Memoria and configure MCP for AI tools (Kiro, Cursor, Claude Code, Codex). Decision tree for embedded vs remote mode, database, embedding provider. Use when helping users set up Memoria.
---

## Decision Tree

### Step 1: Embedded or Remote?

Ask: "Are you setting up your own instance, or connecting to an existing Memoria server?"

- **Own instance** → continue to Step 2
- **Existing server** → skip to [Remote Mode](#remote-mode) (just need URL + token)

### Step 2: Which AI tool?

Ask: "Kiro, Cursor, Claude Code, or Codex?"

Config files generated:
- Kiro: `.kiro/settings/mcp.json` + `.kiro/steering/memory.md`
- Cursor: `.cursor/mcp.json` + `.cursor/rules/memory.mdc`
- Claude: `.mcp.json` + `CLAUDE.md`
- Codex: `~/.codex/config.toml` + `AGENTS.md`

### Step 3: MatrixOne database?

Ask: "Do you have a MatrixOne database running?"

- Already have one → get connection URL
- No, use Docker → `docker compose up -d` (wait 30-60s first start)
- No Docker → [MatrixOne Cloud](https://cloud.matrixorigin.cn) (free tier)

### Step 4: Embedding provider?

⚠️ **Hard to reverse.** Dimension locked into schema on first startup.

Ask: "Do you have an OpenAI-compatible embedding endpoint?"

- Yes → collect: base URL, API key, model, dimension
- No → suggest SiliconFlow (free tier) or Ollama. Local embedding requires `--features local-embedding` build.

## Install Binary

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

## Configure

### Local Docker

```bash
docker compose up -d                    # Start MatrixOne
docker ps --filter name=matrixone       # Verify (wait 30-60s)
cd <user-project>
memoria init --tool <tool>              # + embedding flags below
```

### MatrixOne Cloud / Existing DB

```bash
cd <user-project>
memoria init --tool <tool> --db-url 'mysql+pymysql://<user>:<pass>@<host>:<port>/<db>'
```

### Remote Mode

```bash
cd <user-project>
memoria init --tool <tool> --api-url 'https://host:8100' --token 'sk-...'
```

### Embedding Flags (for own instance)

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

## Verify

```bash
memoria status
```

Tell user to restart their AI tool. Verify: `memory_retrieve("test")` → "No relevant memories found".

## Post-Setup

```bash
memoria rules --force   # After upgrading binary, sync steering rules
```

## MCP Server Modes

```bash
# Embedded (direct DB)
memoria mcp --db-url "mysql+pymysql://root:111@localhost:6001/memoria" --user alice

# Remote (proxy to API)
memoria mcp --api-url "https://host:8100" --token "sk-..."

# SSE transport
memoria mcp --transport sse
```

## Troubleshooting

| Problem | Fix |
|---------|-----|
| MatrixOne won't start | `docker logs memoria-matrixone` |
| Port 6001 in use | Change `MO_PORT` in `.env` |
| Can't connect | Wait 30-60s on first start |
| Docker permission denied | `sudo usermod -aG docker $USER && newgrp docker` |
| Docker not installed | Use MatrixOne Cloud instead |
| First query slow | Normal with local embedding (~3-5s). Use `openai` provider to avoid |
| `local-embedding` not compiled | Use OpenAI-compatible service, or build from source |
| AI tool doesn't use memory | 1. `which memoria` 2. Restart tool 3. Test server directly |
