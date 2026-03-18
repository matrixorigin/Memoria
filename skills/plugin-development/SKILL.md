---
name: plugin-development
description: Create, test, sign, and publish Memoria governance plugins. Covers Rhai and gRPC runtimes, manifest format, lifecycle. Use when developing or managing plugins.
---

## Quick Start

```bash
memoria plugin init --dir ./my-plugin --name my-governance \
  --capabilities governance.plan,governance.execute
```

Creates:
```
my-plugin/
├── manifest.json    # Plugin metadata, permissions, limits
└── policy.rhai      # Governance logic (Rhai script)
```

## manifest.json

```json
{
  "name": "memoria-my-governance",
  "version": "0.1.0",
  "api_version": "v1",
  "runtime": "rhai",
  "entry": { "rhai": { "script": "policy.rhai", "entrypoint": "memoria_plugin" } },
  "capabilities": ["governance.plan", "governance.execute"],
  "compatibility": { "memoria": ">=0.1.0" },
  "permissions": { "network": false, "filesystem": false, "env": [] },
  "limits": { "timeout_ms": 500, "max_memory_mb": 32, "max_output_bytes": 8192 },
  "integrity": { "sha256": "", "signature": "", "signer": "" },
  "metadata": { "display_name": "My Governance Plugin" }
}
```

Key fields:
- `runtime`: `rhai` (sandboxed) or `grpc` (remote service)
- `capabilities`: must include `governance.plan` and/or `governance.execute`
- `integrity`: auto-filled by `memoria plugin publish`

## policy.rhai

```rhai
fn memoria_plugin(ctx) {
    if ctx["phase"] == "plan" {
        let review = decision("my-plugin:check", "Description", 0.8);
        review["evidence"] = [evidence("source", "What was found")];
        return #{
            requires_approval: false,
            actions: [review],
            estimated_impact: #{ "my.metric": 1.0 }
        };
    }
    if ctx["phase"] == "execute" {
        return #{ warnings: [], metrics: #{ "my.metric": 1.0 } };
    }
    return #{};
}
```

Built-in helpers: `decision(id, reason, confidence)`, `evidence(source, description)`

## Runtime Types

| Runtime | Sandboxing | Use Case |
|---------|-----------|----------|
| `rhai` | In-process, memory/time limited | Simple rules, no external deps |
| `grpc` | Out-of-process, network call | Complex logic, external services |

## Development Workflow

### 1. Local dev (no signature, no DB)

```bash
MEMORIA_GOVERNANCE_ENABLED=true \
MEMORIA_GOVERNANCE_PLUGIN_DIR=./my-plugin \
memoria serve
```

### 2. Contract testing

See `memoria-service/tests/plugin_contract.rs` — uses `GovernancePluginContractHarness`.

### 3. Sign and publish

```bash
memoria plugin dev-keygen --dir ./my-plugin
memoria plugin signer-add --signer my-team --public-key <base64-ed25519>
memoria plugin publish --package-dir ./my-plugin
memoria plugin review --key governance:my-governance:v1 --version 0.1.0 --status active
memoria plugin activate --domain governance --binding default \
  --plugin-key governance:my-governance:v1 --version 0.1.0
```

### 4. Production

```bash
MEMORIA_GOVERNANCE_ENABLED=true
MEMORIA_GOVERNANCE_PLUGIN_BINDING=default
MEMORIA_GOVERNANCE_PLUGIN_SUBJECT=system
```

## Lifecycle

```
scaffold → local dev → sign → publish → review → activate → scheduler loads
```

## CLI Reference

| Command | Description |
|---------|-------------|
| `memoria plugin init --dir <d> --name <n>` | Scaffold |
| `memoria plugin dev-keygen --dir <d>` | Generate ed25519 keypair |
| `memoria plugin publish --package-dir <d>` | Sign and publish |
| `memoria plugin list` | List published |
| `memoria plugin review --key <k> --version <v> --status <s>` | Review |
| `memoria plugin activate --domain <d> --binding <b> --plugin-key <k> --version <v>` | Activate |
| `memoria plugin rules` | List binding rules |
| `memoria plugin score --key <k> --version <v>` | Compatibility score |
| `memoria plugin matrix` | Compatibility matrix |
| `memoria plugin events` | Audit events |

## Code Reference

| File | Purpose |
|------|---------|
| `plugin/manifest.rs` | PluginManifest, PluginPackage, signing |
| `plugin/repository.rs` | Publish, review, score, binding CRUD |
| `plugin/rhai_runtime.rs` | RhaiGovernanceStrategy |
| `plugin/grpc_runtime.rs` | GrpcGovernanceStrategy |
| `plugin/governance_hook.rs` | Contract testing harness |
| `plugin/templates/` | Rhai template |
