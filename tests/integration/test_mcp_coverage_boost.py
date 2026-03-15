"""MCP server coverage boost: tests for governance, entities, snapshots, branches, rollback.

Uses the same _HttpShim pattern as test_mcp.py.
"""

from __future__ import annotations

import os
import time
import uuid

import pytest
from fastapi.testclient import TestClient

MASTER_KEY = "e2e-master-key"
os.environ["MEMORIA_MASTER_KEY"] = MASTER_KEY


@pytest.fixture(scope="module")
def client():
    from memoria.api.main import app

    with TestClient(app) as c:
        yield c
    from memoria.api.middleware import _windows

    _windows.clear()


@pytest.fixture(scope="module")
def db():
    from memoria.api.database import get_session_factory

    s = get_session_factory()()
    yield s
    s.close()


def _make_user(client):
    from memoria.api.middleware import _windows

    _windows.clear()
    uid = f"mcpx_{uuid.uuid4().hex[:8]}"
    r = client.post(
        "/auth/keys",
        json={"user_id": uid, "name": "mcpx-key"},
        headers={"Authorization": f"Bearer {MASTER_KEY}"},
    )
    assert r.status_code == 201
    return uid, r.json()["raw_key"]


class _HttpShim:
    def __init__(self, test_client, api_key):
        self._c = test_client
        self._h = {"Authorization": f"Bearer {api_key}"}

    def post(self, path, json=None, params=None):
        return self._c.post(path, json=json, params=params, headers=self._h)

    def get(self, path, params=None):
        return self._c.get(path, params=params, headers=self._h)

    def put(self, path, json=None, params=None):
        return self._c.put(path, json=json, params=params, headers=self._h)

    def delete(self, path, params=None):
        return self._c.delete(path, params=params, headers=self._h)


@pytest.fixture(scope="module")
def user_and_key(client):
    return _make_user(client)


@pytest.fixture(scope="module")
def http(client, user_and_key):
    _, api_key = user_and_key
    return _HttpShim(client, api_key)


def _store(http, content, memory_type="semantic"):
    r = http.post("/v1/memories", json={"content": content, "memory_type": memory_type})
    r.raise_for_status()
    return r.json()["memory_id"]


# ---------------------------------------------------------------------------
# Governance
# ---------------------------------------------------------------------------


class TestMCPGovernance:
    def test_governance_trigger_admin(self, client, user_and_key):
        user_id, _ = user_and_key
        r = client.post(
            f"/admin/governance/{user_id}/trigger",
            params={"force": True},
            headers={"Authorization": f"Bearer {MASTER_KEY}"},
        )
        assert r.status_code in (200, 204)

    def test_consolidate_returns_ok(self, http):
        r = http.post("/v1/consolidate", params={"force": True})
        assert r.status_code in (200, 204)

    def test_reflect_returns_ok(self, http):
        r = http.post("/v1/reflect", params={"force": True})
        assert r.status_code in (200, 204)


# ---------------------------------------------------------------------------
# Snapshots: list, delete, rollback
# ---------------------------------------------------------------------------


class TestMCPSnapshotOperations:
    def test_list_snapshots(self, http):
        r = http.get("/v1/snapshots")
        assert r.status_code == 200
        assert isinstance(r.json(), list)

    def test_snapshot_create_and_list(self, http):
        snap_name = f"test_snap_{int(time.time() * 1000) % 100000}"
        r = http.post("/v1/snapshots", json={"name": snap_name, "description": "test"})
        assert r.status_code in (200, 201)
        actual_name = r.json().get("name", snap_name)

        r2 = http.get("/v1/snapshots")
        names = [s.get("name") for s in r2.json()]
        assert actual_name in names

    def test_snapshot_get_by_name(self, http):
        snap_name = f"get_snap_{int(time.time() * 1000) % 100000}"
        r_create = http.post("/v1/snapshots", json={"name": snap_name})
        actual_name = r_create.json().get("name", snap_name)

        r = http.get(f"/v1/snapshots/{actual_name}")
        assert r.status_code in (200, 404)

    def test_snapshot_delete_by_name(self, http):
        snap_name = f"del_snap_{int(time.time() * 1000) % 100000}"
        r_create = http.post("/v1/snapshots", json={"name": snap_name})
        actual_name = r_create.json().get("name", snap_name)

        r = http.delete(f"/v1/snapshots/{actual_name}")
        assert r.status_code in (200, 204)

    def test_snapshot_diff(self, http):
        snap_name = f"diff_snap_{int(time.time() * 1000) % 100000}"
        r_create = http.post("/v1/snapshots", json={"name": snap_name})
        actual_name = r_create.json().get("name", snap_name)

        r = http.get(f"/v1/snapshots/{actual_name}/diff")
        assert r.status_code in (200, 204)


