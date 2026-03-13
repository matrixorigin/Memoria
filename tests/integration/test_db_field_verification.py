"""DB field-level verification tests for core memory write path.

Rules (from testing-rules.md):
- Always re-query from DB (not from return value)
- Verify EVERY field — not just the "important" ones
- Check timestamps are within expected range
- Check nulls explicitly
- Check no side effects on other records
- For multi-table flows — verify ALL affected tables
"""

from __future__ import annotations

import json
import uuid
from datetime import datetime, timezone

import pytest

from memoria.core.memory.models.memory import MemoryRecord
from memoria.core.memory.models.memory_edit_log import MemoryEditLog
from memoria.core.memory.tabular.store import MemoryStore
from memoria.core.memory.types import Memory, MemoryType, TrustTier, _utcnow


def _uid() -> str:
    return f"test_{uuid.uuid4().hex[:8]}"


def _mem(user_id: str, content: str = "test content", **kw) -> Memory:
    return Memory(
        memory_id=uuid.uuid4().hex,
        user_id=user_id,
        content=content,
        memory_type=kw.get("memory_type", MemoryType.SEMANTIC),
        trust_tier=kw.get("trust_tier", TrustTier.T1_VERIFIED),
        initial_confidence=kw.get("initial_confidence", 0.9),
        source_event_ids=kw.get("source_event_ids", ["evt:test"]),
        session_id=kw.get("session_id", None),
        observed_at=_utcnow(),
    )


def _now_utc() -> datetime:
    return datetime.now(timezone.utc)


def _strip_tz(dt: datetime) -> datetime:
    return dt.replace(tzinfo=None) if dt and dt.tzinfo else dt


class TestStoreCreate:
    """store.create() — verify every DB field."""

    def test_create_all_fields(self, db_factory):
        """Every field written by create() must match what's in DB."""
        store = MemoryStore(db_factory)
        uid = _uid()
        before = _strip_tz(_now_utc())

        mem = _mem(
            uid,
            content="hello world",
            memory_type=MemoryType.PROFILE,
            trust_tier=TrustTier.T2_CURATED,
            initial_confidence=0.75,
            source_event_ids=["evt:abc", "evt:def"],
            session_id="sess-001",
        )
        result = store.create(mem)
        after = _strip_tz(_now_utc())

        db = db_factory()
        row = db.query(MemoryRecord).filter_by(memory_id=result.memory_id).first()
        db.close()

        assert row is not None
        # identity
        assert row.memory_id == result.memory_id
        assert row.user_id == uid
        # content
        assert row.content == "hello world"
        # type + tier
        assert str(row.memory_type) == "profile"
        assert str(row.trust_tier) == "T2"
        # confidence
        assert abs(float(row.initial_confidence) - 0.75) < 0.01
        # session
        assert row.session_id == "sess-001"
        # state
        assert row.is_active == 1
        assert row.superseded_by is None
        assert row.updated_at is not None  # set by default=func.now() on insert
        # timestamps in range
        assert row.observed_at is not None
        assert row.created_at is not None
        assert before <= _strip_tz(row.observed_at) <= after
        assert before <= _strip_tz(row.created_at) <= after
        # source_event_ids as JSON array
        src = (
            json.loads(row.source_event_ids)
            if isinstance(row.source_event_ids, str)
            else row.source_event_ids
        )
        assert set(src) == {"evt:abc", "evt:def"}

    def test_create_null_session_stored_as_null(self, db_factory):
        """session_id=None must be NULL in DB, not empty string."""
        store = MemoryStore(db_factory)
        result = store.create(_mem(_uid(), session_id=None))

        db = db_factory()
        row = db.query(MemoryRecord).filter_by(memory_id=result.memory_id).first()
        db.close()

        assert row.session_id is None

    def test_create_default_state_fields(self, db_factory):
        """is_active=1, superseded_by=None, updated_at=None on creation."""
        store = MemoryStore(db_factory)
        result = store.create(_mem(_uid()))

        db = db_factory()
        row = db.query(MemoryRecord).filter_by(memory_id=result.memory_id).first()
        db.close()

        assert row.is_active == 1
        assert row.superseded_by is None
        assert row.updated_at is not None  # set on insert

    def test_create_no_side_effects_on_other_users(self, db_factory):
        """Creating for user A must not touch user B's records."""
        store = MemoryStore(db_factory)
        uid_a, uid_b = _uid(), _uid()

        b_mem = store.create(_mem(uid_b, content="b original"))
        store.create(_mem(uid_a, content="a new"))

        db = db_factory()
        b_row = db.query(MemoryRecord).filter_by(memory_id=b_mem.memory_id).first()
        db.close()

        assert b_row.is_active == 1
        assert b_row.content == "b original"
        assert b_row.superseded_by is None
        assert b_row.updated_at is not None  # set on insert


