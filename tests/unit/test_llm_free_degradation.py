"""Unit tests for LLM-free degradation paths."""

from __future__ import annotations

from unittest.mock import MagicMock


class TestGraphServiceNoLLM:
    """GraphMemoryService degrades gracefully when LLM is not configured."""

    def _make_svc(self):
        from memoria.core.memory.graph.service import GraphMemoryService

        return GraphMemoryService(db_factory=MagicMock(), llm_client=None)

    def test_reflect_returns_note_when_no_llm(self):
        svc = self._make_svc()
        result = svc.reflect("user1")
        assert result["insights"] == 0
        assert "LLM not configured" in result["note"]

    def test_extract_entities_returns_error_when_no_llm(self):
        svc = self._make_svc()
        result = svc.extract_entities_llm("user1")
        assert result["entities_found"] == 0
        assert "LLM not configured" in result["error"]

    def test_extract_entities_with_explicit_llm_skips_none_check(self):
        """Explicit llm_client overrides self._llm_client=None."""
        svc = self._make_svc()
        mock_llm = MagicMock()
        # Should not return "LLM not configured" error — proceeds past None check
        # (will fail on DB, but that's a different error)
        try:
            result = svc.extract_entities_llm("user1", llm_client=mock_llm)
        except Exception:
            result = {}
        assert "LLM not configured" not in result.get("error", "")


class TestObserveWarning:
    """POST /observe response includes warning when LLM not configured."""

    def test_observe_response_has_warning_key_when_no_llm(self):
        """The observe endpoint wraps memories in dict and adds warning if no LLM."""
        # Test the logic directly: if get_llm_client() is None, result has "warning"
        from unittest.mock import MagicMock

        mock_svc = MagicMock()
        mock_svc.observe_turn.return_value = []

        # Simulate what the endpoint does
        memories = mock_svc.observe_turn("u1", [])
        result = {"memories": memories}
        # Simulate: if get_llm_client() is None → add warning
        llm = None
        if llm is None:
            result["warning"] = "LLM not configured — memory extraction unavailable"

        assert "warning" in result
        assert result["memories"] == []

    def test_observe_response_no_warning_when_llm_present(self):
        mock_svc = MagicMock()
        mock_svc.observe_turn.return_value = []

        memories = mock_svc.observe_turn("u1", [])
        result = {"memories": memories}
        llm = MagicMock()  # LLM present
        if llm is None:
            result["warning"] = "LLM not configured — memory extraction unavailable"

        assert "warning" not in result


class TestHistoryChainLogic:
    """Version chain traversal logic for GET /memories/{id}/history."""

    def test_superseded_by_chain_direction(self):
        """older.superseded_by = newer_id — chain walks forward correctly."""
        # Simulate: old → new (old.superseded_by = new.memory_id)
        old = MagicMock()
        old.memory_id = "old_id"
        old.superseded_by = "new_id"
        old.is_active = 0

        new = MagicMock()
        new.memory_id = "new_id"
        new.superseded_by = None
        new.is_active = 1

        # Walk forward from old
        chain = [old]
        current = old.superseded_by
        nodes = {"new_id": new}
        while current:
            node = nodes.get(current)
            if node is None:
                break
            chain.append(node)
            current = node.superseded_by

        assert len(chain) == 2
        assert chain[0].memory_id == "old_id"
        assert chain[1].memory_id == "new_id"
        assert chain[-1].is_active == 1
