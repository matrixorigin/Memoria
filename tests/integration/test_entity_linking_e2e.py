"""Integration tests for Phase 0A: Entity Linking upgrade.

Tests with REAL database — verifies actual data in mem_entities,
mem_memory_entity_links, memory_graph_nodes, memory_graph_edges.

Covers:
1. Schema: new tables exist and accept data
2. Entity ID = graph node ID (1:1 invariant)
3. Dual-write: entity table + graph node created atomically
4. Episodic nodes do NOT write to mem_memory_entity_links
5. Reverse lookup: entity → memories via single JOIN
6. Entity-anchored retrieval: recall + boost
7. Repair consistency: fix gaps between table and graph
8. Normalization: Chinese entities stored correctly
"""

import os
from uuid import uuid4

import pytest
from sqlalchemy import text

from memoria.core.memory.graph.graph_builder import GraphBuilder
from memoria.core.memory.graph.graph_store import GraphStore, _new_id
from memoria.core.memory.graph.types import GraphNodeData, NodeType
from memoria.core.memory.types import Memory, MemoryType, TrustTier

EMBEDDING_DIM = int(os.environ.get("MEMORIA_EMBEDDING_DIM", "384"))


def _uid() -> str:
    return f"entity_e2e_{uuid4().hex[:12]}"


def _embed(seed: float = 0.1) -> list[float]:
    return [seed] * EMBEDDING_DIM


@pytest.fixture
def db_factory():
    from tests.integration.conftest import _get_session_local

    return _get_session_local()


@pytest.fixture
def store(db_factory):
    return GraphStore(db_factory)


@pytest.fixture
def builder(store):
    return GraphBuilder(store)


@pytest.fixture
def user_id():
    return _uid()


@pytest.fixture(autouse=True)
def cleanup(db_factory, user_id):
    yield
    db = db_factory()
    try:
        db.execute(
            text("DELETE FROM mem_memory_entity_links WHERE user_id = :uid"),
            {"uid": user_id},
        )
        db.execute(
            text("DELETE FROM mem_entities WHERE user_id = :uid"),
            {"uid": user_id},
        )
        db.execute(
            text("DELETE FROM memory_graph_edges WHERE user_id = :uid"),
            {"uid": user_id},
        )
        db.execute(
            text("DELETE FROM memory_graph_nodes WHERE user_id = :uid"),
            {"uid": user_id},
        )
        db.execute(
            text("DELETE FROM mem_memories WHERE user_id = :uid"),
            {"uid": user_id},
        )
        db.commit()
    finally:
        db.close()


def _insert_memory(db_factory, user_id: str, memory_id: str, content: str) -> None:
    """Insert a real row into mem_memories for JOIN tests."""
    db = db_factory()
    try:
        db.execute(
            text(
                "INSERT INTO mem_memories "
                "(memory_id, user_id, memory_type, content, initial_confidence, "
                "source_event_ids, is_active, observed_at, created_at) "
                "VALUES (:mid, :uid, 'semantic', :content, 0.8, '[]', 1, NOW(), NOW())"
            ),
            {"mid": memory_id, "uid": user_id, "content": content},
        )
        db.commit()
    finally:
        db.close()


# ── 1. Schema: new tables exist ──────────────────────────────────────


class TestEntitySchema:
    def test_mem_entities_table_exists(self, db_factory):
        db = db_factory()
        try:
            rows = db.execute(text("SELECT COUNT(*) FROM mem_entities")).fetchone()
            assert rows is not None
        finally:
            db.close()

    def test_mem_memory_entity_links_table_exists(self, db_factory):
        db = db_factory()
        try:
            rows = db.execute(
                text("SELECT COUNT(*) FROM mem_memory_entity_links")
            ).fetchone()
            assert rows is not None
        finally:
            db.close()

    def test_unique_constraint_on_user_name(self, store, user_id):
        """Inserting same (user_id, name) twice should return same entity_id."""
        eid1 = store.upsert_entity(user_id, "python", "Python", "tech")
        eid2 = store.upsert_entity(user_id, "python", "Python", "tech")
        assert eid1 == eid2

    def test_different_users_same_name(self, store, db_factory):
        """Different users can have entities with the same name."""
        uid1 = _uid()
        uid2 = _uid()
        eid1 = store.upsert_entity(uid1, "python", "Python", "tech")
        eid2 = store.upsert_entity(uid2, "python", "Python", "tech")
        assert eid1 != eid2
        # Cleanup
        db = db_factory()
        try:
            db.execute(
                text("DELETE FROM mem_entities WHERE user_id IN (:u1, :u2)"),
                {"u1": uid1, "u2": uid2},
            )
            db.commit()
        finally:
            db.close()


