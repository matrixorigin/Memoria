"""Coverage boost: pure-logic unit tests for previously uncovered modules.

Targets:
- core/exceptions.py (0% → 100%)
- core/utils/similarity.py (0% → 100%)
- core/memory/strategy/params.py (0% → 100%)
- core/validation.py (51% → 95%+)
- core/memory/types.py (76% → 95%+)
- core/memory/tabular/json_utils.py (76% → 100%)
- api/_model_types.py (65% → 95%+)
"""

from __future__ import annotations

import pytest


# ---------------------------------------------------------------------------
# exceptions.py
# ---------------------------------------------------------------------------


class TestExceptions:
    def test_agent_error_base(self):
        from memoria.core.exceptions import AgentError

        e = AgentError("something failed")
        assert e.message == "something failed"
        assert e.code == "AGENT_ERROR"
        assert str(e) == "something failed"

    def test_agent_error_custom_code(self):
        from memoria.core.exceptions import AgentError

        e = AgentError("msg", code="CUSTOM")
        assert e.code == "CUSTOM"

    def test_skill_not_found(self):
        from memoria.core.exceptions import SkillNotFoundError

        e = SkillNotFoundError("my_skill")
        assert "my_skill" in e.message
        assert e.skill_name == "my_skill"
        assert e.code == "SKILL_NOT_FOUND"

    def test_skill_not_found_with_version(self):
        from memoria.core.exceptions import SkillNotFoundError

        e = SkillNotFoundError("my_skill", version="1.2")
        assert "1.2" in e.message

    def test_skill_execution_error(self):
        from memoria.core.exceptions import SkillExecutionError

        e = SkillExecutionError("my_skill", "timeout")
        assert "my_skill" in e.message
        assert e.code == "SKILL_EXECUTION_ERROR"

    def test_skill_validation_error(self):
        from memoria.core.exceptions import SkillValidationError

        e = SkillValidationError("my_skill", "bad input")
        assert e.code == "SKILL_VALIDATION_ERROR"

    def test_replay_error(self):
        from memoria.core.exceptions import ReplayError

        e = ReplayError("replay failed", session_id="sess_123")
        assert e.session_id == "sess_123"
        assert e.code == "REPLAY_ERROR"

    def test_database_error(self):
        from memoria.core.exceptions import DatabaseError

        e = DatabaseError("connection refused")
        assert e.code == "DATABASE_ERROR"

    def test_llm_error(self):
        from memoria.core.exceptions import LLMError

        e = LLMError("api error", provider="openai")
        assert e.provider == "openai"
        assert e.code == "LLM_ERROR"

    def test_llm_timeout_error(self):
        from memoria.core.exceptions import LLMTimeoutError

        e = LLMTimeoutError("openai", 30.0)
        assert "30.0" in e.message
        assert e.code == "LLM_TIMEOUT"

    def test_llm_rate_limit_error(self):
        from memoria.core.exceptions import LLMRateLimitError

        e = LLMRateLimitError("openai")
        assert e.code == "LLM_RATE_LIMIT"

    def test_github_error(self):
        from memoria.core.exceptions import GitHubError

        e = GitHubError("not found", status_code=404)
        assert e.status_code == 404
        assert e.code == "GITHUB_ERROR"

    def test_github_rate_limit(self):
        from memoria.core.exceptions import GitHubRateLimitError

        e = GitHubRateLimitError()
        assert e.status_code == 429
        assert e.code == "GITHUB_RATE_LIMIT"

    def test_configuration_error(self):
        from memoria.core.exceptions import ConfigurationError

        e = ConfigurationError("missing key")
        assert e.code == "CONFIGURATION_ERROR"

    def test_authentication_error(self):
        from memoria.core.exceptions import AuthenticationError

        e = AuthenticationError()
        assert e.code == "AUTHENTICATION_ERROR"

    def test_authorization_error(self):
        from memoria.core.exceptions import AuthorizationError

        e = AuthorizationError("forbidden")
        assert e.code == "AUTHORIZATION_ERROR"

    def test_transient_error(self):
        from memoria.core.exceptions import TransientError

        e = TransientError("retry me", retry_after_ms=500)
        assert e.retry_after_ms == 500
        assert e.code == "TRANSIENT_ERROR"

    def test_memory_error(self):
        from memoria.core.exceptions import MemoryError

        e = MemoryError("store failed")
        assert e.code == "MEMORY_ERROR"

    def test_graph_ingest_error(self):
        from memoria.core.exceptions import GraphIngestError

        cause = RuntimeError("db down")
        e = GraphIngestError("mem_abc", cause)
        assert e.memory_id == "mem_abc"
        assert e.cause is cause
        assert "mem_abc" in e.message

    def test_exception_hierarchy(self):
        from memoria.core.exceptions import (
            AgentError,
            SkillError,
            SkillNotFoundError,
            LLMError,
            LLMTimeoutError,
            MemoryError,
            GraphIngestError,
        )

        assert issubclass(SkillNotFoundError, SkillError)
        assert issubclass(SkillError, AgentError)
        assert issubclass(LLMTimeoutError, LLMError)
        assert issubclass(GraphIngestError, MemoryError)


