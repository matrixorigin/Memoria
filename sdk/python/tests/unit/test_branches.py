"""Unit tests for BranchesResource (sync)."""

from __future__ import annotations

import pytest
from pytest_httpx import HTTPXMock

from memoria import MemoriaClient
from memoria.models import ApplyResult, Branch
from tests.conftest import BASE_URL, API_KEY, BRANCH_STUB


@pytest.fixture
def client() -> MemoriaClient:
    return MemoriaClient(base_url=BASE_URL, api_key=API_KEY, max_retries=0)


def test_create_branch(httpx_mock: HTTPXMock, client: MemoriaClient) -> None:
    httpx_mock.add_response(json=BRANCH_STUB)
    branch = client.branches.create(name="experiment-1")
    assert isinstance(branch, Branch)
    assert branch.name == "experiment-1"
    assert branch.active is False


def test_list_branches(httpx_mock: HTTPXMock, client: MemoriaClient) -> None:
    main_stub = {"name": "main", "active": True}
    # Server returns {"branches": [...], "result": "..."}, not a plain list
    httpx_mock.add_response(json={"branches": [main_stub, BRANCH_STUB], "result": "Branches:\nmain ← active"})
    branches = client.branches.list()
    assert len(branches) == 2
    assert branches[0].active is True


def test_checkout(httpx_mock: HTTPXMock, client: MemoriaClient) -> None:
    httpx_mock.add_response(status_code=200, json={"status": "ok"})
    client.branches.checkout("experiment-1")  # should not raise


def test_diff(httpx_mock: HTTPXMock, client: MemoriaClient) -> None:
    diff_stub = {"added": 3, "removed": 1, "updated": 0}
    httpx_mock.add_response(json=diff_stub)
    result = client.branches.diff("experiment-1")
    assert result["added"] == 3


def test_diff_items(httpx_mock: HTTPXMock, client: MemoriaClient) -> None:
    httpx_mock.add_response(json={"items": [], "next_cursor": None})
    result = client.branches.diff_items("experiment-1", limit=10)
    assert "items" in result


def test_merge(httpx_mock: HTTPXMock, client: MemoriaClient) -> None:
    httpx_mock.add_response(status_code=200, json={"status": "ok"})
    client.branches.merge("experiment-1", strategy="accept")  # should not raise


def test_delete_branch(httpx_mock: HTTPXMock, client: MemoriaClient) -> None:
    httpx_mock.add_response(status_code=204)
    client.branches.delete("experiment-1")


def test_apply(httpx_mock: HTTPXMock, client: MemoriaClient) -> None:
    apply_stub = {
        "applied_adds": ["mem_1"],
        "applied_updates": [],
        "applied_removes": [],
        "applied_conflicts": [],
        "skipped_adds": [],
        "skipped_updates": [],
        "skipped_removes": [],
        "skipped_conflicts": [],
    }
    httpx_mock.add_response(json=apply_stub)
    result = client.branches.apply("experiment-1", adds=["mem_1"])
    assert isinstance(result, ApplyResult)
    assert result.applied_adds == ["mem_1"]


def test_pick(httpx_mock: HTTPXMock, client: MemoriaClient) -> None:
    httpx_mock.add_response(json={"picked": 1, "dry_run": False})
    result = client.branches.pick(
        "experiment-1",
        selector={"type": "key_list", "keys": ["mem_1"]},
    )
    assert result["picked"] == 1
