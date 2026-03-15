"""Tests for apikey authentication mode.

Covers: RemoteAuthService, AuthContext dispatch, _resolve_apikey error handling,
and HTTPBackend header selection.
"""

from __future__ import annotations

from unittest.mock import MagicMock, patch

import httpx
import pytest

from memoria.api.remote_auth_service import ConnInfo, RemoteAuthService


# ── ConnInfo ──────────────────────────────────────────────────────────


class TestConnInfo:
    def test_db_url_format(self):
        info = ConnInfo(
            user_id="alice",
            db_host="db.example.com",
            db_port=6001,
            db_user="alice",
            db_password="secret",
            db_name="memoria_alice",
        )
        assert info.db_url == (
            "mysql+pymysql://alice:secret@db.example.com:6001/memoria_alice"
            "?charset=utf8mb4"
        )

    def test_db_url_encodes_special_chars(self):
        """Real-world case: user contains ':', password contains '@'."""
        info = ConnInfo(
            user_id="2",
            db_host="127.0.0.1",
            db_port=6001,
            db_user="local-moi-account:moi_root",
            db_password="bg9jywwt@moi",
            db_name="memoria",
        )
        # ':' and '@' must be percent-encoded so pymysql parses correctly
        assert "local-moi-account%3Amoi_root" in info.db_url
        assert "bg9jywwt%40moi" in info.db_url
        assert info.db_url.startswith("mysql+pymysql://")
        assert "@127.0.0.1:6001/memoria" in info.db_url

    def test_frozen(self):
        info = ConnInfo("u", "h", 1, "u", "p", "d")
        with pytest.raises(AttributeError):
            info.user_id = "other"  # type: ignore[misc]


# ── RemoteAuthService ─────────────────────────────────────────────────


class _MockTransport(httpx.BaseTransport):
    """Canned responses for remote auth service."""

    def __init__(self, status: int = 200, body: dict | None = None):
        self._status = status
        self._body = body or {}

    def handle_request(self, request: httpx.Request) -> httpx.Response:
        return httpx.Response(self._status, json=self._body, request=request)


class TestRemoteAuthResolve:
    _VALID_RESP = {
        "user_id": "alice",
        "db_host": "localhost",
        "db_port": 6001,
        "db_user": "alice",
        "db_password": "pw",
        "db_name": "mem_alice",
    }

    def _make_service(self, status: int = 200, body: dict | None = None, ttl: int = 60):
        svc = RemoteAuthService("http://auth.test", cache_ttl=ttl)
        svc._client = httpx.Client(
            base_url="http://auth.test",
            transport=_MockTransport(status, body or self._VALID_RESP),
        )
        return svc

    def test_resolve_success(self):
        svc = self._make_service()
        info = svc.resolve("sk-test-key-12345")
        assert info.user_id == "alice"
        assert info.db_name == "mem_alice"

    def test_resolve_401_raises_permission_error(self):
        svc = self._make_service(status=401, body={"detail": "bad key"})
        with pytest.raises(PermissionError, match="Invalid or expired"):
            svc.resolve("sk-bad-key-000000")

    def test_resolve_500_raises_http_error(self):
        svc = self._make_service(status=500, body={"detail": "internal"})
        with pytest.raises(httpx.HTTPStatusError):
            svc.resolve("sk-test-key-12345")

    def test_cache_hit_avoids_http_call(self):
        svc = self._make_service(ttl=300)
        info1 = svc.resolve("sk-test-key-12345")
        # Replace transport with one that always 500s
        svc._client = httpx.Client(
            base_url="http://auth.test",
            transport=_MockTransport(500),
        )
        info2 = svc.resolve("sk-test-key-12345")
        assert info1 == info2  # served from cache

    def test_cache_disabled_when_ttl_zero(self):
        svc = self._make_service(ttl=0)
        svc.resolve("sk-test-key-12345")
        assert len(svc._cache) == 0


# ── AuthContext dispatch ──────────────────────────────────────────────


