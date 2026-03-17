# Local Embedding

Memoria can run embedding entirely on your machine using [fastembed-rs](https://github.com/Anush008/fastembed-rs) (ONNX Runtime). No API key, no network, no data leaves your machine.

## Build from source

Pre-built binaries do NOT include local embedding. You must build from source:

```bash
# Clone and build
git clone https://github.com/matrixorigin/Memoria.git
cd Memoria
make build-local

# Install
sudo cp memoria/target/release/memoria /usr/local/bin/
```

⚠️ The resulting binary is significantly larger (~50-80MB vs ~15MB without) because it bundles the ONNX Runtime. This is expected.

## Configure

```bash
cd your-project
memoria init --tool kiro
# No --embedding-* flags needed — local is the default
```

The generated `mcp.json` will have empty `EMBEDDING_*` env vars. Leave them empty to use local embedding.

## How it works

1. First query triggers model download to `~/.cache/fastembed/` (~30MB for the default model)
2. Model loads into memory via ONNX Runtime (~3-5s on first query)
3. Subsequent queries are fast (in-process, no network)

## Supported models

| Model | Dimension | Size | Notes |
|-------|-----------|------|-------|
| `all-MiniLM-L6-v2` | 384 | ~30MB | **Default**. Fast, good for English |
| `all-MiniLM-L12-v2` | 384 | ~50MB | Slightly better quality |
| `BAAI/bge-small-en-v1.5` | 384 | ~50MB | |
| `BAAI/bge-base-en-v1.5` | 384 | ~130MB | |
| `BAAI/bge-large-en-v1.5` | 1024 | ~650MB | |
| `BAAI/bge-m3` | 1024 | ~1.2GB | Best quality, multilingual |

To use a different model, edit the `env` block in your `mcp.json`:

```json
"env": {
  "EMBEDDING_MODEL": "BAAI/bge-m3",
  "EMBEDDING_DIM": "1024"
}
```

⚠️ Choose the model BEFORE first startup. The embedding dimension is locked into the database schema on first run. Changing it later requires re-creating the embedding column.

## When to use local vs remote

| | Local | Remote (OpenAI / SiliconFlow) |
|---|---|---|
| Privacy | ✅ Data never leaves machine | ⚠️ Text sent to API |
| Cost | Free | API key required |
| First query | ~3-5s (model load) | Fast |
| Build | From source with `--features local-embedding` | Pre-built binary works |
| Multilingual | Only with bge-m3 (1.2GB) | Most API models support it |
| Offline | ✅ Works without internet (after model download) | ❌ Needs network |

**Recommendation**: Use remote embedding (e.g. SiliconFlow free tier) unless you need offline or strict privacy. It's simpler — no source build required.

## Troubleshooting

### "EMBEDDING_PROVIDER=local but compiled without local-embedding feature"

You're using a pre-built binary. Either:
- Build from source: `make build-local`
- Or use a remote embedding service: add `--embedding-provider openai --embedding-api-key sk-...` to `memoria init`

### Model download fails

The model downloads from Hugging Face on first use. If behind a firewall:
- Set `HF_ENDPOINT` env var to a mirror
- Or manually download the model to `~/.cache/fastembed/`

### High memory usage

Larger models use more RAM. `all-MiniLM-L6-v2` (default) uses ~100MB. `BAAI/bge-m3` uses ~1-2GB. Choose based on your available memory.
