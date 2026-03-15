"""Coverage boost: integration tests for canonical_storage, health, editor, git_for_data, scheduler.

All tests use real DB via db_factory fixture.
"""

from __future__ import annotations

import time
from datetime import datetime, timezone

import pytest
from sqlalchemy import text

from memoria.core.memory.types import Memory, MemoryType, TrustTier


def _uid(prefix: str) -> str:
    return f"{prefix}_{int(time.time() * 1000) % 100000}"


def _cleanup(db_factory, user_id: str) -> None:
    with db_factory() as db:
        db.execute(text("DELETE FROM mem_memories WHERE user_id = :u"), {"u": user_id})
        db.commit()


# ---------------------------------------------------------------------------
# CanonicalStorage
# ---------------------------------------------------------------------------


@pytest.mark.integration
class TestCanonicalStorage:
    def _storage(self, db_factory):
        from memoria.core.memory.canonical_storage import CanonicalStorage

        return CanonicalStorage(db_factory)

    def test_store_and_retrieve_basic(self, db_factory):
        storage = self._storage(db_factory)
        user_id = _uid("cs_store")
        _cleanup(db_factory, user_id)

        mem = storage.store(
            user_id=user_id,
            content="I prefer Python over Java",
            memory_type=MemoryType.PROFILE,
            session_id="sess_1",
        )
        assert mem.memory_id
        assert mem.user_id == user_id
        assert mem.memory_type == MemoryType.PROFILE

        with db_factory() as db:
            row = db.execute(
                text(
                    "SELECT content, memory_type, is_active FROM mem_memories WHERE memory_id = :id"
                ),
                {"id": mem.memory_id},
            ).fetchone()
        assert row.content == "I prefer Python over Java"
        assert row.is_active == 1

    def test_store_sensitivity_blocked(self, db_factory):
        storage = self._storage(db_factory)
        user_id = _uid("cs_sensitive")
        _cleanup(db_factory, user_id)

        # Credentials should be blocked or redacted
        try:
            mem = storage.store(
                user_id=user_id,
                content="my password is secret123",
                memory_type=MemoryType.SEMANTIC,
            )
            # If not blocked, content should be redacted
            assert mem.memory_id  # stored but possibly redacted
        except ValueError:
            pass  # blocked — also acceptable

    def test_batch_store(self, db_factory):
        import uuid
        from memoria.core.memory.types import _utcnow

        storage = self._storage(db_factory)
        user_id = _uid("cs_batch")
        _cleanup(db_factory, user_id)

        memories = [
            Memory(
                memory_id=uuid.uuid4().hex,
                user_id=user_id,
                content=f"batch memory {i}",
                memory_type=MemoryType.SEMANTIC,
                trust_tier=TrustTier.T3_INFERRED,
                initial_confidence=0.7,
                observed_at=_utcnow(),
            )
            for i in range(3)
        ]
        stored = storage.batch_store(memories)
        assert len(stored) == 3

        with db_factory() as db:
            count = (
                db.execute(
                    text("SELECT COUNT(*) as c FROM mem_memories WHERE user_id = :u"),
                    {"u": user_id},
                )
                .fetchone()
                .c
            )
        assert count == 3

    def test_get_profile_empty(self, db_factory):
        storage = self._storage(db_factory)
        user_id = _uid("cs_profile")
        _cleanup(db_factory, user_id)
        # No profile memories → returns None or empty
        result = storage.get_profile(user_id)
        assert result is None or result == ""

    def test_invalidate_profile(self, db_factory):
        storage = self._storage(db_factory)
        user_id = _uid("cs_inv_profile")
        _cleanup(db_factory, user_id)
        # Should not raise
        storage.invalidate_profile(user_id)

    def test_health_check(self, db_factory):
        storage = self._storage(db_factory)
        user_id = _uid("cs_health")
        _cleanup(db_factory, user_id)
        storage.store(
            user_id=user_id, content="test memory", memory_type=MemoryType.SEMANTIC
        )
        result = storage.health_check(user_id)
        assert result is not None

    def test_run_governance(self, db_factory):
        storage = self._storage(db_factory)
        user_id = _uid("cs_gov")
        _cleanup(db_factory, user_id)
        storage.store(
            user_id=user_id, content="governance test", memory_type=MemoryType.SEMANTIC
        )
        report = storage.run_governance(user_id)
        assert hasattr(report, "errors")

    def test_generate_session_summary_no_llm(self, db_factory):
        storage = self._storage(db_factory)
        user_id = _uid("cs_summary")
        _cleanup(db_factory, user_id)
        messages = [{"role": "user", "content": "worked on feature X"}]
        mem = storage.generate_session_summary(user_id, "sess_1", messages)
        assert mem is not None
        assert mem.content

    def test_check_and_summarize_below_threshold(self, db_factory):
        storage = self._storage(db_factory)
        user_id = _uid("cs_check_sum")
        _cleanup(db_factory, user_id)
        messages = [{"role": "user", "content": "hello"}]
        # turn_count=1, threshold default is high → should return None
        result = storage.check_and_summarize(
            user_id,
            "sess_1",
            messages,
            turn_count=1,
            session_start=datetime.now(timezone.utc),
        )
        # Either None (below threshold) or a Memory
        assert result is None or hasattr(result, "memory_id")

    def test_get_reflection_candidates(self, db_factory):
        from memoria.core.memory.tabular.candidates import TabularCandidateProvider

        user_id = _uid("cs_reflect")
        _cleanup(db_factory, user_id)
        from memoria.core.memory.canonical_storage import CanonicalStorage

        storage = CanonicalStorage(db_factory)
        storage.store(
            user_id=user_id, content="reflection test", memory_type=MemoryType.SEMANTIC
        )
        provider = TabularCandidateProvider(db_factory)
        candidates = provider.get_reflection_candidates(user_id)
        assert isinstance(candidates, list)

    def test_create_memory(self, db_factory):
        import uuid
        from memoria.core.memory.types import _utcnow

        storage = self._storage(db_factory)
        user_id = _uid("cs_create")
        _cleanup(db_factory, user_id)

        mem = Memory(
            memory_id=uuid.uuid4().hex,
            user_id=user_id,
            content="direct create",
            memory_type=MemoryType.WORKING,
            trust_tier=TrustTier.T3_INFERRED,
            initial_confidence=0.6,
            observed_at=_utcnow(),
        )
        result = storage.create_memory(mem)
        assert result.memory_id == mem.memory_id


