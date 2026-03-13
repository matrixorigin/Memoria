"""GraphMemoryService — graph backend for memory operations.

Implements MemoryReader, MemoryWriter, MemoryAdmin, and CandidateProvider
protocols using a typed directed graph with adjacency-list storage.

See docs/design/memory/graph-memory.md
"""

from __future__ import annotations

import logging
from typing import TYPE_CHECKING, Any

from sqlalchemy.exc import SQLAlchemyError

from memoria.core.memory.graph.candidates import GraphCandidateProvider
from memoria.core.memory.graph.consolidation import (
    ConsolidationResult,
    GraphConsolidator,
)
from memoria.core.memory.graph.graph_builder import GraphBuilder
from memoria.core.memory.graph.graph_store import GraphStore
from memoria.core.memory.graph.retriever import ActivationRetriever
from memoria.core.memory.interfaces import (
    GovernanceReport,
    HealthReport,
    ReflectionCandidate,
)
from memoria.core.memory.types import Memory, MemoryType, RetrievalWeights, TrustTier

if TYPE_CHECKING:
    from memoria.core.db_consumer import DbFactory
    from memoria.core.memory.config import MemoryGovernanceConfig

logger = logging.getLogger(__name__)

# Exceptions that indicate infrastructure/transient failures (recoverable).
# Programming errors (TypeError, ValueError, KeyError, AttributeError) must
# NOT be caught — they indicate bugs that need fixing.
_RECOVERABLE = (SQLAlchemyError, OSError, ConnectionError, TimeoutError)


