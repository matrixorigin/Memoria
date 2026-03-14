"""Memory type definitions and data structures."""

from __future__ import annotations

import enum
import math
from dataclasses import dataclass, field
from datetime import datetime, timezone
from typing import Optional


def _utcnow() -> datetime:
    """Return current UTC time as timezone-aware datetime."""
    return datetime.now(timezone.utc)


def enum_value(e) -> str:
    """Get string value from Enum, handling None and non-Enum types.

    Args:
        e: An Enum instance, string, or None.

    Returns:
        The .value if Enum, str(e) otherwise, or empty string for None.
    """
    if e is None:
        return ""
    return e.value if hasattr(e, "value") else str(e)


class MemoryType(str, enum.Enum):
    PROFILE = "profile"
    SEMANTIC = "semantic"
    PROCEDURAL = "procedural"
    WORKING = "working"
    TOOL_RESULT = "tool_result"


class TrustTier(str, enum.Enum):
    T1_VERIFIED = "T1"
    T2_CURATED = "T2"
    T3_INFERRED = "T3"
    T4_UNVERIFIED = "T4"

    @property
    def default_half_life_days(self) -> float:
        """Built-in default half-life per tier (days)."""
        return _TIER_HALF_LIFE[self]


_TIER_HALF_LIFE: dict["TrustTier", float] = {
    TrustTier.T1_VERIFIED: 365.0,
    TrustTier.T2_CURATED: 180.0,
    TrustTier.T3_INFERRED: 60.0,
    TrustTier.T4_UNVERIFIED: 30.0,
}


# Initial confidence per trust tier (from architecture §1)
TRUST_TIER_INITIAL_CONFIDENCE: dict[TrustTier, float] = {
    TrustTier.T1_VERIFIED: 0.95,
    TrustTier.T2_CURATED: 0.85,
    TrustTier.T3_INFERRED: 0.65,
    TrustTier.T4_UNVERIFIED: 0.40,
}


def trust_tier_defaults(tier: str) -> dict[str, float]:
    """Return initial_confidence and half_life_days for a trust tier string."""
    try:
        t = TrustTier(tier)
    except ValueError:
        t = TrustTier.T3_INFERRED
    return {
        "initial_confidence": TRUST_TIER_INITIAL_CONFIDENCE[t],
        "half_life_days": t.default_half_life_days,
    }


@dataclass
class Memory:
    """In-memory representation of a memory record."""

    memory_id: str
    user_id: str
    memory_type: MemoryType
    content: str
    initial_confidence: float = 0.75
    embedding: Optional[list[float]] = None
    source_event_ids: list[str] = field(default_factory=list)
    superseded_by: Optional[str] = None
    is_active: bool = True
    access_count: int = 0
    session_id: Optional[str] = None
    observed_at: Optional[datetime] = None
    created_at: Optional[datetime] = None
    updated_at: Optional[datetime] = None
    trust_tier: TrustTier = TrustTier.T3_INFERRED
    retrieval_score: Optional[float] = (
        None  # set by retriever; None if not retrieved via scoring
    )

    def effective_confidence(self, half_life_days: Optional[float] = None) -> float:
        """Query-time confidence decay. Uses tier-specific half-life if not overridden."""
        if self.observed_at is None:
            return self.initial_confidence
        if half_life_days is None:
            half_life_days = self.trust_tier.default_half_life_days
        now = _utcnow()
        observed = self.observed_at
        # Handle naive datetime from DB (assume UTC)
        if observed.tzinfo is None:
            observed = observed.replace(tzinfo=timezone.utc)
        age_days = (now - observed).total_seconds() / 86400.0
        return self.initial_confidence * math.exp(-age_days / half_life_days)


@dataclass
class RetrievalWeights:
    """Weights for hybrid retrieval scoring dimensions."""

    vector: float = 0.3
    keyword: float = 0.2
    temporal: float = 0.2
    confidence: float = 0.3

    def __post_init__(self) -> None:
        total = self.vector + self.keyword + self.temporal + self.confidence
        if abs(total - 1.0) > 0.01:
            raise ValueError(f"Weights must sum to 1.0, got {total}")