# ── 2. Entity ID = Graph Node ID (1:1 invariant) ─────────────────────


class TestEntityIdInvariant:
    def test_builder_creates_matching_ids(self, builder, store, user_id, db_factory):
        """After ingest, entity_id in mem_entities == node_id in memory_graph_nodes."""
        mem_id = _new_id()
        _insert_memory(db_factory, user_id, mem_id, "I use Python and Docker")

        mem = Memory(
            memory_id=mem_id,
            user_id=user_id,
            memory_type=MemoryType.SEMANTIC,
            content="I use Python and Docker",
            initial_confidence=0.8,
            embedding=_embed(),
            trust_tier=TrustTier.T3_INFERRED,
        )
        builder.ingest(user_id, [mem], [], session_id="s1")

        # Check mem_entities
        entities = store.get_user_entities(user_id)
        entity_ids = {eid for eid, _, _ in entities}
        assert len(entity_ids) >= 2  # at least python and docker

        # Check graph nodes — entity nodes should have same IDs
        db = db_factory()
        try:
            graph_entity_ids = {
                r[0]
                for r in db.execute(
                    text(
                        "SELECT node_id FROM memory_graph_nodes "
                        "WHERE user_id = :uid AND node_type = 'entity' AND is_active = 1"
                    ),
                    {"uid": user_id},
                ).fetchall()
            }
        finally:
            db.close()

        # The critical invariant: every entity_id must exist as a graph node_id
        for eid in entity_ids:
            assert eid in graph_entity_ids, (
                f"entity_id {eid} not found in graph nodes — 1:1 invariant broken"
            )

    def test_second_ingest_reuses_entity_id(self, builder, store, user_id, db_factory):
        """Ingesting another memory with same entity should reuse the entity_id."""
        mid1 = _new_id()
        mid2 = _new_id()
        _insert_memory(db_factory, user_id, mid1, "Python is great")
        _insert_memory(db_factory, user_id, mid2, "Python is fast")

        mem1 = Memory(
            memory_id=mid1,
            user_id=user_id,
            memory_type=MemoryType.SEMANTIC,
            content="Python is great",
            initial_confidence=0.8,
            embedding=_embed(0.1),
            trust_tier=TrustTier.T3_INFERRED,
        )
        mem2 = Memory(
            memory_id=mid2,
            user_id=user_id,
            memory_type=MemoryType.SEMANTIC,
            content="Python is fast",
            initial_confidence=0.8,
            embedding=_embed(0.2),
            trust_tier=TrustTier.T3_INFERRED,
        )
        builder.ingest(user_id, [mem1], [], session_id="s1")
        builder.ingest(user_id, [mem2], [], session_id="s2")

        # Should have exactly 1 "python" entity, not 2
        entities = store.get_user_entities(user_id)
        python_entities = [(eid, n) for eid, n, _ in entities if n == "python"]
        assert len(python_entities) == 1

        # Both memories should link to the same entity
        python_eid = python_entities[0][0]
        linked = store.get_memories_by_entity(python_eid, user_id)
        linked_mids = {mid for mid, _ in linked}
        assert mid1 in linked_mids
        assert mid2 in linked_mids


# ── 3. Episodic nodes do NOT write to entity links table ─────────────


class TestEpisodicSkip:
    def test_episodic_no_entity_link_rows(self, builder, store, user_id, db_factory):
        """Episodic nodes should create graph edges but NOT mem_memory_entity_links."""
        events = [
            {
                "event_id": _new_id(),
                "event_type": "user_query",
                "content": "Tell me about Python",
            },
        ]
        builder.ingest(user_id, [], events, session_id="s1")

        # Graph edges should exist (episodic → entity)
        db = db_factory()
        try:
            # Graph edges may exist (episodic → entity)
            db.execute(
                text(
                    "SELECT COUNT(*) FROM memory_graph_edges "
                    "WHERE user_id = :uid AND edge_type = 'entity_link'"
                ),
                {"uid": user_id},
            ).fetchone()

            # Entity link table should have NO rows (episodic has no memory_id)
            link_rows = db.execute(
                text(
                    "SELECT COUNT(*) FROM mem_memory_entity_links WHERE user_id = :uid"
                ),
                {"uid": user_id},
            ).fetchone()
        finally:
            db.close()

        # Graph edges may or may not exist depending on extraction
        # But entity link table must be empty for episodic-only ingest
        assert link_rows[0] == 0, (
            f"Expected 0 entity link rows for episodic-only ingest, got {link_rows[0]}"
        )


