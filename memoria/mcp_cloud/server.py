"""SaaS Memory MCP Server — proxies to SaaS API via HTTP."""

from __future__ import annotations

import argparse
import json as _json
import httpx
from mcp.server import FastMCP


def create_server(api_url: str, api_key: str) -> FastMCP:
    server = FastMCP("memoria")
    client = httpx.Client(
        base_url=api_url,
        headers={"Authorization": f"Bearer {api_key}"},
        timeout=30,
        trust_env=False,  # ignore proxy env vars (http_proxy, https_proxy, etc.)
    )

    def _json_dumps(obj: dict | list) -> str:
        return _json.dumps(obj, ensure_ascii=False)

    @server.tool()
    async def memory_store(
        content: str,
        memory_type: str = "semantic",
        session_id: str | None = None,
        format: str = "text",
    ) -> str:
        """Store a memory.

        Args:
            content: The memory content to store.
            memory_type: One of: profile, semantic, procedural, working, tool_result.
            session_id: Session context (optional).
            format: 'text' (default) or 'json' for structured response.
        """
        r = client.post(
            "/v1/memories",
            json={
                "content": content,
                "memory_type": memory_type,
                "session_id": session_id,
            },
        )
        r.raise_for_status()
        d = r.json()
        if format == "json":
            return _json_dumps(
                {"status": "ok", "memory_id": d["memory_id"], "content": d["content"]}
            )
        return f"Stored memory {d['memory_id']}: {d['content'][:80]}"

    @server.tool()
    async def memory_retrieve(query: str, top_k: int = 5, format: str = "text") -> str:
        """Retrieve relevant memories for a query.

        Args:
            query: What to search for in memories.
            top_k: Max number of memories to return (default 5).
            format: 'text' (default) or 'json' for structured response.
        """
        r = client.post("/v1/memories/retrieve", json={"query": query, "top_k": top_k})
        r.raise_for_status()
        items = r.json()
        if format == "json":
            return _json_dumps(
                {
                    "status": "ok",
                    "count": len(items),
                    "memories": [
                        {
                            "memory_id": m.get("memory_id", ""),
                            "type": m.get("memory_type", "fact"),
                            "content": m["content"],
                        }
                        for m in items
                    ],
                }
            )
        if not items:
            return "No relevant memories found."
        lines = [f"- [{m['memory_type']}] {m['content']}" for m in items]
        return "\n".join(lines)

    @server.tool()
    async def memory_search(query: str, top_k: int = 10, format: str = "text") -> str:
        """Semantic search over all memories.

        Args:
            query: Search query.
            top_k: Max results (default 10).
            format: 'text' (default) or 'json' for structured response.
        """
        r = client.post("/v1/memories/search", json={"query": query, "top_k": top_k})
        r.raise_for_status()
        items = r.json()
        if format == "json":
            return _json_dumps(
                {
                    "status": "ok",
                    "count": len(items),
                    "memories": [
                        {
                            "memory_id": m["memory_id"],
                            "type": m.get("memory_type", "fact"),
                            "content": m["content"],
                        }
                        for m in items
                    ],
                }
            )
        if not items:
            return "No memories found."
        lines = [
            f"- [{m['memory_id']}] [{m['memory_type']}] {m['content']}" for m in items
        ]
        return "\n".join(lines)

    @server.tool()
    async def memory_correct(
        memory_id: str | None = None,
        new_content: str = "",
        reason: str = "",
        query: str | None = None,
        format: str = "text",
    ) -> str:
        """Correct an existing memory. Provide memory_id to correct by ID, or query to find and correct by semantic search.

        Args:
            memory_id: ID of the memory to correct.
            new_content: The corrected content.
            reason: Why the correction is needed.
            query: Search query to find the memory to correct.
            format: 'text' (default) or 'json' for structured response.
        """
        if not new_content:
            if format == "json":
                return _json_dumps(
                    {"status": "error", "error": "new_content is required."}
                )
            return "new_content is required."
        if query and not memory_id:
            r = client.post(
                "/v1/memories/correct",
                json={"query": query, "new_content": new_content, "reason": reason},
            )
            if r.status_code == 404:
                msg = f"No memory found matching '{query}'"
                if format == "json":
                    return _json_dumps({"status": "error", "error": msg})
                return msg
            r.raise_for_status()
            d = r.json()
            if format == "json":
                return _json_dumps(
                    {
                        "status": "ok",
                        "memory_id": d["memory_id"],
                        "content": d["content"],
                        "matched_content": d.get("matched_content", ""),
                    }
                )
            return f"Found '{d.get('matched_content', '')}' → corrected to {d['memory_id']}: {d['content'][:80]}"
        if not memory_id:
            if format == "json":
                return _json_dumps(
                    {"status": "error", "error": "Provide either memory_id or query."}
                )
            return "Provide either memory_id or query."
        r = client.put(
            f"/v1/memories/{memory_id}/correct",
            json={"new_content": new_content, "reason": reason},
        )
        r.raise_for_status()
        d = r.json()
        if format == "json":
            return _json_dumps(
                {"status": "ok", "memory_id": d["memory_id"], "content": d["content"]}
            )
        return f"Corrected memory {d['memory_id']}: {d['content'][:80]}"

    @server.tool()
    async def memory_purge(
        memory_id: str | None = None,
        topic: str | None = None,
        reason: str = "",
        format: str = "text",
    ) -> str:
        """Delete memories. Use memory_id for a single memory, or topic to bulk-delete all memories matching a keyword.

        Args:
            memory_id: ID of a specific memory to delete. Supports comma-separated IDs for batch delete (e.g. "id1,id2,id3").
            topic: Keyword/topic — finds and deletes all matching memories.
            reason: Why it should be deleted.
            format: 'text' (default) or 'json' for structured response.
        """
        if not memory_id and not topic:
            err = "Provide either memory_id or topic."
            if format == "json":
                return _json_dumps({"status": "error", "error": err})
            return err

        if topic:
            # Search then batch purge
            hits = client.post(
                "/v1/memories/search", json={"query": topic, "top_k": 50}
            )
            hits.raise_for_status()
            ids = [h["memory_id"] for h in hits.json()]
            if not ids:
                if format == "json":
                    return _json_dumps({"status": "ok", "purged": 0})
                return "Purged 0 memory(ies)."
        else:
            ids = [mid.strip() for mid in memory_id.split(",") if mid.strip()]  # type: ignore[union-attr]

        if len(ids) == 1:
            r = client.delete(f"/v1/memories/{ids[0]}", params={"reason": reason})
            r.raise_for_status()
            purged = r.json().get("purged", 1)
        else:
            r = client.post(
                "/v1/memories/purge",
                json={"memory_ids": ids, "reason": reason},
            )
            r.raise_for_status()
            purged = r.json().get("purged", len(ids))

        if format == "json":
            return _json_dumps({"status": "ok", "purged": purged})
        return f"Purged {purged} memory(ies)."

    @server.tool()
    async def memory_profile() -> str:
        """Get current user's memory profile.

        Note: user_id is resolved server-side from the API key. No user_id parameter needed.
        """
        r = client.get("/v1/profiles/me")
        r.raise_for_status()
        return str(r.json())

    @server.tool()
    async def memory_snapshot(name: str, description: str = "") -> str:
        """Create a read-only snapshot of current memories."""
        r = client.post(
            "/v1/snapshots", json={"name": name, "description": description}
        )
        r.raise_for_status()
        d = r.json()
        return f"Snapshot '{d['name']}' created (ts={d.get('timestamp', 'unknown')})"

    @server.tool()
    async def memory_snapshots() -> str:
        """List all snapshots."""
        r = client.get("/v1/snapshots")
        r.raise_for_status()
        items = r.json()
        if not items:
            return "No snapshots."
        lines = [f"- {s['name']} ({s.get('timestamp', '')})" for s in items]
        return "\n".join(lines)

    @server.tool()
    async def memory_consolidate(force: bool = False) -> str:
        """Detect contradicting memories, fix orphaned nodes. 30min cooldown."""
        r = client.post("/v1/consolidate", params={"force": force})
        r.raise_for_status()
        return str(r.json())

    @server.tool()
    async def memory_reflect(force: bool = False, mode: str = "auto") -> str:
        """Analyze memory clusters and synthesize insights.

        mode: 'auto' (internal LLM if available, else candidates), 'internal', 'candidates'.
        In candidates mode, returns raw clusters for YOU to synthesize, then store via memory_store.
        """
        if mode == "candidates":
            r = client.post("/v1/reflect/candidates")
            r.raise_for_status()
            data = r.json()
            clusters = data.get("candidates", [])
            if not clusters:
                return "No reflection candidates found."
            parts = []
            for i, c in enumerate(clusters, 1):
                mems = "\n".join(
                    f"  - [{m['type']}] {m['content']}" for m in c["memories"]
                )
                parts.append(
                    f"Cluster {i} ({c['signal']}, importance={c['importance']}):\n{mems}"
                )
            return (
                "Synthesize 1-2 insights per cluster, then store via memory_store.\n\n"
                + "\n\n".join(parts)
            )
        r = client.post("/v1/reflect", params={"force": force})
        r.raise_for_status()
        return str(r.json())

    @server.tool()
    async def memory_extract_entities(mode: str = "auto") -> str:
        """Extract entities from memories. mode: 'auto', 'internal', 'candidates'.
        In candidates mode, returns unlinked memories for YOU to extract entities, then call memory_link_entities."""
        if mode == "candidates":
            r = client.post("/v1/extract-entities/candidates")
            r.raise_for_status()
            memories = r.json().get("memories", [])
            if not memories:
                return "No unlinked memories found."
            lines = [f"- [{m['memory_id']}] {m['content']}" for m in memories]
            return (
                f"Found {len(memories)} unlinked memories. Extract entities, then call memory_link_entities.\n\n"
                + "\n".join(lines)
            )
        r = client.post("/v1/extract-entities")
        r.raise_for_status()
        return str(r.json())

    @server.tool()
    async def memory_link_entities(entities: str) -> str:
        """Write entity links from extraction results. entities: JSON [{"memory_id": "...", "entities": [{"name": "...", "type": "..."}]}]"""
        try:
            parsed = _json.loads(entities)
        except (ValueError, TypeError):
            return "Invalid JSON."
        r = client.post("/v1/extract-entities/link", json={"entities": parsed})
        r.raise_for_status()
        d = r.json()
        return (
            f"Linked: {d['entities_created']} new entities, {d['edges_created']} edges."
        )

    @server.tool()
    async def memory_snapshot_diff(name: str) -> str:
        """Compare a snapshot with current memories. Shows added/removed since snapshot."""
        r = client.get(f"/v1/snapshots/{name}/diff")
        r.raise_for_status()
        d = r.json()
        lines = [
            f"Snapshot '{name}': {d['snapshot_count']} memories, Current: {d['current_count']} memories"
        ]
        lines.append(
            f"Added: {d['added_count']}, Removed: {d['removed_count']}, Unchanged: {d['unchanged_count']}"
        )
        for m in d.get("added", []):
            lines.append(f"  + [{m['memory_type']}] {m['content']}")
        for m in d.get("removed", []):
            lines.append(f"  - [{m['memory_type']}] {m['content']}")
        return "\n".join(lines)

    @server.tool()
    async def memory_capabilities() -> str:
        """List available memory tools and current backend mode.

        Call this to discover which tools are available.
        In shared cloud mode, branching/governance tools may not be available.
        In dedicated instance mode (user has their own DB), all tools are available.
        """
        # TODO: query GET /capabilities from server to dynamically reflect what the
        # server supports (shared vs dedicated instance). For now, list what this
        # client exposes — the server will return 404/501 for unsupported operations.
        tools = [
            "memory_store",
            "memory_retrieve",
            "memory_search",
            "memory_correct",
            "memory_purge",
            "memory_profile",
            "memory_capabilities",
            "memory_snapshot",
            "memory_snapshots",
            "memory_snapshot_diff",
            "memory_extract_entities",
            "memory_link_entities",
            "memory_consolidate",
            "memory_reflect",
        ]
        return _json_dumps(
            {
                "mode": "cloud",
                "tools": sorted(tools),
                "note": "Branch/rollback/governance availability depends on server tier.",
            }
        )

    return server


def main():
    parser = argparse.ArgumentParser(description="SaaS Memory MCP Server")
    parser.add_argument("--api-url", required=True, help="SaaS API base URL")
    parser.add_argument("--api-key", required=True, help="API key for authentication")
    parser.add_argument("--transport", default="stdio", choices=["stdio", "sse"])
    args = parser.parse_args()

    server = create_server(args.api_url, args.api_key)
    server.run(transport=args.transport)


if __name__ == "__main__":
    main()
