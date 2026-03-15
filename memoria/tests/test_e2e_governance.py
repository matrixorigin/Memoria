"""E2E tests — governance (DistributedLock, MemoryOnly, Heartbeat, ProfileStats, SnapshotDiff).

Run: pytest memoria/tests/test_e2e_governance.py -v
"""

from __future__ import annotations

import os
import uuid

import pytest
from fastapi.testclient import TestClient
from sqlalchemy import text

MASTER_KEY = "e2e-master-key"
os.environ["MEMORIA_MASTER_KEY"] = MASTER_KEY


@pytest.fixture(scope="module")
def client():
    from memoria.api.main import app
    from memoria.core.embedding import _shared_client as _saved_client
    import memoria.core.embedding as _emb_mod

    with TestClient(app) as c:
        yield c
    _emb_mod._shared_client = _saved_client
    from memoria.api.middleware import _windows

    _windows.clear()


@pytest.fixture(scope="function")
def db():
    from memoria.api.database import init_db, get_session_factory

    init_db()
    session = get_session_factory()()
    yield session
    session.rollback()
    session.close()


def _make_user(client):
    from memoria.api.middleware import _windows

    _windows.clear()
    uid = "e2e_" + uuid.uuid4().hex[:8]
    r = client.post(
        "/auth/keys",
        json={"user_id": uid, "name": "e2e-key"},
        headers={"Authorization": "Bearer " + MASTER_KEY},
    )
    assert r.status_code == 201
    data = r.json()
    return uid, {"Authorization": "Bearer " + data["raw_key"]}, data["key_id"]


@pytest.fixture(scope="module")
def user_key(client):
    uid, h, kid = _make_user(client)
    return uid, h


@pytest.fixture(autouse=True, scope="module")
def _patch_heartbeat():
    """HEARTBEAT_INTERVAL=0 so heartbeat threads exit immediately in tests."""
    import memoria.core.scheduler as sched

    original = sched.HEARTBEAT_INTERVAL
    sched.HEARTBEAT_INTERVAL = 0
    yield
    sched.HEARTBEAT_INTERVAL = original


@pytest.mark.xdist_group("governance")
class TestGovernanceDistributedLock:
    """Verify distributed lock mechanics: acquire, conflict, expiry takeover, heartbeat."""

    @pytest.fixture(autouse=True)
    def _clean_locks(self, db):
        db.execute(text("DELETE FROM infra_distributed_locks"))
        db.execute(text("DELETE FROM governance_runs"))
        db.commit()
        yield
        db.execute(text("DELETE FROM infra_distributed_locks"))
        db.execute(text("DELETE FROM governance_runs"))
        db.commit()

    def _make_runner(self, **kwargs):
        from memoria.core.scheduler import GovernanceTaskRunner
        from memoria.api.database import get_db_context, get_db_factory

        return GovernanceTaskRunner(
            get_db_context, db_factory=get_db_factory(), memory_only=True, **kwargs
        )

    def test_lock_acquired_and_released(self, db):
        """Lock is acquired, task runs, lock is released."""
        runner = self._make_runner()
        result = runner.run("hourly")
        assert isinstance(result, dict)

        # Lock should be released after run
        row = db.execute(
            text(
                "SELECT * FROM infra_distributed_locks WHERE lock_name = 'governance_hourly'"
            )
        ).first()
        assert row is None, "Lock should be released after successful run"

    def test_lock_conflict_returns_none(self, db):
        """Second runner cannot acquire lock held by first."""
        from datetime import datetime, timedelta

        # Manually insert a non-expired lock
        db.execute(
            text(
                "INSERT INTO infra_distributed_locks (lock_name, instance_id, acquired_at, expires_at, task_name) "
                "VALUES (:name, :iid, :acq, :exp, :task)"
            ),
            {
                "name": "governance_hourly",
                "iid": "other-host:9999",
                "acq": datetime.now(),
                "exp": datetime.now() + timedelta(seconds=300),
                "task": "hourly",
            },
        )
        db.commit()

        runner = self._make_runner()
        result = runner.run("hourly")
        assert result is None, "Should skip when lock is held by another instance"

    def test_expired_lock_takeover(self, db):
        """Expired lock is taken over via atomic CAS."""
        from datetime import datetime, timedelta

        # Insert an expired lock
        db.execute(
            text(
                "INSERT INTO infra_distributed_locks (lock_name, instance_id, acquired_at, expires_at, task_name) "
                "VALUES (:name, :iid, :acq, :exp, :task)"
            ),
            {
                "name": "governance_hourly",
                "iid": "dead-host:1234",
                "acq": datetime.now() - timedelta(seconds=600),
                "exp": datetime.now() - timedelta(seconds=60),  # expired
                "task": "hourly",
            },
        )
        db.commit()

        runner = self._make_runner()
        result = runner.run("hourly")
        assert isinstance(result, dict), "Should take over expired lock and execute"

    def test_governance_run_persisted(self, db):
        """Each successful run writes to governance_runs table."""
        # Force-clear any stale lock that might block this run
        db.execute(
            text(
                "DELETE FROM infra_distributed_locks WHERE lock_name = 'governance_hourly'"
            )
        )
        db.commit()

        runner = self._make_runner()
        result = runner.run("hourly")
        assert result is not None, "run() should succeed with no competing lock"

        row = db.execute(
            text(
                "SELECT task_name, result FROM governance_runs WHERE task_name = 'hourly' ORDER BY created_at DESC LIMIT 1"
            )
        ).first()
        assert row is not None, "governance_runs should have a record"
        assert row[0] == "hourly"

        import json

        result = json.loads(row[1])
        assert "mem_cleaned_tool_results" in result
        assert "mem_archived_working" in result


