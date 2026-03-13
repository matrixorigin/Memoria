"""MemoryRetriever — MO-native hybrid retrieval for the memories table.

Scoring strategy (3 phases):
  Phase 1: SQL-side — keyword filter (MATCH in WHERE) + BM25 scoring (MATCH in SELECT)
           + temporal/confidence scoring
  Phase 2: SQL-side — vector candidates via L2_DISTANCE (when embedding provided)
  Phase 3: App-side — merge + re-rank using all 4 dimensions (vector, keyword, temporal, confidence)

Keyword scoring uses MATCH() AGAINST() in SELECT to get continuous BM25 relevance,
normalized to 0-1 via score/(score+1) saturating transform.

Supports EXPLAIN ANALYZE mode: pass explain=True to get execution stats.
"""

from __future__ import annotations

import logging
import math
import time
from dataclasses import dataclass
from typing import Optional

from sqlalchemy import literal_column, text
from sqlalchemy.sql import func

from memoria.core.db_consumer import DbConsumer, DbFactory
from memoria.core.memory.config import MemoryGovernanceConfig
from memoria.core.memory.tabular.explain import CandidateScore, RetrievalStats
from memoria.core.memory.tabular.metrics import MemoryMetrics, Timer
from memoria.core.memory.types import Memory, MemoryType, RetrievalWeights, TrustTier

logger = logging.getLogger(__name__)

# Task-hint → weight presets (all sum to 1.0)
TASK_WEIGHTS: dict[str, RetrievalWeights] = {
    "code": RetrievalWeights(vector=0.3, keyword=0.25, temporal=0.15, confidence=0.3),
    "reasoning": RetrievalWeights(
        vector=0.4, keyword=0.1, temporal=0.2, confidence=0.3
    ),
    "recall": RetrievalWeights(vector=0.5, keyword=0.1, temporal=0.3, confidence=0.1),
    "default": RetrievalWeights(vector=0.3, keyword=0.2, temporal=0.2, confidence=0.3),
}


def _relevance_expr(w_time: float, w_conf: float, decay_hours: float, half_life: float):
    """Build ORM expression for temporal + confidence scoring."""
    from memoria.core.memory.models.memory import MemoryRecord as M

    age_hours = func.timestampdiff(text("HOUR"), M.observed_at, func.now())
    age_days = func.timestampdiff(text("DAY"), M.observed_at, func.now())
    return (
        w_time * func.exp(-age_hours / decay_hours)
        + w_conf * (M.initial_confidence * func.exp(-age_days / half_life))
    ).label("relevance")


def _safe_exp(x: float) -> float:
    """exp() clamped to avoid overflow."""
    return math.exp(max(-500.0, min(500.0, x)))


@dataclass
class _Candidate:
    memory_id: str
    content: str
    memory_type: str
    initial_confidence: float
    observed_at: object
    session_id: Optional[str]
    trust_tier: str = "T3"
    keyword_score: float = 0.0  # Continuous BM25 score from MATCH AGAINST
    l2_dist: Optional[float] = None
    access_count: int = 0


@dataclass
class _PhaseStats:
    keyword_attempted: bool = False
    keyword_hit: bool = False
    keyword_error: Optional[str] = None
    vector_attempted: bool = False
    vector_hit: bool = False
    vector_error: Optional[str] = None


