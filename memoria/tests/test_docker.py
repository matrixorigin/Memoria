"""Memoria — Docker integration tests.

Runs against a live service (default: http://localhost:8100).
Requires the service to be running: cd memoria && docker compose up -d

Usage:
    pytest memoria/tests/test_docker.py -v
    MEMORIA_URL=http://myserver:8100 MEMORIA_MASTER_KEY=mykey pytest memoria/tests/test_docker.py -v
"""

from __future__ import annotations

import os
import time
import uuid

import httpx
import pytest

BASE = os.environ.get("MEMORIA_URL", "http://localhost:8100")
MASTER = os.environ.get("MEMORIA_MASTER_KEY", "test-master-key-for-docker-compose")
CLIENT = httpx.Client(base_url=BASE, timeout=30, trust_env=False)


def pytest_configure(config):
    config.addinivalue_line("markers", "docker: requires running Docker container")


def _check_service():
    try:
        CLIENT.get("/health", timeout=2)
    except Exception:
        pytest.skip(
            "Memoria service not running — start with: cd memoria && docker compose up -d"
        )


@pytest.fixture(autouse=True, scope="session")
def require_service():
    _check_service()


def _make_user() -> tuple[str, dict]:
    uid = f"docker_{uuid.uuid4().hex[:8]}"
    for attempt in range(3):
        r = CLIENT.post(
            "/auth/keys",
            json={"user_id": uid, "name": "docker-test"},
            headers={"Authorization": f"Bearer {MASTER}"},
        )
        if r.status_code == 429:
            time.sleep(10)
            continue
        assert r.status_code == 201, r.text
        return uid, {"Authorization": f"Bearer {r.json()['raw_key']}"}
    pytest.skip("Rate limit on admin key — run tests with a longer interval")


@pytest.fixture(scope="module")
def user() -> tuple[str, dict]:
    return _make_user()


@pytest.fixture()
def fresh_user() -> tuple[str, dict]:
    return _make_user()


# ── Health ────────────────────────────────────────────────────────────


class TestHealth:
    def test_ok(self):
        r = CLIENT.get("/health")
        assert r.status_code == 200
        assert r.json()["status"] == "ok"
        assert r.json()["database"] == "connected"


# ── Auth ──────────────────────────────────────────────────────────────


class TestAuth:
    def test_no_token(self):
        assert CLIENT.get("/v1/memories").status_code in (401, 403)

    def test_bad_token(self):
        assert (
            CLIENT.get(
                "/v1/memories", headers={"Authorization": "Bearer bad"}
            ).status_code
            == 401
        )

    def test_key_create_and_use(self):
        uid, h = _make_user()
        assert CLIENT.get("/v1/memories", headers=h).status_code == 200

    def test_revoke_key(self):
        uid, h = _make_user()
        # Get key_id
        keys = CLIENT.get("/auth/keys", headers=h).json()
        kid = keys[0]["key_id"]

        assert CLIENT.delete(f"/auth/keys/{kid}", headers=h).status_code == 204
        assert CLIENT.get("/v1/memories", headers=h).status_code == 401

    def test_list_keys(self):
        uid, h = _make_user()
        r = CLIENT.get("/auth/keys", headers=h)
        assert r.status_code == 200
        assert len(r.json()) >= 1


# ── Memory List ───────────────────────────────────────────────────────


class TestMemoryList:
    def test_list_empty(self, fresh_user):
        _, h = fresh_user
        r = CLIENT.get("/v1/memories", headers=h)
        assert r.status_code == 200
        assert r.json()["items"] == []
        assert r.json()["next_cursor"] is None

    def test_list_returns_stored(self, fresh_user):
        _, h = fresh_user
        CLIENT.post("/v1/memories", json={"content": "list test 1"}, headers=h)
        CLIENT.post("/v1/memories", json={"content": "list test 2"}, headers=h)

        r = CLIENT.get("/v1/memories", headers=h)
        assert r.status_code == 200
        contents = [m["content"] for m in r.json()["items"]]
        assert "list test 1" in contents
        assert "list test 2" in contents

    def test_cursor_pagination(self, fresh_user):
        _, h = fresh_user
        for i in range(5):
            CLIENT.post("/v1/memories", json={"content": f"page {i}"}, headers=h)

        r1 = CLIENT.get("/v1/memories", params={"limit": 2}, headers=h)
        assert r1.status_code == 200
        data1 = r1.json()
        assert len(data1["items"]) == 2
        assert data1["next_cursor"] is not None

        r2 = CLIENT.get(
            "/v1/memories",
            params={"limit": 2, "cursor": data1["next_cursor"]},
            headers=h,
        )
        data2 = r2.json()
        ids1 = {m["memory_id"] for m in data1["items"]}
        ids2 = {m["memory_id"] for m in data2["items"]}
        assert ids1.isdisjoint(ids2)


