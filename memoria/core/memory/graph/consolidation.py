"""GraphConsolidator — periodic graph maintenance.

1. Detect cross-session contradictions (via edge table scan)
2. Check scene node source integrity
3. Trust tier lifecycle: T4→T3 promotion (age-gated), T3→T4 demotion (staleness)

See docs/design/memory/graph-memory.md §4.2, §4.7, §5.4, §5.5
"""

from __future__ import annotations

import logging
from dataclasses import dataclass, field
from datetime import datetime, timezone
from typing import TYPE_CHECKING

from memoria.core.memory.graph.graph_store import GraphStore
from memoria.core.memory.graph.types import GraphNodeData, NodeType

if TYPE_CHECKING:
    from memoria.core.db_consumer import DbFactory
    from memoria.core.memory.config import MemoryGovernanceConfig

logger = logging.getLogger(__name__)

CONTRADICTION_ASSOCIATION_THRESHOLD = 0.7
SOURCE_INTEGRITY_RATIO = 0.5
T3_DEMOTION_STALE_DAYS = 60
T3_TO_T2_MIN_AGE_DAYS = 30
T3_TO_T2_MIN_CONFIDENCE = 0.85
T3_TO_T2_MIN_CROSS_SESSION = 3
T2_DEMOTION_CONFIDENCE = 0.7


@dataclass
class ConsolidationResult:
    merged_nodes: int = 0
    conflicts_detected: int = 0
    orphaned_scenes: int = 0
    promoted: int = 0
    demoted: int = 0
    errors: list[str] = field(default_factory=list)


