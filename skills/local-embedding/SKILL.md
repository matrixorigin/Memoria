---
name: local-embedding
description: Run embedding on-device with ONNX Runtime. Build from source, model selection, offline mode. Use when setting up local embedding without an API key.
---

## Build from Source

Pre-built binaries do NOT include local embedding.

```bash
cd Memoria
make build-local
sudo cp memoria/target/release/memoria /usr/local/bin/
```

Binary is ~50-80MB (bundles ONNX Runtime). Expected.

## Configure

```bash
memoria init --tool kiro    # No --embedding-* flags needed
```

Leave `EMBEDDING_*` env vars empty in `mcp.json` → local embedding is the default.

## How It Works

1. First query → model downloads to `~/.cache/fastembed/` (~30MB default)
2. Model loads via ONNX Runtime (~3-5s)
3. Subsequent queries are fast (in-process)

## Models

| Model | Dim | Size | Notes |
|-------|-----|------|-------|
| `all-MiniLM-L6-v2` | 384 | ~30MB | **Default**. Fast, English |
| `BAAI/bge-m3` | 1024 | ~1.2GB | Best quality, multilingual |

Change model in `mcp.json` env block:
```json
{ "EMBEDDING_MODEL": "BAAI/bge-m3", "EMBEDDING_DIM": "1024" }
```

⚠️ Choose BEFORE first startup. Dimension is locked into schema.

## When to Use

| | Local | Remote (OpenAI/SiliconFlow) |
|---|---|---|
| Privacy | ✅ Offline | ⚠️ Text sent to API |
| Cost | Free | API key |
| First query | ~3-5s | Fast |
| Build | From source | Pre-built works |
| Offline | ✅ | ❌ |

**Recommendation**: Use remote unless you need offline/strict privacy.

## Troubleshooting

| Problem | Fix |
|---------|-----|
| "compiled without local-embedding" | Build from source: `make build-local` |
| Model download fails | Set `HF_ENDPOINT` for mirror, or manually download to `~/.cache/fastembed/` |
| High memory | Default ~100MB. `bge-m3` ~1-2GB. Choose based on available RAM |
