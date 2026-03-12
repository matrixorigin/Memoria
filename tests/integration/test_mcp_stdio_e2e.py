"""True end-to-end MCP tests.

Spawns a real MCP server subprocess (stdio transport), communicates via the
MCP ClientSession protocol, and verifies results against the real MatrixOne DB.

Also includes benchmark tests that measure throughput and latency.

Usage:
    # Run all e2e tests (requires MatrixOne at 10.222.1.57:6001)
    pytest tests/e2e/test_mcp_stdio_e2e.py -v

    # Run only benchmarks
    pytest tests/e2e/test_mcp_stdio_e2e.py -v -k benchmark

    # Run with verbose benchmark output
    pytest tests/e2e/test_mcp_stdio_e2e.py -v -k benchmark -s
"""

from __future__ import annotations

import asyncio
import os
import sys
import time
import uuid
from contextlib import asynccontextmanager
from typing import AsyncGenerator

import pymysql
import pytest
import pytest_asyncio
from mcp import ClientSession
from mcp.client.stdio import StdioServerParameters, stdio_client

pytestmark = [pytest.mark.slow, pytest.mark.xdist_group("mcp_stdio")]

# ── Config ────────────────────────────────────────────────────────────

DB_URL = os.environ.get(
    "MCP_E2E_DB_URL",
    "mysql+pymysql://root:111@localhost:6001/memoria_e2e_test",
)
_DB_HOST = os.environ.get("MEMORIA_DB_HOST", "localhost")
_DB_PORT = 6001


def _server_env() -> dict[str, str]:
    """Build subprocess env for MCP server. Uses remote embedding if configured, else local."""
    provider = os.environ.get("EMBEDDING_PROVIDER", "mock")
    if provider not in ("local", "mock"):
        return {
            **os.environ,
            "EMBEDDING_PROVIDER": provider,
            "EMBEDDING_MODEL": os.environ.get("EMBEDDING_MODEL", ""),
            "EMBEDDING_DIM": os.environ.get("EMBEDDING_DIM", "1024"),
        }
    return {
        **os.environ,
        "TRANSFORMERS_OFFLINE": "1",
        "HF_DATASETS_OFFLINE": "1",
        "EMBEDDING_PROVIDER": provider,
        "EMBEDDING_MODEL": os.environ.get("EMBEDDING_MODEL", "BAAI/bge-m3"),
        "EMBEDDING_DIM": os.environ.get("EMBEDDING_DIM", "1024"),
    }


_DB_USER = "root"
_DB_PASS = "111"
_DB_NAME = "memoria_e2e_test"

# Unique prefix per test run so parallel runs don't collide
_RUN_ID = uuid.uuid4().hex[:8]


def _user(suffix: str = "") -> str:
    return f"e2e_{_RUN_ID}_{suffix}" if suffix else f"e2e_{_RUN_ID}"


# ── DB helpers ────────────────────────────────────────────────────────


def _db_conn():
    return pymysql.connect(
        host=_DB_HOST,
        port=_DB_PORT,
        user=_DB_USER,
        password=_DB_PASS,
        database=_DB_NAME,
        autocommit=True,
    )


def _db_count(table: str, where: str = "1=1") -> int:
    conn = _db_conn()
    try:
        cur = conn.cursor()
        cur.execute(f"SELECT COUNT(*) FROM {table} WHERE {where}")
        return cur.fetchone()[0]
    finally:
        conn.close()


def _db_fetch(table: str, where: str) -> list[dict]:
    conn = _db_conn()
    try:
        cur = conn.cursor(pymysql.cursors.DictCursor)
        cur.execute(f"SELECT * FROM {table} WHERE {where}")
        return cur.fetchall()
    finally:
        conn.close()


def _db_cleanup(user_id_prefix: str) -> None:
    """Delete all test data for this run."""
    conn = _db_conn()
    try:
        cur = conn.cursor()
        cur.execute(f"DELETE FROM mem_memories WHERE user_id LIKE '{user_id_prefix}%'")
        cur.execute(f"DELETE FROM mem_edit_log WHERE user_id LIKE '{user_id_prefix}%'")
        cur.execute(
            f"DELETE FROM mem_user_state WHERE user_id LIKE '{user_id_prefix}%'"
        )
        conn.commit()
    finally:
        conn.close()


