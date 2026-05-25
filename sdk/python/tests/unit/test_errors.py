"""Unit tests for error mapping and retry logic."""

from __future__ import annotations

import pytest
from pytest_httpx import HTTPXMock

from memoria import MemoriaClient, MemoriaServerError, MemoriaConnectionError
from memoria.exceptions import MemoriaAPIError
from tests.conftest import BASE_URL, API_KEY, MEMORY_STUB


@pytest.fixture
def client_no_retry() -> MemoriaClient:
    return MemoriaClient(base_url=BASE_URL, api_key=API_KEY, max_retries=0)


@pytest.fixture
def client_with_retry() -> MemoriaClient:
    return MemoriaClient(base_url=BASE_URL, api_key=API_KEY, max_retries=2)


def test_500_raises_server_error(httpx_mock: HTTPXMock, client_no_retry: MemoriaClient) -> None:
    httpx_mock.add_response(status_code=500, json={"detail": "internal error"})
    with pytest.raises(MemoriaServerError) as exc:
        client_no_retry.memories.store(content="x")
    assert exc.value.status_code == 500


def test_500_retried_then_succeeds(
    httpx_mock: HTTPXMock, client_with_retry: MemoriaClient
) -> None:
    httpx_mock.add_response(status_code=500, json={"detail": "transient"})
    httpx_mock.add_response(status_code=500, json={"detail": "transient"})
    httpx_mock.add_response(json=MEMORY_STUB)
    mem = client_with_retry.memories.store(content="x")
    assert mem.memory_id == "mem_abc123"


def test_500_exhausted_retries_raises(
    httpx_mock: HTTPXMock, client_with_retry: MemoriaClient
) -> None:
    for _ in range(3):  # max_retries=2 → 3 total attempts
        httpx_mock.add_response(status_code=500, json={"detail": "error"})
    with pytest.raises(MemoriaServerError):
        client_with_retry.memories.store(content="x")


def test_unknown_4xx_raises_api_error(
    httpx_mock: HTTPXMock, client_no_retry: MemoriaClient
) -> None:
    httpx_mock.add_response(status_code=410, json={"detail": "gone"})
    with pytest.raises(MemoriaAPIError) as exc:
        client_no_retry.memories.store(content="x")
    assert exc.value.status_code == 410


def test_exception_str_includes_status_and_detail() -> None:
    err = MemoriaAPIError(422, "content is empty")
    assert "422" in str(err)
    assert "content is empty" in str(err)


def test_ping_success(httpx_mock: HTTPXMock, client_no_retry: MemoriaClient) -> None:
    httpx_mock.add_response(status_code=200, json={"status": "ok"})
    assert client_no_retry.ping() is True
