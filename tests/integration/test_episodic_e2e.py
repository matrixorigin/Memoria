"""End-to-end test for episodic memory: real user scenario."""

import json

import pytest
from sqlalchemy import text

from memoria.core.memory.episodic import generate_episodic_memory
from memoria.core.memory.tabular.service import MemoryService
from memoria.core.memory.types import MemoryType


class MockLLMClient:
    """Mock LLM that generates realistic episodic summaries."""

    def chat(self, messages: list, **kwargs):
        # Extract key info from messages
        content = " ".join(m.get("content", "") for m in messages)

        if "database" in content.lower() and "index" in content.lower():
            return json.dumps(
                {
                    "topic": "Database performance optimization",
                    "action": "Analyzed slow queries using EXPLAIN, identified missing indexes, added composite index on (user_id, created_at)",
                    "outcome": "Query execution time reduced from 2.5s to 180ms (93% improvement), eliminated full table scans",
                }
            )
        elif "bug" in content.lower() and "fix" in content.lower():
            return json.dumps(
                {
                    "topic": "Authentication bug investigation and fix",
                    "action": "Reproduced issue in dev environment, traced root cause to expired JWT tokens not being refreshed, implemented token refresh logic",
                    "outcome": "Bug fixed and deployed to production, added integration test to prevent regression",
                }
            )
        else:
            return json.dumps(
                {
                    "topic": "General development session",
                    "action": "Worked on various tasks",
                    "outcome": "Made progress on project",
                }
            )


@pytest.mark.integration
def test_real_user_workflow_database_optimization(db_factory):
    """Test complete workflow: user optimizes database, generates episodic memory, retrieves it later."""
    service = MemoryService(db_factory)
    user_id = "alice"
    session_id = "sess_db_opt_2026_03_15"

    # Cleanup
    with db_factory() as db:
        db.execute(
            text("DELETE FROM mem_memories WHERE user_id = :user_id"),
            {"user_id": user_id},
        )
        db.commit()

    # === Phase 1: User works on database optimization ===

    # User starts session
    service.store(
        user_id=user_id,
        content="User wants to optimize slow database queries in production",
        memory_type=MemoryType.SEMANTIC,
        session_id=session_id,
    )

    # User analyzes queries
    service.store(
        user_id=user_id,
        content="Ran EXPLAIN on slow queries, found full table scan on users table",
        memory_type=MemoryType.PROCEDURAL,
        session_id=session_id,
    )

    # User adds index
    service.store(
        user_id=user_id,
        content="Added composite index: CREATE INDEX idx_users_lookup ON users(user_id, created_at)",
        memory_type=MemoryType.PROCEDURAL,
        session_id=session_id,
    )

    # User verifies improvement
    service.store(
        user_id=user_id,
        content="Query time reduced from 2.5s to 180ms after adding index",
        memory_type=MemoryType.SEMANTIC,
        session_id=session_id,
    )

    # === Phase 2: Generate episodic summary ===

    from memoria.core.memory.episodic import generate_episodic_memory

    # Get session memories
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

    assert len(memories) == 4

    # Generate episodic summary
    messages = [
        {"id": m.memory_id, "role": "user", "content": f"[{m.memory_type}] {m.content}"}
        for m in memories
    ]

    llm_client = MockLLMClient()
    metadata, truncated = generate_episodic_memory(messages, llm_client)

    assert not truncated
    assert "Database performance optimization" in metadata.topic
    assert "index" in metadata.action.lower()
    assert "180ms" in metadata.outcome or "93%" in metadata.outcome

    # Store episodic memory (cross-session for future retrieval)
    episodic_content = f"Session Summary: {metadata.topic}\n\nActions: {metadata.action}\n\nOutcome: {metadata.outcome}"
    episodic_id = service.store(
        user_id=user_id,
        content=episodic_content,
        memory_type=MemoryType.EPISODIC,
        session_id=None,  # Cross-session
        extra_metadata=metadata.model_dump(),
    )

    # === Phase 3: Later, user retrieves relevant memories ===

    # User starts a new session and asks about database optimization
    memories, _ = service.retrieve(
        user_id=user_id,
        query="how did I optimize database queries before",
        session_id="",  # New session
        top_k=5,
    )

    # Should retrieve the episodic memory
    episodic_memories = [m for m in memories if m.memory_type == MemoryType.EPISODIC]
    assert len(episodic_memories) > 0, (
        "Should retrieve episodic memory from previous session"
    )

    episodic = episodic_memories[0]
    assert episodic.memory_id == episodic_id
    assert "Database performance optimization" in episodic.content
    assert episodic.retrieval_score > 0

    # Verify metadata is preserved
    with db_factory() as db:
        result = db.execute(
            text("SELECT extra_metadata FROM mem_memories WHERE memory_id = :id"),
            {"id": episodic_id},
        )
        row = result.fetchone()
        meta = (
            json.loads(row.extra_metadata)
            if isinstance(row.extra_metadata, str)
            else row.extra_metadata
        )
        assert meta["topic"] == metadata.topic
        assert meta["action"] == metadata.action
        assert meta["outcome"] == metadata.outcome


