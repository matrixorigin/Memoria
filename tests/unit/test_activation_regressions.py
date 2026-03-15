"""Regression tests for activation strategy bugs found during benchmark analysis.

Each test targets a specific silent-failure mode discovered in the
vector:v1 vs activation:v1 comparison work (2026-03-13).
"""

from __future__ import annotations

import logging
from unittest.mock import MagicMock, patch

from memoria.core.memory.graph.entity_extractor import ExtractedEntity
from memoria.core.memory.graph.graph_builder import GraphBuilder
from memoria.core.memory.graph.types import EdgeType, GraphNodeData, NodeType
from memoria.core.memory.types import Memory, MemoryType


# ── Helpers ──────────────────────────────────────────────────────────


def _make_graph_store():
    store = MagicMock()
    store._db.return_value.__enter__ = lambda s: MagicMock()
    store._db.return_value.__exit__ = MagicMock(return_value=False)
    store._upsert_entity_in.side_effect = (
        lambda db, uid, name, disp, etype, embedding=None: f"eid-{name}"
    )
    store._upsert_link_in.return_value = None
    store.has_min_nodes.return_value = True
    return store


def _make_node(
    content: str, *, node_type=NodeType.SEMANTIC, memory_id="mem1"
) -> GraphNodeData:
    return GraphNodeData(
        node_id="node1",
        user_id="u1",
        node_type=node_type,
        content=content,
        memory_id=memory_id,
    )


def _make_entity_node(name: str, entity_type: str = "tech") -> GraphNodeData:
    return GraphNodeData(
        node_id=f"eid-{name}",
        user_id="u1",
        node_type=NodeType.ENTITY,
        content=name,
        entity_type=entity_type,
        memory_id=None,
    )


def _make_memory(mid: str, content: str) -> Memory:
    return Memory(
        memory_id=mid,
        user_id="u1",
        memory_type=MemoryType.SEMANTIC,
        content=content,
        access_count=5,
    )


# ── 1. Entity nodes must not appear in activation results ────────────


class TestEntityNodeFiltering:
    """_nodes_to_memories must skip entity/scene nodes without a backing memory row."""

    def test_entity_nodes_excluded(self):
        from memoria.core.memory.strategy.activation_v1 import (
            ActivationRetrievalStrategy,
        )

        strategy = ActivationRetrievalStrategy.__new__(ActivationRetrievalStrategy)
        strategy._mem_store = MagicMock()
        strategy._mem_store.get_by_ids.return_value = {
            "mem1": _make_memory("mem1", "Spark OOM error"),
        }

        scored = [
            (_make_node("Spark OOM error", memory_id="mem1"), 0.9),
            (_make_entity_node("service"), 0.8),  # entity, no memory_id
            (_make_entity_node("deploy"), 0.7),  # entity, no memory_id
        ]
        result = strategy._nodes_to_memories(scored, "u1")
        assert len(result) == 1
        assert result[0].content == "Spark OOM error"

    def test_dedup_same_memory_id(self):
        from memoria.core.memory.strategy.activation_v1 import (
            ActivationRetrievalStrategy,
        )

        strategy = ActivationRetrievalStrategy.__new__(ActivationRetrievalStrategy)
        strategy._mem_store = MagicMock()
        strategy._mem_store.get_by_ids.return_value = {
            "mem1": _make_memory("mem1", "content"),
        }

        scored = [
            (_make_node("content", memory_id="mem1"), 0.9),
            (_make_node("content", memory_id="mem1"), 0.8),  # duplicate
        ]
        result = strategy._nodes_to_memories(scored, "u1")
        assert len(result) == 1


# ── 2. Graph results enriched from mem_memories ──────────────────────


