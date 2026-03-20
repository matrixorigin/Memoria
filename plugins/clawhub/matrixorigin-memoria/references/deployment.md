# Deployment

Use this for Docker Compose, Kubernetes, and production configuration.

## Docker Compose

```bash
cd Memoria
docker compose up -d
curl http://localhost:8100/health
```

Default service ports:

- Memoria API: `8100`
- MatrixOne: `6001`

## Required Environment

- `MEMORIA_MASTER_KEY`

## Common Configuration

- `MEMORIA_DB_HOST`
- `MEMORIA_DB_PORT`
- `MEMORIA_DB_USER`
- `MEMORIA_DB_PASSWORD`
- `MEMORIA_DB_NAME`
- `MEMORIA_EMBEDDING_PROVIDER`
- `MEMORIA_EMBEDDING_MODEL`
- `MEMORIA_EMBEDDING_API_KEY`
- `MEMORIA_EMBEDDING_BASE_URL`
- `MEMORIA_EMBEDDING_DIM`
- `MEMORIA_INSTANCE_ID`
- `MEMORIA_LOCK_TTL_SECS`
- `MEMORIA_GOVERNANCE_ENABLED`
- `MEMORIA_GOVERNANCE_PLUGIN_BINDING`
- `MEMORIA_GOVERNANCE_PLUGIN_SUBJECT`
- `MEMORIA_GOVERNANCE_PLUGIN_DIR`
- `MEMORIA_API_KEY_SECRET`
- `LLM_API_KEY`
- `LLM_BASE_URL`
- `LLM_MODEL`

## External MatrixOne

Run only the API container and point it at an existing MatrixOne instance:

```bash
MEMORIA_DB_HOST=your-host MEMORIA_DB_PORT=6001 docker compose up -d api
```

## Multi-Instance

Memoria coordinates through MatrixOne. No extra lock service is required.

- Leader election uses DB-backed distributed locks.
- Async tasks are stored in shared tables.
- Crashed instances are recovered by lock expiry.
- Set `MEMORIA_INSTANCE_ID` to the pod name in Kubernetes.

## Probes

- Liveness: `/health`
- Readiness: `/health/instance`

## Security Notes

- API keys are hashed at rest.
- Set `MEMORIA_API_KEY_SECRET` separately from the master key.
- Put Memoria behind TLS in production.
- Choose and fix embedding dimension before first startup.
