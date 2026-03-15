"""Integration test for episodic memory generation."""

import json

import pytest

from memoria.core.memory.episodic import generate_episodic_memory
from memoria.core.memory.tabular.service import MemoryService
from memoria.core.memory.types import MemoryType


class MockLLMClient:
    """Mock LLM for testing."""

    def chat(self, messages: list, **kwargs):
        return json.dumps(
            {
                "topic": "Database optimization session",
                "action": "Analyzed slow queries, added indexes on user_id and created_at columns",
                "outcome": "Query execution time reduced from 2 seconds to 200 milliseconds",
            }
        )


@pytest.mark.integration
def test_episodic_memory_end_to_end(db_factory):
    """Test complete episodic memory flow: store memories → generate summary → retrieve."""
    from sqlalchemy import text

    service = MemoryService(db_factory)
    user_id = "test_episodic_user"
    session_id = "sess_db_optimization"

    # Cleanup: delete any existing test data
    with db_factory() as db:
        db.execute(
            text("DELETE FROM mem_memories WHERE user_id = :user_id"),
            {"user_id": user_id},
        )
        db.commit()

    # Step 1: Store some memories in a session
    service.store(
        user_id=user_id,
        content="User wants to optimize database queries",
        memory_type=MemoryType.SEMANTIC,
        session_id=session_id,
    )
    service.store(
        user_id=user_id,
        content="Analyzed slow queries using EXPLAIN",
        memory_type=MemoryType.PROCEDURAL,
        session_id=session_id,
    )
    service.store(
        user_id=user_id,
        content="Added indexes on user_id and created_at columns",
        memory_type=MemoryType.PROCEDURAL,
        session_id=session_id,
    )

    # Step 2: Retrieve session memories
    with db_factory() as db:
        result = db.execute(
            text(
                "SELECT memory_id, content, memory_type FROM mem_memories "
                "WHERE user_id = :user_id AND session_id = :session_id AND is_active = 1 "
                "ORDER BY created_at ASC"
            ),
            {"user_id": user_id, "session_id": session_id},
        )
        memories = result.fetchall()

    assert len(memories) == 3

    # Step 3: Generate episodic summary
    messages = [
        {
            "id": mem.memory_id,
            "role": "user",
            "content": f"[{mem.memory_type}] {mem.content}",
        }
        for mem in memories
    ]

    llm_client = MockLLMClient()
    metadata, truncated = generate_episodic_memory(messages, llm_client)

    assert not truncated
    assert "Database optimization" in metadata.topic
    assert "indexes" in metadata.action
    assert "200 milliseconds" in metadata.outcome
    assert len(metadata.source_event_ids) == 3

    # Step 4: Store episodic memory (cross-session for future retrieval)
    content = f"Session Summary: {metadata.topic}\n\nActions: {metadata.action}\n\nOutcome: {metadata.outcome}"
    episodic_id = service.store(
        user_id=user_id,
        content=content,
        memory_type=MemoryType.EPISODIC,
        session_id=None,  # Cross-session
        extra_metadata=metadata.model_dump(),
    )

    # Step 5: Verify episodic memory was stored with metadata
    import json

    with db_factory() as db:
        result = db.execute(
            text(
                "SELECT content, extra_metadata FROM mem_memories WHERE memory_id = :id"
            ),
            {"id": episodic_id},
        )
        row = result.fetchone()

    assert row is not None
    assert "Session Summary" in row.content
    assert row.extra_metadata is not None

    # Parse JSON if it's a string
    meta = (
        json.loads(row.extra_metadata)
        if isinstance(row.extra_metadata, str)
        else row.extra_metadata
    )
    assert meta["topic"] == metadata.topic
    assert meta["action"] == metadata.action
    assert meta["outcome"] == metadata.outcome
