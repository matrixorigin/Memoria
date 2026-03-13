"""Admin endpoints — user management, system stats. Cursor-based pagination."""

from fastapi import APIRouter, Depends
from sqlalchemy import func
from sqlalchemy.orm import Session

from memoria.api.database import get_db_session
from memoria.api.dependencies import require_admin
from memoria.api.models import ApiKey, SnapshotRegistry, User

router = APIRouter(tags=["admin"])


@router.get("/admin/stats")
def system_stats(
    _admin: str = Depends(require_admin),
    db: Session = Depends(get_db_session),
):
    """System-wide stats. Uses indexed COUNT for bounded tables, approximate for large ones."""
    from memoria.core.memory.models.memory import MemoryRecord as M

    total_users = (
        db.query(func.count(User.user_id)).filter(User.is_active == 1).scalar() or 0
    )
    total_memories = (
        db.query(func.count(M.memory_id)).filter(M.is_active > 0).scalar() or 0
    )
    total_snapshots = db.query(func.count(SnapshotRegistry.snapshot_name)).scalar() or 0
    return {
        "total_users": total_users,
        "total_memories": total_memories,
        "total_snapshots": total_snapshots,
    }


@router.get("/admin/users")
def list_users(
    cursor: str | None = None,
    limit: int = 100,
    _admin: str = Depends(require_admin),
    db: Session = Depends(get_db_session),
):
    """List users with cursor-based pagination. Pass last user_id as cursor for next page."""
    q = db.query(User.user_id, User.created_at).filter(User.is_active == 1)
    if cursor:
        q = q.filter(User.user_id > cursor)
    rows = q.order_by(User.user_id).limit(limit).all()
    next_cursor = rows[-1].user_id if len(rows) == limit else None
    return {
        "users": [
            {
                "user_id": r.user_id,
                "created_at": r.created_at.isoformat() if r.created_at else None,
            }
            for r in rows
        ],
        "next_cursor": next_cursor,
    }


@router.get("/admin/users/{user_id}/stats")
def user_stats(
    user_id: str,
    _admin: str = Depends(require_admin),
    db: Session = Depends(get_db_session),
):
    from memoria.core.memory.models.memory import MemoryRecord as M

    mem_count = (
        db.query(func.count(M.memory_id))
        .filter(M.user_id == user_id, M.is_active > 0)
        .scalar()
        or 0
    )
    snap_count = db.query(SnapshotRegistry).filter_by(user_id=user_id).count()
    key_count = db.query(ApiKey).filter_by(user_id=user_id, is_active=1).count()
    return {
        "user_id": user_id,
        "memory_count": mem_count,
        "snapshot_count": snap_count,
        "api_key_count": key_count,
    }


@router.get("/admin/users/{user_id}/keys")
def list_user_keys(
    user_id: str,
    _admin: str = Depends(require_admin),
    db: Session = Depends(get_db_session),
):
    """List all active API keys for a user (admin only)."""
    from memoria.api.routers.auth import _key_to_response

    rows = db.query(ApiKey).filter_by(user_id=user_id, is_active=1).all()
    return {"user_id": user_id, "keys": [_key_to_response(r) for r in rows]}


@router.delete("/admin/users/{user_id}/keys")
def revoke_all_user_keys(
    user_id: str,
    _admin: str = Depends(require_admin),
    db: Session = Depends(get_db_session),
):
    """Revoke all active API keys for a user (admin only)."""
    result = (
        db.query(ApiKey)
        .filter_by(user_id=user_id, is_active=1)
        .update({"is_active": 0})
    )
    db.commit()
    return {"user_id": user_id, "revoked": result}


@router.delete("/admin/users/{user_id}")
def delete_user(
    user_id: str,
    _admin: str = Depends(require_admin),
    db: Session = Depends(get_db_session),
):
    """Deactivate user and revoke all API keys."""
    db.query(User).filter_by(user_id=user_id).update({"is_active": 0})
    db.query(ApiKey).filter_by(user_id=user_id).update({"is_active": 0})
    db.commit()
    return {"status": "ok", "user_id": user_id}


@router.post("/admin/users/{user_id}/reset-access-counts")
def reset_access_counts(
    user_id: str,
    _admin: str = Depends(require_admin),
    db: Session = Depends(get_db_session),
):
    """Reset access_count to 0 for all memories of a user. Used by benchmark before evaluation."""
    from memoria.core.memory.models.memory import MemoryRecord as M

    db.query(M).filter_by(user_id=user_id).update({"access_count": 0})
    db.commit()
    return {"user_id": user_id, "status": "ok"}


@router.post("/admin/users/{user_id}/strategy")
def set_strategy(
    user_id: str,
    strategy: str = "vector:v1",
    _admin: str = Depends(require_admin),
):
    """Set retrieval strategy for a user. Used by benchmark to compare strategies."""
    from memoria.api.database import get_db_factory
    from memoria.core.memory.factory import switch_user_strategy

    result = switch_user_strategy(get_db_factory(), user_id, strategy)
    return {
        "user_id": user_id,
        "strategy": result.strategy_key,
        "previous": result.previous_key,
        "status": result.status,
    }


