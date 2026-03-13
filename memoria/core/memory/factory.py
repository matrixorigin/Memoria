"""Factory for creating memory service with pluggable retrieval strategy.

See docs/design/memory/backend-management.md §4.3
"""

from __future__ import annotations

import logging
import os
from dataclasses import dataclass
from typing import TYPE_CHECKING, Any

from memoria.core.memory.canonical_storage import CanonicalStorage
from memoria.core.memory.service import MemoryService
from memoria.core.memory.strategy.registry import StrategyDescriptor, StrategyRegistry

if TYPE_CHECKING:
    from collections.abc import Callable

    from memoria.core.db_consumer import DbFactory
    from memoria.core.memory.config import MemoryGovernanceConfig

logger = logging.getLogger(__name__)

_SENTINEL = object()  # distinguishes "not passed" from explicit None

# ── Global strategy registry ──────────────────────────────────────────

_registry = StrategyRegistry()


def _register_builtins() -> None:
    """Register built-in strategies."""

    def _vector_factory(
        *,
        db_factory: DbFactory,
        params: dict | None = None,
        config: Any = None,
        metrics: Any = None,
        **kw: Any,
    ) -> Any:
        from memoria.core.memory.strategy.vector_v1 import VectorRetrievalStrategy

        return VectorRetrievalStrategy(
            db_factory,
            params=params,
            config=config,
            metrics=metrics,
        )

    def _activation_factory(
        *,
        db_factory: DbFactory,
        params: dict | None = None,
        config: Any = None,
        metrics: Any = None,
        **kw: Any,
    ) -> Any:
        from memoria.core.memory.strategy.activation_v1 import (
            ActivationRetrievalStrategy,
        )

        return ActivationRetrievalStrategy(
            db_factory,
            params=params,
            config=config,
            metrics=metrics,
        )

    def _activation_index_factory(
        *,
        db_factory: DbFactory,
        params: dict | None = None,
        config: Any = None,
        embed_fn: Any = None,
        **kw: Any,
    ) -> Any:
        from memoria.core.memory.strategy.activation_index import ActivationIndexManager

        return ActivationIndexManager(
            db_factory,
            params=params,
            config=config,
            embed_fn=embed_fn,
        )

    _registry.register("vector:v1", _vector_factory)
    _registry.register("activation:v1", _activation_factory, _activation_index_factory)


_register_builtins()


# ── Backward-compatible mapping ───────────────────────────────────────

_BACKEND_TO_STRATEGY: dict[str, str] = {
    "tabular": "vector:v1",
    "graph": "activation:v1",
}


def _resolve_strategy(
    db_factory: DbFactory | None,
    user_id: str | None,
    backend: str | None,
    strategy: str | None,
) -> tuple[str, dict | None]:
    """Resolve strategy key and per-user param overrides.

    Resolution order (§4.2):
    1. Explicit strategy parameter
    2. Backend name mapped to strategy
    3. Per-user DB row (mem_user_memory_config)
    4. MEM_RETRIEVAL_STRATEGY env var
    5. "vector:v1" hardcoded fallback

    Returns (strategy_key, user_params_or_None).
    """
    if strategy:
        return strategy, None
    if backend:
        mapped = _BACKEND_TO_STRATEGY.get(backend)
        if mapped:
            return mapped, None
        return backend, None
    if user_id and db_factory:
        db_key, user_params = _lookup_user_strategy(db_factory, user_id)
        if db_key:
            return db_key, user_params
    return os.environ.get("MEM_RETRIEVAL_STRATEGY", "activation:v1"), None


def _lookup_user_strategy(
    db_factory: DbFactory, user_id: str
) -> tuple[str | None, dict | None]:
    """Look up per-user strategy and param overrides from mem_user_memory_config."""
    from sqlalchemy import text

    try:
        with db_factory() as db:
            row = db.execute(
                text(
                    "SELECT strategy_key, index_status, params_json "
                    "FROM mem_user_memory_config "
                    "WHERE user_id = :uid"
                ),
                {"uid": user_id},
            ).fetchone()
            if row is None:
                return None, None
            if row.index_status == "backfilling":  # type: ignore[union-attr]
                return None, None
            key = row.strategy_key  # type: ignore[union-attr]
            params_json = row.params_json  # type: ignore[union-attr]
            user_params = None
            if params_json and isinstance(params_json, dict):
                user_params = params_json
            return (key if isinstance(key, str) else None), user_params
    except Exception:
        logger.debug("Failed to look up user strategy for %s", user_id, exc_info=True)
        return None, None


