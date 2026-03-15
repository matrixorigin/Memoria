"""Snapshot endpoints — MatrixOne native snapshots, read-only, no rollback, 100 per user."""

from __future__ import annotations

import re

from fastapi import APIRouter, Depends, HTTPException, status
from pydantic import BaseModel, Field
from sqlalchemy import func, text
from sqlalchemy.orm import Session

from memoria.core.git_for_data import GitForData
from memoria.core.memory.models.memory import MemoryRecord as M
from matrixone.sqlalchemy_ext.snapshot import select as mo_select, compile_select
from memoria.api.dependencies import AuthContext, get_auth_context
from memoria.api.models import SnapshotRegistry
from memoria.config import get_settings

router = APIRouter(tags=["snapshots"])


def _exec_snap(db: Session, stmt):
    """Execute a snapshot-aware select built with mo_select().with_snapshot()."""
    return db.execute(text(compile_select(stmt)))


def _sanitize(name: str) -> str:
    safe = re.sub(r"[^a-zA-Z0-9_]", "_", name)
    if not safe:
        raise HTTPException(status_code=400, detail="Invalid snapshot name")
    return safe


def _snap_name(user_id: str, name: str) -> str:
    sn = f"mem_snap_{_sanitize(user_id)[:16]}_{_sanitize(name)}"
    # Defense-in-depth: final name must be pure identifier (used in SQL literals)
    if not re.fullmatch(r"[a-zA-Z0-9_]+", sn):
        raise HTTPException(status_code=400, detail="Invalid snapshot name")
    return sn


def _git(db_factory) -> GitForData:
    return GitForData(db_factory)


class CreateSnapshotRequest(BaseModel):
    name: str = Field(..., min_length=1, max_length=100)
    description: str = ""


class SnapshotResponse(BaseModel):
    name: str
    snapshot_name: str
    description: str | None = None
    timestamp: str


@router.post(
    "/snapshots", response_model=SnapshotResponse, status_code=status.HTTP_201_CREATED
)
def create_snapshot(
    req: CreateSnapshotRequest,
    auth: AuthContext = Depends(get_auth_context),
):
    settings = get_settings()
    user_id = auth.user_id
    db = auth.db_factory()

    # Check limit via registry table
    count = db.query(SnapshotRegistry).filter_by(user_id=user_id).count()
    if count >= settings.snapshot_limit:
        raise HTTPException(
            status_code=status.HTTP_409_CONFLICT,
            detail=f"Snapshot limit reached ({settings.snapshot_limit}). Delete old snapshots first.",
        )

    snap_name = _snap_name(user_id, req.name)

    # Check uniqueness
    if (
        db.query(SnapshotRegistry.snapshot_name)
        .filter_by(snapshot_name=snap_name)
        .first()
    ):
        raise HTTPException(
            status_code=status.HTTP_409_CONFLICT,
            detail=f"Snapshot '{req.name}' already exists",
        )

    # Create MatrixOne native snapshot
    info = _git(lambda: db).create_snapshot(snap_name)

    # Register
    reg = SnapshotRegistry(
        snapshot_name=snap_name,
        user_id=user_id,
        display_name=req.name,
        description=req.description or None,
    )
    db.add(reg)
    db.commit()

    db.close()
    return SnapshotResponse(
        name=req.name,
        snapshot_name=snap_name,
        description=req.description or None,
        timestamp=str(info.get("timestamp", "")),
    )


@router.get("/snapshots", response_model=list[SnapshotResponse])
def list_snapshots(
    auth: AuthContext = Depends(get_auth_context),
):
    user_id = auth.user_id
    db = auth.db_factory()
    try:
        rows = (
            db.query(
                SnapshotRegistry.display_name,
                SnapshotRegistry.snapshot_name,
                SnapshotRegistry.description,
                SnapshotRegistry.created_at,
            )
            .filter_by(user_id=user_id)
            .order_by(SnapshotRegistry.created_at.desc())
            .limit(200)
            .all()
        )
        return [
            SnapshotResponse(
                name=r.display_name,
                snapshot_name=r.snapshot_name,
                description=r.description,
                timestamp=r.created_at.isoformat() if r.created_at else "",
            )
            for r in rows
        ]
    finally:
        db.close()


