#!/usr/bin/env python3
"""Add a normalized 6-bucket memory-ability rollup to benchmark JSON reports."""

from __future__ import annotations

import argparse
import json
from collections import Counter, defaultdict
from pathlib import Path
from typing import Any


TAXONOMY_VERSION = "v1"

BUCKETS: list[dict[str, str]] = [
    {
        "key": "single_session_grounding",
        "label": "Single-Session Grounding",
        "description": "Direct factual or contextual grounding within one session.",
    },
    {
        "key": "preference_understanding",
        "label": "Preference Understanding",
        "description": "Capturing and applying user preferences beyond keyword matching.",
    },
    {
        "key": "multi_session_synthesis",
        "label": "Multi-Session Synthesis",
        "description": "Combining information across multiple sessions into a coherent answer.",
    },
    {
        "key": "temporal_state_tracking",
        "label": "Temporal State Tracking",
        "description": "Reasoning about time, chronology, order, and evolving state.",
    },
    {
        "key": "knowledge_update_and_conflict_handling",
        "label": "Knowledge Update And Conflict Handling",
        "description": "Resolving stale-vs-current knowledge and handling contradictions.",
    },
    {
        "key": "abstention_and_constraint_following",
        "label": "Abstention And Constraint Following",
        "description": "Refusing unsupported answers and following remembered instructions or constraints.",
    },
]

BUCKET_BY_KEY = {bucket["key"]: bucket for bucket in BUCKETS}

LONGMEMEVAL_BUCKET_MAP = {
    "single_session_user": "single_session_grounding",
    "single_session_assistant": "single_session_grounding",
    "single_session_preference": "preference_understanding",
    "multi_session": "multi_session_synthesis",
    "temporal_reasoning": "temporal_state_tracking",
    "knowledge_update": "knowledge_update_and_conflict_handling",
    "abstention": "abstention_and_constraint_following",
}

BEAM_BUCKET_MAP = {
    "information_extraction": "single_session_grounding",
    "preference_following": "preference_understanding",
    "multi_session_reasoning": "multi_session_synthesis",
    "summarization": "multi_session_synthesis",
    "temporal_reasoning": "temporal_state_tracking",
    "event_ordering": "temporal_state_tracking",
    "knowledge_update": "knowledge_update_and_conflict_handling",
    "contradiction_resolution": "knowledge_update_and_conflict_handling",
    "abstention": "abstention_and_constraint_following",
    "instruction_following": "abstention_and_constraint_following",
}


def normalize_text(value: Any) -> str:
    text = str(value or "").strip().lower()
    for ch in (" ", "-", "/", ":", "."):
        text = text.replace(ch, "_")
    while "__" in text:
        text = text.replace("__", "_")
    return text.strip("_")


def grade_from_score(score: float) -> str:
    if score >= 95:
        return "S"
    if score >= 85:
        return "A"
    if score >= 75:
        return "B"
    if score >= 60:
        return "C"
    return "D"


def infer_source_family(payload: dict[str, Any], result: dict[str, Any]) -> str:
    candidates = [
        result.get("source_family"),
        result.get("dataset_family"),
        (result.get("metadata") or {}).get("source_family"),
        (result.get("gold_reference") or {}).get("source_family"),
        (result.get("scenario") or {}).get("domain"),
        payload.get("source_family"),
        payload.get("dataset_family"),
    ]
    dataset_id = str(payload.get("dataset_id") or "").lower()
    for candidate in candidates:
        normalized = normalize_text(candidate)
        if normalized in {"longmemeval", "longmem"}:
            return "longmemeval"
        if normalized == "beam":
            return "beam"
    if dataset_id.startswith("beam"):
        return "beam"
    return "longmemeval"


def infer_question_type(result: dict[str, Any]) -> str:
    candidates = [
        result.get("question_type"),
        (result.get("metadata") or {}).get("question_type"),
        (result.get("gold_reference") or {}).get("question_type"),
        (result.get("scenario") or {}).get("question_type"),
        ((result.get("scenario") or {}).get("metadata") or {}).get("question_type"),
        result.get("title"),
        (result.get("scenario") or {}).get("title"),
    ]
    for candidate in candidates:
        if candidate:
            return normalize_text(candidate)
    scenario_id = normalize_text(result.get("scenario_id"))
    if scenario_id.endswith("_abs"):
        return "abstention"
    return "unknown"


def infer_bucket(source_family: str, question_type: str) -> str:
    qtype = normalize_text(question_type)
    if source_family == "beam":
        if qtype in BEAM_BUCKET_MAP:
            return BEAM_BUCKET_MAP[qtype]
    else:
        if qtype in LONGMEMEVAL_BUCKET_MAP:
            return LONGMEMEVAL_BUCKET_MAP[qtype]
        if qtype.endswith("_abs"):
            return "abstention_and_constraint_following"

    if "preference" in qtype:
        return "preference_understanding"
    if "summar" in qtype or ("multi" in qtype and "session" in qtype):
        return "multi_session_synthesis"
    if "temporal" in qtype or "ordering" in qtype or "timeline" in qtype:
        return "temporal_state_tracking"
    if (
        "update" in qtype
        or "contradiction" in qtype
        or "conflict" in qtype
        or "stale" in qtype
    ):
        return "knowledge_update_and_conflict_handling"
    if "abstention" in qtype or "instruction" in qtype or "constraint" in qtype:
        return "abstention_and_constraint_following"
    return "single_session_grounding"