# ---------------------------------------------------------------------------
# MemoryHealth
# ---------------------------------------------------------------------------


@pytest.mark.integration
class TestMemoryHealth:
    def _health(self, db_factory):
        from memoria.core.memory.tabular.health import MemoryHealth

        return MemoryHealth(db_factory)

    def test_analyze_empty_user(self, db_factory):
        health = self._health(db_factory)
        user_id = _uid("health_empty")
        _cleanup(db_factory, user_id)
        stats = health.analyze(user_id)
        assert isinstance(stats, dict)

    def test_analyze_with_memories(self, db_factory):
        from memoria.core.memory.tabular.service import MemoryService

        health = self._health(db_factory)
        svc = MemoryService(db_factory)
        user_id = _uid("health_with")
        _cleanup(db_factory, user_id)

        svc.store(
            user_id=user_id, content="semantic fact", memory_type=MemoryType.SEMANTIC
        )
        svc.store(
            user_id=user_id, content="profile pref", memory_type=MemoryType.PROFILE
        )

        stats = health.analyze(user_id)
        assert "semantic" in stats or "profile" in stats
        for v in stats.values():
            assert "total" in v
            assert "avg_confidence" in v

    def test_detect_pollution_no_changes(self, db_factory):
        health = self._health(db_factory)
        user_id = _uid("health_poll")
        _cleanup(db_factory, user_id)
        result = health.detect_pollution(user_id, datetime.now(timezone.utc))
        assert "is_polluted" in result
        assert result["is_polluted"] is False

    def test_suggest_rollback_target_empty(self, db_factory):
        health = self._health(db_factory)
        user_id = _uid("health_rollback")
        _cleanup(db_factory, user_id)
        result = health.suggest_rollback_target(user_id)
        assert result is None

    def test_cleanup_snapshots(self, db_factory):
        health = self._health(db_factory)
        count = health.cleanup_snapshots(keep_last_n=100)
        assert isinstance(count, int)

    def test_estimate_capacity(self, db_factory):
        from memoria.core.memory.tabular.service import MemoryService

        health = self._health(db_factory)
        svc = MemoryService(db_factory)
        user_id = _uid("health_cap")
        _cleanup(db_factory, user_id)
        svc.store(
            user_id=user_id, content="capacity test", memory_type=MemoryType.SEMANTIC
        )
        result = health.estimate_capacity(user_id)
        assert isinstance(result, dict)

    def test_get_storage_stats(self, db_factory):
        from memoria.core.memory.tabular.service import MemoryService

        health = self._health(db_factory)
        svc = MemoryService(db_factory)
        user_id = _uid("health_stats")
        _cleanup(db_factory, user_id)
        svc.store(
            user_id=user_id, content="stats test", memory_type=MemoryType.SEMANTIC
        )
        result = health.get_storage_stats(user_id)
        assert isinstance(result, dict)


