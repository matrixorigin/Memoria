"""Integration test fixtures — uses Memoria's own database.

Under xdist (-n auto), each worker gets its own isolated DB
(memoria_test_w0, memoria_test_w1, …) that is dropped after the session.
Without xdist, uses memoria_test.
"""

import os
import pytest
from sqlalchemy import text
from sqlalchemy.orm import sessionmaker

_engine = None
_SessionLocal = None
_worker_db_name: str | None = None


def _db_name() -> str:
    global _worker_db_name
    if _worker_db_name is not None:
        return _worker_db_name
    worker = os.environ.get("PYTEST_XDIST_WORKER", "")  # e.g. "gw0", "gw1"
    base = os.environ.get("MEMORIA_DB_NAME", "memoria_test")
    _worker_db_name = f"{base}_{worker}" if worker else base
    return _worker_db_name


def _get_engine():
    global _engine
    if _engine is not None:
        return _engine
    from matrixone import Client as MoClient

    host = os.environ.get("MEMORIA_DB_HOST", "localhost")
    port = int(os.environ.get("MEMORIA_DB_PORT", "6001"))
    user = os.environ.get("MEMORIA_DB_USER", "root")
    password = os.environ.get("MEMORIA_DB_PASSWORD", "111")
    db_name = _db_name()

    bootstrap = MoClient(
        host=host,
        port=port,
        user=user,
        password=password,
        database="mo_catalog",
        sql_log_mode="off",
    )
    with bootstrap._engine.connect() as c:
        c.execute(text(f"CREATE DATABASE IF NOT EXISTS `{db_name}`"))
        c.execute(text("COMMIT"))
    bootstrap._engine.dispose()

    client = MoClient(
        host=host,
        port=port,
        user=user,
        password=password,
        database=db_name,
        sql_log_mode="off",
    )
    _engine = client._engine
    return _engine


def _get_session_local():
    global _SessionLocal
    if _SessionLocal is None:
        _SessionLocal = sessionmaker(bind=_get_engine())
    return _SessionLocal


def _init_tables():
    engine = _get_engine()
    from memoria.schema import ensure_tables

    ensure_tables(engine, dim=384, force=True)
    # Note: Base.metadata.create_all() is NOT needed — ensure_tables() creates all tables
    # including those defined in ORM models. Calling it would recreate tables without
    # schema migrations (e.g., extra_metadata column).


def _drop_worker_db():
    """Drop the per-worker DB after the session (xdist only)."""
    worker = os.environ.get("PYTEST_XDIST_WORKER", "")
    if not worker:
        return  # don't drop the shared DB in non-parallel runs
    db_name = _db_name()
    from matrixone import Client as MoClient

    host = os.environ.get("MEMORIA_DB_HOST", "localhost")
    port = int(os.environ.get("MEMORIA_DB_PORT", "6001"))
    user = os.environ.get("MEMORIA_DB_USER", "root")
    password = os.environ.get("MEMORIA_DB_PASSWORD", "111")
    try:
        client = MoClient(
            host=host,
            port=port,
            user=user,
            password=password,
            database="mo_catalog",
            sql_log_mode="off",
        )
        with client._engine.connect() as c:
            c.execute(text(f"DROP DATABASE IF EXISTS `{db_name}`"))
            c.execute(text("COMMIT"))
        client._engine.dispose()
    except Exception as e:
        print(f"Warning: failed to drop worker DB {db_name}: {e}")


@pytest.fixture(scope="session", autouse=True)
def _init_db(request):
    _init_tables()
    request.addfinalizer(_drop_worker_db)


@pytest.fixture(scope="session")
def db_factory():
    return _get_session_local()


@pytest.fixture
def db(db_factory):
    session = db_factory()
    yield session
    session.rollback()
    session.close()


@pytest.fixture(scope="session")
def embed_client():
    """Mock embedding client — no sentence_transformers required."""
    from memoria.core.embedding import EmbeddingClient, set_embedding_client

    client = EmbeddingClient(provider="mock", model="mock", dim=384)
    set_embedding_client(client)
    return client
