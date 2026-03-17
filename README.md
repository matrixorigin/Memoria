<div align="center">
  <img src="assets/logo.jpg" alt="Memoria Logo" width="300"/>
  
  # Memoria
  
  **Secure · Auditable · Programmable Memory for AI Agents**
  
  [![MatrixOne](https://img.shields.io/badge/Powered%20by-MatrixOne-00ADD8?style=flat-square&logo=database)](https://github.com/matrixorigin/matrixone)
  [![MCP](https://img.shields.io/badge/Protocol-MCP-7C3AED?style=flat-square)](https://modelcontextprotocol.io)
  [![Git for Data](https://img.shields.io/badge/Git%20for%20Data-Enabled-00A3CC?style=flat-square)](https://github.com/matrixorigin/matrixone)
  [![License](https://img.shields.io/badge/License-Apache%202.0-blue.svg?style=flat-square)](LICENSE)
  
  [Quick Start](#quick-start) · [Why Memoria?](#why-memoria) · [Architecture](#architecture) · [API Reference](#api-reference) · [Documentation](#documentation)
  
</div>

---

## Overview

Memoria is a **persistent memory layer** for AI agents with Git-level version control.
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
    -->|MCP Protocol| B[Memoria Core]

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

## Documentation

| Audience | Guide | Description |
|----------|-------|-------------|
| 📖 **Humans** | [User Guide](docs/user-guide.md) | Full setup, configuration, REST API, MCP config, examples |
| 🤖 **AI Agents** | [Agent Integration Guide](docs/agent-integration-guide.md) | Step-by-step protocol for AI agents helping users install Memoria |
| 🚀 **Operators** | [Deployment Guide](docs/deployment.md) | Docker Compose, env vars, security, production setup |
| 📡 **Developers** | [API Reference](docs/api-reference.md) | REST API endpoints, request/response formats |

---

## Why Memoria?

| Capability | Memoria | Letta / Mem0 / Traditional RAG |
|---|---|---|
| Git-level version control | Native zero-copy snapshots & branches | File-level or none |
| Isolated experimentation | One-click branch, merge after validation | Manual data duplication |
| Audit trail | Full snapshot + provenance on every mutation | Limited logging |
| Semantic retrieval | Vector + full-text hybrid search | Vector only |
| Multi-agent sharing | Shared trusted memory pool per user | Siloed per agent |
| Migration cost | Zero — all state in MatrixOne | Export/import required |

---

## Quick Start

### 1. Start MatrixOne

```bash
git clone https://github.com/matrixorigin/Memoria.git
cd Memoria
docker compose up -d
# Wait ~30-60s for first-time initialization
```

Don't want Docker? Use [MatrixOne Cloud](https://cloud.matrixorigin.cn) (free tier).

### 2. Install Memoria

Download from [GitHub Releases](https://github.com/matrixorigin/Memoria/releases):

```bash
# Linux (x86_64)
curl -LO https://github.com/matrixorigin/Memoria/releases/latest/download/memoria-x86_64-unknown-linux-gnu.tar.gz
tar xzf memoria-x86_64-unknown-linux-gnu.tar.gz
sudo mv memoria /usr/local/bin/

# macOS (Apple Silicon)
curl -LO https://github.com/matrixorigin/Memoria/releases/latest/download/memoria-aarch64-apple-darwin.tar.gz
tar xzf memoria-aarch64-apple-darwin.tar.gz
sudo mv memoria /usr/local/bin/
```

### 3. Configure your AI tool

```bash
cd your-project
memoria init    # auto-detects Kiro / Cursor / Claude, writes MCP config + steering rules
```

With an embedding service (recommended):
```bash
memoria init --embedding-provider openai \
             --embedding-base-url https://api.siliconflow.cn/v1 \
             --embedding-api-key sk-... \
             --embedding-model BAAI/bge-m3 \
             --embedding-dim 1024
```

Or connect to an existing Memoria server (no DB needed):
```bash
memoria init --api-url "https://your-server:8100" --token "sk-your-key..."
```

⚠️ **Configure embedding BEFORE the MCP server starts for the first time.** Tables are created on first startup with the configured dimension.

### 4. Restart & verify

Restart your AI tool, then ask: *"Do you have memory tools available?"*

```bash
memoria status   # check config files and rule versions
```

For detailed setup options, see the [User Guide](docs/user-guide.md).

---

## See It In Action

A real setup conversation in Cursor — tell the agent what you want, it handles the rest:

```
You:     I want to set up Memoria for persistent memory.

Cursor:  Before I start, a few quick questions:

           1. Which AI tool are you using? (Cursor, Kiro, Claude Code, or multiple?)
           2. Do you have a MatrixOne database running? If not, I can start one with
              Docker, or you can use MatrixOne Cloud (free tier, no Docker needed).
           3. Do you have an embedding service? (OpenAI, SiliconFlow, Ollama, etc.)
              If not, I can use a local model — requires building from source.

You:     Cursor. No database yet. I have a SiliconFlow API key.

Cursor:  Got it. I'll start MatrixOne with Docker first — running:

           docker compose up -d

         ...

Cursor:  Everything's configured. Try telling me a preference — like your test framework
         or code style — and I'll remember it across all future conversations.
```

This guided flow is powered by the [Agent Integration Guide](docs/agent-integration-guide.md).

---

## Architecture

```
┌─────────────┐     MCP (stdio)     ┌──────────────────────────────────────┐     SQL      ┌────────────┐
│  Kiro /      │ ◄─────────────────► │  Memoria MCP Server                │ ◄──────────► │ MatrixOne  │
│  Cursor /    │   store / retrieve  │  ├── Canonical Storage              │  vector +    │  Database  │
│  Claude Code │                     │  ├── Retrieval (vector / semantic)  │  fulltext    │            │
│  Any Agent   │                     │  └── Git-for-Data (snap/branch/merge)│             │            │
└─────────────┘                      └──────────────────────────────────────┘              └────────────┘
```

---

## API Reference

Memoria exposes MCP tools that your AI tool calls automatically based on steering rules.

### Core CRUD

| Tool | Description |
|------|-------------|
| `memory_store` | Store a new memory |
| `memory_retrieve` | Retrieve relevant memories for a query (call at conversation start) |
| `memory_correct` | Update an existing memory with new content (by ID or semantic search) |
| `memory_purge` | Delete by ID, comma-separated batch IDs, or bulk-delete by topic keyword |
| `memory_search` | Semantic search across all memories |
| `memory_profile` | Get user's memory-derived profile summary |

### Snapshots

| Tool | Description |
|------|-------------|
| `memory_snapshot` | Create a named snapshot of current memory state |
| `memory_snapshots` | List snapshots with pagination (`limit`, `offset`). Shows total count |
| `memory_snapshot_delete` | Delete snapshots by name(s), prefix, or age. Supports batch deletion |
| `memory_rollback` | Restore memories to a previous snapshot |

### Branches

| Tool | Description |
|------|-------------|
| `memory_branch` | Create a new branch for isolated experimentation (optionally from a snapshot or point-in-time) |
| `memory_branches` | List all branches |
| `memory_checkout` | Switch to a different branch (shows up to `top_k` memories after switching) |
| `memory_merge` | Merge a branch back into main |
| `memory_diff` | Preview what would change on merge (LCA-based diff with semantic classification) |
| `memory_branch_delete` | Delete a branch |

### Maintenance

| Tool | Description |
|------|-------------|
| `memory_governance` | Quarantine low-confidence memories, clean stale data (1h cooldown) |
| `memory_consolidate` | Detect contradictions, fix orphaned graph nodes (30min cooldown) |
| `memory_reflect` | Synthesize high-level insights from memory clusters via LLM (2h cooldown) |
| `memory_extract_entities` | Extract named entities and build entity graph (proactive) |
| `memory_link_entities` | Write entity links from your own extraction results |
| `memory_rebuild_index` | Rebuild IVF vector index for a table |

For REST API details, see the [API Reference](docs/api-reference.md).

---

## Memory Types

| Type | What it stores | Example |
|------|---------------|---------|
| `semantic` | Project facts, technical decisions | "This project uses Go 1.22 with modules" |
| `profile` | User/agent preferences | "Always use pytest, never unittest" |
| `procedural` | How-to knowledge, workflows | "To deploy: run make build then kubectl apply" |
| `working` | Temporary context for current task | "Currently refactoring the auth module" |
| `tool_result` | Results from tool executions | Cached command outputs |
| `episodic` | Session summaries (topic/action/outcome) | "Session Summary: Database optimization\n\nActions: Added indexes\n\nOutcome: 93% faster" |

**Episodic Memory**: High-level summaries of work sessions, generated via API. See [Episodic Memory API](docs/api/episodic_memory.md) for details.

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
AI:  → calls memory_correct(query="formatting tool", new_content="User uses ruff for formatting", reason="switched from black")
```

### Snapshots: Save and Restore State

```
You: "Take a snapshot before we refactor the database layer"
AI:  → calls memory_snapshot(name="before_db_refactor", description="pre-refactor state")

... refactoring goes wrong ...

You: "Roll back to before the refactor"
AI:  → calls memory_rollback(name="before_db_refactor")
```

### Branches: Isolated Experimentation

```
You: "Create a memory branch to evaluate switching from PostgreSQL to SQLite"
AI:  → calls memory_branch(name="eval_sqlite")
     → calls memory_checkout(name="eval_sqlite")

You: "We're now using SQLite instead of PostgreSQL"
AI:  → calls memory_store("Project uses SQLite for persistence", type="semantic")
     (stored on eval_sqlite only — main is untouched)

You: "Merge it"
AI:  → calls memory_diff(source="eval_sqlite")   ← preview first
     → calls memory_merge(source="eval_sqlite", strategy="replace")
```

---

## Commands

| Command | Description |
|---------|-------------|
| `memoria init` | Auto-detect AI tool, write MCP config + steering rules |
| `memoria status` | Show config files, rule versions, bundled version |
| `memoria update-rules` | Update steering rules to match current binary version |
| `memoria mcp --db-url <url> --user <id>` | Start MCP server in embedded mode (direct DB) |
| `memoria mcp --api-url <url> --token <key>` | Start MCP server in remote mode (proxy to REST API) |
| `memoria mcp --transport sse` | Start with SSE transport instead of stdio |
| `memoria serve` | Start REST API server |
| `memoria benchmark --api-url <url> --token <key> --dataset <name>` | Run benchmark against API |

---

## Development

### Quick setup (local dev)

```bash
# Start MatrixOne + API
make up

# In another terminal, configure your AI tool for remote mode:
cd your-project
memoria init --api-url "http://localhost:8100" --token "test-master-key-for-docker-compose"

# Restart your AI tool
```

Or use embedded mode (direct DB, no API):
```bash
cd your-project
memoria init --db-url "mysql+pymysql://root:111@localhost:6001/memoria"
```

### Run tests

```bash
make test-unit          # Unit tests (no DB)
make test               # All tests (needs DB)
make test-e2e           # E2E API tests (needs DB)
```

### Bump version and publish

```bash
make release VERSION=0.2.0      # Bump version, generate CHANGELOG, tag, push
                                 # CI builds binaries + Docker image automatically
make release-rc VERSION=0.2.0-rc1  # Pre-release
```

---

## License

Apache-2.0 © [MatrixOrigin](https://github.com/matrixorigin)