# ---------------------------------------------------------------------------
# utils/similarity.py
# ---------------------------------------------------------------------------


class TestSimilarity:
    def test_cosine_identical(self):
        from memoria.core.utils.similarity import cosine_similarity

        assert cosine_similarity([1.0, 0.0], [1.0, 0.0]) == pytest.approx(1.0)

    def test_cosine_orthogonal(self):
        from memoria.core.utils.similarity import cosine_similarity

        assert cosine_similarity([1.0, 0.0], [0.0, 1.0]) == pytest.approx(0.0)

    def test_cosine_opposite(self):
        from memoria.core.utils.similarity import cosine_similarity

        assert cosine_similarity([1.0, 0.0], [-1.0, 0.0]) == pytest.approx(-1.0)

    def test_cosine_zero_vector(self):
        from memoria.core.utils.similarity import cosine_similarity

        assert cosine_similarity([0.0, 0.0], [1.0, 0.0]) == 0.0

    def test_cosine_both_zero(self):
        from memoria.core.utils.similarity import cosine_similarity

        assert cosine_similarity([0.0, 0.0], [0.0, 0.0]) == 0.0

    def test_cosine_length_mismatch(self):
        from memoria.core.utils.similarity import cosine_similarity

        assert cosine_similarity([1.0], [1.0, 2.0]) == 0.0

    def test_word_overlap_identical(self):
        from memoria.core.utils.similarity import word_overlap

        assert word_overlap("hello world", "hello world") == pytest.approx(1.0)

    def test_word_overlap_disjoint(self):
        from memoria.core.utils.similarity import word_overlap

        assert word_overlap("foo bar", "baz qux") == pytest.approx(0.0)

    def test_word_overlap_partial(self):
        from memoria.core.utils.similarity import word_overlap

        score = word_overlap("hello world", "hello there")
        assert 0.0 < score < 1.0

    def test_word_overlap_empty(self):
        from memoria.core.utils.similarity import word_overlap

        assert word_overlap("", "hello") == 0.0
        assert word_overlap("hello", "") == 0.0

    def test_word_overlap_case_insensitive(self):
        from memoria.core.utils.similarity import word_overlap

        assert word_overlap("Hello World", "hello world") == pytest.approx(1.0)


# ---------------------------------------------------------------------------
# strategy/params.py
# ---------------------------------------------------------------------------