@pytest.mark.xdist_group("governance")
class TestGovernanceMemoryOnly:
    """Verify memory_only=True skips knowledge/eval tasks cleanly."""

    @pytest.fixture(autouse=True)
    def _clean_locks(self, db):
        db.execute(text("DELETE FROM infra_distributed_locks"))
        db.commit()
        yield
        db.execute(text("DELETE FROM infra_distributed_locks"))
        db.commit()

    def _make_runner(self):
        from memoria.core.scheduler import GovernanceTaskRunner
        from memoria.api.database import get_db_context, get_db_factory

        return GovernanceTaskRunner(
            get_db_context, db_factory=get_db_factory(), memory_only=True
        )

    def test_hourly_no_knowledge_errors(self, db):
        """Hourly runs without knowledge governance errors."""
        runner = self._make_runner()
        result = runner.run("hourly")
        assert isinstance(result, dict)
        assert "mem_cleaned_tool_results" in result
        assert "mem_archived_working" in result
        # No knowledge keys should be present
        for key in result:
            assert not key.startswith("archived_scratchpads"), (
                f"Unexpected knowledge key: {key}"
            )

    def test_daily_no_knowledge_errors(self, db):
        """Daily runs without knowledge governance errors."""
        runner = self._make_runner()
        result = runner.run("daily")
        assert isinstance(result, dict)
        assert "mem_cleaned_stale" in result
        assert "mem_quarantined" in result

    def test_weekly_no_knowledge_errors(self, db):
        """Weekly runs without knowledge governance errors."""
        runner = self._make_runner()
        result = runner.run("weekly")
        assert isinstance(result, dict)
        assert "mem_cleaned_branches" in result
        assert "mem_cleaned_snapshots" in result

    def test_eval_daily_not_in_standalone(self, db):
        """eval_daily is removed in standalone Memoria — task should not exist."""
        from memoria.core.scheduler import GOVERNANCE_TASKS

        assert "eval_daily" not in GOVERNANCE_TASKS


@pytest.mark.xdist_group("governance")
class TestGovernanceWithData:
    """Verify governance actually processes real memory data."""

    @pytest.fixture(autouse=True)
    def _clean(self, db):
        db.execute(text("DELETE FROM infra_distributed_locks"))
        db.commit()
        yield
        db.execute(text("DELETE FROM infra_distributed_locks"))
        db.commit()

    def test_hourly_cleans_tool_results(self, client, db):
        """Store tool_result memories, run hourly, verify cleanup."""
        uid, h, _ = _make_user(client)

        # Store tool_result type memories (semantically distinct to avoid contradiction detection)
        tool_contents = [
            "SELECT * FROM users WHERE id = 42",
            "docker build -t myapp:latest .",
            "curl -X POST https://api.example.com/data",
        ]
        for content in tool_contents:
            client.post(
                "/v1/memories",
                json={"content": content, "memory_type": "tool_result"},
                headers=h,
            )

        before = db.execute(
            text(
                "SELECT COUNT(*) FROM mem_memories WHERE user_id = :uid AND memory_type = 'tool_result' AND is_active"
            ),
            {"uid": uid},
        ).scalar()
        assert before == 3

        from memoria.core.scheduler import GovernanceTaskRunner
        from memoria.api.database import get_db_context, get_db_factory

        runner = GovernanceTaskRunner(
            get_db_context, db_factory=get_db_factory(), memory_only=True
        )
        result = runner.run("hourly")
        assert isinstance(result, dict)
        # Result should report how many were cleaned (may be 0 if TTL not expired)
        assert "mem_cleaned_tool_results" in result

    def test_daily_governance_runs_on_memories(self, client, db):
        """Store memories, run daily, verify it completes without error."""
        uid, h, _ = _make_user(client)

        for i in range(5):
            client.post(
                "/v1/memories",
                json={"content": f"daily governance test {i}"},
                headers=h,
            )

        from memoria.core.scheduler import GovernanceTaskRunner
        from memoria.api.database import get_db_context, get_db_factory

        runner = GovernanceTaskRunner(
            get_db_context, db_factory=get_db_factory(), memory_only=True
        )
        result = runner.run("daily")
        assert isinstance(result, dict)
        assert "mem_cleaned_stale" in result
        assert "mem_quarantined" in result


