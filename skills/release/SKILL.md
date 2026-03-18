---
name: release
description: Cut a Memoria release. Version bump, CHANGELOG, CI workflows, Docker image. Use when publishing a new version.
---

## Flow

| Step | Who | How |
|------|-----|-----|
| Bump version, tag, push | You | `make release VERSION=x.y.z` |
| Build binaries (4 platforms) | CI | `release-rust.yml` |
| Build + push Docker image | CI | `release-docker.yml` |
| Create GitHub Release | CI | `softprops/action-gh-release` |

## Stable Release

```bash
git status              # Must be clean
make release VERSION=0.2.0
```

Does: update `Cargo.toml` version → `cargo check` → generate CHANGELOG (git-cliff) → commit → tag → push.

CI then: test → build binaries (x86_64/aarch64 linux/macos) → GitHub Release → Docker image (`:0.2.0` + `:latest`).

## Pre-Release

```bash
make release-rc VERSION=0.2.0-rc1
```

Same but: no CHANGELOG, GitHub Release marked prerelease, Docker gets `:0.2.0-rc1` (not `:latest`).

Typical: `rc1` → `rc2` → stable.

## Manual Docker Push

```bash
make release-docker                  # :latest
make release-docker VERSION=0.2.0   # :0.2.0 + :latest
```

## CI Workflows

| Workflow | Trigger | What |
|----------|---------|------|
| `test.yml` | Push main/develop, PRs | check + clippy + tests |
| `pr-title.yml` | PR open/edit | Conventional Commits validation |
| `release-rust.yml` | Tag `v*` | Test → build → GitHub Release |
| `release-docker.yml` | Tag `v*` | Test → build → Docker Hub |

Both release workflows run full test suite before publishing.

## Version Locations

- `memoria/Cargo.toml` — source of truth
- `memoria --version` — binary
- `mcp.json` — `_version` field
- Steering rules — `<!-- memoria-version: x.y.z -->`

## Checklist

- [ ] Clean working tree, CI green on main
- [ ] README/docs updated for user-facing changes
- [ ] Steering rules updated if tool behavior changed
- [ ] `make release VERSION=x.y.z`
- [ ] Verify GitHub Release has 4 binaries + checksums
- [ ] Verify Docker Hub has new tag
