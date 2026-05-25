"""End-to-end integration tests.

Requires a running Memoria instance.  Set env vars:
  MEMORIA_BASE_URL   — defaults to http://localhost:8100
  MEMORIA_MASTER_KEY — used to create a per-test API key via POST /auth/keys

Run with:
  make python-sdk-test  (from repo root)
or:
  MEMORIA_BASE_URL=... MEMORIA_MASTER_KEY=... pytest tests/integration/ -v

Notes:
  - Tests that call store/retrieve/correct require a valid MEMORIA_EMBEDDING_API_KEY
    in the server config.  If embedding is not configured, those tests are skipped
    via the `embedding_available` fixture rather than failing with HTTP 500.
  - Snapshot / branch names must use underscores; the server normalises dashes to
    underscores, which would break name-equality assertions.
  - The async_client fixture uses scope="function" to avoid event-loop teardown
    errors with pytest-asyncio's default function-scoped event loop.
"""

from __future__ import annotations

import os
import uuid

import httpx
import pytest
import pytest_asyncio

from memoria import AsyncMemoriaClient, Branch, MemoriaClient, MemoriaServerError

BASE_URL = os.environ.get("MEMORIA_BASE_URL", "http://localhost:8100")
MASTER_KEY = os.environ.get("MEMORIA_MASTER_KEY", "")

pytestmark = pytest.mark.skipif(
    not MASTER_KEY,
    reason="MEMORIA_MASTER_KEY not set — skipping integration tests",
)


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


def _create_api_key(base_url: str, master_key: str) -> str:
    """Create a fresh API key via the master key."""
    user_id = f"pytest_{uuid.uuid4().hex[:8]}"
    resp = httpx.post(
        f"{base_url}/auth/keys",
        headers={"Authorization": f"Bearer {master_key}"},
        json={"user_id": user_id, "name": "pytest-sdk-integration"},
        timeout=15,
    )
    resp.raise_for_status()
    return resp.json()["raw_key"]


def _embedding_available(client: MemoriaClient) -> bool:
    """Probe whether the server has a working embedding backend by storing a test memory."""
    try:
        mem = client.memories.store(content="__embedding_probe__", memory_type="working")
        client.memories.delete(mem.memory_id, reason="probe cleanup")
        return True
    except MemoriaServerError as e:
        if "Embedding" in e.detail or "embedding" in e.detail:
            return False
        raise


# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------


@pytest.fixture(scope="module")
def api_key() -> str:
    return _create_api_key(BASE_URL, MASTER_KEY)


@pytest.fixture(scope="module")
def client(api_key: str) -> MemoriaClient:
    return MemoriaClient(base_url=BASE_URL, api_key=api_key)


# async_client uses scope="function" to avoid "event loop is closed" on teardown.
@pytest_asyncio.fixture
async def async_client(api_key: str) -> AsyncMemoriaClient:  # type: ignore[misc]
    async with AsyncMemoriaClient(base_url=BASE_URL, api_key=api_key) as c:
        yield c


@pytest.fixture(scope="module")
def has_embedding(client: MemoriaClient) -> bool:
    return _embedding_available(client)


# ---------------------------------------------------------------------------
# Sync E2E — basic connectivity
# ---------------------------------------------------------------------------


def test_ping(client: MemoriaClient) -> None:
    assert client.ping() is True


def test_list_memories(client: MemoriaClient) -> None:
    page = client.memories.list(limit=10)
    assert page.items is not None


def test_profile_me(client: MemoriaClient) -> None:
    profile = client.profile.me()
    assert profile.user_id


# ---------------------------------------------------------------------------
# Sync E2E — embedding-dependent (skipped if embedding not configured)
# ---------------------------------------------------------------------------


def test_store_retrieve_delete(client: MemoriaClient, has_embedding: bool) -> None:
    if not has_embedding:
        pytest.skip("embedding not configured on this server")
    unique = f"sdk_e2e_{uuid.uuid4().hex[:8]}"
    mem = client.memories.store(content=f"test memory {unique}", memory_type="semantic")
    assert mem.memory_id

    result = client.memories.retrieve(query=unique, top_k=5)
    contents = [m.content for m in result.items]
    assert any(unique in c for c in contents), f"stored content not found in {contents}"

    client.memories.delete(mem.memory_id, reason="e2e cleanup")


