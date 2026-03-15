"""Session-related endpoints for episodic memory generation."""

from __future__ import annotations

import logging
import threading
from typing import Any, Literal

from fastapi import APIRouter, Depends, HTTPException, status
from pydantic import BaseModel, Field
from sqlalchemy import text

from memoria.api.database import get_db_factory
from memoria.api.dependencies import get_current_user_id
from memoria.core.llm import get_llm_client
from memoria.core.memory.episodic import (
    generate_episodic_memory,
    generate_lightweight_summary,
)
from memoria.core.memory.episodic.tasks import get_task_manager
from memoria.core.memory.tabular.service import MemoryService
from memoria.core.memory.types import MemoryType

logger = logging.getLogger(__name__)
router = APIRouter(tags=["sessions"])


class SessionSummaryRequest(BaseModel):
    """Request body for POST /v1/sessions/{session_id}/summary."""

    mode: Literal["full", "lightweight"] = Field(
        default="full",
        description="Summary mode: 'full' (topic/action/outcome) or 'lightweight' (3-5 bullet points)",
    )
    sync: bool = Field(
        default=False,
        description="If true, wait for generation to complete. If false, return task_id for polling (default).",
    )
    focus_topics: list[str] | None = Field(
        default=None, description="Optional: specific topics to focus on in the summary"
    )
    generate_embedding: bool = Field(
        default=True,
        description="If false, skip embedding generation (retrieval unavailable)",
    )


class SessionSummaryResponse(BaseModel):
    """Response for session summary generation."""

    memory_id: str | None = Field(
        None, description="ID of created episodic memory (sync mode)"
    )
    task_id: str | None = Field(None, description="Task ID for polling (async mode)")
    content: str | None = Field(
        None, description="Generated episodic memory content (sync mode)"
    )
    truncated: bool = Field(False, description="True if input was truncated")
    metadata: dict[str, Any] | None = Field(
        None, description="Episodic metadata (sync mode)"
    )
    mode: str = Field("full", description="Summary mode used")


class TaskResponse(BaseModel):
    """Response for task status polling."""

    task_id: str
    status: str  # "processing" | "completed" | "failed"
    created_at: str
    updated_at: str
    result: dict[str, Any] | None = None
    error: dict[str, str] | None = None


@router.post(
    "/sessions/{session_id}/summary",
    response_model=SessionSummaryResponse,
    status_code=200,
)
async def create_session_summary(
    session_id: str,
    request: SessionSummaryRequest,
    user_id: str = Depends(get_current_user_id),
    db_factory=Depends(get_db_factory),
):
    """Generate episodic memory summary from session memories.

    Modes:
    - full: Generates topic/action/outcome summary (default)
    - lightweight: Generates 3-5 bullet points (faster, rate-limited to 3/session)
    """
    llm_client = get_llm_client()
    if llm_client is None:
        raise HTTPException(
            status_code=status.HTTP_503_SERVICE_UNAVAILABLE,
            detail="LLM not configured — episodic memory generation unavailable",
        )

    # Rate limit check + increment for lightweight mode (before task creation)
    task_manager = get_task_manager()
    if request.mode == "lightweight":
        if not task_manager.check_lightweight_rate_limit(session_id):
            count = task_manager.get_lightweight_count(session_id)
            raise HTTPException(
                status_code=status.HTTP_429_TOO_MANY_REQUESTS,
                detail=f"Lightweight summary rate limit exceeded for session {session_id} "
                f"({count}/3 used). Use mode='full' for additional summaries.",
            )
        task_manager.increment_lightweight_count(session_id)

    # Async mode: create task and return immediately
    if not request.sync:
        task_id = task_manager.create_task()

        def _process_async() -> None:
            try:
                result = _generate_and_store(
                    user_id,
                    session_id,
                    llm_client,
                    db_factory,
                    request.generate_embedding,
                    request.mode,
                    request.focus_topics,
                )
                task_manager.complete_task(task_id, result)
            except Exception as e:
                logger.error("Async task %s failed: %s", task_id, e)
                task_manager.fail_task(task_id, "GENERATION_ERROR", str(e))

        thread = threading.Thread(target=_process_async, daemon=True)
        thread.start()

        return SessionSummaryResponse(
            task_id=task_id, truncated=False, mode=request.mode
        )

    # Sync mode: process immediately
    try:
        result = _generate_and_store(
            user_id,
            session_id,
            llm_client,
            db_factory,
            request.generate_embedding,
            request.mode,
            request.focus_topics,
        )
        return SessionSummaryResponse(
            memory_id=result["memory_id"],
            content=result["content"],
            truncated=result["truncated"],
            metadata=result["metadata"],
            mode=request.mode,
        )
    except ValueError as e:
        raise HTTPException(
            status_code=status.HTTP_500_INTERNAL_SERVER_ERROR,
            detail=str(e),
        )


def _generate_and_store(
    user_id: str,
    session_id: str,
    llm_client: Any,
    db_factory: Any,
    generate_embedding: bool,
    mode: str = "full",
    focus_topics: list[str] | None = None,
) -> dict[str, Any]:
    """Generate episodic memory and store it. Used by both sync and async paths."""
    with db_factory() as db:
        result = db.execute(
            text(
                "SELECT memory_id, content, memory_type, created_at "
                "FROM mem_memories "
                "WHERE user_id = :user_id AND session_id = :session_id AND is_active = 1 "
                "ORDER BY created_at ASC"
            ),
            {"user_id": user_id, "session_id": session_id},
        )
        memories = result.fetchall()

    if not memories:
        raise ValueError(f"No memories found for session {session_id}")

    messages = [
        {
            "id": mem.memory_id,
            "role": "user",
            "content": f"[{mem.memory_type}] {mem.content}",
        }
        for mem in memories
    ]

    service = MemoryService(db_factory)

    if mode == "lightweight":
        points, truncated = generate_lightweight_summary(messages, llm_client)
        content = "Session Highlights:\n" + "\n".join(f"• {p}" for p in points)
        metadata: dict[str, Any] = {"mode": "lightweight", "points": points}
        memory_id = service.store(
            user_id=user_id,
            content=content,
            memory_type=MemoryType.EPISODIC,
            session_id=None,
            extra_metadata=metadata,
            generate_embedding=generate_embedding,
        )
    else:
        ep_metadata, truncated = generate_episodic_memory(
            messages, llm_client, session_id=session_id, focus_topics=focus_topics
        )
        content = f"Session Summary: {ep_metadata.topic}\n\nActions: {ep_metadata.action}\n\nOutcome: {ep_metadata.outcome}"
        metadata = ep_metadata.model_dump()
        memory_id = service.store(
            user_id=user_id,
            content=content,
            memory_type=MemoryType.EPISODIC,
            session_id=None,
            extra_metadata=metadata,
            generate_embedding=generate_embedding,
        )

    return {
        "memory_id": memory_id,
        "content": content,
        "truncated": truncated,
        "metadata": metadata,
    }


@router.get("/tasks/{task_id}", response_model=TaskResponse)
async def get_task_status(task_id: str) -> TaskResponse:
    """Poll task status for async episodic generation."""
    task_manager = get_task_manager()
    task = task_manager.get_task(task_id)

    if not task:
        raise HTTPException(
            status_code=status.HTTP_404_NOT_FOUND,
            detail=f"Task {task_id} not found",
        )

    return TaskResponse(
        task_id=task.task_id,
        status=task.status.value,
        created_at=task.created_at.isoformat(),
        updated_at=task.updated_at.isoformat(),
        result=task.result,
        error=task.error,
    )
