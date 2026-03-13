"""Entity extraction — lightweight (regex) and LLM-based.

Lightweight extraction runs automatically on every ingest().
LLM extraction is manual-only (triggered by user via API/MCP).
"""

from __future__ import annotations

import json
import logging
import re
import unicodedata
from dataclasses import dataclass, field
from typing import Any

logger = logging.getLogger(__name__)

# ── Lightweight patterns ──────────────────────────────────────────────

# Known tech terms (lowercase) — common terms that are ambiguous without a list
# (e.g. "go", "rust", "flask" could be English words). Only terms that NEED
# disambiguation belong here. Unambiguous proper nouns (Spark, Consul, OAuth)
# are caught by the capitalized-word heuristic below.
_TECH_TERMS: set[str] = {
    # Languages whose names are common English words
    "python",
    "rust",
    "go",
    "java",
    "ruby",
    "c++",
    "swift",
    # Frameworks whose names are common English words
    "flask",
    "spring",
    "express",
    "gin",
    "lambda",
    # Tools whose names are common English words
    "terraform",
    "ansible",
    "docker",
    "git",
    "ruff",
    "black",
    "jest",
    "mocha",
    # Cloud/infra abbreviations (not capitalized, won't be caught by heuristic)
    "k8s",
    "aws",
    "gcp",
    "s3",
    "ec2",
    "ecs",
    "eks",
}

# Chinese city names (top-50 by population + common travel/food cities)
_CHINESE_CITIES: set[str] = {
    "北京",
    "上海",
    "广州",
    "深圳",
    "成都",
    "杭州",
    "武汉",
    "西安",
    "南京",
    "重庆",
    "天津",
    "苏州",
    "长沙",
    "郑州",
    "东莞",
    "青岛",
    "沈阳",
    "宁波",
    "昆明",
    "大连",
    "厦门",
    "合肥",
    "佛山",
    "福州",
    "哈尔滨",
    "济南",
    "温州",
    "南宁",
    "长春",
    "泉州",
    "石家庄",
    "贵阳",
    "南昌",
    "金华",
    "常州",
    "无锡",
    "嘉兴",
    "太原",
    "徐州",
    "珠海",
    "兰州",
    "乌鲁木齐",
    "拉萨",
    "海口",
    "三亚",
    "丽江",
    "桂林",
    "洛阳",
    "扬州",
    "香港",
    "澳门",
    "台北",
}

# Chinese time expressions
_CHINESE_TIME_RE = re.compile(
    r"(?:今天|昨天|前天|明天|后天|上周|本周|下周|上个月|这个月|下个月|去年|今年|明年"
    r"|周[一二三四五六日天]|星期[一二三四五六日天]"
    r"|\d{4}年(?:\d{1,2}月)?(?:\d{1,2}[日号])?"
    r"|\d{1,2}月\d{1,2}[日号])"
)

# Pattern: @mention or owner/repo
_MENTION_RE = re.compile(r"@([\w.-]+)")
_REPO_RE = re.compile(r"\b([\w.-]+/[\w.-]+)\b")

# Pattern: CamelCase identifiers (likely class/project names)
_CAMEL_RE = re.compile(r"\b([A-Z][a-z]+(?:[A-Z][a-z]+)+)\b")

# Pattern: quoted strings and backtick terms (project/product names)
_QUOTED_RE = re.compile(r'["\u201c]([^"\u201d]{2,30})["\u201d]')
_BACKTICK_RE = re.compile(r"`([^`]{2,30})`")


def normalize_entity_name(name: str) -> str:
    """Normalize entity name: NFKC, trim, collapse whitespace, lowercase ASCII only."""
    name = unicodedata.normalize("NFKC", name).strip()
    name = re.sub(r"\s+", " ", name)
    # Lowercase ASCII characters only; Chinese/other scripts unchanged
    result = []
    for ch in name:
        if ch.isascii() and ch.isalpha():
            result.append(ch.lower())
        else:
            result.append(ch)
    return "".join(result)