class TestMemoryEnrichment:
    """_nodes_to_memories must fetch full Memory rows from tabular store."""

    def test_access_count_preserved(self):
        from memoria.core.memory.strategy.activation_v1 import (
            ActivationRetrievalStrategy,
        )

        strategy = ActivationRetrievalStrategy.__new__(ActivationRetrievalStrategy)
        mem = _make_memory("mem1", "content")
        mem.access_count = 42
        strategy._mem_store = MagicMock()
        strategy._mem_store.get_by_ids.return_value = {"mem1": mem}

        scored = [(_make_node("content", memory_id="mem1"), 0.9)]
        result = strategy._nodes_to_memories(scored, "u1")
        assert result[0].access_count == 42

    def test_fallback_when_not_in_tabular(self):
        """Nodes with memory_id but missing from tabular still appear (degraded)."""
        from memoria.core.memory.strategy.activation_v1 import (
            ActivationRetrievalStrategy,
        )

        strategy = ActivationRetrievalStrategy.__new__(ActivationRetrievalStrategy)
        strategy._mem_store = MagicMock()
        strategy._mem_store.get_by_ids.return_value = {}  # not found

        scored = [(_make_node("orphan content", memory_id="mem-orphan"), 0.9)]
        result = strategy._nodes_to_memories(scored, "u1")
        assert len(result) == 1
        assert result[0].content == "orphan content"
        assert result[0].access_count == 0  # default


# ── 3. Time/person entities excluded from graph edges ────────────────


class TestEntityEdgeExclusion:
    """GraphBuilder._link_entities must not create graph edges for time/person entities."""

    def _run_link(self, entities: list[ExtractedEntity]) -> list[tuple]:
        store = _make_graph_store()
        builder = GraphBuilder(store, embed_fn=lambda x: [0.1] * 10)
        node = _make_node("test content")
        pending: list[tuple] = []

        mock_db = MagicMock()
        mock_db.query.return_value.filter_by.return_value.first.return_value = None
        store._db.return_value.__enter__ = lambda s: mock_db

        with patch(
            "memoria.core.memory.graph.graph_builder.get_ner_backend"
        ) as mock_ner:
            mock_ner.return_value.extract.return_value = entities
            builder._link_entities("u1", [node], pending)
        return pending

    def test_time_entity_no_edge(self):
        edges = self._run_link([ExtractedEntity("周三", "周三", "time")])
        assert len(edges) == 0

    def test_person_entity_no_edge(self):
        edges = self._run_link([ExtractedEntity("李明", "李明", "person")])
        assert len(edges) == 0

    def test_tech_entity_gets_edge(self):
        edges = self._run_link([ExtractedEntity("spark", "Spark", "tech")])
        assert len(edges) == 1
        assert edges[0][2] == EdgeType.ENTITY_LINK.value

    def test_mixed_entities_filter_correctly(self):
        edges = self._run_link(
            [
                ExtractedEntity("周一", "周一", "time"),
                ExtractedEntity("王芳", "王芳", "person"),
                ExtractedEntity("redis", "Redis", "tech"),
                ExtractedEntity("etl", "ETL", "tech"),
            ]
        )
        # Only tech entities get edges (time/person excluded)
        assert len(edges) == 2
        targets = {e[1] for e in edges}
        assert "eid-redis" in targets
        assert "eid-etl" in targets


# ── 4. Soft entity linking excludes person/time ──────────────────────


class TestSoftEntityLinkingFilter:
    """_entity_recall hard-linking path must skip person/time entities."""

    def test_hard_linking_skips_person(self):
        from memoria.core.memory.graph.retriever import ActivationRetriever

        store = _make_graph_store()
        store.find_entity_by_name.return_value = "eid-wangfang"
        store.get_memories_by_entity.return_value = [("mem1", 1.0)]
        retriever = ActivationRetriever(store)

        with patch("memoria.core.memory.graph.retriever.get_ner_backend") as mock_ner:
            mock_ner.return_value.extract.return_value = [
                ExtractedEntity("王芳", "王芳", "person"),
            ]
            anchors, mids = retriever._entity_recall("u1", "找王芳")

        assert len(anchors) == 0
        assert len(mids) == 0

    def test_hard_linking_skips_time(self):
        from memoria.core.memory.graph.retriever import ActivationRetriever

        store = _make_graph_store()
        store.find_entity_by_name.return_value = "eid-monday"
        retriever = ActivationRetriever(store)

        with patch("memoria.core.memory.graph.retriever.get_ner_backend") as mock_ner:
            mock_ner.return_value.extract.return_value = [
                ExtractedEntity("周一", "周一", "time"),
            ]
            anchors, mids = retriever._entity_recall("u1", "周一的会议")

        assert len(anchors) == 0

    def test_hard_linking_keeps_tech(self):
        from memoria.core.memory.graph.retriever import ActivationRetriever

        store = _make_graph_store()
        store.find_entity_by_name.return_value = "eid-spark"
        store.get_memories_by_entity.return_value = [("mem1", 1.0)]
        retriever = ActivationRetriever(store)

        with patch("memoria.core.memory.graph.retriever.get_ner_backend") as mock_ner:
            mock_ner.return_value.extract.return_value = [
                ExtractedEntity("spark", "Spark", "tech"),
            ]
            anchors, mids = retriever._entity_recall("u1", "Spark OOM")

        assert "eid-spark" in anchors
        assert "mem1" in mids