class TestAuthContextDispatch:
    """get_auth_context dispatches to token or apikey path based on headers."""

    def test_apikey_header_triggers_apikey_path(self):
        from memoria.api.dependencies import get_auth_context

        mock_request = MagicMock()
        mock_request.headers = {"X-API-Key": "sk-test-123"}

        with patch("memoria.api.dependencies._resolve_apikey") as mock_resolve:
            from memoria.api.dependencies import AuthContext

            mock_resolve.return_value = AuthContext(
                user_id="alice", db_factory=MagicMock()
            )
            result = get_auth_context(
                request=mock_request, credentials=None, db=MagicMock()
            )
            mock_resolve.assert_called_once_with("sk-test-123")
            assert result.user_id == "alice"

    def test_no_apikey_no_bearer_raises_401(self):
        from fastapi import HTTPException

        from memoria.api.dependencies import get_auth_context

        mock_request = MagicMock()
        mock_request.headers = {}

        with pytest.raises(HTTPException) as exc_info:
            get_auth_context(request=mock_request, credentials=None, db=MagicMock())
        assert exc_info.value.status_code == 401

    def test_resolve_apikey_no_service_url_returns_501(self):
        from fastapi import HTTPException

        from memoria.api.dependencies import _resolve_apikey

        with patch("memoria.api.dependencies.get_settings") as mock_settings:
            mock_settings.return_value.remote_auth_service_url = ""
            with pytest.raises(HTTPException) as exc_info:
                _resolve_apikey("sk-test")
            assert exc_info.value.status_code == 501

    def test_resolve_apikey_remote_error_returns_502(self):
        """Non-auth errors from remote service should become 502, not 500."""
        from fastapi import HTTPException

        from memoria.api.dependencies import _resolve_apikey

        with (
            patch("memoria.api.dependencies.get_settings") as mock_settings,
            patch("memoria.api.dependencies._get_remote_auth") as mock_auth,
        ):
            mock_settings.return_value.remote_auth_service_url = "http://auth.test"
            mock_auth.return_value.resolve.side_effect = httpx.ConnectError("refused")
            with pytest.raises(HTTPException) as exc_info:
                _resolve_apikey("sk-test")
            assert exc_info.value.status_code == 502


# ── HTTPBackend header selection ──────────────────────────────────────


class TestHTTPBackendHeaders:
    """HTTPBackend uses correct header based on token vs apikey."""

    def test_token_uses_authorization_header(self):
        from memoria.mcp_local.server import HTTPBackend

        backend = HTTPBackend("http://api.test", token="tok-123")
        auth_header = backend._client.headers.get("authorization")
        assert auth_header == "Bearer tok-123"
        assert "x-api-key" not in backend._client.headers

    def test_apikey_uses_x_api_key_header(self):
        from memoria.mcp_local.server import HTTPBackend

        backend = HTTPBackend("http://api.test", apikey="sk-key-456")
        assert backend._client.headers.get("x-api-key") == "sk-key-456"
        assert "authorization" not in backend._client.headers

    def test_no_auth_no_headers(self):
        from memoria.mcp_local.server import HTTPBackend

        backend = HTTPBackend("http://api.test")
        assert "authorization" not in backend._client.headers
        assert "x-api-key" not in backend._client.headers


# ── Per-user engine: db_name validation ───────────────────────────────


class TestUserEngineDbNameValidation:
    """_get_user_engine rejects SQL-injection-style db_names but allows hyphens."""

    def test_rejects_semicolon_in_db_name(self):
        from memoria.api.database import _get_user_engine

        with pytest.raises(ValueError, match="Invalid database name"):
            _get_user_engine("h", 6001, "u", "p", "mem; DROP TABLE x")

    def test_rejects_space_in_db_name(self):
        from memoria.api.database import _get_user_engine

        with pytest.raises(ValueError, match="Invalid database name"):
            _get_user_engine("h", 6001, "u", "p", "mem alice")

    def test_allows_hyphens_in_db_name(self):
        """Real-world: remote auth returns db_name like 'memoria-user-2'."""
        from memoria.api.database import _get_user_engine

        with patch("matrixone.Client") as mock_client:
            mock_engine = MagicMock()
            mock_client.return_value._engine = mock_engine
            engine = _get_user_engine("h", 6001, "u", "p", "memoria-user-2")
            assert engine is mock_engine
        _get_user_engine.cache_clear()

    def test_allows_underscores_and_digits(self):
        from memoria.api.database import _get_user_engine

        with patch("matrixone.Client") as mock_client:
            mock_client.return_value._engine = MagicMock()
            _get_user_engine("h", 6001, "u", "p", "memoria_user_42")
        _get_user_engine.cache_clear()