class MemoryRetriever(DbConsumer):
    """Query-aware hybrid retrieval over the memories table.

    Confidence decay is computed at query time only — never mutated in DB.
    """

    def __init__(
        self,
        db_factory: DbFactory,
        decay_hours: float = 720.0,
        half_life_days: float = 30.0,
        metrics: Optional[MemoryMetrics] = None,
        config: Optional[MemoryGovernanceConfig] = None,
    ):
        super().__init__(db_factory)
        self.decay_hours = decay_hours
        self.half_life_days = half_life_days
        self._metrics = metrics or MemoryMetrics()
        if config is None:
            from memoria.core.memory.config import DEFAULT_CONFIG

            config = DEFAULT_CONFIG
        self._config = config

    def retrieve(
        self,
        user_id: str,
        query_text: str,
        session_id: str,
        query_embedding: Optional[list[float]] = None,
        memory_types: Optional[list[MemoryType]] = None,
        limit: int = 10,
        task_hint: Optional[str] = None,
        weights: Optional[RetrievalWeights] = None,
        include_cross_session: bool = True,
        explain: bool = False,
    ) -> tuple[list[Memory], Optional[RetrievalStats]]:
        start = time.time() if explain else 0
        stats = RetrievalStats() if explain else None

        weights = weights or TASK_WEIGHTS.get(
            task_hint or "default", TASK_WEIGHTS["default"]
        )

        # ── L0: session-scoped working/tool_result ──
        # Always included when session_id is set, unless caller explicitly
        # requests only non-working types (e.g. memory_types=[semantic]).
        l0_memories: list[Memory] = []
        caller_excluded_l0 = memory_types is not None and not any(
            t in memory_types for t in (MemoryType.WORKING, MemoryType.TOOL_RESULT)
        )
        if session_id and not caller_excluded_l0:
            l0_memories = self._load_l0(user_id, session_id, query_embedding)

        # ── L1: cross-session semantic/procedural/profile (default retrieval) ──
        l1_types = memory_types or [
            MemoryType.SEMANTIC,
            MemoryType.PROCEDURAL,
            MemoryType.PROFILE,
        ]
        # Exclude L0 types from L1 query to avoid duplicates
        l1_types = [
            t for t in l1_types if t not in (MemoryType.WORKING, MemoryType.TOOL_RESULT)
        ]
        type_values = tuple(t.value for t in l1_types) if l1_types else ()

        if not type_values:
            # Only L0 types requested — skip L1 entirely, return up to limit (no cap).
            memories = l0_memories[:limit]
            if stats:
                stats.final_count = len(memories)
                stats.total_ms = (time.time() - start) * 1000
            return memories, stats

        # Cap L0 at floor(limit/2) so L1 always gets at least ceil(limit/2) slots,
        # preventing many working memories from crowding out long-term semantic memories.
        # This cap only applies when L1 is also being queried (type_values is non-empty).
        l0_cap = limit // 2  # floor: e.g. limit=5→2, limit=3→1, limit=1→0
        l0_memories = l0_memories[:l0_cap]

        # Reserve remaining slots for L1
        l1_limit = max(1, limit - len(l0_memories))

        base_params = {
            "uid": user_id,
            "types": type_values,
            "decay_hours": self.decay_hours,
            "half_life": self.half_life_days,
            "session_id": session_id,
            "include_cross": include_cross_session,
        }

        with Timer("retriever_retrieve", self._metrics):
            with self._db() as db:
                # Phase 1
                p1_start = time.time() if explain else 0
                phase1, p1_stats = self._phase1(
                    db,
                    query_text,
                    base_params,
                    weights,
                    l1_limit * 2 if query_embedding else l1_limit,
                )
            if stats:
                stats.keyword_attempted = p1_stats.keyword_attempted
                stats.keyword_hit = p1_stats.keyword_hit
                stats.keyword_error = p1_stats.keyword_error
                stats.phase1_candidates = len(phase1)
                stats.phase1_ms = (time.time() - p1_start) * 1000

            if not query_embedding:
                logger.warning(
                    "tabular_retriever: query_embedding is None, skipping phase2 vector search"
                )
                l1 = [self._to_memory(c, user_id) for c in phase1[:l1_limit]]
                memories = l0_memories + l1
                memories = memories[:limit]
                if stats:
                    self._annotate_scores(phase1[:l1_limit], weights, stats)
                    stats.final_count = len(memories)
                    stats.total_ms = (time.time() - start) * 1000
                self._bump_access_counts([m.memory_id for m in memories])
                return memories, stats

            # Phase 2
            p2_start = time.time() if explain else 0
            phase2, p2_stats = self._phase2(
                db, query_embedding, base_params, l1_limit * 2
            )
            if stats:
                stats.vector_attempted = p2_stats.vector_attempted
                stats.vector_hit = p2_stats.vector_hit
                stats.vector_error = p2_stats.vector_error
                stats.phase2_candidates = len(phase2)
                stats.phase2_ms = (time.time() - p2_start) * 1000

            # Phase 3: merge
            merge_start = time.time() if explain else 0
            l1 = self._merge(phase1, phase2, user_id, weights, l1_limit, stats=stats)
            memories = l0_memories + l1
            memories = memories[:limit]
            if stats:
                stats.merged_candidates = len(
                    {c.memory_id for c in phase1} | {c.memory_id for c in phase2}
                )
                stats.final_count = len(memories)
                stats.merge_ms = (time.time() - merge_start) * 1000
                stats.total_ms = (time.time() - start) * 1000

            # Increment access counts for returned memories (fire-and-forget)
            self._bump_access_counts([m.memory_id for m in memories])

            return memories, stats

    def _load_l0(
        self,
        user_id: str,
        session_id: str,
        query_embedding: Optional[list[float]] = None,
    ) -> list[Memory]:
        """L0: Load active working/tool_result memories for the current session.

        These are always included — they represent immediate context.
        When query_embedding is available, ordered by vector relevance;
        otherwise by recency (newest first).
        """
        from memoria.core.memory.models.memory import MemoryRecord as M

        with self._db() as db:
            q = db.query(
                M.memory_id,
                M.content,
                M.memory_type,
                M.initial_confidence,
                M.observed_at,
                M.session_id,
                M.trust_tier,
            ).filter(
                M.user_id == user_id,
                M.session_id == session_id,
                M.is_active > 0,
                M.memory_type.in_(
                    (MemoryType.WORKING.value, MemoryType.TOOL_RESULT.value)
                ),
            )
            if query_embedding:
                try:
                    from matrixone.sqlalchemy_ext import l2_distance

                    dist = l2_distance(M.embedding, query_embedding).label("l2_dist")
                    # Use CASE to sort: records with embeddings first (by distance),
                    # then records without embeddings (by recency). Do NOT filter out
                    # records without embeddings — L0 represents immediate context and
                    # should always be visible regardless of embedding availability.
                    from sqlalchemy import case, literal

                    has_emb = case(
                        (M.embedding.isnot(None), literal(0)), else_=literal(1)
                    )
                    q = q.add_columns(dist).order_by(
                        has_emb, "l2_dist", M.observed_at.desc()
                    )
                except Exception:
                    q = q.order_by(M.observed_at.desc())
            else:
                q = q.order_by(M.observed_at.desc())
            rows = q.limit(20).all()
        return [
            Memory(
                memory_id=r.memory_id,
                user_id=user_id,
                memory_type=MemoryType(r.memory_type),
                content=r.content,
                initial_confidence=r.initial_confidence,
                session_id=r.session_id,
                observed_at=r.observed_at,
                trust_tier=TrustTier(r.trust_tier)
                if r.trust_tier
                else TrustTier.T3_INFERRED,
            )
            for r in rows
        ]

    def _phase1(
        self,
        db,
        query_text: str,
        base_params: dict,
        weights: RetrievalWeights,
        limit: int,
    ) -> tuple[list[_Candidate], _PhaseStats]:
        total = weights.temporal + weights.confidence
        w_time = weights.temporal / total if total > 0 else 0.5
        w_conf = weights.confidence / total if total > 0 else 0.5
        stats = _PhaseStats()

        from memoria.core.memory.models.memory import MemoryRecord as M

        rel = _relevance_expr(w_time, w_conf, self.decay_hours, self.half_life_days)
        type_values = base_params["types"]
        uid = base_params["uid"]
        session_id = base_params["session_id"]
        include_cross = base_params["include_cross"]

        def _base_query():
            q = db.query(
                M.memory_id,
                M.content,
                M.memory_type,
                M.initial_confidence,
                M.observed_at,
                M.session_id,
                M.trust_tier,
                M.access_count,
                rel,
            ).filter(M.user_id == uid, M.is_active > 0, M.memory_type.in_(type_values))
            if include_cross:
                if session_id:
                    from sqlalchemy import or_

                    q = q.filter(
                        or_(M.session_id == session_id, M.session_id.is_(None))
                    )
                # else: no session filter — return memories from all sessions
            else:
                q = q.filter(M.session_id == session_id)
            return q

        if query_text and query_text.strip():
            stats.keyword_attempted = True
            try:
                from matrixone.sqlalchemy_ext import boolean_match

                # Strip characters that MySQL fulltext boolean mode treats as
                # operators or that would break the inlined SQL literal.
                # boolean_match.compile() inlines the text without escaping.
                _safe = query_text.replace("'", "").replace('"', "").replace("\\", "")
                ft = boolean_match("content").must(_safe)
                # compile() returns a complete SQL literal, e.g.
                # "MATCH(content) AGAINST('+term' IN BOOLEAN MODE)"
                ft_sql = ft.compile()
                assert ft_sql.startswith("MATCH("), (
                    f"Unexpected ft.compile() output: {ft_sql!r}"
                )
                ft_score = literal_column(ft_sql).label("ft_score")
                rows = (
                    _base_query()
                    .add_columns(ft_score)
                    .filter(ft)
                    .order_by(rel.desc())
                    .limit(limit)
                    .all()
                )
                if rows:
                    self._metrics.increment("retrieval_keyword_hits")
                    stats.keyword_hit = True
                    return [
                        _Candidate(
                            r.memory_id,
                            r.content,
                            r.memory_type,
                            r.initial_confidence,
                            r.observed_at,
                            r.session_id,
                            trust_tier=r.trust_tier or "T3",
                            keyword_score=float(r.ft_score or 0.0),
                            access_count=r.access_count or 0,
                        )
                        for r in rows
                    ], stats
            except Exception as e:
                logger.debug("Keyword search failed: %s", e)
                self._metrics.increment("retrieval_keyword_errors")
                stats.keyword_error = str(e)

        rows = _base_query().order_by(rel.desc()).limit(limit).all()
        self._metrics.increment("retrieval_fallback_hits")
        return [
            _Candidate(
                r.memory_id,
                r.content,
                r.memory_type,
                r.initial_confidence,
                r.observed_at,
                r.session_id,
                trust_tier=r.trust_tier or "T3",
                access_count=r.access_count or 0,
            )
            for r in rows
        ], stats

    def _phase2(
        self,
        db,
        query_embedding: list[float],
        base_params: dict,
        limit: int,
    ) -> tuple[list[_Candidate], _PhaseStats]:
        stats = _PhaseStats(vector_attempted=True)

        from matrixone.sqlalchemy_ext import l2_distance

        from memoria.core.memory.models.memory import MemoryRecord as M

        dist_expr = l2_distance(M.embedding, query_embedding).label("l2_dist")
        uid = base_params["uid"]
        type_values = base_params["types"]
        session_id = base_params["session_id"]
        include_cross = base_params["include_cross"]

        try:
            q = db.query(
                M.memory_id,
                M.content,
                M.memory_type,
                M.initial_confidence,
                M.observed_at,
                M.session_id,
                M.trust_tier,
                M.access_count,
                dist_expr,
            ).filter(
                M.user_id == uid,
                M.is_active > 0,
                M.memory_type.in_(type_values),
                M.embedding.isnot(None),
            )
            if include_cross:
                if session_id:
                    from sqlalchemy import or_

                    q = q.filter(
                        or_(M.session_id == session_id, M.session_id.is_(None))
                    )
                # else: no session filter — return memories from all sessions
            else:
                q = q.filter(M.session_id == session_id)
            rows = q.order_by("l2_dist").limit(limit).all()
            self._metrics.increment("retrieval_vector_hits")
            stats.vector_hit = bool(rows)
            return [
                _Candidate(
                    r.memory_id,
                    r.content,
                    r.memory_type,
                    r.initial_confidence,
                    r.observed_at,
                    r.session_id,
                    trust_tier=r.trust_tier or "T3",
                    l2_dist=float(r.l2_dist),
                    access_count=r.access_count or 0,
                )
                for r in rows
            ], stats
        except Exception as e:
            logger.warning("Vector search failed: %s", e)
            self._metrics.increment("retrieval_vector_errors")
            stats.vector_error = str(e)
            return [], stats

    def _score_candidate(
        self,
        c: _Candidate,
        weights: RetrievalWeights,
        now_ts: float,
    ) -> tuple[float, float, float, float, float]:
        """Compute 4-dimension scores + weighted final score for a candidate."""
        vec_score = 1.0 / (1.0 + c.l2_dist) if c.l2_dist is not None else 0.0
        # Normalize BM25 score to 0-1 via saturating transform: score/(score+1)
        # BM25 scores are non-negative by definition; clamp to 0 defensively.
        kw_score = (
            max(0.0, c.keyword_score) / (max(0.0, c.keyword_score) + 1.0)
            if c.keyword_score > 0
            else 0.0
        )

        if c.observed_at:
            age_hours = (now_ts - c.observed_at.timestamp()) / 3600.0
            time_score = _safe_exp(-age_hours / self.decay_hours)
            age_days = age_hours / 24.0
            tier_half_life = self._config.half_lives.get(
                c.trust_tier, self.half_life_days
            )
            conf_score = c.initial_confidence * _safe_exp(-age_days / tier_half_life)
        else:
            time_score, conf_score = 0.0, c.initial_confidence

        final = (
            weights.vector * vec_score
            + weights.keyword * kw_score
            + weights.temporal * time_score
            + weights.confidence * conf_score
        )

        # Frequency boost: log(1 + access_count) — mild boost for frequently retrieved memories
        if c.access_count > 0:
            final *= 1.0 + 0.1 * math.log(1 + c.access_count)

        return final, vec_score, kw_score, time_score, conf_score

    def _annotate_scores(
        self,
        candidates: list[_Candidate],
        weights: RetrievalWeights,
        stats: RetrievalStats,
    ) -> None:
        """Compute per-candidate scores for explain mode without re-ranking.

        Used by the keyword-only path where SQL-side ordering must be preserved.
        """
        now_ts = time.time()
        scores = []
        for i, c in enumerate(candidates):
            sc = self._score_candidate(c, weights, now_ts)
            scores.append(
                CandidateScore(
                    memory_id=c.memory_id,
                    final_score=round(sc[0], 4),
                    vector_score=round(sc[1], 4),
                    keyword_score=round(sc[2], 4),
                    temporal_score=round(sc[3], 4),
                    confidence_score=round(sc[4], 4),
                    rank=i + 1,
                )
            )
        stats.candidate_scores = scores

    def _merge(
        self,
        phase1: list[_Candidate],
        phase2: list[_Candidate],
        user_id: str,
        weights: RetrievalWeights,
        limit: int,
        stats: Optional[RetrievalStats] = None,
    ) -> list[Memory]:
        merged: dict[str, _Candidate] = {}
        for c in phase1:
            merged[c.memory_id] = c
        for c in phase2:
            if c.memory_id in merged:
                merged[c.memory_id].l2_dist = c.l2_dist
            else:
                merged[c.memory_id] = c

        if not merged:
            return []

        now_ts = time.time()

        if stats:
            # Compute full breakdown once, sort by final, then extract scores
            scored_full = [
                (self._score_candidate(c, weights, now_ts), c) for c in merged.values()
            ]
            scored_full.sort(key=lambda x: x[0][0], reverse=True)
            selected = scored_full[:limit]
            stats.candidate_scores = [
                CandidateScore(
                    memory_id=c.memory_id,
                    final_score=round(sc[0], 4),
                    vector_score=round(sc[1], 4),
                    keyword_score=round(sc[2], 4),
                    temporal_score=round(sc[3], 4),
                    confidence_score=round(sc[4], 4),
                    rank=i + 1,
                )
                for i, (sc, c) in enumerate(selected)
            ]
            return [self._to_memory(c, user_id) for _, c in selected]

        # Hot path: only compute final score, skip per-dimension breakdown
        scored = [
            (self._score_candidate(c, weights, now_ts)[0], c) for c in merged.values()
        ]
        scored.sort(key=lambda x: x[0], reverse=True)
        return [self._to_memory(c, user_id) for _, c in scored[:limit]]

    @staticmethod
    def _to_memory(c: _Candidate, user_id: str) -> Memory:
        return Memory(
            memory_id=c.memory_id,
            user_id=user_id,
            memory_type=MemoryType(c.memory_type),
            content=c.content,
            initial_confidence=c.initial_confidence,
            access_count=c.access_count,
            session_id=c.session_id,
            observed_at=c.observed_at,
            trust_tier=TrustTier(c.trust_tier)
            if c.trust_tier
            else TrustTier.T3_INFERRED,
        )

    def _bump_access_counts(self, memory_ids: list[str]) -> None:
        """Increment access_count for retrieved memories (best-effort)."""
        if not memory_ids:
            return
        try:
            with self._db() as db:
                placeholders = ",".join(f":id{i}" for i in range(len(memory_ids)))
                params = {f"id{i}": mid for i, mid in enumerate(memory_ids)}
                db.execute(
                    text(
                        f"UPDATE mem_memories SET access_count = access_count + 1 "
                        f"WHERE memory_id IN ({placeholders})"
                    ),
                    params,
                )
                db.commit()
        except Exception:
            logger.debug("Failed to bump access counts", exc_info=True)
