"""GraphBuilder — builds graph nodes and edges from memories and events.

Phase 1 (Perceive): called after every TypedObserver.observe().
No LLM calls — purely structural graph construction.
"""

from __future__ import annotations

import logging
from typing import TYPE_CHECKING, Any

from memoria.core.memory.graph.ner import get_ner_backend
from memoria.core.memory.graph.graph_store import GraphStore, _new_id
from memoria.core.memory.graph.types import EdgeType, GraphNodeData, NodeType
from memoria.core.memory.types import TrustTier

if TYPE_CHECKING:
    from memoria.core.memory.config import MemoryGovernanceConfig
    from memoria.core.memory.types import Memory

logger = logging.getLogger(__name__)

ASSOCIATION_TOP_K = 5


def _compute_ingest_importance(
    node_type: NodeType,
    *,
    event: dict[str, Any] | None = None,
    memory: Any | None = None,
    neighbor_count: int = 0,
) -> float:
    base = {NodeType.EPISODIC: 0.3, NodeType.SEMANTIC: 0.5, NodeType.SCENE: 0.6}[
        node_type
    ]
    boost = 0.0
    if event:
        etype = event.get("event_type", "")
        if etype == "tool_error":
            boost += 0.2
        if etype == "user_query":
            content = event.get("content", "")
            if any(
                kw in content.lower()
                for kw in ("no,", "wrong", "not what", "actually", "i said")
            ):
                boost += 0.25
    if memory and getattr(memory, "initial_confidence", 0) >= 0.85:
        boost += 0.1
    if neighbor_count >= 3:
        boost += 0.1
    return min(base + boost, 1.0)


