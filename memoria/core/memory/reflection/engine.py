"""ReflectionEngine — backend-agnostic pattern synthesis.

Receives candidates from CandidateProvider → importance filter →
LLM synthesis → persist as scene-type memories.

Imports only from interfaces.py and types.py — never from tabular/ or graph/.

See docs/design/memory/backend-coexistence.md
See docs/design/memory/graph-memory.md §4.3
"""

from __future__ import annotations

import json
import logging
from dataclasses import dataclass, field
from typing import Any

from memoria.core.memory.interfaces import (
    CandidateProvider,
    MemoryWriter,
    ReflectionCandidate,
)
from memoria.core.memory.reflection.importance import DAILY_THRESHOLD
from memoria.core.memory.reflection.prompts import REFLECTION_SYNTHESIS_PROMPT
from memoria.core.memory.types import MemoryType, TrustTier

logger = logging.getLogger(__name__)


@dataclass
class ReflectionResult:
    """Result of a reflection cycle."""

    candidates_found: int = 0
    candidates_passed: int = 0
    candidates_skipped_low_importance: int = 0
    scenes_created: int = 0
    llm_calls: int = 0
    opinion_checks: int = 0
    errors: list[str] = field(default_factory=list)
    total_ms: float = 0.0
    # Low-importance candidates: [(signal, score)] — lightweight summary only
    low_importance_candidates: list[tuple[str, float]] = field(default_factory=list)


@dataclass
class SynthesizedInsight:
    """An insight produced by LLM synthesis."""

    memory_type: MemoryType
    content: str
    confidence: float
    evidence_summary: str
    source_memory_ids: list[str]