def test_correct_memory(client: MemoriaClient, has_embedding: bool) -> None:
    if not has_embedding:
        pytest.skip("embedding not configured on this server")
    mem = client.memories.store(content="original_content_e2e", memory_type="semantic")
    updated = client.memories.correct(mem.memory_id, new_content="corrected_content_e2e")
    assert updated.content == "corrected_content_e2e"
    # correct() creates a new memory with a new ID; delete the updated record, not the original
    client.memories.delete(updated.memory_id, reason="e2e cleanup")


def test_purge_by_session(client: MemoriaClient, has_embedding: bool) -> None:
    if not has_embedding:
        pytest.skip("embedding not configured on this server")
    session_id = f"sess_{uuid.uuid4().hex[:8]}"
    client.memories.store(content="working mem 1", memory_type="working", session_id=session_id)
    client.memories.store(content="working mem 2", memory_type="working", session_id=session_id)
    result = client.memories.purge(
        session_id=session_id, memory_types=["working"], reason="test cleanup"
    )
    assert result.purged >= 2


# ---------------------------------------------------------------------------
# Sync E2E — store_batch (no embedding needed: working type skips embedding)
# ---------------------------------------------------------------------------


def test_store_batch(client: MemoriaClient, has_embedding: bool) -> None:
    if not has_embedding:
        pytest.skip("embedding not configured on this server")
    session_id = f"batch_{uuid.uuid4().hex[:8]}"
    mems = client.memories.store_batch([
        {"content": "batch item 1", "memory_type": "working", "session_id": session_id},
        {"content": "batch item 2", "memory_type": "working", "session_id": session_id},
    ])
    assert len(mems) == 2
    assert all(m.memory_id for m in mems)
    # Cleanup
    client.memories.purge(session_id=session_id, memory_types=["working"], reason="batch e2e cleanup")


# ---------------------------------------------------------------------------
# Sync E2E — snapshots (no embedding needed)
# ---------------------------------------------------------------------------


def test_snapshot_create_list_rollback(client: MemoriaClient) -> None:
    # Use underscores: server normalises dashes → underscores in snapshot names.
    snap_name = f"e2e_snap_{uuid.uuid4().hex[:6]}"
    snap = client.snapshots.create(name=snap_name, description="e2e test")
    assert snap.name == snap_name

    snaps = client.snapshots.list(limit=20)
    names = [s.name for s in snaps]
    assert snap_name in names

    client.snapshots.rollback(snap_name)
    client.snapshots.delete(snap_name)


# ---------------------------------------------------------------------------
# Sync E2E — branches (no embedding needed)
# ---------------------------------------------------------------------------


def test_branch_create_list_checkout_delete(client: MemoriaClient) -> None:
    # Use underscores: server normalises dashes → underscores in branch names.
    branch_name = f"e2e_branch_{uuid.uuid4().hex[:6]}"
    branch = client.branches.create(name=branch_name)
    assert branch.name == branch_name

    # Verify list() parses {"branches": [...], "result": "..."} correctly
    branches = client.branches.list()
    assert any(isinstance(b, Branch) for b in branches)
    names = [b.name for b in branches]
    assert branch_name in names
    assert "main" in names

    client.branches.checkout(name=branch_name)
    client.branches.checkout(name="main")

    client.branches.delete(branch_name)


# ---------------------------------------------------------------------------
# Async E2E
# ---------------------------------------------------------------------------


@pytest.mark.asyncio
async def test_async_ping(async_client: AsyncMemoriaClient) -> None:
    assert await async_client.ping() is True


@pytest.mark.asyncio
async def test_async_store_retrieve(async_client: AsyncMemoriaClient, has_embedding: bool) -> None:
    if not has_embedding:
        pytest.skip("embedding not configured on this server")
    unique = f"async_e2e_{uuid.uuid4().hex[:8]}"
    mem = await async_client.memories.store(content=f"async test {unique}")
    assert mem.memory_id

    result = await async_client.memories.retrieve(query=unique, top_k=5)
    contents = [m.content for m in result.items]
    assert any(unique in c for c in contents)

    await async_client.memories.delete(mem.memory_id, reason="async e2e cleanup")


@pytest.mark.asyncio
async def test_async_governance_run(async_client: AsyncMemoriaClient) -> None:
    result = await async_client.governance.run()
    # Either executed or on cooldown — both are valid outcomes
    assert isinstance(result.skipped, bool)