class TestStrategyParams:
    def test_vector_v1_defaults(self):
        from memoria.core.memory.strategy.params import VectorV1Params

        p = VectorV1Params()
        assert p.semantic_weight == 0.4
        assert p.temporal_weight == 0.3
        assert p.confidence_weight == 0.2
        assert p.importance_weight == 0.1

    def test_vector_v1_custom(self):
        from memoria.core.memory.strategy.params import VectorV1Params

        p = VectorV1Params(
            semantic_weight=0.9,
            temporal_weight=0.1,
            confidence_weight=0.0,
            importance_weight=0.0,
        )
        assert p.semantic_weight == 0.9

    def test_vector_v1_out_of_range(self):
        from memoria.core.memory.strategy.params import VectorV1Params

        with pytest.raises(Exception):
            VectorV1Params(semantic_weight=1.5)

    def test_activation_v1_defaults(self):
        from memoria.core.memory.strategy.params import ActivationV1Params

        p = ActivationV1Params()
        assert p.spreading_factor == 0.8
        assert p.num_iterations == 3

    def test_validate_strategy_params_none(self):
        from memoria.core.memory.strategy.params import validate_strategy_params

        assert validate_strategy_params("vector:v1", None) is None

    def test_validate_strategy_params_valid(self):
        from memoria.core.memory.strategy.params import validate_strategy_params

        result = validate_strategy_params(
            "vector:v1",
            {
                "semantic_weight": 0.5,
                "temporal_weight": 0.3,
                "confidence_weight": 0.1,
                "importance_weight": 0.1,
            },
        )
        assert result["semantic_weight"] == 0.5

    def test_validate_strategy_params_invalid(self):
        from memoria.core.memory.strategy.params import (
            validate_strategy_params,
            InvalidStrategyParamsError,
        )

        with pytest.raises(InvalidStrategyParamsError):
            validate_strategy_params("vector:v1", {"semantic_weight": 99.0})

    def test_validate_unknown_strategy_passthrough(self):
        from memoria.core.memory.strategy.params import validate_strategy_params

        params = {"foo": "bar"}
        assert validate_strategy_params("unknown:v99", params) == params

    def test_get_default_params(self):
        from memoria.core.memory.strategy.params import get_default_params

        p = get_default_params("vector:v1")
        assert p is not None
        assert "semantic_weight" in p

    def test_get_default_params_unknown(self):
        from memoria.core.memory.strategy.params import get_default_params

        assert get_default_params("unknown:v99") is None


# ---------------------------------------------------------------------------
# validation.py
# ---------------------------------------------------------------------------