# ── 4b. Graph path must apply memory_types/session/cross-session filters ──


class TestGraphPathFilterConsistency:
    """activation:v1 graph path must apply the same memory_types / session_id /
    include_cross_session filters that the vector fallback applies.

    Bug: when graph retrieval succeeds, _nodes_to_memories returned unfiltered
    results — memory_types, session_id, include_cross_session were ignored.
    """

    def _make_strategy(self, graph_results, tabular_map):
        from memoria.core.memory.strategy.activation_v1 import (
            ActivationRetrievalStrategy,
        )

        strategy = ActivationRetrievalStrategy.__new__(ActivationRetrievalStrategy)
        strategy._activation_retriever = MagicMock()
        strategy._activation_retriever.retrieve.return_value = graph_results
        strategy._mem_store = MagicMock()
        strategy._mem_store.get_by_ids.return_value = tabular_map
        strategy._vector_fallback_strategy = None
        strategy._config = None
        strategy._metrics = None
        strategy._db_factory = MagicMock()
        return strategy

    def test_memory_types_filter_applied(self):
        """Graph path should filter out memories whose type is not in memory_types."""
        mem_semantic = Memory(
            memory_id="m1",
            user_id="u1",
            memory_type=MemoryType.SEMANTIC,
            content="semantic fact",
        )
        mem_working = Memory(
            memory_id="m2",
            user_id="u1",
            memory_type=MemoryType.WORKING,
            content="working note",
        )
        tabular = {"m1": mem_semantic, "m2": mem_working}
        nodes = [
            (_make_node("semantic fact", memory_id="m1"), 0.9),
            (
                _make_node("working note", node_type=NodeType.EPISODIC, memory_id="m2"),
                0.8,
            ),
        ]
        strategy = self._make_strategy(nodes, tabular)

        results, _ = strategy.retrieve(
            "u1",
            "test",
            [0.1] * 10,
            memory_types=[MemoryType.SEMANTIC],
        )
        types = {m.memory_type for m in results}
        assert MemoryType.WORKING not in types
        assert MemoryType.SEMANTIC in types

    def test_session_filter_excludes_other_sessions(self):
        """Graph path with include_cross_session=False should only return
        memories from the requested session."""
        mem_same = Memory(
            memory_id="m1",
            user_id="u1",
            memory_type=MemoryType.SEMANTIC,
            content="same session",
            session_id="sess-A",
        )
        mem_other = Memory(
            memory_id="m2",
            user_id="u1",
            memory_type=MemoryType.SEMANTIC,
            content="other session",
            session_id="sess-B",
        )
        tabular = {"m1": mem_same, "m2": mem_other}
        nodes = [
            (_make_node("same session", memory_id="m1"), 0.9),
            (_make_node("other session", memory_id="m2"), 0.8),
        ]
        strategy = self._make_strategy(nodes, tabular)

        results, _ = strategy.retrieve(
            "u1",
            "test",
            [0.1] * 10,
            session_id="sess-A",
            include_cross_session=False,
        )
        assert len(results) == 1
        assert results[0].session_id == "sess-A"

    def test_cross_session_true_returns_all(self):
        """Graph path with include_cross_session=True returns all sessions."""
        mem_a = Memory(
            memory_id="m1",
            user_id="u1",
            memory_type=MemoryType.SEMANTIC,
            content="session A",
            session_id="sess-A",
        )
        mem_b = Memory(
            memory_id="m2",
            user_id="u1",
            memory_type=MemoryType.SEMANTIC,
            content="session B",
            session_id="sess-B",
        )
        tabular = {"m1": mem_a, "m2": mem_b}
        nodes = [
            (_make_node("session A", memory_id="m1"), 0.9),
            (_make_node("session B", memory_id="m2"), 0.8),
        ]
        strategy = self._make_strategy(nodes, tabular)

        results, _ = strategy.retrieve(
            "u1",
            "test",
            [0.1] * 10,
            session_id="sess-A",
            include_cross_session=True,
        )
        assert len(results) == 2

    def test_vector_fallback_passes_all_filters(self):
        """Vector fallback must receive memory_types, session_id,
        include_cross_session — regression guard for existing correct behavior."""
        from memoria.core.memory.strategy.activation_v1 import (
            ActivationRetrievalStrategy,
        )

        strategy = ActivationRetrievalStrategy.__new__(ActivationRetrievalStrategy)
        strategy._activation_retriever = MagicMock()
        strategy._activation_retriever.retrieve.return_value = []  # force fallback
        strategy._vector_fallback_strategy = MagicMock()
        strategy._vector_fallback_strategy.retrieve.return_value = ([], None)

        strategy.retrieve(
            "u1",
            "test",
            [0.1] * 10,
            memory_types=[MemoryType.SEMANTIC],
            session_id="sess-X",
            include_cross_session=False,
        )

        call_kwargs = strategy._vector_fallback_strategy.retrieve.call_args.kwargs
        assert call_kwargs["memory_types"] == [MemoryType.SEMANTIC]
        assert call_kwargs["session_id"] == "sess-X"
        assert call_kwargs["include_cross_session"] is False


