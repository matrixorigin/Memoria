"""End-to-end MCP interface tests.

Tests the MCP tool layer (create_server / call_tool) with a fake in-memory
backend — no real DB or network required.  Covers:
  - Basic CRUD: store, retrieve, correct, purge, search, profile
  - Snapshot / branch lifecycle
  - Concurrent calls (asyncio.gather)
"""

from __future__ import annotations

import asyncio
import threading
import uuid

import pytest

from memoria.mcp_local.server import MemoryBackend, create_server


# ── Fake backend ──────────────────────────────────────────────────────


class FakeBackend(MemoryBackend):
    """Thread-safe in-memory backend for testing."""

    def __init__(self) -> None:
        self._lock = threading.Lock()
        self._memories: dict[str, dict] = {}  # memory_id → record
        self._snapshots: dict[str, dict] = {}  # name → info
        self._branches: dict[str, dict] = {}  # name → info
        self._active_branch: str = "main"
        self._cooldown_cache: dict[tuple[str, str], tuple[float, dict]] = {}

    def _with_cooldown(self, user_id: str, op: str, fn, force: bool = False) -> dict:
        """Cooldown cache with deep copy to prevent pollution."""
        import copy
        import time

        key = (user_id, op)
        now = time.time()
        if not force:
            cached = self._cooldown_cache.get(key)
            if cached:
                ts, result = cached
                remaining = 3600 - (now - ts)  # 1h cooldown
                if remaining > 0:
                    result_copy = copy.deepcopy(result)
                    result_copy["skipped"] = True
                    result_copy["cooldown_remaining_s"] = int(remaining)
                    return result_copy
        result = fn()
        self._cooldown_cache[key] = (now, copy.deepcopy(result))
        return result

    # ── CRUD ──────────────────────────────────────────────────────────

    def store(
        self, user_id: str, content: str, memory_type: str, session_id: str | None
    ) -> dict:
        mid = str(uuid.uuid4())
        with self._lock:
            self._memories[mid] = {
                "memory_id": mid,
                "user_id": user_id,
                "content": content,
                "memory_type": memory_type,
                "session_id": session_id,
            }
        return {"memory_id": mid, "content": content}

    def retrieve(
        self, user_id: str, query: str, top_k: int, session_id: str | None = None
    ) -> list[dict]:
        with self._lock:
            results = [
                {
                    "memory_id": mid,
                    "type": r["memory_type"],
                    "content": r["content"],
                }
                for mid, r in self._memories.items()
                if r["user_id"] == user_id and query.lower() in r["content"].lower()
            ]
        return results[:top_k]

    def correct(
        self, user_id: str, memory_id: str, new_content: str, reason: str
    ) -> dict:
        with self._lock:
            if memory_id not in self._memories:
                raise KeyError(f"Memory {memory_id} not found")
            self._memories[memory_id]["content"] = new_content
        return {"memory_id": memory_id, "content": new_content}

    def purge(
        self,
        user_id: str,
        memory_id: str | None,
        topic: str | None,
        reason: str,
        memory_ids: list[str] | None = None,
    ) -> dict:
        deleted = 0
        with self._lock:
            if memory_ids:
                for mid in memory_ids:
                    if mid in self._memories:
                        del self._memories[mid]
                        deleted += 1
            elif memory_id:
                if memory_id in self._memories:
                    del self._memories[memory_id]
                    deleted = 1
            elif topic:
                to_del = [
                    mid
                    for mid, r in self._memories.items()
                    if r["user_id"] == user_id and topic.lower() in r["content"].lower()
                ]
                for mid in to_del:
                    del self._memories[mid]
                deleted = len(to_del)
        return {"purged": deleted}

    def profile(self, user_id: str) -> dict:
        with self._lock:
            count = sum(1 for r in self._memories.values() if r["user_id"] == user_id)
        return {"user_id": user_id, "memory_count": count}

    def search(self, user_id: str, query: str, top_k: int) -> list[dict]:
        return self.retrieve(user_id, query, top_k)

    # ── Maintenance (stubs) ───────────────────────────────────────────

    def governance(self, user_id: str, force: bool = False) -> dict:
        return {
            "quarantined": 1,
            "cleaned_stale": 2,
            "scenes_created": 0,
            "vector_index_health": {},
        }

    def consolidate(self, user_id: str, force: bool = False) -> dict:
        return {
            "merged_nodes": 1,
            "conflicts_detected": 0,
            "orphaned_scenes": 0,
            "promoted": 0,
            "demoted": 0,
        }

    def reflect(self, user_id: str, force: bool = False) -> dict:
        return {"scenes_created": 1, "candidates_found": 3}

    def rebuild_index(self, table: str) -> str:
        return f"Rebuilt IVF index for {table}: lists 0 → 4 (rows=10)"

    def health_warnings(self, user_id: str) -> list[str]:
        return []

    # ── Snapshots ─────────────────────────────────────────────────────

    def snapshot_create(self, user_id: str, name: str, description: str) -> dict:
        with self._lock:
            self._snapshots[name] = {
                "name": name,
                "description": description,
                "timestamp": "2026-01-01 00:00",
            }
        return {"name": name}

    def snapshot_list(self, user_id: str) -> list[dict]:
        with self._lock:
            return list(self._snapshots.values())

    def snapshot_rollback(self, user_id: str, name: str) -> dict:
        with self._lock:
            if name not in self._snapshots:
                raise KeyError(f"Snapshot {name} not found")
        return {"rolled_back_to": name}

    def snapshot_delete(
        self,
        user_id: str,
        names: list[str] | None = None,
        prefix: str | None = None,
        older_than: str | None = None,
    ) -> dict:
        with self._lock:
            to_del: list[str] = []
            if names:
                to_del = [n for n in names if n in self._snapshots]
            elif prefix:
                to_del = [n for n in self._snapshots if n.startswith(prefix)]
            for n in to_del:
                del self._snapshots[n]
            return {"deleted": len(to_del), "names": to_del}

    # ── Branches ──────────────────────────────────────────────────────

    def branch_create(
        self,
        user_id: str,
        name: str,
        from_snapshot: str | None,
        from_timestamp: str | None,
    ) -> dict:
        with self._lock:
            self._branches[name] = {"name": name, "active": False}
        return {"name": name}

    def branch_list(self, user_id: str) -> list[dict]:
        with self._lock:
            return list(self._branches.values())

    def branch_checkout(self, user_id: str, name: str) -> dict:
        with self._lock:
            self._active_branch = name
        return {"active": name}

    def branch_delete(self, user_id: str, name: str) -> dict:
        with self._lock:
            self._branches.pop(name, None)
        return {"deleted": name}

    def branch_merge(self, user_id: str, source: str, strategy: str) -> dict:
        return {"merged": 0, "skipped": 0, "strategy": strategy}

    def branch_diff(self, user_id: str, source: str, limit: int) -> dict:
        return {"changes": [], "source": source}

    # ── New methods (stubs) ───────────────────────────────────────────

    def correct_by_query(
        self, user_id: str, query: str, new_content: str, reason: str
    ) -> dict:
        matches = self.retrieve(user_id, query, 1)
        if not matches:
            return {
                "error": "no_match",
                "message": f"No memory found matching '{query}'",
            }
        mid = matches[0]["memory_id"]
        matched_content = matches[0]["content"]
        result = self.correct(user_id, mid, new_content, reason)
        result["matched_memory_id"] = mid
        result["matched_content"] = matched_content
        return result

    def extract_entities(self, user_id: str) -> dict:
        return {"total_memories": 3, "entities_found": 2, "edges_created": 4}

    def get_entity_candidates(self, user_id: str) -> dict:
        with self._lock:
            mems = [
                {"memory_id": mid, "content": r["content"]}
                for mid, r in self._memories.items()
                if r["user_id"] == user_id
            ][:5]
        return {
            "memories": mems,
            "existing_entities": [{"name": "python", "entity_type": "tech"}],
        }

    def get_reflect_candidates(self, user_id: str) -> dict:
        with self._lock:
            mems = [
                {"memory_id": mid, "content": r["content"], "type": r["memory_type"]}
                for mid, r in self._memories.items()
                if r["user_id"] == user_id
            ][:3]
        if not mems:
            return {"candidates": []}
        return {
            "candidates": [
                {"signal": "preference_cluster", "importance": 0.85, "memories": mems}
            ]
        }

    def link_entities(self, user_id: str, entities: list[dict]) -> dict:
        total_ents = sum(len(e.get("entities", [])) for e in entities)
        return {
            "entities_created": total_ents,
            "entities_reused": 0,
            "edges_created": total_ents,
        }