# ── Server fixture ────────────────────────────────────────────────────


@asynccontextmanager
async def _mcp_session(user: str) -> AsyncGenerator[ClientSession, None]:
    """Spawn a real MCP server subprocess and return a connected ClientSession."""
    params = StdioServerParameters(
        command=sys.executable,
        args=["-m", "memoria.mcp_local.server", "--db-url", DB_URL, "--user", user],
        env=_server_env(),
    )
    async with stdio_client(params) as (r, w):
        async with ClientSession(r, w) as session:
            await session.initialize()
            yield session


async def _call(session: ClientSession, tool: str, **kwargs) -> str:
    result = await session.call_tool(tool, kwargs)
    return result.content[0].text


# ── Fixtures ──────────────────────────────────────────────────────────


@pytest_asyncio.fixture()
async def session():
    """Single MCP session for one test, auto-cleaned.

    Uses a dedicated event loop per fixture to avoid anyio cancel-scope
    cross-task teardown errors (known pytest-asyncio + anyio issue).
    """
    user = _user("main")
    # We can't yield inside an asynccontextmanager across anyio task boundaries,
    # so we manage the lifecycle manually.
    params = StdioServerParameters(
        command=sys.executable,
        args=["-m", "memoria.mcp_local.server", "--db-url", DB_URL, "--user", user],
        env=_server_env(),
    )

    # Use a queue to pass the session out of the background task
    session_ready: asyncio.Queue = asyncio.Queue()
    done: asyncio.Event = asyncio.Event()

    async def _run():
        async with stdio_client(params) as (r, w):
            async with ClientSession(r, w) as s:
                await s.initialize()
                await session_ready.put(s)
                await done.wait()

    task = asyncio.create_task(_run())
    s = await session_ready.get()
    try:
        yield s
    finally:
        done.set()
        try:
            await asyncio.wait_for(task, timeout=10)
        except Exception:
            pass
        _db_cleanup(f"e2e_{_RUN_ID}")


# ── Tool listing ──────────────────────────────────────────────────────


@pytest.mark.asyncio
async def test_list_tools(session):
    tools = await session.list_tools()
    names = {t.name for t in tools.tools}
    expected = {
        "memory_store",
        "memory_retrieve",
        "memory_correct",
        "memory_purge",
        "memory_search",
        "memory_profile",
        "memory_snapshot",
        "memory_snapshots",
        "memory_rollback",
        "memory_branch",
        "memory_branches",
        "memory_checkout",
        "memory_merge",
        "memory_diff",
        "memory_branch_delete",
    }
    assert expected.issubset(names), f"Missing tools: {expected - names}"


# ── CRUD: store → DB verify ───────────────────────────────────────────


@pytest.mark.asyncio
async def test_store_persists_to_db(session):
    user = _user("main")
    content = f"unique fact {uuid.uuid4().hex}"

    result = await _call(session, "memory_store", content=content, user_id=user)
    assert "Stored memory" in result

    # Extract memory_id from response "Stored memory <id>: <content>"
    mid = result.split("Stored memory ")[1].split(":")[0].strip()

    # Ground truth: verify every field in DB
    rows = _db_fetch("mem_memories", f"memory_id = '{mid}'")
    assert len(rows) == 1, "Memory not persisted to DB"
    row = rows[0]
    assert row["user_id"] == user
    assert row["content"] == content
    assert row["memory_type"] == "semantic"
    assert row["memory_id"] == mid


@pytest.mark.asyncio
async def test_store_with_session_id(session):
    user = _user("main")
    sess_id = f"sess_{uuid.uuid4().hex[:8]}"
    content = f"session-scoped fact {uuid.uuid4().hex}"

    result = await _call(
        session, "memory_store", content=content, user_id=user, session_id=sess_id
    )
    mid = result.split("Stored memory ")[1].split(":")[0].strip()

    rows = _db_fetch("mem_memories", f"memory_id = '{mid}'")
    assert rows[0]["session_id"] == sess_id


