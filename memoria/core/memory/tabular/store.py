"""MemoryStore — CRUD + atomic contradiction resolution for memories."""

from __future__ import annotations

import logging
import uuid

from sqlalchemy import text

from memoria.core.memory.models.memory import MemoryRecord
from memoria.core.db_consumer import DbConsumer, DbFactory
from memoria.core.memory.tabular.metrics import MemoryMetrics, Timer
from memoria.core.memory.types import Memory, MemoryType, TrustTier, _utcnow

logger = logging.getLogger(__name__)


def _to_domain(row: MemoryRecord) -> Memory:
    return Memory(
        memory_id=row.memory_id,
        user_id=row.user_id,
        memory_type=MemoryType(row.memory_type),
        content=row.content,
        initial_confidence=row.initial_confidence,
        embedding=row.embedding,
        source_event_ids=row.source_event_ids or [],
        superseded_by=row.superseded_by,
        is_active=bool(row.is_active),
        access_count=0,  # Will be set by caller if needed
        session_id=row.session_id,
        observed_at=row.observed_at,
        created_at=row.created_at,
        updated_at=row.updated_at,
        trust_tier=TrustTier(row.trust_tier)
        if row.trust_tier
        else TrustTier.T3_INFERRED,
    )


def _to_domain_light(row) -> Memory:
    """Convert a column-tuple row (without embedding) to Memory."""
    return Memory(
        memory_id=row.memory_id,
        user_id=row.user_id,
        memory_type=MemoryType(row.memory_type),
        content=row.content,
        initial_confidence=row.initial_confidence,
        embedding=None,
        source_event_ids=row.source_event_ids or [],
        superseded_by=row.superseded_by,
        is_active=bool(row.is_active),
        access_count=getattr(row, "access_count", 0) or 0,
        session_id=row.session_id,
        observed_at=row.observed_at,
        created_at=row.created_at,
        updated_at=row.updated_at,
        trust_tier=TrustTier(row.trust_tier)
        if row.trust_tier
        else TrustTier.T3_INFERRED,
    )


