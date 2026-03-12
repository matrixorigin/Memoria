"""Unit tests for Phase 0B: Scene Nodes + Opinion Evolution upgrade.

Tests:
1. Three-stage Reflector (subgraph context + opinion check)
2. Trust tier promotion chain (T4→T3→T2, T3→T4, T2→T3)
3. Opinion deltas match plan (-0.12 / 0.18)
"""

from unittest.mock import MagicMock

from memoria.core.memory.config import MemoryGovernanceConfig
from memoria.core.memory.interfaces import ReflectionCandidate
from memoria.core.memory.reflection.engine import ReflectionEngine
from memoria.core.memory.reflection.opinion import OpinionEvolver
from memoria.core.memory.types import Memory, MemoryType, TrustTier


def _mem(mid: str = "m1", content: str = "test", confidence: float = 0.5) -> Memory:
    return Memory(
        memory_id=mid,
        user_id="u1",
        memory_type=MemoryType.SEMANTIC,
        content=content,
        initial_confidence=confidence,
        trust_tier=TrustTier.T4_UNVERIFIED,
    )


def _candidate(memories=None, signal="semantic_cluster", score=0.6):
    c = ReflectionCandidate(
        memories=memories or [_mem()],
        signal=signal,
        session_ids=["s1", "s2"],
    )
    c.importance_score = score
    return c


# ── 1. Three-stage Reflector ─────────────────────────────────────────


class TestThreeStageReflector:
    def _make_engine(self, *, subgraph_collector=None, opinion_checker=None):
        provider = MagicMock()
        writer = MagicMock()
        llm = MagicMock()
        llm.chat.return_value = (
            '[{"type": "semantic", "content": "insight", "confidence": 0.5}]'
        )
        return (
            ReflectionEngine(
                candidate_provider=provider,
                writer=writer,
                llm_client=llm,
                threshold=0.5,
                subgraph_collector=subgraph_collector,
                opinion_checker=opinion_checker,
            ),
            provider,
            writer,
            llm,
        )

    def test_stage1_subgraph_context_passed_to_llm(self):
        """Stage 1: subgraph_collector output is included in LLM prompt."""
        collector = MagicMock(return_value="Entity: Python, Neighbor: uses pytest")
        engine, provider, writer, llm = self._make_engine(subgraph_collector=collector)
        provider.get_reflection_candidates.return_value = [_candidate(score=0.7)]

        result = engine.reflect("u1")

        assert result.scenes_created == 1
        collector.assert_called_once()
        # Verify the LLM received the subgraph context
        call_args = llm.chat.call_args
        prompt = (
            call_args[1]["messages"][0]["content"]
            if "messages" in call_args[1]
            else call_args[0][0][0]["content"]
        )
        assert "RELATED CONTEXT" in prompt
        assert "Python" in prompt

    def test_stage1_collector_failure_does_not_block(self):
        """If subgraph_collector raises, synthesis still proceeds."""
        collector = MagicMock(side_effect=RuntimeError("DB down"))
        engine, provider, writer, llm = self._make_engine(subgraph_collector=collector)
        provider.get_reflection_candidates.return_value = [_candidate(score=0.7)]

        result = engine.reflect("u1")

        assert result.scenes_created == 1  # still persisted
        assert len(result.errors) == 0  # collector failure is debug-level

    def test_stage3_opinion_check_reduces_confidence(self):
        """Stage 3: if opinion_checker finds conflicts, confidence is capped."""
        checker = MagicMock(return_value=["existing_scene_1"])
        engine, provider, writer, llm = self._make_engine(opinion_checker=checker)
        provider.get_reflection_candidates.return_value = [_candidate(score=0.7)]

        result = engine.reflect("u1")

        assert result.scenes_created == 1
        assert result.opinion_checks == 1
        # Writer should be called with capped confidence (≤ 0.4)
        store_call = writer.store.call_args
        assert store_call[1]["initial_confidence"] <= 0.4

    def test_stage3_no_conflict_normal_confidence(self):
        """Stage 3: no conflicts → original confidence preserved."""
        checker = MagicMock(return_value=[])
        engine, provider, writer, llm = self._make_engine(opinion_checker=checker)
        provider.get_reflection_candidates.return_value = [_candidate(score=0.7)]

        result = engine.reflect("u1")

        assert result.scenes_created == 1
        assert result.opinion_checks == 0  # empty list = no conflicts
        store_call = writer.store.call_args
        assert store_call[1]["initial_confidence"] == 0.5

    def test_stage3_checker_failure_does_not_block(self):
        """If opinion_checker raises, persist still proceeds."""
        checker = MagicMock(side_effect=RuntimeError("check failed"))
        engine, provider, writer, llm = self._make_engine(opinion_checker=checker)
        provider.get_reflection_candidates.return_value = [_candidate(score=0.7)]

        result = engine.reflect("u1")

        assert result.scenes_created == 1

    def test_no_collector_no_checker_backward_compatible(self):
        """Without collector/checker, engine works exactly as before."""
        engine, provider, writer, llm = self._make_engine()
        provider.get_reflection_candidates.return_value = [_candidate(score=0.7)]

        result = engine.reflect("u1")

        assert result.scenes_created == 1
        assert result.opinion_checks == 0


# ── 2. Trust tier promotion chain ────────────────────────────────────


