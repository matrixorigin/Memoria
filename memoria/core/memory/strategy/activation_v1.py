"""ActivationRetrievalStrategy — graph spreading activation retrieval.

Self-contained strategy with internal vector fallback when graph is too small.

See docs/design/memory/backend-management.md §3.3, §3.5
"""

from __future__ import annotations

import logging
from typing import TYPE_CHECKING, Any

from memoria.core.memory.graph.retriever import ActivationRetriever
from memoria.core.memory.types import Memory, MemoryType, TrustTier

if TYPE_CHECKING:
    from memoria.core.db_consumer import DbFactory
    from memoria.core.memory.config import MemoryGovernanceConfig
    from memoria.core.memory.graph.types import GraphNodeData
    from memoria.core.memory.tabular.metrics import MemoryMetrics
    from memoria.core.memory.types import RetrievalWeights

logger = logging.getLogger(__name__)

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
        """Retrieve via graph activation, fallback to vector if no results."""
        if query_embedding:
            try:
                activated = self._activation_retriever.retrieve(
                    user_id,
                    query,
                    query_embedding,
                    top_k=top_k,
                    task_type=task_type,
                )
                if activated:
                    memories = self._nodes_to_memories(activated, user_id)
                    logger.info(
                        "activation:v1 graph path — user=%s results=%d",
                        user_id,
                        len(memories),
                    )
                    explain_info = (
                        {"path": "graph", "results": len(memories)} if explain else None
                    )
                    return memories, explain_info
            except Exception:
                logger.warning(
                    "Activation retrieval failed, using vector fallback",
                    exc_info=True,
                )

        # Vector fallback when graph returns nothing
        logger.warning(
            "activation:v1 vector fallback — user=%s query=%r", user_id, query
        )
        memories, vec_explain = self._get_vector_fallback().retrieve(
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
        if explain:
            return memories, {"path": "vector_fallback", "vec_explain": vec_explain}
        return memories, vec_explain

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
                    memories.append(tabular[mid])
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
                    )
                )
        return memories