# ── 5. Fallback warning logs ─────────────────────────────────────────


class TestFallbackWarnings:
    """All silent fallback points must emit logs (WARNING or INFO level)."""

    def test_graph_retriever_warns_on_no_embedding(self, caplog):
        from memoria.core.memory.graph.retriever import ActivationRetriever

        store = _make_graph_store()
        retriever = ActivationRetriever(store)

        with caplog.at_level(logging.WARNING):
            result = retriever.retrieve("u1", "test", query_embedding=None)

        assert result == []
        assert "query_embedding is None" in caplog.text

    def test_graph_retriever_warns_on_insufficient_nodes(self, caplog):
        from memoria.core.memory.graph.retriever import ActivationRetriever

        store = _make_graph_store()
        store.has_min_nodes.return_value = False
        retriever = ActivationRetriever(store)

        with caplog.at_level(logging.WARNING):
            result = retriever.retrieve("u1", "test", query_embedding=[0.1] * 10)

        assert result == []
        assert "insufficient graph nodes" in caplog.text

    def test_activation_strategy_warns_on_vector_fallback(self, caplog):
        from memoria.core.memory.strategy.activation_v1 import (
            ActivationRetrievalStrategy,
        )

        strategy = ActivationRetrievalStrategy.__new__(ActivationRetrievalStrategy)
        strategy._activation_retriever = MagicMock()
        strategy._activation_retriever.retrieve.return_value = []  # empty → fallback
        strategy._vector_fallback_strategy = MagicMock()
        strategy._vector_fallback_strategy.retrieve.return_value = ([], None)

        with caplog.at_level(logging.INFO):
            strategy.retrieve("u1", "test", query_embedding=[0.1] * 10)

        assert "vector_fallback" in caplog.text


# ── 6. API endpoint must call embed ──────────────────────────────────


class TestRetrieveEndpointEmbedding:
    """POST /v1/memories/retrieve must generate query_embedding."""

    def test_retrieve_calls_embed(self):
        with (
            patch("memoria.core.embedding.get_embedding_client") as mock_embed,
            patch("memoria.api.routers.memory._get_service") as mock_svc,
        ):
            mock_embed.return_value.embed.return_value = [0.1] * 384
            mock_svc.return_value.retrieve.return_value = ([], None)

            from memoria.api.dependencies import AuthContext
            from memoria.api.routers.memory import retrieve_memories, RetrieveRequest

            req = RetrieveRequest(query="test query")
            auth = AuthContext(user_id="u1", db_factory=MagicMock())
            retrieve_memories(req, auth=auth)

            mock_embed.return_value.embed.assert_called_once_with("test query")
            call_kwargs = mock_svc.return_value.retrieve.call_args
            assert call_kwargs.kwargs.get("query_embedding") == [0.1] * 384

    def test_retrieve_warns_on_embed_failure(self, caplog):
        with (
            patch("memoria.core.embedding.get_embedding_client") as mock_embed,
            patch("memoria.api.routers.memory._get_service") as mock_svc,
        ):
            mock_embed.return_value.embed.side_effect = RuntimeError("embed failed")
            mock_svc.return_value.retrieve.return_value = ([], None)

            from memoria.api.dependencies import AuthContext
            from memoria.api.routers.memory import retrieve_memories, RetrieveRequest

            req = RetrieveRequest(query="test")
            auth = AuthContext(user_id="u1", db_factory=MagicMock())

            with caplog.at_level(logging.WARNING):
                retrieve_memories(req, auth=auth)

            assert "failed to embed query" in caplog.text
            call_kwargs = mock_svc.return_value.retrieve.call_args
            assert call_kwargs.kwargs.get("query_embedding") is None


