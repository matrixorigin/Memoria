"""Graph memory domain types."""

from __future__ import annotations

import enum
from dataclasses import dataclass, field


class NodeType(str, enum.Enum):
    EPISODIC = "episodic"
    SEMANTIC = "semantic"
    SCENE = "scene"
    ENTITY = "entity"


class EdgeType(str, enum.Enum):
    TEMPORAL = "temporal"  # sequential events (weight: 1.0)
    ABSTRACTION = "abstraction"  # event grounds concept (weight: 0.8)
    ASSOCIATION = "association"  # concept co-occurrence (weight: cosine_sim)
    CAUSAL = "causal"  # cause-effect (weight: 1.5)
    CONSOLIDATION = "consolidation"  # scene synthesis (weight: 1.0)
    ENTITY_LINK = "entity_link"  # memory ↔ named entity (weight: 1.0)


# Activation multipliers per edge type (spreading activation)
EDGE_TYPE_MULTIPLIER: dict[str, float] = {
    EdgeType.TEMPORAL: 1.5,
    EdgeType.ABSTRACTION: 0.8,
    EdgeType.ASSOCIATION: 1.0,
    EdgeType.CAUSAL: 2.0,
    EdgeType.CONSOLIDATION: 1.2,
    EdgeType.ENTITY_LINK: 1.2,
}


@dataclass
class Edge:
    """A single directed edge."""

    target_id: str
    edge_type: str  # EdgeType value
    weight: float = 1.0


@dataclass
class GraphNodeData:
    """In-memory representation of a graph node (no edges — edges live in DB)."""

    node_id: str
    user_id: str
    node_type: NodeType
    content: str
    entity_type: str | None = None  # entity nodes: tech, person, repo, project, concept
    embedding: list[float] | None = None

    # Source references
    event_id: str | None = None
    memory_id: str | None = None
    session_id: str | None = None

    # Confidence and trust
    confidence: float = 0.75
    trust_tier: str = "T3"

    # Importance
    importance: float = 0.0

    # Scene-specific
    source_nodes: list[str] = field(default_factory=list)

    # Conflict tracking
    conflicts_with: str | None = None
    conflict_resolution: str | None = None

    # Stats
    access_count: int = 0
    cross_session_count: int = 0

    is_active: bool = True
    superseded_by: str | None = None
    created_at: str | None = None
