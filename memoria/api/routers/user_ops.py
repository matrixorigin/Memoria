"""User-facing reflect & consolidate — sync with TTL cache."""

import time
from typing import Any

from fastapi import APIRouter, Depends

from memoria.api.database import get_db_factory
from memoria.api.dependencies import get_current_user_id

router = APIRouter(tags=["memory"])

# In-memory TTL cache: (user_id, op) → (timestamp, result)
_cache: dict[tuple[str, str], tuple[float, Any]] = {}
_TTL = {"consolidate": 1800, "reflect": 7200, "extract_entities": 3600}  # seconds


def _with_cache(user_id: str, op: str, fn, force: bool) -> dict:
    key = (user_id, op)
    now = time.time()
    if not force:
        cached = _cache.get(key)
        if cached:
            ts, result = cached
            remaining = _TTL[op] - (now - ts)
            if remaining > 0:
                return {**result, "cached": True, "cooldown_remaining_s": int(remaining)}
    result = fn()
    _cache[key] = (now, result)
    return result


@router.post("/consolidate")
def consolidate(
    force: bool = False,
    user_id: str = Depends(get_current_user_id),
    db_factory=Depends(get_db_factory),
):
    """Detect contradictions, fix orphaned nodes. 30min cooldown."""
    def _run():
        from memoria.core.memory.factory import create_memory_service
        svc = create_memory_service(db_factory, user_id=user_id)
        result = svc.consolidate(user_id)
        return result if isinstance(result, dict) else {"status": "done"}
    return _with_cache(user_id, "consolidate", _run, force)


@router.post("/reflect")
def reflect(
    force: bool = False,
    user_id: str = Depends(get_current_user_id),
    db_factory=Depends(get_db_factory),
):
    """Analyze memory clusters, synthesize insights. 2h cooldown. Requires LLM."""
    def _run():
        try:
            from memoria.core.memory.reflection.engine import ReflectionEngine
            from memoria.core.memory.tabular.candidates import CandidateProvider
            from memoria.core.memory.tabular.store import TabularStore

            store = TabularStore(db_factory)
            provider = CandidateProvider(db_factory)
            # LLM client — may not be configured
            from memoria.core.llm import get_llm_client
            llm = get_llm_client()
            engine = ReflectionEngine(provider, store, llm)
            result = engine.reflect(user_id)
            return {"insights": len(result.new_scenes), "skipped": result.skipped}
        except Exception as e:
            return {"insights": 0, "skipped": 0, "note": f"reflect unavailable: {e}"}
    return _with_cache(user_id, "reflect", _run, force)


@router.post("/extract-entities")
def extract_entities(
    force: bool = False,
    user_id: str = Depends(get_current_user_id),
    db_factory=Depends(get_db_factory),
):
    """LLM entity extraction for unlinked memories. Manual trigger only. 1h cooldown."""
    def _run():
        try:
            from memoria.core.memory.graph.service import GraphMemoryService
            from memoria.core.llm import get_llm_client
            llm = get_llm_client()
            svc = GraphMemoryService(db_factory)
            return svc.extract_entities_llm(user_id, llm)
        except Exception as e:
            return {"total_memories": 0, "entities_found": 0, "edges_created": 0, "error": str(e)}
    return _with_cache(user_id, "extract_entities", _run, force)
