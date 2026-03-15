"""Unit tests for episodic memory generator."""

import json

import pytest

from memoria.core.memory.episodic import (
    generate_episodic_memory,
    generate_lightweight_summary,
)
from memoria.core.memory.episodic.tasks import TaskManager
from memoria.core.memory.types import EpisodicMetadata


class MockLLMClient:
    """Mock LLM client for testing."""

    def __init__(self, response: dict):
        self.response = response
        self.called_with = None

    def chat(self, messages: list, **kwargs):
        self.called_with = messages
        return json.dumps(self.response)


def test_generate_episodic_memory_basic():
    """Test basic episodic memory generation."""
    messages = [
        {
            "id": "msg1",
            "role": "user",
            "content": "I need to optimize the database queries",
        },
        {
            "id": "msg2",
            "role": "assistant",
            "content": "Let's analyze the slow queries first",
        },
        {
            "id": "msg3",
            "role": "user",
            "content": "Added indexes on user_id and created_at columns",
        },
        {
            "id": "msg4",
            "role": "assistant",
            "content": "Great! Query time reduced from 2s to 200ms",
        },
    ]

    mock_llm = MockLLMClient(
        {
            "topic": "Database query optimization",
            "action": "Analyzed slow queries and added indexes on user_id and created_at columns",
            "outcome": "Query execution time reduced from 2 seconds to 200 milliseconds",
        }
    )

    metadata, truncated = generate_episodic_memory(messages, mock_llm)

    assert isinstance(metadata, EpisodicMetadata)
    assert metadata.topic == "Database query optimization"
    assert "indexes" in metadata.action
    assert "200 milliseconds" in metadata.outcome
    assert metadata.source_event_ids == ["msg1", "msg2", "msg3", "msg4"]
    assert not truncated


def test_generate_episodic_memory_truncation():
    """Test message truncation when exceeding limits."""
    messages = [
        {"id": f"msg{i}", "role": "user", "content": f"Message {i}"} for i in range(300)
    ]

    mock_llm = MockLLMClient(
        {
            "topic": "Long conversation",
            "action": "Discussed many topics",
            "outcome": "Reached conclusions",
        }
    )

    metadata, truncated = generate_episodic_memory(messages, mock_llm, max_messages=200)

    assert truncated
    assert len(metadata.source_event_ids) == 200  # Only last 200 messages


def test_generate_episodic_memory_empty_messages():
    """Test error handling for empty message list."""
    mock_llm = MockLLMClient({})

    with pytest.raises(ValueError, match="empty message list"):
        generate_episodic_memory([], mock_llm)


def test_generate_episodic_memory_invalid_llm_response():
    """Test error handling for invalid LLM response."""
    messages = [{"id": "msg1", "role": "user", "content": "Test"}]
    mock_llm = MockLLMClient({"invalid": "response"})  # Missing required fields

    with pytest.raises(ValueError, match="invalid JSON"):
        generate_episodic_memory(messages, mock_llm)


# ── Lightweight mode tests ──────────────────────────────────────────────────


def test_generate_lightweight_summary_basic():
    """Test basic lightweight summary generation."""
    messages = [
        {"role": "user", "content": "Let's refactor the auth module"},
        {"role": "assistant", "content": "Extracted JWT logic into separate class"},
        {"role": "user", "content": "Added unit tests for the new class"},
    ]
    mock_llm = MockLLMClient(
        {
            "points": [
                "Refactored auth module",
                "Extracted JWT logic",
                "Added unit tests",
            ]
        }
    )

    points, truncated = generate_lightweight_summary(messages, mock_llm)

    assert points == [
        "Refactored auth module",
        "Extracted JWT logic",
        "Added unit tests",
    ]
    assert not truncated


def test_generate_lightweight_summary_markdown_json():
    """Test that markdown code fences are stripped from LLM response."""
    messages = [{"role": "user", "content": "test"}]

    class MarkdownLLM:
        def chat(self, messages, **kwargs):
            return '```json\n{"points": ["point a", "point b"]}\n```'

    points, _ = generate_lightweight_summary(messages, MarkdownLLM())
    assert points == ["point a", "point b"]


def test_generate_lightweight_summary_empty_messages():
    """Test error on empty messages."""
    mock_llm = MockLLMClient({})
    with pytest.raises(ValueError, match="empty message list"):
        generate_lightweight_summary([], mock_llm)


def test_generate_lightweight_summary_invalid_response():
    """Test error on invalid LLM response."""
    messages = [{"role": "user", "content": "test"}]
    mock_llm = MockLLMClient({"no_points": "here"})
    with pytest.raises(ValueError, match="lightweight mode"):
        generate_lightweight_summary(messages, mock_llm)


# ── Rate limit tests ────────────────────────────────────────────────────────


def test_rate_limit_allows_up_to_max():
    """Test that lightweight rate limit allows up to max_lightweight_per_session."""
    tm = TaskManager(max_lightweight_per_session=3)
    session = "sess_test"

    assert tm.check_lightweight_rate_limit(session) is True
    tm.increment_lightweight_count(session)
    assert tm.check_lightweight_rate_limit(session) is True
    tm.increment_lightweight_count(session)
    assert tm.check_lightweight_rate_limit(session) is True
    tm.increment_lightweight_count(session)
    # Now at limit
    assert tm.check_lightweight_rate_limit(session) is False


def test_rate_limit_independent_per_session():
    """Test that rate limits are tracked independently per session."""
    tm = TaskManager(max_lightweight_per_session=1)
    tm.increment_lightweight_count("sess_a")

    assert tm.check_lightweight_rate_limit("sess_a") is False
    assert tm.check_lightweight_rate_limit("sess_b") is True
