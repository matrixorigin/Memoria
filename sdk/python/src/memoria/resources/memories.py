"""memories resource — sync and async."""

from __future__ import annotations

from typing import TYPE_CHECKING, Any

from ..exceptions import MemoriaValidationError
from ..models import Memory, MemoryPage, PurgeResult, RetrieveResult

if TYPE_CHECKING:
    from .._http import _HttpTransport


def _strip_none(d: dict[str, Any]) -> dict[str, Any]:
    return {k: v for k, v in d.items() if v is not None}


class MemoriesResource:
    def __init__(self, client: _HttpTransport) -> None:
        self._client = client

    def store(
        self,
        content: str,
        *,
        memory_type: str = "semantic",
        session_id: str | None = None,
        trust_tier: str | None = None,
        branch: str | None = None,
    ) -> Memory:
        body = _strip_none(
            {
                "content": content,
                "memory_type": memory_type,
                "session_id": session_id,
                "trust_tier": trust_tier,
                "branch": branch,
            }
        )
        data = self._client._request("POST", "/v1/memories", json=body)
        return Memory.from_dict(data)

    def store_batch(
        self,
        items: list[dict[str, Any]],
        *,
        branch: str | None = None,
    ) -> list[Memory]:
        if len(items) > 100:
            raise MemoriaValidationError("store_batch: items must not exceed 100")
        # Server field name is "memories", not "items"
        body: dict[str, Any] = {"memories": items}
        if branch is not None:
            body["branch"] = branch
        data = self._client._request("POST", "/v1/memories/batch", json=body)
        return [Memory.from_dict(m) for m in data]

    def retrieve(
        self,
        query: str,
        *,
        top_k: int = 5,
        session_id: str | None = None,
        session_scope: str | None = None,
        explain: bool | str = False,
        branch: str | None = None,
    ) -> RetrieveResult:
        if session_scope is not None and session_id is None:
            raise MemoriaValidationError("retrieve: session_scope requires session_id")
        body = _strip_none(
            {
                "query": query,
                "top_k": top_k,
                "session_id": session_id,
                "session_scope": session_scope,
                "explain": explain if explain is not False else None,
                "branch": branch,
            }
        )
        data = self._client._request("POST", "/v1/memories/retrieve", json=body)
        return RetrieveResult.from_dict(data)

    def search(
        self,
        query: str,
        *,
        top_k: int = 10,
        session_id: str | None = None,
        session_scope: str | None = None,
        explain: bool | str = False,
        branch: str | None = None,
    ) -> RetrieveResult:
        if session_scope is not None and session_id is None:
            raise MemoriaValidationError("search: session_scope requires session_id")
        body = _strip_none(
            {
                "query": query,
                "top_k": top_k,
                "session_id": session_id,
                "session_scope": session_scope,
                "explain": explain if explain is not False else None,
                "branch": branch,
            }
        )
        data = self._client._request("POST", "/v1/memories/search", json=body)
        return RetrieveResult.from_dict(data)

    def list(
        self,
        *,
        limit: int = 100,
        cursor: str | None = None,
        memory_type: str | None = None,
        session_id: str | None = None,
        trust_tier: str | None = None,
        branch: str | None = None,
    ) -> MemoryPage:
        if limit > 500:
            raise MemoriaValidationError("list: limit must not exceed 500")
        params = _strip_none(
            {
                "limit": limit,
                "cursor": cursor,
                "memory_type": memory_type,
                "session_id": session_id,
                "trust_tier": trust_tier,
                "branch": branch,
            }
        )
        data = self._client._request("GET", "/v1/memories", params=params)
        return MemoryPage.from_dict(data)

    def correct(
        self,
        id: str,
        *,
        new_content: str,
        reason: str | None = None,
        branch: str | None = None,
    ) -> Memory:
        body = _strip_none({"new_content": new_content, "reason": reason, "branch": branch})
        data = self._client._request("PUT", f"/v1/memories/{id}/correct", json=body)
        return Memory.from_dict(data)

    def correct_by_query(
        self,
        query: str,
        *,
        new_content: str,
        reason: str | None = None,
        session_id: str | None = None,
        session_scope: str | None = None,
        branch: str | None = None,
    ) -> Memory:
        if session_scope is not None and session_id is None:
            raise MemoriaValidationError("correct_by_query: session_scope requires session_id")
        body = _strip_none(
            {
                "query": query,
                "new_content": new_content,
                "reason": reason,
                "session_id": session_id,
                "session_scope": session_scope,
                "branch": branch,
            }
        )
        data = self._client._request("POST", "/v1/memories/correct", json=body)
        return Memory.from_dict(data)

    def delete(
        self,
        id: str,
        *,
        reason: str | None = None,
        branch: str | None = None,
    ) -> None:
        params = _strip_none({"reason": reason, "branch": branch})
        self._client._request("DELETE", f"/v1/memories/{id}", params=params)

    def purge(
        self,
        *,
        memory_ids: list[str] | None = None,
        topic: str | None = None,
        session_id: str | None = None,
        memory_types: list[str] | None = None,
        reason: str | None = None,
        branch: str | None = None,
    ) -> PurgeResult:
        selectors = sum(
            [
                1 if memory_ids is not None else 0,
                1 if topic is not None else 0,
                1 if session_id is not None else 0,
            ]
        )
        if selectors == 0:
            raise MemoriaValidationError(
                "purge: must specify at least one of memory_ids, topic, or session_id"
            )
        if selectors > 1:
            raise MemoriaValidationError(
                "purge: memory_ids, topic, and session_id are mutually exclusive"
            )
        if memory_types is not None and session_id is None:
            raise MemoriaValidationError("purge: memory_types requires session_id")
        body = _strip_none(
            {
                "memory_ids": memory_ids,
                "topic": topic,
                "session_id": session_id,
                "memory_types": memory_types,
                "reason": reason,
                "branch": branch,
            }
        )
        data = self._client._request("POST", "/v1/memories/purge", json=body)
        return PurgeResult.from_dict(data)

    def feedback(
        self,
        id: str,
        *,
        signal: str,
        context: str | None = None,
    ) -> None:
        body = _strip_none({"signal": signal, "context": context})
        self._client._request("POST", f"/v1/memories/{id}/feedback", json=body)


