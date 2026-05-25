"""profile resource — sync and async."""

from __future__ import annotations

from typing import TYPE_CHECKING

from ..models import Profile

if TYPE_CHECKING:
    from .._http import _HttpTransport


class ProfileResource:
    def __init__(self, client: _HttpTransport) -> None:
        self._client = client

    def me(self) -> Profile:
        data = self._client._request("GET", "/v1/profiles/me")
        return Profile.from_dict(data)


class AsyncProfileResource:
    def __init__(self, client: _HttpTransport) -> None:
        self._client = client

    async def me(self) -> Profile:
        data = await self._client._arequest("GET", "/v1/profiles/me")
        return Profile.from_dict(data)
