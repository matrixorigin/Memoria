---
inclusion: always
---

# Memoria Local Setup

> This steering rule is for the coding agent working on this repo.
> For the AI agent protocol that helps end-users install Memoria, see [docs/agent-integration-guide.md](../../docs/agent-integration-guide.md).

When the user wants to set up Memoria, **do NOT jump straight into installation**. First ask key questions to determine the right path.

## Two MCP Modes — Ask First

`memoria mcp` has two distinct modes. **Determine which one applies before doing anything else.**

| | Embedded mode | Remote mode |
|---|---|---|
| **How** | `--db-url` | `--api-url` + `--token` |
| **Who runs the DB** | User runs MatrixOne locally or on cloud | Someone else already deployed Memoria |
| **Embedding config** | Required (in `env` block of mcp.json) | Not needed (server handles it) |
| **Use case** | Personal setup, local dev, self-hosted | Team/SaaS, user gets an API key from admin |

**Ask the user**: "Are you setting up your own Memoria instance, or connecting to an existing Memoria server someone else deployed?"

- **Own instance** → follow Decision Flow below (Questions 1–3, then Path A/B/C)
- **Existing server** → skip to **Path D: Remote mode** — just need the server URL and API token

## Decision Flow (for own instance only)

### Question 1: Which AI tool?
Ask: "You're using Kiro, Cursor, or Claude Code? (or multiple?)"
This determines which config files to generate.

### Question 2: MatrixOne database
Ask: "Do you already have a MatrixOne database running? If not, I can help you set one up. You have two options:
1. **Local Docker** (recommended for development) — I'll start one for you with docker-compose
2. **MatrixOne Cloud** (free tier available) — register at https://cloud.matrixorigin.cn, no Docker needed"

Based on the answer:
- **Already have one** → ask for the connection URL (host, port, user, password, database)
- **Local Docker** → follow Docker setup below
- **MatrixOne Cloud** → guide user to register, then get connection URL from console

### Question 3: Embedding provider

**⚠️ CRITICAL: This decision is hard to reverse. Once the MCP server starts and creates tables, changing embedding provider requires data migration and re-embedding all memories.**

Ask: "For memory search quality, Memoria needs an embedding model. Do you already have an OpenAI-compatible embedding endpoint? (OpenAI, SiliconFlow, Ollama, or any custom service)
- **Yes** → use it directly. No download, no cold-start. Best choice.
- **No** → use local model. ⚠️ requires building from source with `--features local-embedding`. Pre-built binaries don't include it. Consider SiliconFlow (free tier) or Ollama instead."

**If user has an existing embedding service**, ask:
- "What is the API base URL? (e.g. `https://api.siliconflow.cn/v1`, `http://localhost:11434/v1`, or leave blank for OpenAI official)"
- "API key? (leave blank if not required)"
- "Model name? (e.g. `BAAI/bge-m3`, `text-embedding-3-small`)"
- "Embedding dimension? (e.g. 1024 for bge-m3, 1536 for text-embedding-3-small)"

These values get written into the `env` block of `mcp.json` automatically — no manual editing needed.

**If user chooses local embedding, explicitly warn**: "Local embedding requires building from source with `--features local-embedding`. The pre-built binaries use an OpenAI-compatible embedding service. Consider using SiliconFlow (free tier) or Ollama instead."

**After collecting embedding config, remind user**: "Once your AI tool starts, the database tables will be created with this embedding dimension. Changing it later requires re-creating the embedding column (destructive). Make sure this configuration is correct before proceeding."

## Execution Rules

**CRITICAL: Execute commands one at a time, never chain unrelated steps.**

- Run each command separately and wait for success before proceeding
- If a command fails, stop and diagnose before continuing
- Never chain install + configure + verify into one shell call

## Install Memoria Binary

This step is the same for all paths. Pick one method:

```bash
# Option A: Download pre-built binary (recommended)
# Linux x86_64:
curl -LO https://github.com/matrixorigin/Memoria/releases/latest/download/memoria-x86_64-unknown-linux-gnu.tar.gz
tar xzf memoria-x86_64-unknown-linux-gnu.tar.gz
sudo mv memoria /usr/local/bin/

# macOS Apple Silicon:
curl -LO https://github.com/matrixorigin/Memoria/releases/latest/download/memoria-aarch64-apple-darwin.tar.gz
tar xzf memoria-aarch64-apple-darwin.tar.gz
sudo mv memoria /usr/local/bin/
```
```bash
# Option B: Build from source (needed for local embedding)
git clone https://github.com/matrixorigin/Memoria.git
cd Memoria/memoria
cargo build --release -p memoria-cli              # without local embedding
# cargo build --release -p memoria-cli --features local-embedding  # with local embedding
sudo cp target/release/memoria /usr/local/bin/
```
```bash
# Verify
memoria --version
```

