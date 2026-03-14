"""ActivationRetrievalStrategy — graph spreading activation retrieval.

Self-contained strategy with internal vector fallback when graph is too small.

See docs/design/memory/backend-management.md §3.3, §3.5
"""

from __future__ import annotations

import logging
from typing import TYPE_CHECKING, Any

from memoria.core.explain import explain_timer, set_explain_path
from memoria.core.memory.graph.retriever import ActivationRetriever
from memoria.core.memory.types import Memory, MemoryType, TrustTier

if TYPE_CHECKING:
    from memoria.core.db_consumer import DbFactory
    from memoria.core.memory.config import MemoryGovernanceConfig
    from memoria.core.memory.graph.types import GraphNodeData
    from memoria.core.memory.tabular.metrics import MemoryMetrics
    from memoria.core.memory.types import RetrievalWeights

logger = logging.getLogger(__name__)


def _apply_post_filters(
    memories: list[Memory],
    *,
    memory_types: list[MemoryType] | None = None,
    session_id: str = "",
    include_cross_session: bool = True,
) -> list[Memory]:
    """Apply memory_types / session / cross-session filters to graph results."""
    result = memories
    if memory_types:
        allowed = set(memory_types)
        result = [m for m in result if m.memory_type in allowed]
    if session_id and not include_cross_session:
        result = [m for m in result if m.session_id == session_id]
    return result


# NodeType → MemoryType: preserve original type from graph node
_NODE_TO_MEMORY: dict[str, MemoryType] = {
    "episodic": MemoryType.WORKING,
    "semantic": MemoryType.SEMANTIC,
    "scene": MemoryType.SEMANTIC,
}


def _node_type_to_memory_type(node_type: Any) -> MemoryType:
    val = node_type.value if hasattr(node_type, "value") else str(node_type)
    return _NODE_TO_MEMORY.get(val, MemoryType.SEMANTIC)