@dataclass
class ExtractedEntity:
    """A named entity extracted from text."""

    name: str  # canonical lowercase name
    display_name: str  # original casing
    entity_type: str  # "tech", "person", "repo", "project", "concept"


# Capitalized English words/phrases in CJK context — almost always proper nouns.
# Matches: "Spark", "OAuth", "Next.js", "Node.js", "Apache Spark", "React Server Components"
# Excludes: common English words that appear capitalized at sentence start.
_COMMON_ENGLISH: set[str] = {
    "the",
    "a",
    "an",
    "is",
    "are",
    "was",
    "were",
    "be",
    "been",
    "being",
    "have",
    "has",
    "had",
    "do",
    "does",
    "did",
    "will",
    "would",
    "could",
    "should",
    "may",
    "might",
    "shall",
    "can",
    "need",
    "must",
    "let",
    "we",
    "i",
    "you",
    "he",
    "she",
    "it",
    "they",
    "my",
    "our",
    "your",
    "this",
    "that",
    "these",
    "those",
    "if",
    "then",
    "else",
    "when",
    "where",
    "how",
    "what",
    "which",
    "who",
    "whom",
    "why",
    "not",
    "no",
    "yes",
    "all",
    "each",
    "every",
    "both",
    "few",
    "more",
    "most",
    "other",
    "some",
    "such",
    "only",
    "same",
    "so",
    "than",
    "too",
    "very",
    "just",
    "but",
    "and",
    "or",
    "nor",
    "for",
    "yet",
    "after",
    "before",
    "since",
    "while",
    "about",
    "above",
    "below",
    "to",
    "from",
    "up",
    "down",
    "in",
    "out",
    "on",
    "off",
    "over",
    "under",
    "again",
    "further",
    "once",
    "here",
    "there",
    "any",
    "new",
    "old",
    "also",
    "back",
    "now",
    "well",
    "way",
    "use",
    "her",
    "him",
    "his",
    "its",
    # Days/months (capitalized in English but not entities)
    "monday",
    "tuesday",
    "wednesday",
    "thursday",
    "friday",
    "saturday",
    "sunday",
    "january",
    "february",
    "march",
    "april",
    "june",
    "july",
    "august",
    "september",
    "october",
    "november",
    "december",
    # Common sentence-start words
    "please",
    "thanks",
    "note",
    "see",
    "check",
    "run",
    "try",
    "make",
    "sure",
    "first",
    "next",
    "last",
    "step",
    "error",
    "warning",
    "info",
    # Single uppercase letters (SDK → S, D, K individually)
    "s",
    "d",
    "k",
    "v",
    "p",
    "r",
    "t",
    "m",
    "n",
    "b",
    "c",
    "f",
    "l",
}

# Capitalized word/phrase: 1-3 words starting with uppercase, may contain dots/hyphens.
# Each word must be 2+ chars to avoid matching single letters like "S" in "Stripe SDK".
_CAPITALIZED_TECH_RE = re.compile(
    r"(?<![A-Za-z])"
    r"([A-Z][a-z]+(?:[.-][A-Za-z][a-z]*)*"  # First word 2+ chars: Next.js, OAuth
    r"(?:\s+[A-Z][a-z]+(?:[.-][A-Za-z][a-z]*)*){0,2})"  # Optional 1-2 more words
    r"(?![a-z])"
)