@pytest.mark.asyncio
async def test_retrieve_returns_stored(session):
    user = _user("retrieve")  # isolated user
    keyword = f"kw_{uuid.uuid4().hex[:6]}"
    await _call(
        session, "memory_store", content=f"memory about {keyword}", user_id=user
    )

    result = await _call(
        session, "memory_retrieve", query=keyword, user_id=user, top_k=50
    )
    assert keyword in result


@pytest.mark.asyncio
async def test_correct_updates_db(session):
    """correct() supersedes old memory (deactivate) and creates a new one."""
    user = _user("main")
    result = await _call(
        session, "memory_store", content="original content", user_id=user
    )
    old_mid = result.split("Stored memory ")[1].split(":")[0].strip()

    correct_result = await _call(
        session,
        "memory_correct",
        memory_id=old_mid,
        new_content="corrected content",
        reason="test",
        user_id=user,
    )
    # Response: "Corrected → <new_id>: <content>"
    new_mid = correct_result.split("Corrected → ")[1].split(":")[0].strip()

    # Old memory should be deactivated (is_active=0)
    old_rows = _db_fetch("mem_memories", f"memory_id = '{old_mid}'")
    assert old_rows[0]["is_active"] == 0, "Old memory should be deactivated"

    # New memory should have corrected content
    new_rows = _db_fetch("mem_memories", f"memory_id = '{new_mid}'")
    assert len(new_rows) == 1
    assert new_rows[0]["content"] == "corrected content"
    assert new_rows[0]["is_active"] == 1


@pytest.mark.asyncio
async def test_purge_removes_from_db(session):
    """purge() soft-deletes: sets is_active=0, row still exists."""
    user = _user("main")
    result = await _call(session, "memory_store", content="to be purged", user_id=user)
    mid = result.split("Stored memory ")[1].split(":")[0].strip()

    await _call(session, "memory_purge", memory_id=mid, reason="test", user_id=user)

    rows = _db_fetch("mem_memories", f"memory_id = '{mid}'")
    assert len(rows) == 1, "Row should still exist (soft delete)"
    assert rows[0]["is_active"] == 0, "Memory should be deactivated"


@pytest.mark.asyncio
async def test_purge_by_topic(session):
    """purge by topic soft-deletes all matching active memories."""
    user = _user("main")
    topic_kw = f"topic_{uuid.uuid4().hex[:6]}"
    for i in range(3):
        await _call(
            session, "memory_store", content=f"{topic_kw} item {i}", user_id=user
        )
    await _call(session, "memory_store", content="unrelated memory", user_id=user)

    await _call(
        session, "memory_purge", topic=topic_kw, reason="bulk delete", user_id=user
    )

    # All topic memories should be deactivated
    active = _db_fetch(
        "mem_memories",
        f"user_id = '{user}' AND content LIKE '%{topic_kw}%' AND is_active",
    )
    assert len(active) == 0, "All topic memories should be deactivated"

    # Unrelated memory should still be active
    unrelated = _db_fetch(
        "mem_memories",
        f"user_id = '{user}' AND content = 'unrelated memory' AND is_active",
    )
    assert len(unrelated) == 1


@pytest.mark.asyncio
async def test_search(session):
    user = _user("search")  # isolated user to avoid cross-test interference
    kw = f"searchkw_{uuid.uuid4().hex[:6]}"
    await _call(session, "memory_store", content=f"document about {kw}", user_id=user)

    result = await _call(session, "memory_search", query=kw, user_id=user, top_k=50)
    assert kw in result


@pytest.mark.asyncio
async def test_profile(session):
    user = _user("main")
    for i in range(3):
        await _call(session, "memory_store", content=f"profile fact {i}", user_id=user)

    result = await _call(session, "memory_profile", user_id=user)
    assert result  # non-empty


# ── Snapshot lifecycle ────────────────────────────────────────────────


