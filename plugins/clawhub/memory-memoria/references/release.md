# Release

Use this when cutting a Memoria release.

## Stable Release

```bash
git status
make release VERSION=x.y.z
```

This bumps the version, runs checks, generates the changelog, commits, tags, and pushes.

## Pre-Release

```bash
make release-rc VERSION=x.y.z-rc1
```

## CI Expectations

- Rust release workflow builds platform binaries
- Docker workflow publishes versioned and latest images
- Both release workflows run tests before publishing

## Checklist

- Working tree clean
- Docs updated for user-visible changes
- Steering rules updated if tool behavior changed
- Release artifacts verified after CI finishes