@router.post("/admin/users/{user_id}/params")
def set_user_params(
    user_id: str,
    params: dict,
    _admin: str = Depends(require_admin),
):
    """Set per-user activation param overrides (stored in params_json)."""
    import json

    from sqlalchemy import text

    from memoria.api.database import get_db_factory

    db_factory = get_db_factory()
    with db_factory() as db:
        db.execute(
            text(
                "UPDATE mem_user_memory_config SET params_json = :pj, updated_at = NOW() "
                "WHERE user_id = :uid"
            ),
            {"uid": user_id, "pj": json.dumps(params)},
        )
        db.commit()
    return {"user_id": user_id, "params": params}


@router.post("/admin/governance/{user_id}/trigger")
def admin_trigger_governance(
    user_id: str,
    op: str = "governance",
    _admin: str = Depends(require_admin),
):
    """Admin triggers governance/consolidate/reflect/extract_entities for a user.

    Runs synchronously, skips all cooldowns.
    Used by benchmark executor for maturation phase.
    """
    valid_ops = (
        "governance",
        "consolidate",
        "reflect",
        "extract_entities",
        "extract_entities_llm",
    )
    if op not in valid_ops:
        from fastapi import HTTPException

        raise HTTPException(
            status_code=400,
            detail=f"Invalid op. Must be one of: {', '.join(valid_ops)}",
        )

    from memoria.api.database import get_db_factory

    db_factory = get_db_factory()

    if op == "governance":
        from memoria.core.memory.tabular.governance import GovernanceScheduler

        r = GovernanceScheduler(db_factory).run_cycle(user_id)
        return {
            "op": op,
            "user_id": user_id,
            "result": {"quarantined": r.quarantined, "cleaned_stale": r.cleaned_stale},
        }

    if op == "extract_entities":
        from memoria.core.memory.strategy.activation_index import ActivationIndexManager

        mgr = ActivationIndexManager(db_factory)
        result = mgr.backfill(user_id)
        return {
            "op": op,
            "user_id": user_id,
            "result": {
                "processed": result.processed,
                "skipped": result.skipped,
                "errors": result.errors[:10],
            },
        }

    if op == "extract_entities_llm":
        from memoria.core.llm import get_llm_client
        from memoria.core.memory.graph.entity_extractor import (
            extract_entities_lightweight,
            extract_entities_llm,
            normalize_entity_name,
        )
        from memoria.core.memory.strategy.activation_index import ActivationIndexManager
        from memoria.core.memory.graph.graph_store import GraphStore

        llm = get_llm_client()

        # Backfill using LLM NER (LLM results merged with regex)
        from memoria.core.memory.tabular.store import MemoryStore

        store = GraphStore(db_factory)
        mem_store = MemoryStore(db_factory)
        memories = mem_store.list_active(user_id, load_embedding=True)

        processed = skipped = 0
        errors: list[str] = []
        for mem in memories:
            try:
                # LLM extraction merged with regex
                seen: set[str] = set()
                entities = []
                for e in extract_entities_llm(mem.content, llm):
                    key = normalize_entity_name(e.name)
                    if key not in seen and len(key) >= 2:
                        seen.add(key)
                        entities.append(e)
                for e in extract_entities_lightweight(mem.content):
                    key = normalize_entity_name(e.name)
                    if key not in seen and len(key) >= 2:
                        seen.add(key)
                        entities.append(e)

                if not entities:
                    skipped += 1
                    continue
                # Get or create graph node for this memory
                node = store.get_node_by_memory_id(mem.memory_id)
                if node:
                    pending_edges: list = []
                    # Re-run entity linking with LLM entities
                    from memoria.core.memory.graph.types import EdgeType

                    entity_id_cache: dict[str, str] = {}
                    with store._db() as db:
                        for ent in entities:
                            if ent.name not in entity_id_cache:
                                entity_id_cache[ent.name] = store._upsert_entity_in(
                                    db,
                                    user_id,
                                    ent.name,
                                    ent.display_name,
                                    ent.entity_type,
                                )
                        if mem.memory_id:
                            for ent in entities:
                                eid = entity_id_cache.get(ent.name)
                                if eid:
                                    store._upsert_link_in(
                                        db, mem.memory_id, eid, user_id, "llm", 0.9
                                    )
                        db.commit()
                    # Add graph edges
                    for ent in entities:
                        eid = entity_id_cache.get(ent.name)
                        if eid:
                            pending_edges.append(
                                (node.node_id, eid, EdgeType.ENTITY_LINK.value, 0.9)
                            )
                    if pending_edges:
                        store.add_edges_batch(pending_edges, user_id)
                    processed += 1
                else:
                    skipped += 1
            except Exception as e:
                errors.append(f"{mem.memory_id}: {e}")

        return {
            "op": op,
            "user_id": user_id,
            "result": {
                "processed": processed,
                "skipped": skipped,
                "errors": errors[:10],
            },
        }

    from memoria.core.memory.factory import create_memory_service

    svc = create_memory_service(db_factory, user_id=user_id)
    if op == "consolidate":
        result = svc.consolidate(user_id)
        return {"op": op, "user_id": user_id, "result": result}

    # reflect
    return {
        "op": op,
        "user_id": user_id,
        "result": "reflect requires LLM — use user endpoint",
    }
