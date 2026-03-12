"""Tests for ReflectionEngine — candidate → importance → LLM → persist."""

import json
from unittest.mock import MagicMock


from memoria.core.memory.interfaces import ReflectionCandidate
from memoria.core.memory.reflection.engine import ReflectionEngine
from memoria.core.memory.reflection.importance import DAILY_THRESHOLD
from memoria.core.memory.types import Memory, MemoryType, TrustTier


def _mem(mid: str = "m1", session_id: str = "s1", content: str = "test") -> Memory:
    return Memory(
        memory_id=mid,
        user_id="u1",
        memory_type=MemoryType.SEMANTIC,
        content=content,
        session_id=session_id,
    )


def _candidate(
    n_memories: int = 3,
    n_sessions: int = 3,
    signal: str = "semantic_cluster",
    importance: float = 0.6,
) -> ReflectionCandidate:
    return ReflectionCandidate(
        memories=[_mem(f"m{i}", f"s{i}", f"content {i}") for i in range(n_memories)],
        signal=signal,
        importance_score=importance,
        session_ids=[f"s{i}" for i in range(n_sessions)],
    )


class TestMemoryWriterProtocolCompliance:
    """Regression: ReflectionEngine must call MemoryWriter.store(), not store_memory()."""

    def test_engine_calls_store_not_store_memory(self):
        """ReflectionEngine._persist_insight must call writer.store()."""
        from unittest.mock import sentinel

        provider = MagicMock()
        candidate = _candidate(
            n_memories=5, n_sessions=4, signal="contradiction", importance=0.6
        )
        provider.get_reflection_candidates.return_value = [candidate]

        # Writer that has store() but NOT store_memory()
        writer = MagicMock(spec=["store"])
        writer.store.return_value = sentinel.memory

        llm = MagicMock()
        llm.chat.return_value = json.dumps(
            [
                {
                    "type": "procedural",
                    "content": "test insight",
                    "confidence": 0.5,
                    "evidence_summary": "e",
                }
            ]
        )

        engine = ReflectionEngine(
            candidate_provider=provider,
            writer=writer,
            llm_client=llm,
        )
        result = engine.reflect("u1")

        assert result.scenes_created == 1
        writer.store.assert_called_once()
        assert not hasattr(writer, "store_memory")

    def test_governance_scheduler_satisfies_writer_store(self):
        """GovernanceScheduler must have store() matching MemoryWriter protocol."""
        from unittest.mock import MagicMock

        from memoria.core.memory.tabular.governance import GovernanceScheduler

        gov = GovernanceScheduler(MagicMock(), llm_client=None)
        assert hasattr(gov, "store"), "GovernanceScheduler must implement store()"
        assert callable(gov.store)