# ── Fixtures ──────────────────────────────────────────────────────────


@pytest.fixture()
def backend() -> FakeBackend:
    return FakeBackend()


@pytest.fixture()
def server(backend: FakeBackend):
    return create_server(backend, default_user="test_user")


async def call(server, tool: str, **kwargs) -> str:
    """Helper: call a tool and return the text content."""
    contents, _ = await server.call_tool(tool, kwargs)
    return contents[0].text


# ── Basic CRUD tests ──────────────────────────────────────────────────


@pytest.mark.asyncio
async def test_store_and_retrieve(server):
    await call(server, "memory_store", content="I prefer Python over Java")
    result = await call(server, "memory_retrieve", query="Python")
    assert "Python" in result


@pytest.mark.asyncio
async def test_correct(server, backend):
    stored = backend.store("test_user", "old content", "semantic", None)
    mid = stored["memory_id"]
    result = await call(
        server,
        "memory_correct",
        memory_id=mid,
        new_content="new content",
        reason="update",
    )
    assert "new content" in result


@pytest.mark.asyncio
async def test_purge_by_id(server, backend):
    stored = backend.store("test_user", "to delete", "semantic", None)
    mid = stored["memory_id"]
    await call(server, "memory_purge", memory_id=mid, reason="cleanup")
    assert mid not in backend._memories


