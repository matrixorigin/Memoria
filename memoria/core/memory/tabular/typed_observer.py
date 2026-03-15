"""Typed Observer — extracts typed memories with atomic contradiction detection.

Uses MemoryStore for persistence and contradiction resolution.
Contradiction detection uses DB-side L2_DISTANCE with IVF-flat index.

Includes sensitivity filter — blocks PII/credentials from long-term storage.
"""

from __future__ import annotations

import logging
import time
import uuid
from datetime import datetime
from typing import Any, Optional

from memoria.core.db_consumer import DbFactory
from memoria.core.memory.tabular.explain import ContradictionStats, ObserverStats
from memoria.core.memory.tabular.json_utils import parse_json_array
from memoria.core.memory.tabular.metrics import MemoryMetrics
from memoria.core.memory.tabular.prompts import OBSERVER_EXTRACTION_PROMPT
from memoria.core.memory.tabular.sensitivity import check_sensitivity
from memoria.core.memory.tabular.store import MemoryStore
from memoria.core.memory.types import Memory, MemoryType, TrustTier, _utcnow

logger = logging.getLogger(__name__)

_VALID_TYPES = {t.value for t in MemoryType if t != MemoryType.WORKING}

_DEFAULT_L2_THRESHOLD = 0.55


# Backward-compatible alias for external callers that imported the private name.
_parse_json_array = parse_json_array


