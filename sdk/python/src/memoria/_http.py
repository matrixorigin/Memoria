"""Shared HTTP transport base for MemoriaClient and AsyncMemoriaClient.

Architecture:
  _HttpTransport   — URL building, header injection, error mapping, retry logic
      MemoriaClient       — wraps httpx.Client        (sync)
      AsyncMemoriaClient  — wraps httpx.AsyncClient   (async)

Only ``_request`` / ``_arequest`` differ between the two subclasses.
All business logic (URL, params, body, response parsing) lives in Resource classes
that call ``self._client._request(...)`` or ``await self._client._arequest(...)``.
"""

from __future__ import annotations

import time
from typing import Any

import httpx

from .exceptions import (
    MemoriaAPIError,
    MemoriaAuthError,
    MemoriaConnectionError,
    MemoriaForbiddenError,
    MemoriaNotFoundError,
    MemoriaServerError,
    MemoriaUnprocessableError,
)

_VERSION = "1.0.0"
_DEFAULT_TIMEOUT = 30.0
_DEFAULT_MAX_RETRIES = 3
_RETRY_STATUS = {500, 502, 503, 504}


def _build_headers(api_key: str) -> dict[str, str]:
    return {
        "Authorization": f"Bearer {api_key}",
        "Content-Type": "application/json",
        "User-Agent": f"memoria-python/{_VERSION}",
    }


def _map_error(resp: httpx.Response) -> MemoriaAPIError:
    """Convert an error HTTP response to the appropriate exception subclass."""
    try:
        detail = resp.json().get("detail", resp.text)
    except Exception:
        detail = resp.text or f"HTTP {resp.status_code}"
    sc = resp.status_code
    if sc == 401:
        return MemoriaAuthError(sc, detail)
    if sc == 403:
        return MemoriaForbiddenError(sc, detail)
    if sc == 404:
        return MemoriaNotFoundError(sc, detail)
    if sc == 422:
        return MemoriaUnprocessableError(sc, detail)
    if sc >= 500:
        return MemoriaServerError(sc, detail)
    return MemoriaAPIError(sc, detail)


def _should_retry(status_code: int) -> bool:
    return status_code in _RETRY_STATUS


def _backoff(attempt: int) -> float:
    """Exponential backoff: 0.5s, 1s, 2s, …"""
    return 0.5 * (2 ** attempt)


class _HttpTransport:
    """Shared state and helpers. Subclasses provide the actual HTTP call."""

    def __init__(
        self,
        base_url: str,
        api_key: str,
        timeout: float = _DEFAULT_TIMEOUT,
        max_retries: int = _DEFAULT_MAX_RETRIES,
    ) -> None:
        self._base_url = base_url.rstrip("/")
        self._api_key = api_key
        self._timeout = timeout
        self._max_retries = max_retries
        self._headers = _build_headers(api_key)

    def _url(self, path: str) -> str:
        return self._base_url + path

    # ------------------------------------------------------------------
    # Sync transport
    # ------------------------------------------------------------------

    def _request(
        self,
        method: str,
        path: str,
        *,
        params: dict[str, Any] | None = None,
        json: Any = None,
    ) -> Any:
        """Execute a synchronous HTTP request with retry logic."""
        url = self._url(path)
        last_exc: Exception | None = None
        for attempt in range(self._max_retries + 1):
            try:
                resp = self._http.request(  # type: ignore[attr-defined]
                    method,
                    url,
                    params={k: v for k, v in (params or {}).items() if v is not None},
                    json=json,
                    timeout=self._timeout,
                )
            except (httpx.ConnectError, httpx.TimeoutException, httpx.NetworkError) as exc:
                last_exc = exc
                if attempt < self._max_retries:
                    time.sleep(_backoff(attempt))
                    continue
                raise MemoriaConnectionError(str(exc)) from exc

            if resp.is_success:
                if resp.status_code == 204 or not resp.content:
                    return None
                content_type = resp.headers.get("content-type", "")
                if "json" in content_type:
                    return resp.json()
                try:
                    return resp.json()
                except Exception:
                    return resp.text

            if _should_retry(resp.status_code) and attempt < self._max_retries:
                time.sleep(_backoff(attempt))
                continue

            raise _map_error(resp)

        # should not reach here, but satisfy type checker
        if last_exc:
            raise MemoriaConnectionError(str(last_exc)) from last_exc
        raise MemoriaConnectionError("request failed after retries")

    # ------------------------------------------------------------------
    # Async transport
    # ------------------------------------------------------------------

    async def _arequest(
        self,
        method: str,
        path: str,
        *,
        params: dict[str, Any] | None = None,
        json: Any = None,
    ) -> Any:
        """Execute an asynchronous HTTP request with retry logic."""
        import asyncio

        url = self._url(path)
        last_exc: Exception | None = None
        for attempt in range(self._max_retries + 1):
            try:
                resp = await self._ahttp.request(  # type: ignore[attr-defined]
                    method,
                    url,
                    params={k: v for k, v in (params or {}).items() if v is not None},
                    json=json,
                    timeout=self._timeout,
                )
            except (httpx.ConnectError, httpx.TimeoutException, httpx.NetworkError) as exc:
                last_exc = exc
                if attempt < self._max_retries:
                    await asyncio.sleep(_backoff(attempt))
                    continue
                raise MemoriaConnectionError(str(exc)) from exc

            if resp.is_success:
                if resp.status_code == 204 or not resp.content:
                    return None
                content_type = resp.headers.get("content-type", "")
                if "json" in content_type:
                    return resp.json()
                try:
                    return resp.json()
                except Exception:
                    return resp.text

            if _should_retry(resp.status_code) and attempt < self._max_retries:
                await asyncio.sleep(_backoff(attempt))
                continue

            raise _map_error(resp)

        if last_exc:
            raise MemoriaConnectionError(str(last_exc)) from last_exc
        raise MemoriaConnectionError("request failed after retries")