@pytest.mark.asyncio
async def test_purge_by_topic(server, backend):
    backend.store("test_user", "python tip 1", "semantic", None)
    backend.store("test_user", "python tip 2", "semantic", None)
    backend.store("test_user", "unrelated memory", "semantic", None)
    await call(server, "memory_purge", topic="python", reason="cleanup")
    remaining = [
        r for r in backend._memories.values() if "python" in r["content"].lower()
    ]
    assert len(remaining) == 0


@pytest.mark.asyncio
async def test_search(server, backend):
    backend.store("test_user", "matrixone database", "semantic", None)
    result = await call(server, "memory_search", query="matrixone")
    assert "matrixone" in result.lower()


@pytest.mark.asyncio
async def test_profile(server, backend):
    backend.store("test_user", "fact 1", "semantic", None)
    backend.store("test_user", "fact 2", "semantic", None)
    result = await call(server, "memory_profile")
    assert result  # non-empty response


# ── Snapshot / branch tests ───────────────────────────────────────────


@pytest.mark.asyncio
async def test_snapshot_lifecycle(server, backend):
    await call(server, "memory_snapshot", name="snap1", description="test snapshot")
    assert "snap1" in backend._snapshots

    result = await call(server, "memory_snapshots")
    assert "snap1" in result

    result = await call(server, "memory_rollback", name="snap1")
    assert "snap1" in result


@pytest.mark.asyncio
async def test_branch_lifecycle(server, backend):
    await call(server, "memory_branch", name="exp1")
    assert "exp1" in backend._branches

    result = await call(server, "memory_branches")
    assert "exp1" in result

    await call(server, "memory_checkout", name="exp1")
    assert backend._active_branch == "exp1"

    await call(server, "memory_merge", source="exp1", strategy="append")
    await call(server, "memory_branch_delete", name="exp1")
    assert "exp1" not in backend._branches


# ── Concurrent tests ──────────────────────────────────────────────────