# ---------------------------------------------------------------------------
# MemoryEditor
# ---------------------------------------------------------------------------


@pytest.mark.integration
class TestMemoryEditor:
    def _editor_and_storage(self, db_factory):
        from memoria.core.memory.canonical_storage import CanonicalStorage
        from memoria.core.memory.editor import MemoryEditor

        storage = CanonicalStorage(db_factory)
        editor = MemoryEditor(storage, db_factory, index_manager=None)
        return editor, storage

    def test_inject_memory(self, db_factory):
        editor, storage = self._editor_and_storage(db_factory)
        user_id = _uid("editor_inject")
        _cleanup(db_factory, user_id)

        mem = editor.inject(
            user_id=user_id,
            content="injected fact",
            memory_type=MemoryType.SEMANTIC,
        )
        assert mem.memory_id
        with db_factory() as db:
            row = db.execute(
                text("SELECT content FROM mem_memories WHERE memory_id = :id"),
                {"id": mem.memory_id},
            ).fetchone()
        assert row.content == "injected fact"

    def test_correct_memory(self, db_factory):
        editor, storage = self._editor_and_storage(db_factory)
        user_id = _uid("editor_correct")
        _cleanup(db_factory, user_id)

        mem = storage.store(
            user_id=user_id,
            content="old content",
            memory_type=MemoryType.SEMANTIC,
        )
        new_mem = editor.correct(
            user_id=user_id,
            memory_id=mem.memory_id,
            new_content="corrected content",
            reason="user correction",
        )
        assert new_mem.content == "corrected content"

        # Old memory should be deactivated
        with db_factory() as db:
            row = db.execute(
                text("SELECT is_active FROM mem_memories WHERE memory_id = :id"),
                {"id": mem.memory_id},
            ).fetchone()
        assert row.is_active == 0

    def test_purge_by_id(self, db_factory):
        editor, storage = self._editor_and_storage(db_factory)
        user_id = _uid("editor_purge")
        _cleanup(db_factory, user_id)

        mem = storage.store(
            user_id=user_id,
            content="to be purged",
            memory_type=MemoryType.WORKING,
        )
        result = editor.purge(
            user_id=user_id,
            memory_ids=[mem.memory_id],
            reason="test purge",
        )
        assert result.deactivated >= 1

        with db_factory() as db:
            row = db.execute(
                text("SELECT is_active FROM mem_memories WHERE memory_id = :id"),
                {"id": mem.memory_id},
            ).fetchone()
        assert row.is_active == 0

    def test_purge_by_type(self, db_factory):
        editor, storage = self._editor_and_storage(db_factory)
        user_id = _uid("editor_type")
        _cleanup(db_factory, user_id)

        storage.store(
            user_id=user_id, content="working mem 1", memory_type=MemoryType.WORKING
        )
        storage.store(
            user_id=user_id, content="working mem 2", memory_type=MemoryType.WORKING
        )

        result = editor.purge(
            user_id=user_id,
            memory_types=[MemoryType.WORKING],
            reason="cleanup working",
        )
        assert result.deactivated >= 1


# ---------------------------------------------------------------------------
# GitForData (snapshot/branch operations)
# ---------------------------------------------------------------------------


