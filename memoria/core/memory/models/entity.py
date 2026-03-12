"""Entity SQLAlchemy models — canonical entity registry + memory-entity links."""

from sqlalchemy import Column, Float, Index, String, UniqueConstraint
from sqlalchemy.sql import func

from memoria.core.base import Base
from memoria.core.memory.models._sa_types import DateTime6


class Entity(Base):
    """Canonical entity registry — deduplicated by (user_id, name)."""

    __tablename__ = "mem_entities"
    __table_args__ = (
        UniqueConstraint("user_id", "name", name="uidx_entity_user_name"),
        Index("idx_entity_user", "user_id"),
    )

    entity_id = Column(String(32), primary_key=True)
    user_id = Column(String(64), nullable=False)
    name = Column(String(200), nullable=False)
    display_name = Column(String(200))
    entity_type = Column(String(20), nullable=False, server_default="concept")
    created_at = Column(DateTime6, default=func.now(), nullable=False)


class MemoryEntityLink(Base):
    """Many-to-many link between mem_memories and mem_entities."""

    __tablename__ = "mem_memory_entity_links"
    __table_args__ = (
        Index("idx_link_user_entity", "user_id", "entity_id"),
        Index("idx_link_entity_user", "entity_id", "user_id"),
    )

    memory_id = Column(String(64), primary_key=True)
    entity_id = Column(String(32), primary_key=True)
    user_id = Column(String(64), nullable=False)
    source = Column(String(10), nullable=False, server_default="regex")
    weight = Column(Float, nullable=False, server_default="0.8")
    created_at = Column(DateTime6, default=func.now(), nullable=False)
