"""Test episodic memory retrieval integration."""

import pytest

from memoria.core.memory.tabular.service import MemoryService
from memoria.core.memory.types import MemoryType


@pytest.mark.integration
def test_episodic_memory_retrieval(db_factory):
    """Test that episodic memories are included in retrieval results."""
    from sqlalchemy import text

    service = MemoryService(db_factory)
    user_id = "test_retrieval_user"
    session_id = "sess_optimization"

    # Cleanup
    with db_factory() as db:
        db.execute(
            text("DELETE FROM mem_memories WHERE user_id = :user_id"),
            {"user_id": user_id},
        )
        db.commit()

    # Store some regular memories
    service.store(
        user_id=user_id,
        content="User wants to optimize database queries",
        memory_type=MemoryType.SEMANTIC,
        session_id=session_id,
    )
    service.store(
        user_id=user_id,
        content="Added indexes on user_id column",
        memory_type=MemoryType.PROCEDURAL,
        session_id=session_id,
    )

    # Store an episodic memory (cross-session, no session_id)
    episodic_metadata = {
        "topic": "Database optimization session",
        "action": "Analyzed slow queries and added indexes",
        "outcome": "Query time reduced from 2s to 200ms",
        "source_event_ids": [],
    }
    service.store(
        user_id=user_id,
        content="Session Summary: Database optimization session\n\nActions: Analyzed slow queries and added indexes\n\nOutcome: Query time reduced from 2s to 200ms",
        memory_type=MemoryType.EPISODIC,
        session_id=None,  # Cross-session memory
        extra_metadata=episodic_metadata,
    )

    # Retrieve with a query related to optimization (no session_id = all sessions)
    memories, _ = service.retrieve(
        user_id=user_id,
        query="database optimization",
        session_id="",  # Empty string = retrieve from all sessions
        top_k=10,
    )

    # Verify episodic memory is included
    episodic_memories = [m for m in memories if m.memory_type == MemoryType.EPISODIC]
    assert len(episodic_memories) > 0, "Episodic memory should be retrieved"

    episodic = episodic_memories[0]
    assert "Session Summary" in episodic.content
    assert episodic.retrieval_score is not None
    assert episodic.retrieval_score > 0

    # Verify all memory types are present
    memory_types = {m.memory_type for m in memories}
    assert MemoryType.EPISODIC in memory_types
    assert MemoryType.SEMANTIC in memory_types or MemoryType.PROCEDURAL in memory_types


@pytest.mark.integration
def test_episodic_memory_scoring(db_factory):
    """Test that episodic memories are scored appropriately."""
    from sqlalchemy import text

    service = MemoryService(db_factory)
    user_id = "test_scoring_user"

    # Cleanup
    with db_factory() as db:
        db.execute(
            text("DELETE FROM mem_memories WHERE user_id = :user_id"),
            {"user_id": user_id},
        )
        db.commit()

    # Store episodic memory with high relevance
    service.store(
        user_id=user_id,
        content="Session Summary: Python performance optimization\n\nActions: Profiled code, optimized hot loops\n\nOutcome: 10x speedup achieved",
        memory_type=MemoryType.EPISODIC,
        extra_metadata={
            "topic": "Python performance optimization",
            "action": "Profiled code, optimized hot loops",
            "outcome": "10x speedup achieved",
            "source_event_ids": [],
        },
    )

    # Store a less relevant semantic memory
    service.store(
        user_id=user_id,
        content="User prefers Python for scripting",
        memory_type=MemoryType.SEMANTIC,
    )

    # Retrieve with performance-related query
    memories, _ = service.retrieve(
        user_id=user_id,
        query="python performance optimization",
        session_id="test_session",
        top_k=5,
    )

    # Episodic memory should be highly ranked due to content match
    assert len(memories) > 0
    episodic_memories = [m for m in memories if m.memory_type == MemoryType.EPISODIC]
    assert len(episodic_memories) > 0

    # Check that episodic memory has a reasonable score
    episodic = episodic_memories[0]
    assert episodic.retrieval_score > 0.1, (
        "Episodic memory should have meaningful score"
    )