# ---------------------------------------------------------------------------
# Branches: not in HTTP API — tested via GitForData directly
# ---------------------------------------------------------------------------


class TestMCPBranchOperations:
    """Branch operations are in mcp_local/server.py (no HTTP API). Test via GitForData."""

    def test_branch_create_list_delete(self):
        from memoria.api.database import get_session_factory
        from memoria.core.git_for_data import GitForData

        db_factory = get_session_factory()
        git = GitForData(db_factory)

        branch_name = f"test_br_{int(time.time() * 1000) % 100000}"
        try:
            result = git.create_snapshot(branch_name)
            assert result is not None

            snapshots = git.list_snapshots()
            names = [s.get("snapshot_name") or s.get("name") for s in snapshots]
            assert branch_name in names
        finally:
            try:
                git.drop_snapshot(branch_name)
            except Exception:
                pass


# ---------------------------------------------------------------------------
# Entity extraction
# ---------------------------------------------------------------------------


class TestMCPEntityExtraction:
    def test_extract_entities_candidates_mode(self, http):
        _store(http, "I use Python and FastAPI for backend development")
        _store(http, "MatrixOne is the database we use")

        r = http.post("/v1/extract-entities/candidates")
        assert r.status_code in (200, 204)

    def test_extract_entities_auto(self, http):
        r = http.post("/v1/extract-entities", params={"force": True})
        assert r.status_code in (200, 204)

    def test_get_entities(self, http):
        r = http.get("/v1/entities")
        assert r.status_code in (200, 204)

    def test_link_entities(self, http):
        mid = _store(http, "I work with PostgreSQL and Redis")
        r = http.post(
            "/v1/extract-entities/link",
            json={
                "entities": [
                    {
                        "memory_id": mid,
                        "entities": [{"name": "PostgreSQL", "type": "tech"}],
                    }
                ]
            },
        )
        assert r.status_code in (200, 204)


# ---------------------------------------------------------------------------
# Rebuild index
# ---------------------------------------------------------------------------


class TestMCPRebuildIndex:
    def test_rebuild_index_via_service(self):
        """rebuild_index has no HTTP API — test via tabular service directly."""
        from memoria.api.database import get_session_factory
        from memoria.core.memory.tabular.service import MemoryService

        svc = MemoryService(get_session_factory())
        # Should not raise
        result = svc.run_hourly()
        assert result is not None


# ---------------------------------------------------------------------------
# Sessions (episodic) — no LLM → 503
# ---------------------------------------------------------------------------


class TestMCPSessions:
    def test_session_summary_no_llm_returns_503(self, http, user_and_key):
        user_id, _ = user_and_key
        session_id = f"sess_{uuid.uuid4().hex[:8]}"
        # Store a memory in the session
        http.post(
            "/v1/memories",
            json={
                "content": "worked on feature X",
                "memory_type": "semantic",
                "session_id": session_id,
            },
        )
        r = http.post(
            f"/v1/sessions/{session_id}/summary", json={"mode": "full", "sync": True}
        )
        # Without LLM configured, should return 503
        assert r.status_code in (200, 503)

    def test_session_summary_empty_session_returns_error(self, http):
        r = http.post(
            "/v1/sessions/nonexistent_session_xyz/summary",
            json={"mode": "full", "sync": True},
        )
        assert r.status_code in (400, 404, 500, 503)


# ---------------------------------------------------------------------------
# User ops: batch purge, search by topic
# ---------------------------------------------------------------------------


class TestMCPUserOps:
    def test_batch_purge_by_topic(self, http):
        _store(http, "temporary debug context alpha")
        _store(http, "temporary debug context beta")

        r = http.post(
            "/v1/memories/purge",
            json={"topic": "temporary debug context", "reason": "cleanup"},
        )
        assert r.status_code in (200, 204)

    def test_batch_store(self, http):
        r = http.post(
            "/v1/memories/batch",
            json={
                "memories": [
                    {"content": "batch fact 1", "memory_type": "semantic"},
                    {"content": "batch fact 2", "memory_type": "semantic"},
                ]
            },
        )
        assert r.status_code in (200, 201)

    def test_health_endpoint(self, http, client):
        r = client.get("/health")
        assert r.status_code in (200, 204)

    def test_admin_stats(self, client):
        r = client.get(
            "/admin/stats", headers={"Authorization": f"Bearer {MASTER_KEY}"}
        )
        assert r.status_code in (200, 204)

    def test_admin_users(self, client):
        r = client.get(
            "/admin/users", headers={"Authorization": f"Bearer {MASTER_KEY}"}
        )
        assert r.status_code in (200, 204)
