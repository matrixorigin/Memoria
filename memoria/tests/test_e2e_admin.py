"""E2E tests — admin & boundary (Admin, RateLimit, Isolation, Purge, Stats, Boundary).

Run: pytest memoria/tests/test_e2e_admin.py -v
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


@pytest.fixture(scope="module")
def db():
    from memoria.api.database import init_db, get_session_factory

    init_db()
    session = get_session_factory()()
    yield session
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


class TestAdmin:
    @pytest.fixture()
    def admin_h(self):
        return {"Authorization": f"Bearer {MASTER_KEY}"}

    def test_non_admin_rejected(self, client, user_key):
        _, h = user_key
        assert client.get("/admin/stats", headers=h).status_code == 403
        assert client.get("/admin/users", headers=h).status_code == 403

    def test_stats(self, client, admin_h):
        r = client.get("/admin/stats", headers=admin_h)
        assert r.status_code == 200
        data = r.json()
        assert "total_users" in data
        assert "total_memories" in data
        assert "total_snapshots" in data
        assert data["total_users"] >= 1

    def test_list_users_pagination(self, client, admin_h):
        r = client.get("/admin/users?limit=2", headers=admin_h)
        assert r.status_code == 200
        data = r.json()
        assert "users" in data
        assert "next_cursor" in data
        assert len(data["users"]) <= 2

    def test_user_stats(self, client, admin_h, user_key):
        uid, _ = user_key
        r = client.get(f"/admin/users/{uid}/stats", headers=admin_h)
        assert r.status_code == 200
        data = r.json()
        assert data["user_id"] == uid
        assert "memory_count" in data
        assert "api_key_count" in data

    def test_delete_user_db(self, client, db, admin_h):
        uid, _, kid = _make_user(client)

        r = client.delete(f"/admin/users/{uid}", headers=admin_h)
        assert r.status_code == 200

        # DB: user deactivated
        row = db.execute(
            text("SELECT is_active FROM tm_users WHERE user_id = :uid"), {"uid": uid}
        ).first()
        assert row[0] == 0

        # DB: all keys revoked
        active_keys = db.execute(
            text(
                "SELECT COUNT(*) FROM auth_api_keys WHERE user_id = :uid AND is_active"
            ),
            {"uid": uid},
        ).scalar()
        assert active_keys == 0

    def test_governance_trigger(self, client, admin_h, user_key):
        uid, _ = user_key
        r = client.post(f"/admin/governance/{uid}/trigger", headers=admin_h)
        assert r.status_code == 200
        assert r.json()["op"] == "governance"
        assert r.json()["user_id"] == uid

    def test_governance_invalid_op(self, client, admin_h, user_key):
        uid, _ = user_key
        r = client.post(f"/admin/governance/{uid}/trigger?op=invalid", headers=admin_h)
        assert r.status_code == 400

    def test_admin_list_user_keys(self, client, db, admin_h):
        """GET /admin/users/{user_id}/keys returns all active keys with full fields."""
        uid, h, kid = _make_user(client)
        # Create a second key for the same user
        r2 = client.post(
            "/auth/keys",
            json={"user_id": uid, "name": "second-key"},
            headers=admin_h,
        )
        assert r2.status_code == 201
        kid2 = r2.json()["key_id"]

        r = client.get(f"/admin/users/{uid}/keys", headers=admin_h)
        assert r.status_code == 200
        data = r.json()
        assert data["user_id"] == uid
        keys = data["keys"]
        assert len(keys) == 2
        key_ids = {k["key_id"] for k in keys}
        assert kid in key_ids
        assert kid2 in key_ids

        # All fields present on each key
        for k in keys:
            assert "key_id" in k
            assert "user_id" in k
            assert "name" in k
            assert "key_prefix" in k
            assert "created_at" in k
            assert "expires_at" in k
            assert "last_used_at" in k
            assert k.get("raw_key") is None  # never returned on list
            assert k["user_id"] == uid

        # DB ground truth: both keys active
        count = db.execute(
            text(
                "SELECT COUNT(*) FROM auth_api_keys WHERE user_id = :uid AND is_active"
            ),
            {"uid": uid},
        ).scalar()
        assert count == 2

    def test_admin_list_user_keys_non_admin_rejected(self, client, user_key):
        """Non-admin cannot access /admin/users/{user_id}/keys."""
        uid, h = user_key
        r = client.get(f"/admin/users/{uid}/keys", headers=h)
        assert r.status_code == 403

    def test_admin_list_user_keys_revoked_excluded(self, client, db, admin_h):
        """Revoked keys are not returned in admin key list."""
        uid, h, kid = _make_user(client)
        client.delete(f"/auth/keys/{kid}", headers=h)

        r = client.get(f"/admin/users/{uid}/keys", headers=admin_h)
        assert r.status_code == 200
        key_ids = {k["key_id"] for k in r.json()["keys"]}
        assert kid not in key_ids

    def test_admin_revoke_all_keys(self, client, db, admin_h):
        """DELETE /admin/users/{user_id}/keys revokes all active keys."""
        uid, h, _kid1 = _make_user(client)
        client.post(
            "/auth/keys",
            json={"user_id": uid, "name": "key2"},
            headers=admin_h,
        )

        r = client.delete(f"/admin/users/{uid}/keys", headers=admin_h)
        assert r.status_code == 200
        assert r.json()["revoked"] == 2
        assert r.json()["user_id"] == uid

        # DB: both keys deactivated
        active = db.execute(
            text(
                "SELECT COUNT(*) FROM auth_api_keys WHERE user_id = :uid AND is_active"
            ),
            {"uid": uid},
        ).scalar()
        assert active == 0

        # Both keys rejected
        assert client.get("/v1/memories", headers=h).status_code == 401

    def test_admin_revoke_all_keys_non_admin_rejected(self, client, user_key):
        uid, h = user_key
        r = client.delete(f"/admin/users/{uid}/keys", headers=h)
        assert r.status_code == 403


# ── Rate Limiting ─────────────────────────────────────────────────────


class TestRateLimit:
    def test_rate_limit_headers(self, client, user_key):
        _, h = user_key
        r = client.get("/v1/memories", headers=h)
        assert "x-ratelimit-limit" in r.headers
        assert "x-ratelimit-remaining" in r.headers


# ── Error Paths ───────────────────────────────────────────────────────


class TestErrorPaths:
    def test_correct_nonexistent_memory(self, client, user_key):
        _, h = user_key
        r = client.put(
            "/v1/memories/nonexistent-id/correct",
            json={"new_content": "x", "reason": "y"},
            headers=h,
        )
        assert r.status_code == 404

    def test_delete_nonexistent_snapshot(self, client, user_key):
        _, h = user_key
        r = client.delete("/v1/snapshots/nonexistent_snap", headers=h)
        assert r.status_code == 404

    def test_read_nonexistent_snapshot(self, client, user_key):
        _, h = user_key
        r = client.get("/v1/snapshots/nonexistent_snap", headers=h)
        assert r.status_code == 404

    def test_expired_key_rejected(self, client, db):
        uid, h, kid = _make_user(client)
        # Manually expire the key in DB
        db.execute(
            text(
                "UPDATE auth_api_keys SET expires_at = '2020-01-01 00:00:00' WHERE key_id = :kid"
            ),
            {"kid": kid},
        )
        db.commit()

        r = client.get("/v1/memories", headers=h)
        assert r.status_code == 401

    def test_store_empty_content_rejected(self, client, user_key):
        _, h = user_key
        r = client.post("/v1/memories", json={"content": ""}, headers=h)
        assert r.status_code == 422  # pydantic validation

    def test_batch_empty_list_rejected(self, client, user_key):
        _, h = user_key
        r = client.post("/v1/memories/batch", json={"memories": []}, headers=h)
        assert r.status_code == 422

    def test_search_empty_query_rejected(self, client, user_key):
        _, h = user_key
        r = client.post("/v1/memories/search", json={"query": ""}, headers=h)
        assert r.status_code == 422


# ── Cross-User Isolation ──────────────────────────────────────────────


class TestIsolation:
    def test_user_cannot_see_other_memories(self, client, db):
        _, h_a, _ = _make_user(client)
        _, h_b, _ = _make_user(client)

        # A stores
        r = client.post(
            "/v1/memories", json={"content": "secret of user A"}, headers=h_a
        )
        mid_a = r.json()["memory_id"]

        # B cannot see A's memory in list
        r = client.get("/v1/memories", headers=h_b)
        b_mids = [m["memory_id"] for m in r.json()["items"]]
        assert mid_a not in b_mids

        # B cannot correct A's memory
        r = client.put(
            f"/v1/memories/{mid_a}/correct",
            json={"new_content": "hacked", "reason": "x"},
            headers=h_b,
        )
        assert r.status_code in (404, 403)

        # DB: A's memory untouched
        row = db.execute(
            text("SELECT content, is_active FROM mem_memories WHERE memory_id = :mid"),
            {"mid": mid_a},
        ).first()
        assert row[0] == "secret of user A"
        assert row[1] == 1

    def test_user_cannot_see_other_snapshots(self, client):
        _, h_a, _ = _make_user(client)
        _, h_b, _ = _make_user(client)

        client.post("/v1/snapshots", json={"name": "private_snap"}, headers=h_a)

        # B cannot read A's snapshot
        r = client.get("/v1/snapshots/private_snap", headers=h_b)
        assert r.status_code == 404

    def test_user_cannot_revoke_other_key(self, client):
        _, h_a, kid_a = _make_user(client)
        _, h_b, _ = _make_user(client)

        # Ensure A's key works first
        assert client.get("/v1/memories", headers=h_a).status_code == 200

        r = client.delete(f"/auth/keys/{kid_a}", headers=h_b)
        assert r.status_code in (403, 404)  # not your key


# ── Rate Limiting (actual 429) ────────────────────────────────────────


class TestRateLimitEnforcement:
    def test_rate_limit_headers_decrement(self, client):
        """Verify rate limit remaining decreases with each request."""
        from memoria.api.middleware import _windows

        _, h, _ = _make_user(client)
        _windows.clear()

        r1 = client.get("/v1/memories", headers=h)
        r2 = client.get("/v1/memories", headers=h)
        rem1 = int(r1.headers["x-ratelimit-remaining"])
        rem2 = int(r2.headers["x-ratelimit-remaining"])
        assert rem2 < rem1

    def test_429_when_limit_exceeded(self, client):
        """Hit a rate limit and verify 429."""
        from memoria.api.middleware import _windows, _RATE_LIMITS

        _, h, _ = _make_user(client)
        _windows.clear()

        # Use the actual configured limit for consolidate
        max_req, _ = _RATE_LIMITS.get("POST:/v1/consolidate", (3, 3600))

        for _ in range(max_req):
            r = client.post("/v1/consolidate?force=true", headers=h)
            assert r.status_code == 200

        r = client.post("/v1/consolidate?force=true", headers=h)
        assert r.status_code == 429
        assert "retry-after" in r.headers


# ── Governance Scheduler ──────────────────────────────────────────────


@pytest.mark.xdist_group("governance")
class TestGovernanceScheduler:
    def test_scheduler_starts_and_stops(self):
        """Verify the scheduler can be instantiated and started/stopped without error."""
        import asyncio
        from memoria.core.scheduler import (
            GovernanceTaskRunner,
            AsyncIOBackend,
            MemoryGovernanceScheduler,
        )
        from memoria.api.database import get_db_context, get_db_factory

        runner = GovernanceTaskRunner(
            get_db_context, db_factory=get_db_factory(), memory_only=True
        )
        backend = AsyncIOBackend(runner)
        scheduler = MemoryGovernanceScheduler(backend=backend)

        async def _test():
            await scheduler.start()
            await asyncio.sleep(0.1)  # let it tick
            await scheduler.stop()

        asyncio.run(_test())

    def test_governance_runner_executes(self, client):
        """Run governance directly — verify result dict returned."""
        from memoria.core.scheduler import GovernanceTaskRunner
        from memoria.api.database import get_db_context, get_db_factory

        _, h, _ = _make_user(client)
        client.post(
            "/v1/memories", json={"content": "governance test memory"}, headers=h
        )

        runner = GovernanceTaskRunner(
            get_db_context, db_factory=get_db_factory(), memory_only=True
        )
        result = runner.run("hourly")
        assert result is None or isinstance(result, dict)
        if result is not None:
            assert "mem_cleaned_tool_results" in result
            assert "mem_archived_working" in result


# ── Admin Governance Ops ──────────────────────────────────────────────


class TestAdminGovernanceOps:
    @pytest.fixture()
    def admin_h(self):
        return {"Authorization": f"Bearer {MASTER_KEY}"}

    def test_admin_consolidate(self, client, admin_h, user_key):
        uid, _ = user_key
        r = client.post(
            f"/admin/governance/{uid}/trigger?op=consolidate", headers=admin_h
        )
        assert r.status_code == 200
        assert r.json()["op"] == "consolidate"

    def test_admin_reflect_graceful(self, client, admin_h, user_key):
        """Reflect via admin — returns message since it needs LLM."""
        uid, _ = user_key
        r = client.post(f"/admin/governance/{uid}/trigger?op=reflect", headers=admin_h)
        assert r.status_code == 200
        assert r.json()["op"] == "reflect"


# ── Observe DB Verification ──────────────────────────────────────────


class TestObserveDB:
    def test_observe_persists_extracted_memories(self, client, db):
        """Observe should persist extracted memories to DB (if LLM available)."""
        uid, h, _ = _make_user(client)

        before = db.execute(
            text(
                "SELECT COUNT(*) FROM mem_memories WHERE user_id = :uid AND is_active"
            ),
            {"uid": uid},
        ).scalar()

        r = client.post(
            "/v1/observe",
            json={
                "messages": [
                    {
                        "role": "user",
                        "content": "I prefer Python 3.11 and use MatrixOne as my database",
                    },
                    {
                        "role": "assistant",
                        "content": "Noted — Python 3.11 and MatrixOne.",
                    },
                ]
            },
            headers=h,
        )
        assert r.status_code == 200
        extracted = r.json()

        if len(extracted.get("memories", [])) > 0:
            # If extraction worked, verify DB has new rows
            after = db.execute(
                text(
                    "SELECT COUNT(*) FROM mem_memories WHERE user_id = :uid AND is_active"
                ),
                {"uid": uid},
            ).scalar()
            assert after > before

            # Verify each returned memory_id exists in DB
            for mem in extracted["memories"]:
                row = db.execute(
                    text(
                        "SELECT user_id, is_active FROM mem_memories WHERE memory_id = :mid"
                    ),
                    {"mid": mem["memory_id"]},
                ).first()
                assert row is not None
                assert row[0] == uid
                assert row[1] == 1


# ── Purge Multi-Condition ─────────────────────────────────────────────


class TestPurgeMultiCondition:
    def test_purge_by_memory_ids(self, client, db):
        """Purge specific memory IDs, verify only those deactivated."""
        uid, h, _ = _make_user(client)
        mid1 = client.post(
            "/v1/memories", json={"content": "I visited Tokyo last spring"}, headers=h
        ).json()["memory_id"]
        mid2 = client.post(
            "/v1/memories",
            json={"content": "My car needs an oil change soon"},
            headers=h,
        ).json()["memory_id"]
        mid3 = client.post(
            "/v1/memories",
            json={"content": "I enjoy reading fantasy novels"},
            headers=h,
        ).json()["memory_id"]

        r = client.post(
            "/v1/memories/purge", json={"memory_ids": [mid1, mid2]}, headers=h
        )
        assert r.status_code == 200
        assert r.json()["purged"] >= 2

        # DB: mid1, mid2 deactivated; mid3 survives
        for mid in (mid1, mid2):
            row = db.execute(
                text("SELECT is_active FROM mem_memories WHERE memory_id = :mid"),
                {"mid": mid},
            ).first()
            assert row[0] == 0
        row3 = db.execute(
            text("SELECT is_active FROM mem_memories WHERE memory_id = :mid"),
            {"mid": mid3},
        ).first()
        assert row3[0] == 1

    def test_purge_by_type_and_ids_combined(self, client, db):
        """Purge by type + IDs — both conditions applied."""
        uid, h, _ = _make_user(client)
        mid_wk = client.post(
            "/v1/memories", json={"content": "wk1", "memory_type": "working"}, headers=h
        ).json()["memory_id"]
        mid_sem = client.post(
            "/v1/memories",
            json={"content": "sem1", "memory_type": "semantic"},
            headers=h,
        ).json()["memory_id"]

        # Purge working type
        r = client.post(
            "/v1/memories/purge", json={"memory_types": ["working"]}, headers=h
        )
        assert r.status_code == 200

        # DB: working gone, semantic survives
        wk = db.execute(
            text("SELECT is_active FROM mem_memories WHERE memory_id = :mid"),
            {"mid": mid_wk},
        ).first()
        assert wk[0] == 0
        sem = db.execute(
            text("SELECT is_active FROM mem_memories WHERE memory_id = :mid"),
            {"mid": mid_sem},
        ).first()
        assert sem[0] == 1


# ── Admin Stats Accuracy ─────────────────────────────────────────────


@pytest.mark.xdist_group("governance")
class TestAdminStatsAccuracy:
    def test_stats_reflect_actual_db(self, client, db):
        """Admin stats should be consistent with DB (>= since other workers may write concurrently)."""
        admin_h = {"Authorization": f"Bearer {MASTER_KEY}"}

        # Query DB first, then API — API result must be >= DB snapshot
        actual_users = db.execute(
            text("SELECT COUNT(*) FROM tm_users WHERE is_active = 1")
        ).scalar()
        actual_memories = db.execute(
            text("SELECT COUNT(*) FROM mem_memories WHERE is_active = 1")
        ).scalar()
        actual_snapshots = db.execute(
            text("SELECT COUNT(*) FROM mem_snapshot_registry")
        ).scalar()

        r = client.get("/admin/stats", headers=admin_h)
        assert r.status_code == 200
        stats = r.json()

        assert stats["total_users"] >= actual_users
        assert stats["total_memories"] >= actual_memories
        assert stats["total_snapshots"] >= actual_snapshots

    def test_user_stats_accurate(self, client, db):
        """Per-user stats should match actual DB counts."""
        admin_h = {"Authorization": f"Bearer {MASTER_KEY}"}
        uid, h, _ = _make_user(client)

        # Create known data
        client.post("/v1/memories", json={"content": "stat test 1"}, headers=h)
        client.post("/v1/memories", json={"content": "stat test 2"}, headers=h)
        client.post("/v1/snapshots", json={"name": "stat_snap"}, headers=h)

        r = client.get(f"/admin/users/{uid}/stats", headers=admin_h)
        data = r.json()

        actual_mem = db.execute(
            text(
                "SELECT COUNT(*) FROM mem_memories WHERE user_id = :uid AND is_active"
            ),
            {"uid": uid},
        ).scalar()
        actual_snap = db.execute(
            text("SELECT COUNT(*) FROM mem_snapshot_registry WHERE user_id = :uid"),
            {"uid": uid},
        ).scalar()
        actual_keys = db.execute(
            text(
                "SELECT COUNT(*) FROM auth_api_keys WHERE user_id = :uid AND is_active"
            ),
            {"uid": uid},
        ).scalar()

        assert data["memory_count"] == actual_mem
        assert data["snapshot_count"] == actual_snap
        assert data["api_key_count"] == actual_keys


# ── Consolidate Effect ────────────────────────────────────────────────


class TestConsolidateEffect:
    def test_consolidate_runs_on_real_data(self, client, db):
        """Consolidate on a user with memories — should complete without error."""
        uid, h, _ = _make_user(client)
        # Seed contradictory-ish memories
        client.post(
            "/v1/memories",
            json={"content": "My favorite language is Python"},
            headers=h,
        )
        client.post(
            "/v1/memories", json={"content": "My favorite language is Rust"}, headers=h
        )
        client.post(
            "/v1/memories", json={"content": "I use MatrixOne for storage"}, headers=h
        )

        r = client.post("/v1/consolidate?force=true", headers=h)
        assert r.status_code == 200
        data = r.json()
        assert isinstance(data, dict)
        assert data.get("cached") is not True


# ── Boundary Values ───────────────────────────────────────────────────


class TestBoundaryValues:
    def test_top_k_zero_rejected(self, client, user_key):
        _, h = user_key
        r = client.post(
            "/v1/memories/search", json={"query": "test", "top_k": 0}, headers=h
        )
        assert r.status_code == 422

    def test_top_k_over_max_rejected(self, client, user_key):
        _, h = user_key
        r = client.post(
            "/v1/memories/search", json={"query": "test", "top_k": 101}, headers=h
        )
        assert r.status_code == 422

    def test_top_k_boundary_accepted(self, client, user_key):
        _, h = user_key
        r = client.post(
            "/v1/memories/search", json={"query": "test", "top_k": 1}, headers=h
        )
        assert r.status_code == 200
        r = client.post(
            "/v1/memories/search", json={"query": "test", "top_k": 100}, headers=h
        )
        assert r.status_code == 200

    def test_very_long_content_accepted(self, client, db):
        """Store a large content string — should succeed."""
        uid, h, _ = _make_user(client)
        long_content = "x" * 10000
        r = client.post("/v1/memories", json={"content": long_content}, headers=h)
        assert r.status_code == 201
        mid = r.json()["memory_id"]
        row = db.execute(
            text("SELECT LENGTH(content) FROM mem_memories WHERE memory_id = :mid"),
            {"mid": mid},
        ).scalar()
        assert row == 10000

    def test_invalid_memory_type_rejected(self, client, user_key):
        """Unknown memory_type should be rejected with 422."""
        _, h = user_key
        r = client.post(
            "/v1/memories",
            json={"content": "test", "memory_type": "nonexistent_type"},
            headers=h,
        )
        assert r.status_code == 422

    def test_special_chars_in_content(self, client, db):
        """Content with SQL-injection-like chars should be stored safely."""
        uid, h, _ = _make_user(client)
        evil = "Robert'); DROP TABLE mem_memories;--"
        r = client.post("/v1/memories", json={"content": evil}, headers=h)
        assert r.status_code == 201
        mid = r.json()["memory_id"]
        row = db.execute(
            text("SELECT content FROM mem_memories WHERE memory_id = :mid"),
            {"mid": mid},
        ).first()
        assert row[0] == evil  # stored verbatim, not executed

    def test_unicode_content(self, client, db):
        uid, h, _ = _make_user(client)
        content = "我喜欢用 MatrixOne 🚀 数据库"
        r = client.post("/v1/memories", json={"content": content}, headers=h)
        assert r.status_code == 201
        mid = r.json()["memory_id"]
        row = db.execute(
            text("SELECT content FROM mem_memories WHERE memory_id = :mid"),
            {"mid": mid},
        ).first()
        assert row[0] == content

    def test_snapshot_name_special_chars_sanitized(self, client):
        """Snapshot names with special chars should be sanitized, not crash."""
        _, h, _ = _make_user(client)
        r = client.post("/v1/snapshots", json={"name": "my-snap!@#$"}, headers=h)
        # Should succeed (sanitized) or reject — not crash
        assert r.status_code in (201, 400, 422)

    def test_correct_with_same_content(self, client):
        """Correct a memory with identical content — should still work."""
        _, h, _ = _make_user(client)
        mid = client.post("/v1/memories", json={"content": "same"}, headers=h).json()[
            "memory_id"
        ]
        r = client.put(
            f"/v1/memories/{mid}/correct",
            json={"new_content": "same", "reason": "no change"},
            headers=h,
        )
        assert r.status_code == 200

    def test_batch_large_count(self, client, db):
        """Batch store 50 memories — verify all persisted."""
        uid, h, _ = _make_user(client)
        r = client.post(
            "/v1/memories/batch",
            json={"memories": [{"content": f"bulk_{i}"} for i in range(50)]},
            headers=h,
        )
        assert r.status_code == 201
        assert len(r.json()) == 50
        count = db.execute(
            text(
                "SELECT COUNT(*) FROM mem_memories WHERE user_id = :uid AND is_active AND content LIKE 'bulk_%'"
            ),
            {"uid": uid},
        ).scalar()
        assert count == 50


# ── Governance: Distributed Lock & Scheduling ─────────────────────────