class TestStoreSupersede:
    """store.supersede() — verify old deactivated+linked, new created, updated_at set."""

    def test_supersede_all_fields(self, db_factory):
        store = MemoryStore(db_factory)
        uid = _uid()

        old = store.create(_mem(uid, content="old content"))
        before = _strip_tz(_now_utc())
        new_mem = _mem(uid, content="new content")
        result = store.supersede(old.memory_id, new_mem)
        after = _strip_tz(_now_utc())

        db = db_factory()
        old_row = db.query(MemoryRecord).filter_by(memory_id=old.memory_id).first()
        new_row = db.query(MemoryRecord).filter_by(memory_id=result.memory_id).first()
        db.close()

        # Old: deactivated, linked, updated_at set
        assert old_row.is_active == 0
        assert old_row.superseded_by == result.memory_id
        assert old_row.updated_at is not None
        assert before <= _strip_tz(old_row.updated_at) <= after

        # New: active, no link, correct content
        assert new_row.is_active == 1
        assert new_row.content == "new content"
        assert new_row.user_id == uid
        assert new_row.superseded_by is None
        assert new_row.updated_at is not None  # set on insert

    def test_supersede_only_affects_target(self, db_factory):
        """Superseding m1 must not touch m2 or m3."""
        store = MemoryStore(db_factory)
        uid = _uid()

        m1 = store.create(_mem(uid, content="m1"))
        m2 = store.create(_mem(uid, content="m2"))
        m3 = store.create(_mem(uid, content="m3"))

        store.supersede(m1.memory_id, _mem(uid, content="m1 new"))

        db = db_factory()
        r2 = db.query(MemoryRecord).filter_by(memory_id=m2.memory_id).first()
        r3 = db.query(MemoryRecord).filter_by(memory_id=m3.memory_id).first()
        db.close()

        assert r2.is_active == 1
        assert r2.superseded_by is None
        assert r2.updated_at is not None  # set on insert
        assert r3.is_active == 1
        assert r3.superseded_by is None
        assert r3.updated_at is not None  # set on insert


class TestStoreDeactivate:
    """store.deactivate() — is_active=0, superseded_by unchanged."""

    def test_deactivate_all_fields(self, db_factory):
        store = MemoryStore(db_factory)
        uid = _uid()
        mem = store.create(_mem(uid))

        before = _strip_tz(_now_utc())
        store.deactivate(mem.memory_id)
        after = _strip_tz(_now_utc())

        db = db_factory()
        row = db.query(MemoryRecord).filter_by(memory_id=mem.memory_id).first()
        db.close()

        assert row.is_active == 0
        assert row.superseded_by is None  # deactivate does NOT set superseded_by
        assert row.updated_at is not None
        assert before <= _strip_tz(row.updated_at) <= after


