"""Test that activation strategy sets explain path correctly."""

from datetime import datetime, timezone
from unittest.mock import MagicMock

import pytest

from memoria.core.explain import (
    ExplainLevel,
    clear_explain,
    get_explain_ctx,
    init_explain,
)
from memoria.core.memory.graph.types import GraphNodeData, NodeType
from memoria.core.memory.strategy.activation_v1 import ActivationRetrievalStrategy
from memoria.core.memory.types import Memory, MemoryType


@pytest.fixture(autouse=True)
def reset_explain():
    """Reset explain context before each test."""
    clear_explain()
    yield
    clear_explain()


def test_activation_sets_path_graph():
    """Pure graph retrieval sets path='graph'."""
    db_factory = MagicMock()
    strategy = ActivationRetrievalStrategy(db_factory)

    # Mock 10 graph results
    nodes = [
        (
            GraphNodeData(
                node_id=f"n{i}",
                user_id="u1",
                node_type=NodeType.SEMANTIC,
                content=f"test {i}",
                confidence=0.8,
                memory_id=f"m{i}",
                session_id="s1",
                trust_tier="T3",
            ),
            0.9 - i * 0.01,
        )
        for i in range(10)
    ]
    strategy._activation_retriever = MagicMock()
    strategy._activation_retriever.retrieve.return_value = nodes

    memories = {
        f"m{i}": Memory(
            memory_id=f"m{i}",
            user_id="u1",
            memory_type=MemoryType.SEMANTIC,
            content=f"test {i}",
            initial_confidence=0.8,
            observed_at=datetime.now(timezone.utc),
            session_id="s1",
        )
        for i in range(10)
    }
    strategy._mem_store = MagicMock()
    strategy._mem_store.get_by_ids.return_value = memories

    init_explain(ExplainLevel.BASIC)
    results, explain_info = strategy.retrieve(
        user_id="u1", query="test", query_embedding=[0.1] * 10, top_k=5, explain=True
    )

    ctx = get_explain_ctx()
    assert ctx is not None
    assert ctx.path == "graph"
    # explain_info is None now — all data in context
    assert explain_info is None


def test_activation_sets_path_hybrid():
    """Graph + vector merge sets path='graph+vector'."""
    db_factory = MagicMock()
    strategy = ActivationRetrievalStrategy(db_factory)

    # Mock 2 graph results (< top_k=5)
    nodes = [
        (
            GraphNodeData(
                node_id=f"n{i}",
                user_id="u1",
                node_type=NodeType.SEMANTIC,
                content=f"graph {i}",
                confidence=0.8,
                memory_id=f"m{i}",
                session_id="s1",
                trust_tier="T3",
            ),
            0.9,
        )
        for i in range(2)
    ]
    strategy._activation_retriever = MagicMock()
    strategy._activation_retriever.retrieve.return_value = nodes

    graph_memories = {
        f"m{i}": Memory(
            memory_id=f"m{i}",
            user_id="u1",
            memory_type=MemoryType.SEMANTIC,
            content=f"graph {i}",
            initial_confidence=0.8,
            observed_at=datetime.now(timezone.utc),
            session_id="s1",
        )
        for i in range(2)
    }
    strategy._mem_store = MagicMock()
    strategy._mem_store.get_by_ids.return_value = graph_memories

    # Mock vector fallback
    vec_memories = [
        Memory(
            memory_id=f"v{i}",
            user_id="u1",
            memory_type=MemoryType.SEMANTIC,
            content=f"vector {i}",
            initial_confidence=0.7,
            observed_at=datetime.now(timezone.utc),
            retrieval_score=0.8,
        )
        for i in range(3)
    ]
    mock_vector = MagicMock()
    mock_vector.retrieve.return_value = (vec_memories, None)
    strategy._vector_fallback_strategy = mock_vector

    init_explain(ExplainLevel.BASIC)
    results, explain_info = strategy.retrieve(
        user_id="u1", query="test", query_embedding=[0.1] * 10, top_k=5, explain=True
    )

    ctx = get_explain_ctx()
    assert ctx is not None
    assert ctx.path == "graph+vector"
    # explain_info is None now — all data in context
    assert explain_info is None


def test_activation_sets_path_fallback():
    """No graph results sets path='vector_fallback'."""
    db_factory = MagicMock()
    strategy = ActivationRetrievalStrategy(db_factory)

    # Mock no graph results
    strategy._activation_retriever = MagicMock()
    strategy._activation_retriever.retrieve.return_value = []
    strategy._mem_store = MagicMock()
    strategy._mem_store.get_by_ids.return_value = {}

    # Mock vector fallback
    vec_memories = [
        Memory(
            memory_id=f"v{i}",
            user_id="u1",
            memory_type=MemoryType.SEMANTIC,
            content=f"vector {i}",
            initial_confidence=0.7,
            observed_at=datetime.now(timezone.utc),
            retrieval_score=0.8,
        )
        for i in range(5)
    ]
    mock_vector = MagicMock()
    mock_vector.retrieve.return_value = (vec_memories, None)
    strategy._vector_fallback_strategy = mock_vector

    init_explain(ExplainLevel.BASIC)
    results, explain_info = strategy.retrieve(
        user_id="u1", query="test", query_embedding=[0.1] * 10, top_k=5, explain=True
    )

    ctx = get_explain_ctx()
    assert ctx is not None
    assert ctx.path == "vector_fallback"
    # explain_info is None now — all data in context
    assert explain_info is None
