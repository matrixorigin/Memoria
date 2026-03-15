"""CanonicalStorage — shared storage layer for all retrieval strategies.

Extracted from TabularMemoryService: store, observe_turn, profile, governance, health.
This is the single source of truth for mem_memories.

See docs/design/memory/backend-management.md §2.1
"""

from __future__ import annotations

import logging
from datetime import timedelta
from typing import TYPE_CHECKING, Any

from memoria.core.memory.interfaces import GovernanceReport, HealthReport
from memoria.core.memory.tabular.metrics import MemoryMetrics
from memoria.core.memory.types import Memory, MemoryType, TrustTier, _utcnow

if TYPE_CHECKING:
    from memoria.core.db_consumer import DbFactory
    from memoria.core.memory.config import MemoryGovernanceConfig

logger = logging.getLogger(__name__)


class CanonicalStorage:
    """Canonical storage on mem_memories — shared by all strategies.

    Handles: store, observe_turn, profile, governance, health, session summary.
    Does NOT handle retrieval — that's the strategy's job.
    """

    def __init__(
        self,
        db_factory: DbFactory,
        *,
        llm_client: Any = None,
        embed_fn: Any = None,
        config: MemoryGovernanceConfig | None = None,
        metrics: MemoryMetrics | None = None,
    ) -> None:
        self._db_factory = db_factory
        self._llm_client = llm_client
        self._embed_fn = embed_fn
        self._metrics = metrics or MemoryMetrics()

        if config is None:
            from memoria.core.memory.config import DEFAULT_CONFIG

            self._config = DEFAULT_CONFIG
        else:
            self._config = config

        # Lazy-initialized components
        self._store: Any = None
        self._observer: Any = None
        self._profile_mgr: Any = None
        self._governance: Any = None
        self._health: Any = None
        self._summarizer: Any = None

    # ── Lazy properties ───────────────────────────────────────────────

    @property
    def _store_lazy(self) -> Any:
        if self._store is None:
            from memoria.core.memory.tabular.store import MemoryStore

            self._store = MemoryStore(self._db_factory, metrics=self._metrics)
        return self._store

    @property
    def _observer_lazy(self) -> Any:
        if self._observer is None:
            from memoria.core.memory.tabular.typed_observer import TypedObserver

            self._observer = TypedObserver(
                store=self._store_lazy,
                llm_client=self._llm_client,
                embed_fn=self._embed_fn,
                db_factory=self._db_factory,
                metrics=self._metrics,
            )
        return self._observer

    @property
    def _profile_mgr_lazy(self) -> Any:
        if self._profile_mgr is None:
            from memoria.core.memory.tabular.profile import ProfileManager

            self._profile_mgr = ProfileManager(self._store_lazy)
        return self._profile_mgr

    @property
    def _governance_lazy(self) -> Any:
        if self._governance is None:
            from memoria.core.memory.tabular.governance import GovernanceScheduler

            self._governance = GovernanceScheduler(
                self._db_factory,
                config=self._config,
                metrics=self._metrics,
                llm_client=self._llm_client,
                embed_fn=self._embed_fn,
            )
        return self._governance

    @property
    def _health_lazy(self) -> Any:
        if self._health is None:
            from memoria.core.memory.tabular.health import MemoryHealth

            self._health = MemoryHealth(
                self._db_factory,
                pollution_threshold=self._config.pollution_threshold,
            )
        return self._health

    @property
    def _summarizer_lazy(self) -> Any:
        if self._summarizer is None:
            from memoria.core.memory.tabular.session_summary import SessionSummarizer

            self._summarizer = SessionSummarizer(
                store=self._store_lazy,
                llm_client=self._llm_client,
                embed_fn=self._embed_fn,
                config=self._config,
            )
        return self._summarizer

    # ── Write path ────────────────────────────────────────────────────

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
        """Store a single memory directly — no contradiction detection.

        Admin inject should always create a new record regardless of similarity.
        Contradiction detection is for observe_turn (LLM-extracted memories).
        Sensitivity filter is still applied (blocks PII/credentials).
        """
        import uuid

        from memoria.core.memory.tabular.sensitivity import check_sensitivity

        sensitivity = check_sensitivity(content)
        if sensitivity.blocked:
            raise ValueError(
                f"Content blocked by sensitivity filter: {sensitivity.matched_labels}"
            )
        if sensitivity.redacted_content is not None:
            content = sensitivity.redacted_content

        mem = Memory(
            memory_id=uuid.uuid4().hex,
            user_id=user_id,
            content=content,
            memory_type=memory_type,
            trust_tier=trust_tier,
            initial_confidence=initial_confidence,
            source_event_ids=source_event_ids or [],
            session_id=session_id,
            observed_at=_utcnow(),
        )
        if self._embed_fn is not None:
            try:
                emb = self._embed_fn(content)
                if emb is not None:
                    mem.embedding = emb
            except Exception:
                logger.warning("Embedding failed in store()", exc_info=True)
        return self._store_lazy.create(mem)

    def batch_store(
        self,
        memories: list[Memory],
    ) -> list[Memory]:
        """Batch-store pre-built Memory objects. Single transaction, no contradiction check.

        Caller is responsible for building Memory objects (with embeddings if needed).
        Skips contradiction detection — appropriate for admin/curated batch inject.
        """
        return self._store_lazy.batch_create(memories)

    def observe_turn(
        self,
        user_id: str,
        messages: list[dict[str, Any]],
        *,
        source_event_ids: list[str] | None = None,
        session_id: str | None = None,
    ) -> list[Memory]:
        """Extract and persist memories from a conversation turn."""
        memories, _ = self._observer_lazy.observe(
            user_id=user_id,
            messages=messages,
            source_event_ids=source_event_ids,
            session_id=session_id,
        )
        return memories

    def run_pipeline(
        self,
        user_id: str,
        messages: list[dict[str, Any]],
        *,
        source_event_ids: list[str] | None = None,
    ) -> Any:
        """Run full typed memory pipeline."""
        from memoria.core.memory.tabular.typed_pipeline import run_typed_memory_pipeline

        return run_typed_memory_pipeline(
            db_factory=self._db_factory,
            user_id=user_id,
            messages=messages,
            source_event_ids=source_event_ids,
            llm_client=self._llm_client,
            embed_fn=self._embed_fn,
            metrics=self._metrics,
        )

    # ── Profile ───────────────────────────────────────────────────────

    def get_profile(self, user_id: str) -> str | None:
        profile = self._profile_mgr_lazy.get_profile(user_id)
        return profile or None

    def invalidate_profile(self, user_id: str) -> None:
        self._profile_mgr_lazy.invalidate(user_id)

    # ── Session summary ───────────────────────────────────────────────

    def generate_session_summary(
        self,
        user_id: str,
        session_id: str,
        messages: list[dict[str, Any]],
    ) -> Memory | None:
        return self._summarizer_lazy.generate_full_summary(
            user_id, session_id, messages
        )

    def check_and_summarize(
        self,
        user_id: str,
        session_id: str,
        messages: list[dict[str, Any]],
        turn_count: int,
        session_start: Any,
    ) -> Memory | None:
        return self._summarizer_lazy.check_and_summarize(
            user_id,
            session_id,
            messages,
            turn_count,
            session_start,
        )

    # ── Governance ────────────────────────────────────────────────────

    def run_governance(self, user_id: str) -> GovernanceReport:
        result = self._governance_lazy.run_cycle(user_id)
        return GovernanceReport(
            cleaned_tool_results=result.cleaned_tool_results,
            archived_working=result.archived_working,
            cleaned_stale=result.cleaned_stale,
            quarantined=result.quarantined,
            scenes_created=result.scenes_created,
            cleaned_branches=result.cleaned_branches,
            cleaned_snapshots=result.cleaned_snapshots,
            compressed_redundant=result.compressed_redundant,
            pollution_detected=result.pollution_detected,
            errors=result.errors,
            total_ms=result.total_ms,
        )

    def run_hourly(self) -> GovernanceReport:
        r = self._governance_lazy.run_hourly()
        return GovernanceReport(
            cleaned_tool_results=r.cleaned_tool_results,
            archived_working=r.archived_working,
            errors=r.errors,
            total_ms=r.total_ms,
        )

    def run_daily_all(self) -> GovernanceReport:
        r = self._governance_lazy.run_daily_all()
        return GovernanceReport(
            cleaned_stale=r.cleaned_stale,
            quarantined=r.quarantined,
            compressed_redundant=r.compressed_redundant,
            errors=r.errors,
            total_ms=r.total_ms,
        )

    def run_weekly(self) -> GovernanceReport:
        r = self._governance_lazy.run_weekly()
        return GovernanceReport(
            cleaned_branches=r.cleaned_branches,
            cleaned_snapshots=r.cleaned_snapshots,
            errors=r.errors,
            total_ms=r.total_ms,
        )

    # ── Health ────────────────────────────────────────────────────────

    def health_check(self, user_id: str) -> HealthReport:
        storage = self._health_lazy.get_storage_stats(user_id)
        per_type = self._health_lazy.analyze(user_id)
        pollution = self._health_lazy.detect_pollution(
            user_id,
            _utcnow() - timedelta(days=1),
        )
        return HealthReport(
            total=storage.get("total", 0),
            active=storage.get("active", 0),
            inactive=storage.get("inactive", 0),
            avg_content_size=storage.get("avg_content_size", 0.0),
            per_type_stats=per_type,
            pollution=pollution,
        )

    # ── Low-level CRUD ────────────────────────────────────────────────

    def create_memory(self, memory: Memory) -> Memory:
        """Direct create bypassing Observer pipeline.

        Auto-generates embedding if ``_embed_fn`` is configured and
        ``memory.embedding`` is None.  Mutates the input object in-place.
        """
        if memory.embedding is None and self._embed_fn is not None:
            try:
                emb = self._embed_fn(memory.content)
                if emb is not None:
                    memory.embedding = emb
            except Exception:
                logger.warning("Embedding failed in create_memory", exc_info=True)
        return self._store_lazy.create(memory)

    def get_memory(self, memory_id: str) -> Memory | None:
        return self._store_lazy.get(memory_id)

    def update_memory_content(self, memory_id: str, content: str) -> None:
        # Content-only update. Used by streaming accumulator for intermediate
        # flushes — re-embedding on every flush would be wasteful and the two
        # writes (content + embedding) would be in separate transactions.
        # Callers that need an up-to-date embedding after a content change
        # should call update_memory_embedding() explicitly.
        self._store_lazy.update_content(memory_id, content)

    def update_memory_embedding(self, memory_id: str) -> None:
        """Re-generate and persist embedding for an existing memory."""
        if self._embed_fn is None:
            return
        mem = self._store_lazy.get(memory_id)
        if mem is None:
            logger.debug("update_memory_embedding: memory %s not found", memory_id)
            return
        try:
            embedding = self._embed_fn(mem.content)
            if embedding is not None:
                self._store_lazy.update_embedding(memory_id, embedding)
        except Exception:
            logger.warning("Embedding failed in update_memory_embedding", exc_info=True)

    def list_active(
        self,
        user_id: str,
        memory_type: MemoryType | None = None,
        limit: int | None = None,
        load_embedding: bool = True,
    ) -> list[Memory]:
        return self._store_lazy.list_active(
            user_id,
            memory_type=memory_type,
            limit=limit,
            load_embedding=load_embedding,
        )

    # ── Reflection candidates ─────────────────────────────────────────

    def get_reflection_candidates(
        self,
        user_id: str,
        *,
        since_hours: int = 24,
    ) -> Any:
        """Get reflection candidates from governance scheduler."""
        return self._governance_lazy.get_reflection_candidates(
            user_id,
            since_hours=since_hours,
        )