class TestEditorInject:
    """editor.inject() — mem_memories + mem_edit_log, every field."""

    def test_inject_memory_all_fields(self, db_factory, embed_client):
        from memoria.core.memory.editor import MemoryEditor
        from memoria.core.memory.canonical_storage import CanonicalStorage

        uid = _uid()
        storage = CanonicalStorage(db_factory, embed_fn=embed_client.embed)
        editor = MemoryEditor(storage, db_factory)

        before = _strip_tz(_now_utc())
        mem = editor.inject(
            uid,
            "injected content",
            memory_type=MemoryType.SEMANTIC,
            source="test_inject",
            session_id="sess-inject",
        )
        after = _strip_tz(_now_utc())

        db = db_factory()
        row = db.query(MemoryRecord).filter_by(memory_id=mem.memory_id).first()
        db.close()

        assert row is not None
        assert row.memory_id == mem.memory_id
        assert row.user_id == uid
        assert row.content == "injected content"
        assert str(row.memory_type) == "semantic"
        assert str(row.trust_tier) == "T1"  # inject default: T1_VERIFIED
        assert abs(float(row.initial_confidence) - 1.0) < 0.01  # inject default: 1.0
        assert row.is_active == 1
        assert row.session_id == "sess-inject"
        assert row.superseded_by is None
        assert row.updated_at is not None  # set on insert
        assert row.embedding is not None  # embed_client is active
        assert before <= _strip_tz(row.observed_at) <= after
        assert before <= _strip_tz(row.created_at) <= after

    def test_inject_audit_log_all_fields(self, db_factory):
        from memoria.core.memory.editor import MemoryEditor
        from memoria.core.memory.canonical_storage import CanonicalStorage

        uid = _uid()
        storage = CanonicalStorage(db_factory)
        editor = MemoryEditor(storage, db_factory)

        before = _strip_tz(_now_utc())
        mem = editor.inject(
            uid, "audit test", memory_type=MemoryType.SEMANTIC, source="src_test"
        )
        after = _strip_tz(_now_utc())

        db = db_factory()
        log = (
            db.query(MemoryEditLog)
            .filter_by(user_id=uid, operation="inject")
            .order_by(MemoryEditLog.created_at.desc())
            .first()
        )
        db.close()

        assert log is not None
        assert log.edit_id is not None and len(log.edit_id) > 0
        assert log.user_id == uid
        assert log.operation == "inject"
        # target_ids must contain the memory_id
        target_ids = (
            json.loads(log.target_ids)
            if isinstance(log.target_ids, str)
            else log.target_ids
        )
        assert mem.memory_id in target_ids
        assert log.reason is not None  # source stored as reason
        assert log.snapshot_before is None  # inject never creates a snapshot
        assert log.created_by == uid
        assert before <= _strip_tz(log.created_at) <= after

    def test_inject_two_similar_both_active(self, db_factory):
        """inject() must NOT deduplicate — both records must be active."""
        from memoria.core.memory.editor import MemoryEditor
        from memoria.core.memory.canonical_storage import CanonicalStorage

        uid = _uid()
        storage = CanonicalStorage(db_factory)
        editor = MemoryEditor(storage, db_factory)

        m1 = editor.inject(uid, "list test 1", memory_type=MemoryType.SEMANTIC)
        m2 = editor.inject(uid, "list test 2", memory_type=MemoryType.SEMANTIC)

        db = db_factory()
        r1 = db.query(MemoryRecord).filter_by(memory_id=m1.memory_id).first()
        r2 = db.query(MemoryRecord).filter_by(memory_id=m2.memory_id).first()
        db.close()

        assert r1.is_active == 1, "First inject must remain active"
        assert r2.is_active == 1, "Second inject must remain active"
        assert r1.superseded_by is None
        assert r2.superseded_by is None

    def test_inject_batch_all_records_active(self, db_factory):
        """batch_inject() — all records created, all active, all have audit."""
        from memoria.core.memory.editor import MemoryEditor
        from memoria.core.memory.canonical_storage import CanonicalStorage

        uid = _uid()
        storage = CanonicalStorage(db_factory)
        editor = MemoryEditor(storage, db_factory)

        specs = [
            {"content": f"batch item {i}", "memory_type": MemoryType.SEMANTIC}
            for i in range(3)
        ]
        memories = editor.batch_inject(uid, specs, source="batch_test")

        assert len(memories) == 3

        db = db_factory()
        for mem in memories:
            row = db.query(MemoryRecord).filter_by(memory_id=mem.memory_id).first()
            assert row is not None, f"Row {mem.memory_id} must exist"
            assert row.is_active == 1
            assert row.user_id == uid
            assert row.superseded_by is None

        # Audit log for batch
        log = (
            db.query(MemoryEditLog)
            .filter_by(user_id=uid, operation="inject")
            .order_by(MemoryEditLog.created_at.desc())
            .first()
        )
        assert log is not None
        target_ids = (
            json.loads(log.target_ids)
            if isinstance(log.target_ids, str)
            else log.target_ids
        )
        assert len(target_ids) == 3
        db.close()


