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
from memoria.core.memory.graph.types import GraphNodeData

if TYPE_CHECKING:
    from memoria.core.memory.graph.graph_store import GraphStore

logger = logging.getLogger(__name__)

LAMBDA_SEMANTIC = 0.35
LAMBDA_ACTIVATION = 0.35
LAMBDA_CONFIDENCE = 0.20
LAMBDA_IMPORTANCE = 0.10

CONFLICT_PENALTY = {"superseded": 0.5, "pending": 0.7}

# Node type weights: scene nodes are distilled insights (highest value),
# semantic nodes are facts/preferences, episodic nodes are raw events.
NODE_TYPE_WEIGHT = {"scene": 1.2, "semantic": 1.0, "episodic": 0.8, "entity": 0.6}

MIN_GRAPH_NODES = 10
ANCHOR_TOP_K = 10

# §13.2 Memory mode → activation parameters per task type
_TASK_ACTIVATION_PARAMS: dict[str | None, tuple[int, int]] = {
    # task_type: (iterations, anchor_k)
    "code_review": (3, 10),   # FULL
    "debugging":   (3, 10),   # FULL
    "planning":    (2, 5),    # COMPRESSED
    "general":     (3, 10),   # FULL (fallback)
    None:          (3, 10),   # default
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
        age_days = max((datetime.now(timezone.utc) - created).total_seconds() / 86400.0, 0.0)
        return node.confidence * math.exp(-age_days * math.log(2) / half_life)
    except (ValueError, TypeError):
        return node.confidence


class ActivationRetriever:
    """Graph retrieval via DB-side spreading activation.

    Works at any scale — no full graph load, no tiered thresholds.
    """

    def __init__(self, store: GraphStore) -> None:
        self._store = store

    def retrieve(
        self, user_id: str, query: str,
        query_embedding: list[float] | None = None,
        *, top_k: int = 10,
        task_type: str | None = None,
    ) -> list[tuple[GraphNodeData, float]]:
        if not query_embedding:
            return []
        if not self._store.has_min_nodes(user_id, MIN_GRAPH_NODES):
            return []

        # §13.2 Memory mode → activation parameters
        iterations, anchor_k = _task_activation_params(task_type)

        # 1. DB-side anchor selection (cosine similarity)
        anchor_results = self._store.find_similar_with_scores(
            user_id, query_embedding, top_k=anchor_k,
        )
        if not anchor_results:
            return []

        anchors = {n.node_id: max(s, 0.0) for n, s in anchor_results}
        anchor_semantic = dict(anchors)

        # 2. Spreading activation — DB-side edge traversal (§13.1 task boost)
        sa = SpreadingActivation(self._store, task_type=task_type)
        sa.set_anchors(anchors)
        sa.propagate(iterations=iterations)
        activation_map = sa.get_activated(min_activation=0.01)

        # 3. Collect candidate IDs
        candidate_ids: set[str] = set(anchors.keys())
        for nid, _ in sorted(activation_map.items(), key=lambda x: x[1], reverse=True)[:top_k * 3]:
            candidate_ids.add(nid)

        # 4. Fetch only the candidate nodes (not full graph)
        candidates = self._store.get_nodes_by_ids(list(candidate_ids))

        # 5. Score
        results: list[tuple[GraphNodeData, float]] = []
        for node in candidates:
            activation = activation_map.get(node.node_id, 0.0)
            semantic = anchor_semantic.get(node.node_id, 0.0)
            confidence = _effective_confidence(node)

            score = (
                LAMBDA_SEMANTIC * semantic
                + LAMBDA_ACTIVATION * activation
                + LAMBDA_CONFIDENCE * confidence
                + LAMBDA_IMPORTANCE * node.importance
            )

            # Type-based weighting: prefer scene > semantic > episodic
            score *= NODE_TYPE_WEIGHT.get(node.node_type, 1.0)

            if node.conflicts_with:
                resolution = node.conflict_resolution or "pending"
                score *= CONFLICT_PENALTY.get(resolution, 1.0)

            if score > 0.01:
                results.append((node, score))

        results.sort(key=lambda x: x[1], reverse=True)
        return results[:top_k]