@pytest.mark.asyncio
async def test_snapshot_create_list_rollback(session):
    user = _user("main")
    snap_name = f"snap_{uuid.uuid4().hex[:6]}"

    await _call(session, "memory_store", content="before snapshot", user_id=user)
    await _call(
        session,
        "memory_snapshot",
        name=snap_name,
        description="test snap",
        user_id=user,
    )

    snaps = await _call(session, "memory_snapshots", user_id=user)
    assert snap_name in snaps

    result = await _call(session, "memory_rollback", name=snap_name, user_id=user)
    assert snap_name in result


# ── Branch lifecycle ──────────────────────────────────────────────────


@pytest.mark.asyncio
async def test_branch_full_lifecycle(session):
    user = _user("main")
    branch_name = f"br_{uuid.uuid4().hex[:6]}"

    await _call(session, "memory_branch", name=branch_name, user_id=user)

    branches = await _call(session, "memory_branches", user_id=user)
    assert branch_name in branches

    await _call(session, "memory_checkout", name=branch_name, user_id=user)
    await _call(session, "memory_store", content="branch-only memory", user_id=user)

    diff = await _call(session, "memory_diff", source=branch_name, user_id=user)
    assert diff  # non-empty diff

    await _call(
        session, "memory_merge", source=branch_name, strategy="append", user_id=user
    )
    await _call(session, "memory_checkout", name="main", user_id=user)
    await _call(session, "memory_branch_delete", name=branch_name, user_id=user)

    branches_after = await _call(session, "memory_branches", user_id=user)
    assert branch_name not in branches_after


# ── Concurrent tests (real subprocess) ───────────────────────────────


@pytest.mark.asyncio
async def test_concurrent_store_single_session(session):
    """50 concurrent store calls on one session — no deadlock, all succeed."""
    user = _user("main")
    n = 50
    tasks = [
        _call(session, "memory_store", content=f"concurrent item {i}", user_id=user)
        for i in range(n)
    ]
    results = await asyncio.gather(*tasks)
    assert all("Stored memory" in r for r in results)

    count = _db_count(
        "mem_memories", f"user_id = '{user}' AND content LIKE 'concurrent item%'"
    )
    assert count == n, f"Expected {n} rows in DB, got {count}"


@pytest.mark.asyncio
async def test_concurrent_multi_session():
    """5 independent MCP server processes running concurrently."""
    users = [_user(f"concurrent_{i}") for i in range(5)]

    async def run_one(user: str) -> int:
        async with _mcp_session(user) as s:
            for j in range(10):
                await _call(s, "memory_store", content=f"item {j}", user_id=user)
            result = await _call(s, "memory_retrieve", query="item", user_id=user)
            return result.count("item")

    counts = await asyncio.gather(*[run_one(u) for u in users])
    assert all(c > 0 for c in counts)

    # Verify DB isolation: each user has exactly 10 rows
    for user in users:
        count = _db_count("mem_memories", f"user_id = '{user}'")
        assert count == 10, f"User {user}: expected 10, got {count}"

    _db_cleanup(f"e2e_{_RUN_ID}")


@pytest.mark.asyncio
async def test_concurrent_store_retrieve_interleaved(session):
    """Interleaved stores and retrieves — no corruption."""
    user = _user("main")
    kw = f"interleave_{uuid.uuid4().hex[:6]}"

    store_tasks = [
        _call(session, "memory_store", content=f"{kw} fact {i}", user_id=user)
        for i in range(20)
    ]
    retrieve_tasks = [
        _call(session, "memory_retrieve", query=kw, user_id=user) for _ in range(10)
    ]
    results = await asyncio.gather(*store_tasks, *retrieve_tasks)
    assert len(results) == 30
    assert all(r is not None for r in results)


# ── Benchmark tests ───────────────────────────────────────────────────


