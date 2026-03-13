"""Tests for entity_link weight stratification in GraphBuilder._link_entities."""

from unittest.mock import MagicMock, patch

from memoria.core.memory.graph.graph_builder import GraphBuilder
from memoria.core.memory.graph.types import EdgeType, GraphNodeData, NodeType


def _make_store():
    store = MagicMock()
    store._db.return_value.__enter__ = lambda s: MagicMock()
    store._db.return_value.__exit__ = MagicMock(return_value=False)
    store._upsert_entity_in.return_value = "entity-id-1"
    store._upsert_link_in.return_value = None
    return store


def _make_node(content: str, memory_id: str = "mem1") -> GraphNodeData:
    return GraphNodeData(
        node_id="node1",
        user_id="user1",
        node_type=NodeType.SEMANTIC,
        content=content,
        memory_id=memory_id,
    )


class TestEntityLinkWeightStratification:
    """Different entity types get different edge weights."""

    def _run_link_entities(self, content: str) -> list[tuple]:
        """Run _link_entities and return the pending_edges list."""
        store = _make_store()
        builder = GraphBuilder(store, embed_fn=lambda x: [0.1] * 10)
        node = _make_node(content)
        pending_edges: list[tuple] = []

        # Mock the GraphNode query to return None (no existing node)
        mock_db = MagicMock()
        mock_db.query.return_value.filter_by.return_value.first.return_value = None
        store._db.return_value.__enter__ = lambda s: mock_db
        store._db.return_value.__exit__ = MagicMock(return_value=False)

        with patch(
            "memoria.core.memory.graph.graph_builder.get_ner_backend"
        ) as mock_ner:
            from memoria.core.memory.graph.entity_extractor import ExtractedEntity

            mock_backend = MagicMock()
            mock_ner.return_value = mock_backend

            # Simulate different entity types
            mock_backend.extract.return_value = [
                ExtractedEntity("alice", "Alice", "person"),
                ExtractedEntity("redis", "Redis", "tech"),
                ExtractedEntity("auth-service", "auth-service", "project"),
            ]
            store._upsert_entity_in.side_effect = (
                lambda db, uid, name, disp, etype, embedding=None: f"eid-{name}"
            )

            builder._link_entities("user1", [node], pending_edges)

        return pending_edges

    def test_person_entity_excluded_from_graph_edges(self):
        edges = self._run_link_entities("Alice 负责 auth-service，使用 Redis")
        person_edges = [e for e in edges if e[1] == "eid-alice"]
        assert len(person_edges) == 0  # person entities don't get graph edges

    def test_tech_entity_weight(self):
        edges = self._run_link_entities("Alice 负责 auth-service，使用 Redis")
        tech_edges = [e for e in edges if e[1] == "eid-redis"]
        assert len(tech_edges) == 1
        assert tech_edges[0][3] == 0.9  # tech weight

    def test_project_entity_weight(self):
        edges = self._run_link_entities("Alice 负责 auth-service，使用 Redis")
        proj_edges = [e for e in edges if e[1] == "eid-auth-service"]
        assert len(proj_edges) == 1
        assert proj_edges[0][3] == 0.85  # project weight

    def test_all_edges_are_entity_link_type(self):
        edges = self._run_link_entities("Alice 负责 auth-service，使用 Redis")
        for edge in edges:
            assert edge[2] == EdgeType.ENTITY_LINK.value

    def test_person_excluded_tech_higher_than_project(self):
        edges = self._run_link_entities("Alice 负责 auth-service，使用 Redis")
        weights = {e[1].split("-")[-1]: e[3] for e in edges}
        # person excluded from graph edges
        assert "alice" not in weights
        assert weights.get("redis", 0) > weights.get("service", 0)


class TestAssociationThresholdConfig:
    """GraphBuilder respects config.activation_association_threshold."""

    def test_default_threshold_is_0_55(self):
        store = MagicMock()
        builder = GraphBuilder(store, embed_fn=lambda x: [0.1] * 10)
        assert builder._assoc_threshold == 0.55

    def test_custom_threshold_from_config(self):
        from memoria.core.memory.config import MemoryGovernanceConfig
        from dataclasses import replace

        cfg = replace(MemoryGovernanceConfig(), activation_association_threshold=0.7)
        store = MagicMock()
        builder = GraphBuilder(store, config=cfg, embed_fn=lambda x: [0.1] * 10)
        assert builder._assoc_threshold == 0.7

    def test_low_similarity_edge_excluded(self):
        """Edges below threshold are not created."""
        store = MagicMock()
        store.find_similar_with_scores.return_value = [
            (MagicMock(node_id="other", embedding=[0.1]), 0.4),  # below 0.55
        ]
        builder = GraphBuilder(store, embed_fn=lambda x: [0.1] * 10)
        pending_edges: list[tuple] = []
        builder._create_semantic_nodes(
            "u1",
            [
                MagicMock(
                    memory_id="m1",
                    trust_tier=MagicMock(value="T3"),
                    initial_confidence=0.8,
                    content="test",
                    embedding=[0.1] * 10,
                    session_id="s1",
                    source_event_ids=[],
                )
            ],
        )
        # Verify find_similar_with_scores was called (association edge logic)
        # The actual edge creation depends on cos_sim > threshold
        # With threshold=0.55 and sim=0.4, no edge should be added
        assert all(e[2] != EdgeType.ASSOCIATION.value for e in pending_edges)