# ── Memory CRUD ───────────────────────────────────────────────────────


class TestMemory:
    def test_store_and_fields(self, fresh_user):
        _, h = fresh_user
        r = CLIENT.post(
            "/v1/memories",
            json={"content": "store test", "memory_type": "semantic"},
            headers=h,
        )
        assert r.status_code == 201
        d = r.json()
        assert "memory_id" in d
        assert d["content"] == "store test"
        assert "semantic" in d["memory_type"].lower()

    def test_correct(self, fresh_user):
        _, h = fresh_user
        mid = CLIENT.post(
            "/v1/memories", json={"content": "original"}, headers=h
        ).json()["memory_id"]
        r = CLIENT.put(
            f"/v1/memories/{mid}/correct",
            json={"new_content": "corrected", "reason": "fix"},
            headers=h,
        )
        assert r.status_code == 200
        assert r.json()["content"] == "corrected"

    def test_correct_by_query(self, fresh_user):
        _, h = fresh_user
        mid = CLIENT.post(
            "/v1/memories",
            json={"content": "My favorite language is Python"},
            headers=h,
        ).json()["memory_id"]
        r = CLIENT.post(
            "/v1/memories/correct",
            json={
                "query": "favorite language",
                "new_content": "My favorite language is Rust",
                "reason": "changed preference",
            },
            headers=h,
        )
        assert r.status_code == 200
        data = r.json()
        assert data["content"] == "My favorite language is Rust"
        assert data["matched_memory_id"] == mid

    def test_correct_by_query_no_match(self, fresh_user):
        _, h = fresh_user
        r = CLIENT.post(
            "/v1/memories/correct",
            json={
                "query": "nonexistent topic xyz999",
                "new_content": "irrelevant",
            },
            headers=h,
        )
        assert r.status_code == 404

    def test_delete(self, fresh_user):
        _, h = fresh_user
        mid = CLIENT.post(
            "/v1/memories", json={"content": "to delete"}, headers=h
        ).json()["memory_id"]
        assert CLIENT.delete(f"/v1/memories/{mid}", headers=h).status_code == 200

        # Should not appear in list
        items = CLIENT.get("/v1/memories", headers=h).json()["items"]
        assert not any(m["memory_id"] == mid for m in items)

    def test_batch_store(self, fresh_user):
        _, h = fresh_user
        r = CLIENT.post(
            "/v1/memories/batch",
            json={"memories": [{"content": f"batch_{i}"} for i in range(3)]},
            headers=h,
        )
        assert r.status_code == 201
        assert len(r.json()) == 3

    def test_retrieve(self, user):
        _, h = user
        CLIENT.post(
            "/v1/memories",
            json={"content": "My favorite database is MatrixOne"},
            headers=h,
        )
        r = CLIENT.post(
            "/v1/memories/retrieve", json={"query": "database", "top_k": 5}, headers=h
        )
        assert r.status_code == 200
        data = r.json()
        assert "results" in data
        assert isinstance(data["results"], list)

    def test_search(self, user):
        _, h = user
        r = CLIENT.post(
            "/v1/memories/search", json={"query": "MatrixOne", "top_k": 5}, headers=h
        )
        assert r.status_code == 200

    def test_purge_by_type(self, fresh_user):
        _, h = fresh_user
        CLIENT.post(
            "/v1/memories", json={"content": "wk", "memory_type": "working"}, headers=h
        )
        CLIENT.post(
            "/v1/memories",
            json={"content": "sem", "memory_type": "semantic"},
            headers=h,
        )

        r = CLIENT.post(
            "/v1/memories/purge", json={"memory_types": ["working"]}, headers=h
        )
        assert r.status_code == 200

        items = CLIENT.get("/v1/memories", headers=h).json()["items"]
        types = [m["memory_type"] for m in items]
        assert "working" not in types
        assert any("semantic" in t for t in types)

    def test_purge_by_ids(self, fresh_user):
        _, h = fresh_user
        mid1 = CLIENT.post(
            "/v1/memories", json={"content": "purge 1"}, headers=h
        ).json()["memory_id"]
        mid2 = CLIENT.post("/v1/memories", json={"content": "keep"}, headers=h).json()[
            "memory_id"
        ]

        r = CLIENT.post("/v1/memories/purge", json={"memory_ids": [mid1]}, headers=h)
        assert r.status_code == 200

        items = CLIENT.get("/v1/memories", headers=h).json()["items"]
        mids = [m["memory_id"] for m in items]
        assert mid1 not in mids
        assert mid2 in mids

    def test_profile(self, user):
        _, h = user
        r = CLIENT.get("/v1/profiles/me", headers=h)
        assert r.status_code == 200
        d = r.json()
        assert "user_id" in d
        assert "stats" in d
        assert "total" in d["stats"]