def create_memory_service(
    db_factory: DbFactory,
    *,
    backend: str | None = None,
    strategy: str | None = None,
    user_id: str | None = None,
    params: dict | None = None,
    llm_client: object | None = None,
    embed_fn: Callable | None = None,
    config: MemoryGovernanceConfig | None = None,
) -> MemoryService:
    """Create memory service with pluggable retrieval strategy.

    Args:
        db_factory: Database session factory.
        backend: Legacy backend name ("tabular" or "graph"). Maps to strategy.
        strategy: Explicit strategy key ("vector:v1", "activation:v1").
        user_id: Resolve per-user strategy from DB (§4.2).
        params: Strategy-specific param overrides (validated against schema).
        llm_client: LLM client for memory extraction.
        embed_fn: Embedding function.
        config: Governance configuration.

    Returns:
        MemoryService with canonical storage + selected retrieval strategy.
    """
    strategy_key, user_params = _resolve_strategy(
        db_factory, user_id, backend, strategy
    )

    if config is None:
        from memoria.core.memory.config import DEFAULT_CONFIG

        config = DEFAULT_CONFIG

    # Apply per-user param overrides (from mem_user_memory_config.params_json)
    if user_params:
        from dataclasses import fields as dc_fields

        overrides = {}
        valid_fields = {f.name for f in dc_fields(config)}
        for k, v in user_params.items():
            if k in valid_fields:
                overrides[k] = type(getattr(config, k))(v)
        if overrides:
            from dataclasses import replace

            config = replace(config, **overrides)

    from memoria.core.memory.tabular.metrics import MemoryMetrics

    metrics = MemoryMetrics()

    # Create canonical storage (shared by all strategies)
    storage = CanonicalStorage(
        db_factory,
        llm_client=llm_client,
        embed_fn=embed_fn,
        config=config,
        metrics=metrics,
    )

    # Create retrieval strategy + optional index manager
    descriptor = StrategyDescriptor.parse(strategy_key, params=params)
    retrieval = _registry.create_strategy(
        descriptor,
        db_factory=db_factory,
        config=config,
        metrics=metrics,
    )
    index_manager = _registry.create_index_manager(
        descriptor,
        db_factory=db_factory,
        config=config,
        embed_fn=embed_fn,
    )

    return MemoryService(
        storage=storage,
        retrieval=retrieval,
        index_manager=index_manager,
        db_factory=db_factory,
    )


def create_editor(
    db_factory: DbFactory,
    user_id: str | None = None,
    embed_client: Any | None = _SENTINEL,
) -> Any:
    """Create a MemoryEditor with the appropriate index_manager for the user's strategy.

    Args:
        db_factory: Database session factory.
        user_id: If provided, resolves user's strategy to get the right index_manager.
        embed_client: Embedding client to use. If omitted, auto-resolved from settings.

    Returns:
        MemoryEditor with index_manager wired up.
    """
    from memoria.core.memory.editor import MemoryEditor

    _register_builtins()

    # Resolve embed client: use caller-provided, or auto-resolve from settings.
    if embed_client is _SENTINEL:
        embed_client = None
        try:
            from memoria.core.embedding import get_embedding_client

            embed_client = get_embedding_client()
        except Exception:
            logger.warning(
                "Embedding client not available — memories will be stored without vectors. "
                "Check EMBEDDING_PROVIDER / EMBEDDING_API_KEY configuration.",
                exc_info=True,
            )

    # Wire embed_fn into CanonicalStorage so observe_explicit generates embeddings.
    embed_fn = embed_client.embed if embed_client is not None else None
    storage = CanonicalStorage(db_factory, embed_fn=embed_fn)

    index_manager = None
    if user_id:
        strategy_key, _user_params = _resolve_strategy(
            db_factory, user_id, backend=None, strategy=None
        )
        descriptor = StrategyDescriptor.parse(strategy_key)
        index_manager = _registry.create_index_manager(
            descriptor,
            db_factory=db_factory,
        )

    return MemoryEditor(
        storage, db_factory, index_manager=index_manager, embed_client=embed_client
    )


