"""memoria CLI — configure AI tools to use Memoria memory service.

Usage:
    memoria init       # Detect tools, write MCP config + steering rules
    memoria status     # Show configuration status
    memoria update-rules  # Update steering rules to latest version
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path
from typing import Any

_VERSION = "0.1.4"
_MCP_KEY = "memoria"


# ── Templates ─────────────────────────────────────────────────────────


def _templates_dir() -> Path:
    base = Path(getattr(sys, "_MEIPASS", Path(__file__).parent))
    return base / "templates"


def _load_template(name: str) -> str:
    path = _templates_dir() / name
    if not path.exists():
        raise FileNotFoundError(f"Template not found: {path}")
    return path.read_text()


def _kiro_steering() -> str:
    return _load_template("kiro_steering.md")


def _cursor_rule() -> str:
    return _load_template("cursor_rule.md")


def _claude_rule() -> str:
    return _load_template("claude_rule.md")


# ── MCP config ────────────────────────────────────────────────────────


def _mcp_entry(
    db_url: str | None, api_url: str | None, token: str | None, user: str, **embed: str
) -> dict[str, Any]:
    import shutil

    # Use absolute path so Kiro/Cursor can find the command regardless of PATH
    cmd = shutil.which("memoria-mcp") or "memoria-mcp"
    if api_url:
        args = ["--api-url", api_url]
        if token:
            args += ["--token", token]
        if user != "default":
            args += ["--user", user]
        return {"command": cmd, "args": args}

    args = ["--db-url", db_url or "mysql+pymysql://root:111@localhost:6001/memoria"]
    if user != "default":
        args += ["--user", user]
    # Always emit all env vars so users can see what's configurable.
    # Empty string = "not set" — MCP server uses defaults (local embedding, dim=384).
    env: dict[str, str] = {
        "EMBEDDING_PROVIDER": embed.get("EMBEDDING_PROVIDER", ""),
        "EMBEDDING_MODEL": embed.get("EMBEDDING_MODEL", ""),
        "EMBEDDING_DIM": embed.get("EMBEDDING_DIM", ""),
        "EMBEDDING_API_KEY": embed.get("EMBEDDING_API_KEY", ""),
        "EMBEDDING_BASE_URL": embed.get("EMBEDDING_BASE_URL", ""),
        "HF_HUB_OFFLINE": "1",
        "TRANSFORMERS_OFFLINE": "1",
        "_HF_ENDPOINT": "https://hf-mirror.com",
        "_comment_HF_ENDPOINT": "Uncomment and rename to HF_ENDPOINT to use HuggingFace mirror (remove leading underscore)",
    }
    return {"command": cmd, "args": args, "env": env}


# ── Detection ─────────────────────────────────────────────────────────


def _detect(project_dir: Path) -> list[str]:
    found = []
    if (project_dir / ".kiro").is_dir():
        found.append("kiro")
    if (project_dir / ".cursor").is_dir() or (project_dir / ".cursorrc").exists():
        found.append("cursor")
    if (project_dir / "CLAUDE.md").exists() or (project_dir / ".claude").is_dir():
        found.append("claude")
    return found


# ── Write helpers ─────────────────────────────────────────────────────


def _installed_version(path: Path) -> str | None:
    if not path.exists():
        return None
    m = re.search(r"memoria-version:\s*([\d.]+)", path.read_text()[:500])
    return m.group(1) if m else None


def _write_rule(path: Path, content: str, force: bool, project_dir: Path) -> str:
    rel = path.relative_to(project_dir)
    if not path.exists():
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content)
        return f"  ✅ {rel} (created)"
    installed = _installed_version(path)
    new_ver_m = re.search(r"memoria-version:\s*([\d.]+)", content[:500])
    new_ver = new_ver_m.group(1) if new_ver_m else None
    if installed == new_ver:
        return f"  ⏭️  {rel} (up to date)"
    if not force and installed and installed != new_ver:
        bak = path.with_suffix(path.suffix + ".bak")
        bak.write_text(path.read_text())
    path.write_text(content)
    return f"  ✅ {rel} (updated {installed} → {new_ver})"


def _write_mcp_json(path: Path, entry: dict, project_dir: Path) -> str:
    config = json.loads(path.read_text()) if path.exists() else {"mcpServers": {}}
    config.setdefault("mcpServers", {})[_MCP_KEY] = entry
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(config, indent=2) + "\n")
    return f"  ✅ {path.relative_to(project_dir)}"


def _configure_kiro(project_dir: Path, entry: dict, force: bool) -> list[str]:
    actions = [
        _write_mcp_json(project_dir / ".kiro/settings/mcp.json", entry, project_dir)
    ]
    actions.append(
        _write_rule(
            project_dir / ".kiro/steering/memory.md",
            _kiro_steering(),
            force,
            project_dir,
        )
    )
    return actions


def _configure_cursor(project_dir: Path, entry: dict, force: bool) -> list[str]:
    actions = [_write_mcp_json(project_dir / ".cursor/mcp.json", entry, project_dir)]
    actions.append(
        _write_rule(
            project_dir / ".cursor/rules/memory.mdc", _cursor_rule(), force, project_dir
        )
    )
    return actions


def _configure_claude(project_dir: Path, entry: dict, force: bool) -> list[str]:
    actions = [_write_mcp_json(project_dir / ".claude/mcp.json", entry, project_dir)]
    claude_md = project_dir / "CLAUDE.md"
    rule = _claude_rule()
    if claude_md.exists():
        existing = claude_md.read_text()
        if "memoria-version:" in existing:
            actions.append("  ⏭️  CLAUDE.md (already configured)")
        else:
            claude_md.write_text(existing.rstrip() + "\n\n" + rule)
            actions.append("  ✅ CLAUDE.md (appended)")
    else:
        claude_md.write_text(rule)
        actions.append("  ✅ CLAUDE.md (created)")
    return actions


# ── Commands ──────────────────────────────────────────────────────────


def cmd_init(args: argparse.Namespace) -> None:
    project_dir = Path(args.dir).resolve()
    tools = args.tool or _detect(project_dir)
    if not tools:
        print("No AI tools detected. Use --tool kiro|cursor|claude to specify.")
        return

    embed_env: dict[str, str] = {}
    if args.embedding_provider:
        embed_env["EMBEDDING_PROVIDER"] = args.embedding_provider
    if args.embedding_model:
        embed_env["EMBEDDING_MODEL"] = args.embedding_model
    if args.embedding_dim:
        embed_env["EMBEDDING_DIM"] = args.embedding_dim
    if args.embedding_api_key:
        embed_env["EMBEDDING_API_KEY"] = args.embedding_api_key
    if args.embedding_base_url:
        embed_env["EMBEDDING_BASE_URL"] = args.embedding_base_url

    entry = _mcp_entry(args.db_url, args.api_url, args.token, args.user, **embed_env)

    writers = {
        "kiro": _configure_kiro,
        "cursor": _configure_cursor,
        "claude": _configure_claude,
    }
    for tool in tools:
        print(f"{tool}:")
        for line in writers[tool](project_dir, entry, args.force):
            print(line)
    print("\nDone! Restart your AI tool to load the MCP server.")


def cmd_status(args: argparse.Namespace) -> None:
    project_dir = Path(args.dir).resolve()
    rule_paths = {
        "kiro": project_dir / ".kiro/steering/memory.md",
        "cursor": project_dir / ".cursor/rules/memory.mdc",
        "claude": project_dir / "CLAUDE.md",
    }
    mcp_paths = {
        "kiro": project_dir / ".kiro/settings/mcp.json",
        "cursor": project_dir / ".cursor/mcp.json",
        "claude": project_dir / ".claude/mcp.json",
    }
    for tool in ("kiro", "cursor", "claude"):
        mcp = mcp_paths[tool]
        if not mcp.exists():
            continue
        cfg = json.loads(mcp.read_text())
        has_mcp = _MCP_KEY in cfg.get("mcpServers", {})
        ver = _installed_version(rule_paths[tool])
        mcp_status = "✅" if has_mcp else "❌ not configured"
        rule_status = (
            f"✅ v{ver}"
            if ver == _VERSION
            else (f"⚠️  outdated ({ver})" if ver else "❌ missing")
        )
        print(f"  {tool}: mcp={mcp_status}  rules={rule_status}")


def cmd_update_rules(args: argparse.Namespace) -> None:
    project_dir = Path(args.dir).resolve()
    rules = {
        "kiro": (project_dir / ".kiro/steering/memory.md", _kiro_steering),
        "cursor": (project_dir / ".cursor/rules/memory.mdc", _cursor_rule),
        "claude": (project_dir / "CLAUDE.md", _claude_rule),
    }
    updated = 0
    for tool, (path, loader) in rules.items():
        if path.exists():
            path.write_text(loader())
            print(f"  ✅ {tool}: rules updated to v{_VERSION}")
            updated += 1
    if not updated:
        print("No rule files found. Run 'memoria init' first.")


def main() -> None:
    parser = argparse.ArgumentParser(
        prog="memoria",
        description=f"Memoria v{_VERSION} — configure AI tools for persistent memory",
    )
    parser.add_argument(
        "--dir", default=".", help="Project directory (default: current)"
    )
    sub = parser.add_subparsers(dest="command")

    p = sub.add_parser("init", help="Write MCP config + steering rules")
    p.add_argument(
        "--tool",
        choices=["kiro", "cursor", "claude"],
        action="append",
        help="Target tool (repeatable; default: auto-detect)",
    )
    p.add_argument("--db-url", help="Database URL for embedded mode")
    p.add_argument("--api-url", help="Memoria REST API URL for remote mode")
    p.add_argument("--token", help="API token for remote mode")
    p.add_argument("--user", default="default", help="Default user ID")
    p.add_argument(
        "--force", action="store_true", help="Overwrite customized rule files"
    )
    p.add_argument("--embedding-provider", help="Embedding provider (openai, local)")
    p.add_argument("--embedding-model", help="Embedding model name")
    p.add_argument("--embedding-dim", help="Embedding dimension")
    p.add_argument("--embedding-api-key", help="Embedding API key")
    p.add_argument("--embedding-base-url", help="Embedding API base URL")

    sub.add_parser("status", help="Show MCP config and rule version status")
    sub.add_parser("update-rules", help="Update steering rules to latest version")

    args = parser.parse_args()
    dispatch = {
        "init": cmd_init,
        "status": cmd_status,
        "update-rules": cmd_update_rules,
    }
    fn = dispatch.get(args.command)
    if fn:
        fn(args)
    else:
        parser.print_help()


if __name__ == "__main__":
    main()
