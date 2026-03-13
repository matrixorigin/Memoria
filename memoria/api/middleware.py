"""Rate limiting middleware — per-API-key throttle.

v1: in-memory sliding window. v2: swap to Redis for distributed.

Override any limit via environment variables:
    MEMORIA_RATE_LIMIT_AUTH_KEYS=100,60       # POST /auth/keys: 100 req/60s
    MEMORIA_RATE_LIMIT_STORE=500,60           # POST /v1/memories
    MEMORIA_RATE_LIMIT_BATCH=100,60           # POST /v1/memories/batch
    MEMORIA_RATE_LIMIT_RETRIEVE=1000,60       # POST /v1/memories/retrieve
    MEMORIA_RATE_LIMIT_CONSOLIDATE=10,3600    # POST /v1/consolidate
    MEMORIA_RATE_LIMIT_REFLECT=5,7200         # POST /v1/reflect
    MEMORIA_RATE_LIMIT_DEFAULT=2000,60        # fallback
"""

import os
import time
from collections import defaultdict, deque

from fastapi import Request
from starlette.middleware.base import BaseHTTPMiddleware


def _env_limit(name: str, default: tuple[int, int]) -> tuple[int, int]:
    val = os.environ.get(f"MEMORIA_RATE_LIMIT_{name}")
    if val:
        try:
            parts = val.split(",")
            return int(parts[0]), int(parts[1])
        except Exception:
            pass
    return default


# Limits: (max_requests, window_seconds)
_RATE_LIMITS: dict[str, tuple[int, int]] = {
    "POST:/v1/memories": _env_limit("STORE", (300, 60)),
    "POST:/v1/memories/batch": _env_limit("BATCH", (60, 60)),
    "POST:/v1/memories/correct": _env_limit("CORRECT", (120, 60)),
    "PUT:/v1/memories/": _env_limit("CORRECT", (120, 60)),
    "DELETE:/v1/memories/": _env_limit("DELETE", (120, 60)),
    "POST:/v1/memories/purge": _env_limit("PURGE", (30, 60)),
    "POST:/v1/observe": _env_limit("OBSERVE", (120, 60)),
    "POST:/v1/memories/retrieve": _env_limit("RETRIEVE", (600, 60)),
    "POST:/v1/memories/search": _env_limit("SEARCH", (600, 60)),
    "GET:/v1/memories": _env_limit("LIST", (300, 60)),
    "GET:/v1/profiles/": _env_limit("PROFILE", (120, 60)),
    "POST:/v1/consolidate": _env_limit("CONSOLIDATE", (3, 3600)),
    "POST:/v1/reflect": _env_limit("REFLECT", (2, 7200)),
    "POST:/v1/snapshots": _env_limit("SNAP_CREATE", (30, 60)),
    "GET:/v1/snapshots": _env_limit("SNAP_READ", (120, 60)),
    "DELETE:/v1/snapshots/": _env_limit("SNAP_DELETE", (30, 60)),
    "POST:/auth/keys": _env_limit("AUTH_KEYS", (20, 60)),
    "_default": _env_limit("DEFAULT", (1000, 60)),
}


class _SlidingWindow:
    __slots__ = ("timestamps", "_max")

    def __init__(self, max_size: int = 2000):
        self.timestamps: deque[float] = deque(maxlen=max_size)

    def hit(self, now: float, window: int) -> int:
        cutoff = now - window
        # Pop expired entries from the left (oldest first)
        while self.timestamps and self.timestamps[0] <= cutoff:
            self.timestamps.popleft()
        self.timestamps.append(now)
        return len(self.timestamps)


# key → (method:path_prefix) → SlidingWindow
_windows: dict[str, dict[str, _SlidingWindow]] = defaultdict(
    lambda: defaultdict(_SlidingWindow)
)
_last_cleanup = time.time()
_CLEANUP_INTERVAL = 300  # purge stale keys every 5 minutes


def _match_limit(method: str, path: str) -> tuple[int, int]:
    """Find the most specific rate limit for this request."""
    # Exact match first
    key = f"{method}:{path}"
    if key in _RATE_LIMITS:
        return _RATE_LIMITS[key]
    # Prefix match (for parameterized paths like /v1/memories/{id})
    for pattern, limit in _RATE_LIMITS.items():
        if pattern == "_default":
            continue
        p_method, p_path = pattern.split(":", 1)
        if method == p_method and path.startswith(p_path):
            return limit
    return _RATE_LIMITS["_default"]


class RateLimitMiddleware(BaseHTTPMiddleware):
    async def dispatch(self, request: Request, call_next):
        global _last_cleanup

        # Periodic cleanup of stale key entries
        now = time.time()
        if now - _last_cleanup > _CLEANUP_INTERVAL:
            _last_cleanup = now
            stale = [
                k
                for k, routes in _windows.items()
                if all(not w.timestamps for w in routes.values())
            ]
            for k in stale:
                del _windows[k]

        # Extract API key from Authorization header
        auth = request.headers.get("authorization", "")
        if not auth.startswith("Bearer "):
            return await call_next(request)

        api_key = auth[7:]

        # Master key is exempt from rate limiting (used by admin/benchmark)
        from memoria.api.dependencies import _is_master_key

        if _is_master_key(api_key):
            return await call_next(request)

        key_id = api_key[:12]  # prefix only, don't store full key

        method = request.method
        path = request.url.path
        max_req, window = _match_limit(method, path)

        now = time.time()
        route_key = f"{method}:{path.split('?')[0]}"
        count = _windows[key_id][route_key].hit(now, window)

        if count > max_req:
            from starlette.responses import JSONResponse

            return JSONResponse(
                status_code=429,
                content={
                    "detail": f"Rate limit exceeded. Max {max_req} requests per {window}s."
                },
                headers={"Retry-After": str(window)},
            )

        response = await call_next(request)
        response.headers["X-RateLimit-Limit"] = str(max_req)
        response.headers["X-RateLimit-Remaining"] = str(max(0, max_req - count))
        return response
