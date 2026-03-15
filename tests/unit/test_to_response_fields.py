"""Regression test for Bug 5: _to_response must include session_id and initial_confidence."""

from __future__ import annotations
import pytest
from datetime import datetime, timezone


@pytest.mark.integration
class TestToResponseFields:
    """Bug 5: _to_response was missing session_id, causing it to be lost in API responses."""

    def _make_memory(self, session_id=None, retrieval_score=None):
        from memoria.core.memory.types import MemoryType, TrustTier, Memory

        m = Memory(
            memory_id="m1",
            user_id="user1",
            memory_type=MemoryType.EPISODIC,
            content="Session Summary: test",
            initial_confidence=0.8,
            trust_tier=TrustTier.T3_INFERRED,
            session_id=session_id,
            observed_at=datetime.now(timezone.utc),
        )
        m.retrieval_score = retrieval_score
        return m

    def test_session_id_in_response(self):
        from memoria.api.routers.memory import _to_response

        m = self._make_memory(session_id="sess-xyz")
        r = _to_response(m)
        assert r["session_id"] == "sess-xyz"

    def test_session_id_none_when_cross_session(self):
        from memoria.api.routers.memory import _to_response

        m = self._make_memory(session_id=None)
        r = _to_response(m)
        assert r["session_id"] is None

    def test_initial_confidence_in_response(self):
        from memoria.api.routers.memory import _to_response

        m = self._make_memory()
        r = _to_response(m)
        assert r["initial_confidence"] == pytest.approx(0.8)

    def test_retrieval_score_in_response(self):
        from memoria.api.routers.memory import _to_response

        m = self._make_memory(retrieval_score=0.92)
        r = _to_response(m)
        assert r["retrieval_score"] == pytest.approx(0.92)
