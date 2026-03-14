"""Memory CRUD + retrieval endpoints (SaaS version, no experiments)."""

from __future__ import annotations

import logging
from datetime import datetime
from typing import Any

from fastapi import APIRouter, Depends, HTTPException, status
from pydantic import BaseModel, Field

from memoria.api.database import get_db_factory
from memoria.api.dependencies import get_current_user_id
from memoria.core.explain import init_explain, clear_explain

logger = logging.getLogger(__name__)
router = APIRouter(tags=["memory"])


# ── Schemas ───────────────────────────────────────────────────────────


class StoreRequest(BaseModel):
    content: str = Field(..., min_length=1)
    memory_type: str = Field(default="semantic")
    trust_tier: str | None = None
    session_id: str | None = None
    source: str = "api"
    observed_at: datetime | None = None  # benchmark: override timestamp for decay tests
    initial_confidence: float | None = None  # benchmark: override confidence


class BatchStoreRequest(BaseModel):
    memories: list[StoreRequest] = Field(..., min_length=1)


class RetrieveRequest(BaseModel):
    query: str = Field(..., min_length=1)
    top_k: int = Field(default=10, ge=1, le=100)
    memory_types: list[str] | None = None
    session_id: str | None = None
    include_cross_session: bool = True
    explain: str = Field(
        default="none", description="Explain level: none, basic, verbose, analyze"
    )


class CorrectRequest(BaseModel):
    new_content: str = Field(..., min_length=1)
    reason: str = ""


class CorrectByQueryRequest(BaseModel):
    query: str = Field(..., min_length=1)
    new_content: str = Field(..., min_length=1)
    reason: str = ""


class PurgeRequest(BaseModel):
    memory_ids: list[str] | None = None
    topic: str | None = None  # bulk-delete by keyword match
    memory_types: list[str] | None = None
    before: datetime | None = None
    reason: str = ""


class SearchRequest(BaseModel):
    query: str = Field(..., min_length=1)
    top_k: int = Field(default=10, ge=1, le=100)
    explain: str = Field(
        default="none", description="Explain level: none, basic, verbose, analyze"
    )


class ObserveRequest(BaseModel):
    messages: list[dict[str, Any]] = Field(..., min_length=1)
    source_event_ids: list[str] | None = None


_CURSOR_FMT = "%Y-%m-%d %H:%M:%S.%f"


# ── Helpers ───────────────────────────────────────────────────────────


def _to_response(mem: Any) -> dict[str, Any]:
    from memoria.core.memory.types import enum_value

    mem_type_str = enum_value(mem.memory_type) or "fact"
    trust_tier_str = enum_value(mem.trust_tier) if hasattr(mem, "trust_tier") else None

    return {
        "memory_id": mem.memory_id,
        "content": mem.content,
        "memory_type": mem_type_str,
        "trust_tier": trust_tier_str,
        "confidence": getattr(mem, "initial_confidence", None),
        "observed_at": mem.observed_at.isoformat()
        if hasattr(mem, "observed_at") and mem.observed_at
        else None,
        "retrieval_score": getattr(mem, "retrieval_score", None),
    }


def _get_editor(db_factory, user_id: str):
    from memoria.core.memory.factory import create_editor

    return create_editor(db_factory, user_id=user_id)


def _verify_ownership(db_factory, memory_id: str, user_id: str):
    """Verify memory belongs to user. Raises 404 if not found or not owned."""
    from memoria.core.memory.models.memory import MemoryRecord as M

    db = db_factory()
    try:
        row = (
            db.query(M.user_id)
            .filter_by(memory_id=memory_id)
            .filter(M.is_active > 0)
            .first()
        )
        if row is None or row.user_id != user_id:
            raise HTTPException(status_code=404, detail="Memory not found")
    finally:
        db.close()


def _get_service(db_factory, user_id: str):
    from memoria.core.memory.factory import create_memory_service

    return create_memory_service(db_factory, user_id=user_id)


# ── Endpoints ─────────────────────────────────────────────────────────


