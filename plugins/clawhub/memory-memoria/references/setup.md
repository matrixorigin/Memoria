# Setup

Use this when installing Memoria or wiring it into Kiro, Cursor, Claude Code, Codex, or another MCP client.

## Decision Tree

1. Decide between embedded mode and remote mode.
2. Identify the AI tool that needs MCP config.
3. Confirm MatrixOne access.
4. Choose the embedding provider and dimension before first startup.

## Binary Install

```bash
# Linux x86_64
curl -LO https://github.com/matrixorigin/Memoria/releases/latest/download/memoria-x86_64-unknown-linux-musl.tar.gz
tar xzf memoria-x86_64-unknown-linux-musl.tar.gz
sudo mv memoria /usr/local/bin/

# macOS Apple Silicon
curl -LO https://github.com/matrixorigin/Memoria/releases/latest/download/memoria-aarch64-apple-darwin.tar.gz
tar xzf memoria-aarch64-apple-darwin.tar.gz
sudo mv memoria /usr/local/bin/
```

Build from source when local embedding is required:

```bash
cd Memoria/memoria
cargo build --release -p memoria-cli --features local-embedding
sudo cp target/release/memoria /usr/local/bin/
```

Verify:

```bash
memoria --version
```

## Embedded Mode

Use embedded mode when the user runs their own Memoria and MatrixOne.

```bash
docker compose up -d
cd <project>
memoria init --tool <tool>
```

For OpenAI-compatible embeddings:

```bash
memoria init --tool <tool> \
  --embedding-provider openai \
  --embedding-base-url https://api.openai.com/v1 \
  --embedding-api-key sk-... \
  --embedding-model text-embedding-3-small \
  --embedding-dim 1536
```

## Remote Mode

Use remote mode when the user already has a running Memoria API.

```bash
cd <project>
memoria init --tool <tool> --api-url 'https://host:8100' --token 'sk-...'
```

## Tool Outputs

`memoria init --tool <tool>` writes the right config for:

- Kiro: `.kiro/settings/mcp.json` and `.kiro/steering/memory.md`
- Cursor: `.cursor/mcp.json` and `.cursor/rules/memory.mdc`
- Claude Code: `.mcp.json` and `CLAUDE.md`
- Codex: `~/.codex/config.toml` and `AGENTS.md`

## Verify

```bash
memoria status
```

Restart the AI tool after install, then verify memory tools are visible.

## Important Rules

- If the user has no Docker and no existing DB, use MatrixOne Cloud.
- Local embedding requires a source build with `local-embedding`.
- The embedding dimension is effectively locked in when the schema is first created.
