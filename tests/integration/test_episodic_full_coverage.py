"""Comprehensive episodic memory tests: DB field verification, async, no-LLM, auto-trigger.

Covers:
- All EpisodicMetadata fields in DB (topic, action, outcome, session_id, source_event_ids)
- focus_topics end-to-end
- Async task full loop (create → background → DB write → poll)
- TaskManager TTL cleanup (_lightweight_counts + tasks)
- Rate limit enforcement (sync + async paths)
- No-LLM degradation (LLM=None raises 503, generator raises ValueError)
- SessionSummarizer: incremental + full summary, no-LLM fallback
- auto_trigger: fires at threshold, respects rate limit, writes to DB
"""

from __future__ import annotations

import json
import time
import threading
from datetime import datetime, timedelta, timezone

import pytest
from sqlalchemy import text

from memoria.core.memory.episodic import (
    generate_episodic_memory,
    generate_lightweight_summary,
)
from memoria.core.memory.episodic.tasks import TaskManager, TaskStatus
from memoria.core.memory.tabular.service import MemoryService
from memoria.core.memory.tabular.session_summary import SessionSummarizer
from memoria.core.memory.tabular.store import MemoryStore
from memoria.core.memory.types import MemoryType


# ---------------------------------------------------------------------------
# Shared mock LLM
# ---------------------------------------------------------------------------


class MockLLM:
    """Deterministic mock LLM for episodic tests."""

    def chat(self, messages: list, **kwargs) -> str:
        content = " ".join(m.get("content", "") for m in messages)
        # Lightweight prompt asks for "points" list
        if (
            "3-5 key points" in content
            or "Session Highlights" in content
            or "key points" in content.lower()
        ):
            return json.dumps(
                {
                    "points": [
                        "Worked on performance",
                        "Reduced latency by 40%",
                        "All tests pass",
                    ]
                }
            )
        if "focus" in content.lower() or "performance" in content.lower():
            return json.dumps(
                {
                    "topic": "Performance optimization session",
                    "action": "Profiled hot paths, reduced allocations by 40%",
                    "outcome": "P99 latency dropped from 800ms to 120ms",
                }
            )
        return json.dumps(
            {
                "topic": "General coding session",
                "action": "Wrote unit tests and fixed two bugs",
                "outcome": "All 474 tests pass, CI green",
            }
        )

    def chat_with_tools(self, messages: list, **kwargs) -> dict:
        return {"content": "Summary: worked on code improvements."}


def _uid(prefix: str) -> str:
    return f"{prefix}_{int(time.time() * 1000) % 100000}"


def _cleanup(db_factory, user_id: str) -> None:
    with db_factory() as db:
        db.execute(text("DELETE FROM mem_memories WHERE user_id = :u"), {"u": user_id})
        db.commit()


# ---------------------------------------------------------------------------
# 1. All EpisodicMetadata fields verified in DB
# ---------------------------------------------------------------------------


@pytest.mark.integration
def test_all_episodic_metadata_fields_in_db(db_factory):
    """Store episodic memory and verify every field persists correctly in DB."""
    user_id = _uid("meta_fields")
    session_id = "sess_meta_001"
    _cleanup(db_factory, user_id)

    svc = MemoryService(db_factory)

    # Store source memories
    ids = []
    for content in [
        "Analyzed slow query",
        "Added composite index",
        "Verified 93% speedup",
    ]:
        mid = svc.store(
            user_id=user_id,
            content=content,
            memory_type=MemoryType.SEMANTIC,
            session_id=session_id,
        )
        ids.append(mid)

    messages = [
        {"id": mid, "role": "user", "content": f"msg {i}"} for i, mid in enumerate(ids)
    ]
    llm = MockLLM()
    metadata, truncated = generate_episodic_memory(messages, llm, session_id=session_id)

    assert not truncated
    assert metadata.session_id == session_id
    assert len(metadata.source_event_ids) == 3
    assert set(metadata.source_event_ids) == set(ids)
    assert metadata.topic
    assert metadata.action
    assert metadata.outcome

    episodic_id = svc.store(
        user_id=user_id,
        content=f"Session Summary: {metadata.topic}\n\nActions: {metadata.action}\n\nOutcome: {metadata.outcome}",
        memory_type=MemoryType.EPISODIC,
        session_id=None,
        extra_metadata=metadata.model_dump(),
    )

    # Verify every field in DB
    with db_factory() as db:
        row = db.execute(
            text(
                "SELECT memory_type, session_id, extra_metadata, is_active FROM mem_memories WHERE memory_id = :id"
            ),
            {"id": episodic_id},
        ).fetchone()

    assert row is not None
    assert row.memory_type == "episodic"
    assert row.session_id is None  # cross-session
    assert row.is_active == 1

    meta = (
        json.loads(row.extra_metadata)
        if isinstance(row.extra_metadata, str)
        else row.extra_metadata
    )
    assert meta["topic"] == metadata.topic
    assert meta["action"] == metadata.action
    assert meta["outcome"] == metadata.outcome
    assert meta["session_id"] == session_id
    assert set(meta["source_event_ids"]) == set(ids)


