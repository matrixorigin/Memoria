"""Unit tests for graph reflection — candidates, consolidation, service wiring."""

from unittest.mock import MagicMock

import pytest

from memoria.core.memory.graph.types import Edge, GraphNodeData, NodeType


class TestGraphCandidateProvider:
    def _make_provider(self):
        from memoria.core.memory.graph.candidates import GraphCandidateProvider

        provider = GraphCandidateProvider(lambda: MagicMock())
        provider._store = MagicMock()
        return provider

    def test_empty_graph_returns_no_candidates(self):
        p = self._make_provider()
        p._store.count_user_nodes.return_value = 0
        assert p.get_reflection_candidates("u1") == []

    def test_too_few_nodes_returns_empty(self):
        p = self._make_provider()
        p._store.count_user_nodes.return_value = 2
        assert p.get_reflection_candidates("u1") == []


class TestConnectedComponents:
    def test_single_component(self):
        from memoria.core.memory.graph.candidates import GraphCandidateProvider

        p = GraphCandidateProvider(lambda: MagicMock())
        p._store = MagicMock()

        nodes = [
            GraphNodeData(
                node_id="a", user_id="u1", node_type=NodeType.SEMANTIC, content="a"
            ),
            GraphNodeData(
                node_id="b", user_id="u1", node_type=NodeType.SEMANTIC, content="b"
            ),
        ]
        # a→b edge
        p._store.get_edges_for_nodes.return_value = {
            "a": [Edge("b", "association", 0.8)],
            "b": [],
        }
        result = p._find_connected_components(nodes)
        assert len(result) == 1
        assert len(result[0]) == 2

    def test_two_components(self):
        from memoria.core.memory.graph.candidates import GraphCandidateProvider

        p = GraphCandidateProvider(lambda: MagicMock())
        p._store = MagicMock()

        nodes = [
            GraphNodeData(
                node_id="a", user_id="u1", node_type=NodeType.SEMANTIC, content="a"
            ),
            GraphNodeData(
                node_id="b", user_id="u1", node_type=NodeType.SEMANTIC, content="b"
            ),
            GraphNodeData(
                node_id="c", user_id="u1", node_type=NodeType.SEMANTIC, content="c"
            ),
        ]
        # a→b, c isolated
        p._store.get_edges_for_nodes.return_value = {
            "a": [Edge("b", "association", 0.8)],
            "b": [],
            "c": [],
        }
        result = p._find_connected_components(nodes)
        assert len(result) == 2


class TestGraphConsolidator:
    def _make_consolidator(self):
        from memoria.core.memory.graph.consolidation import GraphConsolidator

        c = GraphConsolidator(lambda: MagicMock())
        c._store = MagicMock()
        c._store.get_association_edges.return_value = []
        c._store.get_user_nodes.return_value = []
        return c

    def test_empty_graph_no_errors(self):
        c = self._make_consolidator()
        result = c.consolidate("u1")
        assert result.conflicts_detected == 0
        assert result.orphaned_scenes == 0
        assert result.errors == []

    def test_conflict_detection_cross_session(self):
        c = self._make_consolidator()

        node_a = GraphNodeData(
            node_id="a",
            user_id="u1",
            node_type=NodeType.SEMANTIC,
            content="prefers Go",
            session_id="s1",
            confidence=0.7,
        )
        node_b = GraphNodeData(
            node_id="b",
            user_id="u1",
            node_type=NodeType.SEMANTIC,
            content="prefers Python",
            session_id="s2",
            confidence=0.6,
        )

        c._store.get_association_edges_with_current_sim.return_value = [
            ("a", "b", 0.8, 0.2),  # edge_weight=0.8, current_cosine_sim=0.2 → conflict
        ]
        c._store.get_nodes_by_ids.return_value = [node_a, node_b]
        c._store.get_user_nodes.return_value = []  # no scenes

        result = c.consolidate("u1")
        assert result.conflicts_detected == 1
        c._store.mark_conflict.assert_called_once()

    def test_orphaned_scene_deactivated(self):
        c = self._make_consolidator()

        scene = GraphNodeData(
            node_id="scene1",
            user_id="u1",
            node_type=NodeType.SCENE,
            content="insight",
            source_nodes=["src1", "src2", "src3"],
        )

        c._store.get_user_nodes.return_value = [scene]
        c._store.get_nodes_by_ids.return_value = [
            GraphNodeData(
                node_id="src1",
                user_id="u1",
                node_type=NodeType.SEMANTIC,
                content="x",
                is_active=False,
            ),
            GraphNodeData(
                node_id="src2",
                user_id="u1",
                node_type=NodeType.SEMANTIC,
                content="y",
                is_active=False,
            ),
        ]

        result = c.consolidate("u1")
        assert result.orphaned_scenes == 1
        c._store.deactivate_node.assert_called_once_with("scene1")

    def test_partial_source_loss_reduces_confidence(self):
        c = self._make_consolidator()

        scene = GraphNodeData(
            node_id="scene1",
            user_id="u1",
            node_type=NodeType.SCENE,
            content="insight",
            confidence=0.9,
            source_nodes=["src1", "src2", "src3", "src4"],
        )

        c._store.get_user_nodes.return_value = [scene]
        c._store.get_nodes_by_ids.return_value = [
            GraphNodeData(
                node_id="src1",
                user_id="u1",
                node_type=NodeType.SEMANTIC,
                content="x",
                is_active=True,
            ),
            GraphNodeData(
                node_id="src2",
                user_id="u1",
                node_type=NodeType.SEMANTIC,
                content="y",
                is_active=False,
            ),
        ]

        result = c.consolidate("u1")
        assert result.orphaned_scenes == 0
        c._store.update_confidence.assert_called_once()
        args = c._store.update_confidence.call_args[0]
        assert args[0] == "scene1"
        assert args[1] == pytest.approx(0.72)