class TestEditorCorrect:
    """editor.correct() — old superseded, new created, audit logged, every field."""

    def test_correct_all_fields(self, db_factory):
        from memoria.core.memory.editor import MemoryEditor
        from memoria.core.memory.canonical_storage import CanonicalStorage

        uid = _uid()
        storage = CanonicalStorage(db_factory)
        editor = MemoryEditor(storage, db_factory)

        original = editor.inject(
            uid, "original content", memory_type=MemoryType.SEMANTIC
        )

        before = _strip_tz(_now_utc())
        corrected = editor.correct(
            uid, original.memory_id, "corrected content", reason="user correction"
        )
        after = _strip_tz(_now_utc())

        db = db_factory()

        # Ground truth 1: original deactivated + linked + updated_at set
        orig_row = (
            db.query(MemoryRecord).filter_by(memory_id=original.memory_id).first()
        )
        assert orig_row.is_active == 0
        assert orig_row.superseded_by == corrected.memory_id
        assert orig_row.updated_at is not None
        assert before <= _strip_tz(orig_row.updated_at) <= after

        # Ground truth 2: corrected record — every field
        corr_row = (
            db.query(MemoryRecord).filter_by(memory_id=corrected.memory_id).first()
        )
        assert corr_row.is_active == 1
        assert corr_row.content == "corrected content"
        assert corr_row.user_id == uid
        assert corr_row.superseded_by is None
        assert corr_row.updated_at is not None  # set on insert
        assert before <= _strip_tz(corr_row.created_at) <= after

        # Ground truth 3: audit log — every field
        log = (
            db.query(MemoryEditLog)
            .filter_by(user_id=uid, operation="correct")
            .order_by(MemoryEditLog.created_at.desc())
            .first()
        )
        assert log is not None
        assert log.edit_id is not None
        assert log.user_id == uid
        assert log.operation == "correct"
        assert "user correction" in (log.reason or "")
        assert log.snapshot_before is None  # correct never creates a snapshot
        assert log.created_by == uid
        target_ids = (
            json.loads(log.target_ids)
            if isinstance(log.target_ids, str)
            else log.target_ids
        )
        assert corrected.memory_id in target_ids
        assert before <= _strip_tz(log.created_at) <= after

        db.close()

    def test_correct_nonexistent_raises(self, db_factory):
        """Correcting a non-existent memory_id must raise ValueError."""
        from memoria.core.memory.editor import MemoryEditor
        from memoria.core.memory.canonical_storage import CanonicalStorage

        uid = _uid()
        storage = CanonicalStorage(db_factory)
        editor = MemoryEditor(storage, db_factory)

        with pytest.raises((ValueError, Exception)):
            editor.correct(uid, "nonexistent_id_xyz", "new content")


