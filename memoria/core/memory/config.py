"""Memory governance configuration."""

from __future__ import annotations

import os
from dataclasses import dataclass, fields


def _env_float(key: str, default: float) -> float:
    v = os.environ.get(key)
    return float(v) if v is not None else default


def _env_int(key: str, default: int) -> int:
    v = os.environ.get(key)
    return int(v) if v is not None else default


@dataclass
class MemoryGovernanceConfig:
    """Configurable parameters for memory governance.

    All timing, threshold, and decay parameters in one place.
    Loads overrides from environment variables at construction time.
    Env var name: ``MEM_`` + uppercase field name, e.g. ``MEM_HALF_LIFE_T1_DAYS=400``.
    """

    pitr_range_value: int = 14
    pitr_range_unit: str = "d"
    milestone_snapshot_keep_n: int = 5
    pollution_threshold: float = 0.3
    sandbox_enabled_types: tuple[str, ...] = ("profile",)
    contradiction_similarity_threshold: float = 0.85
    redundant_similarity_threshold: float = 0.95
    redundant_window_days: int = 90
    redundant_max_pairs: int = 5000

    # ── Cleanup TTLs ──
    tool_result_ttl_hours: int = 24
    tool_result_max_per_session: int = 100
    tool_result_cleanup_on_session_close: bool = True
    working_memory_stale_hours: int = 2

    # ── Confidence decay (per trust tier) ──
    half_life_t1_days: float = (
        365.0  # matches TrustTier.T1_VERIFIED.default_half_life_days
    )
    half_life_t2_days: float = 180.0
    half_life_t3_days: float = 60.0
    half_life_t4_days: float = 30.0

    # ── Quarantine ──
    quarantine_threshold: float = 0.3

    # ── Session summary ──
    session_summary_turn_threshold: int = 10
    session_summary_time_threshold_hours: float = 10 / 60  # 10 minutes

    # ── Reflection: candidate selection ──
    cluster_similarity_threshold: float = 0.8
    min_cross_session_count: int = 2
    min_summary_recurrence: int = 3
    summary_recurrence_window_days: int = 7

    # ── Reflection: importance scoring ──
    reflection_daily_threshold: float = 0.5
    reflection_immediate_threshold: float = 0.7
    reflection_llm_threshold: float = 0.5  # below this: candidate-only (no LLM call)

    # ── Opinion evolution ──
    opinion_supporting_delta: float = 0.05
    opinion_contradicting_delta: float = -0.12
    opinion_confidence_cap: float = 0.95
    opinion_supporting_threshold: float = 0.8
    opinion_contradicting_threshold: float = 0.3
    opinion_quarantine_threshold: float = 0.18
    opinion_t4_to_t3_confidence: float = 0.8
    opinion_t4_to_t3_min_age_days: int = 7

    # ── Entity retrieval ──
    entity_boost: float = 1.8
    entity_node_type_weight: float = 0.8

    # ── Distributed: run_daily_all sharding ──
    daily_batch_size: int = 2000
    shard_index: int = 0  # this worker's shard (0-based)
    shard_count: int = 1  # total workers (1 = no sharding)

    # ── Backend selector ──
    memory_backend: str = "tabular"

    def __post_init__(self) -> None:
        """Validate parameter ranges."""
        for name in (
            "half_life_t1_days",
            "half_life_t2_days",
            "half_life_t3_days",
            "half_life_t4_days",
        ):
            if getattr(self, name) <= 0:
                raise ValueError(f"{name} must be positive, got {getattr(self, name)}")
        for name in (
            "quarantine_threshold",
            "pollution_threshold",
            "contradiction_similarity_threshold",
            "cluster_similarity_threshold",
            "reflection_daily_threshold",
            "reflection_immediate_threshold",
            "reflection_llm_threshold",
            "redundant_similarity_threshold",
        ):
            v = getattr(self, name)
            if not 0.0 <= v <= 1.0:
                raise ValueError(f"{name} must be in [0, 1], got {v}")
        if self.redundant_window_days <= 0:
            raise ValueError(
                f"redundant_window_days must be positive, got {self.redundant_window_days}"
            )
        if self.redundant_max_pairs <= 0:
            raise ValueError(
                f"redundant_max_pairs must be positive, got {self.redundant_max_pairs}"
            )
        if self.shard_count < 1:
            raise ValueError(f"shard_count must be >= 1, got {self.shard_count}")
        if not 0 <= self.shard_index < self.shard_count:
            raise ValueError(
                f"shard_index must be in [0, {self.shard_count}), got {self.shard_index}"
            )

    @property
    def half_lives(self) -> dict[str, float]:
        """Return half-life mapping keyed by tier value string."""
        return {
            "T1": self.half_life_t1_days,
            "T2": self.half_life_t2_days,
            "T3": self.half_life_t3_days,
            "T4": self.half_life_t4_days,
        }

    @classmethod
    def from_env(cls) -> MemoryGovernanceConfig:
        """Create config with overrides from ``MEM_*`` environment variables."""
        overrides: dict[str, object] = {}
        for f in fields(cls):
            env_key = f"MEM_{f.name.upper()}"
            val = os.environ.get(env_key)
            if val is None:
                continue
            if f.type == "float":
                overrides[f.name] = float(val)
            elif f.type == "int":
                overrides[f.name] = int(val)
            elif f.type == "bool":
                overrides[f.name] = val.lower() in ("1", "true", "yes")
            elif f.type == "str":
                overrides[f.name] = val
            # skip complex types (tuple, etc.) — not env-friendly
        return cls(**overrides)  # type: ignore[arg-type]


# Default config instance — picks up MEM_* env overrides automatically
DEFAULT_CONFIG = MemoryGovernanceConfig.from_env()
