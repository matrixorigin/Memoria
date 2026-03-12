"""GraphStore — CRUD for graph nodes and edges (normalized edge table)."""

from __future__ import annotations

import logging
import uuid

from sqlalchemy.orm import Session

from memoria.core.memory.models.entity import Entity
from memoria.core.memory.models.graph import GraphEdge, GraphNode
from memoria.core.db_consumer import DbConsumer
from memoria.core.memory.graph.types import Edge, EdgeType, GraphNodeData, NodeType

logger = logging.getLogger(__name__)

MAX_EDGES_PER_NODE = 30


def _new_id() -> str:
    return uuid.uuid4().hex


def _to_domain(row: GraphNode) -> GraphNodeData:
    """Convert ORM row to domain object."""
    source_nodes = row.source_nodes.split(",") if row.source_nodes else []
    return GraphNodeData(
        node_id=row.node_id,
        user_id=row.user_id,
        node_type=NodeType(row.node_type),
        content=row.content,
        entity_type=row.entity_type,
        embedding=list(row.embedding) if row.embedding is not None else None,
        event_id=row.event_id,
        memory_id=row.memory_id,
        session_id=row.session_id,
        confidence=row.confidence or 0.75,
        trust_tier=row.trust_tier or "T3",
        importance=row.importance or 0.0,
        source_nodes=source_nodes,
        conflicts_with=row.conflicts_with,
        conflict_resolution=row.conflict_resolution,
        access_count=row.access_count or 0,
        cross_session_count=row.cross_session_count or 0,
        is_active=bool(row.is_active),
        superseded_by=row.superseded_by,
        created_at=str(row.created_at) if row.created_at else None,
    )


def _row_tuple_to_domain(row) -> GraphNodeData:
    """Convert a column-query Row (skeleton/partial load) to domain."""
    source_nodes = row.source_nodes.split(",") if row.source_nodes else []
    return GraphNodeData(
        node_id=row.node_id,
        user_id=row.user_id,
        node_type=NodeType(row.node_type),
        content=getattr(row, "content", ""),
        entity_type=getattr(row, "entity_type", None),
        embedding=None,
        event_id=getattr(row, "event_id", None),
        memory_id=getattr(row, "memory_id", None),
        session_id=row.session_id,
        confidence=row.confidence or 0.75,
        trust_tier=row.trust_tier or "T3",
        importance=row.importance or 0.0,
        source_nodes=source_nodes,
        conflicts_with=row.conflicts_with,
        conflict_resolution=row.conflict_resolution,
        access_count=getattr(row, "access_count", 0) or 0,
        cross_session_count=getattr(row, "cross_session_count", 0) or 0,
        is_active=bool(row.is_active),
        superseded_by=getattr(row, "superseded_by", None),
        created_at=str(row.created_at) if getattr(row, "created_at", None) else None,
    )


def _to_row(node: GraphNodeData) -> dict:
    """Convert domain object to column dict for INSERT."""
    return {
        "node_id": node.node_id,
        "user_id": node.user_id,
        "node_type": node.node_type.value
        if isinstance(node.node_type, NodeType)
        else node.node_type,
        "content": node.content,
        "entity_type": node.entity_type,
        "embedding": node.embedding,
        "event_id": node.event_id,
        "memory_id": node.memory_id,
        "session_id": node.session_id,
        "confidence": node.confidence,
        "trust_tier": node.trust_tier,
        "importance": node.importance,
        "source_nodes": ",".join(node.source_nodes) if node.source_nodes else None,
        "conflicts_with": node.conflicts_with,
        "conflict_resolution": node.conflict_resolution,
        "access_count": node.access_count,
        "cross_session_count": node.cross_session_count,
        "is_active": 1 if node.is_active else 0,
        "superseded_by": node.superseded_by,
    }


