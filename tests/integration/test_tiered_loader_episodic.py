"""Test that TieredMemoryLoader includes episodic memories in L1 retrieval (G14 fix)."""

import uuid
from datetime import datetime, timezone

import pytest

from memoria.core.memory.tabular.service import MemoryService
from memoria.core.memory.tabular.store import MemoryStore
from memoria.core.memory.types import Memory, MemoryType, TrustTier
from memoria.core.tiered_loader import TieredMemoryLoader


def _uid() -> str:
    return f"g14_{uuid.uuid4().hex[:12]}"


@pytest.mark.integration
class TestTieredLoaderEpisodicG14:
    """G14: TieredMemoryLoader must include episodic memories in L1 retrieval."""

    def test_episodic_included_in_l1(self, db_factory, embed_client):
        """Episodic memory stored cross-session must appear in load_l1 results."""
        from sqlalchemy import text

        user_id = _uid()
        session_id = f"sess_{uuid.uuid4().hex[:8]}"

        # Cleanup
        with db_factory() as db:
            db.execute(
                text("DELETE FROM mem_memories WHERE user_id = :uid"),
                {"uid": user_id},
            )
            db.commit()

        store = MemoryStore(db_factory)

        # Store an episodic memory (cross-session, session_id=None)
        ep = Memory(
            memory_id=f"ep_{uuid.uuid4().hex}",
            user_id=user_id,
            memory_type=MemoryType.EPISODIC,
            content="Session Summary: Database optimization\n\nActions: Added indexes on user_id\n\nOutcome: Query time reduced 10x",
            initial_confidence=0.75,
            trust_tier=TrustTier.T3_INFERRED,
            session_id=None,  # cross-session
            observed_at=datetime.now(timezone.utc),
            embedding=embed_client.embed("database optimization indexes query"),
        )
        store.create(ep)

        # Store a semantic memory in the same session
        sem = Memory(
            memory_id=f"sem_{uuid.uuid4().hex}",
            user_id=user_id,
            memory_type=MemoryType.SEMANTIC,
            content="User prefers PostgreSQL for production workloads",
            initial_confidence=0.8,
            trust_tier=TrustTier.T3_INFERRED,
            session_id=session_id,
            observed_at=datetime.now(timezone.utc),
            embedding=embed_client.embed("PostgreSQL production database"),
        )
        store.create(sem)

        loader = TieredMemoryLoader(MemoryService(db_factory))
        query_embedding = embed_client.embed("database optimization")

        result, _ = loader.load_l1(
            user_id=user_id,
            session_id=session_id,
            query="database optimization",
            query_embedding=query_embedding,
            limit=10,
        )

        # Ground truth: episodic memory must appear in L1 output
        assert result != "", "L1 should return non-empty result"
        assert "episodic" in result.lower() or "Session Summary" in result, (
            f"Episodic memory must appear in L1 output. Got:\n{result}"
        )

    def test_episodic_not_in_l0_session(self, db_factory, embed_client):
        """Episodic memories must NOT appear in L0-session (working/tool_result only)."""
        from sqlalchemy import text

        user_id = _uid()
        session_id = f"sess_{uuid.uuid4().hex[:8]}"

        with db_factory() as db:
            db.execute(
                text("DELETE FROM mem_memories WHERE user_id = :uid"),
                {"uid": user_id},
            )
            db.commit()

        store = MemoryStore(db_factory)
        ep = Memory(
            memory_id=f"ep_{uuid.uuid4().hex}",
            user_id=user_id,
            memory_type=MemoryType.EPISODIC,
            content="Session Summary: some past session",
            initial_confidence=0.75,
            trust_tier=TrustTier.T3_INFERRED,
            session_id=session_id,
            observed_at=datetime.now(timezone.utc),
        )
        store.create(ep)

        loader = TieredMemoryLoader(MemoryService(db_factory))
        l0_session = loader.load_l0_session(user_id, session_id, limit=10)

        types = {m.memory_type for m in l0_session}
        assert MemoryType.EPISODIC not in types, (
            "Episodic must not appear in L0-session (reserved for working/tool_result)"
        )

    def test_build_section_contains_episodic_label(self, db_factory, embed_client):
        """build_section output must contain [episodic] label when episodic memory exists."""
        from sqlalchemy import text

        user_id = _uid()
        session_id = f"sess_{uuid.uuid4().hex[:8]}"

        with db_factory() as db:
            db.execute(
                text("DELETE FROM mem_memories WHERE user_id = :uid"),
                {"uid": user_id},
            )
            db.commit()

        store = MemoryStore(db_factory)
        ep = Memory(
            memory_id=f"ep_{uuid.uuid4().hex}",
            user_id=user_id,
            memory_type=MemoryType.EPISODIC,
            content="Session Summary: CI pipeline debugging\n\nActions: Fixed flaky tests\n\nOutcome: All 126 tests passing",
            initial_confidence=0.75,
            trust_tier=TrustTier.T3_INFERRED,
            session_id=None,
            observed_at=datetime.now(timezone.utc),
            embedding=embed_client.embed("CI pipeline tests debugging"),
        )
        store.create(ep)

        loader = TieredMemoryLoader(MemoryService(db_factory))
        query_embedding = embed_client.embed("CI tests pipeline")

        section, _ = loader.build_section(
            user_id=user_id,
            session_id=session_id,
            query="CI tests pipeline",
            query_embedding=query_embedding,
        )

        assert "[episodic]" in section, (
            f"build_section must include [episodic] label. Got:\n{section}"
        )
