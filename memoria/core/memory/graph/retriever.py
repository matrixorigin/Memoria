"""ActivationRetriever — graph-based memory retrieval.

All graph traversal is DB-side via normalized edge table.
No full graph load at any scale.

See docs/design/memory/graph-memory.md §3
"""

from __future__ import annotations

import logging
import math
from datetime import datetime, timezone
from typing import TYPE_CHECKING

from memoria.core.memory.graph.activation import SpreadingActivation
from memoria.core.memory.graph.ner import get_ner_backend
from memoria.core.memory.graph.types import GraphNodeData

if TYPE_CHECKING:
    from memoria.core.memory.config import MemoryGovernanceConfig
    from memoria.core.memory.graph.graph_store import GraphStore

logger = logging.getLogger(__name__)

LAMBDA_SEMANTIC = 0.35
LAMBDA_ACTIVATION = 0.35
LAMBDA_CONFIDENCE = 0.20
LAMBDA_IMPORTANCE = 0.10

CONFLICT_PENALTY = {"superseded": 0.5, "pending": 0.7}

NODE_TYPE_WEIGHT = {"scene": 1.2, "semantic": 1.0, "episodic": 0.8, "entity": 0.8}

MIN_GRAPH_NODES = 3
ANCHOR_TOP_K = 10

# Temporal decay for graph nodes (hours)
TEMPORAL_DECAY_HOURS = 720.0

# §13.2 Memory mode → activation parameters per task type
_TASK_ACTIVATION_PARAMS: dict[str | None, tuple[int, int]] = {
    # task_type: (iterations, anchor_k)
    "code_review": (3, 10),  # FULL
    "debugging": (3, 10),  # FULL
    "planning": (2, 5),  # COMPRESSED
    "general": (3, 10),  # FULL (fallback)
    None: (3, 10),  # default
}


def _task_activation_params(task_type: str | None) -> tuple[int, int]:
    """Return (iterations, anchor_k) for the given task type."""
    return _TASK_ACTIVATION_PARAMS.get(task_type, _TASK_ACTIVATION_PARAMS[None])


_HALF_LIVES = {"T1": 365.0, "T2": 180.0, "T3": 60.0, "T4": 30.0}


def _effective_confidence(node: GraphNodeData) -> float:
    """Query-time confidence decay: confidence × 2^(-age/half_life)."""
    if node.confidence is None:
        return 0.5
    if not node.created_at:
        return node.confidence
    half_life = _HALF_LIVES.get(node.trust_tier, 60.0)
    try:
        if isinstance(node.created_at, str):
            created = datetime.fromisoformat(node.created_at.replace("Z", "+00:00"))
        else:
            created = node.created_at
        if created.tzinfo is None:
            created = created.replace(tzinfo=timezone.utc)
        age_days = max(
            (datetime.now(timezone.utc) - created).total_seconds() / 86400.0, 0.0
        )
        return node.confidence * math.exp(-age_days * math.log(2) / half_life)
    except (ValueError, TypeError):
        return node.confidence