class TestReflectionEngine:
    def _make_engine(self, candidates, llm_response, threshold=DAILY_THRESHOLD):
        provider = MagicMock()
        provider.get_reflection_candidates.return_value = candidates

        writer = MagicMock()

        llm = MagicMock()
        llm.chat.return_value = llm_response

        engine = ReflectionEngine(
            candidate_provider=provider,
            writer=writer,
            llm_client=llm,
            threshold=threshold,
        )
        return engine, provider, writer, llm

    def test_no_candidates_returns_empty_result(self):
        engine, provider, writer, llm = self._make_engine([], "")

        result = engine.reflect("u1")

        assert result.candidates_found == 0
        assert result.candidates_passed == 0
        assert result.scenes_created == 0
        assert result.llm_calls == 0
        llm.chat.assert_not_called()
        writer.store.assert_not_called()

    def test_candidates_below_threshold_filtered_out(self):
        # Single-session, small cluster → low score
        weak = ReflectionCandidate(
            memories=[_mem("m1", "s1")],
            signal="semantic_cluster",
            session_ids=["s1"],
        )
        engine, provider, writer, llm = self._make_engine([weak], "")

        result = engine.reflect("u1")

        assert result.candidates_found == 1
        assert result.candidates_passed == 0
        assert result.llm_calls == 0

    def test_qualifying_candidate_triggers_llm_and_persist(self):
        candidate = _candidate(
            n_memories=5, n_sessions=4, signal="contradiction", importance=0.6
        )
        llm_response = json.dumps(
            [
                {
                    "type": "procedural",
                    "content": "Always run linter before commit",
                    "confidence": 0.6,
                    "evidence_summary": "User corrected this 3 times",
                }
            ]
        )
        engine, provider, writer, llm = self._make_engine([candidate], llm_response)

        result = engine.reflect("u1")

        assert result.candidates_found == 1
        assert result.candidates_passed == 1
        assert result.llm_calls == 1
        assert result.scenes_created == 1

        # Verify persist call
        writer.store.assert_called_once()
        call_kwargs = writer.store.call_args
        assert call_kwargs.kwargs["user_id"] == "u1"
        assert (
            call_kwargs.kwargs["content"]
            == "Always run linter before commit\n[source_count=5]"
        )
        assert call_kwargs.kwargs["memory_type"] == MemoryType.PROCEDURAL
        assert call_kwargs.kwargs["initial_confidence"] == 0.6
        assert call_kwargs.kwargs["trust_tier"] == TrustTier.T4_UNVERIFIED
        # source_memory_ids passed as source_event_ids for provenance
        assert call_kwargs.kwargs["source_event_ids"] == [f"m{i}" for i in range(5)]

    def test_llm_returns_two_insights(self):
        candidate = _candidate(
            n_memories=5, n_sessions=4, signal="contradiction", importance=0.6
        )
        llm_response = json.dumps(
            [
                {
                    "type": "procedural",
                    "content": "Insight 1",
                    "confidence": 0.5,
                    "evidence_summary": "e1",
                },
                {
                    "type": "semantic",
                    "content": "Insight 2",
                    "confidence": 0.4,
                    "evidence_summary": "e2",
                },
            ]
        )
        engine, provider, writer, llm = self._make_engine([candidate], llm_response)

        result = engine.reflect("u1")

        assert result.scenes_created == 2
        assert writer.store.call_count == 2

    def test_llm_returns_invalid_json_no_crash(self):
        candidate = _candidate(
            n_memories=5, n_sessions=4, signal="contradiction", importance=0.6
        )
        engine, provider, writer, llm = self._make_engine(
            [candidate], "not json at all"
        )

        result = engine.reflect("u1")

        assert result.llm_calls == 1
        assert result.scenes_created == 0
        assert len(result.errors) == 1  # parse failure recorded as error

    def test_llm_returns_empty_array(self):
        candidate = _candidate(
            n_memories=5, n_sessions=4, signal="contradiction", importance=0.6
        )
        engine, provider, writer, llm = self._make_engine([candidate], "[]")

        result = engine.reflect("u1")

        assert result.scenes_created == 0
        writer.store.assert_not_called()

    def test_confidence_clamped_to_range(self):
        candidate = _candidate(
            n_memories=5, n_sessions=4, signal="contradiction", importance=0.6
        )
        llm_response = json.dumps(
            [
                {
                    "type": "semantic",
                    "content": "test",
                    "confidence": 0.99,
                    "evidence_summary": "e",
                },
            ]
        )
        engine, provider, writer, llm = self._make_engine([candidate], llm_response)

        engine.reflect("u1")

        # Confidence clamped to max 0.7
        assert writer.store.call_args.kwargs["initial_confidence"] == 0.7

    def test_confidence_clamped_min(self):
        candidate = _candidate(
            n_memories=5, n_sessions=4, signal="contradiction", importance=0.6
        )
        llm_response = json.dumps(
            [
                {
                    "type": "semantic",
                    "content": "test",
                    "confidence": 0.1,
                    "evidence_summary": "e",
                },
            ]
        )
        engine, provider, writer, llm = self._make_engine([candidate], llm_response)

        engine.reflect("u1")

        assert writer.store.call_args.kwargs["initial_confidence"] == 0.3

    def test_invalid_memory_type_skipped(self):
        candidate = _candidate(
            n_memories=5, n_sessions=4, signal="contradiction", importance=0.6
        )
        llm_response = json.dumps(
            [
                {
                    "type": "invalid_type",
                    "content": "test",
                    "confidence": 0.5,
                    "evidence_summary": "e",
                },
            ]
        )
        engine, provider, writer, llm = self._make_engine([candidate], llm_response)

        result = engine.reflect("u1")

        assert result.scenes_created == 0

    def test_provider_error_captured(self):
        provider = MagicMock()
        provider.get_reflection_candidates.side_effect = RuntimeError("db down")
        engine = ReflectionEngine(
            candidate_provider=provider,
            writer=MagicMock(),
            llm_client=MagicMock(),
        )

        result = engine.reflect("u1")

        assert result.candidates_found == 0
        assert len(result.errors) == 1
        assert "db down" in result.errors[0]

    def test_llm_error_captured_per_candidate(self):
        candidate = _candidate(
            n_memories=5, n_sessions=4, signal="contradiction", importance=0.6
        )
        provider = MagicMock()
        provider.get_reflection_candidates.return_value = [candidate]
        llm = MagicMock()
        llm.chat.side_effect = RuntimeError("rate limited")
        engine = ReflectionEngine(
            candidate_provider=provider,
            writer=MagicMock(),
            llm_client=llm,
        )

        result = engine.reflect("u1")

        assert result.candidates_passed == 1
        assert result.scenes_created == 0
        assert len(result.errors) == 1
        assert "rate limited" in result.errors[0]

    def test_max_two_insights_per_candidate(self):
        candidate = _candidate(
            n_memories=5, n_sessions=4, signal="contradiction", importance=0.6
        )
        llm_response = json.dumps(
            [
                {
                    "type": "semantic",
                    "content": "i1",
                    "confidence": 0.5,
                    "evidence_summary": "e",
                },
                {
                    "type": "semantic",
                    "content": "i2",
                    "confidence": 0.5,
                    "evidence_summary": "e",
                },
                {
                    "type": "semantic",
                    "content": "i3",
                    "confidence": 0.5,
                    "evidence_summary": "e",
                },
            ]
        )
        engine, provider, writer, llm = self._make_engine([candidate], llm_response)

        result = engine.reflect("u1")

        assert result.scenes_created == 2  # max 2, not 3