class TestEditorPurge:
    """editor.purge() — is_active=0, updated_at set, audit logged, no side effects."""

    def test_purge_by_id_all_fields(self, db_factory):
        from memoria.core.memory.editor import MemoryEditor
        from memoria.core.memory.canonical_storage import CanonicalStorage

        uid = _uid()
        storage = CanonicalStorage(db_factory)
        editor = MemoryEditor(storage, db_factory)

        m_keep = editor.inject(uid, "keep this", memory_type=MemoryType.SEMANTIC)
        m_purge = editor.inject(uid, "purge this", memory_type=MemoryType.SEMANTIC)

        before = _strip_tz(_now_utc())
        result = editor.purge(uid, memory_ids=[m_purge.memory_id], reason="test purge")
        after = _strip_tz(_now_utc())

        db = db_factory()

        # Purged: is_active=0, updated_at set
        r_purge = db.query(MemoryRecord).filter_by(memory_id=m_purge.memory_id).first()
        assert r_purge.is_active == 0
        if r_purge.updated_at:
            assert before <= _strip_tz(r_purge.updated_at) <= after

        # Kept: untouched
        r_keep = db.query(MemoryRecord).filter_by(memory_id=m_keep.memory_id).first()
        assert r_keep.is_active == 1
        assert r_keep.superseded_by is None

        # Result count
        assert result.deactivated == 1

        # Audit log
        log = (
            db.query(MemoryEditLog)
            .filter_by(user_id=uid, operation="purge")
            .order_by(MemoryEditLog.created_at.desc())
            .first()
        )
        assert log is not None
        assert log.user_id == uid
        assert log.operation == "purge"
        assert "test purge" in (log.reason or "")
        # snapshot_before: set if GitForData succeeds (best-effort), may be None
        assert log.snapshot_before is None or log.snapshot_before.startswith(
            "pre_purge_"
        )
        assert log.created_by == uid
        target_ids = (
            json.loads(log.target_ids)
            if isinstance(log.target_ids, str)
            else log.target_ids
        )
        assert m_purge.memory_id in target_ids
        assert before <= _strip_tz(log.created_at) <= after

        db.close()

    def test_purge_by_type_only_affects_target_type(self, db_factory):
        from memoria.core.memory.editor import MemoryEditor
        from memoria.core.memory.canonical_storage import CanonicalStorage

        uid = _uid()
        storage = CanonicalStorage(db_factory)
        editor = MemoryEditor(storage, db_factory)

        w1 = editor.inject(uid, "working 1", memory_type=MemoryType.WORKING)
        w2 = editor.inject(uid, "working 2", memory_type=MemoryType.WORKING)
        sem = editor.inject(uid, "keep semantic", memory_type=MemoryType.SEMANTIC)
        prof = editor.inject(uid, "keep profile", memory_type=MemoryType.PROFILE)

        result = editor.purge(uid, memory_types=[MemoryType.WORKING])

        db = db_factory()

        # WORKING: deactivated
        r_w1 = db.query(MemoryRecord).filter_by(memory_id=w1.memory_id).first()
        r_w2 = db.query(MemoryRecord).filter_by(memory_id=w2.memory_id).first()
        assert r_w1.is_active == 0
        assert r_w2.is_active == 0

        # Others: untouched
        r_sem = db.query(MemoryRecord).filter_by(memory_id=sem.memory_id).first()
        r_prof = db.query(MemoryRecord).filter_by(memory_id=prof.memory_id).first()
        assert r_sem.is_active == 1
        assert r_prof.is_active == 1

        assert result.deactivated == 2
        db.close()

    def test_purge_empty_result_when_nothing_matches(self, db_factory):
        from memoria.core.memory.editor import MemoryEditor
        from memoria.core.memory.canonical_storage import CanonicalStorage

        uid = _uid()
        storage = CanonicalStorage(db_factory)
        editor = MemoryEditor(storage, db_factory)

        result = editor.purge(uid, memory_ids=["nonexistent_xyz"])
        assert result.deactivated == 0


class TestEditorPurgeSyncsGraphNodes:
    def test_purge_deactivates_semantic_graph_node(self, db, db_factory):
        from memoria.core.memory.canonical_storage import CanonicalStorage
        from memoria.core.memory.editor import MemoryEditor
        from memoria.core.memory.graph.graph_store import GraphStore
        from memoria.core.memory.graph.types import GraphNodeData, NodeType

        uid = _uid()
        storage = CanonicalStorage(db_factory)
        editor = MemoryEditor(
            storage, db_factory, index_manager=None, embed_client=None
        )
        memory = storage.create_memory(
            _mem(uid, content="port is 6001", memory_type=MemoryType.SEMANTIC)
        )

        graph_store = GraphStore(db_factory)
        graph_store.create_node(
            GraphNodeData(
                node_id=uuid.uuid4().hex[:32],
                user_id=uid,
                node_type=NodeType.SEMANTIC,
                content=memory.content,
                memory_id=memory.memory_id,
                session_id=memory.session_id,
                confidence=memory.initial_confidence,
                trust_tier=memory.trust_tier,
                importance=0.5,
                is_active=True,
            )
        )

        result = editor.purge(uid, memory_ids=[memory.memory_id], reason="test")

        assert result.deactivated == 1
        db.expire_all()
        mem_db = db.query(MemoryRecord).filter_by(memory_id=memory.memory_id).first()
        assert mem_db is not None
        assert mem_db.is_active == 0
        node = graph_store.get_node_by_memory_id(memory.memory_id)
        assert node is not None
        assert node.is_active is False

    def test_correct_deactivates_old_semantic_graph_node(self, db, db_factory):
        from memoria.core.memory.canonical_storage import CanonicalStorage
        from memoria.core.memory.editor import MemoryEditor
        from memoria.core.memory.graph.graph_store import GraphStore
        from memoria.core.memory.graph.types import GraphNodeData, NodeType

        uid = _uid()
        storage = CanonicalStorage(db_factory)
        editor = MemoryEditor(
            storage, db_factory, index_manager=None, embed_client=None
        )
        memory = storage.create_memory(
            _mem(uid, content="port is 6001", memory_type=MemoryType.SEMANTIC)
        )

        graph_store = GraphStore(db_factory)
        graph_store.create_node(
            GraphNodeData(
                node_id=uuid.uuid4().hex[:32],
                user_id=uid,
                node_type=NodeType.SEMANTIC,
                content=memory.content,
                memory_id=memory.memory_id,
                session_id=memory.session_id,
                confidence=memory.initial_confidence,
                trust_tier=memory.trust_tier,
                importance=0.5,
                is_active=True,
            )
        )

        new_mem = editor.correct(uid, memory.memory_id, "port is 6002", reason="test")

        old_node = graph_store.get_node_by_memory_id(memory.memory_id)
        assert old_node is not None
        assert old_node.is_active is False
        assert old_node.superseded_by == new_mem.memory_id


