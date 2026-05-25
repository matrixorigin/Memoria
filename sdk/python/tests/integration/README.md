# Integration Tests

These tests run against a real Memoria API instance.

## Prerequisites

1. Start Memoria with `make up` (from the repo root).
2. Ensure `MEMORIA_MASTER_KEY` is set in your `.env` (it is auto-exported by the Makefile).
3. Install dev dependencies: `pip install -e ".[dev]"` from `sdk/python/`.

## Running

```bash
# From repo root
make python-sdk-test

# Or directly from sdk/python/
MEMORIA_BASE_URL=http://localhost:8100 \
MEMORIA_MASTER_KEY=your-master-key \
python -m pytest tests/integration/ -v
```

## Notes on Embedding

If the Memoria instance is started without a valid `EMBEDDING_API_KEY`, vector retrieval
automatically falls back to full-text search — no errors, but semantic ranking is not exercised.

Integration tests therefore only assert that:
- the correct number of items is returned (≥ 1)
- items contain the stored content (substring match)
- no exception is raised

They do NOT assert on the order or exact ranking of results, because that depends on embeddings.