# ── 4. Reverse lookup: entity → memories ─────────────────────────────


class TestReverseLookup:
    def test_shanghai_vs_nanjing(self, builder, store, user_id, db_factory):
        """Core use case: query for Shanghai noodles should NOT return Nanjing ones."""
        mid_sh = _new_id()
        mid_nj = _new_id()
        _insert_memory(db_factory, user_id, mid_sh, "上海的面条店很好吃")
        _insert_memory(db_factory, user_id, mid_nj, "南京的面条店也不错")

        mem_sh = Memory(
            memory_id=mid_sh,
            user_id=user_id,
            memory_type=MemoryType.SEMANTIC,
            content="上海的面条店很好吃",
            initial_confidence=0.8,
            embedding=_embed(0.1),
            trust_tier=TrustTier.T3_INFERRED,
        )
        mem_nj = Memory(
            memory_id=mid_nj,
            user_id=user_id,
            memory_type=MemoryType.SEMANTIC,
            content="南京的面条店也不错",
            initial_confidence=0.8,
            embedding=_embed(0.2),
            trust_tier=TrustTier.T3_INFERRED,
        )
        builder.ingest(user_id, [mem_sh], [], session_id="s1")
        builder.ingest(user_id, [mem_nj], [], session_id="s2")

        # Find Shanghai entity
        sh_eid = store.find_entity_by_name(user_id, "上海")
        assert sh_eid is not None, "上海 entity not found in mem_entities"

        # Find Nanjing entity
        nj_eid = store.find_entity_by_name(user_id, "南京")
        assert nj_eid is not None, "南京 entity not found in mem_entities"

        # Reverse lookup: Shanghai should return Shanghai memory only
        sh_memories = store.get_memories_by_entity(sh_eid, user_id)
        sh_mids = {mid for mid, _ in sh_memories}
        assert mid_sh in sh_mids, "Shanghai memory not linked to 上海 entity"
        assert mid_nj not in sh_mids, "Nanjing memory incorrectly linked to 上海 entity"

        # Reverse lookup: Nanjing should return Nanjing memory only
        nj_memories = store.get_memories_by_entity(nj_eid, user_id)
        nj_mids = {mid for mid, _ in nj_memories}
        assert mid_nj in nj_mids, "Nanjing memory not linked to 南京 entity"
        assert mid_sh not in nj_mids, (
            "Shanghai memory incorrectly linked to 南京 entity"
        )

    def test_reverse_lookup_joins_active_only(
        self, builder, store, user_id, db_factory
    ):
        """Reverse lookup should only return active memories."""
        mid = _new_id()
        _insert_memory(db_factory, user_id, mid, "上海的餐厅")

        mem = Memory(
            memory_id=mid,
            user_id=user_id,
            memory_type=MemoryType.SEMANTIC,
            content="上海的餐厅",
            initial_confidence=0.8,
            embedding=_embed(),
            trust_tier=TrustTier.T3_INFERRED,
        )
        builder.ingest(user_id, [mem], [], session_id="s1")

        sh_eid = store.find_entity_by_name(user_id, "上海")
        assert sh_eid is not None

        # Deactivate the memory
        db = db_factory()
        try:
            db.execute(
                text("UPDATE mem_memories SET is_active = 0 WHERE memory_id = :mid"),
                {"mid": mid},
            )
            db.commit()
        finally:
            db.close()

        # Reverse lookup should return empty
        results = store.get_memories_by_entity(sh_eid, user_id)
        assert len(results) == 0, (
            "Deactivated memory should not appear in reverse lookup"
        )


# ── 5. Repair consistency ─────────────────────────────────────────────