@router.get("/snapshots/{name}")
def get_snapshot(
    name: str,
    limit: int = 50,
    offset: int = 0,
    detail: str = "brief",
    auth: AuthContext = Depends(get_auth_context),
):
    """Read snapshot — query memories at snapshot point via time-travel.

    detail: brief (type + truncated content), normal (full content), full (+ confidence).
    """
    user_id = auth.user_id
    db = auth.db_factory()
    try:
        snap_name = _snap_name(user_id, name)
        reg = (
            db.query(SnapshotRegistry.display_name, SnapshotRegistry.description)
            .filter_by(snapshot_name=snap_name, user_id=user_id)
            .first()
        )
        if reg is None:
            raise HTTPException(status_code=404, detail="Snapshot not found")

        display_name = reg.display_name
        description = reg.description

        git = _git(lambda: db)
        all_snaps = git.list_snapshots()
        snap_info = next(
            (s for s in all_snaps if s["snapshot_name"] == snap_name), None
        )
        if snap_info is None:
            raise HTTPException(
                status_code=404, detail="Snapshot not found in database"
            )

        ts = snap_info["timestamp"]

        if limit > 500:
            limit = 500

        _active = M.is_active > 0
        _user = M.user_id == user_id

        # Total count
        total = (
            _exec_snap(
                db,
                mo_select(func.count(M.memory_id))
                .where(_user, _active)
                .with_snapshot(snap_name),
            ).scalar()
            or 0
        )

        # Type distribution
        type_dist = _exec_snap(
            db,
            mo_select(M.memory_type, func.count())
            .where(_user, _active)
            .group_by(M.memory_type)
            .with_snapshot(snap_name),
        ).fetchall()

        # Paginated memories
        if detail == "full":
            cols = (M.memory_id, M.content, M.memory_type, M.initial_confidence)
        else:
            cols = (M.memory_id, M.content, M.memory_type)
        rows = _exec_snap(
            db,
            mo_select(*cols)
            .where(_user, _active)
            .order_by(M.observed_at.desc())
            .limit(limit)
            .offset(offset)
            .with_snapshot(snap_name),
        ).fetchall()

        content_limit = (
            80 if detail == "brief" else (200 if detail == "normal" else 2000)
        )
        memories = []
        for r in rows:
            m: dict = {"memory_id": r[0], "memory_type": r[2]}
            content = r[1] or ""
            m["content"] = (
                (content[:content_limit] + " [truncated]")
                if len(content) > content_limit
                else content
            )
            if detail == "full":
                m["confidence"] = r[3]
            memories.append(m)

        return {
            "name": display_name,
            "snapshot_name": snap_name,
            "description": description,
            "timestamp": str(ts),
            "memory_count": total,
            "by_type": {str(r[0]): r[1] for r in type_dist},
            "memories": memories,
            "limit": limit,
            "offset": offset,
            "has_more": offset + limit < total,
        }
    finally:
        db.close()


@router.delete("/snapshots/{name}", status_code=status.HTTP_204_NO_CONTENT)
def delete_snapshot(
    name: str,
    auth: AuthContext = Depends(get_auth_context),
):
    user_id = auth.user_id
    db = auth.db_factory()
    try:
        snap_name = _snap_name(user_id, name)
        from sqlalchemy.orm import load_only

        reg = (
            db.query(SnapshotRegistry)
            .options(load_only(SnapshotRegistry.snapshot_name))
            .filter_by(snapshot_name=snap_name, user_id=user_id)
            .first()
        )
        if reg is None:
            raise HTTPException(status_code=404, detail="Snapshot not found")

        # Drop MatrixOne native snapshot (DDL-like, needs clean transaction state)
        db.commit()
        db.execute(text(f"DROP SNAPSHOT {snap_name}"))
        # Remove registry entry
        db.delete(reg)
        db.commit()
    finally:
        db.close()