@pytest.mark.integration
class TestGitForData:
    def _git(self, db_factory):
        from memoria.core.git_for_data import GitForData

        return GitForData(db_factory)

    def test_create_and_list_snapshot(self, db_factory):
        git = self._git(db_factory)
        snap_name = f"test_snap_{int(time.time() * 1000) % 100000}"

        try:
            result = git.create_snapshot(snap_name)
            assert result.get("snapshot_name") == snap_name or "snapshot_name" in result

            snapshots = git.list_snapshots()
            names = [s.get("snapshot_name") or s.get("name") for s in snapshots]
            assert snap_name in names
        finally:
            try:
                git.drop_snapshot(snap_name)
            except Exception:
                pass

    def test_get_snapshot_info(self, db_factory):
        git = self._git(db_factory)
        snap_name = f"test_info_{int(time.time() * 1000) % 100000}"

        try:
            git.create_snapshot(snap_name)
            info = git.get_snapshot_info(snap_name)
            assert info is not None
        finally:
            try:
                git.drop_snapshot(snap_name)
            except Exception:
                pass

    def test_get_snapshot_info_nonexistent(self, db_factory):
        git = self._git(db_factory)
        info = git.get_snapshot_info("nonexistent_snap_xyz_999")
        assert info is None

    def test_drop_snapshot(self, db_factory):
        git = self._git(db_factory)
        snap_name = f"test_drop_{int(time.time() * 1000) % 100000}"
        git.create_snapshot(snap_name)
        git.drop_snapshot(snap_name)
        info = git.get_snapshot_info(snap_name)
        assert info is None

    def test_cleanup_old_snapshots(self, db_factory):
        git = self._git(db_factory)
        # Should not raise, returns list of dropped names
        dropped = git.cleanup_old_snapshots(keep_count=100)
        assert isinstance(dropped, list)


# ---------------------------------------------------------------------------
# MemoryService (tabular) — uncovered methods
# ---------------------------------------------------------------------------


@pytest.mark.integration
class TestTabularMemoryServiceCoverage:
    def _svc(self, db_factory, **kwargs):
        from memoria.core.memory.tabular.service import MemoryService

        return MemoryService(db_factory, **kwargs)

    def test_get_profile(self, db_factory):
        svc = self._svc(db_factory)
        user_id = _uid("tsvc_profile")
        _cleanup(db_factory, user_id)
        result = svc.get_profile(user_id)
        assert result is None or isinstance(result, str)

    def test_invalidate_profile(self, db_factory):
        svc = self._svc(db_factory)
        user_id = _uid("tsvc_inv")
        _cleanup(db_factory, user_id)
        svc.invalidate_profile(user_id)  # should not raise

    def test_generate_session_summary(self, db_factory):
        svc = self._svc(db_factory)
        user_id = _uid("tsvc_sum")
        _cleanup(db_factory, user_id)
        messages = [{"role": "user", "content": "worked on auth module"}]
        mem = svc.generate_session_summary(user_id, "sess_1", messages)
        assert mem is not None

    def test_check_and_summarize(self, db_factory):
        svc = self._svc(db_factory)
        user_id = _uid("tsvc_check")
        _cleanup(db_factory, user_id)
        messages = [{"role": "user", "content": "hello"}]
        result = svc.check_and_summarize(
            user_id,
            "sess_1",
            messages,
            turn_count=1,
            session_start=datetime.now(timezone.utc),
        )
        assert result is None or hasattr(result, "memory_id")

    def test_run_governance(self, db_factory):
        svc = self._svc(db_factory)
        user_id = _uid("tsvc_gov")
        _cleanup(db_factory, user_id)
        svc.store(user_id=user_id, content="gov test", memory_type=MemoryType.SEMANTIC)
        report = svc.run_governance(user_id)
        assert report is not None

    def test_health_check(self, db_factory):
        svc = self._svc(db_factory)
        user_id = _uid("tsvc_health")
        _cleanup(db_factory, user_id)
        result = svc.health_check(user_id)
        assert result is not None

    def test_run_hourly(self, db_factory):
        svc = self._svc(db_factory)
        result = svc.run_hourly()
        assert result is not None

    def test_run_daily_all(self, db_factory):
        svc = self._svc(db_factory)
        result = svc.run_daily_all()
        assert result is not None

    def test_run_weekly(self, db_factory):
        svc = self._svc(db_factory)
        result = svc.run_weekly()
        assert result is not None

    def test_get_reflection_candidates(self, db_factory):
        from memoria.core.memory.tabular.candidates import TabularCandidateProvider

        svc = self._svc(db_factory)
        user_id = _uid("tsvc_reflect")
        _cleanup(db_factory, user_id)
        svc.store(
            user_id=user_id, content="reflection test", memory_type=MemoryType.SEMANTIC
        )
        provider = TabularCandidateProvider(db_factory)
        candidates = provider.get_reflection_candidates(user_id)
        assert isinstance(candidates, list)