class TestValidation:
    def test_validate_identifier_valid(self):
        from memoria.core.validation import validate_identifier

        assert validate_identifier("my_table") == "my_table"
        assert validate_identifier("_private") == "_private"

    def test_validate_identifier_with_dot(self):
        from memoria.core.validation import validate_identifier

        assert validate_identifier("db.table", allow_dot=True) == "db.table"

    def test_validate_identifier_empty(self):
        from memoria.core.validation import validate_identifier

        with pytest.raises(ValueError, match="empty"):
            validate_identifier("")

    def test_validate_identifier_too_long(self):
        from memoria.core.validation import validate_identifier

        with pytest.raises(ValueError, match="too long"):
            validate_identifier("a" * 65)

    def test_validate_identifier_invalid_chars(self):
        from memoria.core.validation import validate_identifier

        with pytest.raises(ValueError):
            validate_identifier("'; DROP TABLE--")

    def test_validate_identifier_starts_with_digit(self):
        from memoria.core.validation import validate_identifier

        with pytest.raises(ValueError):
            validate_identifier("1table")

    def test_query_request_valid(self):
        from memoria.core.validation import QueryRequest

        q = QueryRequest(
            user_id="alice", session_id="sess_1", query="what did I work on"
        )
        assert q.user_id == "alice"

    def test_query_request_sql_injection(self):
        from memoria.core.validation import QueryRequest

        with pytest.raises(Exception):
            QueryRequest(
                user_id="alice", session_id="sess_1", query="x; DROP TABLE users"
            )

    def test_query_request_union_select(self):
        from memoria.core.validation import QueryRequest

        with pytest.raises(Exception):
            QueryRequest(
                user_id="alice",
                session_id="sess_1",
                query="x UNION SELECT * FROM users",
            )

    def test_sanitize_string(self):
        from memoria.core.validation import sanitize_string

        assert sanitize_string("hello world") == "hello world"
        assert sanitize_string("a" * 2000, max_length=100) == "a" * 100
        assert "\x00" not in sanitize_string("hello\x00world")

    def test_validate_repo_id_valid(self):
        from memoria.core.validation import validate_repo_id

        assert validate_repo_id(42) == 42

    def test_validate_repo_id_zero(self):
        from memoria.core.validation import validate_repo_id

        with pytest.raises(ValueError):
            validate_repo_id(0)

    def test_validate_repo_id_too_large(self):
        from memoria.core.validation import validate_repo_id

        with pytest.raises(ValueError):
            validate_repo_id(2147483648)

    def test_validate_session_id_valid(self):
        from memoria.core.validation import validate_session_id

        assert validate_session_id("sess-abc_123") == "sess-abc_123"

    def test_validate_session_id_invalid(self):
        from memoria.core.validation import validate_session_id

        with pytest.raises(ValueError):
            validate_session_id("sess with spaces")

    def test_validate_session_id_too_long(self):
        from memoria.core.validation import validate_session_id

        with pytest.raises(ValueError):
            validate_session_id("a" * 256)

    def test_skill_execution_request_valid(self):
        from memoria.core.validation import SkillExecutionRequest

        r = SkillExecutionRequest(
            skill_name="my_skill", user_id="alice", session_id="s1"
        )
        assert r.skill_name == "my_skill"

    def test_skill_execution_request_large_params(self):
        from memoria.core.validation import SkillExecutionRequest

        with pytest.raises(Exception):
            SkillExecutionRequest(
                skill_name="my_skill",
                user_id="alice",
                session_id="s1",
                parameters={"data": "x" * 200000},
            )


# ---------------------------------------------------------------------------
# tabular/json_utils.py
# ---------------------------------------------------------------------------


class TestJsonUtils:
    def test_plain_json_array(self):
        from memoria.core.memory.tabular.json_utils import parse_json_array

        result = parse_json_array('[{"key": "val"}]')
        assert result == [{"key": "val"}]

    def test_markdown_fenced(self):
        from memoria.core.memory.tabular.json_utils import parse_json_array

        text = '```json\n[{"a": 1}]\n```'
        assert parse_json_array(text) == [{"a": 1}]

    def test_markdown_fenced_no_lang(self):
        from memoria.core.memory.tabular.json_utils import parse_json_array

        text = '```\n[{"a": 1}]\n```'
        assert parse_json_array(text) == [{"a": 1}]

    def test_embedded_in_prose(self):
        from memoria.core.memory.tabular.json_utils import parse_json_array

        text = 'Here is the result: [{"x": 2}] done.'
        assert parse_json_array(text) == [{"x": 2}]

    def test_invalid_returns_empty(self):
        from memoria.core.memory.tabular.json_utils import parse_json_array

        assert parse_json_array("not json at all") == []

    def test_empty_string(self):
        from memoria.core.memory.tabular.json_utils import parse_json_array

        assert parse_json_array("") == []

    def test_object_not_array(self):
        from memoria.core.memory.tabular.json_utils import parse_json_array

        # JSON object (not array) → falls through to regex, returns []
        result = parse_json_array('{"key": "val"}')
        assert result == []


# ---------------------------------------------------------------------------
# api/_model_types.py
# ---------------------------------------------------------------------------


