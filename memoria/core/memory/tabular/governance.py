"""Memory Governance — frequency-separated cleanup, quarantine, health.

Confidence decay removed — decay is now query-time only via effective_confidence().
Reflector removed — no episodic→semantic promotion.

Governance cycles:
  - hourly: tool_result cleanup, working memory archival
  - daily: stale inactive cleanup, quarantine low effective_confidence
  - weekly: orphan branch cleanup, snapshot cleanup, health report
"""

from __future__ import annotations

import json
import logging
import time
from dataclasses import dataclass, field
from datetime import timedelta
from typing import Any, Optional

from sqlalchemy import text

try:
    from matrixone.vector_manager import VectorManager
except ImportError:
    VectorManager = None  # type: ignore[assignment,misc]

from memoria.core.db_consumer import DbConsumer, DbFactory
from memoria.core.memory.config import DEFAULT_CONFIG, MemoryGovernanceConfig
from memoria.core.memory.tabular.health import MemoryHealth
from memoria.core.memory.tabular.metrics import MemoryMetrics
from memoria.core.memory.tabular.store import MemoryStore
from memoria.core.memory.types import TrustTier, _utcnow

logger = logging.getLogger(__name__)


@dataclass
class GovernanceCycleResult:
    # Hourly
    cleaned_tool_results: int = 0
    archived_working: int = 0
    # Daily
    cleaned_stale: int = 0
    quarantined: int = 0
    scenes_created: int = 0
    # Weekly
    cleaned_branches: int = 0
    cleaned_snapshots: int = 0
    # Health
    pollution_detected: bool = False
    vector_index_health: dict = field(
        default_factory=dict
    )  # table → {centroids, imbalance, needs_rebuild}
    errors: list[str] = field(default_factory=list)
    total_ms: float = 0.0
    compressed_redundant: int = 0
    # Observability
    input_memories: int = 0  # total active memories considered
    users_processed: int = 0
    users_skipped_no_changes: int = 0
    reflection_candidates_found: int = 0
    reflection_candidates_synthesized: int = 0
    reflection_candidates_skipped_low_importance: int = 0