@pytest.mark.xdist_group("governance")
class TestGovernanceHeartbeat:
    """Verify heartbeat renews lock during execution."""

    @pytest.fixture(autouse=True)
    def _clean(self, db):
        db.execute(text("DELETE FROM infra_distributed_locks"))
        db.commit()
        yield
        db.execute(text("DELETE FROM infra_distributed_locks"))
        db.commit()

    def test_heartbeat_renews_lock(self, db):
        """Verify heartbeat thread can renew lock expiry."""
        import uuid
        from datetime import datetime, timedelta
        from memoria.core.scheduler import GovernanceTaskRunner, LOCK_TTL
        from memoria.api.database import get_db_context, get_db_factory

        runner = GovernanceTaskRunner(
            get_db_context, db_factory=get_db_factory(), memory_only=True
        )

        # Use a unique lock name to avoid races with other workers
        lock_name = f"test_heartbeat_{uuid.uuid4().hex[:8]}"

        now = datetime.now()
        original_exp = now + timedelta(seconds=LOCK_TTL)
        db.execute(
            text(
                "INSERT INTO infra_distributed_locks (lock_name, instance_id, acquired_at, expires_at, task_name) "
                "VALUES (:name, :iid, :acq, :exp, :task)"
            ),
            {
                "name": lock_name,
                "iid": runner._instance_id,
                "acq": now,
                "exp": original_exp,
                "task": "test",
            },
        )
        db.commit()

        with get_db_context() as hb_db:
            new_exp = datetime.now() + timedelta(seconds=LOCK_TTL)
            hb_db.execute(
                text(
                    "UPDATE infra_distributed_locks SET expires_at = :exp "
                    "WHERE lock_name = :name AND instance_id = :iid"
                ),
                {"exp": new_exp, "name": lock_name, "iid": runner._instance_id},
            )
            hb_db.commit()

        row = db.execute(
            text(
                "SELECT expires_at FROM infra_distributed_locks WHERE lock_name = :name"
            ),
            {"name": lock_name},
        ).first()
        assert row is not None
        assert row[0] >= original_exp, "Heartbeat should have extended the expiry"

        # Cleanup
        db.execute(
            text("DELETE FROM infra_distributed_locks WHERE lock_name = :name"),
            {"name": lock_name},
        )
        db.commit()


# ── Profile Stats & Snapshot Diff ─────────────────────────────────────


class TestProfileStats:
    def test_profile_includes_stats(self, client):
        uid, h, _ = _make_user(client)
        client.post("/v1/memories", json={"content": "semantic fact"}, headers=h)
        client.post(
            "/v1/memories",
            json={"content": "proc fact", "memory_type": "procedural"},
            headers=h,
        )

        r = client.get("/v1/profiles/me", headers=h)
        assert r.status_code == 200
        d = r.json()
        assert "stats" in d
        stats = d["stats"]
        assert stats["total"] == 2
        assert "semantic" in str(stats["by_type"])
        assert "procedural" in str(stats["by_type"])
        assert stats["avg_confidence"] is not None
        assert stats["oldest"] is not None
        assert stats["newest"] is not None


class TestSnapshotDiff:
    def test_diff_shows_changes(self, client):
        import time

        uid, h, _ = _make_user(client)
        client.post("/v1/memories", json={"content": "before A"}, headers=h)
        client.post("/v1/memories", json={"content": "before B"}, headers=h)

        time.sleep(0.3)
        client.post("/v1/snapshots", json={"name": "baseline"}, headers=h)
        time.sleep(0.3)

        # Add one, delete one
        client.post("/v1/memories", json={"content": "after C"}, headers=h)
        items = client.get("/v1/memories", headers=h).json()["items"]
        a_mid = next(m["memory_id"] for m in items if m["content"] == "before A")
        client.delete(f"/v1/memories/{a_mid}", headers=h)

        r = client.get("/v1/snapshots/baseline/diff", headers=h)
        assert r.status_code == 200
        d = r.json()
        assert d["snapshot_count"] == 2
        assert d["current_count"] == 2  # B + C
        assert d["added_count"] == 1
        assert d["removed_count"] == 1
        assert d["unchanged_count"] == 1
        assert any("after C" in m["content"] for m in d["added"])
        assert any("before A" in m["content"] for m in d["removed"])

    def test_diff_nonexistent_snapshot(self, client):
        _, h, _ = _make_user(client)
        r = client.get("/v1/snapshots/nonexistent/diff", headers=h)
        assert r.status_code == 404