class TypedObserver:
    """Extract typed memories from conversation turns.

    Flow: LLM extraction → sensitivity filter → embed → contradiction detection → store.
    """

    def __init__(
        self,
        store: MemoryStore,
        llm_client: Any = None,
        embed_fn: Any = None,
        contradiction_threshold: float = 0.85,
        db_factory: Optional[DbFactory] = None,
        metrics: Optional[MemoryMetrics] = None,
    ):
        self.store = store
        self.llm = llm_client
        self.embed_fn = embed_fn
        self.contradiction_threshold = contradiction_threshold
        self._db_factory = db_factory
        self._metrics = metrics or MemoryMetrics()
        self._l2_threshold = (2.0 * (1.0 - contradiction_threshold)) ** 0.5

    def observe(
        self,
        user_id: str,
        messages: list[dict[str, Any]],
        source_event_ids: Optional[list[str]] = None,
        explain: bool = False,
        session_id: Optional[str] = None,
    ) -> tuple[list[Memory], Optional[ObserverStats]]:
        start = time.time() if explain else 0
        stats = ObserverStats() if explain else None

        candidates = self.extract_candidates(
            user_id, messages, source_event_ids, session_id=session_id
        )
        if stats:
            stats.memories_extracted = len(candidates)

        results = []
        for m in candidates:
            mem, c_stats = self.persist_with_contradiction_check(m, explain)
            results.append(mem)
            if stats and c_stats:
                if c_stats.found:
                    stats.memories_superseded += 1
                if stats.contradiction is None:
                    stats.contradiction = c_stats

        if stats:
            stats.memories_stored = len(results)
            stats.total_ms = (time.time() - start) * 1000

        return results, stats

    def extract_candidates(
        self,
        user_id: str,
        messages: list[dict[str, Any]],
        source_event_ids: Optional[list[str]] = None,
        session_id: Optional[str] = None,
    ) -> list[Memory]:
        """Extract candidate memories WITHOUT persisting. Applies sensitivity filter."""
        if not self.llm:
            return []

        raw = self._extract_via_llm(messages)
        if not raw:
            return []

        now = _utcnow()
        results = []

        for item in raw:
            mem = self._parse_item(item, user_id, source_event_ids or [], now)
            if not mem:
                continue

            if session_id:
                mem.session_id = session_id

            # Sensitivity filter — block HIGH-risk, redact MEDIUM-risk
            sensitivity = check_sensitivity(mem.content)
            if sensitivity.blocked:
                logger.info(
                    "Sensitivity filter blocked memory: %s", sensitivity.matched_labels
                )
                self._metrics.increment("sensitivity_blocked")
                continue
            if sensitivity.redacted_content is not None:
                logger.info(
                    "Sensitivity filter redacted memory: %s", sensitivity.matched_labels
                )
                self._metrics.increment("sensitivity_redacted")
                mem.content = sensitivity.redacted_content

            if self.embed_fn:
                try:
                    mem.embedding = self.embed_fn(mem.content)
                except Exception as e:
                    logger.warning("Embedding failed: %s", e)

            results.append(mem)

        return results

    def persist_with_contradiction_check(
        self, mem: Memory, explain: bool = False
    ) -> tuple[Memory, Optional[ContradictionStats]]:
        """Persist a single memory with contradiction detection + opinion evolution. Public API for pipeline."""
        stored, stats = self._store_with_contradiction_check(mem, explain)
        self._evolve_scene_opinions(stored)
        return stored, stats

    def observe_explicit(
        self,
        user_id: str,
        content: str,
        memory_type: MemoryType,
        initial_confidence: float = 0.9,
        source_event_ids: Optional[list[str]] = None,
        trust_tier: TrustTier = TrustTier.T3_INFERRED,
        session_id: Optional[str] = None,
        extra_metadata: Optional[dict[str, Any]] = None,
        generate_embedding: bool = True,
        explain: bool = False,
    ) -> tuple[Memory, Optional[ContradictionStats]]:
        """Directly write a memory (from MemoryWriteTool), skipping LLM extraction."""
        # Sensitivity filter
        sensitivity = check_sensitivity(content)
        if sensitivity.blocked:
            logger.warning(
                "Sensitivity filter blocked explicit memory: %s",
                sensitivity.matched_labels,
            )
            self._metrics.increment("sensitivity_blocked")
            raise ValueError(
                f"Content blocked by sensitivity filter: {sensitivity.matched_labels}"
            )
        if sensitivity.redacted_content is not None:
            logger.info(
                "Sensitivity filter redacted explicit memory: %s",
                sensitivity.matched_labels,
            )
            self._metrics.increment("sensitivity_redacted")
            content = sensitivity.redacted_content

        mem = Memory(
            memory_id=uuid.uuid4().hex,
            user_id=user_id,
            memory_type=memory_type,
            content=content,
            initial_confidence=initial_confidence,
            trust_tier=trust_tier,
            source_event_ids=source_event_ids or [],
            session_id=session_id,
            extra_metadata=extra_metadata,
            observed_at=_utcnow(),
        )
        if generate_embedding and self.embed_fn:
            try:
                mem.embedding = self.embed_fn(content)
            except Exception as e:
                logger.warning("Embedding failed: %s", e)

        return self.persist_with_contradiction_check(mem, explain)

    # Only send the most recent messages to the extraction LLM.
    # Older context is already captured in prior memory entries.
    _MAX_EXTRACT_MESSAGES = 20
    _MAX_EXTRACT_CHARS = 6000

    def _extract_via_llm(self, messages: list[dict[str, Any]]) -> list[dict[str, Any]]:
        recent = messages[-self._MAX_EXTRACT_MESSAGES :]
        conv_text = "\n".join(
            f"[{m.get('role', 'unknown')}]: {m.get('content', '')[:500]}"
            for m in recent
            if m.get("content")
        )
        conv_text = conv_text[-self._MAX_EXTRACT_CHARS :]
        try:
            result = self.llm.chat_with_tools(
                messages=[
                    {"role": "system", "content": OBSERVER_EXTRACTION_PROMPT},
                    {"role": "user", "content": conv_text},
                ],
                tools=[],
                tool_choice="none",
                task_hint="memory_extraction",
            )
            return _parse_json_array(result.get("content", ""))
        except Exception as e:
            logger.warning("Observer LLM extraction failed: %s", e)
            return []

    def _parse_item(
        self,
        item: dict,
        user_id: str,
        source_event_ids: list[str],
        now: datetime,
    ) -> Optional[Memory]:
        if not isinstance(item, dict) or not item.get("content"):
            return None
        mtype_str = item.get("type", "semantic")
        if mtype_str not in _VALID_TYPES:
            mtype_str = "semantic"
        confidence = item.get("confidence", 0.7)
        if not isinstance(confidence, (int, float)):
            confidence = 0.7
        confidence = max(0.0, min(1.0, float(confidence)))

        return Memory(
            memory_id=uuid.uuid4().hex,
            user_id=user_id,
            memory_type=MemoryType(mtype_str),
            content=item["content"],
            initial_confidence=confidence,
            source_event_ids=source_event_ids,
            observed_at=now,
        )

    def _store_with_contradiction_check(
        self, mem: Memory, explain: bool = False
    ) -> tuple[Memory, Optional[ContradictionStats]]:
        stats = ContradictionStats() if explain else None

        if mem.embedding is not None:
            contradiction, c_stats = self._find_contradiction(mem, explain)
            if stats and c_stats:
                stats.checked = c_stats.checked
                stats.query_ms = c_stats.query_ms
                stats.error = c_stats.error
            if contradiction:
                logger.info(
                    "Contradiction detected: '%s' supersedes '%s'",
                    mem.content[:60],
                    contradiction.content[:60],
                )
                if stats:
                    stats.found = True
                    stats.superseded_id = contradiction.memory_id
                return self.store.supersede(contradiction.memory_id, mem), stats

        return self.store.create(mem), stats

    def _find_contradiction(
        self, new: Memory, explain: bool = False
    ) -> tuple[Optional[Memory], Optional[ContradictionStats]]:
        stats = ContradictionStats(checked=True) if explain else None

        if new.embedding is None or self._db_factory is None:
            if stats:
                stats.checked = False
            return None, stats

        from matrixone.sqlalchemy_ext import l2_distance

        from memoria.core.memory.models.memory import MemoryRecord

        dist_expr = l2_distance(MemoryRecord.embedding, new.embedding).label("l2_dist")

        db = self._db_factory()
        start = time.time() if explain else 0
        try:
            row = (
                db.query(
                    MemoryRecord.memory_id,
                    MemoryRecord.content,
                    MemoryRecord.initial_confidence,
                    dist_expr,
                )
                .filter(
                    MemoryRecord.user_id == new.user_id,
                    MemoryRecord.is_active == 1,
                    MemoryRecord.memory_type == new.memory_type.value,
                    MemoryRecord.embedding.isnot(None),
                    MemoryRecord.memory_id != new.memory_id,
                )
                .order_by("l2_dist")
                .limit(1)
                .first()
            )
        except Exception as e:
            if stats:
                stats.error = str(e)
                stats.query_ms = (time.time() - start) * 1000
            raise
        finally:
            db.close()

        if stats:
            stats.query_ms = (time.time() - start) * 1000

        if row is None:
            return None, stats

        if (
            float(row.l2_dist) <= self._l2_threshold
            and row.content.strip() != new.content.strip()
        ):
            return Memory(
                memory_id=row.memory_id,
                user_id=new.user_id,
                memory_type=new.memory_type,
                content=row.content,
                initial_confidence=row.initial_confidence,
            ), stats
        return None, stats

    def _evolve_scene_opinions(self, new_mem: Memory) -> None:
        """Find scene memories similar to new_mem and evolve their confidence.

        Scene memories are reflection-produced (T4, session_id=None).
        Uses DB-side cosine_similarity to find nearby scenes, then OpinionEvolver
        to compute confidence delta. Lightweight: 1 DB query, no LLM.
        """
        if new_mem.embedding is None or self._db_factory is None:
            return

        from memoria.core.memory.reflection.opinion import OpinionEvolver

        from memoria.core.memory.types import TrustTier

        db = self._db_factory()
        try:
            # Use raw SQL with cosine_similarity for accurate similarity
            from sqlalchemy import text as sa_text

            emb_str = "[" + ",".join(str(v) for v in new_mem.embedding) + "]"
            rows = db.execute(
                sa_text("""
                SELECT memory_id, content, initial_confidence, trust_tier,
                       cosine_similarity(embedding, :emb) AS cos_sim
                FROM mem_memories
                WHERE user_id = :uid AND is_active = 1
                  AND session_id IS NULL
                  AND embedding IS NOT NULL
                  AND memory_id != :mid
                ORDER BY cos_sim DESC
                LIMIT 5
            """),
                {"uid": new_mem.user_id, "emb": emb_str, "mid": new_mem.memory_id},
            ).fetchall()

            if not rows:
                return

            evolver = OpinionEvolver()
            for row in rows:
                similarity = float(row.cos_sim)

                scene = Memory(
                    memory_id=row.memory_id,
                    user_id=new_mem.user_id,
                    memory_type=new_mem.memory_type,
                    content=row.content,
                    initial_confidence=row.initial_confidence,
                    trust_tier=TrustTier(row.trust_tier)
                    if row.trust_tier
                    else TrustTier.T4_UNVERIFIED,
                )

                update = evolver.evaluate_evidence(similarity, scene)
                if update.evidence_type == "neutral":
                    continue

                new_tier = None
                if update.promoted:
                    new_tier = "T3"
                is_active = None if not update.quarantined else False

                self.store.update_confidence(
                    update.memory_id,
                    update.new_confidence,
                    trust_tier=new_tier,
                    is_active=is_active,
                )
                logger.info(
                    "Opinion evolved: %s %s %.2f→%.2f%s%s",
                    row.memory_id[:8],
                    update.evidence_type,
                    update.old_confidence,
                    update.new_confidence,
                    " PROMOTED" if update.promoted else "",
                    " QUARANTINED" if update.quarantined else "",
                )
        except Exception as e:
            logger.warning("Opinion evolution failed: %s", e)
        finally:
            db.close()
