# Agent Integration Guide

> **Audience**: AI agents (Kiro, Cursor, Claude Code, etc.) helping a user install and use Memoria.
> If you're a human reading this, see the [User Guide](user-guide.md) instead.

## Before You Start — Ask, Don't Assume

Do NOT jump straight into installation. Gather these answers first:

### 1. Embedded or Remote?

`memoria mcp` has two modes:

| | Embedded mode | Remote mode |
|---|---|---|
| **Flag** | `--db-url` | `--api-url` + `--token` |
| **Who runs the DB** | User runs MatrixOne | Someone else deployed Memoria |
| **Embedding config** | Required (in `env` block) | Not needed (server handles it) |
| **Use case** | Personal setup, local dev | Team/SaaS — admin gives URL + token |

**Ask**: "Are you setting up your own Memoria instance, or connecting to an existing server?"

- **Own instance** → continue to Questions 2–4
- **Existing server** → skip to [Path D: Remote Mode](#path-d-remote-mode)

### 2. Which AI tool?

**Ask**: "Which AI tool are you using? Kiro, Cursor, Claude Code, or multiple?"

This determines which config files `memoria init` generates:
- **Kiro**: `.kiro/settings/mcp.json` + `.kiro/steering/memory.md`
- **Cursor**: `.cursor/mcp.json` + `.cursor/rules/memory.mdc`
- **Claude**: `.mcp.json` + `CLAUDE.md`

### 3. MatrixOne database?

**Ask**: "Do you have a MatrixOne database running?"

- **Already have one** → get the connection URL (host, port, user, password, database)
- **No — use Docker** → follow [Path A](#path-a-local-docker)
- **No — no Docker** → suggest [MatrixOne Cloud](https://cloud.matrixorigin.cn) (free tier), follow [Path B](#path-b-matrixone-cloud)

### 4. Embedding provider?

⚠️ **CRITICAL: This decision is hard to reverse.** Once the MCP server starts and creates tables, the embedding dimension is locked into the schema. Changing it later requires re-creating the embedding column (destructive).

**Ask**: "Do you have an OpenAI-compatible embedding endpoint? (OpenAI, SiliconFlow, Ollama, or any custom service)"

- **Yes** → collect: base URL, API key, model name, embedding dimension
- **No** → suggest SiliconFlow (free tier) or Ollama. Local embedding requires building from source with `--features local-embedding` (pre-built binaries don't include it).

**After collecting config, remind**: "Once your AI tool starts, tables are created with this embedding dimension. Make sure this is correct before proceeding."

---

## Execution Rules

- Run each command separately and wait for success before proceeding
- If a command fails, stop and diagnose — never continue blindly
- Never chain install + configure + verify into one shell call

---

## Install Memoria Binary

```bash
# Linux (x86_64)
curl -LO https://github.com/matrixorigin/Memoria/releases/latest/download/memoria-x86_64-unknown-linux-gnu.tar.gz
tar xzf memoria-x86_64-unknown-linux-gnu.tar.gz
sudo mv memoria /usr/local/bin/

# macOS (Apple Silicon)
curl -LO https://github.com/matrixorigin/Memoria/releases/latest/download/memoria-aarch64-apple-darwin.tar.gz
tar xzf memoria-aarch64-apple-darwin.tar.gz
sudo mv memoria /usr/local/bin/

# Or build from source (required for local embedding)
git clone https://github.com/matrixorigin/Memoria.git
cd Memoria/memoria
cargo build --release -p memoria-cli
# With local embedding: cargo build --release -p memoria-cli --features local-embedding
sudo cp target/release/memoria /usr/local/bin/
```

Verify: `memoria --version`

---

## Setup Paths

### Path A: Local Docker

```bash
# 1. Start MatrixOne
docker compose up -d
# Or: docker run -d --name matrixone -p 6001:6001 -v ./data/matrixone:/mo-data --memory=2g matrixorigin/matrixone:latest

# 2. Verify (wait ~30-60s on first start)
docker ps --filter name=matrixone

# 3. Configure (in user's project directory)
cd <user-project>
memoria init --tool <tool>  # + embedding flags, see below
```

### Path B: MatrixOne Cloud

```bash
# 1. User registers at https://cloud.matrixorigin.cn
# 2. Get connection info from cloud console

# 3. Configure
cd <user-project>
memoria init --tool <tool> --db-url 'mysql+pymysql://<user>:<password>@<host>:<port>/<database>'
# + embedding flags, see below
```

### Path C: Existing MatrixOne

```bash
cd <user-project>
memoria init --tool <tool> --db-url 'mysql+pymysql://<user>:<password>@<host>:<port>/<database>'
# + embedding flags, see below
```

### Path D: Remote Mode

No DB setup, no embedding config needed — the server handles everything.

```bash
cd <user-project>
memoria init --tool <tool> --api-url 'https://memoria-host:8100' --token 'sk-your-key...'
```

### Embedding Flags (for Paths A/B/C)

```bash
# Local (default) — no extra flags
memoria init --tool <tool>

# OpenAI-compatible service (SiliconFlow, Ollama, etc.)
memoria init --tool <tool> \
  --embedding-provider openai \
  --embedding-base-url https://api.siliconflow.cn/v1 \
  --embedding-api-key sk-... \
  --embedding-model BAAI/bge-m3 \
  --embedding-dim 1024
```

---

## After Setup

```bash
memoria status   # verify config files and rule versions
```

Tell the user to **restart their AI tool**, then verify by calling `memory_retrieve("test")` — should return "No relevant memories found".

---

## Troubleshooting

| Problem | Solution |
|---------|----------|
| MatrixOne won't start | `docker logs memoria-matrixone` |
| Port 6001 in use | Edit `.env` to change `MO_PORT`, then `docker compose up -d` |
| Can't connect to DB | MatrixOne needs 30-60s on first start, wait and retry |
| Cloud connection refused | Check firewall/whitelist in cloud console |
| Docker permission denied | `sudo usermod -aG docker $USER && newgrp docker` |
| Image pull slow/timeout | Add `"registry-mirrors": ["https://docker.1ms.run"]` to `/etc/docker/daemon.json`, restart Docker |
| Docker not installed | Suggest MatrixOne Cloud instead |
| Data dir permission error | `mkdir -p data/matrixone && chmod 777 data/matrixone` |
| First query slow | Expected with local embedding (~3-5s model load). Use `--embedding-provider openai` to avoid |
| `local-embedding` not compiled | Pre-built binaries don't include it. Use an OpenAI-compatible service, or build from source with `--features local-embedding` |
| `memory_reflect` returns "LLM not configured" | Add `LLM_API_KEY`, `LLM_BASE_URL`, `LLM_MODEL` to the `env` block in mcp.json |
| AI tool doesn't use memory | 1. `which memoria` 2. Restart AI tool 3. Test server directly |