class MemoryStore(DbConsumer):
    """CRUD operations on the memories table."""

    def __init__(self, db_factory: DbFactory, metrics: MemoryMetrics | None = None):
        super().__init__(db_factory)
        self._metrics = metrics or MemoryMetrics()

    def create(self, memory: Memory) -> Memory:
        if not memory.memory_id:
            memory.memory_id = uuid.uuid4().hex
        now = _utcnow()
        if not memory.observed_at:
            memory.observed_at = now

        with Timer("store_create", self._metrics), self._db() as db:
            row = MemoryRecord(
                memory_id=memory.memory_id,
                user_id=memory.user_id,
                session_id=memory.session_id,
                memory_type=memory.memory_type.value,
                content=memory.content,
                initial_confidence=memory.initial_confidence,
                trust_tier=memory.trust_tier.value,
                embedding=memory.embedding,
                source_event_ids=memory.source_event_ids,
                extra_metadata=memory.extra_metadata,
                is_active=1,
                observed_at=memory.observed_at,
            )
            db.add(row)
            db.commit()
            memory.created_at = row.created_at
        self._metrics.increment("memories_created")
        return memory

    def batch_create(self, memories: list[Memory]) -> list[Memory]:
        """Insert multiple memories in a single transaction."""
        if not memories:
            return []
        now = _utcnow()
        for mem in memories:
            if not mem.memory_id:
                mem.memory_id = uuid.uuid4().hex
            if not mem.observed_at:
                mem.observed_at = now

        with Timer("store_batch_create", self._metrics), self._db() as db:
            rows = [
                MemoryRecord(
                    memory_id=mem.memory_id,
                    user_id=mem.user_id,
                    session_id=mem.session_id,
                    memory_type=mem.memory_type.value,
                    content=mem.content,
                    initial_confidence=mem.initial_confidence,
                    trust_tier=mem.trust_tier.value,
                    embedding=mem.embedding,
                    source_event_ids=mem.source_event_ids,
                    extra_metadata=mem.extra_metadata,
                    is_active=1,
                    observed_at=mem.observed_at,
                )
                for mem in memories
            ]
            db.add_all(rows)
            db.commit()
            for mem, row in zip(memories, rows, strict=True):
                mem.created_at = row.created_at
        self._metrics.increment("memories_created", len(memories))
        return memories

    def get(self, memory_id: str) -> Memory | None:
        with Timer("store_get", self._metrics), self._db() as db:
            row = db.query(MemoryRecord).filter_by(memory_id=memory_id).first()
            return _to_domain(row) if row else None

    def get_by_ids(self, memory_ids: list[str]) -> dict[str, Memory]:
        """Batch fetch memories by ID. Returns {memory_id: Memory} for found rows."""
        if not memory_ids:
            return {}
        with self._db() as db:
            from memoria.core.memory.models.memory_stats import MemoryStats

            rows = (
                db.query(MemoryRecord, MemoryStats.access_count)
                .outerjoin(MemoryStats, MemoryRecord.memory_id == MemoryStats.memory_id)
                .filter(MemoryRecord.memory_id.in_(memory_ids))
                .all()
            )
            result = {}
            for record, access_count in rows:
                mem = _to_domain(record)
                mem.access_count = access_count or 0
                result[record.memory_id] = mem
            return result

    def update_content(self, memory_id: str, content: str) -> None:
        """Update content of an existing memory (e.g. streaming accumulation)."""
        with self._db() as db:
            db.query(MemoryRecord).filter_by(memory_id=memory_id).update(
                {"content": content}
            )
            db.commit()

    def update_embedding(self, memory_id: str, embedding: list[float]) -> int:
        """Update embedding of an existing memory. Returns rows affected (0 if not found)."""
        with self._db() as db:
            rows = (
                db.query(MemoryRecord)
                .filter_by(memory_id=memory_id)
                .update({"embedding": embedding})
            )
            db.commit()
            return rows

    def update_confidence(
        self,
        memory_id: str,
        confidence: float,
        trust_tier: str | None = None,
        is_active: bool | None = None,
    ) -> None:
        """Update confidence (and optionally tier/active) for opinion evolution."""
        with self._db() as db:
            vals: dict = {"initial_confidence": confidence}
            if trust_tier is not None:
                vals["trust_tier"] = trust_tier
            if is_active is not None:
                vals["is_active"] = int(is_active)
            db.query(MemoryRecord).filter_by(memory_id=memory_id).update(vals)
            db.commit()

    def list_active(
        self,
        user_id: str,
        memory_type: MemoryType | None = None,
        limit: int | None = None,
        load_embedding: bool = True,
    ) -> list[Memory]:
        with self._db() as db:
            if load_embedding:
                q = db.query(MemoryRecord)
            else:
                # Skip embedding column (~6KB/row) when not needed
                cols = [
                    c for c in MemoryRecord.__table__.columns if c.name != "embedding"
                ]
                q = db.query(*cols)
            q = q.filter(
                MemoryRecord.user_id == user_id,
                MemoryRecord.is_active > 0,
            )
            if memory_type:
                q = q.filter(MemoryRecord.memory_type == memory_type.value)
            q = q.order_by(MemoryRecord.updated_at.desc())
            if limit is not None:
                q = q.limit(limit)
            if load_embedding:
                return [_to_domain(r) for r in q.all()]
            return [_to_domain_light(r) for r in q.all()]

    def supersede(self, old_id: str, new_memory: Memory) -> Memory:
        if not new_memory.memory_id:
            new_memory.memory_id = uuid.uuid4().hex
        now = _utcnow()
        if not new_memory.observed_at:
            new_memory.observed_at = now

        with self._db() as db:
            db.query(MemoryRecord).filter_by(memory_id=old_id).update(
                {
                    "is_active": 0,
                    "superseded_by": new_memory.memory_id,
                    "updated_at": now,
                }
            )
            # Sync: deactivate graph node whose backing memory was superseded
            db.execute(
                text(
                    "UPDATE memory_graph_nodes SET is_active = 0 WHERE memory_id = :mid"
                ),
                {"mid": old_id},
            )

            row = MemoryRecord(
                memory_id=new_memory.memory_id,
                user_id=new_memory.user_id,
                session_id=new_memory.session_id,
                memory_type=new_memory.memory_type.value,
                content=new_memory.content,
                initial_confidence=new_memory.initial_confidence,
                trust_tier=new_memory.trust_tier.value,
                embedding=new_memory.embedding,
                source_event_ids=new_memory.source_event_ids,
                is_active=1,
                observed_at=new_memory.observed_at,
            )
            db.add(row)
            db.commit()
            new_memory.created_at = row.created_at
        return new_memory

    def archive_working_memories(self, session_id: str) -> int:
        """Archive all WORKING memories for a session (set is_active=0)."""
        with self._db() as db:
            from sqlalchemy import text as sa_text

            result = db.execute(
                sa_text("""
                UPDATE mem_memories SET is_active = 0, updated_at = NOW()
                WHERE session_id = :sid AND memory_type = 'working' AND is_active = 1
            """),
                {"sid": session_id},
            )
            db.commit()
            return result.rowcount

    def deactivate(self, memory_id: str) -> bool:
        with self._db() as db:
            row = db.query(MemoryRecord).filter_by(memory_id=memory_id).first()
            if not row:
                return False
            row.is_active = 0
            db.commit()
            return True