# Chinese name pattern: common Chinese surnames + 1-2 given name chars.
# Requires a word boundary before the name: start of string, whitespace,
# punctuation, or common Chinese particles (的是和与找问叫给让被把).
_CHINESE_SURNAMES = (
    "赵钱孙李周吴郑王冯陈褚卫蒋沈韩杨朱秦尤许何吕施张孔曹严华金魏陶姜"
    "戚谢邹喻柏水窦章云苏潘葛奚范彭郎鲁韦昌马苗凤花方俞任袁柳酆鲍史唐"
    "费廉岑薛雷贺倪汤滕殷罗毕郝邬安常乐于时傅皮卞齐康伍余元卜顾孟黄"
    "穆萧尹姚邵湛汪祁毛禹狄米贝明臧计伏成戴谈宋茅庞熊纪舒屈项祝董梁"
    "杜阮蓝闵席季麻强贾路娄危江童颜郭梅盛林刁钟徐邱骆高夏蔡田樊胡凌霍"
    "虞万支柯昝管卢莫经房裘缪干解应宗丁宣贲邓郁单杭洪包诸左石崔吉钮龚"
)
# Chinese name: surname + 1 given char.
# 2-char names (张伟): require boundary before (的/是/在/etc or start/space/punct).
# After the name: must NOT be followed by another char that could extend it
# into a common word. We use a simple heuristic: the name is valid if
# followed by a known verb/particle, punctuation, space, or end of string.
_CN_BOUNDARY_BEFORE = r"(?:^|(?<=[\s，。！？；：、@的是和与找问叫给让被把在]))"
# For 2-char names: accept if followed by any common verb/particle/punct
# This list covers ~95% of post-name positions in Chinese text.
_CN_NAME_FOLLOWERS = (
    r"[\s，。！？；：、的是在了过着把被给让叫找问说做看想要会能可有没不也都还就才"
    r"提出上下去来最近负责设计开发维护排查发现正在已经曾经刚刚]"
)
_CHINESE_NAME_2_RE = re.compile(
    _CN_BOUNDARY_BEFORE
    + r"(["
    + _CHINESE_SURNAMES
    + r"][\u4e00-\u9fff])"
    + r"(?="
    + _CN_NAME_FOLLOWERS
    + r"|$)"
)
_CHINESE_NAME_3_RE = re.compile(
    _CN_BOUNDARY_BEFORE
    + r"(["
    + _CHINESE_SURNAMES
    + r"][\u4e00-\u9fff]{2})"
    + r"(?="
    + _CN_NAME_FOLLOWERS
    + r"|$)"
)

# Service/component name pattern: word-word (auth-service, user-service, service-mesh)
_SERVICE_NAME_RE = re.compile(r"\b([a-z][a-z0-9]*(?:-[a-z][a-z0-9]*)+)\b")
# Only treat as entity if it looks like a service/component name
_SERVICE_SUFFIXES = {
    "service",
    "server",
    "api",
    "gateway",
    "proxy",
    "mesh",
    "pipeline",
    "worker",
    "queue",
    "cache",
    "store",
    "db",
    "manager",
    "controller",
    "handler",
    "client",
    "sdk",
    "cli",
    "ui",
    "app",
}


