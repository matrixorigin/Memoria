"""Memoria Lite — MCP server.

Exposes memory tools (store, retrieve, correct, purge, profile, search,
snapshots, branches) via MCP protocol.

Two backends:
  - EmbeddedBackend: direct DB access (local / stdio mode)
  - HTTPBackend: proxies to memory service REST API (remote mode)
"""

from __future__ import annotations

import argparse
import contextlib
import logging
import os
from abc import ABC, abstractmethod
from typing import Any, ClassVar

from sqlalchemy import create_engine, text
from sqlalchemy.orm import sessionmaker
from mcp.server import FastMCP

from memoria.mcp_local.messages import (
    MSG_CONSOLIDATION_DONE,
    MSG_CONSOLIDATION_SKIPPED,
    MSG_CORRECT_NO_CONTENT,
    MSG_CORRECT_NO_TARGET,
    MSG_CORRECTED_BY_ID,
    MSG_CORRECTED_BY_QUERY,
    MSG_GOVERNANCE_DONE,
    MSG_GOVERNANCE_SKIPPED,
    MSG_HEALTH_HEADER,
    MSG_INDEX_NEEDS_REBUILD,
    MSG_INDEX_REBUILT,
    MSG_PURGE_NO_TARGET,
    MSG_PURGED,
    MSG_REFLECTION_DONE,
    MSG_REFLECTION_NO_CANDIDATES,
    MSG_REFLECTION_SKIPPED,
    MSG_RETRIEVE_EMPTY,
    MSG_RETRIEVE_FOUND,
    MSG_RETRIEVE_ITEM,
    MSG_SEARCH_EMPTY,
    MSG_SEARCH_FOUND,
    MSG_SEARCH_ITEM,
    MSG_STORED,
    MSG_WARNING_PREFIX,
)

logger = logging.getLogger(__name__)

# ── Backend protocol ──────────────────────────────────────────────────


class MemoryBackend(ABC):
    """Abstract backend for memory operations."""

    @abstractmethod
    def store(
        self, user_id: str, content: str, memory_type: str, session_id: str | None
    ) -> dict: ...
    @abstractmethod
    def retrieve(
        self, user_id: str, query: str, top_k: int, session_id: str | None = None
    ) -> list[dict]: ...
    @abstractmethod
    def correct(
        self, user_id: str, memory_id: str, new_content: str, reason: str
    ) -> dict: ...
    @abstractmethod
    def correct_by_query(
        self, user_id: str, query: str, new_content: str, reason: str
    ) -> dict: ...
    @abstractmethod
    def purge(
        self, user_id: str, memory_id: str | None, topic: str | None, reason: str
    ) -> dict: ...
    @abstractmethod
    def profile(self, user_id: str) -> dict: ...
    @abstractmethod
    def search(self, user_id: str, query: str, top_k: int) -> list[dict]: ...
    @abstractmethod
    def governance(self, user_id: str, force: bool = False) -> dict: ...
    @abstractmethod
    def consolidate(self, user_id: str, force: bool = False) -> dict: ...
    @abstractmethod
    def reflect(self, user_id: str, force: bool = False) -> dict: ...
    @abstractmethod
    def extract_entities(self, user_id: str) -> dict: ...
    @abstractmethod
    def get_reflect_candidates(self, user_id: str) -> dict: ...
    @abstractmethod
    def get_entity_candidates(self, user_id: str) -> dict: ...
    @abstractmethod
    def link_entities(self, user_id: str, entities: list[dict]) -> dict: ...
    @abstractmethod
    def rebuild_index(self, table: str) -> str: ...
    @abstractmethod
    def health_warnings(self, user_id: str) -> list[str]: ...
    # Branching
    @abstractmethod
    def snapshot_create(self, user_id: str, name: str, description: str) -> dict: ...
    @abstractmethod
    def snapshot_list(self, user_id: str) -> list[dict]: ...
    @abstractmethod
    def snapshot_delete(
        self,
        user_id: str,
        names: list[str] | None = None,
        prefix: str | None = None,
        older_than: str | None = None,
    ) -> dict: ...
    @abstractmethod
    def snapshot_rollback(self, user_id: str, name: str) -> dict: ...
    @abstractmethod
    def branch_create(
        self,
        user_id: str,
        name: str,
        from_snapshot: str | None,
        from_timestamp: str | None,
    ) -> dict: ...
    @abstractmethod
    def branch_list(self, user_id: str) -> list[dict]: ...
    @abstractmethod
    def branch_checkout(self, user_id: str, name: str) -> dict: ...
    @abstractmethod
    def branch_delete(self, user_id: str, name: str) -> dict: ...
    @abstractmethod
    def branch_merge(self, user_id: str, source: str, strategy: str) -> dict: ...
    @abstractmethod
    def branch_diff(self, user_id: str, source: str, limit: int) -> dict: ...