class AsyncMemoriesResource:
    def __init__(self, client: _HttpTransport) -> None:
        self._client = client

    async def store(
        self,
        content: str,
        *,
        memory_type: str = "semantic",
        session_id: str | None = None,
        trust_tier: str | None = None,
        branch: str | None = None,
    ) -> Memory:
        body = _strip_none(
            {
                "content": content,
                "memory_type": memory_type,
                "session_id": session_id,
                "trust_tier": trust_tier,
                "branch": branch,
            }
        )
        data = await self._client._arequest("POST", "/v1/memories", json=body)
        return Memory.from_dict(data)

    async def store_batch(
        self,
        items: list[dict[str, Any]],
        *,
        branch: str | None = None,
    ) -> list[Memory]:
        if len(items) > 100:
            raise MemoriaValidationError("store_batch: items must not exceed 100")
        # Server field name is "memories", not "items"
        body: dict[str, Any] = {"memories": items}
        if branch is not None:
            body["branch"] = branch
        data = await self._client._arequest("POST", "/v1/memories/batch", json=body)
        return [Memory.from_dict(m) for m in data]

    async def retrieve(
        self,
        query: str,
        *,
        top_k: int = 5,
        session_id: str | None = None,
        session_scope: str | None = None,
        explain: bool | str = False,
        branch: str | None = None,
    ) -> RetrieveResult:
        if session_scope is not None and session_id is None:
            raise MemoriaValidationError("retrieve: session_scope requires session_id")
        body = _strip_none(
            {
                "query": query,
                "top_k": top_k,
                "session_id": session_id,
                "session_scope": session_scope,
                "explain": explain if explain is not False else None,
                "branch": branch,
            }
        )
        data = await self._client._arequest("POST", "/v1/memories/retrieve", json=body)
        return RetrieveResult.from_dict(data)

    async def search(
        self,
        query: str,
        *,
        top_k: int = 10,
        session_id: str | None = None,
        session_scope: str | None = None,
        explain: bool | str = False,
        branch: str | None = None,
    ) -> RetrieveResult:
        if session_scope is not None and session_id is None:
            raise MemoriaValidationError("search: session_scope requires session_id")
        body = _strip_none(
            {
                "query": query,
                "top_k": top_k,
                "session_id": session_id,
                "session_scope": session_scope,
                "explain": explain if explain is not False else None,
                "branch": branch,
            }
        )
        data = await self._client._arequest("POST", "/v1/memories/search", json=body)
        return RetrieveResult.from_dict(data)

    async def list(
        self,
        *,
        limit: int = 100,
        cursor: str | None = None,
        memory_type: str | None = None,
        session_id: str | None = None,
        trust_tier: str | None = None,
        branch: str | None = None,
    ) -> MemoryPage:
        if limit > 500:
            raise MemoriaValidationError("list: limit must not exceed 500")
        params = _strip_none(
            {
                "limit": limit,
                "cursor": cursor,
                "memory_type": memory_type,
                "session_id": session_id,
                "trust_tier": trust_tier,
                "branch": branch,
            }
        )
        data = await self._client._arequest("GET", "/v1/memories", params=params)
        return MemoryPage.from_dict(data)

    async def correct(
        self,
        id: str,
        *,
        new_content: str,
        reason: str | None = None,
        branch: str | None = None,
    ) -> Memory:
        body = _strip_none({"new_content": new_content, "reason": reason, "branch": branch})
        data = await self._client._arequest("PUT", f"/v1/memories/{id}/correct", json=body)
        return Memory.from_dict(data)

    async def correct_by_query(
        self,
        query: str,
        *,
        new_content: str,
        reason: str | None = None,
        session_id: str | None = None,
        session_scope: str | None = None,
        branch: str | None = None,
    ) -> Memory:
        if session_scope is not None and session_id is None:
            raise MemoriaValidationError("correct_by_query: session_scope requires session_id")
        body = _strip_none(
            {
                "query": query,
                "new_content": new_content,
                "reason": reason,
                "session_id": session_id,
                "session_scope": session_scope,
                "branch": branch,
            }
        )
        data = await self._client._arequest("POST", "/v1/memories/correct", json=body)
        return Memory.from_dict(data)

    async def delete(
        self,
        id: str,
        *,
        reason: str | None = None,
        branch: str | None = None,
    ) -> None:
        params = _strip_none({"reason": reason, "branch": branch})
        await self._client._arequest("DELETE", f"/v1/memories/{id}", params=params)

    async def purge(
        self,
        *,
        memory_ids: list[str] | None = None,
        topic: str | None = None,
        session_id: str | None = None,
        memory_types: list[str] | None = None,
        reason: str | None = None,
        branch: str | None = None,
    ) -> PurgeResult:
        selectors = sum(
            [
                1 if memory_ids is not None else 0,
                1 if topic is not None else 0,
                1 if session_id is not None else 0,
            ]
        )
        if selectors == 0:
            raise MemoriaValidationError(
                "purge: must specify at least one of memory_ids, topic, or session_id"
            )
        if selectors > 1:
            raise MemoriaValidationError(
                "purge: memory_ids, topic, and session_id are mutually exclusive"
            )
        if memory_types is not None and session_id is None:
            raise MemoriaValidationError("purge: memory_types requires session_id")
        body = _strip_none(
            {
                "memory_ids": memory_ids,
                "topic": topic,
                "session_id": session_id,
                "memory_types": memory_types,
                "reason": reason,
                "branch": branch,
            }
        )
        data = await self._client._arequest("POST", "/v1/memories/purge", json=body)
        return PurgeResult.from_dict(data)

    async def feedback(
        self,
        id: str,
        *,
        signal: str,
        context: str | None = None,
    ) -> None:
        body = _strip_none({"signal": signal, "context": context})
        await self._client._arequest("POST", f"/v1/memories/{id}/feedback", json=body)