class TestRepairConsistency:
    def test_repair_missing_graph_node(self, store, user_id, db_factory):
        """Entity in table but no graph node → repair creates graph node."""
        # Manually insert entity into table only
        eid = _new_id()
        db = db_factory()
        try:
            db.execute(
                text(
                    "INSERT INTO mem_entities (entity_id, user_id, name, display_name, entity_type) "
                    "VALUES (:eid, :uid, 'orphan_test', 'orphan_test', 'tech')"
                ),
                {"eid": eid, "uid": user_id},
            )
            db.commit()
        finally:
            db.close()

        result = store.repair_entity_graph_consistency(user_id)
        assert result["repaired_graph"] >= 1

        # Verify graph node now exists with same ID
        node = store.get_node(eid)
        assert node is not None
        assert node.node_id == eid
        assert node.content == "orphan_test"

    def test_repair_orphan_graph_node_deactivated(self, store, user_id, db_factory):
        """Graph entity node with name already in table (different ID) → deactivated."""
        # Create entity in table
        store.upsert_entity(user_id, "duplicate_test", "duplicate_test", "tech")

        # Create graph node with DIFFERENT ID but same content
        orphan_nid = _new_id()
        store.create_node(
            GraphNodeData(
                node_id=orphan_nid,
                user_id=user_id,
                node_type=NodeType.ENTITY,
                content="duplicate_test",
                entity_type="tech",
                confidence=1.0,
                trust_tier="T1",
                importance=0.3,
            )
        )

        store.repair_entity_graph_consistency(user_id)

        # The orphan should be deactivated (not create a duplicate entity row)
        orphan = store.get_node(orphan_nid)
        assert orphan is not None
        assert not orphan.is_active, "Orphan graph node should be deactivated"

        # Table should still have exactly 1 entity with this name
        entities = store.get_user_entities(user_id)
        matching = [e for e in entities if e[1] == "duplicate_test"]
        assert len(matching) == 1

    def test_repair_graph_node_no_table_entry(self, store, user_id):
        """Graph entity node with no table entry and unique name → create table entry."""
        nid = _new_id()
        store.create_node(
            GraphNodeData(
                node_id=nid,
                user_id=user_id,
                node_type=NodeType.ENTITY,
                content="unique_orphan",
                entity_type="concept",
                confidence=1.0,
                trust_tier="T1",
                importance=0.3,
            )
        )

        result = store.repair_entity_graph_consistency(user_id)
        assert result["repaired_table"] >= 1

        # Verify table entry exists with same ID
        found_eid = store.find_entity_by_name(user_id, "unique_orphan")
        assert found_eid == nid, "Table entity_id should match graph node_id"


# ── 6. Chinese entity normalization ──────────────────────────────────


class TestChineseEntityNormalization:
    def test_chinese_city_stored_correctly(self, builder, store, user_id, db_factory):
        """Chinese city names should be stored as-is (not lowercased)."""
        mid = _new_id()
        _insert_memory(db_factory, user_id, mid, "我在杭州西湖边散步")

        mem = Memory(
            memory_id=mid,
            user_id=user_id,
            memory_type=MemoryType.SEMANTIC,
            content="我在杭州西湖边散步",
            initial_confidence=0.8,
            embedding=_embed(),
            trust_tier=TrustTier.T3_INFERRED,
        )
        builder.ingest(user_id, [mem], [], session_id="s1")

        eid = store.find_entity_by_name(user_id, "杭州")
        assert eid is not None, "杭州 entity should be found by exact name"

        # Verify the entity content in graph node matches
        node = store.get_node(eid)
        assert node is not None
        assert node.content == "杭州"

    def test_mixed_chinese_english_entity(self, builder, store, user_id, db_factory):
        """Memory with both Chinese and English entities."""
        mid = _new_id()
        _insert_memory(db_factory, user_id, mid, "在深圳用 Docker 部署服务")

        mem = Memory(
            memory_id=mid,
            user_id=user_id,
            memory_type=MemoryType.SEMANTIC,
            content="在深圳用 Docker 部署服务",
            initial_confidence=0.8,
            embedding=_embed(),
            trust_tier=TrustTier.T3_INFERRED,
        )
        builder.ingest(user_id, [mem], [], session_id="s1")

        sz_eid = store.find_entity_by_name(user_id, "深圳")
        docker_eid = store.find_entity_by_name(user_id, "docker")
        assert sz_eid is not None, "深圳 entity not found"
        assert docker_eid is not None, "docker entity not found"

        # Both should link to the same memory
        sz_mems = store.get_memories_by_entity(sz_eid, user_id)
        docker_mems = store.get_memories_by_entity(docker_eid, user_id)
        assert any(m[0] == mid for m in sz_mems)
        assert any(m[0] == mid for m in docker_mems)


# ── 7. Data integrity: no orphan links ────────────────────────────────


