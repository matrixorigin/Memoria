# Release Guide

## Overview

Releases are semi-automated: you run one `make` command locally, CI handles the rest.

| What | Who | How |
|------|-----|-----|
| Bump version, tag, push | You | `make release VERSION=x.y.z` |
| Build binaries (4 platforms) | CI | `release-rust.yml` |
| Build + push Docker image | CI | `release-docker.yml` |
| Create GitHub Release | CI | `softprops/action-gh-release` |

## Prerequisites

- All changes committed and pushed (the Makefile checks for clean working tree)
- CI tests passing on main
- [git-cliff](https://github.com/orhun/git-cliff) installed (optional, for CHANGELOG generation)

## Release a stable version

```bash
# 1. Make sure everything is committed
git status  # should be clean

# 2. Release
make release VERSION=0.2.0
```

This does:
1. Checks for uncommitted changes (fails if dirty)
2. Updates `version` in `memoria/Cargo.toml`
3. Runs `cargo check` to update `Cargo.lock`
4. Generates `CHANGELOG.md` via git-cliff (skipped if not installed)
5. Commits: `chore(release): v0.2.0`
6. Tags: `v0.2.0`
7. Pushes commit + tag

CI then automatically:
- Builds binaries for `x86_64-linux`, `aarch64-linux`, `x86_64-macos`, `aarch64-macos`
- Creates a GitHub Release with the binaries + SHA256SUMS
- Builds and pushes Docker image: `matrixorigin/memoria:0.2.0` + `matrixorigin/memoria:latest`

## Release a pre-release (RC / beta)

```bash
make release-rc VERSION=0.2.0-rc1
```

Same as stable, except:
- No CHANGELOG generation
- GitHub Release is marked as **prerelease**
- Docker image gets `matrixorigin/memoria:0.2.0-rc1` but NOT `latest`

Typical flow:
```bash
make release-rc VERSION=0.2.0-rc1   # test
make release-rc VERSION=0.2.0-rc2   # fix issues, test again
make release VERSION=0.2.0          # promote to stable
```

## Push Docker image manually

If you need to push a Docker image without going through CI:

```bash
make release-docker                  # pushes :latest
make release-docker VERSION=0.2.0   # pushes :0.2.0 + :latest
```

## Update steering rules only

After a release, users can update their steering rules without re-running `init`:

```bash
memoria update-rules
```

## Version locations

The version string lives in:
- `memoria/Cargo.toml` — source of truth (updated by `make release`)
- Binary: `memoria --version`
- MCP config: `_version` field in generated `mcp.json`
- Steering rules: `<!-- memoria-version: x.y.z -->` comment

## CI workflows

| Workflow | Trigger | What it does |
|----------|---------|--------------|
| `test.yml` | Push to main/develop, PRs | Check, clippy, unit tests, DB tests |
| `pr-title.yml` | PR open/edit | Validates [Conventional Commits](https://www.conventionalcommits.org) format |
| `release-rust.yml` | Tag `v*.*.*` | **Test** → build binaries → create GitHub Release |
| `release-docker.yml` | Tag `v*.*.*` | **Test** → build + push Docker image to Docker Hub |

Both release workflows run the full test suite (check, clippy, unit tests, DB tests) before building. If tests fail, no artifacts are published.

## Checklist

Before releasing:

- [ ] All changes committed and pushed
- [ ] CI passing on main (`test.yml` green)
- [ ] README / docs updated if there are user-facing changes
- [ ] Steering rules updated if tool behavior changed (bundled in binary via `include_str!`)
- [ ] `make release VERSION=x.y.z` (or `release-rc` for pre-release)
- [ ] Verify GitHub Release page has all 4 binaries + checksums
- [ ] Verify Docker Hub has the new image tag
