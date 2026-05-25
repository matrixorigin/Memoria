"""governance resource — sync and async.

Covers governance.run / consolidate / reflect.
Each method maps to a dedicated REST endpoint.

Cooldown behaviour (server-enforced):
  run()         — 1-hour cooldown, bypassed with force=True
  consolidate() — 30-minute cooldown, bypassed with force=True
  reflect()     — 2-hour cooldown when mode != "candidates"; force=True bypasses it.
                  mode="candidates" NEVER has a cooldown and always returns immediately.

When on cooldown the server returns HTTP 200 with {"skipped": true, "cooldown_remaining_s": N}.
SDK returns a GovernanceResult with .skipped=True; callers should check this before
reading operational fields.
"""

from __future__ import annotations

from typing import TYPE_CHECKING, Any

from ..models import GovernanceResult

if TYPE_CHECKING:
    from .._http import _HttpTransport


def _strip_none(d: dict[str, Any]) -> dict[str, Any]:
    return {k: v for k, v in d.items() if v is not None}


class GovernanceResource:
    def __init__(self, client: _HttpTransport) -> None:
        self._client = client

    def run(self, *, force: bool = False) -> GovernanceResult:
        body = _strip_none({"force": force if force else None})
        data = self._client._request("POST", "/v1/governance", json=body)
        return GovernanceResult.from_dict(data)

    def consolidate(self, *, force: bool = False) -> GovernanceResult:
        body = _strip_none({"force": force if force else None})
        data = self._client._request("POST", "/v1/consolidate", json=body)
        return GovernanceResult.from_dict(data)

    def reflect(self, *, force: bool = False, mode: str = "auto") -> GovernanceResult:
        body = _strip_none(
            {
                "force": force if force else None,
                "mode": mode,
            }
        )
        data = self._client._request("POST", "/v1/reflect", json=body)
        return GovernanceResult.from_dict(data)


class AsyncGovernanceResource:
    def __init__(self, client: _HttpTransport) -> None:
        self._client = client

    async def run(self, *, force: bool = False) -> GovernanceResult:
        body = _strip_none({"force": force if force else None})
        data = await self._client._arequest("POST", "/v1/governance", json=body)
        return GovernanceResult.from_dict(data)

    async def consolidate(self, *, force: bool = False) -> GovernanceResult:
        body = _strip_none({"force": force if force else None})
        data = await self._client._arequest("POST", "/v1/consolidate", json=body)
        return GovernanceResult.from_dict(data)

    async def reflect(self, *, force: bool = False, mode: str = "auto") -> GovernanceResult:
        body = _strip_none(
            {
                "force": force if force else None,
                "mode": mode,
            }
        )
        data = await self._client._arequest("POST", "/v1/reflect", json=body)
        return GovernanceResult.from_dict(data)
