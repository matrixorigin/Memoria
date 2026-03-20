# Local Embedding

Use this when the user wants offline embedding or does not want to send text to an external API.

## Build

Prebuilt binaries do not include local embedding.

```bash
cd Memoria
make build-local
sudo cp memoria/target/release/memoria /usr/local/bin/
```

## Configure

```bash
memoria init --tool <tool>
```

Leave embedding env vars empty so local embedding remains the default.

## Model Notes

- `all-MiniLM-L6-v2`: 384 dimensions, small, fast, default
- `BAAI/bge-m3`: 1024 dimensions, much larger, multilingual, higher quality

Choose the model before first startup because dimension affects schema creation.

## Tradeoffs

- Local: better privacy, slower first query, source build required
- Remote: easier install, faster first query, needs API access

## Troubleshooting

- "compiled without local-embedding": rebuild with the feature enabled
- model download failure: use a mirror or pre-populate `~/.cache/fastembed/`
- memory pressure: prefer the default smaller model
