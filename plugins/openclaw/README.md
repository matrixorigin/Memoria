# Memory (Memoria) for OpenClaw

This package turns [MatrixOrigin Memoria](https://github.com/matrixorigin/Memoria) into an installable OpenClaw `memory` plugin.

The plugin targets the current Rust Memoria CLI and API release line. The default installer target is [`v0.1.0`](https://github.com/matrixorigin/Memoria/releases/tag/v0.1.0). There is no bundled Python runtime, no virtualenv, and no `pip install` step.

## Architecture

- `backend: "embedded"` runs the Rust `memoria` CLI locally via `memoria mcp --db-url ... --user ...`
- `backend: "http"` connects to an existing Rust Memoria API server
- OpenClaw keeps its own tool and hook surface, but the storage and retrieval backend is Memoria

In practice that means:

- durable memory still lives in MatrixOne
- snapshots, rollback, branches, merge, diff, governance, reflect, and entity extraction are handled by Rust Memoria
- the plugin shells out to the `memoria` binary instead of importing bundled backend code

## Quick Start

Assume OpenClaw is already installed and healthy.

### Usage Paths

1. GitHub README (this file): cloud-first setup plus local setup.
2. OpenClaw chat/agent: user asks agent to install, agent runs commands and returns raw outputs.

### 1) Cloud (Recommended)

Cloud is the default path for new users.

#### Developer Flow

```bash
# A0. Optional cleanup (if you installed an older/source-linked memory-memoria before)
openclaw plugins uninstall memory-memoria
```

```bash
# A0.5. Ensure memoria CLI exists (required even for cloud mode)
command -v memoria >/dev/null || \
  curl -sSL https://raw.githubusercontent.com/matrixorigin/Memoria/main/scripts/install.sh | bash -s -- -y -d ~/.local/bin
```

```bash
# A1. Install plugin from npm
openclaw plugins install @matrixorigin/memory-memoria
openclaw plugins enable memory-memoria
```

```bash
# A2. If npm package is unavailable, use source-link instead
mkdir -p ~/.openclaw/plugins-src
if [ ! -d ~/.openclaw/plugins-src/Memoria/.git ]; then
  git clone https://github.com/matrixorigin/Memoria.git ~/.openclaw/plugins-src/Memoria
else
  git -C ~/.openclaw/plugins-src/Memoria pull --ff-only
fi
openclaw plugins install --link ~/.openclaw/plugins-src/Memoria/plugins/openclaw
openclaw plugins enable memory-memoria
```

```bash
# B. Connect to Memoria Cloud
openclaw memoria connect \
  --mode cloud \
  --api-url <MEMORIA_API_URL> \
  --api-key <MEMORIA_API_KEY>
```

Success standard:

```bash
openclaw memoria health
# must include: "status": "ok"
```

#### Agent Prompt (Cloud)

```text
Install Memoria plugin for my OpenClaw in cloud mode.
Use:
- apiUrl: <MEMORIA_API_URL>
- apiKey: <MEMORIA_API_KEY>

Execute exactly:
1) Ensure memoria CLI exists:
   command -v memoria >/dev/null || curl -sSL https://raw.githubusercontent.com/matrixorigin/Memoria/main/scripts/install.sh | bash -s -- -y -d ~/.local/bin
2) Try npm install first:
   openclaw plugins install @matrixorigin/memory-memoria
3) If step 2 fails, fallback:
   mkdir -p ~/.openclaw/plugins-src
   if [ ! -d ~/.openclaw/plugins-src/Memoria/.git ]; then git clone https://github.com/matrixorigin/Memoria.git ~/.openclaw/plugins-src/Memoria; else git -C ~/.openclaw/plugins-src/Memoria pull --ff-only; fi
   openclaw plugins install --link ~/.openclaw/plugins-src/Memoria/plugins/openclaw
4) Enable plugin:
   openclaw plugins enable memory-memoria
5) Connect cloud:
   openclaw memoria connect --mode cloud --api-url <MEMORIA_API_URL> --api-key <MEMORIA_API_KEY>
6) Verify:
   openclaw memoria health

Output requirements:
- Return all executed commands and raw outputs
- If failed, classify as network/auth/config and provide exact fix commands
- Do not hide or summarize errors
```

### 2) Local (Embedded)

Use local mode when you explicitly want self-hosted storage/runtime.

#### Developer Flow

```bash
# A. Install memoria CLI (if missing)
curl -sSL https://raw.githubusercontent.com/matrixorigin/Memoria/main/scripts/install.sh | bash -s -- -y -d ~/.local/bin

# B1. Install + enable plugin from npm
openclaw plugins install @matrixorigin/memory-memoria
openclaw plugins enable memory-memoria
```

```bash
# B2. If npm package is unavailable, use source-link instead
mkdir -p ~/.openclaw/plugins-src
if [ ! -d ~/.openclaw/plugins-src/Memoria/.git ]; then
  git clone https://github.com/matrixorigin/Memoria.git ~/.openclaw/plugins-src/Memoria
else
  git -C ~/.openclaw/plugins-src/Memoria pull --ff-only
fi
openclaw plugins install --link ~/.openclaw/plugins-src/Memoria/plugins/openclaw
openclaw plugins enable memory-memoria
```

```bash
# C. Connect local backend
openclaw memoria connect \
  --mode local \
  --db-url <MATRIXONE_DSN> \
  --embedding-provider <provider> \
  --embedding-model <model> \
  --embedding-api-key <embedding_key> \
  --embedding-dim <dim> \
  --memoria-bin ~/.local/bin/memoria
```

Success standard:

```bash
openclaw memoria health
# must include: "status": "ok"
```

#### Agent Prompt (Local)

```text
Install Memoria plugin for my OpenClaw in local mode.
Use:
- dbUrl: <MATRIXONE_DSN>
- embeddingProvider: <provider>
- embeddingModel: <model>
- embeddingApiKey: <embedding_key>
- embeddingDim: <dim>

Execute exactly:
1) Ensure memoria CLI exists (install if missing):
   curl -sSL https://raw.githubusercontent.com/matrixorigin/Memoria/main/scripts/install.sh | bash -s -- -y -d ~/.local/bin
2) Try npm install first:
   openclaw plugins install @matrixorigin/memory-memoria
3) If step 2 fails, fallback:
   mkdir -p ~/.openclaw/plugins-src
   if [ ! -d ~/.openclaw/plugins-src/Memoria/.git ]; then git clone https://github.com/matrixorigin/Memoria.git ~/.openclaw/plugins-src/Memoria; else git -C ~/.openclaw/plugins-src/Memoria pull --ff-only; fi
   openclaw plugins install --link ~/.openclaw/plugins-src/Memoria/plugins/openclaw
4) Enable plugin:
   openclaw plugins enable memory-memoria
5) Connect local:
   openclaw memoria connect --mode local --db-url <MATRIXONE_DSN> --embedding-provider <provider> --embedding-model <model> --embedding-api-key <embedding_key> --embedding-dim <dim> --memoria-bin ~/.local/bin/memoria
6) Verify:
   openclaw memoria health

Output requirements:
- Return all executed commands and raw outputs
- If failed, report the missing dependency/permission/config exactly
- Do not hide or summarize errors
```

## Local Installer Inputs (Optional)

Use this section when you choose `openclaw memoria install` for local bootstrap/repair.

Important environment variables:

- `MEMORIA_DB_URL`: embedded MatrixOne DSN. Default: `mysql://root:111@127.0.0.1:6001/memoria`
- `MEMORIA_EMBEDDING_PROVIDER`: usually `openai`; `local` only works if your `memoria` binary was built with the `local-embedding` feature
- `MEMORIA_EMBEDDING_MODEL`: for example `text-embedding-3-small` or `BAAI/bge-m3`
- `MEMORIA_EMBEDDING_API_KEY`: required unless you intentionally use `local`
- `MEMORIA_EMBEDDING_BASE_URL`: optional for official OpenAI, required for compatible gateways
- `MEMORIA_EMBEDDING_DIM`: must match the embedding model before first startup
- `MEMORIA_LLM_API_KEY`, `MEMORIA_LLM_BASE_URL`, `MEMORIA_LLM_MODEL`: optional, only needed for `autoObserve` and internal LLM-backed Memoria tools
- `MEMORIA_EXECUTABLE`: optional explicit path to an existing `memoria` binary
- `MEMORIA_RELEASE_TAG`: Rust Memoria release tag to install. Default: `v0.1.0`

Installer flags:

- `--openclaw-bin <path|command>`: use an explicit `openclaw` executable
- `--memoria-bin <path|command>`: use an existing `memoria` executable
- `--memoria-version <tag>`: override the Rust Memoria release tag
- `--memoria-install-dir <path>`: where to install `memoria` if it is missing
- `--skip-memoria-install`: require an existing `memoria` executable
- `--skip-plugin-install`: only rewrite plugin config; assume OpenClaw already installed and the plugin already loaded
- `--verify`: run a post-install smoke check

## ⚠️ Common Pitfalls

**macOS `sh` vs `bash`:** The installer script is bash (`#!/usr/bin/env bash`) and uses bash-specific syntax.
If you pipe a script, use `bash -s --`, not `sh -s --`.

```bash
# ✅ Correct
curl -fsSL <url> | bash -s --

# ❌ May fail with "bad substitution"
curl -fsSL <url> | sh -s --
```

**Explicit memory mode is default (`autoObserve=false`):** the plugin does not auto-write memories from conversation turns.
Writes happen when the agent explicitly calls tools like `memory_store` (or related write tools).
This keeps memory writes intentional and reviewable.
If you want auto-capture, set `MEMORIA_AUTO_OBSERVE=true` and provide `MEMORIA_LLM_API_KEY` + `MEMORIA_LLM_MODEL`.

**Old schema vs new runtime:** If you upgraded from an older Memoria setup, existing DB schema may not match current Rust runtime expectations.
Use a fresh database name in `MEMORIA_DB_URL` for a clean install path.

```text
# Old/default style
mysql://root:111@127.0.0.1:6001/memoria

# Clean-start recommendation
mysql://root:111@127.0.0.1:6001/memoria_v2
```

## Tool Surface

The OpenClaw plugin exposes:

- `memory_search`, `memory_get`, `memory_store`, `memory_retrieve`, `memory_recall`
- `memory_list`, `memory_stats`, `memory_profile`, `memory_correct`, `memory_purge`, `memory_forget`, `memory_health`
- `memory_observe`, `memory_governance`, `memory_consolidate`, `memory_reflect`, `memory_extract_entities`, `memory_link_entities`, `memory_rebuild_index`, `memory_capabilities`
- `memory_snapshot`, `memory_snapshots`, `memory_rollback`
- `memory_branch`, `memory_branches`, `memory_checkout`, `memory_branch_delete`, `memory_merge`, `memory_diff`
- compatibility CLI aliases under `openclaw ltm ...`

## Compatibility Notes

This plugin now follows the Rust Memoria behavior, not the old embedded Python bridge.

Current differences to be aware of:

- `memory_get` is resolved from recent tool results plus a bounded scan, because the Rust MCP toolset does not expose a direct get-by-id call
- `memory_stats` is derived from available MCP outputs, so inactive-memory totals and entity totals are not currently available
- `memory_entities` is no longer exposed, because the Rust Memoria MCP toolset does not provide a matching tool
- old `mysql+pymysql://...` DSNs are normalized to `mysql://...` automatically during install and config parsing
- if you previously used an older Memoria stack, schema drift can cause runtime errors; using a fresh DB name (for example `memoria_v2`) avoids most upgrade collisions

## Uninstall

```bash
curl -fsSL https://raw.githubusercontent.com/matrixorigin/Memoria/main/plugins/openclaw/scripts/uninstall-openclaw-memoria.sh | \
  bash -s --
```

That removes the plugin entry, tool policy additions, managed skills, and the default managed checkout path.

## Verification

### Success Criteria

| Level | Check | Command | Pass |
|---|---|---|---|
| 1. Plugin loaded | OpenClaw recognizes plugin | `openclaw plugins list` | `memory-memoria` is listed and enabled |
| 2. Backend reachable | Memoria can reach configured backend | `openclaw memoria health` | returns `status: ok` |
| 3. Memory persisted | Store -> retrieve round-trip works | `openclaw memoria stats` + `openclaw ltm list --limit 10` | non-zero memory appears after a write |

Before the smoke check, confirm the CLIs you are about to use are the ones you expect:

```bash
openclaw --version
openclaw memoria verify
```

After install:

```bash
openclaw memoria capabilities
openclaw memoria stats
openclaw ltm list --limit 10
```

Notes:

- `openclaw memoria capabilities` is a config/plugin check and does not require a live Memoria backend
- `openclaw memoria stats` and `openclaw ltm list` require the configured backend to be reachable; in embedded mode that means MatrixOne must be up and the embedding config must be valid
- OpenClaw reserves `openclaw memory` for its built-in file memory, so this plugin uses `openclaw memoria` and the compatibility alias `openclaw ltm`
- `openclaw memoria connect` is the preferred config entrypoint for cloud/local mode switching
- `openclaw memoria install` is optional local bootstrap/repair (runtime + config rewrite)
- `openclaw memoria verify` is an optional deeper diagnostic; `openclaw memoria health` is the primary quick connectivity check

If `openclaw memoria connect` is missing:

```bash
openclaw plugins update memory-memoria
openclaw plugins enable memory-memoria
openclaw memoria --help
```

Low-level fallback:

```bash
node scripts/verify_plugin_install.mjs \
  --openclaw-bin "$(which openclaw)" \
  --memoria-bin "$(which memoria)"
```

## Development

What changed in this repo:

- `openclaw/client.ts` now talks to Rust Memoria over MCP stdio
- the plugin manifest and config surface now use `memoriaExecutable`
- the installer/uninstaller are pure shell + Node, with no Python dependency
- the package no longer publishes the old bundled Python runtime