class ActivationRetriever:
    """Graph retrieval via DB-side spreading activation.

    Works at any scale — no full graph load, no tiered thresholds.
    Supports entity-anchored retrieval: when query contains recognized entities,
    memories linked to those entities get a configurable boost (default ×1.8).
    """

    def __init__(
        self,
        store: GraphStore,
        *,
        config: MemoryGovernanceConfig | None = None,
    ) -> None:
        self._store = store
        if config is None:
            from memoria.core.memory.config import DEFAULT_CONFIG

            config = DEFAULT_CONFIG
        self._config = config

    def retrieve(
        self,
        user_id: str,
        query: str,
        query_embedding: list[float] | None = None,
        *,
        top_k: int = 10,
        task_type: str | None = None,
    ) -> list[tuple[GraphNodeData, float]]:
        if not query_embedding:
            logger.warning(
                "graph_retriever: query_embedding is None, graph path skipped (returning empty)"
            )
            return []
        if not self._store.has_min_nodes(user_id, MIN_GRAPH_NODES):
            logger.warning(
                "graph_retriever: insufficient graph nodes for user=%s (min=%d), graph path skipped",
                user_id,
                MIN_GRAPH_NODES,
            )
            return []

        # §13.2 Memory mode → activation parameters
        iterations, anchor_k = _task_activation_params(task_type)

        # 1. Dual-trigger anchor selection: BM25 (fulltext) + vector (cosine)
        #    BM25 catches exact-match terms that vector search may miss.
        anchors: dict[str, float] = {}
        anchor_semantic: dict[str, float] = {}

        # 1a. Vector anchors (cosine similarity)
        vector_results = self._store.find_similar_with_scores(
            user_id,
            query_embedding,
            top_k=anchor_k,
        )
        for node, sim in vector_results:
            anchors[node.node_id] = max(sim, 0.0)
        anchor_semantic = dict(anchors)

        # 1b. BM25 anchors (fulltext MATCH...AGAINST)
        try:
            bm25_results = self._store.fulltext_search(user_id, query, top_k=anchor_k)
            for node, _score in bm25_results:
                if node.node_id not in anchors:
                    anchors[node.node_id] = 0.7  # BM25 anchors slightly below vector
        except Exception:
            logger.debug(
                "Fulltext search failed, using vector-only anchors", exc_info=True
            )

        if not anchors:
            return []

        # 2. Soft entity linking: find entity candidates via embedding similarity,
        #    inject as weighted activation anchors.
        entity_anchors, entity_memory_ids = self._entity_recall(
            user_id,
            query,
            query_embedding=query_embedding,
        )

        # Inject entity anchors with similarity-weighted activation
        for nid, weight in entity_anchors.items():
            if nid not in anchors:
                anchors[nid] = 0.8 * weight  # scale by similarity

        # 3. Spreading activation — DB-side edge traversal (§13.1 task boost)
        sa = SpreadingActivation(self._store, task_type=task_type, config=self._config)
        sa.set_anchors(anchors)
        sa.propagate(iterations=iterations)
        activation_map = sa.get_activated(min_activation=0.01)

        # 4. Collect candidate IDs — include entity-recalled memory graph nodes
        candidate_ids: set[str] = set(anchors.keys())
        for nid, _ in sorted(activation_map.items(), key=lambda x: x[1], reverse=True)[
            : top_k * 3
        ]:
            candidate_ids.add(nid)

        # Add graph nodes for entity-recalled memories (reverse lookup recall)
        for mid in entity_memory_ids:
            gnode = self._store.get_node_by_memory_id(mid)
            if gnode and gnode.is_active:
                candidate_ids.add(gnode.node_id)

        # 5. Fetch only the candidate nodes (not full graph)
        candidates = self._store.get_nodes_by_ids(list(candidate_ids))

        # 6. Score
        entity_boost = self._config.entity_boost
        node_type_weights = dict(NODE_TYPE_WEIGHT)
        node_type_weights["entity"] = self._config.entity_node_type_weight
        l_sem = self._config.activation_lambda_semantic
        l_act = self._config.activation_lambda_activation
        l_conf = self._config.activation_lambda_confidence
        l_imp = self._config.activation_lambda_importance

        results: list[tuple[GraphNodeData, float]] = []
        for node in candidates:
            activation = activation_map.get(node.node_id, 0.0)
            semantic = anchor_semantic.get(node.node_id, 0.0)
            confidence = _effective_confidence(node)

            score = (
                l_sem * semantic
                + l_act * activation
                + l_conf * confidence
                + l_imp * node.importance
            )

            # Temporal recency: exponential decay based on node age
            if node.created_at:
                try:
                    if isinstance(node.created_at, str):
                        created = datetime.fromisoformat(
                            node.created_at.replace("Z", "+00:00")
                        )
                    else:
                        created = node.created_at
                    if created.tzinfo is None:
                        created = created.replace(tzinfo=timezone.utc)
                    age_hours = max(
                        (datetime.now(timezone.utc) - created).total_seconds() / 3600.0,
                        0.0,
                    )
                    score *= math.exp(-age_hours / TEMPORAL_DECAY_HOURS)
                except (ValueError, TypeError):
                    pass

            # Frequency boost: mild boost for frequently accessed nodes
            if node.access_count > 0:
                score *= 1.0 + 0.1 * math.log(1 + node.access_count)

            # Type-based weighting: prefer scene > semantic > episodic
            score *= node_type_weights.get(node.node_type, 1.0)

            # Entity boost: if this node's memory_id was recalled via entity lookup
            if node.memory_id and node.memory_id in entity_memory_ids:
                score *= entity_boost

            if node.conflicts_with:
                resolution = node.conflict_resolution or "pending"
                score *= CONFLICT_PENALTY.get(resolution, 1.0)

            if score > 0.01:
                results.append((node, score))

        results.sort(key=lambda x: x[1], reverse=True)
        return results[:top_k]

    def _entity_recall(
        self,
        user_id: str,
        query: str,
        query_embedding: list[float] | None = None,
    ) -> tuple[dict[str, float], set[str]]:
        """Soft entity linking: find entity candidates via embedding similarity.

        Falls back to hard name match when no embedding is available.

        Returns:
            (entity_anchors {node_id: weight}, memory_ids) —
            entity_anchors with similarity-weighted activation,
            memory_ids for candidate recall injection.
        """
        entity_anchors: dict[str, float] = {}
        memory_ids: set[str] = set()

        # Soft linking: use query embedding to find similar entities
        if query_embedding:
            soft_matches = self._store.find_entities_soft(
                user_id,
                query_embedding,
                top_k=5,
                threshold=1.0,
            )
            for entity_id, sim_score in soft_matches:
                entity_anchors[entity_id] = sim_score
                for mid, _weight in self._store.get_memories_by_entity(
                    entity_id, user_id, limit=20
                ):
                    memory_ids.add(mid)

        # Hard linking: extract entity names from query text
        _NO_ACTIVATION = {"time", "person"}
        query_entities = get_ner_backend().extract(query)
        for ent in query_entities:
            if ent.entity_type in _NO_ACTIVATION:
                continue
            entity_id = self._store.find_entity_by_name(user_id, ent.name)
            if entity_id and entity_id not in entity_anchors:
                entity_anchors[entity_id] = 1.0  # exact match = full weight
                for mid, _weight in self._store.get_memories_by_entity(
                    entity_id, user_id, limit=20
                ):
                    memory_ids.add(mid)

        return entity_anchors, memory_ids