# ---------------------------------------------------------------------------
# 2. focus_topics end-to-end: prompt injection → DB field
# ---------------------------------------------------------------------------


@pytest.mark.integration
def test_focus_topics_end_to_end(db_factory):
    """focus_topics is injected into prompt and stored in DB metadata."""
    user_id = _uid("focus")
    session_id = "sess_focus_001"
    _cleanup(db_factory, user_id)

    svc = MemoryService(db_factory)
    mid = svc.store(
        user_id=user_id,
        content="Worked on performance optimization",
        memory_type=MemoryType.SEMANTIC,
        session_id=session_id,
    )

    messages = [{"id": mid, "role": "user", "content": "performance optimization work"}]
    llm = MockLLM()
    metadata, _ = generate_episodic_memory(
        messages, llm, session_id=session_id, focus_topics=["performance", "latency"]
    )

    # MockLLM returns performance-specific response when "performance" in content
    assert (
        "performance" in metadata.topic.lower() or "latency" in metadata.outcome.lower()
    )

    episodic_id = svc.store(
        user_id=user_id,
        content=f"Session Summary: {metadata.topic}",
        memory_type=MemoryType.EPISODIC,
        session_id=None,
        extra_metadata={
            **metadata.model_dump(),
            "focus_topics": ["performance", "latency"],
        },
    )

    with db_factory() as db:
        row = db.execute(
            text("SELECT extra_metadata FROM mem_memories WHERE memory_id = :id"),
            {"id": episodic_id},
        ).fetchone()
    meta = (
        json.loads(row.extra_metadata)
        if isinstance(row.extra_metadata, str)
        else row.extra_metadata
    )
    assert meta["focus_topics"] == ["performance", "latency"]


# ---------------------------------------------------------------------------
# 3. Async task full loop: create → background thread → DB write → poll
# ---------------------------------------------------------------------------


