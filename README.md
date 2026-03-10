<div align="center">
  <img src="assets/logo.jpg" alt="TrustMem Logo" width="300"/>
  
  # TrustMem
  
  **Secure · Auditable · Programmable Memory for AI Agents**
  
  [![MatrixOne](https://img.shields.io/badge/Powered%20by-MatrixOne-00ADD8?style=flat-square&logo=database)](https://github.com/matrixorigin/matrixone)
  [![MCP](https://img.shields.io/badge/Protocol-MCP-7C3AED?style=flat-square)](https://modelcontextprotocol.io)
  [![Git for Data](https://img.shields.io/badge/Git%20for%20Data-Enabled-00A3CC?style=flat-square)](https://github.com/matrixorigin/matrixone)
  [![License](https://img.shields.io/badge/License-Apache%202.0-blue.svg?style=flat-square)](LICENSE)
  
  [See It In Action](#see-it-in-action) · [Quick Start](#quick-start) · [Architecture](#architecture) · [API Reference](#api-reference) · [Why TrustMem?](#why-trustmem)
  
</div>

---

## Overview

TrustMem is a **persistent memory layer** for AI agents with Git-level version control.
Every memory change is tracked, auditable, and reversible — snapshots, branches, merges, and time-travel rollback, all powered by MatrixOne's native Copy-on-Write engine.

```mermaid
%%{init: {'theme': 'base', 'themeVariables': { 
  'primaryColor': '#0A2540',
  'primaryTextColor': '#E0F7FF',
  'primaryBorderColor': '#00D4FF',
  'lineColor': '#00A3CC',
  'secondaryColor': '#1E3A5F',
  'tertiaryColor': '#00D4FF'
}}}%%

graph TD
    A[AI Agent] 
    -->|MCP Protocol| B[TrustMem Core]

    B --> C[Canonical Storage<br/>Single Source of Truth]
    B --> D[Retrieval Strategy<br/>Pluggable Search]

    C --> E[Git-for-Data Engine]
    E --> F[MatrixOne]

    subgraph "Security Layer"
        G[Snapshot & Branch<br/>Zero-Copy Isolation]
        H[Audit & Provenance<br/>Full Traceability]
        I[Self-Governance<br/>Contradiction Detection]
    end

    B --> G
    B --> H
    B --> I

    classDef core fill:#0A2540,stroke:#00D4FF,stroke-width:3px,color:#E0F7FF,rx:15,ry:15;
    classDef storage fill:#1E3A5F,stroke:#00A3CC,stroke-width:2px,color:#E0F7FF;
    classDef strategy fill:#1E3A5F,stroke:#00D4FF,stroke-width:2px,color:#E0F7FF;
    classDef engine fill:#00A3CC,stroke:#00D4FF,color:#0A2540;

    class A,B core;
    class C,D storage;
    class E engine;
    class G,H,I strategy;
```

**Core Capabilities:**
- **Cross-conversation memory** — preferences, facts, and decisions persist across sessions
- **Semantic search** — retrieves memories by meaning, not just keywords
- **Git for Data** — zero-copy branching, instant snapshots, point-in-time rollback
- **Audit trail** — every memory mutation has a snapshot + provenance chain
- **Self-maintaining** — built-in governance detects contradictions, quarantines low-confidence memories
- **Private by default** — local embedding model option, no data leaves your machine

**Supported Agents:** [Kiro](https://kiro.dev) · [Cursor](https://cursor.sh) · [Claude Code](https://docs.anthropic.com/en/docs/claude-code) · Any MCP-compatible agent

**Storage Backend:** [MatrixOne](https://github.com/matrixorigin/matrixone) — Distributed database with native vector indexing

---

## Why TrustMem?

| Capability | TrustMem | Letta / Mem0 / Traditional RAG |
|---|---|---|
| Git-level version control | Native zero-copy snapshots & branches | File-level or none |
| Isolated experimentation | One-click branch, merge after validation | Manual data duplication |
| Audit trail | Full snapshot + provenance on every mutation | Limited logging |
| Semantic retrieval | Vector + full-text hybrid search | Vector only |
| Multi-agent sharing | Shared trusted memory pool per user | Siloed per agent |
| Migration cost | Zero — all state in MatrixOne | Export/import required |

---

## See It In Action

Here's a real setup conversation in Cursor — just tell the agent what you want, it handles the rest:

```
You:     I want to set up TrustMem for persistent memory.

Cursor:  Before I start, a few quick questions:

           1. Which AI tool are you using? (Cursor, Kiro, Claude Code, or multiple?)
           2. Do you have a MatrixOne database running? If not, I can start one with
              Docker, or you can use MatrixOne Cloud (free tier, no Docker needed).
           3. Do you have an embedding service? (OpenAI, SiliconFlow, Ollama, etc.)
              If not, I'll use a local model — but it downloads ~900MB on first use.

You:     Cursor. No database yet. I have a SiliconFlow API key.

Cursor:  Got it. I'll start MatrixOne with Docker first — running:

           docker compose up -d

You:     [output] Container trustmem-matrixone started

Cursor:  MatrixOne is initializing, takes about 30 seconds on first start.
         While we wait — what's your SiliconFlow API key and which model do you use?
         (e.g. BAAI/bge-m3)

You:     key is sk-xxxx, model BAAI/bge-m3

Cursor:  Database is ready. Installing TrustMem now:

           pip install --index-url https://pypi.org/simple/ \
             --extra-index-url https://test.pypi.org/simple/ \
             trust-mem-lite

You:     [output] Successfully installed trust-mem-lite-0.2.2

Cursor:  Configuring for Cursor with your SiliconFlow endpoint:

           trustmem init \
             --embedding-provider openai \
             --embedding-base-url https://api.siliconflow.cn/v1 \
             --embedding-api-key sk-xxxx \
             --embedding-model BAAI/bge-m3 \
             --embedding-dim 1024

You:     [output]
         ✓ Created .cursor/mcp.json
         ✓ Created .cursor/rules/memory.mdc

Cursor:  All set. Please restart Cursor to load the MCP server, then come back.

You:     Restarted. Does it work?

Cursor:  Let me check:

           trustmem status

         TrustMem Status
           Database:   connected (mysql+pymysql://root:***@localhost:6001/trustmem)
           Tables:     8 tables OK
           Embedding:  openai (BAAI/bge-m3, dim=1024) via https://api.siliconflow.cn/v1
           Cursor:     .cursor/mcp.json ✓  |  .cursor/rules/memory.mdc ✓

         Everything looks good. Try telling me a preference — like your test framework
         or code style — and I'll remember it across all future conversations.
```

---

## Quick Start

### Prerequisites

- Python 3.11+
- [MatrixOne](https://github.com/matrixorigin/matrixone) database (local Docker or [cloud](https://cloud.matrixorigin.cn))

### 1. Start MatrixOne (database)

```bash
git clone https://github.com/matrixorigin/TrustMem.git
cd TrustMem
docker compose up -d
# Wait ~30-60s for first-time initialization
```

Or use `docker run` directly:
```bash
docker run -d --name matrixone -p 6001:6001 -v ./data/matrixone:/mo-data --memory=2g matrixorigin/matrixone:latest
```

See [docker-compose.yml](docker-compose.yml) for configuration options. Don't want Docker? Use [MatrixOne Cloud](https://cloud.matrixorigin.cn) (free tier).

### 2. Install TrustMem

```bash
python3 -m venv .venv && source .venv/bin/activate

pip install --index-url https://pypi.org/simple/ --extra-index-url https://test.pypi.org/simple/ 'trust-mem-lite[local-embedding]'
```

> `[local-embedding]` installs a local embedding model (~900MB download on first use). If you already have an embedding service (OpenAI, SiliconFlow, Ollama), skip this and use `trust-mem-lite` instead — see [Embedding providers](#embedding-providers).

### 3. Configure your AI tool

```bash
cd your-project
trustmem init
# Restart your AI tool — done!
```

### 4. Verify

```bash
trustmem status
```

Expected output:
```
TrustMem Status
  Database:    connected (mysql+pymysql://root:***@localhost:6001/trustmem)
  Tables:      8 tables OK
  Embedding:   local (all-MiniLM-L6-v2, dim=384)
  Kiro:        .kiro/settings/mcp.json ✓  |  .kiro/steering/memory.md ✓ (v0.2.2)
  Cursor:      not detected
  Claude Code: not detected
```

---

## Setup by Tool

### Kiro

```bash
cd your-project
mkdir -p .kiro
trustmem init
# Restart Kiro
```

Files created:
- `.kiro/settings/mcp.json` — MCP server config
- `.kiro/steering/memory.md` — steering rules

### Cursor

```bash
cd your-project
mkdir -p .cursor
trustmem init
# Restart Cursor
```

Files created:
- `.cursor/mcp.json` — MCP server config
- `.cursor/rules/memory.mdc` — rules for Cursor

### Claude Code

```bash
cd your-project
trustmem init
# Restart Claude Code
```

Files created:
- `.claude/mcp.json` — MCP server config
- `CLAUDE.md` — rules appended (or created)

### All tools at once

If your project has `.kiro/`, `.cursor/`, and `CLAUDE.md`, `trustmem init` configures all of them in one go.

---

## Configuration Options

### Custom database URL

```bash
trustmem init --db-url 'mysql+pymysql://user:pass@host:6001/mydb'
```

### Embedding providers

TrustMem needs an embedding model to vectorize memories for semantic search.

| Provider | Quality | Cost | First-use latency | Ongoing latency |
|----------|---------|------|-------------------|-----------------|
| **Local** (default) | Good | Free, private | ~900MB download + a few seconds to load on first query | Fast (in-process) |
| **OpenAI** | Better | API key required | None | Network round-trip |
| **Custom service** | Varies | Self-hosted | None | Network round-trip |

**Recommendation**: If you already have an embedding service (OpenAI, Ollama, SiliconFlow), use it — avoids the local model download and cold-start latency. Otherwise, local works well; the download only happens once.

```bash
# Local (default) — no extra flags needed
trustmem init

# OpenAI
trustmem init --embedding-provider openai --embedding-api-key sk-...

# Custom endpoint (Ollama, SiliconFlow, etc.)
trustmem init \
  --embedding-provider openai \
  --embedding-base-url https://api.siliconflow.cn/v1 \
  --embedding-api-key sk-... \
  --embedding-model BAAI/bge-m3 \
  --embedding-dim 1024
```

All flags are written into the `env` block of `mcp.json` automatically — no manual editing needed.

---

## Architecture

```
┌─────────────┐     MCP (stdio)     ┌──────────────────────────────────────┐     SQL      ┌────────────┐
│  Kiro /      │ ◄─────────────────► │  TrustMem MCP Server               │ ◄──────────► │ MatrixOne  │
│  Cursor /    │   store / retrieve  │  ├── Canonical Storage              │  vector +    │  Database  │
│  Claude Code │                     │  ├── Retrieval (vector / semantic)  │  fulltext    │            │
│  Any Agent   │                     │  └── Git-for-Data (snap/branch/merge)│             │            │
└─────────────┘                      └──────────────────────────────────────┘              └────────────┘
```

---

## API Reference

TrustMem exposes MCP tools that your AI tool calls automatically based on steering rules. You can also invoke them directly.

### Core CRUD

| Tool | Description |
|------|-------------|
| `memory_store` | Store a new memory |
| `memory_retrieve` | Retrieve relevant memories for a query (call at conversation start) |
| `memory_correct` | Update an existing memory with new content |
| `memory_purge` | Delete by ID or bulk-delete by topic keyword |
| `memory_search` | Semantic search across all memories |
| `memory_profile` | Get user's memory-derived profile summary |

### Snapshots

| Tool | Description |
|------|-------------|
| `memory_snapshot` | Create a named snapshot of current memory state |
| `memory_snapshots` | List all snapshots |
| `memory_rollback` | Restore memories to a previous snapshot |

### Branches

| Tool | Description |
|------|-------------|
| `memory_branch` | Create a new branch for isolated experimentation |
| `memory_branches` | List all branches |
| `memory_checkout` | Switch to a different branch |
| `memory_merge` | Merge a branch back into main |
| `memory_diff` | Preview what would change on merge |
| `memory_branch_delete` | Delete a branch |

### Maintenance

| Tool | Description |
|------|-------------|
| `memory_governance` | Quarantine low-confidence memories, clean stale data (1h cooldown) |
| `memory_consolidate` | Detect contradictions, fix orphaned graph nodes (30min cooldown) |
| `memory_reflect` | Synthesize high-level insights from memory clusters via LLM (2h cooldown) |
| `memory_rebuild_index` | Rebuild IVF vector index for a table |

---

## Memory Types

| Type | What it stores | Example |
|------|---------------|---------|
| `semantic` | Project facts, technical decisions | "This project uses Go 1.22 with modules" |
| `profile` | User/agent preferences | "Always use pytest, never unittest" |
| `procedural` | How-to knowledge, workflows | "To deploy: run make build then kubectl apply" |
| `working` | Temporary context for current task | "Currently refactoring the auth module" |
| `tool_result` | Results from tool executions | Cached command outputs |

---

## Usage Examples

### Store and Retrieve

```
You: "I prefer tabs over spaces, and always use black for formatting"
AI:  → calls memory_store("User prefers tabs over spaces, uses black for formatting", type="profile")

... next conversation ...

You: "Format this Python file"
AI:  → calls memory_retrieve("format python file")
     ← gets: [profile] User prefers tabs over spaces, uses black for formatting
     → formats with black, uses tabs
```

### Correct a Memory

```
You: "Actually, I switched to ruff instead of black"
AI:  → calls memory_correct(memory_id="abc123", new_content="User uses ruff for formatting", reason="switched from black")
```

### Snapshots: Save and Restore State

```
You: "Take a snapshot before we refactor the database layer"
AI:  → calls memory_snapshot(name="before_db_refactor", description="pre-refactor state")
     ← "Snapshot 'before_db_refactor' created."

... refactoring goes wrong ...

You: "Roll back to before the refactor"
AI:  → calls memory_rollback(name="before_db_refactor")
     ← "Rolled back to snapshot 'before_db_refactor'."
```

### Branches: Isolated Experimentation

```
You: "Create a memory branch to evaluate switching from PostgreSQL to SQLite"
AI:  → calls memory_branch(name="eval_sqlite")
     → calls memory_checkout(name="eval_sqlite")
     ← "Switched to branch 'eval_sqlite'. 42 memories on this branch."

You: "We're now using SQLite instead of PostgreSQL"
AI:  → calls memory_store("Project uses SQLite for persistence", type="semantic")
     (stored on eval_sqlite only — main is untouched)

You: "Merge it"
AI:  → calls memory_diff(source="eval_sqlite")   ← preview first
     → calls memory_merge(source="eval_sqlite", strategy="replace")
     ← "Merged 3 memories from 'eval_sqlite' (skipped 0)."
```

---

## Commands

| Command | Description |
|---------|-------------|
| `trustmem init` | Configure everything (tables + MCP + rules) |
| `trustmem status` | Show configuration and rule versions |
| `trustmem update-rules` | Update steering rules to latest version |
| `trustmem migrate` | Create/update database tables only |
| `trustmem health` | Check memory service health |
| `trustmem governance` | Run memory cleanup and maintenance |

---

## Manual Tuning & Optimization

Integration quality depends on your AI agent's reasoning ability and steering rules. Out-of-the-box behavior may not be optimal.

**If memory usage feels suboptimal**, edit the steering rules in `.kiro/steering/memory.md`, `.cursor/rules/memory.mdc`, or `CLAUDE.md` to be more explicit. For example, if your agent forgets to retrieve memories at conversation start:

```markdown
CRITICAL: At the start of EVERY conversation, call memory_retrieve with the user's first message.
```

---

## Adapting to Other Agents

TrustMem uses the [Model Context Protocol (MCP)](https://modelcontextprotocol.io) standard. Any MCP-compatible agent can integrate by pointing to the server:

```json
{
  "mcpServers": {
    "trustmem": {
      "command": "python",
      "args": ["-m", "mo_memory_mcp"],
      "env": {
        "TRUSTMEM_DB_URL": "mysql+pymysql://root:111@localhost:6001/trustmem"
      }
    }
  }
}
```

See [SETUP_GUIDE.md](SETUP_GUIDE.md) for detailed integration examples.

---

## Troubleshooting

### "Cannot connect to database"

```bash
docker ps | grep matrixone
# If not running:
docker start matrixone
```

### "sentence-transformers not installed"

```bash
pip install --index-url https://pypi.org/simple/ --extra-index-url https://test.pypi.org/simple/ 'trust-mem-lite[local-embedding]'
```

### First query is slow

Expected with local embedding — model loads into memory on first query (~3-5s). Use an embedding service to avoid this:
```bash
trustmem init --embedding-provider openai --embedding-api-key sk-...
```

### AI tool doesn't seem to use memory

1. Check config: `trustmem status`
2. Restart the AI tool after `trustmem init`
3. Verify MCP server starts: `python -m mo_memory_mcp`

### Update steering rules after upgrading

```bash
pip install --upgrade trust-mem-lite
trustmem update-rules
```

---

## For AI Agents

If you're an AI agent helping a user set up TrustMem, **ask before acting**:

1. **Which AI tool?** Kiro / Cursor / Claude Code?
2. **MatrixOne database?** Already have one → get URL. Local Docker → `docker compose up -d`. [MatrixOne Cloud](https://cloud.matrixorigin.cn) → no Docker needed.
3. **Embedding?** Already have a service? Use it. Otherwise local is fine.
4. **Install**: `pip install --index-url https://pypi.org/simple/ --extra-index-url https://test.pypi.org/simple/ 'trust-mem-lite[local-embedding]'`
5. **Configure**: `cd <user-project> && trustmem init`
6. **Verify**: `trustmem status`
7. Tell user to **restart their AI tool**

For detailed troubleshooting, see [SETUP_GUIDE.md](SETUP_GUIDE.md).

---

## License

Apache-2.0 © [MatrixOrigin](https://github.com/matrixorigin)