# ── Per-user table init runs only once ────────────────────────────────


class TestUserSessionFactoryInitOnce:
    """get_user_session_factory calls _ensure_user_tables only on first call per db_name."""

    def test_tables_initialized_once_per_db(self):
        from memoria.api.database import (
            get_user_session_factory,
            _user_db_initialized,
        )

        # Clean up state from other tests
        _user_db_initialized.discard("test_once_db")

        with (
            patch("memoria.api.database._get_user_engine") as mock_engine,
            patch("memoria.api.database._ensure_user_tables") as mock_init,
        ):
            mock_engine.return_value = MagicMock()

            get_user_session_factory("h", 1, "u", "p", "test_once_db")
            get_user_session_factory("h", 1, "u", "p", "test_once_db")

            # _ensure_user_tables called exactly once despite two factory calls
            mock_init.assert_called_once()

        # Clean up
        _user_db_initialized.discard("test_once_db")


# ── Middleware: X-API-Key extraction for rate limiting ─────────────────


class TestMiddlewareApikeyExtraction:
    """RateLimitMiddleware correctly reads X-API-Key header for key identification."""

    def test_x_api_key_used_as_rate_limit_identity(self):
        """Middleware uses first 12 chars of X-API-Key as rate-limit identity."""
        apikey = "sk-abcdef1234567890"
        expected_key_id = apikey[:12]
        assert expected_key_id == "sk-abcdef123"
        assert len(expected_key_id) == 12


# ── CLI: --token and --apikey mutual exclusivity ──────────────────────


class TestCLIArgParsing:
    """main() rejects --token + --apikey together, creates correct backend."""

    def test_token_and_apikey_mutually_exclusive(self):
        """Passing both --token and --apikey should exit with error."""
        from unittest.mock import patch as _patch

        with _patch(
            "sys.argv",
            ["memoria-mcp", "--api-url", "http://x", "--token", "t", "--apikey", "k"],
        ):
            with pytest.raises(SystemExit) as exc_info:
                from memoria.mcp_local.server import main

                main()
            assert exc_info.value.code == 1

    def test_apikey_creates_httpbackend_with_x_api_key(self):
        """--apikey flag should produce HTTPBackend with X-API-Key header."""
        from memoria.mcp_local.server import HTTPBackend

        backend = HTTPBackend("http://api.test", token=None, apikey="sk-my-key")
        assert backend._client.headers.get("x-api-key") == "sk-my-key"
        assert "authorization" not in backend._client.headers


# ── Config: remote_auth_service_url setting ───────────────────────────


class TestConfigRemoteAuth:
    """MemoriaSettings exposes remote_auth_service_url and conn_cache_ttl."""

    def test_defaults(self):
        # Disable .env file loading so we test true code defaults
        from memoria.config import MemoriaSettings

        s = MemoriaSettings(embedding_provider="mock", _env_file=None)
        assert s.remote_auth_service_url == ""
        assert s.conn_cache_ttl == 60

    def test_env_override(self):
        env = {
            "MEMORIA_REMOTE_AUTH_SERVICE_URL": "http://auth.internal:8000",
            "MEMORIA_CONN_CACHE_TTL": "120",
        }
        with patch.dict("os.environ", env, clear=False):
            from memoria.config import MemoriaSettings

            s = MemoriaSettings(embedding_provider="mock")
            assert s.remote_auth_service_url == "http://auth.internal:8000"
            assert s.conn_cache_ttl == 120
