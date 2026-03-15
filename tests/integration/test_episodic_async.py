"""Test async episodic memory generation."""

import json

import pytest

from memoria.core.memory.episodic.tasks import TaskStatus, get_task_manager


class MockLLMClient:
    """Mock LLM for testing."""

    def chat(self, messages: list, **kwargs):
        return json.dumps(
            {
                "topic": "Test async generation",
                "action": "Tested async task processing",
                "outcome": "Task completed successfully",
            }
        )


@pytest.mark.integration
def test_async_task_creation(db_factory):
    """Test async task creation and polling."""
    task_manager = get_task_manager()

    # Create a task
    task_id = task_manager.create_task()
    assert task_id.startswith("task_")

    # Get task status
    task = task_manager.get_task(task_id)
    assert task is not None
    assert task.status == TaskStatus.PROCESSING
    assert task.result is None
    assert task.error is None

    # Complete task
    result = {"memory_id": "test_123", "content": "Test content"}
    task_manager.complete_task(task_id, result)

    # Verify completion
    task = task_manager.get_task(task_id)
    assert task.status == TaskStatus.COMPLETED
    assert task.result == result

    # Test failure
    task_id2 = task_manager.create_task()
    task_manager.fail_task(task_id2, "TEST_ERROR", "Test error message")

    task2 = task_manager.get_task(task_id2)
    assert task2.status == TaskStatus.FAILED
    assert task2.error == {"code": "TEST_ERROR", "message": "Test error message"}


@pytest.mark.integration
def test_task_cleanup(db_factory):
    """Test task TTL cleanup."""
    from datetime import datetime, timedelta, timezone

    task_manager = get_task_manager()

    # Create a task
    task_id = task_manager.create_task()

    # Manually set old timestamp
    task = task_manager.get_task(task_id)
    task.updated_at = datetime.now(timezone.utc) - timedelta(hours=2)

    # Cleanup (TTL is 1 hour by default)
    removed = task_manager.cleanup_old_tasks()
    assert removed == 1

    # Task should be gone
    assert task_manager.get_task(task_id) is None
