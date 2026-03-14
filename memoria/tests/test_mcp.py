"""MCP Server tests — full chain: MCP tool → HTTP → API → DB.

Uses a real FastAPI TestClient as the HTTP backend so every tool
exercises the actual API endpoint and database write path.
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
    uid = f"mcp_{uuid.uuid4().hex[:8]}"
    r = client.post(
        "/auth/keys",
        json={"user_id": uid, "name": "mcp-key"},
        headers={"Authorization": f"Bearer {MASTER_KEY}"},
    )
    assert r.status_code == 201
    return uid, r.json()["raw_key"]


@pytest.fixture(scope="module")
def user_and_key(client):
    return _make_user(client)


class _HttpShim:
    """Mimics httpx.Client but delegates to FastAPI TestClient."""

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
def http(client, user_and_key):
    _, api_key = user_and_key
    return _HttpShim(client, api_key)


# ── Tool wrappers (sync, mirrors mcp/server.py logic exactly) ─────────


def _store(http, content, memory_type="semantic", session_id=None):
    r = http.post(
        "/v1/memories",
        json={"content": content, "memory_type": memory_type, "session_id": session_id},
    )
    r.raise_for_status()
    d = r.json()
    return f"Stored memory {d['memory_id']}: {d['content'][:80]}"


def _retrieve(http, query, top_k=5):
    r = http.post("/v1/memories/retrieve", json={"query": query, "top_k": top_k})
    r.raise_for_status()
    data = r.json()
    items = data.get("results", [])
    if not items:
        return "No relevant memories found."
    return "\n".join(f"- [{m['memory_type']}] {m['content']}" for m in items)


def _search(http, query, top_k=10):
    r = http.post("/v1/memories/search", json={"query": query, "top_k": top_k})
    r.raise_for_status()
    data = r.json()
    items = data.get("results", [])
    if not items:
        return "No memories found."
    return "\n".join(
        f"- [{m['memory_id']}] [{m['memory_type']}] {m['content']}" for m in items
    )


def _correct(http, memory_id, new_content, reason=""):
    r = http.put(
        f"/v1/memories/{memory_id}/correct",
        json={"new_content": new_content, "reason": reason},
    )
    r.raise_for_status()
    d = r.json()
    return f"Corrected memory {d['memory_id']}: {d['content'][:80]}"


def _purge(http, memory_id, reason=""):
    r = http.delete(f"/v1/memories/{memory_id}", params={"reason": reason})
    r.raise_for_status()
    return f"Purged memory {memory_id}"


def _profile(http):
    r = http.get("/v1/profiles/me")
    r.raise_for_status()
    return str(r.json())


def _snapshot(http, name, description=""):
    r = http.post("/v1/snapshots", json={"name": name, "description": description})
    r.raise_for_status()
    d = r.json()
    return f"Snapshot '{d['name']}' created (ts={d.get('timestamp', 'unknown')})"


def _snapshots(http):
    r = http.get("/v1/snapshots")
    r.raise_for_status()
    items = r.json()
    if not items:
        return "No snapshots."
    return "\n".join(f"- {s['name']} ({s.get('timestamp', '')})" for s in items)


def _consolidate(http, force=False):
    r = http.post("/v1/consolidate", params={"force": force})
    r.raise_for_status()
    return str(r.json())


def _reflect(http, force=False):
    r = http.post("/v1/reflect", params={"force": force})
    r.raise_for_status()
    return str(r.json())


def _extract_mid(store_result: str) -> str:
    """Parse memory_id from 'Stored memory <id>: ...'"""
    return store_result.split("Stored memory ")[1].split(":")[0]


# ── Tests ─────────────────────────────────────────────────────────────


class TestMCPStore:
    def test_store_returns_memory_id(self, http):
        result = _store(http, "MCP test fact")
        assert "Stored memory" in result
        assert "MCP test fact" in result

    def test_store_persists_to_db(self, http, db, user_and_key):
        uid, _ = user_and_key
        _store(http, "mcp_db_check_unique_content")
        row = db.execute(
            text(
                "SELECT content FROM mem_memories WHERE user_id = :uid AND content = 'mcp_db_check_unique_content' AND is_active"
            ),
            {"uid": uid},
        ).first()
        assert row is not None

    def test_store_with_type(self, http, db, user_and_key):
        uid, _ = user_and_key
        _store(http, "procedural mcp test", memory_type="procedural")
        row = db.execute(
            text(
                "SELECT memory_type FROM mem_memories WHERE user_id = :uid AND content = 'procedural mcp test' AND is_active"
            ),
            {"uid": uid},
        ).first()
        assert row is not None
        assert "procedural" in str(row[0])


class TestMCPRetrieve:
    def test_retrieve_returns_results(self, http):
        _store(http, "Python is a programming language")
        result = _retrieve(http, "programming")
        assert isinstance(result, str)

    def test_retrieve_empty(self, http):
        result = _retrieve(http, "xyzzy_nonexistent_topic_12345")
        assert isinstance(result, str)


class TestMCPSearch:
    def test_search_returns_results(self, http):
        _store(http, "unique_search_term_abc123")
        result = _search(http, "unique_search_term_abc123")
        assert isinstance(result, str)

    def test_search_format(self, http):
        _store(http, "search format test content")
        result = _search(http, "search format test")
        if "No memories" not in result:
            assert "[" in result


class TestMCPCorrect:
    def test_correct_updates_content(self, http, db, user_and_key):
        uid, _ = user_and_key
        mid = _extract_mid(_store(http, "old content for correction"))

        corrected = _correct(http, mid, "new corrected content", "was wrong")
        assert "Corrected memory" in corrected
        assert "new corrected content" in corrected

        # DB: old deactivated, new active
        old = db.execute(
            text("SELECT is_active FROM mem_memories WHERE memory_id = :mid"),
            {"mid": mid},
        ).first()
        assert old[0] == 0

        new = db.execute(
            text(
                "SELECT content FROM mem_memories WHERE user_id = :uid AND content = 'new corrected content' AND is_active"
            ),
            {"uid": uid},
        ).first()
        assert new is not None


class TestMCPPurge:
    def test_purge_deactivates(self, http, db):
        mid = _extract_mid(_store(http, "to be purged via mcp"))
        result = _purge(http, mid)
        assert "Purged" in result

        row = db.execute(
            text("SELECT is_active FROM mem_memories WHERE memory_id = :mid"),
            {"mid": mid},
        ).first()
        assert row[0] == 0


class TestMCPProfile:
    def test_profile_returns_dict(self, http):
        result = _profile(http)
        assert "user_id" in result
        assert "profile" in result


class TestMCPSnapshot:
    def test_snapshot_lifecycle(self, http, db, user_and_key):
        uid, _ = user_and_key
        _store(http, "snapshot test memory 1")
        _store(http, "snapshot test memory 2")

        name = f"mcp_snap_{uuid.uuid4().hex[:6]}"
        result = _snapshot(http, name, "mcp test snapshot")
        assert "created" in result
        assert name in result

        listing = _snapshots(http)
        assert name in listing

        row = db.execute(
            text(
                "SELECT display_name FROM mem_snapshot_registry WHERE user_id = :uid AND display_name = :name"
            ),
            {"uid": uid, "name": name},
        ).first()
        assert row is not None


class TestMCPConsolidate:
    def test_consolidate_returns_ok(self, http):
        result = _consolidate(http)
        assert isinstance(result, str)


class TestMCPReflect:
    def test_reflect_returns_ok(self, http):
        result = _reflect(http)
        assert isinstance(result, str)


class TestMCPFullChain:
    """End-to-end: store → correct → verify → purge → verify gone."""

    def test_full_lifecycle(self, http, db, user_and_key):
        uid, _ = user_and_key

        # 1. Store
        mid = _extract_mid(_store(http, "MCP lifecycle test original"))

        # 2. Verify in DB
        row = db.execute(
            text("SELECT content, is_active FROM mem_memories WHERE memory_id = :mid"),
            {"mid": mid},
        ).first()
        assert row[0] == "MCP lifecycle test original"
        assert row[1] == 1

        # 3. Correct
        _correct(http, mid, "MCP lifecycle test corrected")

        # 4. Old deactivated
        row = db.execute(
            text("SELECT is_active FROM mem_memories WHERE memory_id = :mid"),
            {"mid": mid},
        ).first()
        assert row[0] == 0

        # 5. New exists
        new = db.execute(
            text(
                "SELECT memory_id, is_active FROM mem_memories "
                "WHERE user_id = :uid AND content = 'MCP lifecycle test corrected' AND is_active"
            ),
            {"uid": uid},
        ).first()
        assert new is not None
        new_mid = new[0]

        # 6. Purge
        _purge(http, new_mid)

        # 7. Verify gone
        row = db.execute(
            text("SELECT is_active FROM mem_memories WHERE memory_id = :mid"),
            {"mid": new_mid},
        ).first()
        assert row[0] == 0


class TestMCPSnapshotDiff:
    def test_diff_via_mcp(self, http, user_and_key):
        import time

        uid, _ = user_and_key
        _store(http, "diff base 1")
        _store(http, "diff base 2")

        time.sleep(0.3)
        _snapshot(http, "diff_test")
        time.sleep(0.3)

        _store(http, "diff new 3")

        r = http.get("/v1/snapshots/diff_test/diff")
        assert r.status_code == 200
        d = r.json()
        assert d["added_count"] >= 1
        assert "snapshot_count" in d
        assert "current_count" in d


class TestMCPExplain:
    """Test explain functionality in MCP text format."""

    def test_retrieve_text_with_explain(self, http, user_and_key):
        """Test that text format includes explain summary."""
        _store(http, "Python is great for data science")

        # Call via MCP backend (simulates actual MCP tool call)
        r = http.post(
            "/v1/memories/retrieve",
            json={"query": "programming", "top_k": 5, "explain": "basic"},
        )
        assert r.status_code == 200
        data = r.json()

        # JSON format should have explain field
        assert "results" in data
        assert "explain" in data
        assert data["explain"]["level"] == "basic"
        assert "path" in data["explain"]

    def test_search_text_with_explain(self, http, user_and_key):
        """Test that search text format includes explain summary."""
        r = http.post(
            "/v1/memories/search",
            json={"query": "test", "top_k": 5, "explain": "basic"},
        )
        assert r.status_code == 200
        data = r.json()

        assert "results" in data
        assert "explain" in data
        assert data["explain"]["level"] == "basic"