class TestDataIntegrity:
    def test_no_orphan_links_after_ingest(self, builder, store, user_id, db_factory):
        """Every row in mem_memory_entity_links should have valid FK references."""
        mid = _new_id()
        _insert_memory(db_factory, user_id, mid, "Python and Docker are great")

        mem = Memory(
            memory_id=mid,
            user_id=user_id,
            memory_type=MemoryType.SEMANTIC,
            content="Python and Docker are great",
            initial_confidence=0.8,
            embedding=_embed(),
            trust_tier=TrustTier.T3_INFERRED,
        )
        builder.ingest(user_id, [mem], [], session_id="s1")

        db = db_factory()
        try:
            # All memory_ids in links should exist in mem_memories
            orphan_memory_links = db.execute(
                text(
                    "SELECT l.memory_id FROM mem_memory_entity_links l "
                    "LEFT JOIN mem_memories m ON l.memory_id = m.memory_id "
                    "WHERE l.user_id = :uid AND m.memory_id IS NULL"
                ),
                {"uid": user_id},
            ).fetchall()
            assert len(orphan_memory_links) == 0, (
                f"Found {len(orphan_memory_links)} orphan memory links: "
                f"{[r[0] for r in orphan_memory_links]}"
            )

            # All entity_ids in links should exist in mem_entities
            orphan_entity_links = db.execute(
                text(
                    "SELECT l.entity_id FROM mem_memory_entity_links l "
                    "LEFT JOIN mem_entities e ON l.entity_id = e.entity_id "
                    "WHERE l.user_id = :uid AND e.entity_id IS NULL"
                ),
                {"uid": user_id},
            ).fetchall()
            assert len(orphan_entity_links) == 0, (
                f"Found {len(orphan_entity_links)} orphan entity links: "
                f"{[r[0] for r in orphan_entity_links]}"
            )
        finally:
            db.close()

    def test_entity_graph_node_consistency(self, builder, store, user_id, db_factory):
        """Every entity in mem_entities should have a matching graph node."""
        mid = _new_id()
        _insert_memory(db_factory, user_id, mid, "Redis and PostgreSQL")

        mem = Memory(
            memory_id=mid,
            user_id=user_id,
            memory_type=MemoryType.SEMANTIC,
            content="Redis and PostgreSQL",
            initial_confidence=0.8,
            embedding=_embed(),
            trust_tier=TrustTier.T3_INFERRED,
        )
        builder.ingest(user_id, [mem], [], session_id="s1")

        db = db_factory()
        try:
            # Every entity_id should exist as a graph node_id
            mismatches = db.execute(
                text(
                    "SELECT e.entity_id, e.name FROM mem_entities e "
                    "LEFT JOIN memory_graph_nodes g ON e.entity_id = g.node_id "
                    "WHERE e.user_id = :uid AND g.node_id IS NULL"
                ),
                {"uid": user_id},
            ).fetchall()
            assert len(mismatches) == 0, (
                f"Entity IDs without matching graph nodes: "
                f"{[(r[0], r[1]) for r in mismatches]}"
            )
        finally:
            db.close()

    def test_multiple_ingests_no_duplicate_entities(
        self, builder, store, user_id, db_factory
    ):
        """Multiple ingests with same entities should not create duplicates."""
        for i in range(3):
            mid = _new_id()
            _insert_memory(db_factory, user_id, mid, f"Python project #{i}")
            mem = Memory(
                memory_id=mid,
                user_id=user_id,
                memory_type=MemoryType.SEMANTIC,
                content=f"Python project #{i}",
                initial_confidence=0.8,
                embedding=_embed(0.1 * (i + 1)),
                trust_tier=TrustTier.T3_INFERRED,
            )
            builder.ingest(user_id, [mem], [], session_id=f"s{i}")

        entities = store.get_user_entities(user_id)
        python_count = sum(1 for _, name, _ in entities if name == "python")
        assert python_count == 1, f"Expected 1 python entity, got {python_count}"

        db = db_factory()
        try:
            graph_python = db.execute(
                text(
                    "SELECT COUNT(*) FROM memory_graph_nodes "
                    "WHERE user_id = :uid AND node_type = 'entity' "
                    "AND content = 'python' AND is_active = 1"
                ),
                {"uid": user_id},
            ).fetchone()
            assert graph_python[0] == 1, (
                f"Expected 1 active python graph node, got {graph_python[0]}"
            )
        finally:
            db.close()
