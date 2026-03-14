"""Decorators for automatic explain data collection."""

from __future__ import annotations

import functools
import time
from typing import Any, Callable, TypeVar

from .context import get_explain_ctx
from .types import ExplainLevel, should_collect

F = TypeVar("F", bound=Callable[..., Any])


def explainable(phase_name: str | None = None) -> Callable[[F], F]:
    """Decorator to automatically time and record a function execution.

    Args:
        phase_name: Name for the phase (defaults to function name)

    Example:
        @explainable("retrieval")
        def retrieve(self, ...):
            ...

        @explainable()  # uses function name
        def vector_search(self, ...):
            ...
    """

    def decorator(func: F) -> F:
        name = phase_name or func.__name__

        @functools.wraps(func)
        def wrapper(*args: Any, **kwargs: Any) -> Any:
            ctx = get_explain_ctx()
            if ctx is None or not should_collect(ctx.level, ExplainLevel.BASIC):
                return func(*args, **kwargs)

            start = time.perf_counter()
            try:
                result = func(*args, **kwargs)
                elapsed = (time.perf_counter() - start) * 1000
                ctx.add_phase(name, elapsed)
                return result
            except Exception as e:
                elapsed = (time.perf_counter() - start) * 1000
                ctx.add_phase(name, elapsed, error=str(e))
                raise

        return wrapper  # type: ignore[return-value]

    return decorator


class explain_timer:
    """Context manager for timing code blocks.

    Example:
        with explain_timer("vector_search") as timer:
            results = db.query(...)
            timer.add_metric("rows", len(results))
    """

    def __init__(self, phase_name: str) -> None:
        self.phase_name = phase_name
        self.ctx = get_explain_ctx()
        self.start: float | None = None
        self.metrics: dict[str, Any] = {}

    def __enter__(self) -> explain_timer:
        if self.ctx is not None and should_collect(self.ctx.level, ExplainLevel.BASIC):
            self.start = time.perf_counter()
        return self

    def __exit__(self, exc_type: Any, exc_val: Any, exc_tb: Any) -> None:
        if self.ctx is None or self.start is None:
            return

        elapsed = (time.perf_counter() - self.start) * 1000

        if exc_val is not None:
            self.metrics["error"] = str(exc_val)

        self.ctx.add_phase(self.phase_name, elapsed, **self.metrics)

    def add_metric(self, key: str, value: Any) -> None:
        """Add a metric to this phase."""
        if self.ctx is not None and should_collect(
            self.ctx.level, ExplainLevel.VERBOSE
        ):
            self.metrics[key] = value


def add_explain_metric(key: str, value: Any) -> None:
    """Add a metric to current explain context (convenience function).

    Args:
        key: Metric name
        value: Metric value
    """
    ctx = get_explain_ctx()
    if ctx is not None:
        ctx.add_metric(key, value)


def set_explain_path(path: str) -> None:
    """Set execution path in current explain context.

    Args:
        path: Execution path description
    """
    ctx = get_explain_ctx()
    if ctx is not None:
        ctx.set_path(path)
