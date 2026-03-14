"""MemoryEditor — inject, correct, purge, relearn operations.

Administrative memory operations with audit trail and snapshot safety.
Operates on CanonicalStorage; notifies IndexManager after mutations.

See docs/design/memory/backend-management.md §6
"""

from __future__ import annotations

import logging
import uuid
from dataclasses import dataclass, field
from datetime import datetime, timezone
from typing import TYPE_CHECKING, Any

from sqlalchemy import text

if TYPE_CHECKING:
    from memoria.core.db_consumer import DbFactory
    from memoria.core.memory.canonical_storage import CanonicalStorage
    from memoria.core.memory.strategy.protocol import IndexManager
    from memoria.core.memory.types import Memory, MemoryType, TrustTier

logger = logging.getLogger(__name__)


@dataclass
class PurgeResult:
    """Result of a purge operation."""

    deactivated: int = 0
    snapshot_name: str | None = None


@dataclass
class EditLogEntry:
    """Audit record for a memory edit operation."""

    edit_id: str
    user_id: str
    operation: str  # inject | correct | purge
    target_ids: list[str] = field(default_factory=list)
    reason: str = ""
    snapshot_before: str | None = None


class MemoryEditor:
    """Inject, correct, and purge memories with audit trail.

    All destructive operations create a snapshot first.
    All mutations are logged to mem_edit_log.
    """

    def __init__(
        self,
        storage: CanonicalStorage,
        db_factory: DbFactory,
        index_manager: IndexManager | None = None,
        embed_client: Any | None = None,
    ) -> None:
        self._storage = storage
        self._db_factory = db_factory
        self._index_manager = index_manager
        self._embed_client = embed_client

    def inject(
        self,
        user_id: str,
        content: str,
        *,
        memory_type: MemoryType,
        trust_tier: TrustTier | None = None,
        source: str = "admin_inject",
        session_id: str | None = None,
        observed_at: datetime | None = None,
        initial_confidence: float = 1.0,
    ) -> Memory:
        """Inject a memory with high trust tier.

        Args:
            user_id: Target user.
            content: Memory content.
            memory_type: Type of memory.
            trust_tier: Trust tier (default: T1_VERIFIED).
            source: Source identifier for audit.
            session_id: Optional session context.
            observed_at: Override timestamp (e.g. for decay benchmark tests).
            initial_confidence: Override initial confidence (default: 1.0).

        Returns:
            The created Memory.
        """
        from memoria.core.memory.types import TrustTier

        if trust_tier is None:
            trust_tier = TrustTier.T1_VERIFIED

        mem = self._storage.store(
            user_id,
            content,
            memory_type=memory_type,
            trust_tier=trust_tier,
            initial_confidence=initial_confidence,
            session_id=session_id,
            source_event_ids=[f"inject:{source}"],
        )
        if observed_at is not None:
            mem.observed_at = observed_at
            # Backdate the stored record so decay scoring uses the correct timestamp
            with self._db_factory() as db:
                from memoria.core.memory.models.memory import MemoryRecord as _MR

                db.query(_MR).filter(_MR.memory_id == mem.memory_id).update(
                    {"observed_at": observed_at}
                )
                db.commit()

        if self._index_manager:
            self._index_manager.on_memories_stored(
                user_id, [mem], session_id=session_id
            )

        self._log_edit(user_id, "inject", target_ids=[mem.memory_id], reason=source)
        return mem

    def batch_inject(
        self,
        user_id: str,
        specs: list[dict[str, Any]],
        *,
        source: str = "batch_inject",
        session_id: str | None = None,
    ) -> list[Memory]:
        """Batch-inject memories: 1 embedding call, 1 DB transaction, 1 index update.

        Args:
            user_id: Target user.
            specs: List of dicts with 'content', optional 'memory_type' and 'trust'.
            source: Source identifier for audit.
            session_id: Optional session context.

        Returns:
            List of created Memory objects.
        """
        import uuid as _uuid

        from memoria.core.memory.types import Memory as MemoryObj
        from memoria.core.memory.types import MemoryType, TrustTier, _utcnow

        if not specs:
            return []

        now = _utcnow()
        memories: list[MemoryObj] = []
        for spec in specs:
            mem = MemoryObj(
                memory_id=_uuid.uuid4().hex,
                user_id=user_id,
                memory_type=MemoryType(spec.get("memory_type", "semantic")),
                content=spec["content"],
                initial_confidence=1.0,
                trust_tier=TrustTier(spec.get("trust", "T2")),
                source_event_ids=[f"inject:{source}"],
                session_id=session_id,
                observed_at=now,
            )
            memories.append(mem)

        # Batch embed — 1 API call instead of N
        if self._embed_client is not None:
            try:
                texts = [m.content for m in memories]
                embeddings = self._embed_client.embed_batch(texts)
                for mem, emb in zip(memories, embeddings, strict=True):
                    mem.embedding = emb
            except Exception:
                logger.warning(
                    "Batch embedding failed, continuing without embeddings",
                    exc_info=True,
                )

        # Batch store — 1 transaction
        stored = self._storage.batch_store(memories)

        # Batch index update — 1 call
        if self._index_manager and stored:
            self._index_manager.on_memories_stored(
                user_id, stored, session_id=session_id
            )

        # Single audit entry
        self._log_edit(
            user_id,
            "inject",
            target_ids=[m.memory_id for m in stored],
            reason=source,
        )
        return stored

    def find_best_match(self, user_id: str, query: str) -> Memory | None:
        """Find the single best-matching active memory for a query via semantic search.

        Returns None if no match found.
        """
        from memoria.core.memory.tabular.retriever import MemoryRetriever

        retriever = MemoryRetriever(self._db_factory)
        query_embedding: list[float] | None = None
        if self._embed_client is not None:
            try:
                query_embedding = self._embed_client.embed(query)
            except Exception:
                logger.warning(
                    "Embedding failed for find_best_match query", exc_info=True
                )

        memories, _ = retriever.retrieve(
            user_id,
            query,
            session_id="",
            query_embedding=query_embedding,
            limit=1,
        )
        return memories[0] if memories else None

    def correct(
        self,
        user_id: str,
        memory_id: str,
        new_content: str,
        *,
        reason: str = "",
    ) -> Memory:
        """Supersede a memory with corrected content.

        The old memory is deactivated and linked via superseded_by.

        Args:
            user_id: Memory owner.
            memory_id: ID of memory to correct.
            new_content: Corrected content.
            reason: Why the correction was made.

        Returns:
            The new corrected Memory.

        Raises:
            ValueError: If memory_id not found.
        """
        from memoria.core.memory.types import Memory as MemoryObj
        from memoria.core.memory.types import TrustTier

        old = self._storage.get_memory(memory_id)
        if old is None:
            raise ValueError(f"Memory {memory_id} not found")

        new_mem = MemoryObj(
            memory_id=uuid.uuid4().hex,
            user_id=user_id,
            content=new_content,
            memory_type=old.memory_type,
            trust_tier=TrustTier.T2_CURATED,
            initial_confidence=old.initial_confidence,
            session_id=old.session_id,
            source_event_ids=[f"correct:{memory_id}"],
            observed_at=datetime.now(timezone.utc),
        )

        if self._embed_client is not None:
            try:
                new_mem.embedding = self._embed_client.embed(new_content)
            except Exception:
                logger.warning("Embedding failed for correct", exc_info=True)

        # Use store's supersede: deactivates old, creates new, links them.
        # supersede() inserts directly into MemoryStore (not through
        # CanonicalStorage.create_memory), so we must embed here.
        from memoria.core.memory.tabular.store import MemoryStore

        store = MemoryStore(self._db_factory)
        result = store.supersede(memory_id, new_mem)

        if self._index_manager:
            self._index_manager.on_memories_stored(
                user_id, [result], session_id=old.session_id
            )

        self._log_edit(
            user_id,
            "correct",
            target_ids=[memory_id, result.memory_id],
            reason=reason,
        )
        return result

    def purge(
        self,
        user_id: str,
        *,
        memory_ids: list[str] | None = None,
        memory_types: list[MemoryType] | None = None,
        before: datetime | None = None,
        reason: str = "",
    ) -> PurgeResult:
        """Deactivate memories matching criteria.

        Creates a snapshot before purging for rollback safety.

        Args:
            user_id: Memory owner.
            memory_ids: Specific IDs to purge.
            memory_types: Purge all of these types.
            before: Purge memories observed before this time.
            reason: Why the purge was done.

        Returns:
            PurgeResult with count and snapshot name.
        """
        # Snapshot before destructive op
        snapshot_name = self._create_safety_snapshot(user_id, "purge")

        deactivated = 0
        purged_ids: list[str] = []

        with self._db_factory() as db:
            if memory_ids:
                for mid in memory_ids:
                    result = db.execute(
                        text(
                            "UPDATE mem_memories SET is_active = 0, updated_at = NOW() "
                            "WHERE memory_id = :mid AND user_id = :uid AND is_active = 1"
                        ),
                        {"mid": mid, "uid": user_id},
                    )
                    if result.rowcount > 0:
                        deactivated += result.rowcount
                        purged_ids.append(mid)
                # Sync: deactivate graph nodes whose backing memory was purged
                if purged_ids:
                    db.execute(
                        text(
                            "UPDATE memory_graph_nodes SET is_active = 0 "
                            "WHERE memory_id IN :mids AND user_id = :uid"
                        ),
                        {"mids": tuple(purged_ids), "uid": user_id},
                    )

            if memory_types:
                type_vals = [mt.value for mt in memory_types]
                for tv in type_vals:
                    q = (
                        "UPDATE mem_memories SET is_active = 0, updated_at = NOW() "
                        "WHERE user_id = :uid AND memory_type = :mt AND is_active = 1"
                    )
                    params: dict[str, Any] = {"uid": user_id, "mt": tv}
                    if before:
                        q += " AND observed_at < :before"
                        params["before"] = before
                    result = db.execute(text(q), params)
                    deactivated += result.rowcount

            if before and not memory_ids and not memory_types:
                result = db.execute(
                    text(
                        "UPDATE mem_memories SET is_active = 0, updated_at = NOW() "
                        "WHERE user_id = :uid AND is_active = 1 AND observed_at < :before"
                    ),
                    {"uid": user_id, "before": before},
                )
                deactivated += result.rowcount

            # Sync graph nodes: deactivate any graph node whose backing memory is inactive
            if deactivated > 0:
                db.execute(
                    text(
                        "UPDATE memory_graph_nodes g "
                        "JOIN mem_memories m ON g.memory_id = m.memory_id "
                        "SET g.is_active = 0 "
                        "WHERE g.user_id = :uid AND g.is_active = 1 AND m.is_active = 0"
                    ),
                    {"uid": user_id},
                )

            db.commit()

        if self._index_manager and deactivated > 0:
            self._index_manager.on_governance(user_id)

        self._log_edit(
            user_id,
            "purge",
            target_ids=purged_ids,
            reason=reason,
            snapshot_before=snapshot_name,
        )
        return PurgeResult(deactivated=deactivated, snapshot_name=snapshot_name)

    # ── Internal helpers ──────────────────────────────────────────────

    def _create_safety_snapshot(self, user_id: str, operation: str) -> str | None:
        """Create a snapshot before destructive operations. Best-effort."""
        from memoria.core.utils.id_generator import generate_id

        name = f"pre_{operation}_{generate_id()}"
        try:
            from memoria.core.git_for_data import GitForData

            git = GitForData(self._db_factory)
            git.create_snapshot(name)
            return name
        except Exception:
            logger.warning("Failed to create safety snapshot %s", name, exc_info=True)
            return None

    def _log_edit(
        self,
        user_id: str,
        operation: str,
        *,
        target_ids: list[str] | None = None,
        reason: str = "",
        snapshot_before: str | None = None,
    ) -> None:
        """Log edit to mem_edit_log. Best-effort — never fails the operation."""
        try:
            import json

            with self._db_factory() as db:
                db.execute(
                    text(
                        "INSERT INTO mem_edit_log "
                        "(edit_id, user_id, operation, target_ids, reason, "
                        " snapshot_before, created_by) "
                        "VALUES (:eid, :uid, :op, :tids, :reason, :snap, :uid)"
                    ),
                    {
                        "eid": uuid.uuid4().hex,
                        "uid": user_id,
                        "op": operation,
                        "tids": json.dumps(target_ids or []),
                        "reason": reason,
                        "snap": snapshot_before,
                    },
                )
                db.commit()
        except Exception:
            logger.debug(
                "Failed to log edit for %s/%s", user_id, operation, exc_info=True
            )
