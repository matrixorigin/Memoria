"""EXPLAIN ANALYZE for Memoria API operations.

Provides execution statistics and debugging information for memory operations,
similar to SQL EXPLAIN ANALYZE.

Usage:
    from memoria.core.explain import ExplainLevel, get_explain_ctx, explainable

    # In API endpoint
    explain_ctx = init_explain(request.explain)
    result = service.retrieve(...)
    return {"results": result, "explain": explain_ctx.to_dict()}
"""

from .types import ExplainLevel, ExplainVersion
from .context import ExplainContext, get_explain_ctx, init_explain, clear_explain
from .decorator import explainable, explain_timer, add_explain_metric, set_explain_path

__all__ = [
    "ExplainLevel",
    "ExplainVersion",
    "ExplainContext",
    "get_explain_ctx",
    "init_explain",
    "clear_explain",
    "explainable",
    "explain_timer",
    "add_explain_metric",
    "set_explain_path",
]