class GovernanceScheduler(DbConsumer):
    """Periodic memory governance tasks.

    No longer mutates confidence — decay is query-time only.
    No longer runs Reflector — episodic type eliminated.
    """

    def __init__(
        self,
        db_factory: DbFactory,
        config: Optional[MemoryGovernanceConfig] = None,
        metrics: Optional[MemoryMetrics] = None,
        llm_client: Any = None,
        embed_fn: Any = None,
    ):
        super().__init__(db_factory)
        self.config = config or DEFAULT_CONFIG
        self._metrics = metrics or MemoryMetrics()
        self._llm_client = llm_client
        self._embed_fn = embed_fn
        self._store = MemoryStore(db_factory, metrics=self._metrics)
        self.health = MemoryHealth(
            db_factory,
            pollution_threshold=self.config.pollution_threshold,
        )

    # ── Audit helper ──────────────────────────────────────────────────

    _AUDIT_MAX_IDS = 500

    def _log_governance_edit(
        self,
        user_id: str,
        operation: str,
        target_ids: list[str],
        reason: str,
    ) -> None:
        """Log governance mutation to mem_edit_log. Best-effort."""
        try:
            import uuid

            ids = target_ids[: self._AUDIT_MAX_IDS]
            if len(target_ids) > self._AUDIT_MAX_IDS:
                reason = (
                    f"{reason} (showing {self._AUDIT_MAX_IDS} of {len(target_ids)})"
                )
            with self._db() as db:
                db.execute(
                    text(
                        "INSERT INTO mem_edit_log "
                        "(edit_id, user_id, operation, target_ids, reason, created_by) "
                        "VALUES (:eid, :uid, :op, :tids, :reason, :uid)"
                    ),
                    {
                        "eid": uuid.uuid4().hex,
                        "uid": user_id,
                        "op": operation,
                        "tids": json.dumps(ids),
                        "reason": reason,
                    },
                )
                db.commit()
        except Exception:
            logger.debug(
                "Failed to log governance edit for %s/%s",
                user_id,
                operation,
                exc_info=True,
            )

    # ── Convenience: run all ──────────────────────────────────────────

    def run_cycle(self, user_id: str) -> GovernanceCycleResult:
        """Run all governance frequencies. Convenience for single-instance deployments."""
        result = GovernanceCycleResult()
        start = time.time()

        h = self.run_hourly()
        result.cleaned_tool_results = h.cleaned_tool_results
        result.archived_working = h.archived_working
        result.errors.extend(h.errors)

        d = self.run_daily(user_id)
        result.cleaned_stale = d.cleaned_stale
        result.quarantined = d.quarantined
        result.scenes_created = d.scenes_created
        result.pollution_detected = d.pollution_detected
        result.errors.extend(d.errors)

        w = self.run_weekly()
        result.cleaned_branches = w.cleaned_branches
        result.cleaned_snapshots = w.cleaned_snapshots
        result.errors.extend(w.errors)

        result.total_ms = (time.time() - start) * 1000

        # Vector index health check + auto-rebuild if needed
        result.vector_index_health = self._check_vector_index_health()
        for table, h in result.vector_index_health.items():
            if h.get("needs_rebuild"):
                try:
                    self.rebuild_vector_index(table)
                    result.vector_index_health[table]["rebuilt"] = True
                    logger.info("Auto-rebuilt IVF index for %s", table)
                except Exception as e:
                    result.vector_index_health[table]["rebuild_error"] = str(e)
                    result.errors.append(f"rebuild_{table}: {e}")

        return result

    # ── Hourly ────────────────────────────────────────────────────────

    def run_hourly(self) -> GovernanceCycleResult:
        """Hourly: tool_result cleanup + working memory archival."""
        result = GovernanceCycleResult()
        try:
            result.cleaned_tool_results = self._cleanup_tool_results()
        except Exception as e:
            logger.error("Tool result cleanup failed: %s", e)
            result.errors.append(f"tool_results: {e}")
        try:
            result.archived_working = self._archive_stale_working()
        except Exception as e:
            logger.error("Working memory archival failed: %s", e)
            result.errors.append(f"working_archival: {e}")
        return result

    # ── Daily ─────────────────────────────────────────────────────────

    def _build_reflection_engine(self) -> Any:
        """Build a ReflectionEngine if LLM client is available, else None."""
        if self._llm_client is None:
            return None
        from memoria.core.memory.reflection.engine import ReflectionEngine
        from memoria.core.memory.tabular.candidates import TabularCandidateProvider

        provider = TabularCandidateProvider(self._db_factory, config=self.config)
        return ReflectionEngine(
            candidate_provider=provider,
            writer=self,
            llm_client=self._llm_client,
            threshold=self.config.reflection_daily_threshold,
            llm_threshold=self.config.reflection_llm_threshold,
        )

    def run_daily_all(self) -> GovernanceCycleResult:
        """Daily governance for ALL users (or this worker's shard).

        Sharding: when config.shard_count > 1, each worker processes only
        users where CRC32(user_id) % shard_count == shard_index.
        This allows N workers to split the user space with no coordination.

        Creates reflection provider/engine once and reuses across users.
        Skips users with no memory changes since last governance run (incremental).
        """
        combined = GovernanceCycleResult()

        # Pre-build reflection engine once (if LLM available)
        reflection_engine = self._build_reflection_engine()

        batch_size = self.config.daily_batch_size
        shard_count = self.config.shard_count
        shard_index = self.config.shard_index
        last_uid = ""

        with self._db() as db:
            while True:
                if shard_count > 1:
                    rows = db.execute(
                        text(
                            "SELECT DISTINCT user_id FROM mem_memories "
                            "WHERE is_active = 1 AND user_id > :last "
                            "AND CRC32(user_id) % :shards = :shard "
                            "ORDER BY user_id LIMIT :limit"
                        ),
                        {
                            "last": last_uid,
                            "limit": batch_size,
                            "shards": shard_count,
                            "shard": shard_index,
                        },
                    ).fetchall()
                else:
                    rows = db.execute(
                        text(
                            "SELECT DISTINCT user_id FROM mem_memories "
                            "WHERE is_active = 1 AND user_id > :last "
                            "ORDER BY user_id LIMIT :limit"
                        ),
                        {"last": last_uid, "limit": batch_size},
                    ).fetchall()
                if not rows:
                    break
                for (uid,) in rows:
                    if not self._has_changes_since_last_governance(db, uid):
                        combined.users_skipped_no_changes += 1
                        continue
                    # Count active memories using the already-open session
                    try:
                        cnt = db.execute(
                            text(
                                "SELECT COUNT(*) FROM mem_memories "
                                "WHERE user_id = :uid AND is_active = 1"
                            ),
                            {"uid": uid},
                        ).scalar()
                        combined.input_memories += cnt or 0
                    except Exception:
                        pass
                    r = self._run_daily_for_user(uid, reflection_engine)
                    combined.cleaned_stale += r.cleaned_stale
                    combined.quarantined += r.quarantined
                    combined.scenes_created += r.scenes_created
                    combined.compressed_redundant += r.compressed_redundant
                    combined.input_memories += r.input_memories
                    combined.reflection_candidates_found += (
                        r.reflection_candidates_found
                    )
                    combined.reflection_candidates_synthesized += (
                        r.reflection_candidates_synthesized
                    )
                    combined.reflection_candidates_skipped_low_importance += (
                        r.reflection_candidates_skipped_low_importance
                    )
                    combined.errors.extend(r.errors)
                    combined.users_processed += 1
                    if not r.errors:
                        self._mark_daily_user(db, uid)
                last_uid = rows[-1][0]
                if len(rows) < batch_size:
                    break
        return combined

    def run_daily(self, user_id: str) -> GovernanceCycleResult:
        """Daily: stale cleanup + quarantine low effective_confidence + orphaned incremental summaries."""
        # Build per-call reflection engine when called standalone
        reflection_engine = self._build_reflection_engine()
        result = self._run_daily_for_user(user_id, reflection_engine)
        if not result.errors:
            with self._db() as db:
                self._mark_daily_user(db, user_id)
        return result

    def _run_daily_for_user(
        self,
        user_id: str,
        reflection_engine: Any = None,
    ) -> GovernanceCycleResult:
        """Daily governance for a single user. Accepts pre-built reflection engine."""
        result = GovernanceCycleResult()
        try:
            result.cleaned_stale = self._cleanup_stale(user_id)
        except Exception as e:
            logger.error("Stale cleanup failed: %s", e)
            result.errors.append(f"stale: {e}")
        try:
            result.quarantined = self._quarantine_low_confidence(user_id)
        except Exception as e:
            logger.error("Quarantine failed: %s", e)
            result.errors.append(f"quarantine: {e}")
        try:
            pollution = self.health.detect_pollution(
                user_id, _utcnow() - timedelta(days=1)
            )
            result.pollution_detected = pollution.get("is_polluted", False)
        except Exception as e:
            logger.error("Pollution detection failed: %s", e)
            result.errors.append(f"pollution: {e}")
        try:
            self._cleanup_orphaned_incrementals(user_id)
        except Exception as e:
            logger.error("Orphaned incremental cleanup failed: %s", e)
            result.errors.append(f"orphaned_incrementals: {e}")
        try:
            result.compressed_redundant = self._compress_redundant(user_id)
        except Exception as e:
            logger.error("Redundancy compression failed: %s", e)
            result.errors.append(f"redundant: {e}")
        # Reflection: synthesize cross-session patterns
        if reflection_engine is not None:
            try:
                ref_result = reflection_engine.reflect(user_id)
                result.scenes_created = ref_result.scenes_created
                result.reflection_candidates_found = ref_result.candidates_found
                result.reflection_candidates_synthesized = (
                    ref_result.candidates_passed
                    - ref_result.candidates_skipped_low_importance
                )
                result.reflection_candidates_skipped_low_importance = (
                    ref_result.candidates_skipped_low_importance
                )
                if ref_result.scenes_created:
                    logger.info(
                        "Reflection created %d scenes for user %s",
                        ref_result.scenes_created,
                        user_id,
                    )
            except Exception as e:
                logger.error("Reflection failed: %s", e)
                result.errors.append(f"reflection: {e}")
        return result

    def _has_changes_since_last_governance(self, db: Any, user_id: str) -> bool:
        """Check if user has memory changes since last daily governance run."""
        try:
            marker = self._daily_marker_key(user_id)
            last_run = db.execute(
                text(
                    "SELECT MAX(created_at) FROM governance_runs "
                    "WHERE task_name = :task"
                ),
                {"task": marker},
            ).scalar()
            if last_run is None:
                return True  # never governed before
            latest_change = db.execute(
                text(
                    "SELECT MAX(GREATEST(created_at, COALESCE(updated_at, created_at))) "
                    "FROM mem_memories WHERE user_id = :uid"
                ),
                {"uid": user_id},
            ).scalar()
            if latest_change is None:
                return False  # no memories at all
            return latest_change > last_run
        except Exception as e:
            logger.debug("Change detection failed for %s: %s", user_id, e)
            return True  # fail-open: run governance if check fails

    @staticmethod
    def _daily_marker_key(user_id: str) -> str:
        """Bounded task_name for governance_runs: ``daily_user:<id_or_hash>``.

        Short user_ids (≤48 chars) are kept verbatim for readability.
        Longer ones are sha256-truncated to 16 hex chars (64-bit).
        Total key length is always ≤64 chars (prefix 11 + id/hash ≤48).

        Note: 16 hex = 64-bit hash space → ~1 in 2³² collision probability
        at 77k users (birthday bound).  Acceptable for governance markers
        where a collision only causes a redundant governance skip, not data loss.
        """
        if len(user_id) <= 48:
            return f"daily_user:{user_id}"
        import hashlib

        uid_hash = hashlib.sha256(user_id.encode()).hexdigest()[:16]
        return f"daily_user:{uid_hash}"

    @staticmethod
    def _mark_daily_user(db: Any, user_id: str) -> None:
        """Write a per-user daily governance marker."""
        try:
            db.execute(
                text(
                    "INSERT INTO governance_runs (task_name, result, created_at) "
                    "VALUES (:task, :result, :ts)"
                ),
                {
                    "task": GovernanceScheduler._daily_marker_key(user_id),
                    "result": "{}",
                    "ts": _utcnow(),
                },
            )
            db.commit()
        except Exception as e:
            logger.debug("daily_user marker write failed for %s: %s", user_id, e)
            try:
                db.rollback()
            except Exception:
                pass

    # ── Weekly ────────────────────────────────────────────────────────

    def run_weekly(self) -> GovernanceCycleResult:
        """Weekly: orphan branch cleanup + snapshot cleanup."""
        result = GovernanceCycleResult()
        try:
            result.cleaned_branches = self.health.cleanup_orphan_branches()
        except Exception as e:
            logger.error("Branch cleanup failed: %s", e)
            result.errors.append(f"branches: {e}")
        try:
            result.cleaned_snapshots = self.health.cleanup_snapshots(
                keep_last_n=self.config.milestone_snapshot_keep_n
            )
        except Exception as e:
            logger.error("Snapshot cleanup failed: %s", e)
            result.errors.append(f"snapshots: {e}")
        return result

    # ── Internal steps ────────────────────────────────────────────────

    def _cleanup_tool_results(self) -> int:
        ttl = self.config.tool_result_ttl_hours
        total = 0
        batch_limit = 5000
        with self._db() as db:
            while True:
                result = db.execute(
                    text("""
                    DELETE FROM mem_memories
                    WHERE memory_type = :mtype
                      AND TIMESTAMPDIFF(HOUR, observed_at, NOW()) > :ttl
                    LIMIT :batch
                """),
                    {"mtype": "tool_result", "ttl": ttl, "batch": batch_limit},
                )
                db.commit()
                total += result.rowcount  # type: ignore[attr-defined]
                if result.rowcount < batch_limit:  # type: ignore[attr-defined]
                    break
        if total > 0:
            logger.info("Cleaned %d expired TOOL_RESULT memories (TTL=%dh)", total, ttl)
        return total

    def _archive_stale_working(self) -> int:
        """Archive working memories from sessions inactive > threshold hours."""
        stale_hours = self.config.working_memory_stale_hours
        with self._db() as db:
            # SELECT first to capture affected IDs for audit
            rows = db.execute(
                text(
                    "SELECT memory_id, user_id FROM mem_memories "
                    "WHERE memory_type = 'working' AND is_active = 1 "
                    "AND TIMESTAMPDIFF(HOUR, observed_at, NOW()) > :stale_hours"
                ),
                {"stale_hours": stale_hours},
            ).fetchall()
            if not rows:
                return 0
            ids = [r.memory_id for r in rows]
            for i in range(0, len(ids), 500):
                batch = ids[i : i + 500]
                placeholders = ", ".join(f":id{j}" for j in range(len(batch)))
                params: dict[str, object] = {
                    f"id{j}": mid for j, mid in enumerate(batch)
                }
                db.execute(
                    text(
                        f"UPDATE mem_memories SET is_active = 0, updated_at = NOW() "
                        f"WHERE memory_id IN ({placeholders})"
                    ),
                    params,
                )
            db.commit()
            # Audit: group by user_id
            by_user: dict[str, list[str]] = {}
            for r in rows:
                by_user.setdefault(r.user_id, []).append(r.memory_id)
            for uid, mids in by_user.items():
                self._log_governance_edit(
                    uid,
                    "governance:archive_working",
                    mids,
                    f"Archived {len(mids)} stale working memories (>{stale_hours}h)",
                )
        count = len(ids)
        if count > 0:
            logger.info("Archived %d stale working memories (>%dh)", count, stale_hours)
        return count

    def _cleanup_stale(self, user_id: str, confidence_threshold: float = 0.1) -> int:
        """Delete inactive memories with low initial_confidence (already superseded)."""
        with self._db() as db:
            result = db.execute(
                text("""
                DELETE FROM mem_memories
                WHERE user_id = :uid
                  AND is_active = 0
                  AND initial_confidence < :threshold
            """),
                {"uid": user_id, "threshold": confidence_threshold},
            )
            db.commit()
            return result.rowcount  # type: ignore[attr-defined]

    def _quarantine_low_confidence(self, user_id: str) -> int:
        """Deactivate memories whose effective_confidence is below quarantine threshold.

        Uses per-tier half-life from config.
        Memories with no trust_tier default to T3.
        """
        threshold = self.config.quarantine_threshold
        half_lives = self.config.half_lives
        quarantined_ids: list[str] = []
        with self._db() as db:
            for tier in TrustTier:
                hl = half_lives[tier.value]
                # SELECT first to capture affected IDs
                rows = db.execute(
                    text("""
                    SELECT memory_id FROM mem_memories
                    WHERE user_id = :uid AND is_active = 1
                      AND COALESCE(trust_tier, 'T3') = :tier
                      AND (initial_confidence * EXP(-TIMESTAMPDIFF(DAY, observed_at, NOW()) / :hl)) < :threshold
                """),
                    {
                        "uid": user_id,
                        "tier": tier.value,
                        "hl": hl,
                        "threshold": threshold,
                    },
                ).fetchall()
                if rows:
                    ids = [r.memory_id for r in rows]
                    quarantined_ids.extend(ids)
                    for i in range(0, len(ids), 500):
                        batch = ids[i : i + 500]
                        placeholders = ", ".join(f":id{j}" for j in range(len(batch)))
                        params: dict[str, object] = {
                            f"id{j}": mid for j, mid in enumerate(batch)
                        }
                        db.execute(
                            text(
                                f"UPDATE mem_memories SET is_active = 0, updated_at = NOW() "
                                f"WHERE memory_id IN ({placeholders})"
                            ),
                            params,
                        )
            db.commit()
        quarantined = len(quarantined_ids)
        if quarantined > 0:
            self._log_governance_edit(
                user_id,
                "governance:quarantine",
                quarantined_ids,
                f"Quarantined {quarantined} memories below threshold {threshold:.2f}",
            )
            logger.info(
                "Quarantined %d memories below threshold %.2f", quarantined, threshold
            )
        return quarantined

    def _cleanup_orphaned_incrementals(
        self, user_id: str, older_than_hours: int = 24
    ) -> int:
        """Deactivate session-scoped incremental summaries from sessions that were never closed.

        A session is considered abnormally terminated if:
        - It has incremental summaries (content LIKE '[session_summary:incremental]%')
        - Those summaries are older than `older_than_hours`
        - No full summary (session_id IS NULL) was created after them for the same user

        Called from run_daily() so it runs once per day per user.
        """
        with self._db() as db:
            # SELECT first to capture affected IDs
            rows = db.execute(
                text("""
                SELECT inc.memory_id FROM mem_memories AS inc
                WHERE inc.user_id = :uid
                  AND inc.is_active = 1
                  AND inc.content LIKE '[session_summary:incremental]%'
                  AND inc.session_id IS NOT NULL
                  AND TIMESTAMPDIFF(HOUR, inc.observed_at, NOW()) > :hours
                  AND NOT EXISTS (
                      SELECT 1 FROM mem_memories AS full_s
                      WHERE full_s.user_id = :uid
                        AND full_s.is_active = 1
                        AND full_s.session_id IS NULL
                        AND full_s.content LIKE '[session_summary]%'
                        AND full_s.observed_at > inc.observed_at
                  )
            """),
                {"uid": user_id, "hours": older_than_hours},
            ).fetchall()
            if not rows:
                return 0
            ids = [r.memory_id for r in rows]
            for i in range(0, len(ids), 500):
                batch = ids[i : i + 500]
                placeholders = ", ".join(f":id{j}" for j in range(len(batch)))
                params: dict[str, object] = {
                    f"id{j}": mid for j, mid in enumerate(batch)
                }
                db.execute(
                    text(
                        f"UPDATE mem_memories SET is_active = 0, updated_at = NOW() "
                        f"WHERE memory_id IN ({placeholders})"
                    ),
                    params,
                )
            db.commit()
            count = len(ids)

        if count:
            self._log_governance_edit(
                user_id,
                "governance:cleanup_orphaned_incrementals",
                ids,
                f"Cleaned {count} orphaned incremental summaries",
            )
            logger.info(
                "Cleaned %d orphaned incremental summaries for user %s", count, user_id
            )
        return count

    def _compress_redundant(self, user_id: str) -> int:
        """Deactivate near-duplicate memories, keeping the newer one.

        Strategy: per memory_type, build a numpy embedding matrix and use
        vectorized L2² distance to find near-duplicates efficiently.
        Only considers active memories within the configured time window.

        Cost control:
        - Per-type grouping avoids cross-type comparisons.
        - Global pairs_checked cap stops early across all types.
        - Already-deactivated ids are skipped in inner loop.
        - numpy vectorized diff avoids per-element Python overhead on
          high-dimensional embeddings (e.g. 1024-d).
        """
        import numpy as np

        cfg = self.config
        threshold = cfg.redundant_similarity_threshold
        window_days = cfg.redundant_window_days
        max_pairs = cfg.redundant_max_pairs

        # For normalized embeddings: L2² = 2(1 - cos_sim)
        l2_sq_threshold = 2.0 * (1.0 - threshold)

        deactivated = 0
        with self._db() as db:
            rows = db.execute(
                text("""
                SELECT memory_id, memory_type, observed_at, embedding
                FROM mem_memories
                WHERE user_id = :uid
                  AND is_active = 1
                  AND embedding IS NOT NULL
                  AND TIMESTAMPDIFF(DAY, observed_at, NOW()) <= :window
                ORDER BY memory_type, observed_at DESC
            """),
                {"uid": user_id, "window": window_days},
            ).fetchall()

            if len(rows) < 2:
                return 0

            by_type: dict[str, list] = {}
            for r in rows:
                by_type.setdefault(r.memory_type, []).append(r)

            to_deactivate: set[str] = set()
            superseded_by: dict[str, str] = {}  # older_id -> newer_id
            pairs_checked = 0

            for _mtype, group in by_type.items():
                if len(group) < 2:
                    continue
                ids = [r.memory_id for r in group]
                timestamps = [r.observed_at for r in group]
                raw = [
                    json.loads(r.embedding)
                    if isinstance(r.embedding, str)
                    else r.embedding
                    for r in group
                ]
                embs = np.array(raw, dtype=np.float32)

                # Group is ordered by observed_at DESC: i is newer than j when i < j
                for i in range(len(group)):
                    if ids[i] in to_deactivate:
                        continue
                    # Vectorized: compute L2² from row i to all j > i at once
                    diffs = embs[i + 1 :] - embs[i]
                    dists_sq = np.einsum("ij,ij->i", diffs, diffs)
                    for k, dist_sq in enumerate(dists_sq):
                        j = i + 1 + k
                        if pairs_checked >= max_pairs:
                            break
                        if ids[j] in to_deactivate:
                            continue
                        pairs_checked += 1
                        if dist_sq < l2_sq_threshold:
                            if timestamps[i] >= timestamps[j]:
                                older, newer = ids[j], ids[i]
                            else:
                                older, newer = ids[i], ids[j]
                            to_deactivate.add(older)
                            superseded_by[older] = newer
                    if pairs_checked >= max_pairs:
                        break
                if pairs_checked >= max_pairs:
                    break

            if to_deactivate:
                ids_list = list(to_deactivate)
                for i in range(0, len(ids_list), 500):
                    batch = ids_list[i : i + 500]
                    for mid in batch:
                        newer_id = superseded_by.get(mid)
                        db.execute(
                            text(
                                "UPDATE mem_memories SET is_active = 0, updated_at = NOW()"
                                + (", superseded_by = :newer" if newer_id else "")
                                + " WHERE memory_id = :mid"
                            ),
                            {"mid": mid, **({"newer": newer_id} if newer_id else {})},
                        )
                db.commit()
                deactivated = len(to_deactivate)

        if deactivated:
            self._log_governance_edit(
                user_id,
                "governance:compress_redundant",
                list(to_deactivate),
                f"Compressed {deactivated} redundant memories (threshold={threshold:.2f})",
            )
            logger.info(
                "Compressed %d redundant memories for user %s (threshold=%.2f)",
                deactivated,
                user_id,
                threshold,
            )
        return deactivated

    # IVF index config per table: (index_name, column, op_type_str)
    _IVF_INDEX_CONFIG = {
        "mem_memories": ("idx_memory_embedding", "embedding", "vector_l2_ops"),
        "memory_graph_nodes": (
            "idx_graph_node_embedding",
            "embedding",
            "vector_l2_ops",
        ),
    }

    def rebuild_vector_index(self, table: str) -> dict:
        """Rebuild IVF index for a table with optimal lists count.

        lists = max(1, total_rows // 50), capped at 1024.
        Returns {table, old_lists, new_lists, total_rows}.
        """
        if table not in self._IVF_INDEX_CONFIG:
            raise ValueError(
                f"Unknown table: {table}. Known: {list(self._IVF_INDEX_CONFIG)}"
            )

        index_name, column, op_type_str = self._IVF_INDEX_CONFIG[table]

        try:
            from memoria.api.database import _mo_client
            from matrixone.sqlalchemy_ext.vector_index import VectorOpType

            if VectorManager is None:
                raise RuntimeError("matrixone.vector_manager not available")
            vm = VectorManager(_mo_client)
        except Exception as e:
            raise RuntimeError(f"VectorManager not available: {e}") from e

        # Get current stats
        stats = vm.get_ivf_stats(table, column)
        counts = stats["distribution"]["centroid_count"]
        total_rows = sum(counts) if counts else 0
        old_lists = len(counts)

        # Compute optimal lists
        new_lists = max(1, min(total_rows // 50, 1024))

        op_type = VectorOpType.VECTOR_L2_OPS  # only l2 supported for now

        logger.info(
            "Rebuilding IVF index %s on %s: lists %d → %d (rows=%d)",
            index_name,
            table,
            old_lists,
            new_lists,
            total_rows,
        )

        vm.drop(table, index_name)
        vm.create_ivf(table, index_name, column, lists=new_lists, op_type=op_type)

        logger.info("IVF index %s rebuilt successfully", index_name)
        return {
            "table": table,
            "old_lists": old_lists,
            "new_lists": new_lists,
            "total_rows": total_rows,
        }

    def _check_vector_index_health(self) -> dict:
        """Check IVF index health for memory tables.

        health rules based on rows/centroids ratio:
        - < 20k rows:   centroids in [1, rows/50], i.e. ratio >= 50
        - 20k–1M rows:  500 < rows/centroids < 1000
        - > 1M rows:    centroids >= 1024
        """
        tables = ["mem_memories", "memory_graph_nodes"]
        health: dict = {}
        try:
            from memoria.api.database import _mo_client

            if VectorManager is None:
                logger.debug("VectorManager not available")
                return {}
            vm = VectorManager(_mo_client)
        except Exception as e:
            logger.debug("VectorManager not available: %s", e)
            return {}

        for table in tables:
            try:
                stats = vm.get_ivf_stats(table, "embedding")
                counts = stats["distribution"]["centroid_count"]
                if not counts:
                    continue
                n_centroids = len(counts)
                total_rows = sum(counts)
                ratio = total_rows / n_centroids if n_centroids > 0 else float("inf")

                if total_rows < 20_000:
                    # centroids must be in [1, total_rows/50], i.e. ratio >= 50
                    needs_rebuild = ratio < 50
                elif total_rows < 1_000_000:
                    needs_rebuild = ratio < 500 or ratio >= 1000
                else:
                    needs_rebuild = n_centroids < 1024

                health[table] = {
                    "centroids": n_centroids,
                    "total_rows": total_rows,
                    "ratio": round(ratio, 1),
                    "needs_rebuild": needs_rebuild,
                }
                if needs_rebuild:
                    logger.warning(
                        "IVF index unhealthy for %s: total_rows=%d centroids=%d ratio=%.1f",
                        table,
                        total_rows,
                        n_centroids,
                        ratio,
                    )
            except Exception as e:
                health[table] = {"error": str(e)}
        return health

    def store(
        self,
        user_id: str,
        content: str,
        *,
        memory_type: Any,
        source_event_ids: list[str] | None = None,
        initial_confidence: float = 0.75,
        trust_tier: Any = None,
        session_id: str | None = None,
    ) -> Any:
        """MemoryWriter.store() — delegates to MemoryStore.create()."""
        from memoria.core.memory.types import Memory, TrustTier as TT

        mem = Memory(
            memory_id="",
            user_id=user_id,
            memory_type=memory_type,
            content=content,
            initial_confidence=initial_confidence,
            trust_tier=trust_tier or TT.T4_UNVERIFIED,
            source_event_ids=source_event_ids or [],
            session_id=session_id,
        )
        return self._store.create(mem)
