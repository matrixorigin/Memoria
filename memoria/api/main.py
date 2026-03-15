"""Memoria — FastAPI application."""

from contextlib import asynccontextmanager

from fastapi import FastAPI
from fastapi.middleware.cors import CORSMiddleware

from memoria.api.database import init_db


def _init_embedding() -> None:
    """Build EmbeddingClient from Memoria config and inject into core singleton."""
    from memoria.config import get_settings
    from memoria.core.embedding import EmbeddingClient, set_embedding_client
    from memoria.core.embedding.client import KNOWN_DIMENSIONS

    s = get_settings()
    dim = s.embedding_dim
    if dim == 0:
        dim = KNOWN_DIMENSIONS.get(s.embedding_model, 1024)
    set_embedding_client(
        EmbeddingClient(
            provider=s.embedding_provider,
            model=s.embedding_model,
            dim=dim,
            api_key=s.embedding_api_key,
            base_url=s.embedding_base_url,
        )
    )


@asynccontextmanager
async def lifespan(app: FastAPI):
    # Inject embedding client from Memoria config before anything else
    _init_embedding()

    # Warn about weak master key
    from memoria.config import get_settings
    import logging

    for warning in get_settings().warn_weak_master_key():
        logging.getLogger("memoria").warning(warning)

    init_db()

    # Start periodic governance scheduler (hourly/daily/weekly)
    from memoria.core.scheduler import (
        GovernanceTaskRunner,
        AsyncIOBackend,
        MemoryGovernanceScheduler,
    )
    from memoria.api.database import get_db_context, get_db_factory

    runner = GovernanceTaskRunner(
        get_db_context, db_factory=get_db_factory(), memory_only=True
    )
    backend = AsyncIOBackend(runner)
    scheduler = MemoryGovernanceScheduler(backend=backend)
    await scheduler.start()

    yield

    await scheduler.stop()


app = FastAPI(
    title="Memoria",
    description="Multi-tenant memory service with API key auth",
    version="0.1.0",
    lifespan=lifespan,
)

app.add_middleware(
    CORSMiddleware,
    allow_origins=["*"],
    allow_credentials=False,
    allow_methods=["*"],
    allow_headers=["*"],
)

from memoria.api.middleware import RateLimitMiddleware  # noqa: E402

app.add_middleware(RateLimitMiddleware)

from memoria.api.routers import (  # noqa: E402
    auth,
    memory,
    snapshots,
    health,
    admin,
    user_ops,
    sessions,
)

app.include_router(auth.router, prefix="/auth")
app.include_router(memory.router, prefix="/v1")
app.include_router(snapshots.router, prefix="/v1")
app.include_router(user_ops.router, prefix="/v1")
app.include_router(sessions.router, prefix="/v1")
app.include_router(admin.router)
app.include_router(health.router)
