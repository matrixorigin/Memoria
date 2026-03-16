pub mod consolidation;
pub mod store;
pub mod types;

pub use consolidation::{ConsolidationResult, GraphConsolidator};
pub use store::GraphStore;
pub use types::{GraphEdge, GraphNode, NodeType, edge_type};