class EmbeddedBackend(MemoryBackend):
    """Direct DB access — for local stdio mode."""

    def __init__(self, db_url: str | None = None) -> None:
        import sys

        # MatrixOne SDK adds a StreamHandler(stdout) during Client().
        # Temporarily redirect stdout → stderr to prevent protocol pollution.
        _real_stdout = sys.stdout
        sys.stdout = sys.stderr
        try:
            if db_url:
                self._engine = create_engine(db_url, pool_pre_ping=True)
                self._db_factory = sessionmaker(bind=self._engine)
                # Auto-create tables on first run (idempotent).
                # Pass EMBEDDING_DIM so the dim check runs even when tables already exist.
                from memoria.schema import ensure_tables, DEFAULT_DIM

                try:
                    ensure_tables(self._engine, dim=DEFAULT_DIM)
                except Exception as e:
                    logger.warning("Auto-migrate failed: %s", e)
            else:
                from memoria.api.database import SessionLocal

                self._engine = None  # dev mode: engine lives inside SessionLocal
                self._db_factory = SessionLocal
            from memoria.core.memory.factory import create_editor, create_memory_service
        finally:
            sys.stdout = _real_stdout

        # Replace matrixone's stdout handler with stderr permanently.
        _mo = logging.getLogger("matrixone")
        _mo.handlers = [logging.StreamHandler(sys.stderr)]
        _mo.setLevel(logging.WARNING)

        # Configure embedding from env vars (standalone) or project settings (dev).
        # Lazy: don't load the model at startup — load on first store/retrieve call.
        # This avoids the ~3-5s sentence-transformers model load blocking MCP handshake.
        self._embed_client = None
        self._embed_client_initialized = False
        self._embed_client_standalone = bool(
            db_url
        )  # only auto-init in standalone mode
        self._create_service = create_memory_service
        self._create_editor = create_editor
        # Per-instance state — class-level dicts would be shared across
        # instances (e.g. in tests with pytest -n auto), causing state pollution.
        self._active_branches: dict[str, str] = {}
        # Cache (engine, sessionmaker) per (user_id, branch_name) to reuse
        # connection pools. Engine stored so we can dispose() on eviction.
        self._branch_factory_cache: dict[tuple[str, str], tuple[Any, Any]] = {}
        self._cooldown_cache: dict[tuple[str, str], tuple[float, dict]] = {}

    @staticmethod
    def _make_embed_client():
        """Build EmbeddingClient from EMBEDDING_* env vars.

        MCP server runs locally as a dev tool. Default to "local" provider
        (sentence-transformers) so `memoria init` works without API keys.
        Production deployments (Memoria, mo-agent API) use their own
        config files which default to "openai" + BAAI/bge-m3.
        """
        provider = os.environ.get("EMBEDDING_PROVIDER") or "local"
        model = os.environ.get("EMBEDDING_MODEL") or "all-MiniLM-L6-v2"
        dim = int(os.environ.get("EMBEDDING_DIM") or "0")
        if dim == 0:
            from memoria.core.embedding.client import KNOWN_DIMENSIONS

            dim = KNOWN_DIMENSIONS.get(model, 1024)
        try:
            from memoria.core.embedding.client import EmbeddingClient

            return EmbeddingClient(
                provider=provider,
                model=model,
                dim=dim,
                api_key=os.environ.get("EMBEDDING_API_KEY") or "",
                base_url=os.environ.get("EMBEDDING_BASE_URL") or None,
            )
        except ImportError as exc:
            # sentence-transformers not installed — warn and degrade gracefully.
            if provider not in ("local", "mock"):
                logger.error(
                    "Embedding provider %r requested but init failed: %s. "
                    "Install the required package (e.g. `pip install openai`).",
                    provider,
                    exc,
                )
                raise
            logger.warning(
                "Embedding client not available, memories won't be vectorized"
            )
            return None

    def _get_embed_client(self):
        """Return embedding client, initializing lazily on first call."""
        if not self._embed_client_initialized:
            self._embed_client_initialized = True
            if self._embed_client_standalone:
                self._embed_client = self._make_embed_client()
        return self._embed_client

    def store(
        self, user_id: str, content: str, memory_type: str, session_id: str | None
    ) -> dict:
        from memoria.core.memory.types import MemoryType

        db_factory = self._branch_db_factory(user_id)
        embed_client = self._get_embed_client()
        editor = self._create_editor(
            db_factory, user_id=user_id, embed_client=embed_client
        )
        mem = editor.inject(
            user_id,
            content,
            memory_type=MemoryType(memory_type),
            source="mcp",
            session_id=session_id,
        )
        result: dict = {
            "memory_id": mem.memory_id,
            "content": mem.content,
            "branch": self._get_active_branch(user_id),
        }
        if embed_client is None:
            result["warning"] = (
                "Embedding client not available — memory stored without vector. Retrieval will fall back to keyword search."
            )
        return result

    def retrieve(
        self, user_id: str, query: str, top_k: int, session_id: str | None = None
    ) -> list[dict]:
        db_factory = self._branch_db_factory(user_id)
        svc = self._create_service(db_factory, user_id=user_id)
        # Generate query embedding for vector search (phase 2).
        # Falls back to keyword-only (phase 1) if embed client unavailable.
        query_embedding: list[float] | None = None
        embed = self._get_embed_client()
        if embed is not None:
            try:
                query_embedding = embed.embed(query)
            except Exception as e:
                logger.warning(
                    "Query embedding failed, falling back to keyword search: %s", e
                )
        memories, _ = svc.retrieve(
            user_id,
            query,
            query_embedding=query_embedding,
            top_k=top_k,
            session_id=session_id or "",
        )
        return [
            {"memory_id": m.memory_id, "content": m.content, "type": str(m.memory_type)}
            for m in memories
        ]

    # Thresholds for health_warnings — surfaced as constants for testability.
    _LOW_CONFIDENCE_THRESHOLD = 0.4
    _LOW_CONFIDENCE_WARNING_MIN = 5

    def health_warnings(self, user_id: str) -> list[str]:
        """Lightweight check for memory quality issues."""
        warnings: list[str] = []
        try:
            with self._db_factory() as db:
                row = db.execute(
                    text(
                        "SELECT COUNT(*) as cnt FROM mem_memories "
                        "WHERE user_id = :uid AND is_active = 1 "
                        "AND initial_confidence < :threshold"
                    ),
                    {"uid": user_id, "threshold": self._LOW_CONFIDENCE_THRESHOLD},
                ).fetchone()
                if row and row.cnt >= self._LOW_CONFIDENCE_WARNING_MIN:
                    warnings.append(
                        f"{row.cnt} memories have low confidence — consider reviewing with memory_search."
                    )
        except Exception as e:
            logger.debug("health_warnings query failed for user=%s: %s", user_id, e)
        return warnings

    def correct(
        self, user_id: str, memory_id: str, new_content: str, reason: str
    ) -> dict:
        db_factory = self._branch_db_factory(user_id)
        embed_client = self._get_embed_client()
        editor = self._create_editor(
            db_factory, user_id=user_id, embed_client=embed_client
        )
        mem = editor.correct(user_id, memory_id, new_content, reason=reason)
        result: dict = {"memory_id": mem.memory_id, "content": mem.content}
        if embed_client is None:
            result["warning"] = (
                "Embedding client not available — memory stored without vector. Retrieval will fall back to keyword search."
            )
        return result

    def correct_by_query(
        self, user_id: str, query: str, new_content: str, reason: str
    ) -> dict:
        db_factory = self._branch_db_factory(user_id)
        embed_client = self._get_embed_client()
        editor = self._create_editor(
            db_factory, user_id=user_id, embed_client=embed_client
        )
        match = editor.find_best_match(user_id, query)
        if match is None:
            return {
                "error": "no_match",
                "message": f"No memory found matching '{query}'",
            }
        mem = editor.correct(user_id, match.memory_id, new_content, reason=reason)
        result: dict = {
            "memory_id": mem.memory_id,
            "content": mem.content,
            "matched_memory_id": match.memory_id,
            "matched_content": match.content,
        }
        if embed_client is None:
            result["warning"] = (
                "Embedding client not available — memory stored without vector. Retrieval will fall back to keyword search."
            )
        return result

    def purge(
        self, user_id: str, memory_id: str | None, topic: str | None, reason: str
    ) -> dict:
        db_factory = self._branch_db_factory(user_id)
        editor = self._create_editor(
            db_factory, user_id=user_id, embed_client=self._get_embed_client()
        )
        if topic:
            # Use SQL LIKE for precise keyword matching. Semantic search
            # (self.retrieve) would return loosely related results ranked by
            # similarity with no score threshold — too dangerous for a
            # destructive bulk operation.
            with db_factory() as db:
                rows = db.execute(
                    text(
                        "SELECT memory_id FROM mem_memories "
                        "WHERE user_id = :uid AND is_active = 1 "
                        "AND content LIKE :pattern"
                    ),
                    {"uid": user_id, "pattern": f"%{topic}%"},
                ).fetchall()
            ids = [r.memory_id for r in rows]
            if not ids:
                return {"purged": 0}
            result = editor.purge(
                user_id, memory_ids=ids, reason=reason or f"topic purge: {topic}"
            )
        elif memory_id:
            ids = [m.strip() for m in memory_id.split(",") if m.strip()]
            result = editor.purge(user_id, memory_ids=ids, reason=reason)
        else:
            return {"purged": 0}
        return {"purged": result.deactivated}

    def profile(self, user_id: str) -> dict:
        db_factory = self._branch_db_factory(user_id)
        svc = self._create_service(db_factory, user_id=user_id)
        return {"user_id": user_id, "profile": svc.get_profile(user_id)}

    def search(self, user_id: str, query: str, top_k: int) -> list[dict]:
        return self.retrieve(user_id, query, top_k)

    # Cooldown: governance/consolidate/reflect are expensive, throttle per user.
    # key = (user_id, op_name), value = (timestamp, result).
    # Instance-level to avoid state pollution across parallel test workers.
    _COOLDOWN_SECONDS: ClassVar[dict[str, int]] = {
        "governance": 3600,
        "consolidate": 1800,
        "reflect": 7200,
    }

    def _with_cooldown(
        self, user_id: str, op: str, fn: Any, force: bool = False
    ) -> dict:
        import copy
        import time

        key = (user_id, op)
        now = time.time()
        if not force:
            cached = self._cooldown_cache.get(key)
            if cached:
                ts, result = cached
                remaining = self._COOLDOWN_SECONDS[op] - (now - ts)
                if remaining > 0:
                    result_copy = copy.deepcopy(result)
                    result_copy["skipped"] = True
                    result_copy["cooldown_remaining_s"] = int(remaining)
                    return result_copy
        result = fn()
        self._cooldown_cache[key] = (now, copy.deepcopy(result))
        return result

    def governance(self, user_id: str, force: bool = False) -> dict:
        # Intentionally operates on main DB only. Governance (decay, quarantine,
        # compression) is a global maintenance operation — running it on a branch
        # would be meaningless since branches are short-lived experiments.
        def _run():
            from memoria.core.memory.tabular.governance import GovernanceScheduler

            gs = GovernanceScheduler(self._db_factory)
            result = gs.run_cycle(user_id)
            gov_result: dict = {
                "quarantined": result.quarantined,
                "cleaned_stale": result.cleaned_stale,
                "compressed_redundant": result.compressed_redundant,
                "scenes_created": result.scenes_created,
                "vector_index_health": result.vector_index_health,
            }
            # Snapshot health: count total and auto-generated snapshots
            try:
                snaps = self.snapshot_list(user_id)
                total = len(snaps)
                auto = sum(
                    1
                    for s in snaps
                    if s.get("snapshot_name", "").startswith("mem_milestone_")
                    or s.get("name", "").startswith("auto:")
                    or s.get("name", "").startswith("pre_")
                )
                gov_result["snapshot_health"] = {
                    "total": total,
                    "auto_ratio": round(auto / total, 2) if total > 0 else 0.0,
                }
            except Exception:
                pass
            return gov_result

        return self._with_cooldown(user_id, "governance", _run, force=force)

    def consolidate(self, user_id: str, force: bool = False) -> dict:
        # Main-only: graph consolidation merges/promotes nodes in the canonical store.
        def _run():
            from memoria.core.memory.graph.consolidation import GraphConsolidator

            gc = GraphConsolidator(self._db_factory)
            result = gc.consolidate(user_id)
            return {
                "merged_nodes": result.merged_nodes,
                "conflicts_detected": result.conflicts_detected,
                "orphaned_scenes": result.orphaned_scenes,
                "promoted": result.promoted,
                "demoted": result.demoted,
            }

        return self._with_cooldown(user_id, "consolidate", _run, force=force)

    def reflect(self, user_id: str, force: bool = False) -> dict:
        # Main-only: reflection synthesizes scene nodes from the canonical memory store.
        def _run():
            from memoria.core.memory.graph.candidates import GraphCandidateProvider
            from memoria.core.memory.graph.service import GraphMemoryService
            from memoria.core.memory.reflection.engine import ReflectionEngine

            provider = GraphCandidateProvider(self._db_factory)
            svc = GraphMemoryService(self._db_factory)
            try:
                from memoria.core.llm import get_llm_client

                llm = get_llm_client()
            except Exception:
                return {"error": "LLM client not available for reflection"}

            store = svc._store  # GraphStore for callbacks

            def _subgraph_collector(uid: str, mem_ids: list[str]) -> str:
                """Collect neighbor + entity context for candidate memories."""
                parts: list[str] = []
                for mid in mem_ids[:10]:
                    node = store.get_node_by_memory_id(mid)
                    if not node:
                        continue
                    edges = store.get_outgoing_edges(node.node_id)
                    for edge in edges[:5]:
                        neighbor = store.get_node(edge.target_id)
                        if neighbor and neighbor.is_active and neighbor.content:
                            parts.append(f"[{neighbor.node_type}] {neighbor.content}")
                return "\n".join(parts[:20])

            def _opinion_checker(uid: str, content: str) -> list[str]:
                """Check new scene against existing high-confidence scenes via embedding similarity."""
                embed_client = self._get_embed_client()
                if not embed_client:
                    return []
                try:
                    new_emb = embed_client.embed_text(content)
                except Exception:
                    return []
                from memoria.core.memory.graph.types import NodeType as NT

                scenes = store.get_user_nodes(
                    uid, node_type=NT.SCENE, active_only=True, load_embedding=True
                )
                conflicts = []
                for s in scenes:
                    if s.confidence < 0.7 or not s.embedding:
                        continue
                    # Cosine similarity
                    dot = sum(a * b for a, b in zip(new_emb, s.embedding))
                    na = sum(a * a for a in new_emb) ** 0.5
                    nb = sum(b * b for b in s.embedding) ** 0.5
                    sim = dot / (na * nb) if na and nb else 0.0
                    # High similarity but different content → potential contradiction
                    if sim > 0.75:
                        conflicts.append(s.node_id)
                return conflicts

            def _entity_names_collector(uid: str, mem_ids: list[str]) -> set[str]:
                """Collect entity names linked to source memories."""
                from memoria.core.memory.models.entity import Entity, MemoryEntityLink

                names: set[str] = set()
                with svc._db_factory() as db:
                    rows = (
                        db.query(Entity.name)
                        .join(
                            MemoryEntityLink,
                            Entity.entity_id == MemoryEntityLink.entity_id,
                        )
                        .filter(
                            MemoryEntityLink.memory_id.in_(mem_ids),
                            MemoryEntityLink.user_id == uid,
                        )
                        .all()
                    )
                    names = {r[0] for r in rows}
                return names

            engine = ReflectionEngine(
                provider,
                svc,
                llm,
                subgraph_collector=_subgraph_collector,
                opinion_checker=_opinion_checker,
                entity_names_collector=_entity_names_collector,
            )
            result = engine.reflect(user_id)
            return {
                "scenes_created": result.scenes_created,
                "candidates_found": result.candidates_found,
            }

        return self._with_cooldown(user_id, "reflect", _run, force=force)

    def rebuild_index(self, table: str) -> str:
        from memoria.core.memory.tabular.governance import GovernanceScheduler

        gs = GovernanceScheduler(self._db_factory)
        result = gs.rebuild_vector_index(table)
        return f"Rebuilt IVF index for {table}: lists {result['old_lists']} → {result['new_lists']} (rows={result['total_rows']})"

    def extract_entities(self, user_id: str) -> dict:
        try:
            from memoria.core.memory.graph.service import GraphMemoryService
            from memoria.core.llm import get_llm_client

            llm = get_llm_client()
            svc = GraphMemoryService(self._db_factory)
            return svc.extract_entities_llm(user_id, llm)
        except Exception as e:
            return {
                "total_memories": 0,
                "entities_found": 0,
                "edges_created": 0,
                "error": str(e),
            }

    def get_reflect_candidates(self, user_id: str) -> dict:
        """Return raw reflection candidates for user-LLM synthesis (no internal LLM)."""
        from memoria.core.memory.graph.candidates import GraphCandidateProvider

        provider = GraphCandidateProvider(self._db_factory)
        candidates = provider.get_reflection_candidates(user_id)
        if not candidates:
            return {"candidates": []}
        clusters = []
        for c in candidates:
            clusters.append(
                {
                    "signal": c.signal,
                    "importance": round(c.importance_score, 3),
                    "memories": [
                        {
                            "memory_id": m.memory_id,
                            "content": m.content,
                            "type": str(m.memory_type),
                        }
                        for m in c.memories
                    ],
                }
            )
        return {"candidates": clusters}

    def get_entity_candidates(self, user_id: str) -> dict:
        """Return unlinked memories for user-LLM entity extraction (no internal LLM)."""
        from memoria.core.memory.graph.graph_store import GraphStore
        from memoria.core.memory.graph.types import EdgeType, NodeType

        store = GraphStore(self._db_factory)
        semantic_nodes = store.get_user_nodes(
            user_id, node_type=NodeType.SEMANTIC, active_only=True, load_embedding=False
        )
        if not semantic_nodes:
            return {"memories": [], "existing_entities": []}
        node_ids = {n.node_id for n in semantic_nodes}
        existing_edges = store.get_edges_for_nodes(node_ids)
        linked_ids = {
            nid
            for nid, edges in existing_edges.items()
            if any(e.edge_type == EdgeType.ENTITY_LINK.value for e in edges)
        }
        unlinked = [n for n in semantic_nodes if n.node_id not in linked_ids]
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

    def link_entities(self, user_id: str, entities: list[dict]) -> dict:
        """Write entity nodes + edges from user-LLM extraction results.

        Args:
            entities: [{"memory_id": "...", "entities": [{"name": "...", "type": "..."}]}]
        """
        from memoria.core.memory.graph.graph_store import GraphStore
        from memoria.core.memory.graph.types import GraphNodeData

        store = GraphStore(self._db_factory)

        nodes: list[GraphNodeData] = []
        entities_per_node: dict[str, list[tuple[str, str]]] = {}
        for item in entities:
            memory_id = item.get("memory_id", "")
            node = store.get_node_by_memory_id(memory_id)
            if not node:
                continue
            ent_list = []
            for ent in item.get("entities", []):
                name = str(ent.get("name", "")).strip().lower()
                if name:
                    ent_list.append((name, ent.get("type", "concept")))
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
        }

    # ── Branching ─────────────────────────────────────────────────────

    MAX_USER_SNAPSHOTS = 1000
    MAX_USER_BRANCHES = 20
    _BRANCH_TABLES = ("mem_memories", "memory_graph_nodes", "memory_graph_edges")

    @staticmethod
    def _sanitize_name(name: str) -> str:
        import re

        clean = re.sub(r"[^a-zA-Z0-9_]", "_", name)[:40]
        if not clean or not clean[0].isalpha():
            clean = "s_" + clean
        return clean

    def _git(self):
        from memoria.core.git_for_data import GitForData

        return GitForData(self._db_factory)

    def _source_db_name(self) -> str:
        """Return the source (main) database name, regardless of init mode."""
        if self._engine is not None:
            # Standalone mode: db_url was given, engine is ours.
            return str(self._engine.url.database)
        # Dev mode: engine lives inside SessionLocal (api.database).
        from memoria.api.database import SessionLocal

        return SessionLocal.kw["bind"].url.database

    def _get_active_branch(self, user_id: str) -> str:
        """Get active branch for user. Loads from DB on first access."""
        if user_id not in self._active_branches:
            # Restore from DB on cold start
            try:
                with self._db_factory() as db:
                    row = db.execute(
                        text(
                            "SELECT active_branch FROM mem_user_state "
                            "WHERE user_id = :uid"
                        ),
                        {"uid": user_id},
                    ).fetchone()
                    self._active_branches[user_id] = (
                        row.active_branch if row else "main"
                    )
            except Exception as e:
                logger.warning(
                    "Failed to load active branch for user=%s, defaulting to main: %s",
                    user_id,
                    e,
                )
                self._active_branches[user_id] = "main"
        return self._active_branches[user_id]

    def _evict_branch_cache(self, user_id: str, branch_name: str) -> None:
        """Remove a branch from the factory cache and dispose its engine."""
        entry = self._branch_factory_cache.pop((user_id, branch_name), None)
        if entry is not None:
            eng, _ = entry
            try:
                eng.dispose()
            except Exception:
                pass

    def _set_active_branch(self, user_id: str, name: str) -> None:
        """Set active branch for user. Persisted to DB.

        Also invalidates the branch factory cache for this user so the next
        _branch_db_factory call picks up the new branch.
        """
        old = self._active_branches.get(user_id)
        self._active_branches[user_id] = name
        # Invalidate cached factory for the old branch (if any)
        if old and old != "main":
            self._evict_branch_cache(user_id, old)
        try:
            with self._db_factory() as db:
                db.execute(
                    text(
                        "INSERT INTO mem_user_state (user_id, active_branch, updated_at) "
                        "VALUES (:uid, :branch, NOW()) "
                        "ON DUPLICATE KEY UPDATE active_branch = :branch, updated_at = NOW()"
                    ),
                    {"uid": user_id, "branch": name},
                )
                db.commit()
        except Exception as e:
            # Best-effort persist; in-memory is authoritative for this session.
            # On next cold start the branch will revert to main if this write failed.
            logger.warning(
                "Failed to persist active branch for user=%s: %s", user_id, e
            )

    def _branch_db_factory(self, user_id: str) -> Any:
        """Return db_factory for the user's active branch. Main → original factory.

        Caches the (engine, sessionmaker) per (user_id, branch_name) so that
        repeated CRUD calls reuse the same connection pool. Engine is stored
        so we can dispose() it when the cache entry is evicted.
        """
        branch = self._get_active_branch(user_id)
        if branch == "main":
            return self._db_factory

        cache_key = (user_id, branch)
        cached = self._branch_factory_cache.get(cache_key)
        if cached is not None:
            return cached[1]  # return factory from (engine, factory) tuple

        # Look up branch_db name
        with self._db_factory() as db:
            row = db.execute(
                text(
                    "SELECT branch_db FROM mem_branches "
                    "WHERE user_id = :uid AND name = :name AND status = 'active'"
                ),
                {"uid": user_id, "name": branch},
            ).fetchone()
        if not row:
            # Branch gone (deleted externally), reset to main silently.
            self._set_active_branch(user_id, "main")
            return self._db_factory
        # Derive the branch URL from the source engine URL — avoids calling
        # Session.get_bind() which was removed in SQLAlchemy 2.0.
        src_url = self._source_engine_url()
        branch_url = src_url.set(database=row.branch_db)
        eng = create_engine(branch_url, pool_pre_ping=True)
        factory = sessionmaker(bind=eng)
        self._branch_factory_cache[cache_key] = (eng, factory)
        return factory

    def _source_engine_url(self) -> Any:
        """Return the SQLAlchemy URL of the source (main) engine."""
        if self._engine is not None:
            return self._engine.url
        # Dev mode: engine lives inside SessionLocal.
        from memoria.api.database import SessionLocal

        return SessionLocal.kw["bind"].url

    def snapshot_create(self, user_id: str, name: str, description: str) -> dict:
        safe = self._sanitize_name(name)
        with self._db_factory() as db:
            cnt = (
                db.execute(
                    text("SELECT COUNT(*) FROM mo_catalog.mo_snapshots")
                ).scalar()
                or 0
            )
        if cnt >= self.MAX_USER_SNAPSHOTS:
            return {
                "error": f"Snapshot limit reached ({self.MAX_USER_SNAPSHOTS}). Delete old snapshots first."
            }
        snap_name = f"mem_snap_{safe}"
        info = self._git().create_snapshot(snap_name)
        return {
            "name": name,
            "snapshot_name": snap_name,
            "timestamp": str(info.get("timestamp", "")),
        }

    def snapshot_list(self, user_id: str) -> list[dict]:
        all_snaps = self._git().list_snapshots()
        result = []
        for s in all_snaps:
            sname = s["snapshot_name"]
            if sname.startswith("mem_snap_") or sname.startswith("mem_milestone_"):
                display = sname.replace("mem_snap_", "").replace(
                    "mem_milestone_", "auto:"
                )
                result.append(
                    {
                        "name": display,
                        "snapshot_name": sname,
                        "timestamp": str(s.get("timestamp", "")),
                    }
                )
        return sorted(result, key=lambda x: x["timestamp"], reverse=True)

    def snapshot_delete(
        self,
        user_id: str,
        names: list[str] | None = None,
        prefix: str | None = None,
        older_than: str | None = None,
    ) -> dict:
        """Delete snapshots by name list, prefix, or age.

        Returns dict with 'deleted' count and 'names' list.
        """
        from datetime import datetime

        snaps = self.snapshot_list(user_id)
        git = self._git()
        to_delete: list[dict] = []

        if names:
            name_set = set(names)
            to_delete = [s for s in snaps if s["name"] in name_set]
        elif prefix:
            to_delete = [s for s in snaps if s["name"].startswith(prefix)]
        elif older_than:
            try:
                cutoff = datetime.fromisoformat(older_than)
            except ValueError:
                return {
                    "error": "older_than must be ISO format (e.g. '2026-03-01' or '2026-03-01T00:00:00')"
                }
            to_delete = [
                s
                for s in snaps
                if s["timestamp"] and datetime.fromisoformat(s["timestamp"]) < cutoff
            ]
        else:
            return {"error": "Provide names, prefix, or older_than"}

        deleted_names: list[str] = []
        errors: list[str] = []
        for s in to_delete:
            try:
                git.drop_snapshot(s["snapshot_name"])
                deleted_names.append(s["name"])
            except Exception as e:
                errors.append(f"{s['name']}: {e}")

        result: dict = {"deleted": len(deleted_names), "names": deleted_names}
        if errors:
            result["errors"] = errors
        return result

    def snapshot_rollback(self, user_id: str, name: str) -> dict:
        safe = self._sanitize_name(name)
        snap_name = (
            name
            if name.startswith("mem_snap_") or name.startswith("mem_milestone_")
            else f"mem_snap_{safe}"
        )
        git = self._git()
        for table in (
            "mem_memories",
            "memory_graph_nodes",
            "memory_graph_edges",
            "mem_edit_log",
        ):
            try:
                git.restore_table_from_snapshot(table, snap_name)
            except Exception as e:
                if table == "mem_memories":
                    return {"error": f"Rollback failed: {e}"}
                logger.debug("Rollback table %s skipped: %s", table, e)
        return {"rolled_back_to": snap_name}

    def branch_create(
        self,
        user_id: str,
        name: str,
        from_snapshot: str | None,
        from_timestamp: str | None = None,
    ) -> dict:
        if from_snapshot and from_timestamp:
            return {"error": "Specify from_snapshot or from_timestamp, not both."}
        safe = self._sanitize_name(name)

        # Global branch limit (not per-user). Prevents resource exhaustion across all users.
        with self._db_factory() as db:
            active = (
                db.execute(
                    text("SELECT COUNT(*) FROM mem_branches WHERE status = 'active'")
                ).scalar()
                or 0
            )
        if active >= self.MAX_USER_BRANCHES:
            return {
                "error": f"Branch limit reached ({self.MAX_USER_BRANCHES}). Delete old branches first."
            }

        # Duplicate check: reject if branch with same name already exists (active or deleted).
        # This prevents name reuse confusion. Deleted branches are soft-deleted and can be purged later.
        with self._db_factory() as db:
            dup = db.execute(
                text(
                    "SELECT branch_id FROM mem_branches WHERE user_id = :uid AND name = :name AND status != 'purged'"
                ),
                {"uid": user_id, "name": safe},
            ).fetchone()
        if dup:
            return {
                "error": f"Branch '{safe}' already exists or was recently deleted. Use a different name."
            }

        snap = from_snapshot
        if (
            snap
            and not snap.startswith("mem_snap_")
            and not snap.startswith("mem_milestone_")
        ):
            snap = f"mem_snap_{self._sanitize_name(snap)}"

        # Validate timestamp: within last 30 minutes
        if from_timestamp:
            from datetime import datetime, timedelta, timezone

            try:
                ts = datetime.strptime(from_timestamp, "%Y-%m-%d %H:%M:%S").replace(
                    tzinfo=timezone.utc
                )
            except ValueError:
                return {"error": "from_timestamp must be 'YYYY-MM-DD HH:MM:SS'"}
            now = datetime.now(timezone.utc)
            if ts > now:
                return {"error": "from_timestamp cannot be in the future"}
            if now - ts > timedelta(minutes=30):
                return {"error": "from_timestamp must be within the last 30 minutes"}

        from memoria.core.utils.id_generator import generate_id

        branch_id = generate_id()
        branch_db = f"mem_br_{branch_id}"
        src_db = self._source_db_name()

        # Determine branch point
        snap_name: str | None = None
        if snap:
            snap_name = snap
            with contextlib.suppress(Exception):
                self._git().create_snapshot(snap_name)
        elif not from_timestamp:
            snap_name = f"mem_br_base_{branch_id}"
            self._git().create_snapshot(snap_name)

        # CREATE DATABASE (DDL, separate commit)
        with self._db_factory() as db:
            db.commit()
            db.execute(text(f"DROP DATABASE IF EXISTS `{branch_db}`"))
            db.commit()
            db.execute(text(f"CREATE DATABASE `{branch_db}`"))
            db.commit()

        try:
            # Branch tables + INSERT mem_branches in one commit
            from matrixone.branch_builder import create_table_branch

            with self._db_factory() as db:
                for table in self._BRANCH_TABLES:
                    try:
                        if snap_name:
                            stmt = create_table_branch(
                                f"{branch_db}.{table}"
                            ).from_table(f"{src_db}.{table}", snapshot=snap_name)
                            db.execute(text(str(stmt)))
                        else:
                            # timestamp mode — SDK doesn't support yet
                            db.execute(
                                text(
                                    f"data branch create table {branch_db}.{table} "
                                    f'from {src_db}.{table}{{timestamp="{from_timestamp}"}}'
                                )
                            )
                    except Exception as e:
                        if table == "mem_memories":
                            raise
                        logger.debug("Branch table %s failed: %s", table, e)
                base_label = snap_name or from_timestamp or "current"
                db.execute(
                    text(
                        "INSERT INTO mem_branches (branch_id, user_id, name, branch_db, base_snapshot, status, created_at) "
                        "VALUES (:bid, :uid, :name, :bdb, :snap, 'active', NOW())"
                    ),
                    {
                        "bid": branch_id,
                        "uid": user_id,
                        "name": safe,
                        "bdb": branch_db,
                        "snap": base_label,
                    },
                )
                db.commit()
        except Exception:
            with self._db_factory() as db:
                db.commit()
                db.execute(text(f"DROP DATABASE IF EXISTS `{branch_db}`"))
                db.commit()
            raise

        return {"name": safe, "branch_db": branch_db, "branch_id": branch_id}

    def branch_list(self, user_id: str) -> list[dict]:
        with self._db_factory() as db:
            rows = db.execute(
                text(
                    "SELECT branch_id, name, branch_db, created_at "
                    "FROM mem_branches WHERE user_id = :uid AND status = 'active' "
                    "ORDER BY created_at"
                ),
                {"uid": user_id},
            ).fetchall()
        active = self._get_active_branch(user_id)  # Call once, not per row
        result = [
            {
                "name": "main",
                "branch_db": self._source_db_name(),
                "active": active == "main",
            }
        ]
        for r in rows:
            result.append(
                {"name": r.name, "branch_db": r.branch_db, "active": active == r.name}
            )
        return result

    def branch_checkout(self, user_id: str, name: str) -> dict:
        if name == "main":
            self._set_active_branch(user_id, "main")
            return {"active_branch": "main"}
        safe = self._sanitize_name(name)
        with self._db_factory() as db:
            row = db.execute(
                text(
                    "SELECT name FROM mem_branches WHERE user_id = :uid AND name = :name AND status = 'active'"
                ),
                {"uid": user_id, "name": safe},
            ).fetchone()
        if not row:
            return {"error": f"Branch '{name}' not found"}
        self._set_active_branch(user_id, safe)
        return {"active_branch": safe}

    def branch_delete(self, user_id: str, name: str) -> dict:
        if name == "main":
            return {"error": "Cannot delete main"}
        safe = self._sanitize_name(name)
        with self._db_factory() as db:
            row = db.execute(
                text(
                    "SELECT branch_id, branch_db FROM mem_branches "
                    "WHERE user_id = :uid AND name = :name AND status = 'active'"
                ),
                {"uid": user_id, "name": safe},
            ).fetchone()
        if not row:
            return {"error": f"Branch '{name}' not found"}

        # delete_table_branch + mark deleted in one commit
        try:
            from matrixone.branch_builder import delete_table_branch

            with self._db_factory() as db:
                for table in self._BRANCH_TABLES:
                    try:
                        stmt = delete_table_branch(f"{row.branch_db}.{table}")
                        db.execute(text(str(stmt)))
                    except Exception:
                        pass
                db.execute(
                    text(
                        "UPDATE mem_branches SET status = 'deleted', updated_at = NOW() WHERE branch_id = :bid"
                    ),
                    {"bid": row.branch_id},
                )
                db.commit()
        except Exception:
            logger.warning("Failed to delete branch tables %s", row.branch_db)

        # DROP DATABASE is DDL, must be separate
        try:
            with self._db_factory() as db:
                db.commit()
                db.execute(text(f"DROP DATABASE IF EXISTS `{row.branch_db}`"))
                db.commit()
        except Exception:
            logger.warning("Failed to drop branch DB %s", row.branch_db)

        if self._get_active_branch(user_id) == safe:
            self._set_active_branch(user_id, "main")
        # Evict cached factory and dispose its engine (points to dropped DB).
        self._evict_branch_cache(user_id, safe)
        return {"deleted": safe}

    def _get_diff_rows(self, branch_db: str, src_db: str, limit: int):
        """Get diff rows via SDK. Returns (total, rows). limit=0 means count only.

        Uses MatrixOne's native diff_table_branch API for efficient comparison.
        Errors are logged but don't fail the merge — we fall back to conservative merge.
        """
        from matrixone.branch_builder import diff_table_branch

        try:
            stmt_count = (
                diff_table_branch(f"{branch_db}.mem_memories")
                .against(f"{src_db}.mem_memories")
                .output_count()
            )
            with self._db_factory() as db:
                db.commit()
                total = db.execute(text(str(stmt_count))).scalar() or 0
            if total == 0 or limit == 0:
                return total, []
            stmt_rows = (
                diff_table_branch(f"{branch_db}.mem_memories")
                .against(f"{src_db}.mem_memories")
                .output_limit(limit)
            )
            with self._db_factory() as db:
                db.commit()
                rows = db.execute(text(str(stmt_rows))).fetchall()
            return total, rows
        except Exception as e:
            logger.warning(
                "diff_table_branch failed: %s. Falling back to conservative merge.", e
            )
            return 0, []

    def _resolve_branch(self, user_id: str, name: str):
        """Lookup active branch. Returns (branch_db, error_dict)."""
        safe = self._sanitize_name(name)
        with self._db_factory() as db:
            row = db.execute(
                text(
                    "SELECT branch_id, branch_db FROM mem_branches "
                    "WHERE user_id = :uid AND name = :name AND status = 'active'"
                ),
                {"uid": user_id, "name": safe},
            ).fetchone()
        if not row:
            return None, {"error": f"Branch '{name}' not found"}
        # Verify branch DB exists
        with self._db_factory() as db:
            exists = db.execute(
                text(
                    "SELECT COUNT(*) FROM information_schema.SCHEMATA WHERE SCHEMA_NAME = :db"
                ),
                {"db": row.branch_db},
            ).scalar()
        if not exists:
            return None, {"error": f"Branch DB '{row.branch_db}' no longer exists"}
        return row.branch_db, None

    _MAX_MERGE_CHANGES: ClassVar[int] = (
        5000  # Safety limit: prevent accidental large merges.
    )
    _CONFLICT_COSINE_THRESHOLD: ClassVar[float] = (
        0.9  # Single source of truth for conflict detection threshold.
    )

    def branch_merge(self, user_id: str, source: str, strategy: str) -> dict:
        """Merge branch into main. All SQL — no rows pulled to Python.

        1. SDK diff count → safety check (prevent >5000 changes)
        2. Bulk INSERT...SELECT for non-conflicting new memories
        3. For replace: UPDATE via subquery for conflicts

        Design rationale for _MAX_MERGE_CHANGES=5000:
        - Cosine similarity on 5000 memories takes ~10-30s (acceptable)
        - Prevents accidental merges of massive branches
        - User can split large branches into smaller ones
        """

        branch_db, err = self._resolve_branch(user_id, source)
        if err:
            return err
        src_db = self._source_db_name()

        # 1. Safety check — count memory diff only.
        # total==0 does NOT mean the branch is empty: it may have graph-only changes.
        # We never short-circuit here; graph nodes/edges are always merged below.
        total, _ = self._get_diff_rows(branch_db, src_db, limit=0)
        if total > self._MAX_MERGE_CHANGES:
            return {
                "error": f"Too many changes ({total}). Max {self._MAX_MERGE_CHANGES}. Reduce branch scope."
            }

        with self._db_factory() as db:
            # 2. Bulk INSERT non-conflicting new memories (one SQL).
            # Preserve b.memory_id so the row is idempotent across repeated merges
            # and so mem_edit_log.target_ids references remain valid.
            # Use result.rowcount — avoids a redundant COUNT(*) that would repeat
            # the same expensive cosine scan.
            insert_result = db.execute(
                text(f"""
                INSERT INTO mem_memories (memory_id, user_id, content, memory_type,
                    initial_confidence, trust_tier, embedding, source_event_ids,
                    is_active, observed_at, created_at, updated_at)
                SELECT b.memory_id, b.user_id, b.content, b.memory_type,
                    b.initial_confidence, b.trust_tier, b.embedding, b.source_event_ids,
                    1, b.observed_at, NOW(), NOW()
                FROM `{branch_db}`.mem_memories b
                WHERE b.user_id = :uid AND b.is_active = 1
                AND NOT EXISTS (
                    SELECT 1 FROM mem_memories m WHERE m.memory_id = b.memory_id AND m.is_active = 1
                )
                AND NOT EXISTS (
                    SELECT 1 FROM mem_memories m
                    WHERE m.user_id = :uid AND m.is_active = 1
                    AND m.memory_type = b.memory_type
                    AND b.embedding IS NOT NULL AND m.embedding IS NOT NULL
                    AND cosine_similarity(m.embedding, b.embedding) > :threshold
                )
            """),
                {"uid": user_id, "threshold": self._CONFLICT_COSINE_THRESHOLD},
            )
            inserted = insert_result.rowcount  # type: ignore[attr-defined]

            # 3. Handle conflicts
            replaced = 0
            skipped = 0
            conflict_where = f"""
                FROM `{branch_db}`.mem_memories b
                WHERE b.user_id = :uid AND b.is_active = 1
                AND NOT EXISTS (SELECT 1 FROM mem_memories m2 WHERE m2.memory_id = b.memory_id AND m2.is_active = 1)
                AND EXISTS (
                    SELECT 1 FROM mem_memories m
                    WHERE m.user_id = :uid AND m.is_active = 1
                    AND m.memory_type = b.memory_type
                    AND b.embedding IS NOT NULL AND m.embedding IS NOT NULL
                    AND cosine_similarity(m.embedding, b.embedding) > :threshold
                )
            """
            conflict_count = (
                db.execute(
                    text(f"SELECT COUNT(*) {conflict_where}"),
                    {"uid": user_id, "threshold": self._CONFLICT_COSINE_THRESHOLD},
                ).scalar()
                or 0
            )
            if strategy == "replace" and conflict_count > 0:
                db.execute(
                    text(f"""
                    UPDATE mem_memories m
                    SET m.content = (
                        SELECT b.content FROM `{branch_db}`.mem_memories b
                        WHERE b.user_id = :uid AND b.is_active = 1
                        AND b.memory_type = m.memory_type
                        AND b.embedding IS NOT NULL
                        AND cosine_similarity(m.embedding, b.embedding) > :threshold
                        AND NOT EXISTS (SELECT 1 FROM mem_memories m2 WHERE m2.memory_id = b.memory_id AND m2.is_active = 1)
                        LIMIT 1
                    ),
                    m.embedding = (
                        SELECT b.embedding FROM `{branch_db}`.mem_memories b
                        WHERE b.user_id = :uid AND b.is_active = 1
                        AND b.memory_type = m.memory_type
                        AND b.embedding IS NOT NULL
                        AND cosine_similarity(m.embedding, b.embedding) > :threshold
                        AND NOT EXISTS (SELECT 1 FROM mem_memories m2 WHERE m2.memory_id = b.memory_id AND m2.is_active = 1)
                        LIMIT 1
                    ),
                    m.updated_at = NOW()
                    WHERE m.user_id = :uid AND m.is_active = 1
                    AND EXISTS (
                        SELECT 1 FROM `{branch_db}`.mem_memories b
                        WHERE b.user_id = :uid AND b.is_active = 1
                        AND b.memory_type = m.memory_type
                        AND b.embedding IS NOT NULL
                        AND cosine_similarity(m.embedding, b.embedding) > :threshold
                        AND NOT EXISTS (SELECT 1 FROM mem_memories m2 WHERE m2.memory_id = b.memory_id AND m2.is_active = 1)
                    )
                """),
                    {"uid": user_id, "threshold": self._CONFLICT_COSINE_THRESHOLD},
                )
                replaced = conflict_count
            else:
                skipped = conflict_count

            # 4. Merge graph nodes (append only — skip existing node_ids).
            # Use INSERT+rowcount, not COUNT+INSERT, to avoid TOCTOU and double scan.
            node_result = db.execute(
                text(f"""
                INSERT INTO memory_graph_nodes (
                    node_id, user_id, node_type, content, entity_type, embedding,
                    event_id, memory_id, session_id,
                    confidence, trust_tier, importance,
                    source_nodes, conflicts_with, conflict_resolution,
                    access_count, cross_session_count,
                    is_active, superseded_by, created_at
                )
                SELECT
                    b.node_id, b.user_id, b.node_type, b.content, b.entity_type, b.embedding,
                    b.event_id, b.memory_id, b.session_id,
                    b.confidence, b.trust_tier, b.importance,
                    b.source_nodes, b.conflicts_with, b.conflict_resolution,
                    b.access_count, b.cross_session_count,
                    b.is_active, b.superseded_by, b.created_at
                FROM `{branch_db}`.memory_graph_nodes b
                WHERE b.user_id = :uid AND b.is_active = 1
                AND NOT EXISTS (
                    SELECT 1 FROM memory_graph_nodes m WHERE m.node_id = b.node_id
                )
            """),
                {"uid": user_id},
            )
            graph_nodes_merged = node_result.rowcount  # type: ignore[attr-defined]

            # 5. Merge graph edges (append only — skip existing src+tgt+type).
            # Same INSERT+rowcount pattern.
            edge_result = db.execute(
                text(f"""
                INSERT INTO memory_graph_edges (
                    source_id, target_id, edge_type, weight, user_id
                )
                SELECT
                    b.source_id, b.target_id, b.edge_type, b.weight, b.user_id
                FROM `{branch_db}`.memory_graph_edges b
                WHERE b.user_id = :uid
                AND NOT EXISTS (
                    SELECT 1 FROM memory_graph_edges m
                    WHERE m.source_id = b.source_id
                    AND m.target_id = b.target_id
                    AND m.edge_type = b.edge_type
                )
            """),
                {"uid": user_id},
            )
            graph_edges_merged = edge_result.rowcount  # type: ignore[attr-defined]

            db.commit()

        return {
            "inserted": inserted,
            "replaced": replaced,
            "merged": inserted + replaced,  # total for backward compat
            "skipped": skipped,
            "graph_nodes_merged": graph_nodes_merged,
            "graph_edges_merged": graph_edges_merged,
            "source": source,
        }

    def _detect_conflicts(
        self, branch_db: str, user_id: str, memory_ids: list[str]
    ) -> set[str]:
        """Find branch memories that semantically conflict with main (cosine > 0.9).

        Uses a single SQL query with JOIN to batch-check all candidate IDs
        against main's active memories. Returns the subset of memory_ids that
        have a near-duplicate in main.
        """
        if not memory_ids:
            return set()
        placeholders = ", ".join(f":id{i}" for i in range(len(memory_ids)))
        params: dict[str, Any] = {"uid": user_id}
        for i, mid in enumerate(memory_ids):
            params[f"id{i}"] = mid
        try:
            with self._db_factory() as db:
                rows = db.execute(
                    text(f"""
                    SELECT DISTINCT b.memory_id
                    FROM `{branch_db}`.mem_memories b
                    JOIN mem_memories m
                      ON m.user_id = :uid AND m.is_active = 1
                      AND m.memory_type = b.memory_type
                      AND m.embedding IS NOT NULL
                      AND cosine_similarity(m.embedding, b.embedding) > :threshold
                    WHERE b.memory_id IN ({placeholders})
                      AND b.embedding IS NOT NULL
                """),
                    {**params, "threshold": self._CONFLICT_COSINE_THRESHOLD},
                ).fetchall()
            return {r.memory_id for r in rows}
        except Exception as e:
            logger.warning("_detect_conflicts failed: %s", e)
            return set()

    def branch_diff(self, user_id: str, source: str, limit: int = 200) -> dict:
        """Diff branch against main using SDK diff + semantic conflict detection."""
        branch_db, err = self._resolve_branch(user_id, source)
        if err:
            return err
        src_db = self._source_db_name()

        total, rows = self._get_diff_rows(branch_db, src_db, limit)
        if total == 0:
            return {"source": source, "total": 0, "changes": [], "truncated": False}

        # Classify each change
        changes: list[dict] = []
        inserts_with_emb_ids: list[str] = []
        inserts_with_emb_entries: list[dict] = []

        for r in rows:
            m = dict(r._mapping)
            flag = m.get("flag", "UNKNOWN")
            entry = {
                "memory_id": m.get("memory_id"),
                "content": (m.get("content") or "")[:200],
                "memory_type": m.get("memory_type"),
                "flag": flag,
            }
            if flag in ("DELETE", "UPDATE"):
                entry["semantic"] = "removed" if flag == "DELETE" else "modified"
                changes.append(entry)
            elif flag == "INSERT":
                if m.get("embedding") is None:
                    entry["semantic"] = "new (no embedding)"
                    changes.append(entry)
                else:
                    inserts_with_emb_ids.append(m["memory_id"])
                    inserts_with_emb_entries.append(entry)
            else:
                entry["semantic"] = flag.lower()
                changes.append(entry)

        # Batch semantic conflict detection
        conflict_ids: set[str] = set()
        if inserts_with_emb_ids:
            conflict_ids = self._detect_conflicts(
                branch_db, user_id, inserts_with_emb_ids
            )
        for entry in inserts_with_emb_entries:
            entry["semantic"] = (
                "conflict" if entry["memory_id"] in conflict_ids else "new"
            )
            changes.append(entry)

        summary: dict[str, int] = {}
        for c in changes:
            s = c["semantic"]
            summary[s] = summary.get(s, 0) + 1

        return {
            "source": source,
            "total": total,
            "truncated": total > limit,
            "summary": summary,
            "changes": changes,
        }