@pytest.mark.asyncio
async def test_concurrent_store(server, backend):
    """50 concurrent store calls must all succeed without data loss."""
    n = 50
    tasks = [
        call(server, "memory_store", content=f"concurrent memory {i}") for i in range(n)
    ]
    results = await asyncio.gather(*tasks)
    assert len(results) == n
    assert all("Stored memory" in r for r in results)
    assert len(backend._memories) == n


@pytest.mark.asyncio
async def test_concurrent_store_and_retrieve(server, backend):
    """Interleaved stores and retrieves must not deadlock or corrupt state."""
    store_tasks = [
        call(server, "memory_store", content=f"fact about topic_{i % 5}")
        for i in range(20)
    ]
    retrieve_tasks = [
        call(server, "memory_retrieve", query=f"topic_{i % 5}") for i in range(20)
    ]
    results = await asyncio.gather(*store_tasks, *retrieve_tasks)
    assert len(results) == 40


@pytest.mark.asyncio
async def test_concurrent_mixed_operations(server, backend):
    """Store, retrieve, search, profile all running concurrently."""
    # Pre-populate
    for i in range(5):
        backend.store("test_user", f"pre-stored item {i}", "semantic", None)

    tasks = (
        [call(server, "memory_store", content=f"new item {i}") for i in range(10)]
        + [call(server, "memory_retrieve", query="item") for _ in range(10)]
        + [call(server, "memory_search", query="pre-stored") for _ in range(10)]
        + [call(server, "memory_profile") for _ in range(5)]
    )
    results = await asyncio.gather(*tasks)
    assert len(results) == 35
    assert all(r is not None for r in results)


@pytest.mark.asyncio
async def test_concurrent_snapshot_creation(server, backend):
    """Multiple snapshots created concurrently must all be persisted."""
    tasks = [
        call(server, "memory_snapshot", name=f"snap_{i}", description=f"snapshot {i}")
        for i in range(10)
    ]
    await asyncio.gather(*tasks)
    assert len(backend._snapshots) == 10


@pytest.mark.asyncio
async def test_concurrent_purge_no_double_delete(server, backend):
    """Concurrent purges on the same memory_id must not raise errors."""
    stored = backend.store("test_user", "to be purged", "semantic", None)
    mid = stored["memory_id"]

    # Fire 5 concurrent purges for the same ID — only one should delete, rest are no-ops
    tasks = [
        call(server, "memory_purge", memory_id=mid, reason="concurrent")
        for _ in range(5)
    ]
    await asyncio.gather(*tasks)
    assert mid not in backend._memories


# ── Maintenance tool MCP tests ────────────────────────────────────────


@pytest.mark.asyncio
async def test_governance(server):
    result = await call(server, "memory_governance")
    assert "Governance done" in result
    assert "quarantined=1" in result
    assert "cleaned_stale=2" in result


@pytest.mark.asyncio
async def test_consolidate(server):
    result = await call(server, "memory_consolidate")
    assert "Consolidation done" in result
    assert "merged_nodes=1" in result


@pytest.mark.asyncio
async def test_reflect_internal(server):
    """Reflect with internal mode returns done message."""
    result = await call(server, "memory_reflect", mode="internal")
    assert "Reflection done" in result
    assert "scenes_created=1" in result


@pytest.mark.asyncio
async def test_reflect_candidates(server, backend):
    """Reflect with candidates mode returns clusters when memories exist."""
    backend.store("test_user", "I prefer pytest", "profile", None)
    backend.store("test_user", "Always use black formatter", "profile", None)
    result = await call(server, "memory_reflect", mode="candidates")
    assert "Cluster 1" in result
    assert "preference_cluster" in result


@pytest.mark.asyncio
async def test_reflect_candidates_empty(server):
    """Reflect with candidates mode and no memories returns appropriate message."""
    result = await call(server, "memory_reflect", mode="candidates")
    assert "No reflection candidates" in result


@pytest.mark.asyncio
async def test_extract_entities_internal(server):
    result = await call(server, "memory_extract_entities", mode="internal")
    assert '"status": "done"' in result or '"status":"done"' in result
    assert '"entities_found": 2' in result or '"entities_found":2' in result