# ── Per-user strategy binding ─────────────────────────────────────────


@dataclass
class SwitchResult:
    """Result of a strategy switch request."""

    status: str  # "ready" | "backfilling"
    strategy_key: str
    previous_key: str | None = None
    estimated_seconds: int | None = None


def set_user_strategy(
    db_factory: DbFactory,
    user_id: str,
    strategy_key: str,
) -> None:
    """Set or create per-user strategy binding (no backfill check)."""
    from sqlalchemy import text

    with db_factory() as db:
        db.execute(
            text(
                "INSERT INTO mem_user_memory_config (user_id, strategy_key) "
                "VALUES (:uid, :sk) "
                "ON DUPLICATE KEY UPDATE strategy_key = :sk, updated_at = NOW()"
            ),
            {"uid": user_id, "sk": strategy_key},
        )
        db.commit()


def switch_user_strategy(
    db_factory: DbFactory,
    user_id: str,
    new_strategy: str,
) -> SwitchResult:
    """Switch a user's retrieval strategy, with backfill if needed.

    If the new strategy has an IndexManager that needs backfill,
    marks status as 'backfilling' and runs backfill synchronously
    (async job integration is Phase 4).

    Returns SwitchResult with status and previous strategy.
    """
    from sqlalchemy import text

    descriptor = StrategyDescriptor.parse(new_strategy)
    # Validate strategy exists
    _registry.create_strategy(descriptor, db_factory=db_factory)

    # Get current strategy
    previous_key: str | None = None
    with db_factory() as db:
        row = db.execute(
            text(
                "SELECT strategy_key FROM mem_user_memory_config WHERE user_id = :uid"
            ),
            {"uid": user_id},
        ).fetchone()
        if row:
            previous_key = row.strategy_key  # type: ignore[union-attr]

    if previous_key == new_strategy:
        return SwitchResult(
            status="ready", strategy_key=new_strategy, previous_key=previous_key
        )

    # Check if backfill is needed
    index_mgr = _registry.create_index_manager(descriptor, db_factory=db_factory)
    needs_backfill = index_mgr and index_mgr.backfill_needed(user_id)

    if needs_backfill:
        # Mark as backfilling
        _upsert_user_config(
            db_factory,
            user_id,
            new_strategy,
            index_status="backfilling",
            migrated_from=previous_key,
        )
        # Run backfill (synchronous for now; async job in Phase 4)
        try:
            index_mgr.backfill(user_id)  # type: ignore[union-attr]
            _upsert_user_config(
                db_factory,
                user_id,
                new_strategy,
                index_status="ready",
                migrated_from=previous_key,
            )
            return SwitchResult(
                status="ready",
                strategy_key=new_strategy,
                previous_key=previous_key,
            )
        except Exception:
            _upsert_user_config(
                db_factory,
                user_id,
                previous_key or "vector:v1",
                index_status="failed",
                migrated_from=previous_key,
            )
            raise
    else:
        set_user_strategy(db_factory, user_id, new_strategy)
        return SwitchResult(
            status="ready",
            strategy_key=new_strategy,
            previous_key=previous_key,
        )


def _upsert_user_config(
    db_factory: DbFactory,
    user_id: str,
    strategy_key: str,
    *,
    index_status: str = "ready",
    migrated_from: str | None = None,
) -> None:
    """Upsert mem_user_memory_config row (atomic)."""
    from sqlalchemy import text

    with db_factory() as db:
        db.execute(
            text(
                "INSERT INTO mem_user_memory_config "
                "(user_id, strategy_key, index_status, migrated_from) "
                "VALUES (:uid, :sk, :st, :mf) "
                "ON DUPLICATE KEY UPDATE "
                "strategy_key = :sk, index_status = :st, "
                "migrated_from = :mf, updated_at = NOW()"
            ),
            {
                "uid": user_id,
                "sk": strategy_key,
                "st": index_status,
                "mf": migrated_from,
            },
        )
        db.commit()
