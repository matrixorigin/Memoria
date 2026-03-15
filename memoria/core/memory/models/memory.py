"""Memory SQLAlchemy model."""

from matrixone import VectorPrecision, VectorType

if not getattr(VectorType, "cache_ok", False):
    VectorType.cache_ok = True
from matrixone.sqlalchemy_ext import FulltextIndex, FulltextParserType
from sqlalchemy import (
    Column,
    Float,
    Index,
    SmallInteger,
    String,
    Text,
)
from sqlalchemy.sql import func

from memoria.core.base import Base
from memoria.core.memory.models._sa_types import EMBEDDING_DIM
from memoria.core.memory.models._sa_types import DateTime6, NullableJSON as JSON


class MemoryRecord(Base):
    """Typed, versioned memory with vector embedding and fulltext index."""

    __tablename__ = "mem_memories"
    __table_args__ = (
        FulltextIndex(
            "ft_memory_content", ["content"], parser=FulltextParserType.NGRAM
        ),
        Index("idx_memory_user_type_active", "user_id", "memory_type", "is_active"),
        Index("idx_memory_user_active", "user_id", "is_active"),
        Index("idx_memory_user_session", "user_id", "session_id"),
        Index("idx_memory_observed_at", "observed_at"),
        Index("idx_memory_superseded_by", "superseded_by"),
        Index(
            "idx_memory_user_active_type_observed",
            "user_id",
            "is_active",
            "memory_type",
            "observed_at",
        ),
    )

    memory_id = Column(String(64), primary_key=True)
    user_id = Column(String(64), nullable=False)
    session_id = Column(String(64), nullable=True)
    memory_type = Column(String(20), nullable=False)
    content = Column(Text, nullable=False)
    initial_confidence = Column(Float, default=0.75, nullable=False)
    trust_tier = Column(String(10), default="T3", nullable=True)
    embedding = Column(VectorType(EMBEDDING_DIM, VectorPrecision.F32))
    source_event_ids = Column(JSON, nullable=False, default=list)
    extra_metadata = Column(
        JSON, nullable=True
    )  # For episodic and other structured metadata
    superseded_by = Column(String(64), nullable=True)
    # access_count moved to mem_memories_stats table
    is_active = Column(SmallInteger, server_default="1", nullable=False)
    observed_at = Column(DateTime6, nullable=False)
    created_at = Column(DateTime6, default=func.now(), nullable=False)
    updated_at = Column(DateTime6, default=func.now(), onupdate=func.now())
