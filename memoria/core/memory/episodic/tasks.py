"""Background task management for async episodic memory generation.

⚠️ Phase 1 Limitation: Tasks are stored in process memory.
- Tasks are lost on server restart
- Multi-process deployments: task status only visible on the process that created it
- For guaranteed delivery, use sync=true in API requests
- Phase 2 will add Redis/Celery for distributed task storage
"""

from __future__ import annotations

import logging
import threading
import uuid
from dataclasses import dataclass
from datetime import datetime, timezone
from enum import Enum
from typing import Any, Optional

logger = logging.getLogger(__name__)


class TaskStatus(str, Enum):
    """Task execution status."""

    PROCESSING = "processing"
    COMPLETED = "completed"
    FAILED = "failed"


@dataclass
class Task:
    """Background task state."""

    task_id: str
    status: TaskStatus
    created_at: datetime
    updated_at: datetime
    result: Optional[dict[str, Any]] = None
    error: Optional[dict[str, str]] = None


class TaskManager:
    """In-memory task manager for async episodic generation.

    Phase 1: Simple in-memory storage with TTL cleanup.
    Phase 2: Could move to Redis or database for persistence.
    """

    def __init__(self, ttl_seconds: int = 3600, max_lightweight_per_session: int = 3):
        self._tasks: dict[str, Task] = {}
        self._lock = threading.Lock()
        self._ttl = ttl_seconds
        self._max_lightweight_per_session = max_lightweight_per_session
        # session_id -> (count, last_updated_at) — TTL-bounded to prevent unbounded growth
        self._lightweight_counts: dict[str, tuple[int, datetime]] = {}

    def create_task(self) -> str:
        """Create a new task and return its ID."""
        task_id = f"task_{uuid.uuid4().hex[:16]}"
        now = datetime.now(timezone.utc)
        task = Task(
            task_id=task_id,
            status=TaskStatus.PROCESSING,
            created_at=now,
            updated_at=now,
        )
        with self._lock:
            self._tasks[task_id] = task
        return task_id

    def get_task(self, task_id: str) -> Optional[Task]:
        """Get task by ID."""
        with self._lock:
            return self._tasks.get(task_id)

    def complete_task(self, task_id: str, result: dict[str, Any]) -> None:
        """Mark task as completed with result."""
        with self._lock:
            task = self._tasks.get(task_id)
            if task:
                task.status = TaskStatus.COMPLETED
                task.result = result
                task.updated_at = datetime.now(timezone.utc)

    def fail_task(self, task_id: str, error_code: str, error_message: str) -> None:
        """Mark task as failed with error."""
        with self._lock:
            task = self._tasks.get(task_id)
            if task:
                task.status = TaskStatus.FAILED
                task.error = {"code": error_code, "message": error_message}
                task.updated_at = datetime.now(timezone.utc)

    def cleanup_old_tasks(self) -> int:
        """Remove tasks and lightweight counts older than TTL. Returns count of removed tasks."""
        now = datetime.now(timezone.utc)
        with self._lock:
            old_tasks = [
                tid
                for tid, task in self._tasks.items()
                if (now - task.updated_at).total_seconds() > self._ttl
            ]
            for tid in old_tasks:
                del self._tasks[tid]
            # Also clean up stale lightweight counts
            stale_sessions = [
                sid
                for sid, (_, ts) in self._lightweight_counts.items()
                if (now - ts).total_seconds() > self._ttl
            ]
            for sid in stale_sessions:
                del self._lightweight_counts[sid]
        return len(old_tasks)

    def check_lightweight_rate_limit(self, session_id: str) -> bool:
        """Check if lightweight summary is allowed for this session.

        Returns True if allowed, False if rate limit exceeded.
        """
        now = datetime.now(timezone.utc)
        with self._lock:
            entry = self._lightweight_counts.get(session_id)
            if entry is None:
                return True
            count, ts = entry
            # Expired entry — treat as fresh
            if (now - ts).total_seconds() > self._ttl:
                return True
            return count < self._max_lightweight_per_session

    def increment_lightweight_count(self, session_id: str) -> int:
        """Increment lightweight summary count for session. Returns new count."""
        now = datetime.now(timezone.utc)
        with self._lock:
            entry = self._lightweight_counts.get(session_id)
            if entry is None or (now - entry[1]).total_seconds() > self._ttl:
                count = 1
            else:
                count = entry[0] + 1
            self._lightweight_counts[session_id] = (count, now)
            return count

    def get_lightweight_count(self, session_id: str) -> int:
        """Get current lightweight summary count for session."""
        now = datetime.now(timezone.utc)
        with self._lock:
            entry = self._lightweight_counts.get(session_id)
            if entry is None:
                return 0
            count, ts = entry
            if (now - ts).total_seconds() > self._ttl:
                return 0
            return count


# Global task manager instance
_task_manager: Optional[TaskManager] = None


def get_task_manager() -> TaskManager:
    """Get or create global task manager."""
    global _task_manager
    if _task_manager is None:
        _task_manager = TaskManager()
    return _task_manager