class GraphMemoryService:
    """Graph memory backend — typed directed graph with adjacency-list storage.

    Write path: tabular + graph ingest (dual-write).
    Read path: activation retrieval with tabular fallback.
    Governance: tabular governance + graph consolidation + graph candidates.
    """

    def __init__(
        self,
        db_factory: DbFactory,
        llm_client: Any = None,
        embed_fn: Any = None,
        config: MemoryGovernanceConfig | None = None,
    ) -> None:
        self._db_factory = db_factory
        self._llm_client = llm_client
        self._embed_fn = embed_fn

        if config is None:
            from memoria.core.memory.config import DEFAULT_CONFIG

            self._config = DEFAULT_CONFIG
        else:
            self._config = config

        # Lazy-initialized components
        self._graph_store: GraphStore | None = None
        self._graph_builder: GraphBuilder | None = None
        self._activation_retriever: ActivationRetriever | None = None
        self._graph_candidates: GraphCandidateProvider | None = None
        self._graph_consolidator: GraphConsolidator | None = None
        self._tabular: Any = None

        # Pending graph syncs — memory_ids where tabular succeeded but graph failed.
        # Drained by run_governance() or explicit retry.
        self._pending_graph_sync: list[str] = []

    # ── Lazy-initialized components ───────────────────────────────────

    @property
    def _store(self) -> GraphStore:
        if self._graph_store is None:
            self._graph_store = GraphStore(self._db_factory)
        return self._graph_store

    @property
    def _builder(self) -> GraphBuilder:
        if self._graph_builder is None:
            self._graph_builder = GraphBuilder(
                self._store,
                config=self._config,
                embed_fn=self._embed_fn,
            )
        return self._graph_builder

    @property
    def _retriever(self) -> ActivationRetriever:
        if self._activation_retriever is None:
            self._activation_retriever = ActivationRetriever(
                self._store, config=self._config
            )
        return self._activation_retriever

    @property
    def _candidates(self) -> GraphCandidateProvider:
        if self._graph_candidates is None:
            self._graph_candidates = GraphCandidateProvider(
                self._db_factory,
                config=self._config,
            )
        return self._graph_candidates

    @property
    def _consolidator(self) -> GraphConsolidator:
        if self._graph_consolidator is None:
            self._graph_consolidator = GraphConsolidator(
                self._db_factory, config=self._config
            )
        return self._graph_consolidator

    @property
    def _tabular_delegate(self) -> Any:
        """Tabular backend for dual-write and fallback."""
        if self._tabular is None:
            from memoria.core.memory.tabular.service import TabularMemoryService

            self._tabular = TabularMemoryService(
                self._db_factory,
                llm_client=self._llm_client,
                embed_fn=self._embed_fn,
                config=self._config,
            )
        return self._tabular

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
    ) -> list[Memory]:
        """Retrieve memories. Activation retrieval with tabular fallback."""
        if query_embedding:
            try:
                activated = self._retriever.retrieve(
                    user_id,
                    query,
                    query_embedding,
                    top_k=top_k,
                    task_type=task_hint,
                )
                if activated:
                    return self._nodes_to_memories(activated)
            except _RECOVERABLE:
                logger.warning(
                    "Activation retrieval failed, falling back to tabular",
                    exc_info=True,
                )

        result = self._tabular_delegate.retrieve(
            user_id,
            query,
            session_id=session_id,
            query_embedding=query_embedding,
            memory_types=memory_types,
            top_k=top_k,
            task_hint=task_hint,
            weights=weights,
            include_cross_session=include_cross_session,
        )
        return result[0] if isinstance(result, tuple) else result

    @staticmethod
    def _nodes_to_memories(scored_nodes: list[tuple[Any, float]]) -> list[Memory]:
        """Convert scored graph nodes to Memory domain objects."""
        memories: list[Memory] = []
        for node, _score in scored_nodes:
            try:
                tier = TrustTier(node.trust_tier)
            except ValueError:
                tier = TrustTier.T3_INFERRED
            memories.append(
                Memory(
                    memory_id=node.memory_id or node.node_id,
                    user_id=node.user_id,
                    memory_type=MemoryType.SEMANTIC,
                    content=node.content,
                    initial_confidence=node.confidence,
                    embedding=node.embedding,
                    session_id=node.session_id,
                    trust_tier=tier,
                )
            )
        return memories

    def get_profile(self, user_id: str) -> str | None:
        return self._tabular_delegate.get_profile(user_id)

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
        """Store a memory and build graph nodes.

        Dual-write: tabular is source of truth. If graph ingest fails due to
        a transient error, the memory_id is queued for retry in governance.
        Programming errors (TypeError, etc.) are NOT caught.
        """
        mem = self._tabular_delegate.store(
            user_id,
            content,
            memory_type=memory_type,
            source_event_ids=source_event_ids,
            initial_confidence=initial_confidence,
            trust_tier=trust_tier,
            session_id=session_id,
        )
        try:
            created = self._builder.ingest(user_id, [mem], [], session_id=session_id)
            self._run_opinion_evolution(user_id, created)
        except _RECOVERABLE:
            logger.warning(
                "Graph ingest failed for memory %s, queued for retry",
                mem.memory_id,
                exc_info=True,
            )
            self._pending_graph_sync.append(mem.memory_id)
        return mem

    def observe_turn(
        self,
        user_id: str,
        messages: list[dict[str, Any]],
        *,
        source_event_ids: list[str] | None = None,
    ) -> list[Memory]:
        """Extract memories from a turn and build graph."""
        memories = self._tabular_delegate.observe_turn(
            user_id,
            messages,
            source_event_ids=source_event_ids,
        )
        events = [
            {"event_id": eid, "event_type": "unknown"}
            for eid in (source_event_ids or [])
        ]
        try:
            session_id = memories[0].session_id if memories else None
            created = self._builder.ingest(
                user_id, memories, events, session_id=session_id
            )
            self._run_opinion_evolution(user_id, created)
        except _RECOVERABLE:
            logger.warning(
                "Graph ingest failed for turn, queued for retry", exc_info=True
            )
            self._pending_graph_sync.extend(m.memory_id for m in memories)
        return memories

    def _run_opinion_evolution(
        self,
        user_id: str,
        created_nodes: list[Any],
    ) -> None:
        """Run opinion evolution for newly created nodes with embeddings."""
        from memoria.core.memory.graph.opinion import evolve_opinions

        for node in created_nodes:
            if not node.embedding:
                continue
            try:
                result = evolve_opinions(
                    self._store, node.node_id, user_id, self._config
                )
                if result.scenes_evaluated:
                    logger.debug(
                        "Opinion evolution for %s: %d scenes, %d supporting, %d contradicting, %d quarantined",
                        node.node_id,
                        result.scenes_evaluated,
                        result.supporting,
                        result.contradicting,
                        result.quarantined,
                    )
            except _RECOVERABLE:
                logger.warning(
                    "Opinion evolution failed for node %s", node.node_id, exc_info=True
                )

    # ── Pending sync ──────────────────────────────────────────────────

    @property
    def pending_graph_sync_count(self) -> int:
        """Number of memories pending graph sync."""
        return len(self._pending_graph_sync)

    def drain_pending_graph_sync(self) -> list[str]:
        """Return and clear pending memory_ids. Called by governance."""
        ids = self._pending_graph_sync[:]
        self._pending_graph_sync.clear()
        return ids

    # ── MemoryAdmin ───────────────────────────────────────────────────

    def run_governance(self, user_id: str) -> GovernanceReport:
        """Run tabular governance + graph consolidation + pending sync retry."""
        report = self._tabular_delegate.run_governance(user_id)

        # Retry pending graph syncs
        pending = self.drain_pending_graph_sync()
        if pending:
            logger.info("Retrying graph sync for %d memories", len(pending))
            retried = 0
            still_pending = []
            for mid in pending:
                mem = self._tabular_delegate.get_memory(mid)
                if not mem:
                    continue
                try:
                    created = self._builder.ingest(
                        user_id, [mem], [], session_id=mem.session_id
                    )
                    self._run_opinion_evolution(user_id, created)
                    retried += 1
                except _RECOVERABLE:
                    still_pending.append(mid)
            self._pending_graph_sync.extend(still_pending)
            if report.errors is None:
                report.errors = []
            report.errors.append(
                f"graph_pending_sync: retried={retried}, still_pending={len(still_pending)}"
            )

        # Graph consolidation (conflict detection, source integrity)
        try:
            cr = self._consolidator.consolidate(user_id)
            if cr.errors:
                report.errors = (report.errors or []) + cr.errors
        except _RECOVERABLE as e:
            logger.warning("Graph consolidation failed: %s", e)
            if report.errors is None:
                report.errors = []
            report.errors.append(f"graph_consolidation: {e}")

        return report

    def health_check(self, user_id: str) -> HealthReport:
        return self._tabular_delegate.health_check(user_id)

    # ── CandidateProvider ─────────────────────────────────────────────

    def get_reflection_candidates(
        self,
        user_id: str,
        *,
        since_hours: int = 24,
    ) -> list[ReflectionCandidate]:
        """Get reflection candidates from graph activation patterns."""
        try:
            candidates = self._candidates.get_reflection_candidates(
                user_id,
                since_hours=since_hours,
            )
            if candidates:
                return candidates
        except _RECOVERABLE:
            logger.warning(
                "Graph candidate selection failed, falling back", exc_info=True
            )

        # Fallback to tabular candidates
        return self._tabular_delegate._governance_lazy.get_reflection_candidates(
            user_id,
            since_hours=since_hours,
        )

    # ── Graph-specific ────────────────────────────────────────────────

    def get_graph_stats(self, user_id: str) -> dict[str, int]:
        return {"total_nodes": self._store.count_user_nodes(user_id)}

    def consolidate(self, user_id: str) -> ConsolidationResult:
        """Run graph consolidation directly (for testing/admin)."""
        return self._consolidator.consolidate(user_id)

    def extract_entities_llm(self, user_id: str, llm_client: Any) -> dict[str, Any]:
        """Manual LLM entity extraction for all active semantic memories.

        Scans memories that don't yet have entity_link edges, extracts entities
        via LLM, creates entity nodes and edges. Idempotent — skips already-linked.
        """
        from memoria.core.memory.graph.entity_extractor import (
            LLMEntityExtractionResult,
            extract_entities_llm as _extract_llm,
        )
        from memoria.core.memory.graph.types import EdgeType, NodeType

        result = LLMEntityExtractionResult()

        # Get semantic nodes without entity_link edges
        semantic_nodes = self._store.get_user_nodes(
            user_id,
            node_type=NodeType.SEMANTIC,
            active_only=True,
            load_embedding=False,
        )
        if not semantic_nodes:
            return {"total_memories": 0, "entities_found": 0, "edges_created": 0}

        # Find which nodes already have entity_link edges
        node_ids = {n.node_id for n in semantic_nodes}
        existing_edges = self._store.get_edges_for_nodes(node_ids)
        linked_ids = {
            nid
            for nid, edges in existing_edges.items()
            if any(e.edge_type == EdgeType.ENTITY_LINK.value for e in edges)
        }
        unlinked = [n for n in semantic_nodes if n.node_id not in linked_ids]
        result.total_memories = len(unlinked)

        if not unlinked:
            return {"total_memories": 0, "entities_found": 0, "edges_created": 0}

        # Extract entities per node via LLM
        entities_per_node: dict[str, list[tuple[str, str]]] = {}
        for node in unlinked:
            try:
                entities = _extract_llm(node.content, llm_client)
                if entities:
                    entities_per_node[node.node_id] = [
                        (ent.name, ent.entity_type) for ent in entities
                    ]
                    result.entities_found += len(entities)
            except Exception as e:
                result.errors.append(f"{node.node_id}: {e}")

        # Use unified linking
        created, pending_edges, _reused = self._store.link_entities_batch(
            user_id,
            unlinked,
            entities_per_node,
            source="llm",
        )
        if pending_edges:
            self._store.add_edges_batch(pending_edges, user_id)
            result.edges_created = len(pending_edges)

        return {
            "total_memories": result.total_memories,
            "entities_found": result.entities_found,
            "edges_created": result.edges_created,
            "errors": result.errors,
        }