def extract_entities_lightweight(text: str) -> list[ExtractedEntity]:
    """Fast regex-based entity extraction. No LLM, no network calls."""
    seen: set[str] = set()
    entities: list[ExtractedEntity] = []

    def _add(name: str, display: str, etype: str) -> None:
        key = normalize_entity_name(name)
        if key not in seen and len(key) >= 2:
            seen.add(key)
            entities.append(ExtractedEntity(key, display, etype))

    # 1. Known tech terms
    words = set(re.findall(r"\b[\w+#.-]+\b", text.lower()))
    for w in words:
        if w in _TECH_TERMS:
            _add(w, w, "tech")

    # 2. Capitalized English words in text — likely tech proper nouns
    #    (Spark, Consul, Next.js, React Server Components, etc.)
    for m in _CAPITALIZED_TECH_RE.finditer(text):
        name = m.group(1).strip()
        if name.lower() in _COMMON_ENGLISH or name.lower() in _TECH_TERMS:
            continue
        if len(name) < 2:
            continue
        key = normalize_entity_name(name)
        if key not in seen:
            _add(name, name, "tech")

    # 2b. Uppercase acronyms (2-10 chars): OAuth, RSC, DNS, ETL, API, SDK, etc.
    for m in re.finditer(r"(?<![A-Za-z])([A-Z][A-Za-z]*[A-Z][A-Za-z]*)\b", text):
        name = m.group(1)
        if name.lower() in _COMMON_ENGLISH or name.lower() in _TECH_TERMS:
            continue
        if 2 <= len(name) <= 10:
            key = normalize_entity_name(name)
            if key not in seen:
                _add(name, name, "tech")

    # 3. @mentions
    for m in _MENTION_RE.finditer(text):
        _add(m.group(1), m.group(1), "person")

    # 4. Chinese person names (surname + 1-2 chars)
    for m in _CHINESE_NAME_2_RE.finditer(text):
        name = m.group(1)
        if normalize_entity_name(name) not in seen:
            _add(name, name, "person")
    for m in _CHINESE_NAME_3_RE.finditer(text):
        name = m.group(1)
        if normalize_entity_name(name) not in seen:
            _add(name, name, "person")

    # 5. owner/repo patterns
    for m in _REPO_RE.finditer(text):
        _add(m.group(1), m.group(1), "repo")

    # 6. CamelCase identifiers (likely project/class names)
    for m in _CAMEL_RE.finditer(text):
        name = m.group(1)
        if normalize_entity_name(name) not in seen and name.lower() not in _TECH_TERMS:
            _add(name, name, "project")

    # 7. Service/component names (auth-service, service-mesh, etc.)
    for m in _SERVICE_NAME_RE.finditer(text):
        name = m.group(1)
        parts = name.split("-")
        if any(p in _SERVICE_SUFFIXES for p in parts):
            if normalize_entity_name(name) not in seen:
                _add(name, name, "project")

    # 8. Chinese city names
    for city in _CHINESE_CITIES:
        if city in text:
            _add(city, city, "location")

    # 9. Chinese time expressions
    for m in _CHINESE_TIME_RE.finditer(text):
        _add(m.group(0), m.group(0), "time")

    # 10. Quoted strings and backtick terms
    for m in _QUOTED_RE.finditer(text):
        val = m.group(1).strip()
        if normalize_entity_name(val) not in seen:
            _add(val, val, "project")
    for m in _BACKTICK_RE.finditer(text):
        val = m.group(1).strip()
        if normalize_entity_name(val) not in seen:
            _add(val, val, "project")

    return entities


# ── LLM extraction ────────────────────────────────────────────────────

_LLM_EXTRACT_PROMPT = """\
Extract named entities from the following text. Return a JSON array of objects.
Each object: {{"name": "canonical name", "type": "tech|person|repo|project|concept"}}

Rules:
- Only extract specific, named entities (not generic words like "数据库", "服务", "系统")
- For tech terms: use the most common canonical form (e.g. "React", "Spark", "OAuth", "ETL")
  - Preserve original casing for proper nouns (React, not react; OAuth, not oauth)
  - Expand abbreviations only if unambiguous (RSC → keep as RSC, not "React Server Components")
- For people: use the name as written, including context if needed to disambiguate
  (e.g. if two people share a name, use "张伟（前端）" and "张伟（后端）")
- For @mentions: extract the name without @ (e.g. "@李明" → "李明")
- For service/module names: preserve hyphens (e.g. "auth-service", "payment-service")
- Deduplicate: if the same entity appears multiple times, include it once
- Max 10 entities per text
- Do NOT extract: generic verbs, common nouns, numbers, dates, or punctuation

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
    text: str,
    llm_client: Any,
) -> list[ExtractedEntity]:
    """LLM-based entity extraction. More accurate but slower."""
    try:
        response = llm_client.chat(
            messages=[
                {
                    "role": "user",
                    "content": _LLM_EXTRACT_PROMPT.format(text=text[:2000]),
                }
            ],
            temperature=0.0,
            max_tokens=300,
        )
        raw = (
            response
            if isinstance(response, str)
            else getattr(response, "content", str(response))
        )
        # Extract JSON array from LLM response — tolerates markdown fences and preamble text.
        # Falls back to empty list on any parse failure (best-effort, not critical path).
        start = raw.find("[")
        end = raw.rfind("]")
        if start == -1 or end == -1:
            return []
        try:
            items = json.loads(raw[start : end + 1])
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
            entities.append(
                ExtractedEntity(
                    name=name.lower(),
                    display_name=name,
                    entity_type=item.get("type", "concept"),
                )
            )
        return entities
    except Exception:
        logger.warning("LLM entity extraction failed", exc_info=True)
        return []
