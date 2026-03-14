"""User-facing reflect & consolidate — sync with TTL cache."""

import hashlib
import json
import time
from datetime import datetime
from typing import Any

from fastapi import APIRouter, Depends
from pydantic import BaseModel, Field
from sqlalchemy import text

from memoria.api.database import get_db_factory
from memoria.api.dependencies import get_current_user_id
from memoria.core.memory.types import enum_value

router = APIRouter(tags=["memory"])

# In-memory TTL cache: (user_id, op) → (timestamp, result)
_cache: dict[tuple[str, str], tuple[float, Any]] = {}
_TTL = {"consolidate": 1800, "reflect": 7200, "extract_entities": 3600}  # seconds


def _with_cache(user_id: str, op: str, fn, force: bool, db_factory) -> dict:
    key = (user_id, op)
    uid_hash = hashlib.sha256(user_id.encode()).hexdigest()[:32]
    task_name = f"user_op:{op}:{uid_hash}"
    now = time.time()
    if not force:
        cached = _cache.get(key)
        if cached:
            ts, result = cached
            remaining = _TTL[op] - (now - ts)
            if remaining > 0:
                return {
                    **result,
                    "cached": True,
                    "cooldown_remaining_s": int(remaining),
                }
        db = db_factory()
        try:
            row = db.execute(
                text(
                    "SELECT result, created_at FROM governance_runs "
                    "WHERE task_name = :task "
                    "ORDER BY created_at DESC LIMIT 1"
                ),
                {"task": task_name},
            ).first()
            if row and row[1]:
                remaining = _TTL[op] - (now - row[1].timestamp())
                if remaining > 0:
                    try:
                        result = json.loads(row[0]) if row[0] else {}
                    except Exception:
                        result = {}
                    _cache[key] = (row[1].timestamp(), result)
                    return {
                        **result,
                        "cached": True,
                        "cooldown_remaining_s": int(remaining),
                    }
        finally:
            db.close()
    result = fn()
    _cache[key] = (now, result)
    db = db_factory()
    try:
        db.execute(
            text(
                "INSERT INTO governance_runs (task_name, result, created_at) "
                "VALUES (:task, :result, :ts)"
            ),
            {"task": task_name, "result": json.dumps(result), "ts": datetime.now()},
        )
        db.commit()
    except Exception:
        db.rollback()
    finally:
        db.close()
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

    return _with_cache(user_id, "consolidate", _run, force, db_factory)


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

    return _with_cache(user_id, "reflect", _run, force, db_factory)


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
            return {
                "total_memories": 0,
                "entities_found": 0,
                "edges_created": 0,
                "error": str(e),
            }

    return _with_cache(user_id, "extract_entities", _run, force, db_factory)


@router.post("/reflect/candidates")
def reflect_candidates(
    user_id: str = Depends(get_current_user_id),
    db_factory=Depends(get_db_factory),
):
    """Return raw reflection candidates for user-LLM synthesis (no internal LLM needed)."""
    from memoria.core.memory.graph.candidates import GraphCandidateProvider

    provider = GraphCandidateProvider(db_factory)
    candidates = provider.get_reflection_candidates(user_id)
    return {
        "candidates": [
            {
                "signal": c.signal,
                "importance": round(c.importance_score, 3),
                "memories": [
                    {
                        "memory_id": m.memory_id,
                        "content": m.content,
                        "type": enum_value(m.memory_type),
                    }
                    for m in c.memories
                ],
            }
            for c in candidates
        ]
    }


@router.post("/extract-entities/candidates")
def entity_candidates(
    user_id: str = Depends(get_current_user_id),
    db_factory=Depends(get_db_factory),
):
    """Return unlinked memories for user-LLM entity extraction."""
    from memoria.core.memory.graph.graph_store import GraphStore
    from memoria.core.memory.graph.types import EdgeType, NodeType

    store = GraphStore(db_factory)
    nodes = store.get_user_nodes(
        user_id, node_type=NodeType.SEMANTIC, active_only=True, load_embedding=False
    )
    if not nodes:
        return {"memories": []}
    node_ids = {n.node_id for n in nodes}
    edges = store.get_edges_for_nodes(node_ids)
    linked = {
        nid
        for nid, es in edges.items()
        if any(e.edge_type == EdgeType.ENTITY_LINK.value for e in es)
    }
    unlinked = [n for n in nodes if n.node_id not in linked]

    # Include existing entity nodes so caller knows what types are already linked
    entity_nodes = store.get_user_nodes(
        user_id, node_type=NodeType.ENTITY, active_only=True, load_embedding=False
    )
    return {
        "memories": [
            {"memory_id": n.memory_id or n.node_id, "content": n.content}
            for n in unlinked[:50]
        ],
        "existing_entities": [
            {"name": n.content, "entity_type": n.entity_type} for n in entity_nodes
        ],
    }


# Allowed entity types — free-form input is normalized to these.
VALID_ENTITY_TYPES = {"tech", "person", "repo", "project", "concept"}


class LinkEntitiesRequest(BaseModel):
    entities: list[dict] = Field(..., min_length=1)


@router.get("/entities")
def list_entities(
    user_id: str = Depends(get_current_user_id),
    db_factory=Depends(get_db_factory),
):
    """List all entity nodes for the current user."""
    from memoria.core.memory.graph.graph_store import GraphStore
    from memoria.core.memory.graph.types import NodeType

    store = GraphStore(db_factory)
    nodes = store.get_user_nodes(
        user_id, node_type=NodeType.ENTITY, active_only=True, load_embedding=False
    )
    return {
        "entities": [
            {
                "node_id": n.node_id,
                "name": n.content,
                "entity_type": n.entity_type,
                "importance": round(n.importance, 2),
            }
            for n in nodes
        ]
    }


@router.post("/extract-entities/link")
def link_entities(
    req: LinkEntitiesRequest,
    user_id: str = Depends(get_current_user_id),
    db_factory=Depends(get_db_factory),
):
    """Write entity nodes + edges from user-LLM extraction results."""
    from memoria.core.memory.graph.graph_store import GraphStore
    from memoria.core.memory.graph.types import GraphNodeData

    store = GraphStore(db_factory)

    # Resolve memory_ids → graph nodes, collect entities per node
    nodes: list[GraphNodeData] = []
    entities_per_node: dict[str, list[tuple[str, str]]] = {}
    for item in req.entities:
        memory_id = item.get("memory_id", "")
        node = store.get_node_by_memory_id(memory_id)
        if not node:
            continue
        ent_list = []
        for ent in item.get("entities", []):
            name = str(ent.get("name", "")).strip().lower()
            if name:
                etype = str(ent.get("type", "concept")).lower()
                if etype not in VALID_ENTITY_TYPES:
                    etype = "concept"
                ent_list.append((name, etype))
        if ent_list:
            nodes.append(node)
            entities_per_node[node.node_id] = ent_list

    created, pending_edges, reused = store.link_entities_batch(
        user_id,
        nodes,
        entities_per_node,
        source="manual",
    )
    if pending_edges:
        store.add_edges_batch(pending_edges, user_id)
    return {
        "entities_created": len(created),
        "entities_reused": reused,
        "edges_created": len(pending_edges),
        "entities": [
            {"name": e.content, "entity_type": e.entity_type} for e in created
        ],
    }