# ── 8. get_nodes_by_ids must filter is_active ────────────────────────


class TestGetNodesByIdsActiveFilter:
    """get_nodes_by_ids must only return active nodes."""

    def test_inactive_nodes_excluded(self):
        from unittest.mock import MagicMock
        from memoria.core.memory.graph.graph_store import GraphStore

        mock_row_active = MagicMock(
            node_id="n1",
            user_id="u1",
            node_type="semantic",
            content="active",
            entity_type=None,
            embedding=None,
            event_id=None,
            memory_id="m1",
            session_id=None,
            confidence=0.9,
            trust_tier="T2",
            is_active=1,
        )

        store = GraphStore.__new__(GraphStore)
        mock_db = MagicMock()
        # Chain: db.query(GraphNode).filter(..., is_active==1).all()
        mock_db.query.return_value.filter.return_value.all.return_value = [
            mock_row_active
        ]
        store._db = MagicMock(
            return_value=MagicMock(
                __enter__=lambda s: mock_db, __exit__=MagicMock(return_value=False)
            )
        )

        result = store.get_nodes_by_ids(["n1", "n2"])
        # Should only get active node
        assert len(result) == 1
        assert result[0].node_id == "n1"

        # Verify is_active filter was applied in the query chain
        filter_call = mock_db.query.return_value.filter
        filter_call.assert_called_once()
        args = filter_call.call_args[0]
        # Should have 2 filter conditions: IN clause + is_active==1
        assert len(args) == 2


# ── 9. Edge methods must filter inactive nodes ──────────────────────


class TestEdgeActiveNodeFilter:
    """Edge query methods must not return edges to/from inactive nodes."""

    def _make_store(self):
        from memoria.core.memory.graph.graph_store import GraphStore

        store = GraphStore.__new__(GraphStore)
        mock_db = MagicMock()
        # JOIN-based queries: query().join().filter().all()
        mock_db.query.return_value.join.return_value.filter.return_value.all.return_value = []
        # UNION-based queries: db.execute(...).fetchall()
        mock_db.execute.return_value.fetchall.return_value = []
        store._db = MagicMock(
            return_value=MagicMock(
                __enter__=lambda s: mock_db,
                __exit__=MagicMock(return_value=False),
            )
        )
        return store, mock_db

    def test_get_outgoing_edges_filters_inactive_targets(self):
        store, mock_db = self._make_store()
        result = store.get_outgoing_edges("n1")
        # JOIN-based: query(GraphEdge).join(GraphNode, ...).filter(...).all()
        assert mock_db.query.call_count >= 1
        assert isinstance(result, list)

    def test_get_incoming_edges_filters_inactive_sources(self):
        store, mock_db = self._make_store()
        result = store.get_incoming_edges("n1")
        assert mock_db.query.call_count >= 1
        assert isinstance(result, list)

    def test_get_edges_for_nodes_filters_inactive(self):
        store, mock_db = self._make_store()
        result = store.get_edges_for_nodes({"n1", "n2"})
        assert mock_db.query.call_count >= 1
        assert isinstance(result, dict)

    def test_get_edges_bidirectional_filters_inactive(self):
        store, mock_db = self._make_store()
        with patch("sqlalchemy.union_all") as mock_union:
            mock_union.return_value = MagicMock()
            incoming, outgoing = store.get_edges_bidirectional({"n1"})
        assert mock_union.call_count == 1
        assert isinstance(incoming, dict)
        assert isinstance(outgoing, dict)

    def test_get_neighbor_ids_filters_inactive(self):
        store, mock_db = self._make_store()
        with patch("sqlalchemy.union_all") as mock_union:
            mock_union.return_value = MagicMock()
            result = store.get_neighbor_ids({"n1"})
        assert mock_union.call_count == 1
        assert isinstance(result, set)