@pytest.mark.integration
def test_multiple_sessions_episodic_memories(db_factory):
    """Test user with multiple sessions, each generating episodic memory."""
    service = MemoryService(db_factory)
    user_id = "bob"

    # Cleanup
    with db_factory() as db:
        db.execute(
            text("DELETE FROM mem_memories WHERE user_id = :user_id"),
            {"user_id": user_id},
        )
        db.commit()

    llm_client = MockLLMClient()

    # === Session 1: Database optimization ===
    session1 = "sess_db_2026_03_15_morning"
    service.store(
        user_id=user_id,
        content="Optimized database queries",
        memory_type=MemoryType.SEMANTIC,
        session_id=session1,
    )
    service.store(
        user_id=user_id,
        content="Added index on user_id column",
        memory_type=MemoryType.PROCEDURAL,
        session_id=session1,
    )

    with db_factory() as db:
        result = db.execute(
            text(
                "SELECT memory_id, content FROM mem_memories WHERE user_id = :user_id AND session_id = :session_id ORDER BY created_at"
            ),
            {"user_id": user_id, "session_id": session1},
        )
        msgs1 = [
            {"id": r.memory_id, "role": "user", "content": r.content}
            for r in result.fetchall()
        ]

    meta1, _ = generate_episodic_memory(msgs1, llm_client)
    service.store(
        user_id=user_id,
        content=f"Session Summary: {meta1.topic}\n\nActions: {meta1.action}\n\nOutcome: {meta1.outcome}",
        memory_type=MemoryType.EPISODIC,
        session_id=None,
        extra_metadata=meta1.model_dump(),
    )

    # === Session 2: Bug fix ===
    session2 = "sess_bugfix_2026_03_15_afternoon"
    service.store(
        user_id=user_id,
        content="Investigating authentication bug",
        memory_type=MemoryType.SEMANTIC,
        session_id=session2,
    )
    service.store(
        user_id=user_id,
        content="Fixed JWT token refresh issue",
        memory_type=MemoryType.PROCEDURAL,
        session_id=session2,
    )

    with db_factory() as db:
        result = db.execute(
            text(
                "SELECT memory_id, content FROM mem_memories WHERE user_id = :user_id AND session_id = :session_id ORDER BY created_at"
            ),
            {"user_id": user_id, "session_id": session2},
        )
        msgs2 = [
            {"id": r.memory_id, "role": "user", "content": r.content}
            for r in result.fetchall()
        ]

    meta2, _ = generate_episodic_memory(msgs2, llm_client)
    service.store(
        user_id=user_id,
        content=f"Session Summary: {meta2.topic}\n\nActions: {meta2.action}\n\nOutcome: {meta2.outcome}",
        memory_type=MemoryType.EPISODIC,
        session_id=None,
        extra_metadata=meta2.model_dump(),
    )

    # === Retrieve all episodic memories ===
    memories, _ = service.retrieve(
        user_id=user_id,
        query="what did I work on recently",
        session_id="",
        top_k=10,
    )

    episodic_memories = [m for m in memories if m.memory_type == MemoryType.EPISODIC]
    assert len(episodic_memories) == 2, (
        "Should have 2 episodic memories from 2 sessions"
    )

    # Verify both topics are present
    topics = [m.content for m in episodic_memories]
    assert any("database" in t.lower() for t in topics)
    assert any("bug" in t.lower() or "authentication" in t.lower() for t in topics)