class ActivationRetrievalStrategy:
    """activation:v1 — spreading activation on graph nodes/edges.

    Internal vector fallback when graph retrieval returns no results.
    """

    def __init__(
        self,
        db_factory: DbFactory,
        *,
        params: dict[str, Any] | None = None,
        config: MemoryGovernanceConfig | None = None,
        metrics: MemoryMetrics | None = None,
    ) -> None:
        from memoria.core.memory.graph.graph_store import GraphStore
        from memoria.core.memory.tabular.store import MemoryStore

        self._db_factory = db_factory
        self._config = config
        self._metrics = metrics
        self._store = GraphStore(db_factory)
        self._mem_store = MemoryStore(db_factory, metrics=metrics)
        self._activation_retriever = ActivationRetriever(self._store, config=config)
        self._vector_fallback_strategy: Any = None

    @property
    def strategy_key(self) -> str:
        return "activation:v1"

    def retrieve(
        self,
        user_id: str,
        query: str,
        query_embedding: list[float] | None = None,
        *,
        top_k: int = 10,
        task_type: str | None = None,
        session_id: str = "",
        memory_types: list[MemoryType] | None = None,
        weights: RetrievalWeights | None = None,
        include_cross_session: bool = True,
        explain: bool = False,
    ) -> tuple[list[Memory], Any]:
        """Retrieve via graph activation, fallback to vector if insufficient."""
        graph_memories: list[Memory] = []
        if query_embedding:
            try:
                with explain_timer("phase1_graph_activation") as timer:
                    activated = self._activation_retriever.retrieve(
                        user_id,
                        query,
                        query_embedding,
                        top_k=top_k,
                        task_type=task_type,
                    )
                    timer.add_metric("candidates_returned", len(activated))

                    if activated:
                        graph_memories = self._nodes_to_memories(activated, user_id)
                        timer.add_metric("memories_converted", len(graph_memories))

                        graph_memories = _apply_post_filters(
                            graph_memories,
                            memory_types=memory_types,
                            session_id=session_id,
                            include_cross_session=include_cross_session,
                        )
                        timer.add_metric("after_filters", len(graph_memories))
            except Exception:
                logger.warning(
                    "Activation retrieval failed, using vector fallback",
                    exc_info=True,
                )

        # If graph returned enough results, use them directly
        if len(graph_memories) >= top_k:
            logger.info(
                "activation:v1 graph path — user=%s results=%d",
                user_id,
                len(graph_memories),
            )
            set_explain_path("graph")
            # explain_timer already collected phase1 data in explain context
            return graph_memories[:top_k], None

        # Graph insufficient — supplement with vector results
        with explain_timer("phase2_vector_fallback") as timer:
            vec_memories, vec_explain = self._get_vector_fallback().retrieve(
                user_id,
                query,
                query_embedding,
                top_k=top_k,
                task_type=task_type,
                session_id=session_id,
                memory_types=memory_types,
                weights=weights,
                include_cross_session=include_cross_session,
                explain=explain,
            )
            timer.add_metric("vector_results", len(vec_memories))

        # Merge: deduplicate, then sort by retrieval_score descending.
        # Graph results have activation-based scores; vector results have
        # vector/keyword scores. Sorting by score lets the best results win
        # regardless of which path produced them.
        with explain_timer("merge_rerank") as timer:
            seen: set[str] = set()
            pool: list[Memory] = []
            for m in graph_memories + vec_memories:
                if m.memory_id not in seen:
                    seen.add(m.memory_id)
                    pool.append(m)
            pool.sort(key=lambda m: m.retrieval_score or 0.0, reverse=True)
            merged = pool[:top_k]

            timer.add_metric("graph_candidates", len(graph_memories))
            timer.add_metric("vector_candidates", len(vec_memories))
            timer.add_metric("unique_after_dedup", len(pool))
            timer.add_metric("final_results", len(merged))

        path = "graph+vector" if graph_memories else "vector_fallback"
        set_explain_path(path)
        logger.info(
            "activation:v1 %s — user=%s graph=%d vector=%d merged=%d",
            path,
            user_id,
            len(graph_memories),
            len(vec_memories),
            len(merged),
        )
        # explain_timer already collected all phase data in explain context
        # No need to return dict —桥接逻辑会从 context 读取
        return merged, None

    def _get_vector_fallback(self) -> Any:
        """Lazy-init vector fallback."""
        if self._vector_fallback_strategy is None:
            from memoria.core.memory.strategy.vector_v1 import VectorRetrievalStrategy

            self._vector_fallback_strategy = VectorRetrievalStrategy(
                self._db_factory,
                config=self._config,
                metrics=self._metrics,
            )
        return self._vector_fallback_strategy

    def _nodes_to_memories(
        self,
        scored_nodes: list[tuple[GraphNodeData, float]],
        user_id: str,
    ) -> list[Memory]:
        """Convert scored graph nodes to Memory objects, enriched from mem_memories."""
        # Collect memory_ids to batch-fetch full rows from tabular store
        memory_ids = [node.memory_id for node, _ in scored_nodes if node.memory_id]
        tabular = self._mem_store.get_by_ids(memory_ids) if memory_ids else {}

        memories: list[Memory] = []
        seen: set[str] = set()
        for node, _score in scored_nodes:
            mid = node.memory_id or node.node_id
            if mid in tabular:
                if mid not in seen:
                    seen.add(mid)
                    mem = tabular[mid]
                    mem.retrieval_score = round(_score, 4)
                    memories.append(mem)
                continue
            # Skip entity/scene nodes that have no backing memory row
            if not node.memory_id:
                continue
            # Fallback: construct from graph node (missing access_count, created_at etc.)
            if mid not in seen:
                seen.add(mid)
                try:
                    tier = TrustTier(node.trust_tier)
                except ValueError:
                    tier = TrustTier.T3_INFERRED
                memories.append(
                    Memory(
                        memory_id=mid,
                        user_id=node.user_id,
                        memory_type=_node_type_to_memory_type(node.node_type),
                        content=node.content,
                        initial_confidence=node.confidence,
                        embedding=node.embedding,
                        session_id=node.session_id,
                        trust_tier=tier,
                        retrieval_score=round(_score, 4),
                    )
                )
        return memories