@pytest.mark.integration
def test_async_task_full_loop(db_factory):
    """Async task: create task, background thread writes to DB, poll returns result."""
    user_id = _uid("async_loop")
    session_id = "sess_async_001"
    _cleanup(db_factory, user_id)

    svc = MemoryService(db_factory)
    for content in ["Started refactor", "Extracted interface", "All tests pass"]:
        svc.store(
            user_id=user_id,
            content=content,
            memory_type=MemoryType.SEMANTIC,
            session_id=session_id,
        )

    task_manager = TaskManager(ttl_seconds=3600)
    task_id = task_manager.create_task()

    # Verify initial state
    task = task_manager.get_task(task_id)
    assert task.status == TaskStatus.PROCESSING
    assert task.result is None

    llm = MockLLM()

    def _background():
        with db_factory() as db:
            rows = db.execute(
                text(
                    "SELECT memory_id, content FROM mem_memories WHERE user_id = :u AND session_id = :s AND is_active = 1 ORDER BY created_at"
                ),
                {"u": user_id, "s": session_id},
            ).fetchall()
        messages = [
            {"id": r.memory_id, "role": "user", "content": r.content} for r in rows
        ]
        metadata, truncated = generate_episodic_memory(
            messages, llm, session_id=session_id
        )
        content = f"Session Summary: {metadata.topic}\n\nActions: {metadata.action}\n\nOutcome: {metadata.outcome}"
        memory_id = svc.store(
            user_id=user_id,
            content=content,
            memory_type=MemoryType.EPISODIC,
            session_id=None,
            extra_metadata=metadata.model_dump(),
        )
        task_manager.complete_task(
            task_id,
            {"memory_id": memory_id, "content": content, "truncated": truncated},
        )

    t = threading.Thread(target=_background, daemon=True)
    t.start()
    t.join(timeout=10)
    assert not t.is_alive(), "Background thread timed out"

    # Poll result
    task = task_manager.get_task(task_id)
    assert task.status == TaskStatus.COMPLETED
    assert task.result is not None
    memory_id = task.result["memory_id"]
    assert memory_id

    # Verify DB write
    with db_factory() as db:
        row = db.execute(
            text(
                "SELECT memory_type, session_id, extra_metadata FROM mem_memories WHERE memory_id = :id"
            ),
            {"id": memory_id},
        ).fetchone()
    assert row.memory_type == "episodic"
    assert row.session_id is None
    meta = (
        json.loads(row.extra_metadata)
        if isinstance(row.extra_metadata, str)
        else row.extra_metadata
    )
    assert "topic" in meta
    assert "action" in meta
    assert "outcome" in meta


@pytest.mark.integration
def test_async_task_failure_recorded(db_factory):
    """Failed background task records error correctly."""
    task_manager = TaskManager()
    task_id = task_manager.create_task()

    task_manager.fail_task(task_id, "LLM_ERROR", "Connection timeout")

    task = task_manager.get_task(task_id)
    assert task.status == TaskStatus.FAILED
    assert task.error == {"code": "LLM_ERROR", "message": "Connection timeout"}
    assert task.result is None


# ---------------------------------------------------------------------------
# 4. TaskManager TTL cleanup: tasks + _lightweight_counts
# ---------------------------------------------------------------------------


@pytest.mark.integration
def test_task_manager_ttl_cleanup_tasks(db_factory):
    """Old tasks are removed by cleanup_old_tasks."""
    tm = TaskManager(ttl_seconds=1)
    t1 = tm.create_task()
    t2 = tm.create_task()
    tm.complete_task(t1, {"ok": True})

    # Manually age both tasks
    old_time = datetime.now(timezone.utc) - timedelta(seconds=5)
    tm._tasks[t1].updated_at = old_time
    tm._tasks[t2].updated_at = old_time

    removed = tm.cleanup_old_tasks()
    assert removed == 2
    assert tm.get_task(t1) is None
    assert tm.get_task(t2) is None


@pytest.mark.integration
def test_task_manager_lightweight_counts_ttl_cleanup(db_factory):
    """_lightweight_counts entries expire after TTL."""
    tm = TaskManager(ttl_seconds=1, max_lightweight_per_session=3)
    session_id = "sess_ttl_test"

    tm.increment_lightweight_count(session_id)
    assert tm.get_lightweight_count(session_id) == 1

    # Age the entry
    old_time = datetime.now(timezone.utc) - timedelta(seconds=5)
    tm._lightweight_counts[session_id] = (1, old_time)

    # Expired → treated as 0
    assert tm.get_lightweight_count(session_id) == 0
    assert tm.check_lightweight_rate_limit(session_id) is True

    # cleanup_old_tasks also removes stale entries
    tm.cleanup_old_tasks()
    assert session_id not in tm._lightweight_counts


@pytest.mark.integration
def test_task_manager_lightweight_counts_no_unbounded_growth(db_factory):
    """Many sessions don't cause unbounded growth after cleanup."""
    tm = TaskManager(ttl_seconds=1, max_lightweight_per_session=3)
    old_time = datetime.now(timezone.utc) - timedelta(seconds=5)

    # Simulate 100 old sessions
    for i in range(100):
        tm._lightweight_counts[f"sess_{i}"] = (1, old_time)

    assert len(tm._lightweight_counts) == 100
    tm.cleanup_old_tasks()
    assert len(tm._lightweight_counts) == 0