# ── Observe ───────────────────────────────────────────────────────────


class TestObserve:
    def test_observe(self, fresh_user):
        _, h = fresh_user
        r = CLIENT.post(
            "/v1/observe",
            json={
                "messages": [
                    {
                        "role": "user",
                        "content": "I work at Acme Corp as a senior engineer",
                    },
                    {"role": "assistant", "content": "Got it."},
                ]
            },
            headers=h,
        )
        assert r.status_code == 200
        assert isinstance(r.json(), list)


# ── Snapshots ─────────────────────────────────────────────────────────


class TestSnapshots:
    def test_lifecycle(self, fresh_user):
        _, h = fresh_user
        CLIENT.post("/v1/memories", json={"content": "before snap"}, headers=h)

        name = f"snap_{uuid.uuid4().hex[:6]}"
        r = CLIENT.post("/v1/snapshots", json={"name": name}, headers=h)
        assert r.status_code == 201
        assert r.json()["name"] == name

        # Add memory AFTER snapshot
        CLIENT.post("/v1/memories", json={"content": "after snap"}, headers=h)

        # Read snapshot — should only see "before snap"
        r = CLIENT.get(f"/v1/snapshots/{name}", headers=h)
        assert r.status_code == 200
        contents = [m["content"] for m in r.json()["memories"]]
        assert "before snap" in contents
        assert "after snap" not in contents

        # List
        assert any(
            s["name"] == name for s in CLIENT.get("/v1/snapshots", headers=h).json()
        )

        # Delete
        assert CLIENT.delete(f"/v1/snapshots/{name}", headers=h).status_code == 204
        assert CLIENT.get(f"/v1/snapshots/{name}", headers=h).status_code == 404

    def test_duplicate_409(self, fresh_user):
        _, h = fresh_user
        assert (
            CLIENT.post("/v1/snapshots", json={"name": "dup"}, headers=h).status_code
            == 201
        )
        assert (
            CLIENT.post("/v1/snapshots", json={"name": "dup"}, headers=h).status_code
            == 409
        )

    def test_diff(self, fresh_user):
        _, h = fresh_user
        CLIENT.post("/v1/memories", json={"content": "diff A"}, headers=h)
        CLIENT.post("/v1/memories", json={"content": "diff B"}, headers=h)

        time.sleep(0.3)
        CLIENT.post("/v1/snapshots", json={"name": "diff_base"}, headers=h)
        time.sleep(0.3)

        CLIENT.post("/v1/memories", json={"content": "diff C"}, headers=h)
        items = CLIENT.get("/v1/memories", headers=h).json()["items"]
        a_mid = next(m["memory_id"] for m in items if m["content"] == "diff A")
        CLIENT.delete(f"/v1/memories/{a_mid}", headers=h)

        r = CLIENT.get("/v1/snapshots/diff_base/diff", headers=h)
        assert r.status_code == 200
        d = r.json()
        assert d["added_count"] == 1
        assert d["removed_count"] == 1
        assert any("diff C" in m["content"] for m in d["added"])
        assert any("diff A" in m["content"] for m in d["removed"])


# ── User Ops ──────────────────────────────────────────────────────────


class TestUserOps:
    def test_consolidate(self, user):
        _, h = user
        r = CLIENT.post("/v1/consolidate", headers=h)
        assert r.status_code == 200
        assert isinstance(r.json(), dict)

    def test_consolidate_cooldown(self, fresh_user):
        _, h = fresh_user
        r1 = CLIENT.post("/v1/consolidate", headers=h)
        assert r1.status_code == 200
        assert r1.json().get("cached") is not True

        r2 = CLIENT.post("/v1/consolidate", headers=h)
        assert r2.status_code == 200
        assert r2.json().get("cached") is True

    def test_consolidate_force(self, fresh_user):
        _, h = fresh_user
        CLIENT.post("/v1/consolidate", headers=h)
        r = CLIENT.post("/v1/consolidate?force=true", headers=h)
        assert r.status_code == 200
        assert r.json().get("cached") is not True

    def test_reflect(self, user):
        _, h = user
        r = CLIENT.post("/v1/reflect", headers=h)
        assert r.status_code == 200