@pytest.mark.asyncio
@pytest.mark.benchmark
async def test_benchmark_sequential_store(session, capsys):
    """Measure sequential store latency (p50, p95, p99)."""
    user = _user("bench_seq")
    n = 30
    latencies: list[float] = []

    for i in range(n):
        t0 = time.perf_counter()
        await _call(
            session,
            "memory_store",
            content=f"bench item {i} {uuid.uuid4().hex}",
            user_id=user,
        )
        latencies.append(time.perf_counter() - t0)

    latencies.sort()
    p50 = latencies[int(n * 0.50)]
    p95 = latencies[int(n * 0.95)]
    p99 = latencies[int(n * 0.99)]
    avg = sum(latencies) / n

    with capsys.disabled():
        print(f"\n[benchmark] sequential store (n={n})")
        print(
            f"  avg={avg * 1000:.1f}ms  p50={p50 * 1000:.1f}ms  p95={p95 * 1000:.1f}ms  p99={p99 * 1000:.1f}ms"
        )

    # Sanity: p99 should be under 10s (network + DB + embedding)
    assert p99 < 10.0, f"p99 latency too high: {p99:.2f}s"


@pytest.mark.asyncio
@pytest.mark.benchmark
async def test_benchmark_concurrent_store_throughput(session, capsys):
    """Measure concurrent store throughput (ops/sec)."""
    user = _user("bench_conc")
    n = 50

    t0 = time.perf_counter()
    tasks = [
        _call(
            session,
            "memory_store",
            content=f"bench concurrent {i} {uuid.uuid4().hex}",
            user_id=user,
        )
        for i in range(n)
    ]
    results = await asyncio.gather(*tasks)
    elapsed = time.perf_counter() - t0

    throughput = n / elapsed
    assert all("Stored memory" in r for r in results)

    with capsys.disabled():
        print(f"\n[benchmark] concurrent store (n={n})")
        print(f"  total={elapsed:.2f}s  throughput={throughput:.1f} ops/sec")

    assert throughput > 1.0, f"Throughput too low: {throughput:.1f} ops/sec"


@pytest.mark.asyncio
@pytest.mark.benchmark
async def test_benchmark_retrieve_latency(session, capsys):
    """Measure retrieve latency after pre-populating memories."""
    user = _user("bench_ret")
    kw = f"retrieval_bench_{uuid.uuid4().hex[:6]}"

    # Pre-populate
    for i in range(20):
        await _call(session, "memory_store", content=f"{kw} document {i}", user_id=user)

    # Measure retrieve
    n = 20
    latencies: list[float] = []
    for _ in range(n):
        t0 = time.perf_counter()
        await _call(session, "memory_retrieve", query=kw, user_id=user, top_k=5)
        latencies.append(time.perf_counter() - t0)

    latencies.sort()
    p50 = latencies[int(n * 0.50)]
    p95 = latencies[int(n * 0.95)]
    avg = sum(latencies) / n

    with capsys.disabled():
        print(f"\n[benchmark] retrieve latency (n={n}, corpus=20)")
        print(
            f"  avg={avg * 1000:.1f}ms  p50={p50 * 1000:.1f}ms  p95={p95 * 1000:.1f}ms"
        )

    assert p95 < 10.0, f"p95 retrieve latency too high: {p95:.2f}s"


@pytest.mark.asyncio
@pytest.mark.benchmark
async def test_benchmark_mixed_workload(session, capsys):
    """Simulate realistic mixed workload: 60% retrieve, 30% store, 10% search."""
    user = _user("bench_mixed")
    kw = f"mixed_{uuid.uuid4().hex[:6]}"

    # Seed data
    for i in range(10):
        await _call(session, "memory_store", content=f"{kw} seed {i}", user_id=user)

    n = 60
    ops = (
        [("memory_retrieve", {"query": kw, "user_id": user})] * 36  # 60%
        + [
            ("memory_store", {"content": f"{kw} new {i}", "user_id": user})
            for i in range(18)
        ]  # 30%
        + [("memory_search", {"query": kw, "user_id": user})] * 6  # 10%
    )
    # Shuffle deterministically
    import random

    random.seed(42)
    random.shuffle(ops)

    t0 = time.perf_counter()
    tasks = [_call(session, tool, **args) for tool, args in ops]
    results = await asyncio.gather(*tasks)
    elapsed = time.perf_counter() - t0

    throughput = n / elapsed
    with capsys.disabled():
        print(f"\n[benchmark] mixed workload (n={n}, 60/30/10 retrieve/store/search)")
        print(f"  total={elapsed:.2f}s  throughput={throughput:.1f} ops/sec")

    assert len(results) == n
    assert throughput > 1.0
