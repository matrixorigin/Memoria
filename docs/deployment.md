# Deployment Guide

## Docker Compose (Recommended)

A `.env` file is pre-configured for local development. Just start:

```bash
cd memoria
docker compose up -d
```

For a fresh environment, copy the template and fill in your values:

```bash
cp .env.example .env
# Set MEMORIA_MASTER_KEY and MEMORIA_EMBEDDING_API_KEY
docker compose up -d
```

> The build context is the project root (`context: ..` in docker-compose.yml) because Memoria depends on `core/` and shared files in `api/`. Both `cd memoria && docker compose up -d` and `docker compose -f memoria/docker-compose.yml up -d` work.

This starts two services:

| Service | Port | Description |
|---------|------|-------------|
| API | 8100 | Memoria REST API |
| MatrixOne | 6001 | HTAP database (memory storage, vector search, snapshots) |

Verify:
```bash
curl http://localhost:8100/health
# {"status": "ok", "database": "connected"}
```

## Environment Variables

| Variable | Required | Default | Description |
|----------|----------|---------|-------------|
| `MEMORIA_MASTER_KEY` | **Yes** | — | Admin API key (min 16 chars) |
| `MEMORIA_API_KEY_SECRET` | Recommended | — | HMAC secret for API key hashing. If unset, falls back to `MASTER_KEY` — set this so rotating `MASTER_KEY` doesn't invalidate existing keys |
| `MEMORIA_DB_HOST` | No | `matrixone` | MatrixOne host |
| `MEMORIA_DB_PORT` | No | `6001` | MatrixOne port |
| `MEMORIA_DB_USER` | No | `root` | Database user |
| `MEMORIA_DB_PASSWORD` | No | `111` | Database password |
| `MEMORIA_DB_NAME` | No | `memoria` | Database name |
| `MEMORIA_EMBEDDING_PROVIDER` | No | `local` | `local` or `openai` |
| `MEMORIA_EMBEDDING_MODEL` | No | `all-MiniLM-L6-v2` | Embedding model name |
| `MEMORIA_EMBEDDING_API_KEY` | No | — | Required if provider is `openai` |
| `MEMORIA_EMBEDDING_BASE_URL` | No | — | Custom embedding endpoint (OpenAI-compatible) |
| `MEMORIA_EMBEDDING_DIM` | No | `0` (auto) | Embedding dimension, 0 = auto-infer |
| `API_PORT` | No | `8100` | Host-side API port |
| `MATRIXONE_PORT` | No | `6001` | Host-side MatrixOne port |
| `MATRIXONE_DEBUG_PORT` | No | — | Expose MatrixOne pprof port (e.g. `6060`) |
| `MATRIXONE_DATA_DIR` | No | `./data/matrixone` | Host path for MatrixOne data (bind mount) |

## Data Persistence

MatrixOne data is bind-mounted to `./data/matrixone` (relative to `memoria/`). Data survives container restarts and `docker compose down`. To change the path:

```bash
MATRIXONE_DATA_DIR=/your/path docker compose up -d
```

## External MatrixOne

To use an existing MatrixOne instance instead of the bundled one:

```bash
# .env
MEMORIA_DB_HOST=your-matrixone-host
MEMORIA_DB_PORT=6001
MEMORIA_DB_USER=root
MEMORIA_DB_PASSWORD=your-password
```

Start only the API:
```bash
docker compose up -d api
```

Tables are auto-created on first startup.

## Embedding Options

### OpenAI-compatible (recommended)

No extra build step needed. Works with OpenAI, SiliconFlow, Azure OpenAI, local vLLM, etc.

```bash
MEMORIA_EMBEDDING_PROVIDER=openai
MEMORIA_EMBEDDING_MODEL=BAAI/bge-m3
MEMORIA_EMBEDDING_DIM=1024
MEMORIA_EMBEDDING_API_KEY=sk-...
MEMORIA_EMBEDDING_BASE_URL=https://api.siliconflow.cn/v1
```

### Local (no API key)

Requires building from source with the `local-embedding` feature:

```bash
cd Memoria/memoria
cargo build --release -p memoria-cli --features local-embedding
```

```bash
MEMORIA_EMBEDDING_PROVIDER=local
MEMORIA_EMBEDDING_MODEL=all-MiniLM-L6-v2
```

## Debug Port

To expose MatrixOne's pprof/debug HTTP port:

```bash
# .env
MATRIXONE_DEBUG_PORT=6060
```

Then access `http://localhost:6060/debug/pprof/` for profiling.

## Rate Limits

All limits are configurable via env vars (format: `max_requests,window_seconds`):

```bash
MEMORIA_RATE_LIMIT_AUTH_KEYS=1000,60    # relaxed for testing
MEMORIA_RATE_LIMIT_CONSOLIDATE=100,60
MEMORIA_RATE_LIMIT_REFLECT=100,60
```

See the rate limit configuration in the API server source for all configurable keys and defaults.

## Automated Governance

A background scheduler starts automatically with the API server:

| Frequency | Task |
|-----------|------|
| Hourly | Confidence decay for stale memories, quarantine low-quality entries |
| Daily | Clean up expired/quarantined memories |
| Weekly | Compress redundant memories |

Admins can also trigger governance manually per user:

```bash
curl -X POST http://localhost:8100/admin/governance/alice/trigger \
  -H "Authorization: Bearer YOUR_MASTER_KEY"
```

## Security Notes

- API keys are HMAC-SHA256 hashed at rest (keyed with `MEMORIA_API_KEY_SECRET`, falls back to `MASTER_KEY`) — raw keys are never stored
- Set `MEMORIA_API_KEY_SECRET` independently of `MASTER_KEY` so you can rotate the master key without invalidating existing user API keys
- All queries are scoped to the authenticated user's `user_id`
- Master key is required for all admin operations
- Snapshot names are sanitized and regex-validated before entering SQL
- Rate limiting is per API key (in-memory sliding window)
- Run behind a reverse proxy (nginx/Caddy) with TLS in production