# ── Admin ─────────────────────────────────────────────────────────────


class TestAdmin:
    @pytest.fixture()
    def ah(self):
        return {"Authorization": f"Bearer {MASTER}"}

    def test_non_admin_rejected(self, user):
        _, h = user
        assert CLIENT.get("/admin/stats", headers=h).status_code == 403

    def test_stats(self, ah):
        r = CLIENT.get("/admin/stats", headers=ah)
        assert r.status_code == 200
        d = r.json()
        assert d["total_users"] >= 1
        assert "total_memories" in d
        assert "total_snapshots" in d

    def test_list_users(self, ah):
        r = CLIENT.get("/admin/users?limit=2", headers=ah)
        assert r.status_code == 200
        assert "users" in r.json()

    def test_user_stats(self, ah, user):
        uid, _ = user
        r = CLIENT.get(f"/admin/users/{uid}/stats", headers=ah)
        assert r.status_code == 200
        assert r.json()["user_id"] == uid

    def test_delete_user(self, ah):
        uid, h = _make_user()
        assert CLIENT.delete(f"/admin/users/{uid}", headers=ah).status_code == 200
        # Key should be revoked
        assert CLIENT.get("/v1/memories", headers=h).status_code == 401

    def test_governance_trigger(self, ah, user):
        uid, _ = user
        r = CLIENT.post(f"/admin/governance/{uid}/trigger", headers=ah)
        assert r.status_code == 200
        assert r.json()["op"] == "governance"

    def test_governance_invalid_op(self, ah, user):
        uid, _ = user
        assert (
            CLIENT.post(
                f"/admin/governance/{uid}/trigger?op=bad", headers=ah
            ).status_code
            == 400
        )


# ── Rate Limiting ─────────────────────────────────────────────────────


class TestRateLimit:
    def test_headers_present(self, user):
        _, h = user
        r = CLIENT.get("/v1/memories", headers=h)
        assert "x-ratelimit-limit" in r.headers
        assert "x-ratelimit-remaining" in r.headers

    def test_remaining_decrements(self, fresh_user):
        _, h = fresh_user
        r1 = CLIENT.get("/v1/memories", headers=h)
        r2 = CLIENT.get("/v1/memories", headers=h)
        assert int(r2.headers["x-ratelimit-remaining"]) < int(
            r1.headers["x-ratelimit-remaining"]
        )


# ── Error Paths ───────────────────────────────────────────────────────


class TestErrorPaths:
    def test_correct_nonexistent(self, user):
        _, h = user
        assert (
            CLIENT.put(
                "/v1/memories/nonexistent/correct",
                json={"new_content": "x", "reason": "y"},
                headers=h,
            ).status_code
            == 404
        )

    def test_delete_nonexistent_snapshot(self, user):
        _, h = user
        assert (
            CLIENT.delete("/v1/snapshots/nonexistent_snap", headers=h).status_code
            == 404
        )

    def test_read_nonexistent_snapshot(self, user):
        _, h = user
        assert (
            CLIENT.get("/v1/snapshots/nonexistent_snap", headers=h).status_code == 404
        )

    def test_empty_content_rejected(self, user):
        _, h = user
        assert (
            CLIENT.post("/v1/memories", json={"content": ""}, headers=h).status_code
            == 422
        )

    def test_batch_empty_rejected(self, user):
        _, h = user
        assert (
            CLIENT.post(
                "/v1/memories/batch", json={"memories": []}, headers=h
            ).status_code
            == 422
        )

    def test_search_empty_query_rejected(self, user):
        _, h = user
        assert (
            CLIENT.post(
                "/v1/memories/search", json={"query": ""}, headers=h
            ).status_code
            == 422
        )

    def test_top_k_zero_rejected(self, user):
        _, h = user
        assert (
            CLIENT.post(
                "/v1/memories/search", json={"query": "x", "top_k": 0}, headers=h
            ).status_code
            == 422
        )

    def test_top_k_over_max_rejected(self, user):
        _, h = user
        assert (
            CLIENT.post(
                "/v1/memories/search", json={"query": "x", "top_k": 101}, headers=h
            ).status_code
            == 422
        )

    def test_invalid_memory_type_rejected(self, user):
        _, h = user
        assert (
            CLIENT.post(
                "/v1/memories",
                json={"content": "x", "memory_type": "invalid"},
                headers=h,
            ).status_code
            == 422
        )


