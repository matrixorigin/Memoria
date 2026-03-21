# OpenClaw Setup

Use this when Memoria needs to be installed, enabled, or verified as OpenClaw's memory plugin.

## Install

Preferred package install:

```bash
openclaw plugins install @matrixorigin/memory-memoria
openclaw plugins enable memory-memoria

MEMORIA_DB_URL='mysql://root:111@127.0.0.1:6001/memoria' \
MEMORIA_EMBEDDING_PROVIDER='openai' \
MEMORIA_EMBEDDING_MODEL='text-embedding-3-small' \
MEMORIA_EMBEDDING_API_KEY='sk-...' \
MEMORIA_EMBEDDING_DIM='1536' \
openclaw memoria install
```

Local checkout install:

```bash
openclaw plugins install --link /path/to/Memoria/plugins/openclaw
openclaw plugins enable memory-memoria
openclaw memoria install
```

## Backend Modes

- `embedded`: run local `memoria mcp` against MatrixOne
- `http`: connect to an existing Memoria API

## Verify

```bash
openclaw plugins list
openclaw memoria capabilities
openclaw memoria verify
openclaw memoria stats
openclaw ltm list --limit 10
```

Success means:

- `memory-memoria` is enabled
- `openclaw memoria verify` passes
- a store and retrieve round-trip works

## Important Notes

- OpenClaw reserves `openclaw memory` for built-in file memory, so this plugin uses `openclaw memoria` and `openclaw ltm`.
- The plugin defaults to explicit memory writes rather than silent auto-capture.
- Choose embedding model and dimensions before first startup.
- If upgrading from an old stack, use a fresh DB name to avoid schema drift.