class HTTPBackend(MemoryBackend):
    """Proxy to memory service REST API — for remote mode."""

    def __init__(self, api_url: str, token: str | None = None) -> None:
        import httpx

        headers = {"Authorization": f"Bearer {token}"} if token else {}
        self._client = httpx.Client(
            base_url=api_url.rstrip("/"), headers=headers, timeout=30
        )

    def store(
        self, user_id: str, content: str, memory_type: str, session_id: str | None
    ) -> dict:
        r = self._client.post(
            "/v1/memories",
            json={
                "content": content,
                "memory_type": memory_type,
                "session_id": session_id,
            },
        )
        r.raise_for_status()
        return r.json()

    def retrieve(
        self, user_id: str, query: str, top_k: int, session_id: str | None = None
    ) -> list[dict]:
        payload: dict[str, Any] = {"query": query, "top_k": top_k}
        # Only include session_id in payload if provided (not None).
        # This allows the remote API to distinguish between "no session context" (None)
        # and "empty session context" (""), enabling proper cross-session retrieval behavior.
        if session_id:
            payload["session_id"] = session_id
        r = self._client.post("/v1/memories/retrieve", json=payload)
        r.raise_for_status()
        return r.json()

    def correct(
        self, user_id: str, memory_id: str, new_content: str, reason: str
    ) -> dict:
        r = self._client.put(
            f"/v1/memories/{memory_id}/correct",
            json={"new_content": new_content, "reason": reason},
        )
        r.raise_for_status()
        return r.json()

    def correct_by_query(
        self, user_id: str, query: str, new_content: str, reason: str
    ) -> dict:
        r = self._client.post(
            "/v1/memories/correct",
            json={"query": query, "new_content": new_content, "reason": reason},
        )
        if r.status_code == 404:
            return {
                "error": "no_match",
                "message": f"No memory found matching '{query}'",
            }
        r.raise_for_status()
        return r.json()

    def purge(
        self, user_id: str, memory_id: str | None, topic: str | None, reason: str
    ) -> dict:
        if topic:
            # Search then purge each match.  Collect partial results so a
            # mid-batch failure doesn't lose the count of already-purged items.
            hits = self.search(user_id, topic, top_k=50)
            ids = [h["memory_id"] for h in hits]
            purged = 0
            errors: list[str] = []
            for mid in ids:
                try:
                    r = self._client.delete(
                        f"/v1/memories/{mid}",
                        params={"reason": reason or f"topic purge: {topic}"},
                    )
                    r.raise_for_status()
                    purged += r.json().get("purged", 1)
                except Exception as e:
                    errors.append(f"{mid}: {e}")
            result: dict = {"purged": purged}
            if errors:
                result["errors"] = errors
            return result
        elif memory_id:
            r = self._client.delete(
                f"/v1/memories/{memory_id}", params={"reason": reason}
            )
            r.raise_for_status()
            return r.json()
        return {"purged": 0}

    def profile(self, user_id: str) -> dict:
        r = self._client.get(f"/v1/profiles/{user_id}")
        r.raise_for_status()
        return r.json()

    def search(self, user_id: str, query: str, top_k: int) -> list[dict]:
        r = self._client.post(
            "/v1/memories/search", json={"query": query, "top_k": top_k}
        )
        r.raise_for_status()
        return r.json()

    def governance(self, user_id: str, force: bool = False) -> dict:
        r = self._client.post(
            "/v1/memories/governance", json={"user_id": user_id, "force": force}
        )
        r.raise_for_status()
        return r.json()

    def consolidate(self, user_id: str, force: bool = False) -> dict:
        r = self._client.post(
            "/v1/memories/consolidate", json={"user_id": user_id, "force": force}
        )
        r.raise_for_status()
        return r.json()

    def reflect(self, user_id: str, force: bool = False) -> dict:
        r = self._client.post(
            "/v1/memories/reflect", json={"user_id": user_id, "force": force}
        )
        r.raise_for_status()
        return r.json()

    def extract_entities(self, user_id: str) -> dict:
        # user_id is resolved server-side from the API key in Authorization header
        r = self._client.post("/v1/extract-entities")
        r.raise_for_status()
        return r.json()

    def get_reflect_candidates(self, user_id: str) -> dict:
        r = self._client.post("/v1/reflect/candidates")
        r.raise_for_status()
        return r.json()

    def get_entity_candidates(self, user_id: str) -> dict:
        r = self._client.post("/v1/extract-entities/candidates")
        r.raise_for_status()
        return r.json()

    def link_entities(self, user_id: str, entities: list[dict]) -> dict:
        r = self._client.post("/v1/extract-entities/link", json={"entities": entities})
        r.raise_for_status()
        return r.json()

    def rebuild_index(self, table: str) -> str:
        r = self._client.post("/v1/memories/rebuild-index", json={"table": table})
        r.raise_for_status()
        return r.json().get("message", str(r.json()))

    def health_warnings(self, user_id: str) -> list[str]:
        return []  # Not available via HTTP yet

    def snapshot_create(self, user_id: str, name: str, description: str) -> dict:
        return {"error": "Not available via HTTP"}

    def snapshot_list(self, user_id: str) -> list[dict]:
        return []

    def snapshot_delete(
        self,
        user_id: str,
        names: list[str] | None = None,
        prefix: str | None = None,
        older_than: str | None = None,
    ) -> dict:
        return {"error": "Not available via HTTP"}

    def snapshot_rollback(self, user_id: str, name: str) -> dict:
        return {"error": "Not available via HTTP"}

    def branch_create(
        self,
        user_id: str,
        name: str,
        from_snapshot: str | None,
        from_timestamp: str | None = None,
    ) -> dict:
        return {"error": "Not available via HTTP"}

    def branch_list(self, user_id: str) -> list[dict]:
        return []

    def branch_checkout(self, user_id: str, name: str) -> dict:
        return {"error": "Not available via HTTP"}

    def branch_delete(self, user_id: str, name: str) -> dict:
        return {"error": "Not available via HTTP"}

    def branch_merge(self, user_id: str, source: str, strategy: str) -> dict:
        return {"error": "Not available via HTTP"}

    def branch_diff(self, user_id: str, source: str, limit: int = 200) -> dict:
        return {"error": "Not available via HTTP"}


