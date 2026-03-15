"""Remote auth service client — resolves API keys to per-user DB connections."""

from __future__ import annotations

import time
from dataclasses import dataclass
from threading import Lock
from typing import Any
from urllib.parse import quote_plus

import httpx


@dataclass(frozen=True)
class ConnInfo:
    """Connection info returned by the remote auth service."""

    user_id: str
    db_host: str
    db_port: int
    db_user: str
    db_password: str
    db_name: str

    @property
    def db_url(self) -> str:
        # URL-encode user/password — they may contain special chars
        # like ':' or '@' (e.g. "local-moi-account:moi_root").
        user = quote_plus(self.db_user)
        pwd = quote_plus(self.db_password)
        return (
            f"mysql+pymysql://{user}:{pwd}"
            f"@{self.db_host}:{self.db_port}/{self.db_name}"
            "?charset=utf8mb4"
        )


class _CacheEntry:
    __slots__ = ("value", "expires_at")

    def __init__(self, value: ConnInfo, ttl: int) -> None:
        self.value = value
        self.expires_at = time.monotonic() + ttl


class RemoteAuthService:
    """Calls remote auth service to resolve an API key into ConnInfo.

    Results are cached by key prefix (first 12 chars) with a configurable TTL.
    """

    def __init__(self, base_url: str, cache_ttl: int = 60) -> None:
        self._client = httpx.Client(base_url=base_url.rstrip("/"), timeout=10)
        self._ttl = cache_ttl
        self._cache: dict[str, _CacheEntry] = {}
        self._lock = Lock()

    def resolve(self, apikey: str) -> ConnInfo:
        """Resolve an API key to connection info, with TTL cache."""
        cache_key = apikey[:12]

        with self._lock:
            entry = self._cache.get(cache_key)
            if entry and entry.expires_at > time.monotonic():
                return entry.value

        resp = self._client.post(
            "/apikey/connection",
            headers={"Authorization": f"Bearer {apikey}"},
        )
        if resp.status_code == 401:
            raise PermissionError("Invalid or expired API key")
        resp.raise_for_status()

        data: dict[str, Any] = resp.json()
        info = ConnInfo(
            user_id=data["user_id"],
            db_host=data["db_host"],
            db_port=int(data["db_port"]),
            db_user=data["db_user"],
            db_password=data["db_password"],
            db_name=data["db_name"],
        )

        if self._ttl > 0:
            with self._lock:
                self._cache[cache_key] = _CacheEntry(info, self._ttl)

        return info

    def close(self) -> None:
        self._client.close()
