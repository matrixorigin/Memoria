#!/usr/bin/env python3
"""Bump memoria version across all files.

Usage:
    python scripts/bump_version.py patch   # 0.1.0 -> 0.1.1
    python scripts/bump_version.py minor   # 0.1.3 -> 0.2.0
    python scripts/bump_version.py major   # 0.1.3 -> 1.0.0
"""

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

FILES = [
    ("pyproject.toml", r'(version\s*=\s*")([^"]+)(")'),
    ("memoria/cli.py", r'(_VERSION\s*=\s*")([^"]+)(")'),
    (
        "memoria/templates/kiro_steering.md",
        r'(<!-- memoria-version:\s*)([^"]+)(\s*-->)',
    ),
    ("memoria/templates/claude_rule.md", r'(<!-- memoria-version:\s*)([^"]+)(\s*-->)'),
    ("memoria/templates/cursor_rule.md", r'(<!-- memoria-version:\s*)([^"]+)(\s*-->)'),
]


def bump(version: str, part: str) -> str:
    major, minor, patch = (int(x) for x in version.split("."))
    if part == "patch":
        patch += 1
    elif part == "minor":
        minor += 1
        patch = 0
    elif part == "major":
        major += 1
        minor = patch = 0
    else:
        raise ValueError(f"Unknown part: {part}")
    return f"{major}.{minor}.{patch}"


def main() -> None:
    part = sys.argv[1] if len(sys.argv) > 1 else "patch"

    # Read current version from canonical source
    cli = (ROOT / "memoria/cli.py").read_text()
    m = re.search(r'_VERSION\s*=\s*"([^"]+)"', cli)
    if not m:
        sys.exit("Cannot find _VERSION in memoria/cli.py")
    old = m.group(1)

    # Support direct version string (e.g. "0.1.0") or bump type (patch/minor/major)
    new = part if re.match(r"^\d+\.\d+\.\d+$", part) else bump(old, part)

    for relpath, pattern in FILES:
        path = ROOT / relpath
        text = path.read_text()
        updated, n = re.subn(pattern, rf"\g<1>{new}\3", text, count=1)
        if n == 0:
            print(f"  ⚠️  no match in {relpath}")
        else:
            path.write_text(updated)
            print(f"  ✅ {relpath}")

    print(f"\n{old} → {new}")


if __name__ == "__main__":
    main()
