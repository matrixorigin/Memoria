"""Memoria database engine and session factory."""

import collections
import logging
import re
import threading
from contextlib import contextmanager

from sqlalchemy import Engine, text
from sqlalchemy.orm import sessionmaker

from memoria.config import get_settings

_engine = None
_SessionLocal = None
_mo_client = None

# Export SessionLocal for backward compatibility
SessionLocal = None


def _get_engine():
    global _engine, _mo_client
    if _engine is None:
        settings = get_settings()
        from matrixone import Client as MoClient

        # Validate db_name to prevent SQL injection via config
        import re

        if not re.fullmatch(r"[a-zA-Z0-9_]+", settings.db_name):
            raise ValueError(f"Invalid database name: {settings.db_name!r}")

        bootstrap = MoClient(
            host=settings.db_host,
            port=settings.db_port,
            user=settings.db_user,
            password=settings.db_password,
            database="mo_catalog",
            sql_log_mode="off",
        )
        with bootstrap._engine.begin() as c:
            c.execute(text(f"CREATE DATABASE IF NOT EXISTS `{settings.db_name}`"))
        bootstrap._engine.dispose()

        _mo_client = MoClient(
            host=settings.db_host,
            port=settings.db_port,
            user=settings.db_user,
            password=settings.db_password,
            database=settings.db_name,
            sql_log_mode="off",
        )
        _engine = _mo_client._engine
    return _engine


def get_session_factory():
    global _SessionLocal, SessionLocal
    if _SessionLocal is None:
        _SessionLocal = sessionmaker(
            autocommit=False, autoflush=False, bind=_get_engine()
        )
        SessionLocal = _SessionLocal
    return _SessionLocal


def get_db_session():
    factory = get_session_factory()
    db = factory()
    try:
        yield db
    except Exception:
        db.rollback()
        raise
    finally:
        db.close()


def get_db_factory():
    return get_session_factory()


@contextmanager
def get_db_context():
    factory = get_session_factory()
    db = factory()
    try:
        yield db
        db.commit()
    except Exception:
        db.rollback()
        raise
    finally:
        db.close()


def init_db():
    from sqlalchemy.sql.ddl import CreateIndex, CreateTable

    from memoria.api.models import Base

    engine = _get_engine()
    # Use IF NOT EXISTS instead of checkfirst to avoid TOCTOU races
    # when multiple processes initialise the same database concurrently
    # (e.g. pytest-xdist workers).  MatrixOne's dialect supports has_table()
    # but the check-then-create is not atomic, so a parallel worker can
    # slip in between the check and the CREATE TABLE.
    with engine.begin() as conn:
        for table in Base.metadata.sorted_tables:
            conn.execute(CreateTable(table, if_not_exists=True))
            for idx in table.indexes:
                try:
                    conn.execute(CreateIndex(idx))
                except Exception as exc:
                    # 1061 = duplicate key name (index already exists)
                    if "1061" not in str(exc):
                        raise

    from memoria.schema import ensure_tables

    settings = get_settings()
    dim = settings.embedding_dim
    if dim == 0:
        from memoria.core.embedding.client import KNOWN_DIMENSIONS

        dim = KNOWN_DIMENSIONS.get(settings.embedding_model, 1024)
    ensure_tables(engine, dim=dim)

    # Governance infrastructure tables (used by scheduler)
    with engine.begin() as c:
        c.execute(
            text(
                "CREATE TABLE IF NOT EXISTS infra_distributed_locks ("
                "  lock_name VARCHAR(64) PRIMARY KEY,"
                "  instance_id VARCHAR(64) NOT NULL,"
                "  acquired_at DATETIME(6) NOT NULL DEFAULT NOW(),"
                "  expires_at DATETIME(6) NOT NULL,"
                "  task_name VARCHAR(255) NOT NULL"
                ")"
            )
        )
        c.execute(
            text(
                "CREATE TABLE IF NOT EXISTS governance_runs ("
                "  id BIGINT AUTO_INCREMENT PRIMARY KEY,"
                "  task_name VARCHAR(255) NOT NULL,"
                "  result TEXT,"
                "  created_at DATETIME(6) NOT NULL DEFAULT NOW(),"
                "  INDEX idx_governance_runs_task (task_name)"
                ")"
            )
        )


# ── Per-user engine cache with LRU eviction (apikey mode) ───────────
#
# Each user (= unique account) gets its own Engine with a small pool
# (default pool_size=1, max_overflow=2 → max 3 connections per user).
# An LRU cache keeps the most recently active engines; evicted engines
# are dispose()d so their connections are properly closed.
#
# With 256 cached engines × 3 max connections = 768 max connections,
# well within MatrixOne's limits.

_log = logging.getLogger(__name__)

# key = (host, port, user, password, db_name)
_UserEngineKey = tuple[str, int, str, str, str]


class _EngineEntry:
    """Holds an Engine + its sessionmaker so both can be reused."""

    __slots__ = ("engine", "factory")

    def __init__(self, engine: Engine, factory: sessionmaker) -> None:
        self.engine = engine
        self.factory = factory


