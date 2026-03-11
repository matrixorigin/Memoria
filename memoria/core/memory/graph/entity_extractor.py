"""Entity extraction — lightweight (regex) and LLM-based.

Lightweight extraction runs automatically on every ingest().
LLM extraction is manual-only (triggered by user via API/MCP).
"""

from __future__ import annotations

import json
import logging
import re
from dataclasses import dataclass, field
from typing import Any

logger = logging.getLogger(__name__)

# ── Lightweight patterns ──────────────────────────────────────────────

# Known tech terms (lowercase) — intentionally hardcoded for zero-latency extraction.
# This is a best-effort heuristic, not exhaustive. LLM extraction (manual trigger)
# covers terms not in this list. Extend as needed for your domain.
_TECH_TERMS: set[str] = {
    "python", "rust", "go", "java", "typescript", "javascript", "ruby", "c++",
    "react", "vue", "angular", "svelte", "nextjs", "fastapi", "django", "flask",
    "docker", "kubernetes", "k8s", "terraform", "ansible",
    "postgresql", "postgres", "mysql", "redis", "mongodb", "sqlite", "matrixone",
    "elasticsearch", "kafka", "rabbitmq", "nginx", "grafana", "prometheus",
    "aws", "gcp", "azure", "s3", "ec2", "lambda", "ecs", "eks",
    "git", "github", "gitlab", "bitbucket",
    "linux", "macos", "windows", "ubuntu", "debian", "centos",
    "openai", "anthropic", "claude", "gpt", "llama", "deepseek",
    "pytest", "jest", "mocha", "ruff", "black", "mypy", "eslint",
    "sqlalchemy", "pydantic", "numpy", "pandas", "scipy", "pytorch", "tensorflow",
}

# Pattern: @mention or owner/repo
_MENTION_RE = re.compile(r"@([\w.-]+)")
_REPO_RE = re.compile(r"\b([\w.-]+/[\w.-]+)\b")

# Pattern: CamelCase identifiers (likely class/project names)
_CAMEL_RE = re.compile(r"\b([A-Z][a-z]+(?:[A-Z][a-z]+)+)\b")


@dataclass
class ExtractedEntity:
    """A named entity extracted from text."""
    name: str           # canonical lowercase name
    display_name: str   # original casing
    entity_type: str    # "tech", "person", "repo", "project", "concept"


def extract_entities_lightweight(text: str) -> list[ExtractedEntity]:
    """Fast regex-based entity extraction. No LLM, no network calls."""
    seen: set[str] = set()
    entities: list[ExtractedEntity] = []

    def _add(name: str, display: str, etype: str) -> None:
        key = name.lower()
        if key not in seen and len(key) >= 2:
            seen.add(key)
            entities.append(ExtractedEntity(key, display, etype))

    # 1. Known tech terms
    words = set(re.findall(r"\b[\w+#.-]+\b", text.lower()))
    for w in words:
        if w in _TECH_TERMS:
            _add(w, w, "tech")

    # 2. @mentions
    for m in _MENTION_RE.finditer(text):
        _add(m.group(1), m.group(1), "person")

    # 3. owner/repo patterns
    for m in _REPO_RE.finditer(text):
        _add(m.group(1), m.group(1), "repo")

    # 4. CamelCase identifiers (likely project/class names)
    for m in _CAMEL_RE.finditer(text):
        name = m.group(1)
        if name.lower() not in seen and name.lower() not in _TECH_TERMS:
            _add(name.lower(), name, "project")

    return entities


# ── LLM extraction ────────────────────────────────────────────────────

_LLM_EXTRACT_PROMPT = """\
Extract named entities from the following text. Return a JSON array of objects.
Each object: {"name": "canonical name", "type": "tech|person|repo|project|concept"}

Rules:
- Only extract specific, named entities (not generic words)
- Normalize names: lowercase for tech, original case for people/projects
- Deduplicate: if the same entity appears multiple times, include it once
- Max 10 entities per text

Text:
{text}

JSON array:"""


@dataclass
class LLMEntityExtractionResult:
    """Result of LLM entity extraction for a batch of memories."""
    total_memories: int = 0
    entities_found: int = 0
    edges_created: int = 0
    errors: list[str] = field(default_factory=list)


def extract_entities_llm(
    text: str, llm_client: Any,
) -> list[ExtractedEntity]:
    """LLM-based entity extraction. More accurate but slower."""
    try:
        response = llm_client.chat(
            messages=[{"role": "user", "content": _LLM_EXTRACT_PROMPT.format(text=text[:2000])}],
            temperature=0.0,
            max_tokens=300,
        )
        raw = response if isinstance(response, str) else getattr(response, "content", str(response))
        # Extract JSON array from LLM response — tolerates markdown fences and preamble text.
        # Falls back to empty list on any parse failure (best-effort, not critical path).
        start = raw.find("[")
        end = raw.rfind("]")
        if start == -1 or end == -1:
            return []
        try:
            items = json.loads(raw[start:end + 1])
        except json.JSONDecodeError:
            logger.debug("LLM entity extraction returned invalid JSON: %s", raw[:200])
            return []
        if not isinstance(items, list):
            return []
        entities: list[ExtractedEntity] = []
        seen: set[str] = set()
        for item in items[:10]:
            name = str(item.get("name", "")).strip()
            if not name or name.lower() in seen:
                continue
            seen.add(name.lower())
            entities.append(ExtractedEntity(
                name=name.lower(),
                display_name=name,
                entity_type=item.get("type", "concept"),
            ))
        return entities
    except Exception:
        logger.warning("LLM entity extraction failed", exc_info=True)
        return []