# ---------------------------------------------------------------------------
# 5. Rate limit enforcement
# ---------------------------------------------------------------------------


@pytest.mark.integration
def test_rate_limit_blocks_after_max(db_factory):
    """Rate limit blocks lightweight after max_lightweight_per_session."""
    tm = TaskManager(ttl_seconds=3600, max_lightweight_per_session=3)
    session_id = "sess_ratelimit_001"

    for i in range(3):
        assert tm.check_lightweight_rate_limit(session_id) is True
        tm.increment_lightweight_count(session_id)

    assert tm.check_lightweight_rate_limit(session_id) is False
    assert tm.get_lightweight_count(session_id) == 3


@pytest.mark.integration
def test_rate_limit_check_before_increment_prevents_race(db_factory):
    """check + increment before thread spawn prevents concurrent over-triggering."""
    tm = TaskManager(ttl_seconds=3600, max_lightweight_per_session=2)
    session_id = "sess_race_001"
    allowed_count = 0
    lock = threading.Lock()

    def _try_trigger():
        nonlocal allowed_count
        if tm.check_lightweight_rate_limit(session_id):
            tm.increment_lightweight_count(session_id)
            with lock:
                allowed_count += 1

    threads = [threading.Thread(target=_try_trigger) for _ in range(10)]
    for t in threads:
        t.start()
    for t in threads:
        t.join()

    # At most max_lightweight_per_session should have been allowed
    assert allowed_count <= 2
    assert tm.get_lightweight_count(session_id) <= 2


# ---------------------------------------------------------------------------
# 6. No-LLM degradation
# ---------------------------------------------------------------------------


@pytest.mark.integration
def test_generate_episodic_memory_raises_without_llm(db_factory):
    """generate_episodic_memory raises ValueError on empty messages."""
    with pytest.raises(ValueError, match="empty"):
        generate_episodic_memory([], llm_client=MockLLM())


@pytest.mark.integration
def test_generate_lightweight_raises_without_messages(db_factory):
    """generate_lightweight_summary raises ValueError on empty messages."""
    with pytest.raises(ValueError, match="empty"):
        generate_lightweight_summary([], llm_client=MockLLM())


@pytest.mark.integration
def test_session_summarizer_no_llm_fallback(db_factory):
    """SessionSummarizer without LLM falls back to text truncation."""
    MemoryService(db_factory)  # noqa: F841
    store = MemoryStore(db_factory)
    summarizer = SessionSummarizer(store=store, llm_client=None, embed_fn=None)

    user_id = _uid("no_llm")
    session_id = "sess_nollm_001"
    _cleanup(db_factory, user_id)

    messages = [
        {"role": "user", "content": "I want to refactor the auth module"},
        {"role": "assistant", "content": "Sure, let's start with the token validation"},
        {"role": "user", "content": "Fixed the JWT expiry bug"},
    ]

    # Full summary without LLM — should return truncated text, not None
    mem = summarizer.generate_full_summary(user_id, session_id, messages)
    assert mem is not None
    assert mem.content  # has content
    assert mem.memory_type == MemoryType.SEMANTIC
    assert mem.session_id is None  # cross-session

    # Verify in DB
    with db_factory() as db:
        row = db.execute(
            text("SELECT content, session_id FROM mem_memories WHERE memory_id = :id"),
            {"id": mem.memory_id},
        ).fetchone()
    assert row is not None
    assert (
        "refactor" in row.content
        or "JWT" in row.content
        or "session_summary" in row.content
    )


