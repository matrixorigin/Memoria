"""GraphBuilder — builds graph nodes and edges from memories and events.

Phase 1 (Perceive): called after every TypedObserver.observe().
No LLM calls — purely structural graph construction.
"""

from __future__ import annotations

import logging
from typing import TYPE_CHECKING, Any

from memoria.core.memory.graph.entity_extractor import extract_entities_lightweight
from memoria.core.memory.graph.graph_store import GraphStore, _new_id
from memoria.core.memory.graph.types import EdgeType, GraphNodeData, NodeType

if TYPE_CHECKING:
    from memoria.core.memory.types import Memory

logger = logging.getLogger(__name__)

ASSOCIATION_TOP_K = 5


def _compute_ingest_importance(
    node_type: NodeType,
    *, event: dict[str, Any] | None = None,
    memory: Any | None = None,
    neighbor_count: int = 0,
) -> float:
    base = {NodeType.EPISODIC: 0.3, NodeType.SEMANTIC: 0.5, NodeType.SCENE: 0.6}[node_type]
    boost = 0.0
    if event:
        etype = event.get("event_type", "")
        if etype == "tool_error":
            boost += 0.2
        if etype == "user_query":
            content = event.get("content", "")
            if any(kw in content.lower() for kw in ("no,", "wrong", "not what", "actually", "i said")):
                boost += 0.25
    if memory and getattr(memory, "initial_confidence", 0) >= 0.85:
        boost += 0.1
    if neighbor_count >= 3:
        boost += 0.1
    return min(base + boost, 1.0)


