# OpenClaw Setup

Use this when Memoria needs to be installed, enabled, or verified as OpenClaw's memory plugin.

## Cloud Install (Recommended)

```bash
openclaw plugins install @matrixorigin/thememoria
openclaw memoria setup --mode cloud --api-url <MEMORIA_API_URL> --api-key <API_KEY>
openclaw memoria health
```

No binary download needed. Setup also enables the plugin automatically.

If the npm package is unavailable, install from source:

```bash
mkdir -p ~/.openclaw/plugins-src
git clone https://github.com/matrixorigin/Memoria.git ~/.openclaw/plugins-src/Memoria
openclaw plugins install --link ~/.openclaw/plugins-src/Memoria/plugins/openclaw
openclaw memoria setup --mode cloud --api-url <MEMORIA_API_URL> --api-key <API_KEY>
```

## Local Install (Embedded)

```bash
curl -sSL https://raw.githubusercontent.com/matrixorigin/Memoria/main/scripts/install.sh | bash -s -- -y -d ~/.local/bin
openclaw plugins install @matrixorigin/thememoria
openclaw memoria setup --mode local --install-memoria --embedding-api-key <EMBEDDING_API_KEY>
openclaw memoria health
```

## Backend Modes

| Mode | Transport | Binary needed | Use case |
|------|-----------|---------------|----------|
| `embedded` | Rust CLI → local MatrixOne | Yes | Self-hosted / local dev |
| `api` | Direct HTTP → Memoria REST API | No | Cloud OpenClaw |

## Verify

```bash
openclaw plugins list              # thememoria is listed and enabled
openclaw memoria health            # status: ok
openclaw memoria capabilities      # config check (no live backend needed)
```

## Important Notes

- OpenClaw reserves `openclaw memory` for built-in file memory. This plugin uses `openclaw memoria` and `openclaw ltm`.
- The plugin defaults to explicit memory writes rather than silent auto-capture.
- `openclaw memoria setup` is the recommended onboarding command. `openclaw memoria connect` is the lower-level config-only variant.
