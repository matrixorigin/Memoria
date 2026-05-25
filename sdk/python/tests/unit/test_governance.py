"""Unit tests for GovernanceResource (sync)."""

from __future__ import annotations

import pytest
from pytest_httpx import HTTPXMock

from memoria import MemoriaClient
from memoria.models import GovernanceResult
from tests.conftest import BASE_URL, API_KEY


@pytest.fixture
def client() -> MemoriaClient:
    return MemoriaClient(base_url=BASE_URL, api_key=API_KEY, max_retries=0)


def test_governance_run_executes(httpx_mock: HTTPXMock, client: MemoriaClient) -> None:
    httpx_mock.add_response(
        json={"skipped": False, "quarantined": 2, "cleaned_stale": 5, "orphan_graph_cleaned": 0}
    )
    result = client.governance.run()
    assert isinstance(result, GovernanceResult)
    assert result.skipped is False
    assert result.cleaned_stale == 5


def test_governance_run_skipped_on_cooldown(
    httpx_mock: HTTPXMock, client: MemoriaClient
) -> None:
    httpx_mock.add_response(json={"skipped": True, "cooldown_remaining_s": 3540})
    result = client.governance.run()
    assert result.skipped is True
    assert result.cooldown_remaining_s == 3540


def test_governance_run_force(httpx_mock: HTTPXMock, client: MemoriaClient) -> None:
    httpx_mock.add_response(
        json={"skipped": False, "quarantined": 0, "cleaned_stale": 1, "orphan_graph_cleaned": 0}
    )
    result = client.governance.run(force=True)
    assert result.skipped is False
    req = httpx_mock.get_request()
    assert req is not None
    import json
    body = json.loads(req.content)
    assert body.get("force") is True


def test_consolidate_executes(httpx_mock: HTTPXMock, client: MemoriaClient) -> None:
    httpx_mock.add_response(
        json={
            "skipped": False,
            "status": "done",
            "conflicts_detected": 3,
            "promoted": 1,
            "demoted": 0,
        }
    )
    result = client.governance.consolidate()
    assert result.conflicts_detected == 3
    assert result.status == "done"


def test_consolidate_skipped(httpx_mock: HTTPXMock, client: MemoriaClient) -> None:
    httpx_mock.add_response(json={"skipped": True, "cooldown_remaining_s": 1200})
    result = client.governance.consolidate()
    assert result.skipped is True


def test_reflect_auto_executes(httpx_mock: HTTPXMock, client: MemoriaClient) -> None:
    httpx_mock.add_response(json={"skipped": False, "scenes_created": 2, "candidates_found": 4})
    result = client.governance.reflect()
    assert result.scenes_created == 2


def test_reflect_auto_skipped(httpx_mock: HTTPXMock, client: MemoriaClient) -> None:
    httpx_mock.add_response(json={"skipped": True, "cooldown_remaining_s": 7200})
    result = client.governance.reflect()
    assert result.skipped is True


def test_reflect_candidates_mode_never_skipped(
    httpx_mock: HTTPXMock, client: MemoriaClient
) -> None:
    """mode='candidates' never has a cooldown."""
    httpx_mock.add_response(
        json={"skipped": False, "candidates": [{"id": "c1"}, {"id": "c2"}]}
    )
    result = client.governance.reflect(mode="candidates")
    assert result.skipped is False
    assert len(result.candidates) == 2