class GraphBuilder:
    """Builds graph structure from memories and events."""

    def __init__(self, store: GraphStore) -> None:
        self._store = store

    def ingest(
        self, user_id: str, new_memories: list[Memory],
        source_events: list[dict[str, Any]],
        *, session_id: str | None = None,
    ) -> list[GraphNodeData]:
        pending_edges: list[tuple[str, str, str, float]] = []
        created: list[GraphNodeData] = []

        episodic_nodes = self._create_episodic_nodes(
            user_id, source_events, pending_edges, session_id=session_id,
        )
        created.extend(episodic_nodes)

        semantic_nodes = self._create_semantic_nodes(
            user_id, new_memories, session_id=session_id,
        )
        created.extend(semantic_nodes)

        # Abstraction edges
        for ep in episodic_nodes:
            for sem in semantic_nodes:
                if ep.session_id and ep.session_id == sem.session_id:
                    pending_edges.append((
                        ep.node_id, sem.node_id, EdgeType.ABSTRACTION.value, 0.8,
                    ))

        # Association edges (DB-side cosine similarity as weight)
        for node in semantic_nodes:
            if not node.embedding:
                continue
            similar = self._store.find_similar_with_scores(
                user_id, node.embedding,
                top_k=ASSOCIATION_TOP_K, node_type=NodeType.SEMANTIC,
            )
            for candidate, cos_sim in similar:
                if candidate.node_id == node.node_id:
                    continue
                if cos_sim > 0.3:
                    pending_edges.append((
                        node.node_id, candidate.node_id,
                        EdgeType.ASSOCIATION.value, round(cos_sim, 3),
                    ))

        # Causal edges
        self._collect_causal_edges(episodic_nodes, source_events, pending_edges)

        # Entity linking (lightweight — no LLM)
        all_content_nodes = episodic_nodes + semantic_nodes
        entity_nodes = self._link_entities(user_id, all_content_nodes, pending_edges)
        created.extend(entity_nodes)

        if pending_edges:
            self._store.add_edges_batch(pending_edges, user_id)

        return created

    def _create_episodic_nodes(
        self, user_id: str, events: list[dict[str, Any]],
        pending_edges: list[tuple[str, str, str, float]],
        *, session_id: str | None = None,
    ) -> list[GraphNodeData]:
        nodes: list[GraphNodeData] = []
        new_nodes: list[GraphNodeData] = []
        prev_episodic = self._store.get_latest_episodic_in_session(
            user_id, session_id or "",
        )

        for event in events:
            event_id = event.get("event_id", "")
            if not event_id:
                continue
            existing = self._store.get_node_by_event_id(event_id)
            if existing:
                nodes.append(existing)
                prev_episodic = existing
                continue

            node = GraphNodeData(
                node_id=_new_id(), user_id=user_id,
                node_type=NodeType.EPISODIC,
                content=event.get("content", ""),
                embedding=event.get("embedding"),
                event_id=event_id, session_id=session_id,
                confidence=1.0, trust_tier="T1",
                importance=_compute_ingest_importance(
                    NodeType.EPISODIC, event=event,
                    neighbor_count=1 if prev_episodic else 0,
                ),
            )
            new_nodes.append(node)
            nodes.append(node)

            if prev_episodic:
                pending_edges.append((
                    prev_episodic.node_id, node.node_id,
                    EdgeType.TEMPORAL.value, 1.0,
                ))
            prev_episodic = node

        if new_nodes:
            self._store.create_nodes_batch(new_nodes)
        return nodes

    def _create_semantic_nodes(
        self, user_id: str, memories: list[Memory],
        *, session_id: str | None = None,
    ) -> list[GraphNodeData]:
        nodes: list[GraphNodeData] = []
        new_nodes: list[GraphNodeData] = []

        for mem in memories:
            existing = self._store.get_node_by_memory_id(mem.memory_id)
            if existing:
                nodes.append(existing)
                continue

            node = GraphNodeData(
                node_id=_new_id(), user_id=user_id,
                node_type=NodeType.SEMANTIC,
                content=mem.content, embedding=mem.embedding,
                memory_id=mem.memory_id,
                session_id=session_id or mem.session_id,
                confidence=mem.initial_confidence,
                trust_tier=mem.trust_tier.value if hasattr(mem.trust_tier, "value") else str(mem.trust_tier),
                importance=_compute_ingest_importance(NodeType.SEMANTIC, memory=mem),
            )
            new_nodes.append(node)
            nodes.append(node)

        if new_nodes:
            self._store.create_nodes_batch(new_nodes)
        return nodes

    @staticmethod
    def _collect_causal_edges(
        episodic_nodes: list[GraphNodeData],
        source_events: list[dict[str, Any]],
        pending_edges: list[tuple[str, str, str, float]],
    ) -> None:
        if len(episodic_nodes) < 2:
            return
        node_by_event: dict[str, GraphNodeData] = {}
        for node in episodic_nodes:
            if node.event_id:
                node_by_event[node.event_id] = node

        prev_event: dict[str, Any] | None = None
        for ev in source_events:
            if (
                prev_event
                and ev.get("event_type") == "tool_error"
                and prev_event.get("event_type") == "tool_call"
            ):
                src_node = node_by_event.get(prev_event.get("event_id", ""))
                tgt_node = node_by_event.get(ev.get("event_id", ""))
                if src_node and tgt_node:
                    pending_edges.append((
                        src_node.node_id, tgt_node.node_id,
                        EdgeType.CAUSAL.value, 1.5,
                    ))
            prev_event = ev

    def _link_entities(
        self, user_id: str,
        content_nodes: list[GraphNodeData],
        pending_edges: list[tuple[str, str, str, float]],
    ) -> list[GraphNodeData]:
        """Extract entities from content nodes and link them.

        Reuses existing entity nodes (by name match) to build a shared entity graph.
        """
        if not content_nodes:
            return []

        created_entities: list[GraphNodeData] = []
        # Cache: entity_name → node_id (avoid repeated DB lookups within batch)
        entity_cache: dict[str, str] = {}

        for node in content_nodes:
            if not node.content:
                continue
            entities = extract_entities_lightweight(node.content)
            for ent in entities:
                ent_node_id = entity_cache.get(ent.name)
                if not ent_node_id:
                    # Check DB for existing entity node
                    existing = self._store.find_entity_node(user_id, ent.name)
                    if existing:
                        ent_node_id = existing.node_id
                    else:
                        ent_node_id = _new_id()
                        # Lightweight regex extraction → importance 0.3
                        # (LLM extraction in service.extract_entities_llm uses 0.4)
                        ent_node = GraphNodeData(
                            node_id=ent_node_id, user_id=user_id,
                            node_type=NodeType.ENTITY,
                            content=ent.display_name,
                            confidence=1.0, trust_tier="T1",
                            importance=0.3,
                        )
                        self._store.create_node(ent_node)
                        created_entities.append(ent_node)
                    entity_cache[ent.name] = ent_node_id

                pending_edges.append((
                    node.node_id, ent_node_id,
                    EdgeType.ENTITY_LINK.value, 1.0,
                ))

        return created_entities