@router.get("/snapshots/{name}/diff")
def diff_snapshot(
    name: str,
    limit: int = 50,
    auth: AuthContext = Depends(get_auth_context),
):
    """Compare snapshot memories vs current state. Diff computed in DB, not Python."""
    user_id = auth.user_id
    db = auth.db_factory()
    try:
        snap_name = _snap_name(user_id, name)
        if (
            not db.query(SnapshotRegistry.snapshot_name)
            .filter_by(snapshot_name=snap_name, user_id=user_id)
            .first()
        ):
            raise HTTPException(status_code=404, detail="Snapshot not found")

        if limit > 200:
            limit = 200

        _active = M.is_active > 0
        _user = M.user_id == user_id

        # Counts — single-table, use SDK
        snap_count = (
            _exec_snap(
                db,
                mo_select(func.count(M.memory_id))
                .where(_user, _active)
                .with_snapshot(snap_name),
            ).scalar()
            or 0
        )

        curr_count = (
            db.query(func.count(M.memory_id)).filter(_user, _active).scalar() or 0
        )

        # LEFT JOIN diff — MatrixOne-specific cross-snapshot join, must stay raw SQL
        added_rows = db.execute(
            text(
                "SELECT c.memory_id, c.content, c.memory_type FROM mem_memories c"
                f" LEFT JOIN mem_memories {{SNAPSHOT = '{snap_name}'}} s"
                " ON c.memory_id = s.memory_id AND s.is_active"
                " WHERE c.user_id = :uid AND c.is_active AND s.memory_id IS NULL"
                " LIMIT :lim"
            ),
            {"uid": user_id, "lim": limit},
        ).fetchall()

        removed_rows = db.execute(
            text(
                f"SELECT s.memory_id, s.content, s.memory_type FROM mem_memories {{SNAPSHOT = '{snap_name}'}} s"
                " LEFT JOIN mem_memories c"
                " ON s.memory_id = c.memory_id AND c.is_active"
                " WHERE s.user_id = :uid AND s.is_active AND c.memory_id IS NULL"
                " LIMIT :lim"
            ),
            {"uid": user_id, "lim": limit},
        ).fetchall()

        added_count = (
            db.execute(
                text(
                    "SELECT COUNT(*) FROM mem_memories c"
                    f" LEFT JOIN mem_memories {{SNAPSHOT = '{snap_name}'}} s"
                    " ON c.memory_id = s.memory_id AND s.is_active"
                    " WHERE c.user_id = :uid AND c.is_active AND s.memory_id IS NULL"
                ),
                {"uid": user_id},
            ).scalar()
            or 0
        )

        removed_count = (
            db.execute(
                text(
                    f"SELECT COUNT(*) FROM mem_memories {{SNAPSHOT = '{snap_name}'}} s"
                    " LEFT JOIN mem_memories c"
                    " ON s.memory_id = c.memory_id AND c.is_active"
                    " WHERE s.user_id = :uid AND s.is_active AND c.memory_id IS NULL"
                ),
                {"uid": user_id},
            ).scalar()
            or 0
        )

        added = [
            {"memory_id": r[0], "content": (r[1] or "")[:200], "memory_type": r[2]}
            for r in added_rows
        ]
        removed = [
            {"memory_id": r[0], "content": (r[1] or "")[:200], "memory_type": r[2]}
            for r in removed_rows
        ]

        return {
            "snapshot_name": name,
            "snapshot_count": snap_count,
            "current_count": curr_count,
            "added": added,
            "removed": removed,
            "added_count": added_count,
            "removed_count": removed_count,
            "unchanged_count": snap_count - removed_count,
        }
    finally:
        db.close()
