---
name: skills-index
description: Index of all Memoria skills. Use to find the right skill for a task.
---

# Memoria Skills

| Skill | Description |
|-------|-------------|
| [Architecture](skills/architecture/SKILL.md) | Workspace layout, traits, tables, config, testing |
| [API Reference](skills/api-reference/SKILL.md) | REST endpoints, request/response formats, rate limits |
| [Deployment](skills/deployment/SKILL.md) | Docker Compose, K8s, env vars, multi-instance, security |
| [Plugin Development](skills/plugin-development/SKILL.md) | Scaffold, test, sign, publish governance plugins |
| [Setup](skills/setup/SKILL.md) | Install Memoria, configure MCP for AI tools |
| [Release](skills/release/SKILL.md) | Version bump, CI workflows, publish |
| [Local Embedding](skills/local-embedding/SKILL.md) | Build from source, model selection, offline mode |

## Quick Commands

```bash
make check          # cargo check + clippy -D warnings (MUST pass)
make test-unit      # Unit tests (no DB)
make test           # All tests (needs MatrixOne)
make test-e2e       # API e2e tests only
```
