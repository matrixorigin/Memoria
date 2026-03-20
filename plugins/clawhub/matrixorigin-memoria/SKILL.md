---
name: matrixorigin-memoria
description: Durable memory skill pack for Memoria. Covers setup, memory operations, snapshots and branches, REST and MCP usage, deployment, OpenClaw integration, plugin development, local embedding, release flow, and codebase navigation.
version: 0.1.0
metadata:
  openclaw:
    requires:
      anyBins:
        - memoria
        - docker
        - cargo
        - openclaw
    emoji: "🧠"
    homepage: https://github.com/matrixorigin/Memoria
---

# MatrixOrigin Memoria

Use this skill when the task involves Memoria as a product, backend, plugin, or codebase.

## Routing

Pick the smallest reference that matches the task:

- Setup or MCP onboarding: `references/setup.md`
- Daily memory usage, repair, recovery, branches: `references/operations.md`
- REST API or MCP tool behavior: `references/api.md`
- Deploying or configuring service instances: `references/deployment.md`
- OpenClaw plugin install or verification: `references/openclaw.md`
- Governance plugin authoring: `references/plugin-development.md`
- Local embedding without an API key: `references/local-embedding.md`
- Releasing Memoria: `references/release.md`
- Navigating or modifying the Rust workspace: `references/architecture.md`

## Core Rules

1. Choose the embedding provider and dimension before first startup. Schema shape depends on it.
2. Before bulk delete, purge, or rollback work, create a snapshot first.
3. For durable user facts or preferences, prefer Memoria over ad hoc note files unless the user explicitly wants file-based notes.
4. For code changes in this repo, `make check` is the minimum validation target.
5. For OpenClaw tasks, use the native plugin flow instead of inventing a custom bridge.

## Quick Decisions

- Need memory setup for an AI tool: read `references/setup.md`
- Need to store, recall, correct, or delete memory: read `references/operations.md`
- Need an endpoint or request shape: read `references/api.md`
- Need cluster or Docker deployment guidance: read `references/deployment.md`
- Need OpenClaw install commands: read `references/openclaw.md`
- Need offline embedding: read `references/local-embedding.md`
- Need to build a governance plugin: read `references/plugin-development.md`
- Need to modify Memoria internals: read `references/architecture.md`

## Validation

Common repo checks:

```bash
make check
make test-unit
make test-e2e
```