class TestTrustTierLifecycle:
    """§4.7 — T4→T3 promotion (age-gated) and T3→T4 demotion."""

    def _make_consolidator(self):
        from memoria.core.memory.graph.consolidation import GraphConsolidator

        c = GraphConsolidator(lambda: MagicMock())
        c._store = MagicMock()
        c._store.get_association_edges.return_value = []
        return c

    def _scene(self, node_id="s1", confidence=0.85, trust_tier="T4", age_days=10):
        from datetime import datetime, timedelta, timezone

        created = datetime.now(timezone.utc) - timedelta(days=age_days)
        return GraphNodeData(
            node_id=node_id,
            user_id="u1",
            node_type=NodeType.SCENE,
            content="insight",
            confidence=confidence,
            trust_tier=trust_tier,
            created_at=created.isoformat(),
        )

    def test_t4_promoted_when_confident_and_old_enough(self):
        c = self._make_consolidator()
        scene = self._scene(confidence=0.85, trust_tier="T4", age_days=10)
        c._store.get_user_nodes.return_value = [scene]

        result = c.consolidate("u1")

        assert result.promoted == 1
        c._store.update_confidence_and_tier.assert_called_once_with("s1", 0.85, "T3")

    def test_t4_not_promoted_when_too_young(self):
        c = self._make_consolidator()
        scene = self._scene(confidence=0.85, trust_tier="T4", age_days=3)
        c._store.get_user_nodes.return_value = [scene]

        result = c.consolidate("u1")

        assert result.promoted == 0
        c._store.update_confidence_and_tier.assert_not_called()

    def test_t4_not_promoted_when_low_confidence(self):
        c = self._make_consolidator()
        scene = self._scene(confidence=0.6, trust_tier="T4", age_days=30)
        c._store.get_user_nodes.return_value = [scene]

        result = c.consolidate("u1")

        assert result.promoted == 0

    def test_t3_demoted_when_stale_and_low_confidence(self):
        c = self._make_consolidator()
        scene = self._scene(confidence=0.5, trust_tier="T3", age_days=65)
        c._store.get_user_nodes.return_value = [scene]

        result = c.consolidate("u1")

        assert result.demoted == 1
        c._store.update_confidence_and_tier.assert_called_once_with("s1", 0.5, "T4")

    def test_t3_not_demoted_when_confident(self):
        c = self._make_consolidator()
        scene = self._scene(confidence=0.85, trust_tier="T3", age_days=90)
        c._store.get_user_nodes.return_value = [scene]

        result = c.consolidate("u1")

        assert result.demoted == 0

    def test_t3_not_demoted_when_young(self):
        c = self._make_consolidator()
        scene = self._scene(confidence=0.5, trust_tier="T3", age_days=30)
        c._store.get_user_nodes.return_value = [scene]

        result = c.consolidate("u1")

        assert result.demoted == 0

    def test_t2_not_affected(self):
        c = self._make_consolidator()
        scene = self._scene(confidence=0.4, trust_tier="T2", age_days=100)
        c._store.get_user_nodes.return_value = [scene]

        result = c.consolidate("u1")

        assert result.promoted == 0
        # T2 with confidence 0.4 < 0.7 → demoted to T3
        assert result.demoted == 1
        c._store.update_confidence_and_tier.assert_called_once_with("s1", 0.4, "T3")

    def test_t2_not_demoted_when_confident(self):
        c = self._make_consolidator()
        scene = self._scene(confidence=0.75, trust_tier="T2", age_days=100)
        c._store.get_user_nodes.return_value = [scene]

        result = c.consolidate("u1")

        assert result.demoted == 0


class TestGraphServiceReflection:
    def _make_service(self):
        from memoria.core.memory.graph.service import GraphMemoryService

        svc = GraphMemoryService(lambda: MagicMock())
        svc._tabular = MagicMock()
        return svc

    def test_get_reflection_candidates_uses_graph(self):
        svc = self._make_service()
        mock_candidates = MagicMock()
        mock_candidates.get_reflection_candidates.return_value = ["c1"]
        svc._graph_candidates = mock_candidates

        result = svc.get_reflection_candidates("u1")
        assert result == ["c1"]

    def test_candidates_fallback_to_tabular_on_error(self):
        from sqlalchemy.exc import OperationalError

        svc = self._make_service()
        mock_candidates = MagicMock()
        mock_candidates.get_reflection_candidates.side_effect = OperationalError(
            "db", {}, Exception("conn lost")
        )
        svc._graph_candidates = mock_candidates
        svc._tabular._governance_lazy = MagicMock()
        svc._tabular._governance_lazy.get_reflection_candidates.return_value = [
            "fallback"
        ]

        result = svc.get_reflection_candidates("u1")
        assert result == ["fallback"]

    def test_run_governance_includes_consolidation(self):
        svc = self._make_service()
        svc._tabular.run_governance.return_value = MagicMock(errors=[])

        from memoria.core.memory.graph.consolidation import ConsolidationResult

        mock_consolidator = MagicMock()
        mock_consolidator.consolidate.return_value = ConsolidationResult()
        svc._graph_consolidator = mock_consolidator

        svc.run_governance("u1")
        mock_consolidator.consolidate.assert_called_once_with("u1")

    def test_consolidate_direct_access(self):
        svc = self._make_service()
        from memoria.core.memory.graph.consolidation import ConsolidationResult

        mock_consolidator = MagicMock()
        mock_consolidator.consolidate.return_value = ConsolidationResult(
            conflicts_detected=2
        )
        svc._graph_consolidator = mock_consolidator

        result = svc.consolidate("u1")
        assert result.conflicts_detected == 2