# ── Cross-User Isolation ──────────────────────────────────────────────


class TestIsolation:
    def test_cannot_see_other_memories(self):
        _, h_a = _make_user()
        _, h_b = _make_user()

        mid_a = CLIENT.post(
            "/v1/memories", json={"content": "secret A"}, headers=h_a
        ).json()["memory_id"]

        b_mids = [
            m["memory_id"]
            for m in CLIENT.get("/v1/memories", headers=h_b).json()["items"]
        ]
        assert mid_a not in b_mids

    def test_cannot_correct_other_memory(self):
        _, h_a = _make_user()
        _, h_b = _make_user()

        mid_a = CLIENT.post(
            "/v1/memories", json={"content": "A's memory"}, headers=h_a
        ).json()["memory_id"]
        r = CLIENT.put(
            f"/v1/memories/{mid_a}/correct",
            json={"new_content": "hacked", "reason": "x"},
            headers=h_b,
        )
        assert r.status_code in (403, 404)

        # A's memory unchanged
        items = CLIENT.get("/v1/memories", headers=h_a).json()["items"]
        assert any(m["content"] == "A's memory" for m in items)

    def test_cannot_see_other_snapshots(self):
        _, h_a = _make_user()
        _, h_b = _make_user()

        CLIENT.post("/v1/snapshots", json={"name": "private_snap"}, headers=h_a)
        assert CLIENT.get("/v1/snapshots/private_snap", headers=h_b).status_code == 404

    def test_cannot_revoke_other_key(self):
        _, h_a = _make_user()
        _, h_b = _make_user()

        kid_a = CLIENT.get("/auth/keys", headers=h_a).json()[0]["key_id"]
        assert CLIENT.delete(f"/auth/keys/{kid_a}", headers=h_b).status_code in (
            403,
            404,
        )
        assert CLIENT.get("/v1/memories", headers=h_a).status_code == 200


# ── Boundary Values ───────────────────────────────────────────────────


class TestBoundaryValues:
    def test_top_k_boundary_accepted(self, user):
        _, h = user
        assert (
            CLIENT.post(
                "/v1/memories/search", json={"query": "x", "top_k": 1}, headers=h
            ).status_code
            == 200
        )
        assert (
            CLIENT.post(
                "/v1/memories/search", json={"query": "x", "top_k": 100}, headers=h
            ).status_code
            == 200
        )

    def test_very_long_content(self, fresh_user):
        _, h = fresh_user
        r = CLIENT.post("/v1/memories", json={"content": "x" * 10000}, headers=h)
        assert r.status_code == 201

    def test_sql_injection_safe(self, fresh_user):
        _, h = fresh_user
        evil = "Robert'); DROP TABLE mem_memories;--"
        r = CLIENT.post("/v1/memories", json={"content": evil}, headers=h)
        assert r.status_code == 201
        # Verify it's stored verbatim
        items = CLIENT.get("/v1/memories", headers=h).json()["items"]
        assert any(m["content"] == evil for m in items)

    def test_unicode_content(self, fresh_user):
        _, h = fresh_user
        content = "我喜欢用 MatrixOne 🚀 数据库"
        r = CLIENT.post("/v1/memories", json={"content": content}, headers=h)
        assert r.status_code == 201
        items = CLIENT.get("/v1/memories", headers=h).json()["items"]
        assert any(m["content"] == content for m in items)

    def test_snapshot_special_chars_sanitized(self, fresh_user):
        _, h = fresh_user
        r = CLIENT.post("/v1/snapshots", json={"name": "my-snap!@#$"}, headers=h)
        assert r.status_code in (201, 400, 422)

    def test_batch_50(self, fresh_user):
        _, h = fresh_user
        r = CLIENT.post(
            "/v1/memories/batch",
            json={"memories": [{"content": f"bulk_{i}"} for i in range(50)]},
            headers=h,
        )
        assert r.status_code == 201
        assert len(r.json()) == 50


# ── Profile Stats ─────────────────────────────────────────────────────


class TestProfileStats:
    def test_stats_fields(self, fresh_user):
        _, h = fresh_user
        CLIENT.post("/v1/memories", json={"content": "sem"}, headers=h)
        CLIENT.post(
            "/v1/memories",
            json={"content": "proc", "memory_type": "procedural"},
            headers=h,
        )

        r = CLIENT.get("/v1/profiles/me", headers=h)
        assert r.status_code == 200
        stats = r.json()["stats"]
        assert stats["total"] == 2
        assert stats["avg_confidence"] is not None
        assert stats["oldest"] is not None
        assert stats["newest"] is not None
