# Memoria

**Secure · Auditable · Programmable Memory for AI Agents**

[![MCP](https://img.shields.io/badge/Protocol-MCP-7C3AED?style=flat-square)](https://modelcontextprotocol.io)
[![License](https://img.shields.io/badge/License-Apache%202.0-blue.svg?style=flat-square)](https://github.com/matrixorigin/Memoria/blob/main/LICENSE)
[![PyPI](https://img.shields.io/pypi/v/memoria?style=flat-square)](https://pypi.org/project/memoria/)

Persistent memory layer for AI agents (Kiro, Cursor, Claude Code, any MCP-compatible agent) with Git-level version control — snapshots, branches, rollback, and full audit trail.

Full documentation: **https://github.com/matrixorigin/Memoria**

---

## Two Modes

| | Managed / Remote | Self-hosted |
|---|---|---|
| **Flag** | `--api-url` + `--token` | `--db-url` |
| **Requires** | Nothing — connect to existing server | MatrixOne DB + embedding config |
| **When** | Team / SaaS, admin gives you a URL + token | Personal setup, local dev |

---

## Install

```bash
# Managed / remote mode — no extras needed
pip install memoria

# Self-hosted embedded mode — choose an embedding provider:
pip install "memoria[openai-embedding]"   # OpenAI / SiliconFlow / any OpenAI-compatible endpoint
pip install "memoria[local-embedding]"    # Local sentence-transformers (~900MB download)

# If no NVIDIA GPU available, install CPU-only PyTorch first to avoid large CUDA dependencies:
pip install torch --index-url https://download.pytorch.org/whl/cpu
pip install "memoria[local-embedding]"
```

---

## Quick Start

### Managed mode (no database, no embedding setup)

If your team or provider gives you a server URL and API token:

```bash
cd your-project
memoria init --api-url "https://your-server:8100" --token "sk-your-key..."
```

Restart your AI tool — done.

### Self-hosted mode (run your own database)

```bash
# 1. Start MatrixOne
git clone https://github.com/matrixorigin/Memoria.git && cd Memoria
docker compose up -d

# 2. Configure
cd your-project
memoria init --db-url "mysql+pymysql://root:111@localhost:6001/memoria"

# With OpenAI-compatible embedding (recommended over local model)
memoria init --db-url "mysql+pymysql://root:111@localhost:6001/memoria" \
             --embedding-provider openai \
             --embedding-base-url https://api.siliconflow.cn/v1 \
             --embedding-api-key sk-... \
             --embedding-model BAAI/bge-m3 \
             --embedding-dim 1024
```

`memoria init` auto-detects Kiro / Cursor / Claude and writes MCP config + steering rules.

### Verify

```bash
memoria status
```

---

## Embedding Providers (self-hosted mode only)

| Provider | Quality | Privacy | Cost | First-use latency |
|----------|---------|---------|------|-------------------|
| **Local** (default) | Good | ✅ Data never leaves machine | Free | ~900MB download on first use |
| **OpenAI / SiliconFlow** | Better | ⚠️ Text sent to API | API key required | None |
| **Custom service** | Varies | Depends on host | Self-hosted | None |

> Managed mode users don't need to configure embedding — the server handles it.

---



`memory_store`, `memory_retrieve`, `memory_correct`, `memory_purge`, `memory_search`, `memory_profile`, `memory_snapshot`, `memory_rollback`, `memory_branch`, `memory_merge`, `memory_diff`, and more.

---

## License

Apache-2.0 © [MatrixOrigin](https://github.com/matrixorigin)