class TestTrustTierChain:
    def _make_consolidator(self):
        from memoria.core.memory.graph.consolidation import GraphConsolidator

        c = GraphConsolidator.__new__(GraphConsolidator)
        c._store = MagicMock()
        c._config = MemoryGovernanceConfig()
        # Mock: no conflicts, no scenes for integrity check
        c._store.get_association_edges_with_current_sim.return_value = []
        return c

    def _scene(self, *, confidence, trust_tier, age_days, cross_session=0):
        from memoria.core.memory.graph.types import GraphNodeData, NodeType
        from datetime import datetime, timedelta, timezone

        created = datetime.now(timezone.utc) - timedelta(days=age_days)
        return GraphNodeData(
            node_id="s1",
            user_id="u1",
            node_type=NodeType.SCENE,
            content="test scene",
            confidence=confidence,
            trust_tier=trust_tier,
            importance=0.5,
            cross_session_count=cross_session,
            created_at=created.isoformat(),
        )

    def test_t4_to_t3(self):
        c = self._make_consolidator()
        scene = self._scene(confidence=0.85, trust_tier="T4", age_days=10)
        c._store.get_user_nodes.return_value = [scene]
        result = c.consolidate("u1")
        assert result.promoted == 1
        c._store.update_confidence_and_tier.assert_called_with("s1", 0.85, "T3")

    def test_t3_to_t2(self):
        """T3 → T2: confidence ≥ 0.85, age ≥ 30 days, cross_session ≥ 3."""
        c = self._make_consolidator()
        scene = self._scene(
            confidence=0.9, trust_tier="T3", age_days=35, cross_session=4
        )
        c._store.get_user_nodes.return_value = [scene]
        result = c.consolidate("u1")
        assert result.promoted == 1
        c._store.update_confidence_and_tier.assert_called_with("s1", 0.9, "T2")

    def test_t3_not_promoted_low_cross_session(self):
        """T3 with high confidence but low cross_session stays T3."""
        c = self._make_consolidator()
        scene = self._scene(
            confidence=0.9, trust_tier="T3", age_days=35, cross_session=1
        )
        c._store.get_user_nodes.return_value = [scene]
        result = c.consolidate("u1")
        assert result.promoted == 0

    def test_t3_not_promoted_too_young(self):
        """T3 with high confidence but too young stays T3."""
        c = self._make_consolidator()
        scene = self._scene(
            confidence=0.9, trust_tier="T3", age_days=20, cross_session=5
        )
        c._store.get_user_nodes.return_value = [scene]
        result = c.consolidate("u1")
        assert result.promoted == 0

    def test_t3_to_t4_demotion(self):
        """T3 → T4: stale + low confidence."""
        c = self._make_consolidator()
        scene = self._scene(confidence=0.5, trust_tier="T3", age_days=70)
        c._store.get_user_nodes.return_value = [scene]
        result = c.consolidate("u1")
        assert result.demoted == 1
        c._store.update_confidence_and_tier.assert_called_with("s1", 0.5, "T4")

    def test_t2_to_t3_demotion(self):
        """T2 → T3: confidence dropped below 0.7."""
        c = self._make_consolidator()
        scene = self._scene(confidence=0.6, trust_tier="T2", age_days=50)
        c._store.get_user_nodes.return_value = [scene]
        result = c.consolidate("u1")
        assert result.demoted == 1
        c._store.update_confidence_and_tier.assert_called_with("s1", 0.6, "T3")

    def test_t2_stays_when_confident(self):
        """T2 with confidence ≥ 0.7 stays T2."""
        c = self._make_consolidator()
        scene = self._scene(confidence=0.75, trust_tier="T2", age_days=100)
        c._store.get_user_nodes.return_value = [scene]
        result = c.consolidate("u1")
        assert result.demoted == 0
        assert result.promoted == 0


# ── 3. Opinion deltas ────────────────────────────────────────────────


class TestOpinionDeltas:
    def test_contradicting_delta_is_minus_012(self):
        config = MemoryGovernanceConfig()
        evolver = OpinionEvolver(config)
        scene = _mem(confidence=0.5)
        update = evolver.evaluate_evidence(0.1, scene)  # low sim → contradicting
        assert update.evidence_type == "contradicting"
        assert abs(update.new_confidence - 0.38) < 0.01  # 0.5 + (-0.12) = 0.38

    def test_quarantine_at_018(self):
        config = MemoryGovernanceConfig()
        evolver = OpinionEvolver(config)
        scene = _mem(confidence=0.25)
        update = evolver.evaluate_evidence(0.1, scene)
        # 0.25 + (-0.12) = 0.13 < 0.18 → quarantined
        assert update.quarantined is True
        assert update.new_confidence < 0.18

    def test_not_quarantined_above_018(self):
        config = MemoryGovernanceConfig()
        evolver = OpinionEvolver(config)
        scene = _mem(confidence=0.35)
        update = evolver.evaluate_evidence(0.1, scene)
        # 0.35 + (-0.12) = 0.23 > 0.18 → not quarantined
        assert update.quarantined is False

    def test_supporting_delta_is_005(self):
        config = MemoryGovernanceConfig()
        evolver = OpinionEvolver(config)
        scene = _mem(confidence=0.5)
        update = evolver.evaluate_evidence(0.9, scene)  # high sim → supporting
        assert update.evidence_type == "supporting"
        assert abs(update.new_confidence - 0.55) < 0.01

    def test_three_contradictions_quarantine(self):
        """A scene at 0.5 should be quarantined after 3 contradictions."""
        config = MemoryGovernanceConfig()
        evolver = OpinionEvolver(config)
        conf = 0.5
        for i in range(3):
            scene = _mem(confidence=conf)
            update = evolver.evaluate_evidence(0.1, scene)
            conf = update.new_confidence
        # 0.5 → 0.38 → 0.26 → 0.14 < 0.18
        assert conf < 0.18
        assert update.quarantined is True
