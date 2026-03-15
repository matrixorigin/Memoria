"""API authentication — supports both token mode and apikey mode.

Token mode (existing): Authorization: Bearer <token>
  - Validates against master key or auth_api_keys table in shared DB.
  - All users share one MatrixOne database.

Apikey mode (new): X-API-Key: <apikey>
  - Calls remote auth service to resolve apikey → per-user DB connection.
  - Each user gets their own MatrixOne database.
"""

from __future__ import annotations

import hmac
from dataclasses import dataclass
from datetime import datetime, timezone
from typing import Any

from fastapi import Depends, HTTPException, Request, status
from fastapi.security import HTTPAuthorizationCredentials, HTTPBearer
from sqlalchemy import update
from sqlalchemy.orm import Session, sessionmaker
from sqlalchemy.sql import func

from memoria.api.database import get_db_factory, get_db_session
from memoria.api.models import ApiKey
from memoria.config import get_settings

security = HTTPBearer(auto_error=False)

ADMIN_USER_ID = "__admin__"

# ── Singleton for remote auth service client ────────────────────────

_remote_auth: Any = None


def _get_remote_auth():
    global _remote_auth
    if _remote_auth is None:
        from memoria.api.remote_auth_service import RemoteAuthService

        settings = get_settings()
        _remote_auth = RemoteAuthService(
            base_url=settings.remote_auth_service_url,
            cache_ttl=settings.conn_cache_ttl,
        )
    return _remote_auth


# ── Token mode helpers (existing logic) ─────────────────────────────


def _is_master_key(token: str) -> bool:
    """Check if token is the master key (timing-safe)."""
    settings = get_settings()
    return bool(settings.master_key and hmac.compare_digest(token, settings.master_key))


def get_current_user_id(
    request: Request,
    credentials: HTTPAuthorizationCredentials = Depends(security),
    db: Session = Depends(get_db_session),
) -> str:
    """Authenticate via API key, return user_id (token mode only).

    Admin (master key) can impersonate a specific user via the
    ``X-Impersonate-User`` header.  Only master-key holders can use this;
    regular API keys ignore the header entirely.
    """
    if credentials is None:
        raise HTTPException(
            status_code=status.HTTP_401_UNAUTHORIZED,
            detail="Missing Authorization header",
        )
    token = credentials.credentials

    if _is_master_key(token):
        impersonate = request.headers.get("X-Impersonate-User")
        if impersonate:
            return impersonate
        return ADMIN_USER_ID

    key_hash = ApiKey.hash_key(token)
    row = (
        db.query(ApiKey.key_id, ApiKey.user_id, ApiKey.expires_at)
        .filter(
            ApiKey.key_hash == key_hash,
            ApiKey.is_active > 0,
        )
        .first()
    )
    if row is None:
        raise HTTPException(
            status_code=status.HTTP_401_UNAUTHORIZED, detail="Invalid API key"
        )

    if row.expires_at:
        exp = (
            row.expires_at.replace(tzinfo=timezone.utc)
            if row.expires_at.tzinfo is None
            else row.expires_at
        )
        if exp < datetime.now(timezone.utc):
            raise HTTPException(
                status_code=status.HTTP_401_UNAUTHORIZED, detail="API key expired"
            )

    try:
        db.execute(
            update(ApiKey)
            .where(ApiKey.key_id == row.key_id)
            .values(last_used_at=func.now())
        )
        db.commit()
    except Exception:
        db.rollback()

    return row.user_id


def require_admin(
    credentials: HTTPAuthorizationCredentials = Depends(security),
) -> str:
    """Require admin (master key) access."""
    if credentials is None or not _is_master_key(credentials.credentials):
        raise HTTPException(
            status_code=status.HTTP_403_FORBIDDEN, detail="Admin access required"
        )
    return ADMIN_USER_ID


# ── Unified auth context (supports both modes) ─────────────────────


@dataclass
class AuthContext:
    """Resolved authentication result — carries user_id and db_factory."""

    user_id: str
    db_factory: sessionmaker


def get_auth_context(
    request: Request,
    credentials: HTTPAuthorizationCredentials = Depends(security),
    db: Session = Depends(get_db_session),
) -> AuthContext:
    """Unified auth dependency — auto-dispatches to token or apikey mode.

    - If ``X-API-Key`` header is present → apikey mode (remote auth service).
    - Otherwise → token mode (existing ``Authorization: Bearer`` flow).
    """
    apikey = request.headers.get("X-API-Key")

    if apikey:
        return _resolve_apikey(apikey)

    # Fall back to token mode
    if credentials is None:
        raise HTTPException(
            status_code=status.HTTP_401_UNAUTHORIZED,
            detail="Missing Authorization or X-API-Key header",
        )
    user_id = get_current_user_id(request, credentials, db)
    return AuthContext(user_id=user_id, db_factory=get_db_factory())


def _resolve_apikey(apikey: str) -> AuthContext:
    """Resolve an apikey via remote auth service → per-user DB."""
    settings = get_settings()
    if not settings.remote_auth_service_url:
        raise HTTPException(
            status_code=status.HTTP_501_NOT_IMPLEMENTED,
            detail="Apikey mode not configured (REMOTE_AUTH_SERVICE_URL not set)",
        )

    try:
        conn = _get_remote_auth().resolve(apikey)
    except PermissionError:
        raise HTTPException(
            status_code=status.HTTP_401_UNAUTHORIZED,
            detail="Invalid or expired API key",
        )
    except Exception as exc:
        raise HTTPException(
            status_code=status.HTTP_502_BAD_GATEWAY,
            detail=f"Remote auth service unavailable: {exc}",
        )

    from memoria.api.database import get_user_session_factory

    factory = get_user_session_factory(
        host=conn.db_host,
        port=conn.db_port,
        user=conn.db_user,
        password=conn.db_password,
        db_name=conn.db_name,
    )
    return AuthContext(user_id=conn.user_id, db_factory=factory)