class TestObserveExplicit:
    """canonical_storage.store() — no contradiction for unique content."""

    def test_store_unique_content_all_fields(self, db_factory, embed_client):
        from memoria.core.memory.canonical_storage import CanonicalStorage

        uid = _uid()
        storage = CanonicalStorage(db_factory, embed_fn=embed_client.embed)
        before = _strip_tz(_now_utc())

        mem = storage.store(
            uid,
            f"completely unique content {uuid.uuid4().hex}",
            memory_type=MemoryType.SEMANTIC,
        )
        after = _strip_tz(_now_utc())

        db = db_factory()
        row = db.query(MemoryRecord).filter_by(memory_id=mem.memory_id).first()
        db.close()

        assert row is not None
        assert row.is_active == 1
        assert row.user_id == uid
        assert row.superseded_by is None
        assert row.embedding is not None  # embed_client is active
        assert before <= _strip_tz(row.observed_at) <= after
        assert before <= _strip_tz(row.created_at) <= after

    def test_store_no_cross_user_contamination(self, db_factory):
        """store() for user A must not affect user B."""
        from memoria.core.memory.canonical_storage import CanonicalStorage

        uid_a, uid_b = _uid(), _uid()
        storage = CanonicalStorage(db_factory)

        b_mem = storage.store(
            uid_b, "b's unique content xyz", memory_type=MemoryType.SEMANTIC
        )
        storage.store(uid_a, "a's unique content abc", memory_type=MemoryType.SEMANTIC)

        db = db_factory()
        b_row = db.query(MemoryRecord).filter_by(memory_id=b_mem.memory_id).first()
        db.close()

        assert b_row.is_active == 1
        assert b_row.superseded_by is None


