# Plugin Development

Use this when building Memoria governance plugins.

## Scaffold

```bash
memoria plugin init --dir ./my-plugin --name my-governance \
  --capabilities governance.plan,governance.execute
```

Expected files:

- `manifest.json`
- `policy.rhai`

## Manifest Basics

Important fields:

- `name`
- `version`
- `api_version`
- `runtime`
- `entry`
- `capabilities`
- `compatibility.memoria`
- `permissions`
- `limits`
- `integrity`

Use `runtime: "rhai"` for sandboxed in-process logic or `runtime: "grpc"` for remote services.

## Development Flow

1. Scaffold the plugin.
2. Run local dev mode with `MEMORIA_GOVERNANCE_PLUGIN_DIR`.
3. Contract test the plugin.
4. Generate a signing key.
5. Publish, review, and activate it.

## Local Dev

```bash
MEMORIA_GOVERNANCE_ENABLED=true \
MEMORIA_GOVERNANCE_PLUGIN_DIR=./my-plugin \
memoria serve
```

## Sign And Publish

```bash
memoria plugin dev-keygen --dir ./my-plugin
memoria plugin signer-add --signer my-team --public-key <base64-ed25519>
memoria plugin publish --package-dir ./my-plugin
memoria plugin review --key governance:my-governance:v1 --version 0.1.0 --status active
memoria plugin activate --domain governance --binding default \
  --plugin-key governance:my-governance:v1 --version 0.1.0
```

## Built-In Helpers

Rhai helpers include:

- `decision(id, reason, confidence)`
- `evidence(source, description)`

## Code Pointers

- `memoria-service/src/plugin/manifest.rs`
- `memoria-service/src/plugin/repository.rs`
- `memoria-service/src/plugin/rhai_runtime.rs`
- `memoria-service/src/plugin/grpc_runtime.rs`
- `memoria-service/src/plugin/governance_hook.rs`