@router.get("/memories")
def list_memories(
    memory_type: str | None = None,
    limit: int = 100,
    cursor: str | None = None,
    user_id: str = Depends(get_current_user_id),
    db_factory=Depends(get_db_factory),
):
    """List active memories for the current user. Cursor-based pagination (pass last observed_at|memory_id)."""
    from memoria.core.memory.models.memory import MemoryRecord as M
    from sqlalchemy import or_, and_

    db = db_factory()
    try:
        if limit > 500:
            limit = 500
        q = db.query(
            M.memory_id, M.content, M.memory_type, M.initial_confidence, M.observed_at
        ).filter(
            M.user_id == user_id,
            M.is_active > 0,
        )
        if memory_type:
            q = q.filter(M.memory_type == memory_type)
        if cursor:
            parts = cursor.split("|", 1)
            if len(parts) == 2:
                try:
                    cursor_ts = datetime.strptime(parts[0], _CURSOR_FMT)
                except ValueError:
                    raise HTTPException(status_code=422, detail="Invalid cursor format")
                q = q.filter(
                    or_(
                        M.observed_at < cursor_ts,
                        and_(M.observed_at == cursor_ts, M.memory_id < parts[1]),
                    )
                )
        rows = q.order_by(M.observed_at.desc(), M.memory_id.desc()).limit(limit).all()
        items = [
            {
                "memory_id": r.memory_id,
                "content": r.content,
                "memory_type": r.memory_type,
                "confidence": r.initial_confidence,
                "observed_at": r.observed_at.strftime(_CURSOR_FMT)
                if r.observed_at
                else None,
            }
            for r in rows
        ]
        next_cursor = None
        if len(rows) == limit and rows:
            last = rows[-1]
            ts = last.observed_at.strftime(_CURSOR_FMT) if last.observed_at else ""
            next_cursor = f"{ts}|{last.memory_id}"
        return {"items": items, "next_cursor": next_cursor}
    finally:
        db.close()


@router.post("/memories", status_code=status.HTTP_201_CREATED)
def store_memory(
    req: StoreRequest,
    user_id: str = Depends(get_current_user_id),
    db_factory=Depends(get_db_factory),
):
    from memoria.core.memory.types import MemoryType, TrustTier

    editor = _get_editor(db_factory, user_id)
    try:
        mem = editor.inject(
            user_id,
            req.content,
            memory_type=MemoryType(req.memory_type),
            trust_tier=TrustTier(req.trust_tier) if req.trust_tier else None,
            source=req.source,
            session_id=req.session_id,
            observed_at=req.observed_at,
            initial_confidence=req.initial_confidence
            if req.initial_confidence is not None
            else 1.0,
        )
    except ValueError as e:
        raise HTTPException(status_code=422, detail=str(e))
    return _to_response(mem)


@router.post("/memories/batch", status_code=status.HTTP_201_CREATED)
def batch_store(
    req: BatchStoreRequest,
    user_id: str = Depends(get_current_user_id),
    db_factory=Depends(get_db_factory),
):
    from memoria.core.memory.types import MemoryType

    editor = _get_editor(db_factory, user_id)
    specs = [
        {
            "content": m.content,
            "memory_type": MemoryType(m.memory_type),
            "source": m.source,
        }
        for m in req.memories
    ]
    memories = editor.batch_inject(user_id, specs, source="api_batch")
    return [_to_response(m) for m in memories]