class _UserEngineCache:
    """Thread-safe LRU cache for per-user SQLAlchemy engines.

    When an entry is evicted, ``engine.dispose()`` is called to release
    all pooled connections back to the database.
    """

    def __init__(self, maxsize: int = 256) -> None:
        self._maxsize = maxsize
        self._lock = threading.Lock()
        # OrderedDict gives us O(1) LRU: move_to_end on access, popitem(last=False) on eviction
        self._cache: collections.OrderedDict[_UserEngineKey, _EngineEntry] = (
            collections.OrderedDict()
        )

    def get(self, key: _UserEngineKey) -> _EngineEntry | None:
        with self._lock:
            entry = self._cache.get(key)
            if entry is not None:
                self._cache.move_to_end(key)  # mark as recently used
            return entry

    def put(self, key: _UserEngineKey, entry: _EngineEntry) -> None:
        with self._lock:
            if key in self._cache:
                self._cache.move_to_end(key)
                self._cache[key] = entry
                return
            self._cache[key] = entry
            # Evict oldest if over capacity
            while len(self._cache) > self._maxsize:
                evicted_key, evicted = self._cache.popitem(last=False)
                _log.info(
                    "Evicting user engine %s:%d/%s (pool disposed)",
                    evicted_key[0],
                    evicted_key[1],
                    evicted_key[4],
                )
                try:
                    evicted.engine.dispose()
                except Exception:
                    _log.warning(
                        "Failed to dispose engine for %s", evicted_key, exc_info=True
                    )

    def discard(self, key: _UserEngineKey) -> None:
        with self._lock:
            entry = self._cache.pop(key, None)
            if entry is not None:
                try:
                    entry.engine.dispose()
                except Exception:
                    pass


_user_engine_cache = _UserEngineCache()

# Track (host, port, db_name) to avoid redundant table init.
_user_db_initialized: set[tuple[str, int, str]] = set()
_user_db_init_lock = threading.Lock()


def _create_user_engine(
    host: str,
    port: int,
    user: str,
    password: str,
    db_name: str,
) -> Engine:
    """Create a small-pool Engine for a single user's database."""
    from sqlalchemy import create_engine
    from sqlalchemy.engine import URL

    settings = get_settings()

    url = URL.create(
        drivername="mysql+pymysql",
        username=user,
        password=password,
        host=host,
        port=port,
        database=db_name,
    )
    return create_engine(
        url,
        pool_size=settings.user_pool_size,
        max_overflow=settings.user_pool_max_overflow,
        pool_timeout=30,
        pool_recycle=3600,
        pool_pre_ping=True,
    )


def get_user_session_factory(
    host: str, port: int, user: str, password: str, db_name: str
) -> sessionmaker:
    """Return a cached session factory for a per-user database.

    Each unique (host, port, user, password, db_name) gets its own Engine
    with a small connection pool. An LRU cache evicts cold users and
    calls ``dispose()`` to release their connections.
    """
    if not re.fullmatch(r"[a-zA-Z0-9_\-]+", db_name):
        raise ValueError(f"Invalid database name: {db_name!r}")

    key: _UserEngineKey = (host, port, user, password, db_name)
    entry = _user_engine_cache.get(key)
    if entry is not None:
        return entry.factory

    # Create new engine + factory
    engine = _create_user_engine(host, port, user, password, db_name)

    init_key = (host, port, db_name)
    with _user_db_init_lock:
        if init_key not in _user_db_initialized:
            _ensure_user_tables(engine, db_name)
            _user_db_initialized.add(init_key)

    factory = sessionmaker(autocommit=False, autoflush=False, bind=engine)
    _user_engine_cache.put(key, _EngineEntry(engine, factory))
    return factory


def _ensure_user_tables(engine: Engine, db_name: str) -> None:
    """Create memory tables in a per-user database (apikey mode).

    Does NOT call ``ensure_database`` — in apikey mode the remote auth
    service has already created the database.
    """
    from memoria.schema import TABLE_NAMES, _ddl_statements, _ensure_entity_type_column

    settings = get_settings()
    dim = settings.embedding_dim
    if dim == 0:
        from memoria.core.embedding.client import KNOWN_DIMENSIONS

        dim = KNOWN_DIMENSIONS.get(settings.embedding_model, 1024)

    stmts = _ddl_statements(dim)

    with engine.connect() as conn:
        for _name, ddl in zip(TABLE_NAMES, stmts):
            conn.execute(text(ddl))
        _ensure_entity_type_column(conn)
        conn.commit()

    # Governance infrastructure tables (for on-demand governance)
    with engine.begin() as c:
        c.execute(
            text(
                "CREATE TABLE IF NOT EXISTS infra_distributed_locks ("
                "  lock_name VARCHAR(64) PRIMARY KEY,"
                "  instance_id VARCHAR(64) NOT NULL,"
                "  acquired_at DATETIME(6) NOT NULL DEFAULT NOW(),"
                "  expires_at DATETIME(6) NOT NULL,"
                "  task_name VARCHAR(255) NOT NULL"
                ")"
            )
        )
        c.execute(
            text(
                "CREATE TABLE IF NOT EXISTS governance_runs ("
                "  id BIGINT AUTO_INCREMENT PRIMARY KEY,"
                "  task_name VARCHAR(255) NOT NULL,"
                "  result TEXT,"
                "  created_at DATETIME(6) NOT NULL DEFAULT NOW(),"
                "  INDEX idx_governance_runs_task (task_name)"
                ")"
            )
        )
