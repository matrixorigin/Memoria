"""TabularMemoryService — tabular backend for memory operations.

Implements MemoryReader, MemoryWriter, and MemoryAdmin protocols using
flat tables with vector/fulltext hybrid retrieval.

See docs/design/memory/tabular-memory.md
"""

from __future__ import annotations

import logging
from datetime import timedelta
from typing import TYPE_CHECKING, Any

from memoria.core.memory.interfaces import GovernanceReport, HealthReport
from memoria.core.memory.tabular.metrics import MemoryMetrics
from memoria.core.memory.types import (
    Memory,
    MemoryType,
    RetrievalWeights,
    TrustTier,
    _utcnow,
)

if TYPE_CHECKING:
    from memoria.core.db_consumer import DbFactory
    from memoria.core.memory.config import MemoryGovernanceConfig

logger = logging.getLogger(__name__)


class TabularMemoryService:
    """Tabular memory backend — flat table + vector/fulltext retrieval.

    Wraps internal components (MemoryStore, MemoryRetriever, ProfileManager,
    TypedObserver, GovernanceScheduler, MemoryHealth) behind Protocol interfaces.
    """

    def __init__(
        self,
        db_factory: DbFactory,
        llm_client: Any = None,
        embed_fn: Any = None,
        config: MemoryGovernanceConfig | None = None,
        metrics: MemoryMetrics | None = None,
    ):
        self._db_factory = db_factory
        self._llm_client = llm_client
        self._embed_fn = embed_fn
        self._metrics = metrics or MemoryMetrics()

        # Lazy-initialized internal components (use properties below)
        self._store: Any = None
        self._retriever: Any = None
        self._profile_mgr: Any = None
        self._observer: Any = None
        self._governance: Any = None
        self._health: Any = None
        self._summarizer: Any = None

        # Deferred config import
        if config is None:
            from memoria.core.memory.config import DEFAULT_CONFIG

            self._config = DEFAULT_CONFIG
        else:
            self._config = config

    # ── Lazy-initialized component properties ─────────────────────────
    # Named as nouns (what they are), not verbs (what they do).

    @property
    def _store_lazy(self) -> Any:
        if self._store is None:
            from memoria.core.memory.tabular.store import MemoryStore

            self._store = MemoryStore(self._db_factory, metrics=self._metrics)
        return self._store

    @property
    def _retriever_lazy(self) -> Any:
        if self._retriever is None:
            from memoria.core.memory.tabular.retriever import MemoryRetriever

            self._retriever = MemoryRetriever(
                self._db_factory, metrics=self._metrics, config=self._config
            )
        return self._retriever

    @property
    def _profile_mgr_lazy(self) -> Any:
        if self._profile_mgr is None:
            from memoria.core.memory.tabular.profile import ProfileManager

            self._profile_mgr = ProfileManager(self._store_lazy)
        return self._profile_mgr

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
        """Retrieve memories ranked by hybrid relevance."""
        return self._retriever_lazy.retrieve(
            user_id=user_id,
            query_text=query,
            session_id=session_id,
            query_embedding=query_embedding,
            memory_types=memory_types,
            limit=top_k,
            task_hint=task_hint,
            weights=weights,
            include_cross_session=include_cross_session,
            explain=explain,
        )

    def get_profile(self, user_id: str) -> str | None:
        """Get synthesized user profile string."""
        profile = self._profile_mgr_lazy.get_profile(user_id)
        return profile or None

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
        extra_metadata: dict[str, Any] | None = None,
        generate_embedding: bool = True,
    ) -> str:
        """Store a single memory with contradiction detection.

        Returns:
            memory_id of the created memory
        """
        mem, _ = self._observer_lazy.observe_explicit(
            user_id=user_id,
            content=content,
            memory_type=memory_type,
            initial_confidence=initial_confidence,
            source_event_ids=source_event_ids,
            trust_tier=trust_tier,
            session_id=session_id,
            extra_metadata=extra_metadata,
            generate_embedding=generate_embedding,
        )
        return mem.memory_id

    def observe_turn(
        self,
        user_id: str,
        messages: list[dict[str, Any]],
        *,
        source_event_ids: list[str] | None = None,
        session_id: str | None = None,
        turn_count: int = 0,
    ) -> list[Memory]:
        """Extract and persist memories from a conversation turn.

        If session_id and turn_count are provided, auto-triggers a lightweight
        episodic summary when turn_count reaches the configured threshold.
        """
        memories, _ = self._observer_lazy.observe(
            user_id=user_id,
            messages=messages,
            source_event_ids=source_event_ids,
        )

        # Auto-trigger lightweight episodic summary
        if session_id and turn_count > 0 and self._llm_client is not None:
            threshold = self._config.auto_trigger_threshold
            if threshold > 0 and turn_count % threshold == 0:
                self._trigger_lightweight_summary_async(user_id, session_id, messages)

        return memories

    def _trigger_lightweight_summary_async(
        self,
        user_id: str,
        session_id: str,
        messages: list[dict[str, Any]],
    ) -> None:
        """Fire-and-forget lightweight episodic summary in background thread."""
        import threading

        from memoria.core.memory.episodic import generate_lightweight_summary
        from memoria.core.memory.episodic.tasks import get_task_manager
        from memoria.core.memory.types import MemoryType

        task_manager = get_task_manager()
        if not task_manager.check_lightweight_rate_limit(session_id):
            logger.debug(
                "Auto-trigger skipped: lightweight rate limit reached for session %s",
                session_id,
            )
            return
        # Increment before spawning thread to prevent concurrent over-triggering
        task_manager.increment_lightweight_count(session_id)

        llm_client = self._llm_client
        embed_fn = self._embed_fn

        def _run() -> None:
            try:
                points, _ = generate_lightweight_summary(messages, llm_client)
                content = "Session Highlights:\n" + "\n".join(f"• {p}" for p in points)
                self.store(
                    user_id=user_id,
                    content=content,
                    memory_type=MemoryType.EPISODIC,
                    session_id=None,
                    extra_metadata={
                        "mode": "lightweight",
                        "points": points,
                        "auto_triggered": True,
                    },
                    generate_embedding=embed_fn is not None,
                )
                logger.debug(
                    "Auto-triggered lightweight summary for session %s", session_id
                )
            except Exception as e:
                logger.warning(
                    "Auto-trigger lightweight summary failed for session %s: %s",
                    session_id,
                    e,
                )

        threading.Thread(target=_run, daemon=True).start()

    def run_pipeline(
        self,
        user_id: str,
        messages: list[dict[str, Any]],
        *,
        source_event_ids: list[str] | None = None,
    ) -> Any:
        """Run full typed memory pipeline (extract → sandbox → persist → profile).

        Unlike observe_turn() which only extracts+persists, this runs the
        complete pipeline including sandbox validation and profile update.
        """
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

    def invalidate_profile(self, user_id: str) -> None:
        """Invalidate cached profile after profile memory changes."""
        self._profile_mgr_lazy.invalidate(user_id)

    def generate_session_summary(
        self,
        user_id: str,
        session_id: str,
        messages: list[dict[str, Any]],
    ) -> Memory | None:
        """Generate full session summary on close."""
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
        """Check thresholds and generate incremental summary if needed."""
        return self._summarizer_lazy.check_and_summarize(
            user_id,
            session_id,
            messages,
            turn_count,
            session_start,
        )

    # ── MemoryAdmin ───────────────────────────────────────────────────

    def run_governance(self, user_id: str) -> GovernanceReport:
        """Run all governance cycles for a user."""
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

    def health_check(self, user_id: str) -> HealthReport:
        """Get memory health analytics."""
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

    # ── Low-level CRUD (for Tool Context Engine) ──────────────────────

    def create_memory(self, memory: Memory) -> Memory:
        """Create a memory record directly (bypasses Observer pipeline).

        Use for TOOL_RESULT and other programmatic writes that don't need
        LLM extraction or contradiction detection.
        """
        return self._store_lazy.create(memory)

    def get_memory(self, memory_id: str) -> Memory | None:
        """Get a single memory by ID."""
        return self._store_lazy.get(memory_id)

    def update_memory_content(self, memory_id: str, content: str) -> None:
        """Update content of an existing memory (e.g. streaming accumulation)."""
        self._store_lazy.update_content(memory_id, content)

    def list_active(
        self,
        user_id: str,
        memory_type: MemoryType | None = None,
        limit: int | None = None,
        load_embedding: bool = True,
    ) -> list[Memory]:
        """List active memories, optionally filtered by type."""
        return self._store_lazy.list_active(
            user_id,
            memory_type=memory_type,
            limit=limit,
            load_embedding=load_embedding,
        )

    # ── Governance (scheduler-level) ──────────────────────────────────

    def run_hourly(self) -> GovernanceReport:
        """Run hourly governance tasks (tool_result cleanup, working archive)."""
        r = self._governance_lazy.run_hourly()
        return GovernanceReport(
            cleaned_tool_results=r.cleaned_tool_results,
            archived_working=r.archived_working,
            errors=r.errors,
            total_ms=r.total_ms,
        )

    def run_daily_all(self) -> GovernanceReport:
        """Run daily governance tasks for all users (stale cleanup, quarantine)."""
        r = self._governance_lazy.run_daily_all()
        return GovernanceReport(
            cleaned_stale=r.cleaned_stale,
            quarantined=r.quarantined,
            errors=r.errors,
            total_ms=r.total_ms,
        )

    def run_weekly(self) -> GovernanceReport:
        """Run weekly governance tasks (branch/snapshot cleanup)."""
        r = self._governance_lazy.run_weekly()
        return GovernanceReport(
            cleaned_branches=r.cleaned_branches,
            cleaned_snapshots=r.cleaned_snapshots,
            errors=r.errors,
            total_ms=r.total_ms,
        )


# Backward-compat aliases — new code should use core.memory.service.MemoryService
# or core.memory.create_memory_service() instead.
MemoryService = TabularMemoryService