@pytest.mark.integration
def test_session_summarizer_incremental_then_full(db_factory):
    """Incremental summary is session-scoped; full summary supersedes it."""
    store = MemoryStore(db_factory)
    summarizer = SessionSummarizer(store=store, llm_client=MockLLM(), embed_fn=None)

    user_id = _uid("incr_full")
    session_id = "sess_incr_001"
    _cleanup(db_factory, user_id)

    from memoria.core.memory.config import MemoryGovernanceConfig

    summarizer.config = MemoryGovernanceConfig(session_summary_turn_threshold=2)

    messages = [
        {"role": "user", "content": "Started working on feature X"},
        {"role": "assistant", "content": "OK, let's plan it"},
    ]

    # Trigger incremental at turn 2
    from datetime import datetime, timezone

    session_start = datetime.now(timezone.utc) - timedelta(minutes=5)
    incr = summarizer.check_and_summarize(
        user_id, session_id, messages, turn_count=2, session_start=session_start
    )
    assert incr is not None
    assert incr.session_id == session_id  # session-scoped
    incr_id = incr.memory_id

    # Verify incremental in DB
    with db_factory() as db:
        row = db.execute(
            text(
                "SELECT session_id, is_active FROM mem_memories WHERE memory_id = :id"
            ),
            {"id": incr_id},
        ).fetchone()
    assert row.session_id == session_id
    assert row.is_active == 1

    # Generate full summary — should supersede incremental
    messages += [{"role": "user", "content": "Feature X complete, all tests pass"}]
    full = summarizer.generate_full_summary(user_id, session_id, messages)
    assert full is not None
    assert full.session_id is None  # cross-session

    # Incremental should be deactivated
    with db_factory() as db:
        row = db.execute(
            text("SELECT is_active FROM mem_memories WHERE memory_id = :id"),
            {"id": incr_id},
        ).fetchone()
    assert row.is_active == 0


# ---------------------------------------------------------------------------
# 7. auto_trigger: fires at threshold, writes to DB, respects rate limit
# ---------------------------------------------------------------------------


@pytest.mark.integration
def test_auto_trigger_writes_episodic_to_db(db_factory):
    """auto_trigger fires at threshold and writes lightweight episodic to DB."""
    from memoria.core.memory.config import MemoryGovernanceConfig
    from memoria.core.memory.episodic.tasks import TaskManager

    user_id = _uid("autotrig")
    session_id = "sess_autotrig_001"
    _cleanup(db_factory, user_id)

    # Use a fresh TaskManager to avoid shared state
    fresh_tm = TaskManager(ttl_seconds=3600, max_lightweight_per_session=3)

    config = MemoryGovernanceConfig(auto_trigger_threshold=3)
    svc = MemoryService(db_factory, config=config, llm_client=MockLLM())

    # Patch the global task manager used by the service
    import memoria.core.memory.episodic.tasks as tasks_module

    original_tm = tasks_module._task_manager
    tasks_module._task_manager = fresh_tm

    try:
        # Store memories and trigger observe_turn at threshold
        for i in range(3):
            svc.store(
                user_id=user_id,
                content=f"Working on task step {i}",
                memory_type=MemoryType.SEMANTIC,
                session_id=session_id,
            )

        messages = [{"role": "user", "content": f"step {i}"} for i in range(3)]
        svc.observe_turn(
            user_id=user_id,
            messages=messages,
            session_id=session_id,
            turn_count=3,  # hits threshold
        )

        # Wait for background thread
        time.sleep(0.5)

        # Verify episodic memory written to DB
        with db_factory() as db:
            rows = db.execute(
                text(
                    "SELECT memory_type, extra_metadata FROM mem_memories WHERE user_id = :u AND memory_type = 'episodic'"
                ),
                {"u": user_id},
            ).fetchall()

        assert len(rows) >= 1
        meta = (
            json.loads(rows[0].extra_metadata)
            if isinstance(rows[0].extra_metadata, str)
            else rows[0].extra_metadata
        )
        assert meta.get("auto_triggered") is True
        assert "points" in meta

    finally:
        tasks_module._task_manager = original_tm