class ReflectionEngine:
    """Backend-agnostic reflection: 3-stage pipeline.

    Stage 1: Collect high-activation subgraph around candidate cluster
    Stage 2: Synthesize scene with enriched context (neighbors + entities)
    Stage 3: Persist scene node + run opinion evolution against existing scenes

    Args:
        candidate_provider: backend-specific provider (tabular or graph).
        writer: MemoryWriter for persisting new scene memories.
        llm_client: LLM client for synthesis calls.
        threshold: minimum importance score to trigger synthesis.
        subgraph_collector: optional callable(user_id, memory_ids) → context string.
            If provided, Stage 1 collects neighbor/entity context for richer synthesis.
        opinion_checker: optional callable(user_id, content) → list of conflicts.
            If provided, Stage 3 checks new scene against existing scenes.
        entity_names_collector: optional callable(user_id, memory_ids) → set of entity names.
            If provided, enriches persisted scene content with entity summary (0B.4).
    """

    def __init__(
        self,
        candidate_provider: CandidateProvider,
        writer: MemoryWriter,
        llm_client: Any,
        threshold: float = DAILY_THRESHOLD,
        llm_threshold: float | None = None,
        llm_retries: int = 1,
        subgraph_collector: Any = None,
        opinion_checker: Any = None,
        entity_names_collector: Any = None,
    ):
        self._provider = candidate_provider
        self._writer = writer
        self._llm = llm_client
        self._threshold = threshold
        self._llm_threshold = llm_threshold if llm_threshold is not None else threshold
        self._llm_retries = llm_retries
        self._subgraph_collector = subgraph_collector
        self._opinion_checker = opinion_checker
        self._entity_names_collector = entity_names_collector

    def reflect(
        self,
        user_id: str,
        *,
        since_hours: int = 24,
        existing_knowledge: str = "",
    ) -> ReflectionResult:
        """Run one reflection cycle for a user (3-stage pipeline).

        Stage 1: Get candidates, filter by importance, collect subgraph context
                 for all qualifying candidates (neighbors + entities)
        Stage 2: LLM synthesis with enriched context per candidate
        Stage 3: Persist scene + opinion evolution against existing scenes
        """
        import time

        start = time.time()
        result = ReflectionResult()

        # ── Stage 1: Get candidates ──
        try:
            candidates = self._provider.get_reflection_candidates(
                user_id,
                since_hours=since_hours,
            )
        except Exception as e:
            logger.error("Reflection candidate retrieval failed: %s", e)
            result.errors.append(f"candidates: {e}")
            result.total_ms = (time.time() - start) * 1000
            return result

        result.candidates_found = len(candidates)
        if not candidates:
            result.total_ms = (time.time() - start) * 1000
            return result

        # Score and filter
        passed = [
            (c, c.importance_score)
            for c in candidates
            if c.importance_score >= self._threshold
        ]
        result.candidates_passed = len(passed)

        if not passed:
            result.total_ms = (time.time() - start) * 1000
            return result

        # Split: high-importance → LLM synthesis, low → candidates-only
        synth_candidates = [(c, s) for c, s in passed if s >= self._llm_threshold]
        low_candidates = [(c, s) for c, s in passed if s < self._llm_threshold]
        result.candidates_skipped_low_importance = len(low_candidates)
        result.low_importance_candidates = [(c.signal, s) for c, s in low_candidates]

        # ── Stage 1b: Collect subgraph context for all synth candidates ──
        subgraph_contexts: dict[int, str] = {}
        if self._subgraph_collector and synth_candidates:
            for idx, (candidate, _score) in enumerate(synth_candidates):
                try:
                    mem_ids = [m.memory_id for m in candidate.memories]
                    subgraph_contexts[idx] = self._subgraph_collector(user_id, mem_ids)
                except Exception as e:
                    logger.debug(
                        "Subgraph collection failed for %s: %s", candidate.signal, e
                    )

        # ── Stage 2: Synthesize with enriched context ──
        for idx, (candidate, score) in enumerate(synth_candidates):
            try:
                subgraph_context = subgraph_contexts.get(idx, "")

                result.llm_calls += 1
                insights = self._synthesize_with_retry(
                    candidate, existing_knowledge, subgraph_context
                )

                # ── Stage 3: Persist + opinion check ──
                for insight in insights:
                    try:
                        # Check for conflicts with existing scenes before persisting
                        if self._opinion_checker:
                            try:
                                conflicts = self._opinion_checker(
                                    user_id, insight.content
                                )
                                if conflicts:
                                    result.opinion_checks += 1
                                    logger.info(
                                        "New scene conflicts with %d existing scenes, flagging",
                                        len(conflicts),
                                    )
                                    insight.confidence = min(insight.confidence, 0.4)
                            except Exception as e:
                                logger.debug("Opinion check failed: %s", e)

                        self._persist_insight(user_id, insight)
                        result.scenes_created += 1
                    except Exception as e:
                        logger.warning("Failed to persist insight: %s", e)
                        result.errors.append(f"persist: {e}")

            except Exception as e:
                logger.warning("Reflection synthesis failed: %s", e)
                result.errors.append(f"synthesis: {e}")

        result.total_ms = (time.time() - start) * 1000
        return result

    def _synthesize_with_retry(
        self,
        candidate: ReflectionCandidate,
        existing_knowledge: str,
        subgraph_context: str = "",
    ) -> list[SynthesizedInsight]:
        """Call _synthesize with retry on failure."""
        last_err: Exception | None = None
        for attempt in range(1 + self._llm_retries):
            try:
                return self._synthesize(candidate, existing_knowledge, subgraph_context)
            except Exception as e:
                last_err = e
                if attempt < self._llm_retries:
                    logger.info("Retrying synthesis (attempt %d): %s", attempt + 1, e)
        raise last_err  # type: ignore[misc]

    def _synthesize(
        self,
        candidate: ReflectionCandidate,
        existing_knowledge: str,
        subgraph_context: str = "",
    ) -> list[SynthesizedInsight]:
        """LLM synthesis for a single candidate cluster."""
        experiences = "\n\n".join(
            f"[{m.memory_type.value}] {m.content}" for m in candidate.memories
        )

        # Enrich with subgraph context (neighbors + entities) if available
        if subgraph_context:
            experiences = f"{experiences}\n\nRELATED CONTEXT:\n{subgraph_context}"

        prompt = REFLECTION_SYNTHESIS_PROMPT.format(
            existing_knowledge=existing_knowledge or "(none)",
            experiences=experiences,
        )

        response = self._llm.chat(
            messages=[{"role": "user", "content": prompt}],
            temperature=0.3,
            max_tokens=500,
        )

        raw = (
            response
            if isinstance(response, str)
            else getattr(response, "content", str(response))
        )
        return self._parse_insights(raw, candidate)

    def _parse_insights(
        self,
        raw: str,
        candidate: ReflectionCandidate,
    ) -> list[SynthesizedInsight]:
        """Parse LLM JSON output into SynthesizedInsight list.

        Raises ValueError on unparseable output so callers can record the error.
        """
        # Extract JSON array from response
        text = raw.strip()
        start = text.find("[")
        end = text.rfind("]")
        if start == -1 or end == -1:
            raise ValueError(f"No JSON array in LLM output: {text[:200]}")

        try:
            items = json.loads(text[start : end + 1])
        except json.JSONDecodeError as e:
            raise ValueError(f"Invalid JSON in LLM output: {e} — {text[:200]}") from e

        source_ids = [m.memory_id for m in candidate.memories]
        insights = []
        for item in items[:2]:  # max 2 insights per candidate
            try:
                mt = MemoryType(item["type"])
            except (KeyError, ValueError):
                continue
            conf = max(0.3, min(0.7, float(item.get("confidence", 0.5))))
            insights.append(
                SynthesizedInsight(
                    memory_type=mt,
                    content=item.get("content", ""),
                    confidence=conf,
                    evidence_summary=item.get("evidence_summary", ""),
                    source_memory_ids=source_ids,
                )
            )
        return insights

    def _persist_insight(self, user_id: str, insight: SynthesizedInsight) -> None:
        """Persist a synthesized insight as a scene-type memory.

        Enriches content with source_count and entity summary (0B.4).
        """
        source_count = len(insight.source_memory_ids)
        content = insight.content
        if source_count > 0:
            content = f"{content}\n[source_count={source_count}]"

        # Collect entity names from source memories via subgraph_collector context
        if self._entity_names_collector:
            try:
                entity_names = self._entity_names_collector(
                    user_id, insight.source_memory_ids
                )
                if entity_names:
                    content = (
                        f"{content}\n[entities: {', '.join(sorted(entity_names))}]"
                    )
            except Exception as e:
                logger.debug("Entity names collection failed: %s", e)

        self._writer.store(
            user_id=user_id,
            content=content,
            memory_type=insight.memory_type,
            source_event_ids=insight.source_memory_ids,
            initial_confidence=insight.confidence,
            trust_tier=TrustTier.T4_UNVERIFIED,
        )
