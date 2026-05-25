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

try:
    from importlib.metadata import version as _pkg_version

    _VERSION: str = _pkg_version("memoria-client")
except Exception:
    _VERSION = "dev"

_DEFAULT_TIMEOUT = 30.0
_DEFAULT_MAX_RETRIES = 3

# 502/503/504 are gateway-level errors: the upstream server almost certainly
# did not process the request, so retrying is safe for any HTTP method.
# 500 is ambiguous for non-idempotent methods: the server may have processed
# the request and written data before returning the error. Retrying a POST/PATCH
# on 500 could produce duplicate writes (e.g. duplicate memories), so we exclude
# it for non-safe methods.
_RETRY_STATUS_SAFE = {500, 502, 503, 504}       # idempotent methods: GET/HEAD/PUT/DELETE
_RETRY_STATUS_UNSAFE = {502, 503, 504}           # non-idempotent methods: POST/PATCH

# HTTP methods where repeating the request is guaranteed not to cause side-effects.
_IDEMPOTENT_METHODS = {"GET", "HEAD", "PUT", "DELETE", "OPTIONS", "TRACE"}


def _should_retry(method: str, status_code: int) -> bool:
    if method.upper() in _IDEMPOTENT_METHODS:
        return status_code in _RETRY_STATUS_SAFE
    return status_code in _RETRY_STATUS_UNSAFE


def _should_retry_network_error(method: str, exc: Exception) -> bool:
    """ConnectError is safe to retry for any method (server never received the request).
    TimeoutException is only safe for idempotent methods — a read timeout on POST may
    mean the server processed the request but the response was lost in transit."""
    if isinstance(exc, httpx.ConnectError):
        return True
    if isinstance(exc, (httpx.TimeoutException, httpx.NetworkError)):
        return method.upper() in _IDEMPOTENT_METHODS
    return False


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
        """Execute a synchronous HTTP request with retry logic.

        Retry policy:
          - 502/503/504: always retried (gateway errors; server did not process request)
          - 500: only retried for idempotent methods (GET/HEAD/PUT/DELETE) to avoid
            duplicate writes on non-idempotent endpoints like POST memories.store
          - ConnectError: always retried (connection never reached server)
          - TimeoutException/NetworkError: only retried for idempotent methods
        """
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
                if attempt < self._max_retries and _should_retry_network_error(method, exc):
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

            if _should_retry(method, resp.status_code) and attempt < self._max_retries:
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
        """Execute an asynchronous HTTP request with retry logic.

        Same retry policy as _request — see its docstring for details.
        """
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
                if attempt < self._max_retries and _should_retry_network_error(method, exc):
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

            if _should_retry(method, resp.status_code) and attempt < self._max_retries:
                await asyncio.sleep(_backoff(attempt))
                continue

            raise _map_error(resp)

        if last_exc:
            raise MemoriaConnectionError(str(last_exc)) from last_exc
        raise MemoriaConnectionError("request failed after retries")
