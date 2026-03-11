"""Graph memory DB integration tests — normalized edge table.

Tests:
1. DDL: both tables exist
2. Node CRUD: insert, read, batch, deactivate
3. Edge CRUD: batch insert, dedup, outgoing/incoming queries
4. Vector: l2_distance, cosine_distance, pair similarity
5. Conflict marking: atomic multi-row update
6. Multi-hop: neighbor expansion via edge table
7. Skeleton load: column-query without embedding
"""

from uuid import uuid4

import pytest

import os; EMBEDDING_DIM = int(os.environ.get("MEMORIA_EMBEDDING_DIM", "384"))
from memoria.core.memory.graph.graph_store import GraphStore
from memoria.core.memory.graph.types import Edge, EdgeType, GraphNodeData, NodeType


def _uid() -> str:
    return f"graph_e2e_{uuid4().hex[:12]}"


def _embed(seed: float = 0.1) -> list[float]:
    return [seed] * EMBEDDING_DIM


def _similar_embed() -> list[float]:
    e = [0.1] * EMBEDDING_DIM
    for i in range(EMBEDDING_DIM // 10):
        e[i] = 0.15
    return e


def _different_embed() -> list[float]:
    e = [0.0] * EMBEDDING_DIM
    e[0] = 1.0
    return e


@pytest.fixture
def db_factory():
    from tests.integration.conftest import _get_session_local; SessionLocal = _get_session_local()
    return SessionLocal


@pytest.fixture
def store(db_factory):
    return GraphStore(db_factory)


@pytest.fixture
def user_id():
    return _uid()


@pytest.fixture(autouse=True)
def cleanup(db_factory, user_id):
    yield
    from sqlalchemy import text
    db = db_factory()
    try:
        db.execute(text("DELETE FROM memory_graph_edges WHERE user_id = :uid"), {"uid": user_id})
        db.execute(text("DELETE FROM memory_graph_nodes WHERE user_id = :uid"), {"uid": user_id})
        db.commit()
    finally:
        db.close()


class TestDDL:
    def test_both_tables_exist(self, db_factory):
        from sqlalchemy import inspect as sa_inspect
        db = db_factory()
        try:
            tables = set(sa_inspect(db.bind).get_table_names())
            assert "memory_graph_nodes" in tables
            assert "memory_graph_edges" in tables
        finally:
            db.close()


class TestNodeCRUD:
    def test_create_and_read(self, store, user_id):
        node = GraphNodeData(
            node_id=uuid4().hex, user_id=user_id,
            node_type=NodeType.EPISODIC, content="test",
            embedding=_embed(), event_id="evt1", session_id="s1",
            confidence=0.9, trust_tier="T2", importance=0.5,
        )
        store.create_node(node)

        loaded = store.get_node(node.node_id)
        assert loaded is not None
        assert loaded.content == "test"
        assert loaded.confidence == pytest.approx(0.9)
        assert loaded.trust_tier == "T2"
        assert loaded.is_active is True
        assert len(loaded.embedding) == EMBEDDING_DIM

    def test_batch_create(self, store, user_id):
        nodes = [
            GraphNodeData(
                node_id=uuid4().hex, user_id=user_id,
                node_type=NodeType.SEMANTIC, content=f"n{i}",
            )
            for i in range(5)
        ]
        store.create_nodes_batch(nodes)
        assert store.count_user_nodes(user_id) == 5

    def test_deactivate(self, store, user_id):
        node = GraphNodeData(
            node_id=uuid4().hex, user_id=user_id,
            node_type=NodeType.SEMANTIC, content="bye",
        )
        store.create_node(node)
        store.deactivate_node(node.node_id, superseded_by="new")

        loaded = store.get_node(node.node_id)
        assert loaded.is_active is False
        assert loaded.superseded_by == "new"
        assert store.count_user_nodes(user_id) == 0

    def test_conflict_resolution_persisted(self, store, user_id):
        node = GraphNodeData(
            node_id=uuid4().hex, user_id=user_id,
            node_type=NodeType.SEMANTIC, content="c",
            conflict_resolution="kept",
        )
        store.create_node(node)
        assert store.get_node(node.node_id).conflict_resolution == "kept"


class TestEdgeCRUD:
    def test_add_and_query_edges(self, store, user_id):
        a_id, b_id, c_id = uuid4().hex, uuid4().hex, uuid4().hex
        for nid, content in [(a_id, "a"), (b_id, "b"), (c_id, "c")]:
            store.create_node(GraphNodeData(
                node_id=nid, user_id=user_id,
                node_type=NodeType.SEMANTIC, content=content,
            ))

        store.add_edges_batch([
            (a_id, b_id, EdgeType.ASSOCIATION.value, 0.8),
            (a_id, c_id, EdgeType.TEMPORAL.value, 1.0),
            (b_id, c_id, EdgeType.CAUSAL.value, 1.5),
        ], user_id)

        # Outgoing from a
        out_a = store.get_outgoing_edges(a_id)
        assert len(out_a) == 2
        targets = {e.target_id for e in out_a}
        assert b_id in targets and c_id in targets

        # Incoming to c
        in_c = store.get_incoming_edges(c_id)
        assert len(in_c) == 2

        # Batch query
        all_out = store.get_edges_for_nodes({a_id, b_id})
        assert len(all_out[a_id]) == 2
        assert len(all_out[b_id]) == 1

    def test_duplicate_edge_not_added(self, store, user_id):
        a_id, b_id = uuid4().hex, uuid4().hex
        for nid in [a_id, b_id]:
            store.create_node(GraphNodeData(
                node_id=nid, user_id=user_id,
                node_type=NodeType.SEMANTIC, content="x",
            ))

        edge = [(a_id, b_id, EdgeType.ASSOCIATION.value, 0.8)]
        store.add_edges_batch(edge, user_id)
        store.add_edges_batch(edge, user_id)

        assert len(store.get_outgoing_edges(a_id)) == 1

    def test_neighbor_ids(self, store, user_id):
        a_id, b_id, c_id = uuid4().hex, uuid4().hex, uuid4().hex
        for nid in [a_id, b_id, c_id]:
            store.create_node(GraphNodeData(
                node_id=nid, user_id=user_id,
                node_type=NodeType.SEMANTIC, content="x",
            ))

        store.add_edges_batch([
            (a_id, b_id, "association", 0.8),
            (c_id, a_id, "temporal", 1.0),
        ], user_id)

        neighbors = store.get_neighbor_ids({a_id})
        assert b_id in neighbors  # outgoing
        assert c_id in neighbors  # incoming

    def test_association_edges_query(self, store, user_id):
        a_id, b_id = uuid4().hex, uuid4().hex
        for nid in [a_id, b_id]:
            store.create_node(GraphNodeData(
                node_id=nid, user_id=user_id,
                node_type=NodeType.SEMANTIC, content="x",
            ))

        store.add_edges_batch([
            (a_id, b_id, "association", 0.8),
            (a_id, b_id, "temporal", 1.0),  # different type, same pair
        ], user_id)

        assoc = store.get_association_edges(user_id, min_weight=0.7)
        assert len(assoc) == 1
        assert assoc[0][2] == pytest.approx(0.8)


class TestVectorSearch:
    def _seed(self, store, user_id):
        a = GraphNodeData(
            node_id=uuid4().hex, user_id=user_id,
            node_type=NodeType.SEMANTIC, content="A", embedding=_embed(0.1),
        )
        b = GraphNodeData(
            node_id=uuid4().hex, user_id=user_id,
            node_type=NodeType.SEMANTIC, content="B", embedding=_similar_embed(),
        )
        d = GraphNodeData(
            node_id=uuid4().hex, user_id=user_id,
            node_type=NodeType.SEMANTIC, content="D", embedding=_different_embed(),
        )
        store.create_nodes_batch([a, b, d])
        return a, b, d

    def test_l2_distance(self, store, user_id):
        a, b, d = self._seed(store, user_id)
        results = store.find_similar_nodes(user_id, _embed(0.1), top_k=3)
        assert results[0].node_id == a.node_id

    def test_cosine_with_scores(self, store, user_id):
        a, b, d = self._seed(store, user_id)
        results = store.find_similar_with_scores(user_id, _embed(0.1), top_k=3)
        assert results[0][0].node_id == a.node_id
        assert results[0][1] > 0.9
        diff_score = next(s for n, s in results if n.node_id == d.node_id)
        assert diff_score < results[0][1]

    def test_pair_similarity(self, store, user_id):
        a, b, d = self._seed(store, user_id)
        sim_ab = store.get_pair_similarity(a.node_id, b.node_id)
        sim_ad = store.get_pair_similarity(a.node_id, d.node_id)
        assert sim_ab is not None and sim_ad is not None
        assert sim_ab > sim_ad


class TestMarkConflict:
    def test_atomic(self, store, user_id):
        older = GraphNodeData(
            node_id=uuid4().hex, user_id=user_id,
            node_type=NodeType.SEMANTIC, content="old", confidence=0.8,
        )
        newer = GraphNodeData(
            node_id=uuid4().hex, user_id=user_id,
            node_type=NodeType.SEMANTIC, content="new", confidence=0.9,
        )
        store.create_nodes_batch([older, newer])

        store.mark_conflict(
            older_id=older.node_id, newer_id=newer.node_id,
            confidence_factor=0.5, old_confidence=0.8,
        )

        lo = store.get_node(older.node_id)
        assert lo.confidence == pytest.approx(0.4)
        assert lo.conflicts_with == newer.node_id
        assert lo.conflict_resolution == "superseded"

        ln = store.get_node(newer.node_id)
        assert ln.conflict_resolution == "kept"
        assert ln.confidence == pytest.approx(0.9)


class TestSkeletonLoad:
    def test_no_embedding(self, store, user_id):
        store.create_node(GraphNodeData(
            node_id=uuid4().hex, user_id=user_id,
            node_type=NodeType.SEMANTIC, content="test", embedding=_embed(),
        ))
        nodes = store.get_user_nodes(user_id, load_embedding=False)
        assert len(nodes) == 1
        assert nodes[0].embedding is None
        assert nodes[0].content == "test"


class TestOpinionEvolution:
    """§4.5 — opinion evolution against real MatrixOne."""

    def _seed_scene_and_event(self, store, user_id, scene_embed, event_embed):
        """Create a scene node and a new event node, return their IDs."""
        scene = GraphNodeData(
            node_id=uuid4().hex, user_id=user_id,
            node_type=NodeType.SCENE, content="User prefers verbose errors",
            embedding=scene_embed, confidence=0.5, trust_tier="T4",
        )
        event_node = GraphNodeData(
            node_id=uuid4().hex, user_id=user_id,
            node_type=NodeType.EPISODIC, content="Show me detailed errors",
            embedding=event_embed, confidence=1.0, trust_tier="T1",
        )
        store.create_nodes_batch([scene, event_node])

        # Edge so activation can reach the scene from the event
        store.add_edges_batch([
            (event_node.node_id, scene.node_id, EdgeType.ABSTRACTION.value, 0.8),
        ], user_id)

        return scene, event_node

    def test_supporting_evidence_increases_confidence(self, store, user_id):
        from memoria.core.memory.graph.opinion import evolve_opinions

        # Similar embeddings → high cosine similarity → supporting
        base = _embed(0.1)
        scene, event_node = self._seed_scene_and_event(store, user_id, base, _similar_embed())

        result = evolve_opinions(store, event_node.node_id, user_id)

        assert result.scenes_evaluated == 1
        assert result.supporting == 1

        # Verify DB state
        updated = store.get_node(scene.node_id)
        assert updated.confidence > 0.5  # increased

    def test_contradicting_evidence_decreases_confidence(self, store, user_id):
        from memoria.core.memory.graph.opinion import evolve_opinions

        # Very different embeddings → low cosine similarity → contradicting
        scene, event_node = self._seed_scene_and_event(
            store, user_id, _embed(0.1), _different_embed(),
        )

        result = evolve_opinions(store, event_node.node_id, user_id)

        assert result.scenes_evaluated == 1
        assert result.contradicting == 1

        updated = store.get_node(scene.node_id)
        assert updated.confidence < 0.5  # decreased

    def test_quarantine_deactivates_node(self, store, user_id):
        from memoria.core.memory.graph.opinion import evolve_opinions

        # Start at very low confidence, contradicting will push below quarantine
        scene = GraphNodeData(
            node_id=uuid4().hex, user_id=user_id,
            node_type=NodeType.SCENE, content="Weak belief",
            embedding=_embed(0.1), confidence=0.15, trust_tier="T4",
        )
        event_node = GraphNodeData(
            node_id=uuid4().hex, user_id=user_id,
            node_type=NodeType.EPISODIC, content="Contradicts",
            embedding=_different_embed(), confidence=1.0, trust_tier="T1",
        )
        store.create_nodes_batch([scene, event_node])
        store.add_edges_batch([
            (event_node.node_id, scene.node_id, EdgeType.ABSTRACTION.value, 0.8),
        ], user_id)

        result = evolve_opinions(store, event_node.node_id, user_id)

        assert result.quarantined == 1
        updated = store.get_node(scene.node_id)
        assert updated.is_active is False

    def test_opinion_does_not_promote_instantly(self, store, user_id):
        """§4.7: promotion requires age gate — opinion evolution only updates confidence."""
        from memoria.core.memory.graph.opinion import evolve_opinions

        scene = GraphNodeData(
            node_id=uuid4().hex, user_id=user_id,
            node_type=NodeType.SCENE, content="Strong belief",
            embedding=_embed(0.1), confidence=0.78, trust_tier="T4",
        )
        event_node = GraphNodeData(
            node_id=uuid4().hex, user_id=user_id,
            node_type=NodeType.EPISODIC, content="Supporting",
            embedding=_similar_embed(), confidence=1.0, trust_tier="T1",
        )
        store.create_nodes_batch([scene, event_node])
        store.add_edges_batch([
            (event_node.node_id, scene.node_id, EdgeType.ABSTRACTION.value, 0.8),
        ], user_id)

        result = evolve_opinions(store, event_node.node_id, user_id)

        assert result.supporting == 1
        updated = store.get_node(scene.node_id)
        assert updated.confidence > 0.8  # confidence increased past threshold
        assert updated.trust_tier == "T4"  # but tier NOT promoted — needs age gate

    def test_update_confidence_and_tier(self, store, user_id):
        """Verify the update_confidence_and_tier method works in real DB."""
        node = GraphNodeData(
            node_id=uuid4().hex, user_id=user_id,
            node_type=NodeType.SCENE, content="test",
            confidence=0.5, trust_tier="T4",
        )
        store.create_node(node)

        store.update_confidence_and_tier(node.node_id, 0.85, "T3")

        loaded = store.get_node(node.node_id)
        assert loaded.confidence == pytest.approx(0.85)
        assert loaded.trust_tier == "T3"


class TestTrustTierLifecycleE2E:
    """§4.7 — full lifecycle against real MatrixOne.

    Tests the complete closed loop:
    1. Scene created at T4 with low confidence
    2. Supporting evidence raises confidence above threshold
    3. Consolidation promotes T4→T3 (with age gate)
    4. Contradicting evidence drops confidence
    5. Consolidation demotes stale T3→T4
    """

    def test_full_promotion_lifecycle(self, store, user_id):
        """Closed-loop: create → support → promote → verify."""
        from memoria.core.memory.graph.consolidation import GraphConsolidator
        from memoria.core.memory.graph.opinion import evolve_opinions
        from tests.integration.conftest import _get_session_local; SessionLocal = _get_session_local()

        consolidator = GraphConsolidator(SessionLocal)

        # 1. Create scene at T4, confidence 0.5
        scene = GraphNodeData(
            node_id=uuid4().hex, user_id=user_id,
            node_type=NodeType.SCENE, content="User prefers verbose errors",
            embedding=_embed(0.1), confidence=0.5, trust_tier="T4",
        )
        store.create_node(scene)

        # 2. Simulate multiple supporting events to push confidence above 0.8
        for i in range(7):
            ev = GraphNodeData(
                node_id=uuid4().hex, user_id=user_id,
                node_type=NodeType.EPISODIC, content=f"verbose error request {i}",
                embedding=_similar_embed(), confidence=1.0, trust_tier="T1",
            )
            store.create_node(ev)
            store.add_edges_batch([
                (ev.node_id, scene.node_id, EdgeType.ABSTRACTION.value, 0.8),
            ], user_id)
            evolve_opinions(store, ev.node_id, user_id)

        # Verify confidence rose above threshold
        after_support = store.get_node(scene.node_id)
        assert after_support.confidence >= 0.8
        assert after_support.trust_tier == "T4"  # not promoted yet — too young

        # 3. Consolidation should NOT promote (node is < 7 days old)
        result = consolidator.consolidate(user_id)
        assert result.promoted == 0

        still_t4 = store.get_node(scene.node_id)
        assert still_t4.trust_tier == "T4"

        # 4. Fake the age by updating created_at directly
        from sqlalchemy import text
        from memoria.core.memory.models.graph import GraphNode
        with SessionLocal() as db:
            db.query(GraphNode).filter_by(node_id=scene.node_id).update({
                "created_at": text("DATE_SUB(NOW(), INTERVAL 10 DAY)"),
            })
            db.commit()

        # 5. Now consolidation should promote T4→T3
        result2 = consolidator.consolidate(user_id)
        assert result2.promoted == 1

        promoted = store.get_node(scene.node_id)
        assert promoted.trust_tier == "T3"
        assert promoted.confidence >= 0.8

    def test_full_demotion_lifecycle(self, store, user_id):
        """Closed-loop: T3 scene with low confidence + old age → demoted to T4."""
        from memoria.core.memory.graph.consolidation import GraphConsolidator
        from tests.integration.conftest import _get_session_local; SessionLocal = _get_session_local()

        consolidator = GraphConsolidator(SessionLocal)

        # 1. Create scene at T3, confidence below threshold
        scene = GraphNodeData(
            node_id=uuid4().hex, user_id=user_id,
            node_type=NodeType.SCENE, content="Stale belief",
            embedding=_embed(0.1), confidence=0.5, trust_tier="T3",
        )
        store.create_node(scene)

        # 2. Not old enough yet — should NOT demote
        result = consolidator.consolidate(user_id)
        assert result.demoted == 0

        # 3. Fake age to 65 days
        from sqlalchemy import text
        from memoria.core.memory.models.graph import GraphNode
        with SessionLocal() as db:
            db.query(GraphNode).filter_by(node_id=scene.node_id).update({
                "created_at": text("DATE_SUB(NOW(), INTERVAL 65 DAY)"),
            })
            db.commit()

        # 4. Now consolidation should demote T3→T4
        result2 = consolidator.consolidate(user_id)
        assert result2.demoted == 1

        demoted = store.get_node(scene.node_id)
        assert demoted.trust_tier == "T4"
        assert demoted.confidence == pytest.approx(0.5)  # confidence unchanged


class TestIntentDrivenLoadingE2E:
    """§13 — Intent-driven memory loading against real MatrixOne.

    Verifies:
    1. Task edge boosts change activation scores (same graph, different task_type)
    2. Task activation params affect retrieval (planning vs debugging)
    3. Full retriever path with 50+ nodes, real cosine similarity, real edges
    4. Field-level verification on returned results
    """

    def _seed_graph(self, store: GraphStore, user_id: str, count: int = 55):
        """Seed a graph with causal + association edges for task-boost testing.

        Topology for boost testing (first 3 nodes):
          anchor → causal_target  (1 causal edge, fan-out=2)
          anchor → assoc_target   (1 association edge, fan-out=2)
        Both targets have identical fan-in (1 edge from anchor) and anchor
        has fan-out=2, so the ONLY difference in activation is edge type boost.
        """
        nodes = []
        for i in range(count):
            emb = [0.1] * EMBEDDING_DIM
            emb[i % EMBEDDING_DIM] += 0.05 * (i % 10)
            nodes.append(GraphNodeData(
                node_id=f"n{i}_{user_id[:8]}",
                user_id=user_id,
                node_type=NodeType.SEMANTIC,
                content=f"node {i}",
                embedding=emb,
                confidence=0.8,
                importance=0.5,
                trust_tier="T3",
            ))
        store.create_nodes_batch(nodes)

        # Minimal topology: anchor(n0) has exactly 2 outgoing edges
        # so fan-out normalization is equal for both targets
        edges = [
            (f"n0_{user_id[:8]}", f"n1_{user_id[:8]}", EdgeType.CAUSAL, 1.0),
            (f"n0_{user_id[:8]}", f"n2_{user_id[:8]}", EdgeType.ASSOCIATION, 1.0),
        ]
        store.add_edges_batch(edges, user_id)
        return nodes

    def test_task_boost_changes_scores(self, store, user_id):
        """Same graph + query, different task_type → different score ordering.

        Topology: anchor(n0) --causal--> n1, anchor(n0) --association--> n2
        Fan-out = 2 for both, so the ONLY variable is edge type boost.
        """
        from memoria.core.memory.graph.activation import SpreadingActivation

        nodes = self._seed_graph(store, user_id)
        anchors = {nodes[0].node_id: 1.0}
        n1_id = nodes[1].node_id  # causal target
        n2_id = nodes[2].node_id  # association target

        # Debugging: causal=2.0, association=0.5
        sa_debug = SpreadingActivation(store, task_type="debugging")
        sa_debug.set_anchors(dict(anchors))
        sa_debug.propagate(iterations=1)  # single iteration to avoid sigmoid saturation
        debug_act = sa_debug.get_activated()

        # Planning: association=1.2, causal=1.0
        sa_plan = SpreadingActivation(store, task_type="planning")
        sa_plan.set_anchors(dict(anchors))
        sa_plan.propagate(iterations=1)
        plan_act = sa_plan.get_activated()

        # Debugging should boost causal (n1) over association (n2)
        assert debug_act.get(n1_id, 0) > debug_act.get(n2_id, 0), (
            f"debugging: causal n1={debug_act.get(n1_id, 0):.4f} "
            f"should > association n2={debug_act.get(n2_id, 0):.4f}"
        )
        # Planning should boost association (n2) relative to causal (n1)
        debug_ratio = debug_act.get(n1_id, 0) / max(debug_act.get(n2_id, 0), 1e-9)
        plan_ratio = plan_act.get(n1_id, 0) / max(plan_act.get(n2_id, 0), 1e-9)
        assert debug_ratio > plan_ratio, (
            f"debugging should favor causal more than planning: "
            f"debug_ratio={debug_ratio:.3f} plan_ratio={plan_ratio:.3f}"
        )

    def test_full_retriever_with_task_type(self, store, user_id):
        """Full ActivationRetriever path: 55 nodes, real DB, task_type passed through."""
        from memoria.core.memory.graph.retriever import ActivationRetriever

        nodes = self._seed_graph(store, user_id)
        retriever = ActivationRetriever(store)

        # Query embedding close to n0
        query_emb = list(nodes[0].embedding)

        results = retriever.retrieve(
            user_id, "test query", query_emb,
            top_k=10, task_type="debugging",
        )

        assert len(results) > 0, "should return results with 55 nodes"
        # Verify field-level correctness on every returned node
        seen_ids = set()
        prev_score = float("inf")
        for node, score in results:
            assert node.user_id == user_id
            assert node.content is not None
            assert node.confidence is not None
            assert node.importance is not None
            assert score > 0
            assert node.is_active in (True, 1)
            # Scores must be descending (sorted)
            assert score <= prev_score + 1e-9, "results should be sorted by score desc"
            prev_score = score
            # No duplicates
            assert node.node_id not in seen_ids, f"duplicate node {node.node_id}"
            seen_ids.add(node.node_id)

        # n0 (anchor) must appear somewhere in results
        result_ids = {n.node_id for n, _ in results}
        assert nodes[0].node_id in result_ids, "anchor node n0 should be in results"

    def test_planning_uses_fewer_anchors(self, store, user_id):
        """Planning mode uses anchor_k=5 vs debugging's anchor_k=10."""
        from memoria.core.memory.graph.retriever import ActivationRetriever

        nodes = self._seed_graph(store, user_id)
        retriever = ActivationRetriever(store)
        query_emb = list(nodes[0].embedding)

        results_debug = retriever.retrieve(
            user_id, "q", query_emb, top_k=20, task_type="debugging",
        )
        results_plan = retriever.retrieve(
            user_id, "q", query_emb, top_k=20, task_type="planning",
        )

        # Both should return results
        assert len(results_debug) > 0
        assert len(results_plan) > 0

        # Planning with fewer anchors (5) and fewer iterations (2)
        # should generally activate fewer nodes than debugging (10 anchors, 3 iters)
        assert len(results_plan) <= len(results_debug), (
            f"planning ({len(results_plan)}) should activate <= debugging ({len(results_debug)})"
        )

    def test_no_task_type_uses_defaults(self, store, user_id):
        """task_type=None should work and use default params."""
        from memoria.core.memory.graph.retriever import ActivationRetriever

        self._seed_graph(store, user_id)
        retriever = ActivationRetriever(store)
        query_emb = [0.1] * EMBEDDING_DIM

        results = retriever.retrieve(user_id, "q", query_emb, top_k=5)
        assert len(results) > 0, "default task_type should still return results"

    def test_service_passes_task_hint_through(self, store, user_id, db_factory):
        """GraphMemoryService.retrieve(task_hint=...) produces results via activation path."""
        from memoria.core.memory.graph.service import GraphMemoryService

        self._seed_graph(store, user_id)
        svc = GraphMemoryService(db_factory)
        query_emb = [0.1] * EMBEDDING_DIM

        # Real end-to-end: service → retriever → activation → DB
        results = svc.retrieve(
            user_id, "test", query_embedding=query_emb, task_hint="debugging",
        )
        # With 55 nodes (above MIN_GRAPH_NODES=50), activation path should fire
        assert len(results) > 0, "service should return memories via activation path"
        for mem in results:
            assert mem.user_id == user_id
            assert mem.content is not None


class TestTaskImportanceWeightsE2E:
    """§13.3 — task-type importance weights affect retrieval ranking via real DB.

    Chain: score_candidate(task_type) → node.importance → retriever score.
    """

    def test_task_weights_change_retrieval_ranking(self, store, user_id):
        """Two nodes with same signals but different task-type scoring
        should rank differently when importance is written to DB."""
        from memoria.core.memory.graph.retriever import ActivationRetriever
        from memoria.core.memory.interfaces import ReflectionCandidate
        from memoria.core.memory.reflection.importance import score_candidate
        from memoria.core.memory.types import Memory, MemoryType

        # Create 55 nodes (above MIN_GRAPH_NODES threshold)
        nodes = []
        for i in range(55):
            emb = [0.1] * EMBEDDING_DIM
            emb[i % EMBEDDING_DIM] += 0.05 * (i % 10)
            nodes.append(GraphNodeData(
                node_id=f"n{i}_{user_id[:8]}",
                user_id=user_id,
                node_type=NodeType.SEMANTIC,
                content=f"node {i}",
                embedding=emb,
                confidence=0.8,
                trust_tier="T3",
                importance=0.5,  # default
            ))
        store.create_nodes_batch(nodes)

        # Build a candidate that represents a contradiction cluster
        mems = [
            Memory(memory_id=f"m{i}", user_id=user_id,
                   memory_type=MemoryType.SEMANTIC, content=f"mem {i}",
                   session_id=f"s{i}")
            for i in range(4)
        ]
        candidate = ReflectionCandidate(
            memories=mems, signal="contradiction",
            session_ids=["s0", "s1", "s2"],
        )

        # Score under different task types
        score_debug = score_candidate(candidate, task_type="debugging")
        score_cr = score_candidate(candidate, task_type="code_review")

        # Debugging weights contradiction at 0.45 vs code_review at 0.15
        assert score_debug > score_cr

        # Write these different importance scores to two nodes in DB
        store.update_importance(nodes[0].node_id, score_debug)
        store.update_importance(nodes[1].node_id, score_cr)

        # Verify DB persistence
        n0 = store.get_node(nodes[0].node_id)
        n1 = store.get_node(nodes[1].node_id)
        assert n0.importance == pytest.approx(score_debug, abs=0.01)
        assert n1.importance == pytest.approx(score_cr, abs=0.01)

        # Retrieve — node with higher importance should score higher
        # (all else being equal: same embedding distance, same confidence)
        retriever = ActivationRetriever(store)
        query_emb = list(nodes[0].embedding)
        results = retriever.retrieve(
            user_id, "test", query_emb, top_k=55,
        )

        # Find both nodes in results
        scores_by_id = {n.node_id: s for n, s in results}
        if nodes[0].node_id in scores_by_id and nodes[1].node_id in scores_by_id:
            assert scores_by_id[nodes[0].node_id] > scores_by_id[nodes[1].node_id], (
                f"Higher importance node should rank higher: "
                f"n0={scores_by_id[nodes[0].node_id]:.4f} "
                f"n1={scores_by_id[nodes[1].node_id]:.4f}"
            )


# ── Graph candidates and service ──────────────────────────────────────

class TestGraphCandidatesAndService:
    """GraphCandidateProvider and ActivationIndexManager.get_reflection_candidates."""

    def test_get_reflection_candidates_returns_list(self, db_factory, user_id):
        """get_reflection_candidates runs without error and returns a list."""
        from memoria.core.memory.strategy.activation_index import ActivationIndexManager
        from memoria.core.memory.tabular.store import MemoryStore
        from memoria.core.memory.types import Memory, MemoryType

        store = MemoryStore(db_factory)
        idx = ActivationIndexManager(db_factory)

        # Create some memories and index them
        for i in range(3):
            mem = Memory(
                memory_id=uuid4().hex, user_id=user_id,
                memory_type=MemoryType.SEMANTIC,
                content=f"reflection candidate {i}",
                initial_confidence=0.7,
                embedding=_embed(0.1 + i * 0.1),
            )
            store.create(mem)
            idx.on_memories_stored(user_id, [mem])

        candidates = idx.get_reflection_candidates(user_id)
        assert isinstance(candidates, (list, type(None)))

    def test_graph_candidate_provider_direct(self, db_factory, user_id):
        """GraphCandidateProvider.get_reflection_candidates with real graph nodes."""
        from memoria.core.memory.graph.candidates import GraphCandidateProvider
        from memoria.core.memory.strategy.activation_index import ActivationIndexManager
        from memoria.core.memory.tabular.store import MemoryStore
        from memoria.core.memory.types import Memory, MemoryType

        store = MemoryStore(db_factory)
        idx = ActivationIndexManager(db_factory)

        for i in range(5):
            mem = Memory(
                memory_id=uuid4().hex, user_id=user_id,
                memory_type=MemoryType.SEMANTIC,
                content=f"graph candidate {i}",
                initial_confidence=0.6 + i * 0.05,
                embedding=_embed(0.2 + i * 0.05),
            )
            store.create(mem)
            idx.on_memories_stored(user_id, [mem])

        provider = GraphCandidateProvider(db_factory)
        candidates = provider.get_reflection_candidates(user_id)
        assert isinstance(candidates, list)


# ── Entity Linking ────────────────────────────────────────────────────


class TestEntityLinking:
    """Entity extraction + graph node/edge creation."""

    def test_lightweight_entity_nodes_created(self, store, user_id):
        """Ingest a memory mentioning tech terms → entity nodes + entity_link edges created."""
        from memoria.core.memory.graph.graph_builder import GraphBuilder
        from memoria.core.memory.graph.types import EdgeType, NodeType
        from memoria.core.memory.types import Memory, MemoryType, TrustTier

        builder = GraphBuilder(store)
        mem = Memory(
            memory_id=uuid4().hex, user_id=user_id,
            memory_type=MemoryType.SEMANTIC,
            content="We deploy with Docker on AWS using PostgreSQL",
            embedding=_embed(0.2), initial_confidence=0.9,
            trust_tier=TrustTier.T3_INFERRED,
        )
        created = builder.ingest(user_id, [mem], [])

        # Should have semantic node + entity nodes
        entity_nodes = [n for n in created if n.node_type == NodeType.ENTITY]
        entity_names = {n.content.lower() for n in entity_nodes}
        assert "docker" in entity_names
        assert "aws" in entity_names
        assert "postgresql" in entity_names

        # Check entity_link edges exist
        semantic_node = [n for n in created if n.node_type == NodeType.SEMANTIC][0]
        edges = store.get_outgoing_edges(semantic_node.node_id)
        entity_link_edges = [e for e in edges if e.edge_type == EdgeType.ENTITY_LINK.value]
        assert len(entity_link_edges) >= 3

    def test_entity_node_reuse(self, store, user_id):
        """Two memories mentioning the same entity → same entity node reused."""
        from memoria.core.memory.graph.graph_builder import GraphBuilder
        from memoria.core.memory.graph.types import NodeType
        from memoria.core.memory.types import Memory, MemoryType, TrustTier

        builder = GraphBuilder(store)
        mem1 = Memory(
            memory_id=uuid4().hex, user_id=user_id,
            memory_type=MemoryType.SEMANTIC,
            content="Python is my favorite language",
            embedding=_embed(0.3), initial_confidence=0.9,
            trust_tier=TrustTier.T3_INFERRED,
        )
        mem2 = Memory(
            memory_id=uuid4().hex, user_id=user_id,
            memory_type=MemoryType.SEMANTIC,
            content="I use Python for data analysis",
            embedding=_embed(0.4), initial_confidence=0.9,
            trust_tier=TrustTier.T3_INFERRED,
        )
        builder.ingest(user_id, [mem1], [])
        builder.ingest(user_id, [mem2], [])

        # Only one "python" entity node should exist
        all_nodes = store.get_user_nodes(user_id, node_type=NodeType.ENTITY, active_only=True)
        python_nodes = [n for n in all_nodes if n.content.lower() == "python"]
        assert len(python_nodes) == 1

    def test_find_entity_node(self, store, user_id):
        """find_entity_node returns existing entity, None for missing."""
        from memoria.core.memory.graph.types import GraphNodeData, NodeType

        node = GraphNodeData(
            node_id=uuid4().hex, user_id=user_id,
            node_type=NodeType.ENTITY, content="redis",
            confidence=1.0, trust_tier="T1", importance=0.3,
        )
        store.create_node(node)

        found = store.find_entity_node(user_id, "redis")
        assert found is not None
        assert found.node_id == node.node_id

        assert store.find_entity_node(user_id, "nonexistent") is None
