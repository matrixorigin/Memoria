# OpenClaw

Use this when Memoria is being installed or debugged as an OpenClaw memory plugin.

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

- `embedded`: shell out to local `memoria mcp`
- `http`: talk to an existing Memoria API

## Verification

```bash
openclaw plugins list
openclaw memoria capabilities
openclaw memoria verify
openclaw memoria stats
openclaw ltm list --limit 10
```

## Common Environment

- `MEMORIA_DB_URL`
- `MEMORIA_EMBEDDING_PROVIDER`
- `MEMORIA_EMBEDDING_MODEL`
- `MEMORIA_EMBEDDING_API_KEY`
- `MEMORIA_EMBEDDING_BASE_URL`
- `MEMORIA_EMBEDDING_DIM`
- `MEMORIA_LLM_API_KEY`
- `MEMORIA_LLM_BASE_URL`
- `MEMORIA_LLM_MODEL`
- `MEMORIA_EXECUTABLE`
- `MEMORIA_RELEASE_TAG`

## Important Notes

- The preferred onboarding entrypoint is `openclaw memoria install`.
- The plugin defaults to explicit memory operations instead of silent auto-write.
- If an older database schema causes drift, use a fresh DB name.