@pytest.mark.asyncio
async def test_extract_entities_candidates(server, backend):
    backend.store("test_user", "Uses Python 3.11 with MatrixOne", "semantic", None)
    result = await call(server, "memory_extract_entities", mode="candidates")
    assert '"status": "candidates"' in result or '"status":"candidates"' in result
    assert "memory_id" in result


@pytest.mark.asyncio
async def test_extract_entities_candidates_empty(server):
    result = await call(server, "memory_extract_entities", mode="candidates")
    assert '"status": "complete"' in result or '"status":"complete"' in result


@pytest.mark.asyncio
async def test_link_entities(server, backend):
    stored = backend.store("test_user", "Uses Python with MatrixOne", "semantic", None)
    mid = stored["memory_id"]
    import json

    entities_json = json.dumps(
        [
            {
                "memory_id": mid,
                "entities": [
                    {"name": "python", "type": "tech"},
                    {"name": "matrixone", "type": "database"},
                ],
            }
        ]
    )
    result = await call(server, "memory_link_entities", entities=entities_json)
    assert '"status": "done"' in result or '"status":"done"' in result
    assert '"entities_created": 2' in result or '"entities_created":2' in result
    assert '"edges_created": 2' in result or '"edges_created":2' in result


@pytest.mark.asyncio
async def test_link_entities_invalid_json(server):
    result = await call(server, "memory_link_entities", entities="not json")
    assert '"status": "error"' in result or '"status":"error"' in result
    assert "Invalid JSON" in result


@pytest.mark.asyncio
async def test_link_entities_invalid_format(server):
    import json

    result = await call(
        server, "memory_link_entities", entities=json.dumps([{"no_memory_id": True}])
    )
    assert '"status": "error"' in result or '"status":"error"' in result
    assert "Invalid format" in result


@pytest.mark.asyncio
async def test_rebuild_index(server):
    result = await call(server, "memory_rebuild_index", table="mem_memories")
    assert "Rebuilt IVF index" in result
    assert "mem_memories" in result


@pytest.mark.asyncio
async def test_capabilities(server):
    result = await call(server, "memory_capabilities")
    import json

    data = json.loads(result)
    assert data["mode"] == "embedded"
    assert "memory_store" in data["tools"]
    assert "memory_governance" in data["tools"]
    assert "memory_branch" in data["tools"]
    assert "memory_capabilities" in data["tools"]


@pytest.mark.asyncio
async def test_cooldown_cache_not_polluted(server, backend):
    """Verify that handler mutations (e.g., pop) don't pollute the cooldown cache.

    Regression test for: governance handler calls result.pop("vector_index_health")
    which was modifying the cached object. Second call would lose vector_index_health.
    """
    # Test _with_cooldown directly with a mock function that returns a dict
    # with nested structure (like governance result)

    call_count = 0

    def mock_fn():
        nonlocal call_count
        call_count += 1
        return {
            "quarantined": 1,
            "cleaned_stale": 2,
            "vector_index_health": {"mem_memories": {"needs_rebuild": True}},
        }

    # First call — should execute fn and cache result
    result1 = backend._with_cooldown("test_user", "governance", mock_fn)
    assert call_count == 1
    assert "vector_index_health" in result1

    # Simulate what the handler does: pop vector_index_health
    health = result1.pop("vector_index_health", {})
    assert health == {"mem_memories": {"needs_rebuild": True}}
    assert "vector_index_health" not in result1  # Handler removed it

    # Second call within cooldown — should return cached copy
    result2 = backend._with_cooldown("test_user", "governance", mock_fn)
    assert call_count == 1  # fn not called again (still in cooldown)
    assert result2.get("skipped") is True
    assert "cooldown_remaining_s" in result2

    # CRITICAL: Cached result should still have vector_index_health
    # (not polluted by the pop() we did on result1)
    key = ("test_user", "governance")
    ts, cached_result = backend._cooldown_cache[key]
    assert "vector_index_health" in cached_result, (
        "Cache was polluted: vector_index_health was removed by handler's pop()"
    )
    assert cached_result["vector_index_health"] == {
        "mem_memories": {"needs_rebuild": True}
    }
