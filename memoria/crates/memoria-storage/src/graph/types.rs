/// Graph domain types — mirrors Python's graph/types.py

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeType {
    Episodic,
    Semantic,
    Scene,
    Entity,
}

impl NodeType {
    pub fn as_str(&self) -> &'static str {
        match self {
            NodeType::Episodic => "episodic",
            NodeType::Semantic => "semantic",
            NodeType::Scene => "scene",
            NodeType::Entity => "entity",
        }
    }
}

impl std::str::FromStr for NodeType {
    type Err = std::convert::Infallible;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "episodic" => NodeType::Episodic,
            "scene" => NodeType::Scene,
            "entity" => NodeType::Entity,
            _ => NodeType::Semantic,
        })
    }
}

#[derive(Debug, Clone)]
pub struct GraphNode {
    pub node_id: String,
    pub user_id: String,
    pub node_type: NodeType,
    pub content: String,
    pub entity_type: Option<String>,
    pub embedding: Option<Vec<f32>>,
    pub memory_id: Option<String>,
    pub session_id: Option<String>,
    pub confidence: f32,
    pub trust_tier: String,
    pub importance: f32,
    pub source_nodes: Vec<String>,
    pub conflicts_with: Option<String>,
    pub conflict_resolution: Option<String>,
    pub access_count: i32,
    pub cross_session_count: i32,
    pub is_active: bool,
    pub superseded_by: Option<String>,
    pub created_at: Option<chrono::NaiveDateTime>,
}

impl GraphNode {
    pub fn age_days(&self) -> i64 {
        let Some(created) = self.created_at else { return 0 };
        let now = chrono::Utc::now().naive_utc();
        (now - created).num_days().max(0)
    }
}

#[derive(Debug, Clone)]
pub struct GraphEdge {
    pub source_id: String,
    pub target_id: String,
    pub edge_type: String,
    pub weight: f32,
    pub user_id: String,
}

pub mod edge_type {
    pub const TEMPORAL: &str = "temporal";
    pub const ABSTRACTION: &str = "abstraction";
    pub const ASSOCIATION: &str = "association";
    pub const CAUSAL: &str = "causal";
    pub const CONSOLIDATION: &str = "consolidation";
    pub const ENTITY_LINK: &str = "entity_link";
}
