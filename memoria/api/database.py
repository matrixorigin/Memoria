"""Memoria database engine and session factory."""

import re
import threading
from contextlib import contextmanager
from functools import lru_cache

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


# ── Per-user engine cache (apikey mode) ─────────────────────────────


@lru_cache(maxsize=128)
def _get_user_engine(
    host: str, port: int, user: str, password: str, db_name: str
) -> Engine:
    """Return a cached SQLAlchemy engine for a per-user MatrixOne database."""
    if not re.fullmatch(r"[a-zA-Z0-9_\-]+", db_name):
        raise ValueError(f"Invalid database name: {db_name!r}")

    from matrixone import Client as MoClient

    client = MoClient(
        host=host,
        port=port,
        user=user,
        password=password,
        database=db_name,
        sql_log_mode="off",
    )
    return client._engine


_user_db_initialized: set[str] = set()
_user_db_init_lock = threading.Lock()


def get_user_session_factory(
    host: str, port: int, user: str, password: str, db_name: str
) -> sessionmaker:
    """Return a session factory bound to a per-user engine.

    On first call for a given db_name, ensures memory tables exist.
    """
    engine = _get_user_engine(host, port, user, password, db_name)

    with _user_db_init_lock:
        if db_name not in _user_db_initialized:
            _ensure_user_tables(engine)
            _user_db_initialized.add(db_name)

    return sessionmaker(autocommit=False, autoflush=False, bind=engine)


def _ensure_user_tables(engine: Engine) -> None:
    """Create memory tables in a per-user database (apikey mode)."""
    from memoria.config import get_settings
    from memoria.schema import ensure_tables

    settings = get_settings()
    dim = settings.embedding_dim
    if dim == 0:
        from memoria.core.embedding.client import KNOWN_DIMENSIONS

        dim = KNOWN_DIMENSIONS.get(settings.embedding_model, 1024)
    ensure_tables(engine, dim=dim)

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