class GraphConsolidator:
    def __init__(
        self, db_factory: DbFactory, config: MemoryGovernanceConfig | None = None
    ) -> None:
        self._store = GraphStore(db_factory)
        if config is None:
            from memoria.core.memory.config import DEFAULT_CONFIG

            config = DEFAULT_CONFIG
        self._config = config

    def consolidate(self, user_id: str) -> ConsolidationResult:
        result = ConsolidationResult()
        try:
            result.conflicts_detected = self._detect_conflicts(user_id)
        except Exception as e:
            logger.warning("Conflict detection failed: %s", e)
            result.errors.append(f"conflicts: {e}")
        try:
            result.orphaned_scenes = self._check_source_integrity(user_id)
        except Exception as e:
            logger.warning("Source integrity check failed: %s", e)
            result.errors.append(f"integrity: {e}")
        try:
            promoted, demoted = self._trust_tier_lifecycle(user_id)
            result.promoted = promoted
            result.demoted = demoted
        except Exception as e:
            logger.warning("Trust tier lifecycle failed: %s", e)
            result.errors.append(f"tier_lifecycle: {e}")
        return result

    def _detect_conflicts(self, user_id: str) -> int:
        """Detect contradictions: nodes whose current embedding cosine sim dropped
        below 0.4 despite having a strong historical association edge (weight >= 0.7).

        This catches cases where a memory was corrected (embedding changed) but the
        old association edge still exists — the edge weight reflects historical similarity,
        while the current embedding reflects the corrected content.

        Algorithm: single DB query returns (edge_weight, current_cosine_sim) per pair.
        No Python-side embedding loading or computation.
        """
        # Single query: association edges with current cosine similarity
        # edge_weight = historical cosine sim (at edge creation time)
        # current_sim = cosine sim of current embeddings
        candidates = self._store.get_association_edges_with_current_sim(
            user_id,
            min_edge_weight=CONTRADICTION_ASSOCIATION_THRESHOLD,
            max_current_sim=0.4,
        )
        if not candidates:
            return 0

        # Load only the conflicting node pairs
        candidate_ids = {nid for src, tgt, _, _ in candidates for nid in (src, tgt)}
        nodes = self._store.get_nodes_by_ids(list(candidate_ids))
        node_map = {n.node_id: n for n in nodes}

        conflicts_found = 0
        for src_id, tgt_id, _edge_w, _cur_sim in candidates:
            node = node_map.get(src_id)
            neighbor = node_map.get(tgt_id)
            if not node or not neighbor:
                continue
            if not node.is_active or not neighbor.is_active:
                continue
            if (
                node.node_type != NodeType.SEMANTIC
                or neighbor.node_type != NodeType.SEMANTIC
            ):
                continue
            if node.conflicts_with or neighbor.conflicts_with:
                continue
            if node.session_id == neighbor.session_id:
                continue

            if node.node_id < neighbor.node_id:
                older, newer = node, neighbor
            else:
                older, newer = neighbor, node

            self._store.mark_conflict(
                older_id=older.node_id,
                newer_id=newer.node_id,
                confidence_factor=0.5,
                old_confidence=older.confidence,
            )
            conflicts_found += 1

        return conflicts_found

    def _check_source_integrity(self, user_id: str) -> int:
        """Check scene nodes for orphaned sources."""
        scene_nodes = self._store.get_user_nodes(
            user_id,
            node_type=NodeType.SCENE,
            active_only=True,
            load_embedding=False,
        )
        orphaned = 0
        for scene in scene_nodes:
            if not scene.source_nodes:
                continue
            source_nodes = self._store.get_nodes_by_ids(scene.source_nodes)
            active_sources = [n for n in source_nodes if n.is_active]
            if len(active_sources) == 0:
                self._store.deactivate_node(scene.node_id)
                orphaned += 1
            elif len(active_sources) < len(scene.source_nodes) * SOURCE_INTEGRITY_RATIO:
                self._store.update_confidence(scene.node_id, scene.confidence * 0.8)
        return orphaned

    def _trust_tier_lifecycle(self, user_id: str) -> tuple[int, int]:
        """Trust tier promotion and demotion.

        Promotion: T4 → T3 if confidence > threshold AND age > min_days.
        Promotion: T3 → T2 if confidence ≥ 0.85 AND age ≥ 30 days AND cross_session ≥ 3.
        Demotion:  T3 → T4 if age > stale_days AND confidence < threshold.
        Demotion:  T2 → T3 if confidence < 0.7 (after contradicting evidence).

        Returns:
            (promoted_count, demoted_count)
        """
        now = datetime.now(timezone.utc)
        min_age_days = self._config.opinion_t4_to_t3_min_age_days
        confidence_threshold = self._config.opinion_t4_to_t3_confidence

        # Load all active scene nodes (skeleton — no embeddings)
        scenes = self._store.get_user_nodes(
            user_id,
            node_type=NodeType.SCENE,
            active_only=True,
            load_embedding=False,
        )

        promoted = 0
        demoted = 0

        for scene in scenes:
            age_days = self._node_age_days(scene, now)

            if scene.trust_tier == "T4":
                # T4 → T3: confidence ≥ 0.8 AND age ≥ 7 days
                if (
                    scene.confidence >= confidence_threshold
                    and age_days >= min_age_days
                ):
                    self._store.update_confidence_and_tier(
                        scene.node_id, scene.confidence, "T3"
                    )
                    promoted += 1
                    logger.info(
                        "Promoted scene %s T4→T3 (confidence=%.2f, age=%d days)",
                        scene.node_id,
                        scene.confidence,
                        age_days,
                    )

            elif scene.trust_tier == "T3":
                # T3 → T2: confidence ≥ 0.85 AND age ≥ 30 days AND cross_session ≥ 3
                if (
                    scene.confidence >= T3_TO_T2_MIN_CONFIDENCE
                    and age_days >= T3_TO_T2_MIN_AGE_DAYS
                    and scene.cross_session_count >= T3_TO_T2_MIN_CROSS_SESSION
                ):
                    self._store.update_confidence_and_tier(
                        scene.node_id, scene.confidence, "T2"
                    )
                    promoted += 1
                    logger.info(
                        "Promoted scene %s T3→T2 (confidence=%.2f, age=%d, cross_session=%d)",
                        scene.node_id,
                        scene.confidence,
                        age_days,
                        scene.cross_session_count,
                    )
                # T3 → T4: stale with low confidence
                elif (
                    age_days >= T3_DEMOTION_STALE_DAYS
                    and scene.confidence < confidence_threshold
                ):
                    self._store.update_confidence_and_tier(
                        scene.node_id, scene.confidence, "T4"
                    )
                    demoted += 1
                    logger.info(
                        "Demoted scene %s T3→T4 (confidence=%.2f, age=%d days)",
                        scene.node_id,
                        scene.confidence,
                        age_days,
                    )

            elif scene.trust_tier == "T2":
                # T2 → T3: confidence dropped below threshold
                if scene.confidence < T2_DEMOTION_CONFIDENCE:
                    self._store.update_confidence_and_tier(
                        scene.node_id, scene.confidence, "T3"
                    )
                    demoted += 1
                    logger.info(
                        "Demoted scene %s T2→T3 (confidence=%.2f)",
                        scene.node_id,
                        scene.confidence,
                    )

        return promoted, demoted

    @staticmethod
    def _node_age_days(node: GraphNodeData, now: datetime) -> int:
        """Calculate node age in days from created_at string."""
        if not node.created_at:
            return 0
        try:
            # created_at stored as string from DB, e.g. "2026-03-01 10:00:00"
            created = datetime.fromisoformat(str(node.created_at).replace(" ", "T"))
            if created.tzinfo is None:
                created = created.replace(tzinfo=timezone.utc)
            return max(0, (now - created).days)
        except (ValueError, TypeError):
            return 0