class GraphStore(DbConsumer):
    """CRUD for graph nodes + normalized edge table.

    Edges live in memory_graph_edges — no JSON adjacency lists.
    All graph traversal is DB-side.
    """

    # ── Node Create ───────────────────────────────────────────────────

    def create_node(self, node: GraphNodeData) -> GraphNodeData:
        if not node.node_id:
            node.node_id = _new_id()
        with self._db() as db:
            db.add(GraphNode(**_to_row(node)))
            db.commit()
        return node

    def create_nodes_batch(self, nodes: list[GraphNodeData]) -> list[GraphNodeData]:
        if not nodes:
            return []
        for n in nodes:
            if not n.node_id:
                n.node_id = _new_id()
        with self._db() as db:
            db.bulk_save_objects([GraphNode(**_to_row(n)) for n in nodes])
            db.commit()
        return nodes

    # ── Node Read ─────────────────────────────────────────────────────

    def get_node(self, node_id: str) -> GraphNodeData | None:
        with self._db() as db:
            row = db.query(GraphNode).filter_by(node_id=node_id).first()
            return _to_domain(row) if row else None

    def get_nodes_by_ids(self, node_ids: list[str]) -> list[GraphNodeData]:
        if not node_ids:
            return []
        with self._db() as db:
            rows = db.query(GraphNode).filter(GraphNode.node_id.in_(node_ids)).all()
            return [_to_domain(r) for r in rows]

    def get_user_nodes(
        self,
        user_id: str,
        *,
        node_type: NodeType | None = None,
        active_only: bool = True,
        load_embedding: bool = True,
    ) -> list[GraphNodeData]:
        with self._db() as db:
            if load_embedding:
                q = db.query(GraphNode).filter_by(user_id=user_id)
            else:
                cols = [c for c in GraphNode.__table__.columns if c.name != "embedding"]
                q = db.query(*cols).filter_by(user_id=user_id)
            if active_only:
                q = q.filter(GraphNode.is_active == 1)
            if node_type is not None:
                q = q.filter_by(node_type=node_type.value)
            if load_embedding:
                return [_to_domain(r) for r in q.all()]
            return [_row_tuple_to_domain(r) for r in q.all()]

    def get_node_by_event_id(self, event_id: str) -> GraphNodeData | None:
        with self._db() as db:
            row = (
                db.query(GraphNode)
                .filter_by(event_id=event_id, node_type=NodeType.EPISODIC.value)
                .first()
            )
            return _to_domain(row) if row else None

    def get_node_by_memory_id(self, memory_id: str) -> GraphNodeData | None:
        with self._db() as db:
            row = (
                db.query(GraphNode)
                .filter_by(memory_id=memory_id, node_type=NodeType.SEMANTIC.value)
                .first()
            )
            return _to_domain(row) if row else None

    def find_entity_node(self, user_id: str, entity_name: str) -> GraphNodeData | None:
        """Find an existing active entity node by case-insensitive content match."""
        with self._db() as db:
            row = (
                db.query(GraphNode)
                .filter_by(
                    user_id=user_id, node_type=NodeType.ENTITY.value, is_active=1
                )
                .filter(GraphNode.content == entity_name.lower())
                .first()
            )
            return _to_domain(row) if row else None

    def link_entities_batch(
        self,
        user_id: str,
        content_nodes: list[GraphNodeData],
        entities_per_node: dict[str, list[tuple[str, str]]],
        *,
        source: str = "regex",
    ) -> tuple[list[GraphNodeData], list[tuple[str, str, str, float]], int]:
        """Unified entity linking: dual-write entity tables + graph nodes/edges.

        Writes to all four tables in a single transaction:
        1. mem_entities (source of truth)
        2. mem_memory_entity_links (for semantic nodes with memory_id)
        3. memory_graph_nodes (entity nodes, entity_id == node_id)
        4. memory_graph_edges (collected as pending_edges, committed by caller)

        Returns:
            (created_entity_nodes, pending_edges, reused_count).
        """
        _WEIGHT = {"regex": 0.8, "llm": 0.9, "manual": 1.0}
        weight = _WEIGHT.get(source, 1.0)
        _IMPORTANCE = {"regex": 0.3, "llm": 0.4, "manual": 0.5}
        importance = _IMPORTANCE.get(source, 0.4)

        entity_cache: dict[str, str] = {}
        created: list[GraphNodeData] = []
        reused = 0
        pending_edges: list[tuple[str, str, str, float]] = []

        with self._db() as db:
            # 1. Upsert mem_entities + cache entity_id
            for node in content_nodes:
                for canonical_name, etype in entities_per_node.get(node.node_id, []):
                    if canonical_name not in entity_cache:
                        eid = self._upsert_entity_in(
                            db, user_id, canonical_name, canonical_name, etype
                        )
                        # Check if graph node already exists (reused vs new)
                        existing_graph = (
                            db.query(GraphNode).filter_by(node_id=eid).first()
                        )
                        if existing_graph:
                            reused += 1
                        else:
                            ent_node = GraphNodeData(
                                node_id=eid,
                                user_id=user_id,
                                node_type=NodeType.ENTITY,
                                content=canonical_name.lower(),
                                entity_type=etype,
                                confidence=1.0,
                                trust_tier="T1",
                                importance=importance,
                            )
                            db.add(GraphNode(**_to_row(ent_node)))
                            created.append(ent_node)
                        entity_cache[canonical_name] = eid

            # 2. Write mem_memory_entity_links for semantic nodes
            for node in content_nodes:
                if not node.memory_id:
                    continue
                for canonical_name, _etype in entities_per_node.get(node.node_id, []):
                    eid = entity_cache.get(canonical_name)
                    if eid:
                        self._upsert_link_in(
                            db, node.memory_id, eid, user_id, source, weight
                        )

            # 3. Collect pending edges (caller commits via add_edges_batch)
            for node in content_nodes:
                for canonical_name, _etype in entities_per_node.get(node.node_id, []):
                    eid = entity_cache.get(canonical_name)
                    if eid:
                        pending_edges.append(
                            (node.node_id, eid, EdgeType.ENTITY_LINK.value, weight)
                        )

            db.commit()

        return created, pending_edges, reused

    def count_user_nodes(self, user_id: str) -> int:
        with self._db() as db:
            return db.query(GraphNode).filter_by(user_id=user_id, is_active=1).count()

    def has_min_nodes(self, user_id: str, minimum: int) -> bool:
        """Check if user has at least `minimum` active nodes without full COUNT(*)."""
        with self._db() as db:
            rows = (
                db.query(GraphNode.node_id)
                .filter_by(user_id=user_id, is_active=1)
                .limit(minimum)
                .all()
            )
            return len(rows) >= minimum

    # ── Vector Search ─────────────────────────────────────────────────

    def find_similar_nodes(
        self,
        user_id: str,
        embedding: list[float],
        *,
        top_k: int = 5,
        node_type: NodeType | None = None,
    ) -> list[GraphNodeData]:
        from matrixone.sqlalchemy_ext import l2_distance

        with self._db() as db:
            dist = l2_distance(GraphNode.embedding, embedding)
            q = (
                db.query(GraphNode)
                .filter_by(user_id=user_id, is_active=1)
                .filter(GraphNode.embedding.isnot(None))
            )
            if node_type is not None:
                q = q.filter_by(node_type=node_type.value)
            return [_to_domain(r) for r in q.order_by(dist).limit(top_k).all()]

    def find_similar_with_scores(
        self,
        user_id: str,
        embedding: list[float],
        *,
        top_k: int = 5,
        node_type: NodeType | None = None,
    ) -> list[tuple[GraphNodeData, float]]:
        """Top-K nodes with cosine similarity (DB-side)."""
        from matrixone.sqlalchemy_ext import cosine_distance

        with self._db() as db:
            cos_dist = cosine_distance(GraphNode.embedding, embedding)
            cos_sim = (1.0 - cos_dist).label("cos_sim")
            q = (
                db.query(GraphNode, cos_sim)
                .filter_by(user_id=user_id, is_active=1)
                .filter(GraphNode.embedding.isnot(None))
            )
            if node_type is not None:
                q = q.filter_by(node_type=node_type.value)
            return [
                (_to_domain(row), float(sim))
                for row, sim in q.order_by(cos_dist).limit(top_k).all()
            ]

    def get_pair_similarity(self, node_a_id: str, node_b_id: str) -> float | None:
        """Cosine similarity between two nodes (single DB query, self-join)."""
        from matrixone.sqlalchemy_ext import cosine_distance
        from sqlalchemy.orm import aliased

        with self._db() as db:
            A = aliased(GraphNode, name="a")
            B = aliased(GraphNode, name="b")
            result = (
                db.query((1.0 - cosine_distance(A.embedding, B.embedding)).label("sim"))
                .filter(A.node_id == node_a_id, B.node_id == node_b_id)
                .filter(A.embedding.isnot(None), B.embedding.isnot(None))
                .first()
            )
            return float(result.sim) if result else None

    def fulltext_search(
        self,
        user_id: str,
        query: str,
        *,
        top_k: int = 10,
    ) -> list[tuple[GraphNodeData, float]]:
        """Fulltext (BM25-like) search on memory_graph_nodes.content.

        Uses MatrixOne's boolean_match for fulltext index (ft_graph_content).
        Returns (node, relevance_score) pairs.
        Gracefully returns [] if fulltext is unavailable.
        """
        if not query or not query.strip():
            return []
        import re as _re

        safe = _re.sub(r"[+\-<>()~*\"@]", " ", query).strip()
        if not safe:
            return []

        try:
            from matrixone.sqlalchemy_ext import boolean_match
            from sqlalchemy import literal_column

            with self._db() as db:
                ft = boolean_match("content").must(safe)
                ft_sql = ft.compile()
                ft_score = literal_column(str(ft_sql)).label("ft_score")
                rows = (
                    db.query(GraphNode, ft_score)
                    .filter_by(user_id=user_id, is_active=1)
                    .filter(ft)
                    .order_by(ft_score.desc())
                    .limit(top_k)
                    .all()
                )
                return [(_to_domain(row), float(score)) for row, score in rows]
        except Exception:
            logger.debug("Fulltext search failed", exc_info=True)
            return []

    def get_pairs_similarity_batch(
        self,
        pairs: list[tuple[str, str]],
    ) -> dict[tuple[str, str], float]:
        """Cosine similarity for multiple node pairs — DB-side computation.

        Single query with self-join on the specific pair IDs.
        Returns dict keyed by (node_a_id, node_b_id) → cosine_similarity.
        """
        if not pairs:
            return {}
        from matrixone.sqlalchemy_ext import cosine_distance
        from sqlalchemy.orm import aliased

        a_ids = [p[0] for p in pairs]
        b_ids = [p[1] for p in pairs]
        pair_set = set(pairs)

        with self._db() as db:
            A = aliased(GraphNode, name="a")
            B = aliased(GraphNode, name="b")
            rows = (
                db.query(
                    A.node_id.label("a_id"),
                    B.node_id.label("b_id"),
                    (1.0 - cosine_distance(A.embedding, B.embedding)).label("sim"),
                )
                .filter(A.node_id.in_(a_ids), B.node_id.in_(b_ids))
                .filter(A.embedding.isnot(None), B.embedding.isnot(None))
                .all()
            )
        return {
            (r.a_id, r.b_id): float(r.sim) for r in rows if (r.a_id, r.b_id) in pair_set
        }

    # ── Edge Operations (normalized table) ────────────────────────────

    def add_edges_batch(
        self,
        edges: list[tuple[str, str, str, float]],
        user_id: str,
    ) -> None:
        """Insert edges, ignoring duplicates (composite PK).

        Uses INSERT ... ON DUPLICATE KEY UPDATE — avoids N SELECT round-trips.
        """
        if not edges:
            return
        with self._db() as db:
            from sqlalchemy import text as sa_text

            # Build multi-value INSERT to avoid N round-trips.
            # ON DUPLICATE KEY UPDATE handles composite-PK conflicts.
            placeholders = ", ".join(
                f"(:src{i}, :tgt{i}, :etype{i}, :w{i}, :uid)" for i in range(len(edges))
            )
            params: dict = {"uid": user_id}
            for i, (src, tgt, etype, weight) in enumerate(edges):
                params[f"src{i}"] = src
                params[f"tgt{i}"] = tgt
                params[f"etype{i}"] = etype
                params[f"w{i}"] = weight
            db.execute(
                sa_text(
                    "INSERT INTO memory_graph_edges "
                    "(source_id, target_id, edge_type, weight, user_id) "
                    f"VALUES {placeholders} "
                    "ON DUPLICATE KEY UPDATE weight = VALUES(weight)"
                ),
                params,
            )
            db.commit()

    def get_outgoing_edges(self, node_id: str) -> list[Edge]:
        """All outgoing edges from a node."""
        with self._db() as db:
            rows = db.query(GraphEdge).filter_by(source_id=node_id).all()
            return [Edge(r.target_id, r.edge_type, r.weight) for r in rows]

    def get_incoming_edges(self, node_id: str) -> list[Edge]:
        """All incoming edges to a node."""
        with self._db() as db:
            rows = db.query(GraphEdge).filter_by(target_id=node_id).all()
            return [Edge(r.source_id, r.edge_type, r.weight) for r in rows]

    def get_edges_for_nodes(self, node_ids: set[str]) -> dict[str, list[Edge]]:
        """Batch: all outgoing edges for a set of nodes. Single query."""
        if not node_ids:
            return {}
        with self._db() as db:
            rows = (
                db.query(GraphEdge)
                .filter(GraphEdge.source_id.in_(list(node_ids)))
                .all()
            )
            result: dict[str, list[Edge]] = {nid: [] for nid in node_ids}
            for r in rows:
                result[r.source_id].append(Edge(r.target_id, r.edge_type, r.weight))
            return result

    def get_edges_bidirectional(
        self,
        node_ids: set[str],
    ) -> tuple[dict[str, list[Edge]], dict[str, list[Edge]]]:
        """Batch: incoming AND outgoing edges for a set of nodes.

        Two indexed queries (each uses its own index efficiently).
        Returns (incoming, outgoing).
        """
        if not node_ids:
            return {}, {}
        id_list = list(node_ids)
        with self._db() as db:
            # Query 1: outgoing (uses PK: source_id leading column)
            out_rows = (
                db.query(GraphEdge).filter(GraphEdge.source_id.in_(id_list)).all()
            )
            # Query 2: incoming (uses idx_edge_target)
            in_rows = db.query(GraphEdge).filter(GraphEdge.target_id.in_(id_list)).all()
            incoming: dict[str, list[Edge]] = {nid: [] for nid in node_ids}
            outgoing: dict[str, list[Edge]] = {nid: [] for nid in node_ids}
            for r in out_rows:
                outgoing[r.source_id].append(Edge(r.target_id, r.edge_type, r.weight))
            for r in in_rows:
                incoming[r.target_id].append(Edge(r.source_id, r.edge_type, r.weight))
            return incoming, outgoing

    def get_incoming_for_nodes(self, node_ids: set[str]) -> dict[str, list[Edge]]:
        """Batch: all incoming edges for a set of nodes. Single query."""
        if not node_ids:
            return {}
        with self._db() as db:
            rows = (
                db.query(GraphEdge)
                .filter(GraphEdge.target_id.in_(list(node_ids)))
                .all()
            )
            result: dict[str, list[Edge]] = {nid: [] for nid in node_ids}
            for r in rows:
                result[r.target_id].append(Edge(r.source_id, r.edge_type, r.weight))
            return result

    def get_neighbor_ids(self, node_ids: set[str]) -> set[str]:
        """All 1-hop neighbor IDs (both directions). Single query."""
        if not node_ids:
            return set()
        with self._db() as db:
            out_rows = (
                db.query(GraphEdge.target_id)
                .filter(GraphEdge.source_id.in_(list(node_ids)))
                .all()
            )
            in_rows = (
                db.query(GraphEdge.source_id)
                .filter(GraphEdge.target_id.in_(list(node_ids)))
                .all()
            )
            return {r[0] for r in out_rows} | {r[0] for r in in_rows}

    def get_user_edge_count(self, user_id: str) -> int:
        with self._db() as db:
            return db.query(GraphEdge).filter_by(user_id=user_id).count()

    def get_association_edges(
        self, user_id: str, min_weight: float = 0.0
    ) -> list[tuple[str, str, float]]:
        """All association edges for a user. For consolidation conflict scan."""
        with self._db() as db:
            rows = (
                db.query(GraphEdge.source_id, GraphEdge.target_id, GraphEdge.weight)
                .filter_by(user_id=user_id, edge_type="association")
                .filter(GraphEdge.weight >= min_weight)
                .all()
            )
            return [(r.source_id, r.target_id, r.weight) for r in rows]

    def get_association_edges_with_current_sim(
        self,
        user_id: str,
        *,
        min_edge_weight: float = 0.7,
        max_current_sim: float = 0.4,
    ) -> list[tuple[str, str, float, float]]:
        """Association edges where historical weight is high but current cosine sim is low.

        Single DB query: JOIN edges with nodes, compute cosine_distance inline.
        Returns list of (source_id, target_id, edge_weight, current_cosine_sim).
        """
        from matrixone.sqlalchemy_ext import cosine_distance
        from sqlalchemy.orm import aliased

        with self._db() as db:
            A = aliased(GraphNode, name="a")
            B = aliased(GraphNode, name="b")
            cur_sim = (1.0 - cosine_distance(A.embedding, B.embedding)).label("cur_sim")
            rows = (
                db.query(
                    GraphEdge.source_id,
                    GraphEdge.target_id,
                    GraphEdge.weight,
                    cur_sim,
                )
                .join(A, A.node_id == GraphEdge.source_id)
                .join(B, B.node_id == GraphEdge.target_id)
                .filter(
                    GraphEdge.user_id == user_id,
                    GraphEdge.edge_type == "association",
                    GraphEdge.weight >= min_edge_weight,
                    A.embedding.isnot(None),
                    B.embedding.isnot(None),
                )
                .all()
            )
        return [
            (r.source_id, r.target_id, float(r.weight), float(r.cur_sim))
            for r in rows
            if float(r.cur_sim) < max_current_sim
        ]

    # ── Node Update ───────────────────────────────────────────────────

    def deactivate_node(
        self, node_id: str, *, superseded_by: str | None = None
    ) -> None:
        with self._db() as db:
            updates: dict = {"is_active": 0}
            if superseded_by:
                updates["superseded_by"] = superseded_by
            db.query(GraphNode).filter_by(node_id=node_id).update(updates)
            db.commit()

    def update_importance(self, node_id: str, importance: float) -> None:
        with self._db() as db:
            db.query(GraphNode).filter_by(node_id=node_id).update(
                {"importance": importance}
            )
            db.commit()

    def update_confidence(self, node_id: str, confidence: float) -> None:
        with self._db() as db:
            db.query(GraphNode).filter_by(node_id=node_id).update(
                {"confidence": confidence}
            )
            db.commit()

    def update_confidence_and_tier(
        self,
        node_id: str,
        confidence: float,
        trust_tier: str,
    ) -> None:
        with self._db() as db:
            db.query(GraphNode).filter_by(node_id=node_id).update(
                {
                    "confidence": confidence,
                    "trust_tier": trust_tier,
                }
            )
            db.commit()

    def mark_conflict(
        self,
        older_id: str,
        newer_id: str,
        *,
        confidence_factor: float = 0.5,
        old_confidence: float = 0.75,
    ) -> None:
        """Atomic conflict marking — single transaction."""
        with self._db() as db:
            db.query(GraphNode).filter_by(node_id=older_id).update(
                {
                    "confidence": old_confidence * confidence_factor,
                    "conflicts_with": newer_id,
                    "conflict_resolution": "superseded",
                }
            )
            db.query(GraphNode).filter_by(node_id=newer_id).update(
                {
                    "conflict_resolution": "kept",
                }
            )
            db.commit()

    # ── Session-level queries ─────────────────────────────────────────

    def get_latest_episodic_in_session(
        self, user_id: str, session_id: str
    ) -> GraphNodeData | None:
        with self._db() as db:
            row = (
                db.query(GraphNode)
                .filter_by(
                    user_id=user_id,
                    session_id=session_id,
                    node_type=NodeType.EPISODIC.value,
                    is_active=1,
                )
                .order_by(GraphNode.created_at.desc())
                .first()
            )
            return _to_domain(row) if row else None

    def delete_user_data(self, user_id: str) -> None:
        """Remove all graph nodes, edges, entities, and entity links for a user."""
        with self._db() as db:
            from sqlalchemy import text as sa_text

            db.execute(
                sa_text("DELETE FROM mem_memory_entity_links WHERE user_id = :uid"),
                {"uid": user_id},
            )
            db.execute(
                sa_text("DELETE FROM mem_entities WHERE user_id = :uid"),
                {"uid": user_id},
            )
            db.execute(
                sa_text("DELETE FROM memory_graph_edges WHERE user_id = :uid"),
                {"uid": user_id},
            )
            db.execute(
                sa_text("DELETE FROM memory_graph_nodes WHERE user_id = :uid"),
                {"uid": user_id},
            )
            db.commit()

    # ── Entity Table Operations ───────────────────────────────────────

    def upsert_entity(
        self,
        user_id: str,
        name: str,
        display_name: str,
        entity_type: str,
        *,
        session: Session | None = None,
    ) -> str:
        """Upsert into mem_entities. Returns entity_id (existing or new).

        If ``session`` is provided, uses it without committing (caller owns tx).
        Otherwise opens a new session and commits.
        """
        if session is not None:
            return self._upsert_entity_in(
                session, user_id, name, display_name, entity_type
            )
        with self._db() as db:
            eid = self._upsert_entity_in(db, user_id, name, display_name, entity_type)
            db.commit()
            return eid

    @staticmethod
    def _upsert_entity_in(
        db: Session, user_id: str, name: str, display_name: str, entity_type: str
    ) -> str:
        existing = db.query(Entity).filter_by(user_id=user_id, name=name).first()
        if existing:
            return existing.entity_id
        entity_id = _new_id()
        db.add(
            Entity(
                entity_id=entity_id,
                user_id=user_id,
                name=name,
                display_name=display_name,
                entity_type=entity_type,
            )
        )
        db.flush()
        return entity_id

    def upsert_memory_entity_link(
        self,
        memory_id: str,
        entity_id: str,
        user_id: str,
        *,
        source: str = "regex",
        weight: float = 0.8,
        session: Session | None = None,
    ) -> None:
        """Insert link into mem_memory_entity_links, skip if exists.

        If ``session`` is provided, uses it without committing (caller owns tx).
        """
        if session is not None:
            self._upsert_link_in(session, memory_id, entity_id, user_id, source, weight)
            return
        with self._db() as db:
            self._upsert_link_in(db, memory_id, entity_id, user_id, source, weight)
            db.commit()

    @staticmethod
    def _upsert_link_in(
        db: Session,
        memory_id: str,
        entity_id: str,
        user_id: str,
        source: str,
        weight: float,
    ) -> None:
        from sqlalchemy import text as sa_text

        db.execute(
            sa_text(
                "INSERT INTO mem_memory_entity_links "
                "(memory_id, entity_id, user_id, source, weight) "
                "VALUES (:mid, :eid, :uid, :src, :w) "
                "ON DUPLICATE KEY UPDATE weight = VALUES(weight)"
            ),
            {
                "mid": memory_id,
                "eid": entity_id,
                "uid": user_id,
                "src": source,
                "w": weight,
            },
        )

    def get_memories_by_entity(
        self,
        entity_id: str,
        user_id: str,
        *,
        limit: int = 50,
    ) -> list[tuple[str, float]]:
        """Reverse lookup: entity → memory_ids with weights via mem_memory_entity_links.

        Returns list of (memory_id, weight) ordered by weight DESC.
        """
        with self._db() as db:
            from sqlalchemy import text as sa_text

            rows = db.execute(
                sa_text(
                    "SELECT l.memory_id, l.weight "
                    "FROM mem_memory_entity_links l "
                    "JOIN mem_memories m ON l.memory_id = m.memory_id "
                    "WHERE l.entity_id = :eid AND l.user_id = :uid AND m.is_active = 1 "
                    "ORDER BY l.weight DESC, m.created_at DESC "
                    "LIMIT :lim"
                ),
                {"eid": entity_id, "uid": user_id, "lim": limit},
            ).fetchall()
            return [(r[0], float(r[1])) for r in rows]

    def find_entity_by_name(self, user_id: str, name: str) -> str | None:
        """Find entity_id in mem_entities by exact name match."""
        with self._db() as db:
            row = (
                db.query(Entity.entity_id).filter_by(user_id=user_id, name=name).first()
            )
            return row[0] if row else None

    def get_user_entities(self, user_id: str) -> list[tuple[str, str, str]]:
        """List all entities for a user. Returns [(entity_id, name, entity_type)]."""
        with self._db() as db:
            rows = (
                db.query(Entity.entity_id, Entity.name, Entity.entity_type)
                .filter_by(user_id=user_id)
                .all()
            )
            return [(r[0], r[1], r[2]) for r in rows]

    def repair_entity_graph_consistency(self, user_id: str) -> dict[str, int]:
        """Scan mem_entities vs memory_graph_nodes(node_type='entity') and fix gaps.

        Invariant: entity_id in mem_entities == node_id in memory_graph_nodes.
        - Entity in table but no graph node with that ID → create graph node
        - Graph entity node whose node_id is NOT in mem_entities → adopt or deactivate

        Returns counts of repairs made.
        """
        repaired_graph = 0
        repaired_table = 0

        with self._db() as db:
            table_entities = (
                db.query(Entity.entity_id, Entity.name, Entity.entity_type)
                .filter_by(user_id=user_id)
                .all()
            )
            table_map = {r[0]: (r[1], r[2]) for r in table_entities}

            graph_entities = (
                db.query(GraphNode.node_id, GraphNode.content, GraphNode.entity_type)
                .filter_by(
                    user_id=user_id, node_type=NodeType.ENTITY.value, is_active=1
                )
                .all()
            )
            graph_map = {r[0]: (r[1], r[2]) for r in graph_entities}

        # Table entity missing graph node → create graph node with same ID
        for eid, (name, etype) in table_map.items():
            if eid not in graph_map:
                self.create_node(
                    GraphNodeData(
                        node_id=eid,
                        user_id=user_id,
                        node_type=NodeType.ENTITY,
                        content=name,
                        entity_type=etype or "concept",
                        confidence=1.0,
                        trust_tier="T1",
                        importance=0.3,
                    )
                )
                repaired_graph += 1

        # Graph entity node not in table → create entity row or deactivate orphan
        for nid, (content, etype) in graph_map.items():
            if nid not in table_map:
                with self._db() as db:
                    existing = (
                        db.query(Entity.entity_id)
                        .filter_by(user_id=user_id, name=content)
                        .first()
                    )
                if existing:
                    # Name already exists with different ID — orphan graph node
                    self.deactivate_node(nid)
                else:
                    with self._db() as db:
                        db.add(
                            Entity(
                                entity_id=nid,
                                user_id=user_id,
                                name=content,
                                display_name=content,
                                entity_type=etype or "concept",
                            )
                        )
                        db.commit()
                    repaired_table += 1

        return {"repaired_graph": repaired_graph, "repaired_table": repaired_table}