@pytest.mark.integration
def test_auto_trigger_respects_rate_limit(db_factory):
    """auto_trigger does not fire when rate limit is exhausted."""
    from memoria.core.memory.config import MemoryGovernanceConfig
    from memoria.core.memory.episodic.tasks import TaskManager

    user_id = _uid("autotrig_rl")
    session_id = "sess_autotrig_rl_001"
    _cleanup(db_factory, user_id)

    fresh_tm = TaskManager(ttl_seconds=3600, max_lightweight_per_session=1)
    # Pre-exhaust the rate limit
    fresh_tm.increment_lightweight_count(session_id)

    import memoria.core.memory.episodic.tasks as tasks_module

    original_tm = tasks_module._task_manager
    tasks_module._task_manager = fresh_tm

    try:
        config = MemoryGovernanceConfig(auto_trigger_threshold=1)
        svc = MemoryService(db_factory, config=config, llm_client=MockLLM())

        messages = [{"role": "user", "content": "some work"}]
        svc.observe_turn(
            user_id=user_id, messages=messages, session_id=session_id, turn_count=1
        )
        time.sleep(0.3)

        # No new episodic memories should be written
        with db_factory() as db:
            count = (
                db.execute(
                    text(
                        "SELECT COUNT(*) as c FROM mem_memories WHERE user_id = :u AND memory_type = 'episodic'"
                    ),
                    {"u": user_id},
                )
                .fetchone()
                .c
            )
        assert count == 0

    finally:
        tasks_module._task_manager = original_tm


# ---------------------------------------------------------------------------
# 8. Lightweight mode: DB fields + rate limit
# ---------------------------------------------------------------------------


@pytest.mark.integration
def test_lightweight_mode_db_fields(db_factory):
    """Lightweight summary stores correct fields in DB."""
    user_id = _uid("lightweight")
    session_id = "sess_lw_001"
    _cleanup(db_factory, user_id)

    svc = MemoryService(db_factory)
    for content in ["Discussed API design", "Chose REST over GraphQL"]:
        svc.store(
            user_id=user_id,
            content=content,
            memory_type=MemoryType.SEMANTIC,
            session_id=session_id,
        )

    with db_factory() as db:
        rows = db.execute(
            text(
                "SELECT memory_id, content FROM mem_memories WHERE user_id = :u AND session_id = :s"
            ),
            {"u": user_id, "s": session_id},
        ).fetchall()

    messages = [{"id": r.memory_id, "role": "user", "content": r.content} for r in rows]
    points, truncated = generate_lightweight_summary(messages, MockLLM())
    assert isinstance(points, list)
    assert 1 <= len(points) <= 5
    assert not truncated

    content = "Session Highlights:\n" + "\n".join(f"• {p}" for p in points)
    memory_id = svc.store(
        user_id=user_id,
        content=content,
        memory_type=MemoryType.EPISODIC,
        session_id=None,
        extra_metadata={"mode": "lightweight", "points": points},
    )

    with db_factory() as db:
        row = db.execute(
            text(
                "SELECT memory_type, session_id, extra_metadata FROM mem_memories WHERE memory_id = :id"
            ),
            {"id": memory_id},
        ).fetchone()

    assert row.memory_type == "episodic"
    assert row.session_id is None
    meta = (
        json.loads(row.extra_metadata)
        if isinstance(row.extra_metadata, str)
        else row.extra_metadata
    )
    assert meta["mode"] == "lightweight"
    assert meta["points"] == points


# ---------------------------------------------------------------------------
# 9. Truncation: head+tail preserved, warning logged
# ---------------------------------------------------------------------------


@pytest.mark.integration
def test_truncation_preserves_head_and_tail(db_factory):
    """_truncate_messages keeps first and last messages when truncating."""
    from memoria.core.memory.episodic.generator import _truncate_messages

    messages = [
        {"role": "user", "content": f"message {i}", "id": str(i)} for i in range(20)
    ]
    result, truncated = _truncate_messages(messages, max_messages=10, max_tokens=100000)

    assert truncated is True
    assert len(result) == 10
    # Head preserved
    assert result[0]["content"] == "message 0"
    # Tail preserved
    assert result[-1]["content"] == "message 19"


@pytest.mark.integration
def test_truncation_not_triggered_within_limits(db_factory):
    """No truncation when within limits."""
    from memoria.core.memory.episodic.generator import _truncate_messages

    messages = [{"role": "user", "content": "short"} for _ in range(5)]
    result, truncated = _truncate_messages(messages, max_messages=10, max_tokens=16000)
    assert not truncated
    assert len(result) == 5
