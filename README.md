<div align="center">
  <img src="assets/memoria-logo.png" alt="Memoria Logo" width="300"/>
  
  # Memoria
  
  **Secure · Auditable · Programmable Memory for AI Agents**
  
  [![MatrixOne](https://img.shields.io/badge/Powered%20by-MatrixOne-00ADD8?style=flat-square&logo=database)](https://github.com/matrixorigin/matrixone)
  [![MCP](https://img.shields.io/badge/Protocol-MCP-7C3AED?style=flat-square)](https://modelcontextprotocol.io)
  [![Git for Data](https://img.shields.io/badge/Git%20for%20Data-Enabled-00A3CC?style=flat-square)](https://github.com/matrixorigin/matrixone)
  [![License](https://img.shields.io/badge/License-Apache%202.0-blue.svg?style=flat-square)](LICENSE)
  
  [Quick Start](#quick-start) · [Steering Rules](#steering-rules) · [API Reference](#api-reference) · [For AI Agents](#for-ai-agents)
  
</div>

---

## Overview

Memoria is a **persistent memory layer** for AI agents with Git-level version control.
Every memory change is tracked, auditable, and reversible — snapshots, branches, merges, and time-travel rollback, all powered by MatrixOne's native Copy-on-Write engine.

**Core Capabilities:**
- **Cross-conversation memory** — preferences, facts, and decisions persist across sessions
- **Semantic search** — retrieves memories by meaning, not just keywords
- **Git for Data** — zero-copy branching, instant snapshots, point-in-time rollback
- **Audit trail** — every memory mutation has a snapshot + provenance chain
- **Self-maintaining** — built-in governance detects contradictions, quarantines low-confidence memories
- **Private by default** — local embedding model option, no data leaves your machine

**Supported Agents:** [Kiro](https://kiro.dev) · [Cursor](https://cursor.sh) · [Claude Code](https://docs.anthropic.com/en/docs/claude-code) · [Codex](https://openai.com/index/introducing-codex/) · [OpenClaw](plugins/openclaw/README.md) · Any MCP-compatible agent

**Storage Backend:** [MatrixOne](https://github.com/matrixorigin/matrixone) — Distributed database with native vector indexing

---

## Why Memoria?

| Capability | Memoria | Letta / Mem0 / Traditional RAG |
|---|---|---|
| Git-level version control | Native zero-copy snapshots & branches | File-level or none |
| Isolated experimentation | One-click branch, merge after validation | Manual data duplication |
| Audit trail | Full snapshot + provenance on every mutation | Limited logging |
| Semantic retrieval | Vector + full-text hybrid search | Vector only |
| Self-governance | Automatic contradiction detection & quarantine | Manual cleanup |

---

## Quick Start

### 1. Start MatrixOne

```bash
git clone https://github.com/matrixorigin/Memoria.git
cd Memoria
docker compose up -d
```

Or use [MatrixOne Cloud](https://cloud.matrixorigin.cn) (free tier, no Docker needed).

### 2. Install Memoria

```bash
curl -sSL https://raw.githubusercontent.com/matrixorigin/Memoria/main/scripts/install.sh | bash
```

Or download from [GitHub Releases](https://github.com/matrixorigin/Memoria/releases).

### 3. Configure your AI tool

```bash
cd your-project
memoria init -i   # Interactive wizard (recommended)
```

This creates MCP config + steering rules for your AI tool (Kiro, Cursor, Claude, or Codex).

### 🦞 OpenClaw Plugin (Already Using OpenClaw?)

Use the native OpenClaw plugin guide: [OpenClaw Plugin Setup](plugins/openclaw/README.md).

```bash
openclaw plugins install @matrixorigin/memory-memoria
openclaw plugins enable memory-memoria
openclaw memoria install
```

### 4. Restart & verify

Restart your AI tool, then ask: *"Do you have memory tools available?"*

For detailed setup, see [Setup Skill](skills/setup/SKILL.md).

---

## Steering Rules

Steering rules teach your AI agent **when and how** to use memory tools. Without them, the agent has tools but no guidance — like having a database without knowing the schema.

### What They Do

| Rule | Purpose |
|------|---------|
| `memory` | Core memory tools — when to store, retrieve, correct, purge |
| `session-lifecycle` | Bootstrap at conversation start, cleanup at end |
| `memory-hygiene` | Proactive governance, contradiction resolution, snapshot cleanup |
| `memory-branching-patterns` | Isolated experiments with branches |
| `goal-driven-evolution` | Track goals, plans, progress across conversations |

### Example: Conversation Lifecycle

```
┌─────────────────────────────────────────────────────────────────────────────┐
│  CONVERSATION START                                                         │
│  ┌─────────────────────────────────────────────────────────────────────┐   │
│  │ 1. memory_retrieve(query="<user's question>")  ← load context       │   │
│  │ 2. memory_search(query="GOAL ACTIVE")          ← check active goals │   │
│  └─────────────────────────────────────────────────────────────────────┘   │
├─────────────────────────────────────────────────────────────────────────────┤
│  MID-CONVERSATION                                                           │
│  ┌─────────────────────────────────────────────────────────────────────┐   │
│  │ • User states preference → memory_store(type="profile")             │   │
│  │ • User corrects a fact   → memory_correct(query="...", new="...")   │   │
│  │ • Topic shifts           → memory_retrieve(query="<new topic>")     │   │
│  └─────────────────────────────────────────────────────────────────────┘   │
├─────────────────────────────────────────────────────────────────────────────┤
│  CONVERSATION END                                                           │
│  ┌─────────────────────────────────────────────────────────────────────┐   │
│  │ 1. memory_purge(topic="<task>")  ← clean up working memories        │   │
│  │ 2. memory_store(type="episodic") ← save session summary             │   │
│  └─────────────────────────────────────────────────────────────────────┘   │
└─────────────────────────────────────────────────────────────────────────────┘
```

### Example: Goal-Driven Evolution

```
You: "I want to add OAuth support to the API"

AI:  → memory_search(query="GOAL OAuth")           ← check for existing goal
     → memory_store(content="🎯 GOAL: Add OAuth support\nStatus: ACTIVE", type="procedural")
     
     ... works on implementation, stores progress as working memories ...
     
     → memory_store(content="✅ STEP 1/3: Added OAuth routes", type="working")
     → memory_store(content="❌ STEP 2/3: Token refresh failed — need to fix expiry logic", type="working")

... next conversation ...

AI:  → memory_search(query="GOAL ACTIVE")          ← finds OAuth goal
     → memory_search(query="STEP for GOAL OAuth")  ← loads progress
     "Last time we were working on OAuth. Step 2 failed on token refresh. Want to continue?"

... goal completed ...

AI:  → memory_correct(query="GOAL OAuth", new_content="🎯 GOAL: OAuth — ✅ ACHIEVED")
     → memory_store(content="💡 LESSON: Token refresh needs 5min buffer before expiry", type="procedural")
     → memory_purge(topic="STEP for GOAL OAuth")   ← clean up working memories
```

### Example: Branch for Risky Experiments

```
You: "Let's try switching from PostgreSQL to SQLite"

AI:  → memory_branch(name="eval_sqlite")
     → memory_checkout(name="eval_sqlite")
     
     ... experiments on branch, stores findings ...
     
     → memory_diff(source="eval_sqlite")     ← preview changes
     → memory_checkout(name="main")
     → memory_merge(source="eval_sqlite")    ← or delete if failed
```

### File Locations

- Kiro: `.kiro/steering/*.md`
- Cursor: `.cursor/rules/*.mdc`
- Claude: `.claude/rules/*.md`
- Codex: `AGENTS.md`

### Update Rules

After upgrading Memoria:
```bash
memoria rules --force
```

---

## API Reference

### Core Tools

| Tool | Description |
|------|-------------|
| `memory_store` | Store a new memory |
| `memory_retrieve` | Retrieve relevant memories (call at conversation start) |
| `memory_search` | Semantic search across all memories |
| `memory_correct` | Update an existing memory |
| `memory_purge` | Delete by ID or topic keyword |
| `memory_list` | List active memories |
| `memory_profile` | Get user's memory-derived profile |
| `memory_feedback` | Record relevance feedback (useful/irrelevant/outdated/wrong) |
| `memory_capabilities` | List available memory tools |

### Snapshots & Branches

| Tool | Description |
|------|-------------|
| `memory_snapshot` | Create named snapshot |
| `memory_snapshots` | List snapshots with pagination |
| `memory_snapshot_delete` | Delete snapshots by name, prefix, or age |
| `memory_rollback` | Restore to snapshot |
| `memory_branch` | Create isolated branch |
| `memory_branches` | List all branches |
| `memory_checkout` | Switch branch |
| `memory_merge` | Merge branch back |
| `memory_branch_delete` | Delete a branch |
| `memory_diff` | Preview merge changes |

### Maintenance

| Tool | Description |
|------|-------------|
| `memory_governance` | Quarantine low-confidence memories (1h cooldown) |
| `memory_consolidate` | Detect contradictions (30min cooldown) |
| `memory_reflect` | Synthesize insights (2h cooldown) |

> `memory_rebuild_index`, `memory_observe`, `memory_get_retrieval_params`, `memory_tune_params`, `memory_extract_entities`, and `memory_link_entities` are available via REST API but hidden from MCP tool listing — they are ops/debug tools not intended for agent use.

Full API details: [API Reference Skill](skills/api-reference/SKILL.md)

---

## Memory Types

| Type | Use for | Example |
|------|---------|---------|
| `semantic` | Project facts, decisions | "Uses Go 1.22 with modules" |
| `profile` | User preferences | "Prefers pytest over unittest" |
| `procedural` | Workflows, how-to | "Deploy: make build && kubectl apply" |
| `working` | Temporary task context | "Currently debugging auth module" |
| `episodic` | Session summaries | "Session: optimized DB, added indexes" |

---

## Commands

| Command | Description |
|---------|-------------|
| `memoria init -i` | Interactive setup wizard |
| `memoria status` | Show config and rule versions |
| `memoria rules` | Update steering rules (auto-detect, `--tool`, or `-i`) |
| `memoria mcp` | Start MCP server |
| `memoria serve` | Start REST API server |
| `memoria benchmark` | Run benchmark suite |

---

## For AI Agents

If you're an AI agent helping a user set up Memoria:

1. **Load the [Setup Skill](skills/setup/SKILL.md)** — it has step-by-step instructions
2. **Ask before acting**:
   - Which AI tool? (Kiro / Cursor / Claude / Codex)
   - MatrixOne database? (Docker / Cloud / existing)
   - Embedding service? (OpenAI / SiliconFlow / local)
3. **Run `memoria init -i`** in the user's project directory
4. **Tell user to restart** their AI tool
5. **Verify** with `memory_retrieve("test")`

⚠️ **Configure embedding BEFORE first MCP server start** — dimension is locked into schema.

---

## Architecture

```
┌─────────────┐     MCP (stdio)     ┌──────────────────────────────────────┐     SQL      ┌────────────┐
│  AI Agent   │ ◄─────────────────► │  Memoria MCP Server                  │ ◄──────────► │ MatrixOne  │
│             │   store / retrieve  │  ├── Canonical Storage               │  vector +    │  Database  │
│             │                     │  ├── Retrieval (vector / semantic)   │  fulltext    │            │
│             │                     │  └── Git-for-Data (snap/branch/merge)│              │            │
└─────────────┘                     └──────────────────────────────────────┘              └────────────┘
```

For codebase details, see [Architecture Skill](skills/architecture/SKILL.md).

---

## Development

```bash
make up              # Start MatrixOne + API
make test            # Run all tests
make release VERSION=0.2.0   # Bump, tag, push
```

**Developer documentation** (for contributing to Memoria):

| Skill | Description |
|-------|-------------|
| [Architecture](skills/architecture/SKILL.md) | Codebase layout, traits, tables |
| [API Reference](skills/api-reference/SKILL.md) | REST endpoints, request/response |
| [Deployment](skills/deployment/SKILL.md) | Docker, K8s, multi-instance |
| [Plugin Development](skills/plugin-development/SKILL.md) | Governance plugins |
| [Release](skills/release/SKILL.md) | Version bump, CI/CD |
| [Local Embedding](skills/local-embedding/SKILL.md) | Offline embedding build |

---

## License

Apache-2.0 © [MatrixOrigin](https://github.com/matrixorigin)
