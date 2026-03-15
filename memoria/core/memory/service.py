"""MemoryService — facade composing CanonicalStorage + RetrievalStrategy + IndexManager.

This replaces both TabularMemoryService and GraphMemoryService.

See docs/design/memory/backend-management.md §2
"""

from __future__ import annotations

import logging
from typing import TYPE_CHECKING, Any

if TYPE_CHECKING:
    from memoria.core.db_consumer import DbFactory
    from memoria.core.memory.canonical_storage import CanonicalStorage
    from memoria.core.memory.editor import MemoryEditor
    from memoria.core.memory.interfaces import (
        GovernanceReport,
        HealthReport,
        ReflectionCandidate,
    )
    from memoria.core.memory.strategy.protocol import IndexManager, RetrievalStrategy
    from memoria.core.memory.types import Memory, MemoryType, RetrievalWeights

from memoria.core.memory.types import TrustTier

logger = logging.getLogger(__name__)


class MemoryService:
    """Unified memory service: canonical storage + pluggable retrieval.

    Implements MemoryReader, MemoryWriter, MemoryAdmin protocols.
    Replaces TabularMemoryService and GraphMemoryService.
    """

    def __init__(
        self,
        storage: CanonicalStorage,
        retrieval: RetrievalStrategy,
        index_manager: IndexManager | None = None,
        db_factory: DbFactory | None = None,
    ) -> None:
        self._storage = storage
        self._retrieval = retrieval
        self._index_manager = index_manager
        self._db_factory = db_factory
        self._editor: MemoryEditor | None = None

    @property
    def storage(self) -> CanonicalStorage:
        """Access canonical storage directly (for advanced use)."""
        return self._storage

    @property
    def editor(self) -> MemoryEditor:
        """Lazy-initialized MemoryEditor for inject/correct/purge."""
        if self._editor is None:
            from memoria.core.memory.editor import MemoryEditor as _Editor

            if self._db_factory is None:
                raise RuntimeError(
                    "MemoryService needs db_factory for editor operations"
                )
            self._editor = _Editor(self._storage, self._db_factory, self._index_manager)
        return self._editor

    @property
    def strategy_key(self) -> str:
        """Current retrieval strategy key."""
        return self._retrieval.strategy_key

    # ── MemoryReader ──────────────────────────────────────────────────

    def retrieve(
        self,
        user_id: str,
        query: str,
        *,
        session_id: str = "",
        query_embedding: list[float] | None = None,
        memory_types: list[MemoryType] | None = None,
        top_k: int = 10,
        task_hint: str | None = None,
        weights: RetrievalWeights | None = None,
        include_cross_session: bool = True,
        explain: bool = False,
    ) -> tuple[list[Memory], Any]:
        """Retrieve memories via the active retrieval strategy."""
        return self._retrieval.retrieve(
            user_id,
            query,
            query_embedding,
            top_k=top_k,
            task_type=task_hint,
            session_id=session_id,
            memory_types=memory_types,
            weights=weights,
            include_cross_session=include_cross_session,
            explain=explain,
        )

    def get_profile(self, user_id: str) -> str | None:
        return self._storage.get_profile(user_id)

    # ── MemoryWriter ──────────────────────────────────────────────────

    def store(
        self,
        user_id: str,
        content: str,
        *,
        memory_type: MemoryType,
        source_event_ids: list[str] | None = None,
        initial_confidence: float = 0.75,
        trust_tier: TrustTier = TrustTier.T3_INFERRED,
        session_id: str | None = None,
    ) -> Memory:
        """Store memory in canonical storage, then update index."""
        mem = self._storage.store(
            user_id,
            content,
            memory_type=memory_type,
            source_event_ids=source_event_ids,
            initial_confidence=initial_confidence,
            trust_tier=trust_tier,
            session_id=session_id,
        )
        if self._index_manager:
            self._index_manager.on_memories_stored(
                user_id,
                [mem],
                session_id=session_id,
            )
        return mem

    def observe_turn(
        self,
        user_id: str,
        messages: list[dict[str, Any]],
        *,
        source_event_ids: list[str] | None = None,
        session_id: str | None = None,
    ) -> list[Memory]:
        """Extract memories from turn, then update index."""
        memories = self._storage.observe_turn(
            user_id,
            messages,
            source_event_ids=source_event_ids,
            session_id=session_id,
        )
        if self._index_manager and memories:
            sid = session_id or (memories[0].session_id if memories else None)
            self._index_manager.on_memories_stored(
                user_id,
                memories,
                session_id=sid,
            )
        return memories

    def run_pipeline(
        self,
        user_id: str,
        messages: list[dict[str, Any]],
        *,
        source_event_ids: list[str] | None = None,
    ) -> Any:
        return self._storage.run_pipeline(
            user_id,
            messages,
            source_event_ids=source_event_ids,
        )

    def invalidate_profile(self, user_id: str) -> None:
        self._storage.invalidate_profile(user_id)

    def generate_session_summary(
        self,
        user_id: str,
        session_id: str,
        messages: list[dict[str, Any]],
    ) -> Memory | None:
        return self._storage.generate_session_summary(user_id, session_id, messages)

    def check_and_summarize(
        self,
        user_id: str,
        session_id: str,
        messages: list[dict[str, Any]],
        turn_count: int,
        session_start: Any,
    ) -> Memory | None:
        return self._storage.check_and_summarize(
            user_id,
            session_id,
            messages,
            turn_count,
            session_start,
        )

    # ── MemoryAdmin ───────────────────────────────────────────────────

    def run_governance(self, user_id: str) -> GovernanceReport:
        report = self._storage.run_governance(user_id)
        if self._index_manager:
            self._index_manager.on_governance(user_id)
        return report

    def health_check(self, user_id: str) -> HealthReport:
        return self._storage.health_check(user_id)

    def run_hourly(self) -> GovernanceReport:
        return self._storage.run_hourly()

    def run_daily_all(self) -> GovernanceReport:
        return self._storage.run_daily_all()

    def run_weekly(self) -> GovernanceReport:
        return self._storage.run_weekly()

    # ── CandidateProvider ─────────────────────────────────────────────

    def get_reflection_candidates(
        self,
        user_id: str,
        *,
        since_hours: int = 24,
    ) -> list[ReflectionCandidate]:
        """Get reflection candidates from index manager or canonical fallback."""
        if self._index_manager:
            result = self._index_manager.get_reflection_candidates(
                user_id,
                since_hours=since_hours,
            )
            if result is not None:
                return result

        return self._storage.get_reflection_candidates(
            user_id,
            since_hours=since_hours,
        )

    # ── Low-level CRUD (for Tool Context Engine) ──────────────────────

    def create_memory(self, memory: Memory) -> Memory:
        return self._storage.create_memory(memory)

    def get_memory(self, memory_id: str) -> Memory | None:
        return self._storage.get_memory(memory_id)

    def update_memory_content(self, memory_id: str, content: str) -> None:
        self._storage.update_memory_content(memory_id, content)

    def update_memory_embedding(self, memory_id: str) -> None:
        self._storage.update_memory_embedding(memory_id)

    def list_active(
        self,
        user_id: str,
        memory_type: MemoryType | None = None,
        limit: int | None = None,
        load_embedding: bool = True,
    ) -> list[Memory]:
        return self._storage.list_active(
            user_id,
            memory_type=memory_type,
            limit=limit,
            load_embedding=load_embedding,
        )

    # ── Graph-specific (backward compat) ──────────────────────────────

    def get_graph_stats(self, user_id: str) -> dict[str, int]:
        """Get graph stats if using activation strategy."""
        get_stats = getattr(self._index_manager, "get_graph_stats", None)
        if get_stats:
            return get_stats(user_id)
        return {"total_nodes": 0}

    def consolidate(self, user_id: str) -> Any:
        """Run graph consolidation directly (for testing/admin)."""
        do_consolidate = getattr(self._index_manager, "consolidate", None)
        if do_consolidate:
            return do_consolidate(user_id)
        return None
