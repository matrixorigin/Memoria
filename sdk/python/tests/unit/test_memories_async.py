"""Unit tests for AsyncMemoriesResource — mirrors test_memories.py with async/await."""

from __future__ import annotations

import pytest
from pytest_httpx import HTTPXMock

from memoria import AsyncMemoriaClient, MemoriaAuthError, MemoriaValidationError
from memoria.models import Memory, MemoryPage, PurgeResult, RetrieveResult
from tests.conftest import BASE_URL, API_KEY, MEMORY_STUB


@pytest.fixture
def client() -> AsyncMemoriaClient:
    return AsyncMemoriaClient(base_url=BASE_URL, api_key=API_KEY, max_retries=0)


@pytest.mark.asyncio
async def test_store_happy_path(httpx_mock: HTTPXMock, client: AsyncMemoriaClient) -> None:
    httpx_mock.add_response(json=MEMORY_STUB)
    mem = await client.memories.store(content="test content")
    assert isinstance(mem, Memory)
    assert mem.memory_id == "mem_abc123"


@pytest.mark.asyncio
async def test_store_batch_over_limit_raises(client: AsyncMemoriaClient) -> None:
    items = [{"content": f"item {i}"} for i in range(101)]
    with pytest.raises(MemoriaValidationError, match="100"):
        await client.memories.store_batch(items)


@pytest.mark.asyncio
async def test_retrieve_happy_path(httpx_mock: HTTPXMock, client: AsyncMemoriaClient) -> None:
    httpx_mock.add_response(json={"results": [MEMORY_STUB]})
    result = await client.memories.retrieve(query="hello")
    assert isinstance(result, RetrieveResult)
    assert len(result.items) == 1


@pytest.mark.asyncio
async def test_list_happy_path(httpx_mock: HTTPXMock, client: AsyncMemoriaClient) -> None:
    httpx_mock.add_response(json={"items": [MEMORY_STUB], "next_cursor": "cursor_xyz"})
    page = await client.memories.list(limit=10)
    assert isinstance(page, MemoryPage)
    assert page.next_cursor == "cursor_xyz"


@pytest.mark.asyncio
async def test_purge_by_ids(httpx_mock: HTTPXMock, client: AsyncMemoriaClient) -> None:
    httpx_mock.add_response(json={"purged": 1, "snapshot_name": "snap_x"})
    result = await client.memories.purge(memory_ids=["id1"], reason="done")
    assert isinstance(result, PurgeResult)
    assert result.purged == 1


@pytest.mark.asyncio
async def test_401_raises_auth_error(httpx_mock: HTTPXMock, client: AsyncMemoriaClient) -> None:
    httpx_mock.add_response(status_code=401, json={"detail": "rate limit exceeded"})
    with pytest.raises(MemoriaAuthError):
        await client.memories.store(content="x")


@pytest.mark.asyncio
async def test_session_scope_without_session_id_raises(client: AsyncMemoriaClient) -> None:
    with pytest.raises(MemoriaValidationError, match="session_id"):
        await client.memories.search(query="x", session_scope="only")