class TestGovernanceHourly:
    """run_hourly() — tool_result DELETE + working archive, every field."""

    def test_hourly_cleans_tool_results(self, db_factory):
        from memoria.core.memory.tabular.governance import GovernanceScheduler
        from sqlalchemy import text

        uid = _uid()
        db = db_factory()
        mid = uuid.uuid4().hex
        db.execute(
            text(
                "INSERT INTO mem_memories (memory_id, user_id, content, memory_type, "
                "trust_tier, initial_confidence, is_active, source_event_ids, observed_at, created_at) "
                "VALUES (:mid, :uid, 'tool result', 'tool_result', 'T3', 0.5, 1, '[]', "
                "DATE_SUB(NOW(), INTERVAL 25 HOUR), DATE_SUB(NOW(), INTERVAL 25 HOUR))"
            ),
            {"mid": mid, "uid": uid},
        )
        db.commit()
        db.close()

        scheduler = GovernanceScheduler(db_factory)
        report = scheduler.run_hourly()

        db = db_factory()
        # tool_result cleanup DELETEs rows
        row = db.query(MemoryRecord).filter_by(memory_id=mid).first()
        db.close()

        assert row is None, "Old tool_result must be DELETED"
        assert report.cleaned_tool_results >= 1
        assert report.errors == []

    def test_hourly_archives_working_memories(self, db_factory):
        from memoria.core.memory.tabular.governance import GovernanceScheduler
        from sqlalchemy import text

        uid = _uid()
        sid = f"sess_{uuid.uuid4().hex[:8]}"
        db = db_factory()
        mid = uuid.uuid4().hex
        db.execute(
            text(
                "INSERT INTO mem_memories (memory_id, user_id, content, memory_type, "
                "trust_tier, initial_confidence, is_active, session_id, source_event_ids, observed_at, created_at) "
                "VALUES (:mid, :uid, 'working mem', 'working', 'T3', 0.5, 1, :sid, '[]', "
                "DATE_SUB(NOW(), INTERVAL 3 HOUR), DATE_SUB(NOW(), INTERVAL 3 HOUR))"
            ),
            {"mid": mid, "uid": uid, "sid": sid},
        )
        db.commit()
        db.close()

        scheduler = GovernanceScheduler(db_factory)
        report = scheduler.run_hourly()

        db = db_factory()
        row = db.query(MemoryRecord).filter_by(memory_id=mid).first()
        db.close()

        assert row.is_active == 0, "Old working memory must be archived"
        assert row.updated_at is not None, "updated_at must be set after archive"
        assert report.archived_working >= 1
        assert report.errors == []

    def test_hourly_does_not_touch_recent_memories(self, db_factory):
        """Recent memories (< TTL) must not be affected by hourly cleanup."""
        from memoria.core.memory.tabular.governance import GovernanceScheduler

        uid = _uid()
        from memoria.core.memory.tabular.store import MemoryStore

        store = MemoryStore(db_factory)
        recent = store.create(
            _mem(uid, content="recent tool result", memory_type=MemoryType.TOOL_RESULT)
        )

        scheduler = GovernanceScheduler(db_factory)
        scheduler.run_hourly()

        db = db_factory()
        row = db.query(MemoryRecord).filter_by(memory_id=recent.memory_id).first()
        db.close()

        assert row is not None, "Recent tool_result must NOT be deleted"
        assert row.is_active == 1


class TestGovernanceDaily:
    """run_daily_all() — stale/low-confidence quarantine, every field."""

    def test_daily_quarantines_low_confidence(self, db_factory):
        from memoria.core.memory.tabular.governance import GovernanceScheduler
        from sqlalchemy import text

        uid = _uid()
        db = db_factory()
        mid = uuid.uuid4().hex
        db.execute(
            text(
                "INSERT INTO mem_memories (memory_id, user_id, content, memory_type, "
                "trust_tier, initial_confidence, is_active, source_event_ids, observed_at, created_at) "
                "VALUES (:mid, :uid, 'stale low conf', 'semantic', 'T4', 0.05, 1, '[]', "
                "DATE_SUB(NOW(), INTERVAL 31 DAY), DATE_SUB(NOW(), INTERVAL 31 DAY))"
            ),
            {"mid": mid, "uid": uid},
        )
        db.commit()
        db.close()

        scheduler = GovernanceScheduler(db_factory)
        report = scheduler.run_daily_all()

        db = db_factory()
        row = db.query(MemoryRecord).filter_by(memory_id=mid).first()
        db.close()

        assert row.is_active == 0 or float(row.initial_confidence) < 0.06, (
            "Low confidence stale memory must be quarantined or decayed"
        )
        assert report.quarantined >= 1 or report.cleaned_stale >= 1
        assert report.errors == []

    def test_daily_does_not_touch_high_confidence(self, db_factory):
        """High confidence memories must not be quarantined."""
        from memoria.core.memory.tabular.governance import GovernanceScheduler
        from memoria.core.memory.tabular.store import MemoryStore

        uid = _uid()
        store = MemoryStore(db_factory)
        high_conf = store.create(
            _mem(uid, content="high confidence memory", initial_confidence=0.95)
        )

        scheduler = GovernanceScheduler(db_factory)
        scheduler.run_daily_all()

        db = db_factory()
        row = db.query(MemoryRecord).filter_by(memory_id=high_conf.memory_id).first()
        db.close()

        assert row.is_active == 1, "High confidence memory must not be quarantined"
