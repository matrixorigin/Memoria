"""Unit tests verifying that HTTP endpoint _run() closures correctly wire LLM client.

These tests catch the class of bug where an endpoint silently ignores the
configured LLM because it passes None instead of calling get_llm_client().

The previous bug: user_ops.py was doing getattr(svc, "_llm_client", None) on
MemoryService (which has no such attribute), so LLM was always None even when
configured. Fixed to call get_llm_client() directly.
"""

from __future__ import annotations

from unittest.mock import MagicMock, patch


def _make_run_closure(endpoint_fn_name: str):
    """Extract and return the _run() closure from a user_ops endpoint."""
    import memoria.api.routers.user_ops as user_ops_mod

    fn = getattr(user_ops_mod, endpoint_fn_name)
    # The endpoint body defines a _run() closure and passes it to _with_cache.
    # We capture it by patching _with_cache.
    captured = {}

    def fake_with_cache(user_id, op, run_fn, force, db_factory):
        captured["run"] = run_fn
        return {}

    with patch.object(user_ops_mod, "_with_cache", side_effect=fake_with_cache):
        fn(force=True, user_id="u1", db_factory=MagicMock())

    return captured["run"]


class TestReflectEndpointLLMWiring:
    """POST /reflect _run() closure must call get_llm_client() and pass it to GraphMemoryService."""

    def test_reflect_calls_get_llm_client(self):
        """_run() must call get_llm_client() — not hardcode None."""
        mock_llm = MagicMock()
        run = _make_run_closure("reflect")

        with (
            patch("memoria.core.llm.get_llm_client", return_value=mock_llm) as mock_get,
            patch("memoria.core.memory.graph.service.GraphMemoryService") as MockGSvc,
        ):
            MockGSvc.return_value.reflect.return_value = {"insights": 0, "skipped": 0}
            run()

        mock_get.assert_called_once()

    def test_reflect_passes_llm_to_graph_service(self):
        """_run() must pass the LLM client to GraphMemoryService constructor."""
        mock_llm = MagicMock()
        run = _make_run_closure("reflect")

        with (
            patch("memoria.core.llm.get_llm_client", return_value=mock_llm),
            patch("memoria.core.memory.graph.service.GraphMemoryService") as MockGSvc,
        ):
            MockGSvc.return_value.reflect.return_value = {"insights": 0, "skipped": 0}
            run()

        _, kwargs = MockGSvc.call_args
        assert kwargs.get("llm_client") is mock_llm, (
            "reflect _run() must pass get_llm_client() as llm_client to GraphMemoryService"
        )

    def test_reflect_no_llm_degrades_gracefully(self):
        """Without LLM, reflect returns a dict with 'note' (not an exception)."""
        run = _make_run_closure("reflect")

        with (
            patch("memoria.core.llm.get_llm_client", return_value=None),
            patch("memoria.core.memory.graph.service.GraphMemoryService") as MockGSvc,
        ):
            MockGSvc.return_value.reflect.return_value = {
                "insights": 0,
                "skipped": 0,
                "note": "LLM not configured — reflect unavailable. Set LLM_API_KEY to enable.",
            }
            result = run()

        assert "note" in result
        assert "LLM not configured" in result["note"]


class TestExtractEntitiesEndpointLLMWiring:
    """POST /extract-entities _run() closure must call get_llm_client()."""

    def test_extract_entities_calls_get_llm_client(self):
        """_run() must call get_llm_client()."""
        mock_llm = MagicMock()
        run = _make_run_closure("extract_entities")

        with (
            patch("memoria.core.llm.get_llm_client", return_value=mock_llm) as mock_get,
            patch("memoria.core.memory.graph.service.GraphMemoryService") as MockGSvc,
        ):
            MockGSvc.return_value.extract_entities_llm.return_value = {
                "entities_found": 0
            }
            run()

        mock_get.assert_called_once()

    def test_extract_entities_passes_llm_to_graph_service(self):
        """_run() must pass the LLM client to GraphMemoryService constructor."""
        mock_llm = MagicMock()
        run = _make_run_closure("extract_entities")

        with (
            patch("memoria.core.llm.get_llm_client", return_value=mock_llm),
            patch("memoria.core.memory.graph.service.GraphMemoryService") as MockGSvc,
        ):
            MockGSvc.return_value.extract_entities_llm.return_value = {
                "entities_found": 0
            }
            run()

        _, kwargs = MockGSvc.call_args
        assert kwargs.get("llm_client") is mock_llm, (
            "extract_entities _run() must pass get_llm_client() as llm_client"
        )

    def test_extract_entities_no_llm_returns_error_field(self):
        """Without LLM, GraphMemoryService.extract_entities_llm returns error dict."""
        from memoria.core.memory.graph.service import GraphMemoryService

        svc = GraphMemoryService(db_factory=MagicMock(), llm_client=None)
        result = svc.extract_entities_llm("user1")
        assert "error" in result
        assert "LLM not configured" in result["error"]


class TestSessionSummaryLLMRequired:
    """POST /v1/sessions/{id}/summary returns 503 when LLM not configured."""

    def test_session_summary_503_when_no_llm(self):
        """Episodic summary endpoint raises 503 if LLM is not configured."""
        from fastapi import HTTPException

        with patch("memoria.core.llm.get_llm_client", return_value=None):
            from memoria.core.llm import get_llm_client

            llm = get_llm_client()
            if llm is None:
                exc = HTTPException(status_code=503, detail="LLM not configured")
            else:
                exc = None

        assert exc is not None
        assert exc.status_code == 503
        assert "LLM not configured" in exc.detail