class TestModelTypes:
    def test_datetime6_col_spec(self):
        from memoria.api._model_types import DateTime6

        dt = DateTime6()
        assert dt.get_col_spec() == "DATETIME(6)"

    def test_nullable_json_none_returns_null(self):
        from memoria.api._model_types import NullableJSON
        from unittest.mock import MagicMock

        nj = NullableJSON()
        dialect = MagicMock()
        # impl_instance.bind_processor returns a function
        nj.impl_instance = MagicMock()
        nj.impl_instance.bind_processor.return_value = lambda v: f'"{v}"'
        processor = nj.bind_processor(dialect)
        assert processor(None) is None

    def test_nullable_json_value_delegates(self):
        from memoria.api._model_types import NullableJSON
        from unittest.mock import MagicMock

        nj = NullableJSON()
        dialect = MagicMock()
        nj.impl_instance = MagicMock()
        nj.impl_instance.bind_processor.return_value = lambda v: f"processed:{v}"
        processor = nj.bind_processor(dialect)
        assert processor("hello") == "processed:hello"

    def test_nullable_json_no_impl_processor(self):
        from memoria.api._model_types import NullableJSON
        from unittest.mock import MagicMock

        nj = NullableJSON()
        dialect = MagicMock()
        nj.impl_instance = MagicMock()
        nj.impl_instance.bind_processor.return_value = None
        processor = nj.bind_processor(dialect)
        assert processor("raw") == "raw"
        assert processor(None) is None


# ---------------------------------------------------------------------------
# core/memory/types.py — uncovered branches
# ---------------------------------------------------------------------------


class TestMemoryTypes:
    def test_utcnow_is_aware(self):
        from memoria.core.memory.types import _utcnow
        from datetime import timezone

        dt = _utcnow()
        assert dt.tzinfo is not None
        assert dt.tzinfo == timezone.utc

    def test_enum_value_enum(self):
        from memoria.core.memory.types import enum_value, MemoryType

        assert enum_value(MemoryType.SEMANTIC) == "semantic"

    def test_enum_value_none(self):
        from memoria.core.memory.types import enum_value

        assert enum_value(None) == ""

    def test_enum_value_string(self):
        from memoria.core.memory.types import enum_value

        assert enum_value("raw_string") == "raw_string"

    def test_trust_tier_half_life(self):
        from memoria.core.memory.types import TrustTier

        assert TrustTier.T1_VERIFIED.default_half_life_days == 365.0
        assert TrustTier.T4_UNVERIFIED.default_half_life_days == 30.0

    def test_trust_tier_defaults(self):
        from memoria.core.memory.types import trust_tier_defaults

        d = trust_tier_defaults("T1")
        assert d["initial_confidence"] == 0.95
        assert d["half_life_days"] == 365.0

    def test_trust_tier_defaults_invalid_falls_back(self):
        from memoria.core.memory.types import trust_tier_defaults

        d = trust_tier_defaults("INVALID")
        # Falls back to T3_INFERRED
        assert d["initial_confidence"] == 0.65

    def test_memory_type_values(self):
        from memoria.core.memory.types import MemoryType

        assert MemoryType.EPISODIC.value == "episodic"
        assert MemoryType.PROFILE.value == "profile"

    def test_retrieval_weights_validation(self):
        from memoria.core.memory.types import RetrievalWeights

        w = RetrievalWeights()
        assert w.vector + w.keyword + w.temporal + w.confidence == pytest.approx(1.0)

    def test_retrieval_weights_sum_invalid(self):
        from memoria.core.memory.types import RetrievalWeights

        w = RetrievalWeights()
        w.vector = 0.9  # break the sum
        with pytest.raises(ValueError):
            w.__post_init__()

    def test_episodic_metadata_fields(self):
        from memoria.core.memory.types import EpisodicMetadata

        m = EpisodicMetadata(
            topic="test topic",
            action="did something",
            outcome="it worked",
            source_event_ids=["id1", "id2"],
            session_id="sess_abc",
        )
        assert m.session_id == "sess_abc"
        assert m.source_event_ids == ["id1", "id2"]
        d = m.model_dump()
        assert d["session_id"] == "sess_abc"