def infer_score(result: dict[str, Any]) -> float | None:
    for key in ("total_score", "score", "overall_score"):
        value = result.get(key)
        if isinstance(value, (int, float)):
            return float(value)
    return None


def infer_passed(result: dict[str, Any], score: float | None) -> bool | None:
    passed = result.get("passed")
    if isinstance(passed, bool):
        return passed
    verdict = str(result.get("verdict") or "").strip().lower()
    if verdict:
        if verdict in {"passed", "pass", "correct"}:
            return True
        if verdict in {"failed", "fail", "incorrect", "runner_error"}:
            return False
    if score is not None:
        return score >= 100.0
    return None


def summarize_bucket(results: list[dict[str, Any]]) -> dict[str, Any]:
    scored = [result["_normalized_score"] for result in results if result["_normalized_score"] is not None]
    pass_values = [result["_normalized_passed"] for result in results if result["_normalized_passed"] is not None]
    question_type_counts = Counter(result["_normalized_question_type"] for result in results)
    source_family_counts = Counter(result["_normalized_source_family"] for result in results)
    score = round(sum(scored) / len(scored), 2) if scored else None
    pass_rate = round(100.0 * sum(1 for value in pass_values if value) / len(pass_values), 2) if pass_values else None
    return {
        "scenario_count": len(results),
        "score": score,
        "grade": grade_from_score(score) if score is not None else None,
        "pass_rate": pass_rate,
        "pass_count": sum(1 for value in pass_values if value),
        "scored_scenario_count": len(scored),
        "sample_scenario_ids": [result.get("scenario_id") for result in results[:10] if result.get("scenario_id")],
        "question_type_counts": dict(sorted(question_type_counts.items())),
        "source_family_counts": dict(sorted(source_family_counts.items())),
    }


def augment_report(payload: dict[str, Any]) -> bool:
    results = payload.get("results")
    if not isinstance(results, list) or not results:
        return False

    bucket_results: dict[str, list[dict[str, Any]]] = defaultdict(list)
    for result in results:
        if not isinstance(result, dict):
            continue
        source_family = infer_source_family(payload, result)
        question_type = infer_question_type(result)
        bucket_key = infer_bucket(source_family, question_type)
        score = infer_score(result)
        passed = infer_passed(result, score)

        result["memory_ability_bucket"] = bucket_key
        result["memory_ability_bucket_label"] = BUCKET_BY_KEY[bucket_key]["label"]
        result["_normalized_source_family"] = source_family
        result["_normalized_question_type"] = question_type
        result["_normalized_score"] = score
        result["_normalized_passed"] = passed
        bucket_results[bucket_key].append(result)

    payload["memory_ability_taxonomy"] = {
        "version": TAXONOMY_VERSION,
        "buckets": BUCKETS,
        "notes": [
            "This rollup normalizes LongMemEval and BEAM into shared memory-ability buckets.",
            "Raw benchmark labels remain the source of truth and are preserved per result.",
            "Use these buckets for cross-benchmark Memoria reporting, not as a replacement for official benchmark categories.",
        ],
    }
    payload["by_memory_ability_bucket"] = {
        bucket["key"]: {
            "label": bucket["label"],
            "description": bucket["description"],
            **summarize_bucket(bucket_results[bucket["key"]]),
        }
        for bucket in BUCKETS
        if bucket_results.get(bucket["key"])
    }

    for result in results:
        result.pop("_normalized_source_family", None)
        result.pop("_normalized_question_type", None)
        result.pop("_normalized_score", None)
        result.pop("_normalized_passed", None)
    return True


def iter_report_paths(paths: list[str]) -> list[Path]:
    resolved: list[Path] = []
    for raw in paths:
        path = Path(raw)
        if path.is_dir():
            resolved.extend(sorted(path.rglob("*.report.json")))
            resolved.extend(sorted(path.rglob("*.qa.report.json")))
            continue
        resolved.append(path)
    unique: list[Path] = []
    seen: set[Path] = set()
    for path in resolved:
        resolved_path = path.resolve()
        if resolved_path not in seen:
            seen.add(resolved_path)
            unique.append(resolved_path)
    return unique


def main() -> None:
    parser = argparse.ArgumentParser(
        description="Normalize benchmark JSON reports into a shared 6-bucket memory taxonomy."
    )
    parser.add_argument(
        "paths",
        nargs="+",
        help="Report files or directories containing *.report.json / *.qa.report.json files.",
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="Print the paths that would be updated without rewriting them.",
    )
    args = parser.parse_args()

    changed = 0
    for path in iter_report_paths(args.paths):
        if not path.exists():
            print(f"skip missing: {path}")
            continue
        payload = json.loads(path.read_text(encoding="utf-8"))
        if not augment_report(payload):
            print(f"skip unsupported: {path}")
            continue
        if args.dry_run:
            print(f"would update: {path}")
        else:
            path.write_text(
                json.dumps(payload, ensure_ascii=False, indent=2) + "\n",
                encoding="utf-8",
            )
            print(f"updated: {path}")
        changed += 1
    if changed == 0:
        raise SystemExit("no benchmark reports updated")


if __name__ == "__main__":
    main()
