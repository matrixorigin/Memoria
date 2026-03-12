"""Cloud MCP server tests.

Validates that the cloud server's JSON format parameter doesn't break
API compatibility, and that memory_capabilities returns correct mode.
Uses httpx mock transport to avoid real HTTP calls.
"""

from __future__ import annotations

import json

import httpx
import pytest

from memoria.mcp_cloud.server import create_server


# ── Mock HTTP transport ───────────────────────────────────────────────


class MockTransport(httpx.BaseTransport):
    """Returns canned responses for cloud API endpoints."""

    def handle_request(self, request: httpx.Request) -> httpx.Response:
        path = request.url.path
        body = json.loads(request.content) if request.content else {}

        if path == "/v1/memories" and request.method == "POST":
            return httpx.Response(
                200, json={"memory_id": "mem_001", "content": body.get("content", "")}
            )
        if path == "/v1/memories/retrieve":
            return httpx.Response(
                200,
                json=[
                    {
                        "memory_id": "mem_001",
                        "memory_type": "semantic",
                        "content": "test memory",
                    }
                ],
            )
        if path == "/v1/memories/search":
            return httpx.Response(
                200,
                json=[
                    {
                        "memory_id": "mem_001",
                        "memory_type": "semantic",
                        "content": "test memory",
                    }
                ],
            )
        if path.endswith("/correct") and request.method == "PUT":
            return httpx.Response(
                200,
                json={"memory_id": "mem_001", "content": body.get("new_content", "")},
            )
        if path == "/v1/memories/correct" and request.method == "POST":
            return httpx.Response(
                200,
                json={
                    "memory_id": "mem_001",
                    "content": body.get("new_content", ""),
                    "matched_content": "old",
                },
            )
        if path == "/v1/memories/purge" and request.method == "POST":
            ids = body.get("memory_ids", [])
            return httpx.Response(200, json={"purged": len(ids)})
        if request.method == "DELETE":
            return httpx.Response(200, json={"purged": 1})

        return httpx.Response(404, json={"error": "not found"})


@pytest.fixture()
def server():
    srv = create_server("http://mock-api", "test-key")
    # Patch the httpx client to use mock transport
    for tool_name in srv._tool_manager._tools:
        pass  # just verify tools are registered
    # Replace the client in the closure — access via the tool functions' closure
    mock_client = httpx.Client(
        base_url="http://mock-api",
        transport=MockTransport(),
        headers={"Authorization": "Bearer test-key"},
    )
    # The cloud server stores `client` in the closure of each tool function.
    # We need to replace it. The simplest way: recreate the server with a patched module.
    # Instead, let's just monkey-patch the client variable in the closure.
    for tool in srv._tool_manager._tools.values():
        fn = tool.fn
        if hasattr(fn, "__wrapped__"):
            fn = fn.__wrapped__
        # Walk closure cells to find the httpx.Client
        if hasattr(fn, "__closure__") and fn.__closure__:
            for cell in fn.__closure__:
                try:
                    val = cell.cell_contents
                    if isinstance(val, httpx.Client):
                        cell.cell_contents = mock_client
                except ValueError:
                    pass
    return srv


async def call(server, tool: str, **kwargs) -> str:
    contents, _ = await server.call_tool(tool, kwargs)
    return contents[0].text


# ── Tests ─────────────────────────────────────────────────────────────


@pytest.mark.asyncio
async def test_cloud_store_text(server):
    result = await call(server, "memory_store", content="hello world")
    assert "mem_001" in result
    assert "hello world" in result


@pytest.mark.asyncio
async def test_cloud_store_json(server):
    result = await call(server, "memory_store", content="hello world", format="json")
    data = json.loads(result)
    assert data["status"] == "ok"
    assert data["memory_id"] == "mem_001"
    assert data["content"] == "hello world"


@pytest.mark.asyncio
async def test_cloud_retrieve_text(server):
    result = await call(server, "memory_retrieve", query="test")
    assert "semantic" in result
    assert "test memory" in result


@pytest.mark.asyncio
async def test_cloud_retrieve_json(server):
    result = await call(server, "memory_retrieve", query="test", format="json")
    data = json.loads(result)
    assert data["status"] == "ok"
    assert data["count"] == 1
    assert data["memories"][0]["type"] == "semantic"


@pytest.mark.asyncio
async def test_cloud_search_text(server):
    result = await call(server, "memory_search", query="test")
    assert "test memory" in result


@pytest.mark.asyncio
async def test_cloud_search_json(server):
    result = await call(server, "memory_search", query="test", format="json")
    data = json.loads(result)
    assert data["status"] == "ok"
    assert data["count"] == 1


@pytest.mark.asyncio
async def test_cloud_correct_text(server):
    result = await call(
        server, "memory_correct", memory_id="mem_001", new_content="updated"
    )
    assert "mem_001" in result
    assert "updated" in result


@pytest.mark.asyncio
async def test_cloud_correct_json(server):
    result = await call(
        server,
        "memory_correct",
        memory_id="mem_001",
        new_content="updated",
        format="json",
    )
    data = json.loads(result)
    assert data["status"] == "ok"
    assert data["memory_id"] == "mem_001"


@pytest.mark.asyncio
async def test_cloud_purge_text(server):
    result = await call(server, "memory_purge", memory_id="mem_001")
    assert "Purged" in result


@pytest.mark.asyncio
async def test_cloud_purge_json(server):
    result = await call(server, "memory_purge", memory_id="mem_001", format="json")
    data = json.loads(result)
    assert data["status"] == "ok"
    assert data["purged"] == 1


@pytest.mark.asyncio
async def test_cloud_purge_batch(server):
    result = await call(server, "memory_purge", memory_id="mem_001,mem_002,mem_003")
    assert "3" in result


@pytest.mark.asyncio
async def test_cloud_purge_topic(server):
    result = await call(server, "memory_purge", topic="test")
    assert "Purged" in result


@pytest.mark.asyncio
async def test_cloud_purge_no_target(server):
    result = await call(server, "memory_purge")
    assert "Provide" in result


@pytest.mark.asyncio
async def test_cloud_capabilities(server):
    result = await call(server, "memory_capabilities")
    data = json.loads(result)
    assert data["mode"] == "cloud"
    assert "memory_store" in data["tools"]
    assert "memory_capabilities" in data["tools"]
    # Cloud mode should NOT have branching tools
    assert "memory_branch" not in data["tools"]
    assert "memory_governance" not in data["tools"]