class GraphBuilder:
    """Builds graph structure from memories and events."""

    def __init__(
        self,
        store: GraphStore,
        *,
        config: MemoryGovernanceConfig | None = None,
        embed_fn: Any | None = None,
    ) -> None:
        self._store = store
        self._embed_fn = embed_fn
        if config is None:
            from memoria.core.memory.config import DEFAULT_CONFIG

            config = DEFAULT_CONFIG
        self._assoc_threshold = config.activation_association_threshold

    def ingest(
        self,
        user_id: str,
        new_memories: list[Memory],
        source_events: list[dict[str, Any]],
        *,
        session_id: str | None = None,
    ) -> list[GraphNodeData]:
        pending_edges: list[tuple[str, str, str, float]] = []
        created: list[GraphNodeData] = []

        episodic_nodes = self._create_episodic_nodes(
            user_id,
            source_events,
            pending_edges,
            session_id=session_id,
        )
        created.extend(episodic_nodes)

        semantic_nodes = self._create_semantic_nodes(
            user_id,
            new_memories,
            session_id=session_id,
        )
        created.extend(semantic_nodes)

        # Abstraction edges
        for ep in episodic_nodes:
            for sem in semantic_nodes:
                if ep.session_id and ep.session_id == sem.session_id:
                    pending_edges.append(
                        (
                            ep.node_id,
                            sem.node_id,
                            EdgeType.ABSTRACTION.value,
                            0.8,
                        )
                    )

        # Association edges (DB-side cosine similarity as weight)
        for node in semantic_nodes:
            if not node.embedding:
                continue
            # Search both semantic and scene nodes for associations
            search_type = (
                None if node.node_type == NodeType.SCENE else NodeType.SEMANTIC
            )
            similar = self._store.find_similar_with_scores(
                user_id,
                node.embedding,
                top_k=ASSOCIATION_TOP_K,
                node_type=search_type,
            )
            for candidate, cos_sim in similar:
                if candidate.node_id == node.node_id:
                    continue
                if cos_sim > self._assoc_threshold:
                    pending_edges.append(
                        (
                            node.node_id,
                            candidate.node_id,
                            EdgeType.ASSOCIATION.value,
                            round(cos_sim, 3),
                        )
                    )

        # Causal edges
        self._collect_causal_edges(episodic_nodes, source_events, pending_edges)

        # Consolidation edges: scene → source memories
        for node in semantic_nodes:
            if node.node_type == NodeType.SCENE and node.source_nodes:
                for src_mid in node.source_nodes:
                    src_node = self._store.get_node_by_memory_id(src_mid)
                    if src_node:
                        pending_edges.append(
                            (
                                node.node_id,
                                src_node.node_id,
                                EdgeType.CONSOLIDATION.value,
                                1.0,
                            )
                        )

        # Entity linking (lightweight — no LLM)
        all_content_nodes = episodic_nodes + semantic_nodes
        entity_nodes = self._link_entities(user_id, all_content_nodes, pending_edges)
        created.extend(entity_nodes)

        if pending_edges:
            self._store.add_edges_batch(pending_edges, user_id)

        return created

    def _create_episodic_nodes(
        self,
        user_id: str,
        events: list[dict[str, Any]],
        pending_edges: list[tuple[str, str, str, float]],
        *,
        session_id: str | None = None,
    ) -> list[GraphNodeData]:
        nodes: list[GraphNodeData] = []
        new_nodes: list[GraphNodeData] = []
        prev_episodic = self._store.get_latest_episodic_in_session(
            user_id,
            session_id or "",
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
                node_id=_new_id(),
                user_id=user_id,
                node_type=NodeType.EPISODIC,
                content=event.get("content", ""),
                embedding=event.get("embedding"),
                event_id=event_id,
                session_id=session_id,
                confidence=1.0,
                trust_tier="T1",
                importance=_compute_ingest_importance(
                    NodeType.EPISODIC,
                    event=event,
                    neighbor_count=1 if prev_episodic else 0,
                ),
            )
            new_nodes.append(node)
            nodes.append(node)

            if prev_episodic:
                pending_edges.append(
                    (
                        prev_episodic.node_id,
                        node.node_id,
                        EdgeType.TEMPORAL.value,
                        1.0,
                    )
                )
            prev_episodic = node

        if new_nodes:
            self._store.create_nodes_batch(new_nodes)
        return nodes

    def _create_semantic_nodes(
        self,
        user_id: str,
        memories: list[Memory],
        *,
        session_id: str | None = None,
    ) -> list[GraphNodeData]:
        nodes: list[GraphNodeData] = []
        new_nodes: list[GraphNodeData] = []

        for mem in memories:
            existing = self._store.get_node_by_memory_id(mem.memory_id)
            if existing:
                nodes.append(existing)
                continue

            # Reflection-produced memories (T4 with source_event_ids pointing to
            # other memories) become scene nodes in the graph.
            is_scene = (
                mem.trust_tier == TrustTier.T4_UNVERIFIED
                and len(mem.source_event_ids) > 0
            )
            node_type = NodeType.SCENE if is_scene else NodeType.SEMANTIC

            node = GraphNodeData(
                node_id=_new_id(),
                user_id=user_id,
                node_type=node_type,
                content=mem.content,
                embedding=mem.embedding,
                memory_id=mem.memory_id,
                session_id=session_id or mem.session_id,
                confidence=mem.initial_confidence,
                trust_tier=mem.trust_tier.value
                if hasattr(mem.trust_tier, "value")
                else str(mem.trust_tier),
                importance=_compute_ingest_importance(node_type, memory=mem),
                source_nodes=mem.source_event_ids if is_scene else [],
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
                    pending_edges.append(
                        (
                            src_node.node_id,
                            tgt_node.node_id,
                            EdgeType.CAUSAL.value,
                            1.5,
                        )
                    )
            prev_event = ev

    def _link_entities(
        self,
        user_id: str,
        content_nodes: list[GraphNodeData],
        pending_edges: list[tuple[str, str, str, float]],
    ) -> list[GraphNodeData]:
        """Extract entities from content nodes and link them.

        Dual-write in a single transaction (all-or-nothing):
        1. mem_entities (source of truth)
        2. mem_memory_entity_links (semantic nodes only)
        3. memory_graph_nodes (entity nodes, entity_id == node_id)
        4. memory_graph_edges (collected into pending_edges for batch commit)

        entity_id in mem_entities == node_id in memory_graph_nodes (1:1 mapping).
        Only semantic nodes (with memory_id) get written to mem_memory_entity_links;
        episodic nodes only get graph edges (they have no mem_memories row).
        """
        if not content_nodes:
            return []

        # Entity link weight by extraction source — higher = more confident
        _ENTITY_WEIGHT: dict[str, float] = {
            "person": 1.0,  # @mention or Chinese name — high confidence
            "tech": 0.9,  # capitalized word or known term
            "project": 0.85,  # CamelCase or service-name pattern
            "repo": 0.95,  # owner/repo pattern — very specific
            "org": 0.8,
            "location": 0.7,
            "time": 0.6,
            "concept": 0.7,
        }

        # Build {node_id: [(canonical_name, entity_type, weight), ...]}
        entities_per_node: dict[str, list[tuple[str, str, float]]] = {}
        for node in content_nodes:
            if not node.content:
                continue
            entities = get_ner_backend().extract(node.content)
            if entities:
                entities_per_node[node.node_id] = [
                    (
                        ent.name,
                        ent.entity_type,
                        _ENTITY_WEIGHT.get(ent.entity_type, 0.8),
                    )
                    for ent in entities
                ]

        if not entities_per_node:
            return []

        # Single transaction for all four tables
        entity_id_cache: dict[str, str] = {}
        created: list[GraphNodeData] = []

        # Batch-embed entity names for soft dedup
        all_entity_names: list[str] = []
        for ent_list in entities_per_node.values():
            for canonical_name, _etype, _w in ent_list:
                if canonical_name not in entity_id_cache:
                    all_entity_names.append(canonical_name)
        entity_embeddings: dict[str, list[float]] = {}
        if all_entity_names and self._embed_fn:
            try:
                for n in all_entity_names:
                    vec = self._embed_fn(n)
                    if vec:
                        entity_embeddings[n] = vec
            except Exception:
                pass  # fall back to no-embedding upsert

        with self._store._db() as db:
            # 1. Upsert mem_entities
            for node in content_nodes:
                for canonical_name, etype, _w in entities_per_node.get(
                    node.node_id, []
                ):
                    if canonical_name not in entity_id_cache:
                        entity_id_cache[canonical_name] = self._store._upsert_entity_in(
                            db,
                            user_id,
                            canonical_name,
                            canonical_name,
                            etype,
                            embedding=entity_embeddings.get(canonical_name),
                        )

            # 2. Write mem_memory_entity_links (semantic nodes only)
            for node in content_nodes:
                if not node.memory_id:
                    continue
                for canonical_name, _etype, w in entities_per_node.get(
                    node.node_id, []
                ):
                    entity_id = entity_id_cache.get(canonical_name)
                    if entity_id:
                        self._store._upsert_link_in(
                            db, node.memory_id, entity_id, user_id, "regex", w
                        )

            # 3. Ensure graph entity nodes exist (entity_id == node_id)
            from memoria.core.memory.models.graph import GraphNode

            seen: set[str] = set()
            for ent_list in entities_per_node.values():
                for canonical_name, etype, _w in ent_list:
                    eid = entity_id_cache.get(canonical_name)
                    if not eid or eid in seen:
                        continue
                    seen.add(eid)
                    existing = db.query(GraphNode).filter_by(node_id=eid).first()
                    if not existing:
                        gnode = GraphNodeData(
                            node_id=eid,
                            user_id=user_id,
                            node_type=NodeType.ENTITY,
                            content=canonical_name.lower(),
                            entity_type=etype,
                            confidence=1.0,
                            trust_tier="T1",
                            importance=0.3,
                        )
                        from memoria.core.memory.graph.graph_store import _to_row

                        db.add(GraphNode(**_to_row(gnode)))
                        created.append(gnode)

            db.commit()

        # Entity types that should NOT participate in graph activation.
        # time: creates spurious chains (周一↔周二↔周三)
        # person: too generic as activation anchors, kept as metadata only
        _NO_GRAPH_EDGE_TYPES = {"time", "person"}

        # 4. Collect graph edges (all content nodes, including episodic)
        for node in content_nodes:
            for canonical_name, etype, w in entities_per_node.get(node.node_id, []):
                if etype in _NO_GRAPH_EDGE_TYPES:
                    continue
                ent_node_id = entity_id_cache.get(canonical_name)
                if ent_node_id:
                    pending_edges.append(
                        (node.node_id, ent_node_id, EdgeType.ENTITY_LINK.value, w)
                    )

        return created
