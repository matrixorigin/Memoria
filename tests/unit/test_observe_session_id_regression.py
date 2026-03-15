"""Regression tests for Bug 3 (Memoria side): session_id propagation in observe_turn.

Without session_id, observer-extracted memories get session_id=NULL,
breaking session-scoped retrieval and mixing them with cross-session episodic memories.
"""

from __future__ import annotations

import pytest


@pytest.mark.integration
class TestObserveTurnSessionIdPropagation:
    """session_id must flow: API → MemoryService → CanonicalStorage → TypedObserver → Memory."""

    def test_extract_candidates_sets_session_id(self):
        """TypedObserver.extract_candidates must set session_id on each candidate."""
        from unittest.mock import MagicMock, patch
        from memoria.core.memory.tabular.typed_observer import TypedObserver

        store = MagicMock()
        llm = MagicMock()
        llm.chat.return_value = (
            '[{"content": "User prefers Python", "memory_type": "semantic"}]'
        )

        observer = TypedObserver(store=store, llm_client=llm, embed_fn=None)

        with patch.object(
            observer,
            "_extract_via_llm",
            return_value=[
                {"content": "User prefers Python", "memory_type": "semantic"}
            ],
        ):
            candidates = observer.extract_candidates(
                user_id="user1",
                messages=[{"role": "user", "content": "I prefer Python"}],
                session_id="sess-test",
            )

        assert len(candidates) > 0
        for c in candidates:
            assert c.session_id == "sess-test", (
                f"Memory session_id must be 'sess-test', got {c.session_id!r}"
            )

    def test_extract_candidates_no_session_id_leaves_none(self):
        """Without session_id, candidates must have session_id=None (cross-session)."""
        from unittest.mock import MagicMock, patch
        from memoria.core.memory.tabular.typed_observer import TypedObserver

        store = MagicMock()
        observer = TypedObserver(store=store, llm_client=MagicMock(), embed_fn=None)

        with patch.object(
            observer,
            "_extract_via_llm",
            return_value=[
                {"content": "User prefers Python", "memory_type": "semantic"}
            ],
        ):
            candidates = observer.extract_candidates(
                user_id="user1",
                messages=[{"role": "user", "content": "I prefer Python"}],
                # no session_id
            )

        for c in candidates:
            assert c.session_id is None

    def test_observe_passes_session_id_to_extract_candidates(self):
        """TypedObserver.observe must pass session_id to extract_candidates."""
        from unittest.mock import MagicMock, patch
        from memoria.core.memory.tabular.typed_observer import TypedObserver

        store = MagicMock()
        observer = TypedObserver(store=store, llm_client=MagicMock(), embed_fn=None)

        with (
            patch.object(
                observer, "extract_candidates", return_value=[]
            ) as mock_extract,
            patch.object(
                observer,
                "persist_with_contradiction_check",
                return_value=(MagicMock(), None),
            ),
        ):
            observer.observe(
                user_id="user1",
                messages=[{"role": "user", "content": "test"}],
                session_id="sess-abc",
            )

        mock_extract.assert_called_once()
        call_kwargs = mock_extract.call_args.kwargs
        assert call_kwargs.get("session_id") == "sess-abc"