# ── MCP Server ────────────────────────────────────────────────────────


def create_server(backend: MemoryBackend, default_user: str = "default") -> FastMCP:
    """Create MCP server with memory tools."""

    server = FastMCP(
        "memoria-lite",
        instructions=(
            "Memoria Lite — persistent memory across conversations (local single-user mode). "
            "\n\n"
            "MANDATORY RULES:\n"
            "1. ALWAYS call memory_retrieve with the user's first message BEFORE responding.\n"
            "   If the response includes ⚠️ Memory health warnings, inform the user and offer to help.\n"
            "2. AFTER each response, call memory_store for any new fact, preference, or decision.\n"
            "\n"
            "CRUD: memory_store, memory_retrieve, memory_correct, memory_purge, memory_profile, memory_search.\n"
            "memory_purge supports single ID, comma-separated batch IDs (e.g. 'id1,id2,id3'), or topic-based bulk delete.\n"
            "MAINTENANCE (only when user asks): memory_governance, memory_consolidate, memory_reflect, memory_rebuild_index.\n"
            "\n"
            "memory_store types: semantic (default), profile, procedural, working, tool_result."
        ),
    )

    def _user(user_id: str | None) -> str:
        return user_id or default_user

    def _has_internal_llm() -> bool:
        """Check if Memoria has its own LLM configured."""
        from memoria.core.llm import get_llm_client

        return get_llm_client() is not None

    def _json_dumps(obj: dict) -> str:
        import json

        return json.dumps(obj, ensure_ascii=False)

    def _with_warning(msg: str, result: dict) -> str:
        """Append warning from backend result dict to a tool response string."""
        w = result.get("warning")
        return f"{msg}{MSG_WARNING_PREFIX}{w}" if w else msg

    def _format(result: Any, fmt: str) -> str:
        """Return JSON string if fmt='json', otherwise return result as-is (text)."""
        if fmt == "json":
            return (
                _json_dumps(result) if isinstance(result, (dict, list)) else str(result)
            )
        return str(result)

    _VALID_MEMORY_TYPES = {
        "profile",
        "semantic",
        "procedural",
        "working",
        "tool_result",
    }
    _VALID_REBUILD_TABLES = {"mem_memories", "memory_graph_nodes"}

    @server.tool()
    def memory_store(
        content: str,
        memory_type: str = "semantic",
        user_id: str | None = None,
        session_id: str | None = None,
        format: str = "text",
    ) -> str:
        """Store a memory. Use for facts, preferences, decisions, or corrections the user shares.

        Args:
            content: The memory content to store.
            memory_type: One of: profile, semantic, procedural, working, tool_result. Default: semantic.
            user_id: User ID (optional, uses default if omitted).
            session_id: Session context (optional).
            format: 'text' (default) or 'json' for structured response with memory_id, content, warning fields.
        """
        if not content or not content.strip():
            err = "content is required and cannot be empty."
            return (
                _format({"status": "error", "error": err}, "json")
                if format == "json"
                else err
            )
        if memory_type not in _VALID_MEMORY_TYPES:
            err = f"Invalid memory_type '{memory_type}'. Must be one of: {', '.join(sorted(_VALID_MEMORY_TYPES))}"
            return (
                _format({"status": "error", "error": err}, "json")
                if format == "json"
                else err
            )
        result = backend.store(_user(user_id), content, memory_type, session_id)
        if format == "json":
            return _format(
                {
                    "status": "ok",
                    "memory_id": result["memory_id"],
                    "content": result["content"],
                    **({"warning": result["warning"]} if "warning" in result else {}),
                },
                "json",
            )
        return _with_warning(
            MSG_STORED.format(memory_id=result["memory_id"], content=result["content"]),
            result,
        )

    @server.tool()
    def memory_retrieve(
        query: str,
        top_k: int = 5,
        user_id: str | None = None,
        session_id: str | None = None,
        format: str = "text",
    ) -> str:
        """Retrieve relevant memories for a query. Call this at conversation start or when context is needed.

        Args:
            query: What to search for in memories.
            top_k: Max number of memories to return (default 5).
            user_id: User ID (optional).
            session_id: Session context (optional). When set, prioritizes memories from this session.
                When None, searches across all sessions (include_cross_session=True).
                When set, the underlying retrieval strategy ranks session-scoped memories higher.
            format: 'text' (default) or 'json' for structured response with memory_id, type, content per item.
        """
        uid = _user(user_id)
        top_k = max(1, min(top_k, 100))
        results = backend.retrieve(uid, query, top_k, session_id=session_id)
        warnings = backend.health_warnings(uid)
        if format == "json":
            return _format(
                {
                    "status": "ok",
                    "count": len(results),
                    "memories": [
                        {
                            "memory_id": r["memory_id"],
                            "type": r.get("type", "fact"),
                            "content": r["content"],
                        }
                        for r in results
                    ],
                    **({"warnings": warnings} if warnings else {}),
                },
                "json",
            )
        parts: list[str] = []
        if not results:
            parts.append(MSG_RETRIEVE_EMPTY)
        else:
            lines = [
                MSG_RETRIEVE_ITEM.format(
                    type=r.get("type", "fact"), content=r["content"]
                )
                for r in results
            ]
            parts.append(
                MSG_RETRIEVE_FOUND.format(count=len(results)) + "\n".join(lines)
            )
        if warnings:
            parts.append(MSG_HEALTH_HEADER + "\n".join(f"- {w}" for w in warnings))
        return "\n".join(parts)

    @server.tool()
    def memory_correct(
        memory_id: str | None = None,
        new_content: str = "",
        reason: str = "",
        query: str | None = None,
        user_id: str | None = None,
        format: str = "text",
    ) -> str:
        """Correct an existing memory with updated information.

        Args:
            memory_id: ID of the memory to correct. Either memory_id or query is required.
            new_content: The corrected content.
            reason: Why the correction is needed.
            query: Search query to find the memory to correct. Uses semantic search to find the best match.
            user_id: User ID (optional).
            format: 'text' (default) or 'json' for structured response with memory_id, content fields.
        """
        if not new_content:
            if format == "json":
                return _format(
                    {"status": "error", "error": MSG_CORRECT_NO_CONTENT}, "json"
                )
            return MSG_CORRECT_NO_CONTENT
        uid = _user(user_id)
        if query and not memory_id:
            result = backend.correct_by_query(uid, query, new_content, reason)
            if result.get("error") == "no_match":
                if format == "json":
                    return _format(
                        {"status": "error", "error": result["message"]}, "json"
                    )
                return result["message"]
            if format == "json":
                return _format(
                    {
                        "status": "ok",
                        "memory_id": result["memory_id"],
                        "content": result["content"],
                        "matched_content": result.get("matched_content", ""),
                        **(
                            {"warning": result["warning"]}
                            if "warning" in result
                            else {}
                        ),
                    },
                    "json",
                )
            matched = result.get("matched_content", "")
            msg = MSG_CORRECTED_BY_QUERY.format(
                matched=matched,
                memory_id=result["memory_id"],
                content=result["content"],
            )
        elif not memory_id:
            if format == "json":
                return _format(
                    {"status": "error", "error": MSG_CORRECT_NO_TARGET}, "json"
                )
            return MSG_CORRECT_NO_TARGET
        else:
            result = backend.correct(uid, memory_id, new_content, reason)
            if format == "json":
                return _format(
                    {
                        "status": "ok",
                        "memory_id": result["memory_id"],
                        "content": result["content"],
                        **(
                            {"warning": result["warning"]}
                            if "warning" in result
                            else {}
                        ),
                    },
                    "json",
                )
            msg = MSG_CORRECTED_BY_ID.format(
                memory_id=result["memory_id"], content=result["content"]
            )
        return _with_warning(msg, result)

    @server.tool()
    def memory_purge(
        memory_id: str | None = None,
        topic: str | None = None,
        reason: str = "",
        user_id: str | None = None,
        format: str = "text",
    ) -> str:
        """Delete memories. Use memory_id for a single memory, or topic to bulk-delete all memories matching a keyword.

        Args:
            memory_id: ID of a specific memory to delete. Supports comma-separated batch (e.g. "id1,id2,id3").
            topic: Keyword/topic — finds and deletes all matching memories.
            reason: Why it should be deleted.
            user_id: User ID (optional).
            format: 'text' (default) or 'json' for structured response with purged count.
        """
        if not memory_id and not topic:
            if format == "json":
                return _format(
                    {"status": "error", "error": MSG_PURGE_NO_TARGET}, "json"
                )
            return MSG_PURGE_NO_TARGET
        result = backend.purge(_user(user_id), memory_id, topic, reason)
        if format == "json":
            return _format({"status": "ok", "purged": result["purged"]}, "json")
        return MSG_PURGED.format(count=result["purged"])

    @server.tool()
    def memory_profile(
        user_id: str | None = None,
    ) -> str:
        """Get the user's memory-derived profile summary.

        Args:
            user_id: User ID (optional).
        """
        result = backend.profile(_user(user_id))
        profile = (
            result.get("profile")
            or "No profile available yet. Use memory_search to browse all stored memories."
        )
        return f"Profile for {result['user_id']}:\n{profile}"

    @server.tool()
    def memory_search(
        query: str,
        top_k: int = 10,
        user_id: str | None = None,
        format: str = "text",
    ) -> str:
        """Semantic search over all memories.

        Args:
            query: Search query.
            top_k: Max results (default 10).
            user_id: User ID (optional).
            format: 'text' (default) or 'json' for structured response with memory_id, type, content per item.
        """
        results = backend.search(_user(user_id), query, max(1, min(top_k, 100)))
        if format == "json":
            return _format(
                {
                    "status": "ok",
                    "count": len(results),
                    "memories": [
                        {
                            "memory_id": r["memory_id"],
                            "type": r.get("type", "fact"),
                            "content": r["content"],
                        }
                        for r in results
                    ],
                },
                "json",
            )
        if not results:
            return MSG_SEARCH_EMPTY
        lines = [
            MSG_SEARCH_ITEM.format(
                type=r.get("type", "fact"),
                memory_id=r["memory_id"],
                content=r["content"],
            )
            for r in results
        ]
        return MSG_SEARCH_FOUND.format(count=len(results)) + "\n".join(lines)

    @server.tool()
    def memory_governance(
        user_id: str | None = None,
        force: bool = False,
    ) -> str:
        """Run memory governance: quarantine low-confidence memories, clean stale data.

        Do NOT call proactively. Only call when user explicitly asks to
        "clean up memories", "run maintenance", or "check memory health".
        Has a 1-hour cooldown per user. Use force=True only if user insists.

        Args:
            user_id: User ID (optional).
            force: Skip cooldown (only when user explicitly requests).
        """
        result = backend.governance(_user(user_id), force=force)
        if result.get("skipped"):
            return f"{MSG_GOVERNANCE_SKIPPED}{result['cooldown_remaining_s']}s remaining). Last result: {', '.join(f'{k}={v}' for k, v in result.items() if k not in ('skipped', 'cooldown_remaining_s', 'vector_index_health', 'snapshot_health'))}"
        health = result.pop("vector_index_health", {})
        snap_health = result.pop("snapshot_health", {})
        parts = [f"{k}={v}" for k, v in result.items()]
        msg = f"{MSG_GOVERNANCE_DONE}{', '.join(parts)}"
        for table, h in health.items():
            if h.get("needs_rebuild") and not h.get("rebuilt"):
                msg += f"\n{MSG_INDEX_NEEDS_REBUILD.format(table=table)} (rows={h.get('total_rows')}, centroids={h['centroids']}, ratio={h.get('ratio')})"
            elif h.get("rebuilt"):
                msg += f"\n{MSG_INDEX_REBUILT.format(table=table)}"
            elif h.get("rebuild_error"):
                msg += f"\n❌ {table}: IVF rebuild failed: {h['rebuild_error']}"
        if snap_health:
            total = snap_health.get("total", 0)
            auto_ratio = snap_health.get("auto_ratio", 0.0)
            msg += f"\n📸 Snapshots: {total} total, {auto_ratio:.0%} auto-generated."
            if auto_ratio > 0.5 and total > 10:
                msg += " Consider cleaning up with memory_snapshot_delete(prefix=...) or memory_snapshot_delete(older_than=...)."
        return msg

    @server.tool()
    def memory_consolidate(
        user_id: str | None = None,
        force: bool = False,
    ) -> str:
        """Run graph consolidation: detect contradicting memories, fix orphaned nodes, manage trust tiers.

        Do NOT call proactively. Only call when user explicitly asks to
        "check for conflicts", "consolidate memories", or "fix memory graph".
        Has a 30-minute cooldown per user. Use force=True only if user insists.

        Args:
            user_id: User ID (optional).
            force: Skip cooldown (only when user explicitly requests).
        """
        result = backend.consolidate(_user(user_id), force=force)
        if result.get("skipped"):
            return f"{MSG_CONSOLIDATION_SKIPPED}{result['cooldown_remaining_s']}s remaining)."
        parts = [f"{k}={v}" for k, v in result.items()]
        return f"{MSG_CONSOLIDATION_DONE}{', '.join(parts)}"

    @server.tool()
    def memory_reflect(
        user_id: str | None = None,
        force: bool = False,
        mode: str = "auto",
    ) -> str:
        """Analyze memory clusters and synthesize high-level insights (scene nodes).

        Do NOT call proactively. Only call when user explicitly asks to
        "reflect on memories", "find patterns", or "summarize what you know".
        Has a 2-hour cooldown per user. Use force=True only if user insists.

        Args:
            user_id: User ID (optional).
            force: Skip cooldown (only when user explicitly requests).
            mode: 'auto' (use internal LLM if configured, else return candidates),
                  'internal' (use Memoria's LLM — fails if not configured),
                  'candidates' (return raw memory clusters for YOU to synthesize,
                  then store insights via memory_store with trust_tier='T4').
        """
        uid = _user(user_id)
        if mode == "candidates" or (mode == "auto" and not _has_internal_llm()):
            result = backend.get_reflect_candidates(uid)
            clusters = result.get("candidates", [])
            if not clusters:
                return MSG_REFLECTION_NO_CANDIDATES
            parts = []
            for i, c in enumerate(clusters, 1):
                mems = "\n".join(
                    f"  - [{m['type']}] {m['content']}" for m in c["memories"]
                )
                parts.append(
                    f"Cluster {i} ({c['signal']}, importance={c['importance']}):\n{mems}"
                )
            return (
                "Here are memory clusters for reflection. Synthesize 1-2 insights per cluster, "
                "then store each via memory_store(content=..., memory_type='semantic').\n\n"
                + "\n\n".join(parts)
            )
        result = backend.reflect(uid, force=force)
        if result.get("skipped"):
            return (
                f"{MSG_REFLECTION_SKIPPED}{result['cooldown_remaining_s']}s remaining)."
            )
        if "error" in result:
            return f"Reflection failed: {result['error']}"
        return MSG_REFLECTION_DONE.format(
            scenes_created=result["scenes_created"],
            candidates_found=result["candidates_found"],
        )

    @server.tool()
    def memory_extract_entities(
        user_id: str | None = None,
        mode: str = "auto",
    ) -> str:
        """Extract named entities from memories and build entity graph.

        Call proactively when:
        - You stored ≥5 memories this session and haven't extracted yet
        - User mentions a project/technology/person not seen in previous retrieve results
        - User asks about relationships between concepts
        Do NOT call if conversation is short (<3 turns) with no new named entities.

        Lightweight regex extraction runs automatically on every memory store.
        This tool adds deeper extraction via LLM.

        Args:
            user_id: User ID (optional).
            mode: 'auto' (use internal LLM if configured, else return candidates),
                  'internal' (use Memoria's LLM — fails if not configured),
                  'candidates' (return unlinked memories for YOU to extract entities from,
                  then call memory_link_entities with the results).
        """
        uid = _user(user_id)
        if mode == "candidates" or (mode == "auto" and not _has_internal_llm()):
            result = backend.get_entity_candidates(uid)
            memories = result.get("memories", [])
            existing = result.get("existing_entities", [])
            if not memories:
                return _json_dumps(
                    {
                        "status": "complete",
                        "unlinked": 0,
                        "message": "All memories already have entity links.",
                    }
                )
            return _json_dumps(
                {
                    "status": "candidates",
                    "unlinked": len(memories),
                    "memories": memories,
                    "existing_entities": existing,
                    "instruction": "Extract named entities (people, tech, projects, repos) from each memory, then call memory_link_entities.",
                }
            )
        result = backend.extract_entities(uid)
        if "error" in result:
            return _json_dumps({"status": "error", "error": result["error"]})
        return _json_dumps(
            {
                "status": "done",
                "total_memories": result["total_memories"],
                "entities_found": result["entities_found"],
                "edges_created": result["edges_created"],
            }
        )

    @server.tool()
    def memory_link_entities(
        entities: str,
        user_id: str | None = None,
    ) -> str:
        """Write entity links from user-LLM extraction results.

        Call this after memory_extract_entities(mode='candidates') returns memories.
        You extract entities from each memory, then pass them here.

        Args:
            entities: JSON string: [{"memory_id": "...", "entities": [{"name": "python", "type": "tech"}]}]
            user_id: User ID (optional).
        """
        import json as _json

        try:
            parsed = _json.loads(entities)
        except (ValueError, TypeError):
            return _json_dumps(
                {
                    "status": "error",
                    "error": "Invalid JSON",
                    "expected_format": [
                        {
                            "memory_id": "...",
                            "entities": [{"name": "...", "type": "..."}],
                        }
                    ],
                }
            )
        if not isinstance(parsed, list) or not all(
            isinstance(x, dict) and "memory_id" in x for x in parsed
        ):
            return _json_dumps(
                {
                    "status": "error",
                    "error": "Invalid format",
                    "expected_format": [
                        {
                            "memory_id": "...",
                            "entities": [{"name": "...", "type": "..."}],
                        }
                    ],
                }
            )
        try:
            result = backend.link_entities(_user(user_id), parsed)
        except Exception as e:
            return _json_dumps({"status": "error", "error": str(e)})
        return _json_dumps(
            {
                "status": "done",
                "entities_created": result["entities_created"],
                "entities_reused": result.get("entities_reused", 0),
                "edges_created": result["edges_created"],
            }
        )

    @server.tool()
    def memory_rebuild_index(
        table: str = "mem_memories",
        user_id: str | None = None,
    ) -> str:
        """Rebuild IVF vector index for a memory table with optimal centroid count.

        Only call when memory_governance reports 'needs_rebuild=True' for a table,
        or when user explicitly asks to rebuild the vector index.
        This operation is expensive (full table scan). Do NOT call proactively.

        Args:
            table: Table to rebuild. One of: 'mem_memories', 'memory_graph_nodes'.
            user_id: User ID (optional, unused but kept for consistency).
        """
        if table not in _VALID_REBUILD_TABLES:
            return f"Error: Invalid table '{table}'. Must be one of: {', '.join(sorted(_VALID_REBUILD_TABLES))}"
        return backend.rebuild_index(table)

    @server.tool()
    def memory_capabilities() -> str:
        """List available memory tools and current backend mode.

        Call this to discover which tools are available in the current deployment.
        Local (embedded) mode has full features; remote (cloud) mode may have fewer tools.
        """
        is_embedded = not isinstance(backend, HTTPBackend)
        mode = "embedded" if is_embedded else "remote"
        tools = [
            "memory_store",
            "memory_retrieve",
            "memory_search",
            "memory_correct",
            "memory_purge",
            "memory_profile",
            "memory_capabilities",
            "memory_snapshot",
            "memory_snapshots",
            "memory_extract_entities",
            "memory_link_entities",
            "memory_consolidate",
            "memory_reflect",
        ]
        if is_embedded:
            tools.extend(
                [
                    "memory_governance",
                    "memory_rebuild_index",
                    "memory_rollback",
                    "memory_snapshot_delete",
                    "memory_branch",
                    "memory_branches",
                    "memory_checkout",
                    "memory_merge",
                    "memory_diff",
                    "memory_branch_delete",
                ]
            )
        return _json_dumps({"mode": mode, "tools": sorted(tools)})

    # ── Snapshot tools ────────────────────────────────────────────────

    def _validate_name(name: str, label: str = "name") -> str | None:
        """Return error message if name is invalid, else None."""
        if not name or not name.strip():
            return f"{label} is required and cannot be empty."
        if len(name) > 100:
            return f"{label} is too long (max 100 characters)."
        return None

    @server.tool()
    def memory_snapshot(
        name: str,
        description: str = "",
        user_id: str | None = None,
    ) -> str:
        """Create a named snapshot of current memory state.

        Args:
            name: Snapshot name (e.g. 'before_refactor').
            description: Optional description.
            user_id: User ID (optional).
        """
        err = _validate_name(name, "Snapshot name")
        if err:
            return f"Error: {err}"
        result = backend.snapshot_create(_user(user_id), name, description)
        if "error" in result:
            return f"Error: {result['error']}"
        return f"Snapshot '{name}' created."

    @server.tool()
    def memory_snapshots(
        limit: int = 20, offset: int = 0, user_id: str | None = None
    ) -> str:
        """List memory snapshots with pagination.

        Returns up to `limit` snapshots starting from `offset`, sorted newest first.
        Shows total count so the agent can inform the user and paginate if needed.

        Args:
            limit: Max snapshots to return (default 20).
            offset: Number of snapshots to skip (default 0, starts from newest).
            user_id: User ID (optional).
        """
        snaps = backend.snapshot_list(_user(user_id))
        if not snaps:
            return "No snapshots found."
        total = len(snaps)
        page = snaps[offset : offset + limit]
        lines = [f"  {s['name']} ({s['timestamp']})" for s in page]
        start = offset + 1
        end = offset + len(page)
        header = f"Found {total} snapshots (showing {start}-{end} of {total}):\n"
        return header + "\n".join(lines)

    @server.tool()
    def memory_snapshot_delete(
        names: str | None = None,
        prefix: str | None = None,
        older_than: str | None = None,
        user_id: str | None = None,
    ) -> str:
        """Delete snapshots by name(s), prefix, or age. Supports batch deletion.

        Use this to clean up auto-generated or outdated snapshots.
        Typically called after memory_governance reports high auto_ratio in snapshot_health,
        or when memory_snapshots shows too many entries.

        Exactly one of names, prefix, or older_than must be provided.

        Common prefixes:
        - "auto:" — auto-generated milestone snapshots (from governance/health checks)
        - "pre_" — safety snapshots created before purge/correct operations

        Args:
            names: Comma-separated snapshot names to delete (e.g. "snap1,snap2,snap3").
            prefix: Delete all snapshots whose name starts with this prefix (e.g. "auto:" or "pre_").
            older_than: Delete snapshots older than this ISO date (e.g. "2026-03-01").
            user_id: User ID (optional).
        """
        name_list = (
            [n.strip() for n in names.split(",") if n.strip()] if names else None
        )
        result = backend.snapshot_delete(
            _user(user_id), names=name_list, prefix=prefix, older_than=older_than
        )
        if "error" in result:
            return f"Error: {result['error']}"
        deleted = result.get("deleted", 0)
        errors = result.get("errors", [])
        msg = f"Deleted {deleted} snapshot(s)."
        if result.get("names"):
            msg += f" Names: {', '.join(result['names'][:10])}"
            if len(result["names"]) > 10:
                msg += f" ... and {len(result['names']) - 10} more"
        if errors:
            msg += f"\n⚠️ {len(errors)} error(s): {'; '.join(errors[:5])}"
        return msg

    @server.tool()
    def memory_rollback(
        name: str,
        user_id: str | None = None,
    ) -> str:
        """Restore memories to a previous snapshot. WARNING: changes after the snapshot will be lost.

        Args:
            name: Snapshot name to rollback to.
            user_id: User ID (optional).
        """
        err = _validate_name(name, "Snapshot name")
        if err:
            return f"Error: {err}"
        result = backend.snapshot_rollback(_user(user_id), name)
        if "error" in result:
            return f"Error: {result['error']}"
        return f"Rolled back to snapshot '{name}'."

    # ── Branch tools ──────────────────────────────────────────────────

    @server.tool()
    def memory_branch(
        name: str,
        from_snapshot: str | None = None,
        from_timestamp: str | None = None,
        user_id: str | None = None,
    ) -> str:
        """Create a new memory branch for isolated experimentation.

        Args:
            name: Branch name (e.g. 'eval_postgres', 'experiment_a').
            from_snapshot: Branch from a named snapshot.
            from_timestamp: Branch from a point in time (e.g. '2026-03-09 12:00:00'). Must be within last 30 minutes.
            user_id: User ID (optional).

        If neither from_snapshot nor from_timestamp is given, branches from current state.
        """
        if from_snapshot and from_timestamp:
            return "Error: specify from_snapshot or from_timestamp, not both."
        err = _validate_name(name, "Branch name")
        if err:
            return f"Error: {err}"
        result = backend.branch_create(
            _user(user_id), name, from_snapshot, from_timestamp
        )
        if "error" in result:
            return f"Error: {result['error']}"
        actual_name = result.get("name", name)
        src = ""
        if from_snapshot:
            src = f" from snapshot '{from_snapshot}'"
        elif from_timestamp:
            src = f" from timestamp '{from_timestamp}'"
        return (
            f"Branch '{actual_name}' created{src}. Use memory_checkout to switch to it."
        )

    @server.tool()
    def memory_branches(user_id: str | None = None) -> str:
        """List all memory branches.

        Args:
            user_id: User ID (optional).
        """
        branches = backend.branch_list(_user(user_id))
        if not branches:
            return "No branches."
        lines = [f"  {'* ' if b['active'] else '  '}{b['name']}" for b in branches]
        return "\n".join(lines)

    @server.tool()
    def memory_checkout(name: str, top_k: int = 50, user_id: str | None = None) -> str:
        """Switch to a different memory branch.

        Args:
            name: Branch name to switch to (or 'main').
            top_k: Max memories to show after switching (default 50).
            user_id: User ID (optional).
        """
        uid = _user(user_id)
        err = _validate_name(name, "Branch name")
        if err:
            return f"Error: {err}"
        top_k = max(1, min(top_k, 200))
        result = backend.branch_checkout(uid, name)
        if "error" in result:
            return f"Error: {result['error']}"
        memories = backend.search(uid, "", top_k=top_k)
        if not memories:
            return f"Switched to branch '{name}'. No memories on this branch yet."
        lines = [f"- [{r.get('type', 'fact')}] {r['content']}" for r in memories]
        return (
            f"Switched to branch '{name}'. {len(memories)} memories on this branch:\n"
            + "\n".join(lines)
        )

    @server.tool()
    def memory_branch_delete(name: str, user_id: str | None = None) -> str:
        """Delete a memory branch.

        Args:
            name: Branch name to delete.
            user_id: User ID (optional).
        """
        err = _validate_name(name, "Branch name")
        if err:
            return f"Error: {err}"
        result = backend.branch_delete(_user(user_id), name)
        if "error" in result:
            return f"Error: {result['error']}"
        return f"Branch '{name}' deleted."

    @server.tool()
    def memory_merge(
        source: str,
        strategy: str = "append",
        user_id: str | None = None,
    ) -> str:
        """Merge a branch back into main.

        Args:
            source: Branch name to merge from.
            strategy: 'append' (skip duplicates) or 'replace' (overwrite duplicates).
            user_id: User ID (optional).
        """
        err = _validate_name(source, "Branch name")
        if err:
            return f"Error: {err}"
        if strategy not in ("append", "replace"):
            return "Error: strategy must be 'append' or 'replace'."
        result = backend.branch_merge(_user(user_id), source, strategy)
        if "error" in result:
            return f"Error: {result['error']}"
        return f"Merged {result['merged']} memories from '{source}' (skipped {result['skipped']})."

    @server.tool()
    def memory_diff(
        source: str,
        limit: int = 50,
        user_id: str | None = None,
    ) -> str:
        """Show what would change if a branch were merged into main.

        Uses kernel-level diff (LCA-based) then classifies each change semantically:
        - new: memory exists only on branch, no semantic match in main
        - conflict: branch memory is semantically similar (cosine>0.9) to a main memory but different content
        - modified: same memory_id changed on branch
        - removed: memory deleted on branch
        - new (no embedding): new memory without embedding, cannot check for conflicts

        Args:
            source: Branch name to diff.
            limit: Max changes to return (default 50).
            user_id: User ID (optional).
        """
        err = _validate_name(source, "Branch name")
        if err:
            return f"Error: {err}"
        limit = max(1, min(limit, 500))
        result = backend.branch_diff(_user(user_id), source, limit)
        if "error" in result:
            return f"Error: {result['error']}"
        if result["total"] == 0:
            return f"Branch '{source}' has no changes vs main."
        lines = [f"Branch '{source}': {result['total']} total changes"]
        if result.get("truncated"):
            lines[0] += f" (showing first {limit})"
        if result.get("summary"):
            parts = [f"{v} {k}" for k, v in result["summary"].items()]
            lines.append(f"Summary: {', '.join(parts)}")
        for c in result["changes"]:
            content_preview = c["content"][:80]
            lines.append(f"  [{c['semantic']}] {content_preview}")
        return "\n".join(lines)

    return server


