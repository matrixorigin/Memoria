"""Core types for EXPLAIN ANALYZE."""

from __future__ import annotations

import enum


class ExplainLevel(str, enum.Enum):
    """EXPLAIN level - controls detail and overhead.

    - NONE: No explain information (default, zero overhead)
    - BASIC: Basic execution path and timing
    - VERBOSE: Detailed phase information and candidate counts
    - ANALYZE: Full execution statistics with actual metrics
    """

    NONE = "none"
    BASIC = "basic"
    VERBOSE = "verbose"
    ANALYZE = "analyze"


class ExplainVersion(str, enum.Enum):
    """Explain output version for backward compatibility."""

    V1_0 = "1.0"


# Default explain version
DEFAULT_EXPLAIN_VERSION = ExplainVersion.V1_0


def should_collect(level: ExplainLevel, min_level: ExplainLevel) -> bool:
    """Check if current level should collect data for min_level.

    Args:
        level: Current explain level
        min_level: Minimum level required for collection

    Returns:
        True if should collect
    """
    levels = [
        ExplainLevel.NONE,
        ExplainLevel.BASIC,
        ExplainLevel.VERBOSE,
        ExplainLevel.ANALYZE,
    ]
    try:
        return levels.index(level) >= levels.index(min_level)
    except ValueError:
        return False
