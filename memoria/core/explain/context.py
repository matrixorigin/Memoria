"""Explain context management using contextvars."""

from __future__ import annotations

import contextvars
import time
from dataclasses import dataclass, field
from typing import Any

from .types import ExplainLevel, ExplainVersion, DEFAULT_EXPLAIN_VERSION, should_collect

# Context variable for explain context
_explain_ctx: contextvars.ContextVar[ExplainContext | None] = contextvars.ContextVar(
    "explain_ctx", default=None
)


@dataclass
class PhaseStats:
    """Statistics for a single execution phase."""

    name: str
    elapsed_ms: float = 0.0
    metrics: dict[str, Any] = field(default_factory=dict)
    error: str | None = None


@dataclass
class ExplainContext:
    """Container for explain data throughout a request lifecycle."""

    level: ExplainLevel
    version: ExplainVersion = DEFAULT_EXPLAIN_VERSION
    path: str | None = None
    total_ms: float | None = None
    phases: dict[str, PhaseStats] = field(default_factory=dict)
    metrics: dict[str, Any] = field(default_factory=dict)
    errors: list[str] = field(default_factory=list)
    _start_time: float | None = field(default=None, repr=False)

    def __post_init__(self):
        if self._start_time is None:
            self._start_time = time.perf_counter()

    def set_path(self, path: str) -> None:
        """Set the execution path (e.g., 'graph+vector', 'vector_fallback')."""
        if should_collect(self.level, ExplainLevel.BASIC):
            self.path = path

    def add_phase(self, name: str, elapsed_ms: float, **metrics: Any) -> None:
        """Add phase statistics."""
        if not should_collect(self.level, ExplainLevel.BASIC):
            return
        self.phases[name] = PhaseStats(
            name=name, elapsed_ms=elapsed_ms, metrics=metrics
        )

    def add_metric(self, key: str, value: Any) -> None:
        """Add a top-level metric."""
        if not should_collect(self.level, ExplainLevel.VERBOSE):
            return
        self.metrics[key] = value

    def add_error(self, phase: str, error: str) -> None:
        """Record an error during explain collection."""
        self.errors.append(f"{phase}: {error}")

    def finish(self) -> None:
        """Mark the explain context as complete and calculate total time."""
        if self._start_time is not None:
            self.total_ms = (time.perf_counter() - self._start_time) * 1000

    def to_dict(self) -> dict[str, Any]:
        """Convert to dictionary for API response."""
        if self.level == ExplainLevel.NONE:
            return {}

        result: dict[str, Any] = {
            "version": self.version.value,
            "level": self.level.value,
        }

        if self.path is not None:
            result["path"] = self.path

        if self.total_ms is not None:
            result["total_ms"] = round(self.total_ms, 2)

        # BASIC: phases timing
        if should_collect(self.level, ExplainLevel.BASIC) and self.phases:
            result["phases"] = {
                name: {"ms": round(stats.elapsed_ms, 2)}
                for name, stats in self.phases.items()
            }

        # VERBOSE: phase metrics
        if should_collect(self.level, ExplainLevel.VERBOSE):
            if self.phases:
                result["phases"] = {
                    name: {
                        "ms": round(stats.elapsed_ms, 2),
                        **stats.metrics,
                    }
                    for name, stats in self.phases.items()
                }
            if self.metrics:
                result["metrics"] = self.metrics

        # ANALYZE: everything including internal details
        if should_collect(self.level, ExplainLevel.ANALYZE):
            # Add raw timing data for analysis
            if self._start_time is not None:
                result["_raw_start"] = self._start_time

        if self.errors:
            result["errors"] = self.errors

        return result


def get_explain_ctx() -> ExplainContext | None:
    """Get current explain context."""
    return _explain_ctx.get()


def init_explain(level: ExplainLevel | str) -> ExplainContext | None:
    """Initialize explain context for current request.

    Args:
        level: Explain level (enum or string)

    Returns:
        ExplainContext or None if level is NONE
    """
    if isinstance(level, str):
        try:
            level = ExplainLevel(level)
        except ValueError:
            level = ExplainLevel.NONE

    if level == ExplainLevel.NONE:
        _explain_ctx.set(None)
        return None

    ctx = ExplainContext(level=level)
    _explain_ctx.set(ctx)
    return ctx


def clear_explain() -> None:
    """Clear explain context."""
    _explain_ctx.set(None)