@router.post("/memories/retrieve")
def retrieve_memories(
    req: RetrieveRequest,
    user_id: str = Depends(get_current_user_id),
    db_factory=Depends(get_db_factory),
) -> dict[str, Any]:
    from memoria.core.memory.types import MemoryType

    # Initialize explain context
    explain_ctx = init_explain(req.explain)

    try:
        svc = _get_service(db_factory, user_id=user_id)
        memory_types = (
            [MemoryType(t) for t in req.memory_types] if req.memory_types else None
        )

        # Embed query for vector search and activation strategy
        query_embedding = None
        try:
            from memoria.core.embedding import get_embedding_client

            query_embedding = get_embedding_client().embed(req.query)
        except Exception as e:
            logger.warning(
                "retrieve: failed to embed query, graph/vector path degraded: %s", e
            )

        memories, _meta = svc.retrieve(
            user_id,
            req.query,
            query_embedding=query_embedding,
            top_k=req.top_k,
            memory_types=memory_types,
            session_id=req.session_id or "",
            include_cross_session=req.include_cross_session,
            explain=explain_ctx is not None,
        )

        response: dict[str, Any] = {"results": [_to_response(m) for m in memories]}

        # Add explain output if requested
        if explain_ctx is not None:
            explain_ctx.finish()
            result = explain_ctx.to_dict()

            # Bridge old explain system data if available
            if _meta:
                if isinstance(_meta, dict):
                    if "path" in _meta:
                        result["path"] = _meta["path"]
                    # Merge any other metadata
                    if "metrics" not in result:
                        result["metrics"] = {}
                    for k, v in _meta.items():
                        if k != "path":
                            result["metrics"][k] = v

            response["explain"] = result

        return response
    finally:
        clear_explain()


@router.post("/memories/search")
def search_memories(
    req: SearchRequest,
    user_id: str = Depends(get_current_user_id),
    db_factory=Depends(get_db_factory),
) -> dict[str, Any]:
    # Initialize explain context
    explain_ctx = init_explain(req.explain)

    try:
        svc = _get_service(db_factory, user_id=user_id)

        query_embedding = None
        try:
            from memoria.core.embedding import get_embedding_client

            query_embedding = get_embedding_client().embed(req.query)
        except Exception as e:
            logger.warning(
                "search: failed to embed query, vector search degraded: %s", e
            )

        memories, _meta = svc.retrieve(
            user_id,
            req.query,
            query_embedding=query_embedding,
            top_k=req.top_k,
            explain=explain_ctx is not None,
        )

        response: dict[str, Any] = {"results": [_to_response(m) for m in memories]}

        # Add explain output if requested
        if explain_ctx is not None:
            explain_ctx.finish()
            result = explain_ctx.to_dict()

            # Bridge old explain system data if available
            if _meta:
                if isinstance(_meta, dict):
                    if "path" in _meta:
                        result["path"] = _meta["path"]
                    # Merge any other metadata
                    if "metrics" not in result:
                        result["metrics"] = {}
                    for k, v in _meta.items():
                        if k != "path":
                            result["metrics"][k] = v

            response["explain"] = result

        return response
    finally:
        clear_explain()


@router.put("/memories/{memory_id}/correct")
def correct_memory(
    memory_id: str,
    req: CorrectRequest,
    user_id: str = Depends(get_current_user_id),
    db_factory=Depends(get_db_factory),
):
    _verify_ownership(db_factory, memory_id, user_id)
    editor = _get_editor(db_factory, user_id)
    try:
        mem = editor.correct(user_id, memory_id, req.new_content, reason=req.reason)
    except ValueError as e:
        raise HTTPException(status_code=404, detail=str(e))
    return _to_response(mem)


@router.post("/memories/correct")
def correct_by_query(
    req: CorrectByQueryRequest,
    user_id: str = Depends(get_current_user_id),
    db_factory=Depends(get_db_factory),
):
    """Find the best-matching memory by semantic search and correct it."""
    editor = _get_editor(db_factory, user_id)
    match = editor.find_best_match(user_id, req.query)
    if match is None:
        raise HTTPException(status_code=404, detail="No matching memory found")
    _verify_ownership(db_factory, match.memory_id, user_id)
    mem = editor.correct(user_id, match.memory_id, req.new_content, reason=req.reason)
    return {
        **_to_response(mem),
        "matched_memory_id": match.memory_id,
        "matched_content": match.content,
    }


@router.delete("/memories/{memory_id}")
def delete_memory(
    memory_id: str,
    reason: str = "",
    user_id: str = Depends(get_current_user_id),
    db_factory=Depends(get_db_factory),
):
    _verify_ownership(db_factory, memory_id, user_id)
    editor = _get_editor(db_factory, user_id)
    result = editor.purge(user_id, memory_ids=[memory_id], reason=reason)
    return {"purged": result.deactivated}