# ── Entry point ───────────────────────────────────────────────────────


def main():
    import sys

    parser = argparse.ArgumentParser(description="Memoria Lite — MCP memory server")
    parser.add_argument("--api-url", help="Memory service API URL (remote mode)")
    parser.add_argument(
        "--db-url", help="Database URL for embedded mode (or set MEMORIA_DB_URL)"
    )
    parser.add_argument("--token", help="Auth token for remote mode")
    parser.add_argument("--user", default="default", help="Default user ID")
    parser.add_argument("--transport", choices=["stdio", "sse"], default="stdio")
    args = parser.parse_args()

    # MCP stdio uses stdout for JSON-RPC — ALL logging MUST go to stderr.
    _stderr_handler = logging.StreamHandler(sys.stderr)
    _stderr_handler.setFormatter(logging.Formatter("%(name)s: %(message)s"))
    logging.root.handlers = [_stderr_handler]
    logging.root.setLevel(logging.WARNING)

    if args.api_url:
        backend: MemoryBackend = HTTPBackend(args.api_url, token=args.token)
    else:
        # Priority: --db-url > MEMORIA_DB_URL > config碎变量 (MEMORIA_DB_HOST etc) > DEFAULT_DB_URL
        db_url = args.db_url or os.environ.get("MEMORIA_DB_URL")
        if not db_url:
            try:
                from memoria.config import get_settings

                db_url = get_settings().db_url
            except Exception:
                # Fallback to hardcoded default if config loading fails
                from memoria.schema import DEFAULT_DB_URL

                db_url = DEFAULT_DB_URL

        if not db_url:
            from memoria.schema import DEFAULT_DB_URL

            db_url = DEFAULT_DB_URL

        backend = EmbeddedBackend(db_url=db_url)

    server = create_server(backend, default_user=args.user)

    if args.transport == "sse":
        server.run(transport="sse")
    else:
        server.run(transport="stdio")


if __name__ == "__main__":
    main()
