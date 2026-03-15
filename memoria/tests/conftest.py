"""Memoria e2e test configuration.

Each xdist worker gets its own isolated DB (memoria_e2e_gw0, etc.) with dim=1024.
This allows e2e tests to run in parallel safely.
"""

import os
import pytest


def pytest_configure(config):
    ci = os.environ.get("CI") or os.environ.get("GITHUB_ACTIONS")
    provider = os.environ.get("MEMORIA_EMBEDDING_PROVIDER", "local")
    if ci and provider == "local":
        pytest.exit(
            "CI environment detected but MEMORIA_EMBEDDING_PROVIDER=local. "
            "Set MEMORIA_EMBEDDING_PROVIDER=openai and MEMORIA_EMBEDDING_API_KEY secret.",
            returncode=1,
        )


def pytest_collection_modifyitems(items):
    """Keep governance tests on one worker to avoid lock contention."""
    for item in items:
        if "Governance" in item.nodeid or "AdminStatsAccuracy" in item.nodeid:
            item.add_marker(pytest.mark.xdist_group("governance"))


@pytest.fixture(scope="session", autouse=True)
def isolated_e2e_db(request):
    """Give each xdist worker its own e2e DB (dim=1024).

    Without xdist: uses 'memoria' (the default).
    With xdist -n auto: uses 'memoria_e2e_gw0', 'memoria_e2e_gw1', etc.
    """
    worker = os.environ.get("PYTEST_XDIST_WORKER", "")
    if not worker:
        # Serial run — use the real DB as-is
        yield os.environ.get("MEMORIA_DB_NAME", "memoria")
        return

    # Parallel run — give this worker its own DB
    db_name = f"memoria_e2e_{worker}"
    os.environ["MEMORIA_DB_NAME"] = db_name

    # Reset singletons so they pick up the new DB name
    import memoria.api.database as _db_mod
    import memoria.config as _cfg_mod

    _db_mod._engine = None
    _db_mod._SessionLocal = None
    _cfg_mod._settings = None

    # Bootstrap: create DB and tables (dim=1024 from settings)
    from memoria.api.database import init_db

    init_db()

    yield db_name

    # Teardown: drop the worker DB
    try:
        from sqlalchemy import text
        from matrixone import Client as MoClient
        from memoria.config import get_settings

        s = get_settings()
        bootstrap = MoClient(
            host=s.db_host,
            port=s.db_port,
            user=s.db_user,
            password=s.db_password,
            database="mo_catalog",
            sql_log_mode="off",
        )
        with bootstrap._engine.connect() as c:
            c.execute(text(f"DROP DATABASE IF EXISTS `{db_name}`"))
            c.execute(text("COMMIT"))
        bootstrap._engine.dispose()
    except Exception:
        pass