@router.post("/memories/purge")
def purge_memories(
    req: PurgeRequest,
    user_id: str = Depends(get_current_user_id),
    db_factory=Depends(get_db_factory),
):
    from memoria.core.memory.types import MemoryType

    editor = _get_editor(db_factory, user_id)

    # Topic purge: find matching memory_ids via fulltext search, then purge by ID
    if req.topic and not req.memory_ids:
        from sqlalchemy import text as sa_text

        try:
            from matrixone.sqlalchemy_ext import boolean_match
            from memoria.core.memory.models.memory import MemoryRecord

            with db_factory() as db:
                ft = boolean_match("content").must(req.topic)
                rows = (
                    db.query(MemoryRecord.memory_id)
                    .filter_by(user_id=user_id, is_active=1)
                    .filter(ft)
                    .all()
                )
        except Exception:
            # Fallback to LIKE if fulltext unavailable
            with db_factory() as db:
                rows = db.execute(
                    sa_text(
                        "SELECT memory_id FROM mem_memories "
                        "WHERE user_id = :uid AND is_active = 1 AND content LIKE :pat"
                    ),
                    {"uid": user_id, "pat": f"%{req.topic}%"},
                ).fetchall()

        ids = [r[0] for r in rows]
        if not ids:
            return {"purged": 0}
        result = editor.purge(
            user_id, memory_ids=ids, reason=req.reason or f"topic purge: {req.topic}"
        )
        return {"purged": result.deactivated}

    memory_types = (
        [MemoryType(t) for t in req.memory_types] if req.memory_types else None
    )
    result = editor.purge(
        user_id,
        memory_ids=req.memory_ids,
        memory_types=memory_types,
        before=req.before,
        reason=req.reason,
    )
    return {"purged": result.deactivated}


@router.get("/profiles/{target_user_id}")
def get_profile(
    target_user_id: str,
    user_id: str = Depends(get_current_user_id),
    db_factory=Depends(get_db_factory),
):
    # "me" resolves to the authenticated user
    resolved = user_id if target_user_id == "me" else target_user_id
    svc = _get_service(db_factory, user_id=resolved)
    profile = svc.get_profile(resolved)

    # Enrich with stats for quality assessment
    from sqlalchemy import func as sa_func
    from memoria.core.memory.models.memory import MemoryRecord as M

    db = db_factory()
    try:
        stats: dict[str, Any] = {}
        rows = (
            db.query(M.memory_type, sa_func.count())
            .filter(
                M.user_id == resolved,
                M.is_active > 0,
            )
            .group_by(M.memory_type)
            .all()
        )
        stats["by_type"] = {str(r[0]): r[1] for r in rows}
        stats["total"] = sum(r[1] for r in rows)
        stats["avg_confidence"] = (
            db.query(sa_func.round(sa_func.avg(M.initial_confidence), 2))
            .filter(M.user_id == resolved, M.is_active > 0)
            .scalar()
        )
        stats["oldest"] = (
            db.query(sa_func.min(M.observed_at))
            .filter(
                M.user_id == resolved,
                M.is_active > 0,
            )
            .scalar()
        )
        stats["newest"] = (
            db.query(sa_func.max(M.observed_at))
            .filter(
                M.user_id == resolved,
                M.is_active > 0,
            )
            .scalar()
        )
        if stats["oldest"]:
            stats["oldest"] = stats["oldest"].isoformat()
        if stats["newest"]:
            stats["newest"] = stats["newest"].isoformat()
    finally:
        db.close()

    return {"user_id": resolved, "profile": profile, "stats": stats}


@router.post("/observe")
def observe_turn(
    req: ObserveRequest,
    user_id: str = Depends(get_current_user_id),
    db_factory=Depends(get_db_factory),
):
    svc = _get_service(db_factory, user_id=user_id)
    memories = svc.observe_turn(
        user_id, req.messages, source_event_ids=req.source_event_ids
    )
    return [_to_response(m) for m in memories]
