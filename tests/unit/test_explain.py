"""Tests for EXPLAIN ANALYZE functionality."""

from __future__ import annotations

from memoria.core.explain import (
    ExplainLevel,
    ExplainContext,
    init_explain,
    get_explain_ctx,
    clear_explain,
    explainable,
    explain_timer,
    add_explain_metric,
    set_explain_path,
)


class TestExplainLevel:
    """Test ExplainLevel enum."""

    def test_enum_values(self):
        assert ExplainLevel.NONE.value == "none"
        assert ExplainLevel.BASIC.value == "basic"
        assert ExplainLevel.VERBOSE.value == "verbose"
        assert ExplainLevel.ANALYZE.value == "analyze"


class TestExplainContext:
    """Test ExplainContext functionality."""

    def test_basic_level(self):
        ctx = ExplainContext(level=ExplainLevel.BASIC)
        ctx.set_path("test_path")
        ctx.add_phase("phase1", 10.5)
        ctx.finish()

        result = ctx.to_dict()
        assert result["version"] == "1.0"
        assert result["level"] == "basic"
        assert result["path"] == "test_path"
        assert "total_ms" in result
        assert "phases" in result
        assert result["phases"]["phase1"]["ms"] == 10.5

    def test_verbose_level_includes_metrics(self):
        ctx = ExplainContext(level=ExplainLevel.VERBOSE)
        ctx.add_metric("candidates", 100)
        ctx.add_phase("retrieval", 20.0, rows_scanned=50)
        ctx.finish()

        result = ctx.to_dict()
        assert result["level"] == "verbose"
        assert result["metrics"]["candidates"] == 100
        assert result["phases"]["retrieval"]["rows_scanned"] == 50

    def test_analyze_level(self):
        ctx = ExplainContext(level=ExplainLevel.ANALYZE)
        ctx.finish()

        result = ctx.to_dict()
        assert result["level"] == "analyze"
        assert "_raw_start" in result

    def test_none_level_returns_empty(self):
        ctx = ExplainContext(level=ExplainLevel.NONE)
        ctx.finish()

        result = ctx.to_dict()
        assert result == {}

    def test_error_collection(self):
        ctx = ExplainContext(level=ExplainLevel.BASIC)
        ctx.add_error("phase1", "something went wrong")
        ctx.finish()

        result = ctx.to_dict()
        assert "errors" in result
        assert "phase1: something went wrong" in result["errors"]


class TestExplainContextVars:
    """Test context variable management."""

    def test_init_explain_basic(self):
        ctx = init_explain("basic")
        assert ctx is not None
        assert ctx.level == ExplainLevel.BASIC
        assert get_explain_ctx() is ctx

    def test_init_explain_none(self):
        ctx = init_explain("none")
        assert ctx is None
        assert get_explain_ctx() is None

    def test_init_explain_invalid(self):
        ctx = init_explain("invalid")
        assert ctx is None

    def test_clear_explain(self):
        init_explain("basic")
        clear_explain()
        assert get_explain_ctx() is None


class TestExplainableDecorator:
    """Test @explainable decorator."""

    def test_decorator_times_function(self):
        init_explain("basic")

        @explainable("test_phase")
        def slow_function():
            import time

            time.sleep(0.01)
            return "done"

        result = slow_function()
        assert result == "done"

        ctx = get_explain_ctx()
        assert "test_phase" in ctx.phases
        assert ctx.phases["test_phase"].elapsed_ms > 0

        clear_explain()

    def test_decorator_no_explain_context(self):
        clear_explain()

        @explainable("test_phase")
        def normal_function():
            return "done"

        result = normal_function()
        assert result == "done"


class TestExplainTimer:
    """Test explain_timer context manager."""

    def test_timer_records_phase(self):
        init_explain("basic")

        with explain_timer("db_query"):
            import time

            time.sleep(0.01)

        ctx = get_explain_ctx()
        assert "db_query" in ctx.phases
        assert ctx.phases["db_query"].elapsed_ms > 0

        clear_explain()

    def test_timer_with_metrics(self):
        init_explain("verbose")

        with explain_timer("search") as timer:
            timer.add_metric("rows", 100)
            timer.add_metric("index_hits", 5)

        ctx = get_explain_ctx()
        assert ctx.phases["search"].metrics["rows"] == 100
        assert ctx.phases["search"].metrics["index_hits"] == 5

        clear_explain()


class TestConvenienceFunctions:
    """Test convenience functions."""

    def test_add_explain_metric(self):
        init_explain("verbose")

        add_explain_metric("cache_hits", 10)

        ctx = get_explain_ctx()
        assert ctx.metrics["cache_hits"] == 10

        clear_explain()

    def test_add_explain_metric_no_context(self):
        clear_explain()
        # Should not raise
        add_explain_metric("key", "value")

    def test_set_explain_path(self):
        init_explain("basic")

        set_explain_path("graph+vector")

        ctx = get_explain_ctx()
        assert ctx.path == "graph+vector"

        clear_explain()


class TestExplainIntegration:
    """Integration tests for explain workflow."""

    def test_full_workflow(self):
        """Simulate a full retrieval workflow with explain."""
        ctx = init_explain("verbose")

        # Simulate embedding phase
        with explain_timer("embedding") as t:
            t.add_metric("tokens", 50)

        # Simulate retrieval phase
        with explain_timer("retrieval") as t:
            t.add_metric("candidates", 100)
            t.add_metric("filtered", 10)

        set_explain_path("vector_fallback")
        add_explain_metric("total_memories", 1000)

        ctx.finish()

        result = ctx.to_dict()
        assert result["level"] == "verbose"
        assert result["path"] == "vector_fallback"
        assert "embedding" in result["phases"]
        assert "retrieval" in result["phases"]
        assert result["metrics"]["total_memories"] == 1000
        assert result["phases"]["embedding"]["tokens"] == 50

        clear_explain()
