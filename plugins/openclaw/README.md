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

If you want the shortest OpenClaw-native path:

- run MatrixOne locally, or use MatrixOne Cloud
- use an OpenAI-compatible embedding API
- install the plugin with `openclaw plugins install`
- let the plugin install or validate the Rust `memoria` runtime through `openclaw memoria install`

Example:

```bash
openclaw plugins install --link /path/to/openclaw-memoria/plugins/openclaw
openclaw plugins enable memory-memoria

MEMORIA_DB_URL='mysql://root:111@127.0.0.1:6001/memoria' \
MEMORIA_EMBEDDING_PROVIDER='openai' \
MEMORIA_EMBEDDING_MODEL='text-embedding-3-small' \
MEMORIA_EMBEDDING_API_KEY='sk-...' \
MEMORIA_EMBEDDING_DIM='1536' \
openclaw memoria install
```

If you published the package to npm, the install step becomes:

```bash
openclaw plugins install @matrixorigin/memory-memoria
openclaw plugins enable memory-memoria
openclaw memoria install
```

The installer will:

- reuse the current plugin checkout or installed package
- install the Rust `memoria` binary if it is missing
- enable `memory-memoria` in OpenClaw
- write the plugin config into `~/.openclaw/openclaw.json`
- add the Memoria tool surface to global and existing agent tool policy
- install the managed skills in `~/.openclaw/skills`

## Installer Inputs

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

## Uninstall

```bash
curl -fsSL https://raw.githubusercontent.com/matrixorigin/openclaw-memoria/main/scripts/uninstall-openclaw-memoria.sh | \
  bash -s --
```

That removes the plugin entry, tool policy additions, managed skills, and the default managed checkout path.

## Verification

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
- `openclaw memoria install` is the preferred onboarding entrypoint once the plugin has been installed through OpenClaw
- `openclaw memoria verify` runs OpenClaw config validation plus deeper `stats` / `ltm list` checks when the embedded database is reachable

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