## Execution Paths

### Path A: Local Docker (most common)

```bash
# Step 1: Start MatrixOne (run alone, check output)
docker compose up -d
# or:
docker run -d --name matrixone -p 6001:6001 -v ./data/matrixone:/mo-data --memory=2g matrixorigin/matrixone:latest
```
Wait for success, then:
```bash
# Step 2: Verify MatrixOne is running
docker ps --filter name=matrixone
```
Wait ~30-60s on first start, then:
```bash
# Step 3: Configure (in user's project directory)
# Add embedding flags based on Question 3 answer — see "Embedding provider flags" section below
cd <user-project>
memoria init --tool <tool>  # + embedding flags if applicable
```

### Path B: MatrixOne Cloud

```bash
# 1. User registers at https://cloud.matrixorigin.cn (free tier)
# 2. Get connection info from cloud console: host, port, user, password

# 3. Configure with cloud URL
cd <user-project>
memoria init --tool <tool> --db-url 'mysql+pymysql://<user>:<password>@<host>:<port>/<database>'
# + embedding flags if applicable (see "Embedding provider flags" section)
```

### Path C: Existing MatrixOne

```bash
# 1. Configure with existing DB
cd <user-project>
memoria init --tool <tool> --db-url 'mysql+pymysql://<user>:<password>@<host>:<port>/<database>'
# + embedding flags if applicable (see "Embedding provider flags" section)
```

### Path D: Remote mode (connecting to an existing Memoria server)

Use this when the user has been given a server URL and API token by an admin — no DB setup, no embedding config needed.

```bash
# 1. Configure with remote server
cd <user-project>
memoria init --tool <tool> --api-url 'https://memoria-host:8100' --token 'sk-your-key...'
```

The resulting `mcp.json` will be:
```json
{
  "mcpServers": {
    "memoria": {
      "command": "memoria",
      "args": ["mcp", "--api-url", "https://memoria-host:8100", "--token", "sk-your-key..."]
    }
  }
}
```

No `env` block needed — embedding is handled server-side.

```bash
# 4. Verify
memoria status
# Tell user to restart their AI tool
```

### Embedding provider flags (for any path)

```bash
# Local (default) — no extra flags needed
memoria init --tool <tool>

# OpenAI
memoria init --tool <tool> --embedding-provider openai --embedding-api-key sk-...

# Existing service (Ollama, SiliconFlow, custom endpoint, etc.)
# All of these get written into the env block of mcp.json automatically
memoria init --tool <tool> \
  --embedding-provider openai \
  --embedding-base-url https://api.siliconflow.cn/v1 \
  --embedding-api-key sk-... \
  --embedding-model BAAI/bge-m3 \
  --embedding-dim 1024
```

The resulting `mcp.json` `env` block will contain the configured variables:
```json
{
  "EMBEDDING_PROVIDER": "openai",
  "EMBEDDING_BASE_URL": "https://api.siliconflow.cn/v1",
  "EMBEDDING_API_KEY": "sk-...",
  "EMBEDDING_MODEL": "BAAI/bge-m3",
  "EMBEDDING_DIM": "1024"
}
```

## After any path

```bash
# Verify
memoria status

# Tell user to restart their AI tool
```

## Troubleshooting
- MatrixOne won't start → `docker logs memoria-matrixone` to check errors
- Port 6001 in use → edit `.env` to change `MO_PORT`, then `docker compose up -d`
- Can't connect to DB → MatrixOne needs 30-60s on first start, wait and retry
- Cloud connection refused → check firewall/whitelist settings in cloud console
- **Docker permission denied** → `sudo usermod -aG docker $USER && newgrp docker`
- **Image pull slow/timeout** → configure Docker mirror in `/etc/docker/daemon.json`, add `"registry-mirrors": ["https://docker.1ms.run"]`, then `sudo systemctl restart docker`
- **Docker not installed** → suggest MatrixOne Cloud (https://cloud.matrixorigin.cn) as alternative, no Docker needed
- **Data dir permission error** → `mkdir -p data/matrixone && chmod 777 data/matrixone`
- **First query slow** → expected with local embedding; model loads into memory on first use (~3-5s). Subsequent queries are fast. Use `--embedding-provider openai` to avoid this.
