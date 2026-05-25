"""Shared test fixtures for unit and integration tests."""

from __future__ import annotations

import pytest
import pytest_asyncio

from memoria import AsyncMemoriaClient, MemoriaClient

BASE_URL = "http://localhost:8100"
API_KEY = "sk-test-key"


@pytest.fixture
def sync_client() -> MemoriaClient:
    """Sync client pointed at a mock/real server."""
    return MemoriaClient(base_url=BASE_URL, api_key=API_KEY, max_retries=0)


@pytest_asyncio.fixture
async def async_client() -> AsyncMemoriaClient:  # type: ignore[misc]
    """Async client pointed at a mock/real server."""
    client = AsyncMemoriaClient(base_url=BASE_URL, api_key=API_KEY, max_retries=0)
    yield client
    await client.aclose()


# ---------------------------------------------------------------------------
# Minimal response stubs reused across multiple test modules
# ---------------------------------------------------------------------------

MEMORY_STUB: dict = {
    "memory_id": "mem_abc123",
    "content": "test content",
    "memory_type": "semantic",
    "trust_tier": "T3",
    "initial_confidence": 0.75,
    "is_active": True,
    "user_id": "user_1",
}

SNAPSHOT_STUB: dict = {
    "name": "snap-1",
    "created_at": "2026-01-01T00:00:00Z",
    "description": "test snapshot",
}

BRANCH_STUB: dict = {
    "name": "experiment-1",
    "active": False,
    "created_at": "2026-01-01T00:00:00Z",
}
