use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use memoria_core::MemoriaError;
use memoria_storage::{GraphNode, GraphStore, NodeType};

use crate::strategy_domain::{StrategyDecision, StrategyEvidence, StrategyReport, StrategyStatus};

const CONTRADICTION_ASSOCIATION_THRESHOLD: f32 = 0.7;
const MAX_CURRENT_SIMILARITY: f32 = 0.4;
const SOURCE_INTEGRITY_RATIO: f32 = 0.5;
const T3_DEMOTION_STALE_DAYS: i64 = 60;
const T3_TO_T2_MIN_AGE_DAYS: i64 = 30;
const T3_TO_T2_MIN_CONFIDENCE: f32 = 0.85;
const T3_TO_T2_MIN_CROSS_SESSION: i32 = 3;
const T2_DEMOTION_CONFIDENCE: f32 = 0.7;
const T4_TO_T3_MIN_AGE_DAYS: i64 = 7;
const T4_TO_T3_CONFIDENCE: f32 = 0.8;
const CONFLICT_CONFIDENCE_FACTOR: f32 = 0.5;

#[derive(Debug, Clone, PartialEq)]
pub struct ConflictInput {
    pub user_id: String,
    pub min_edge_weight: f32,
    pub max_current_similarity: f32,
}

impl ConflictInput {
    pub fn for_user(user_id: impl Into<String>) -> Self {
        Self {
            user_id: user_id.into(),
            min_edge_weight: CONTRADICTION_ASSOCIATION_THRESHOLD,
            max_current_similarity: MAX_CURRENT_SIMILARITY,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Conflict {
    pub node_ids: Vec<String>,
    pub memory_ids: Vec<String>,
    pub conflict_type: String,
    pub evidence: Vec<StrategyEvidence>,
    pub severity: f32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ConsolidationInput {
    pub user_id: String,
}

impl ConsolidationInput {
    pub fn for_user(user_id: impl Into<String>) -> Self {
        Self {
            user_id: user_id.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TrustEvaluationInput {
    pub user_id: String,
}

impl TrustEvaluationInput {
    pub fn for_user(user_id: impl Into<String>) -> Self {
        Self {
            user_id: user_id.into(),
        }
    }
}

/// Storage adapter for graph-domain strategies.
#[async_trait]
pub trait GraphDomainStore: Send + Sync {
    /// Find association edges whose current similarity dropped below the threshold.
    async fn get_association_edges_with_current_sim(
        &self,
        user_id: &str,
        min_edge_weight: f32,
        max_current_similarity: f32,
    ) -> Result<Vec<(String, String, f32, f32)>, MemoriaError>;

    /// Fetch graph nodes by node id.
    async fn get_nodes_by_ids(&self, ids: &[String]) -> Result<Vec<GraphNode>, MemoriaError>;

    /// List user nodes for the requested node type.
    async fn get_user_nodes(
        &self,
        user_id: &str,
        node_type: &NodeType,
        active_only: bool,
    ) -> Result<Vec<GraphNode>, MemoriaError>;

    /// Mark two nodes as conflicting and apply the configured confidence reduction.
    async fn mark_conflict(
        &self,
        older_id: &str,
        newer_id: &str,
        confidence_factor: f32,
        old_confidence: f32,
    ) -> Result<(), MemoriaError>;

    /// Deactivate a graph node.
    async fn deactivate_node(&self, node_id: &str) -> Result<(), MemoriaError>;

    /// Update a node's confidence and trust tier.
    async fn update_confidence_and_tier(
        &self,
        node_id: &str,
        confidence: f32,
        tier: &str,
    ) -> Result<(), MemoriaError>;
}

#[async_trait]
impl GraphDomainStore for GraphStore {
    async fn get_association_edges_with_current_sim(
        &self,
        user_id: &str,
        min_edge_weight: f32,
        max_current_similarity: f32,
    ) -> Result<Vec<(String, String, f32, f32)>, MemoriaError> {
        GraphStore::get_association_edges_with_current_sim(
            self,
            user_id,
            min_edge_weight,
            max_current_similarity,
        )
        .await
    }

    async fn get_nodes_by_ids(&self, ids: &[String]) -> Result<Vec<GraphNode>, MemoriaError> {
        GraphStore::get_nodes_by_ids(self, ids).await
    }

    async fn get_user_nodes(
        &self,
        user_id: &str,
        node_type: &NodeType,
        active_only: bool,
    ) -> Result<Vec<GraphNode>, MemoriaError> {
        GraphStore::get_user_nodes(self, user_id, node_type, active_only).await
    }

    async fn mark_conflict(
        &self,
        older_id: &str,
        newer_id: &str,
        confidence_factor: f32,
        old_confidence: f32,
    ) -> Result<(), MemoriaError> {
        GraphStore::mark_conflict(self, older_id, newer_id, confidence_factor, old_confidence).await
    }

    async fn deactivate_node(&self, node_id: &str) -> Result<(), MemoriaError> {
        GraphStore::deactivate_node(self, node_id).await
    }

    async fn update_confidence_and_tier(
        &self,
        node_id: &str,
        confidence: f32,
        tier: &str,
    ) -> Result<(), MemoriaError> {
        GraphStore::update_confidence_and_tier(self, node_id, confidence, tier).await
    }
}

/// Detects cross-memory conflicts that need later consolidation decisions.
#[async_trait]
pub trait ConflictDetector: Send + Sync {
    /// Stable detector key, e.g. `conflict:default:v1`.
    fn detector_key(&self) -> &'static str;

    /// Detect conflicts for the provided input.
    async fn detect(
        &self,
        store: &dyn GraphDomainStore,
        input: &ConflictInput,
    ) -> Result<Vec<Conflict>, MemoriaError>;
}

/// Applies conflict-handling, source-integrity, and trust updates for a user.
#[async_trait]
pub trait ConsolidationStrategy: Send + Sync {
    /// Stable strategy key, e.g. `consolidation:default:v1`.
    fn strategy_key(&self) -> &'static str;

    /// Run consolidation and return a structured strategy report.
    async fn consolidate(
        &self,
        store: &dyn GraphDomainStore,
        input: &ConsolidationInput,
    ) -> Result<StrategyReport, MemoriaError>;
}

/// Evaluates and mutates trust-tier lifecycle state for graph scenes.
#[async_trait]
pub trait TrustLifecycleStrategy: Send + Sync {
    /// Stable strategy key, e.g. `trust:default:v1`.
    fn strategy_key(&self) -> &'static str;

    /// Evaluate the input and apply trust-tier transitions.
    async fn evaluate(
        &self,
        store: &dyn GraphDomainStore,
        input: &TrustEvaluationInput,
    ) -> Result<StrategyReport, MemoriaError>;
}

#[derive(Debug, Default)]
pub struct DefaultConflictDetector;

#[async_trait]
impl ConflictDetector for DefaultConflictDetector {
    fn detector_key(&self) -> &'static str {
        "conflict:default:v1"
    }

    async fn detect(
        &self,
        store: &dyn GraphDomainStore,
        input: &ConflictInput,
    ) -> Result<Vec<Conflict>, MemoriaError> {
        let candidates = store
            .get_association_edges_with_current_sim(
                &input.user_id,
                input.min_edge_weight,
                input.max_current_similarity,
            )
            .await?;

        if candidates.is_empty() {
            return Ok(vec![]);
        }

        let candidate_ids: Vec<String> = candidates
            .iter()
            .flat_map(|(src, tgt, _, _)| [src.clone(), tgt.clone()])
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        let nodes = store.get_nodes_by_ids(&candidate_ids).await?;
        let node_map: HashMap<String, GraphNode> = nodes
            .into_iter()
            .map(|node| (node.node_id.clone(), node))
            .collect();

        let mut conflicts = Vec::new();
        for (src_id, tgt_id, edge_weight, current_similarity) in candidates {
            let Some(node) = node_map.get(&src_id) else {
                continue;
            };
            let Some(neighbor) = node_map.get(&tgt_id) else {
                continue;
            };
            if !node.is_active || !neighbor.is_active {
                continue;
            }
            if node.node_type != NodeType::Semantic || neighbor.node_type != NodeType::Semantic {
                continue;
            }
            if node.conflicts_with.is_some() || neighbor.conflicts_with.is_some() {
                continue;
            }
            if node.session_id == neighbor.session_id {
                continue;
            }

            let (older, newer) = if node.node_id < neighbor.node_id {
                (node, neighbor)
            } else {
                (neighbor, node)
            };
            let memory_ids = [older, newer]
                .iter()
                .map(|entry| {
                    entry
                        .memory_id
                        .clone()
                        .unwrap_or_else(|| entry.node_id.clone())
                })
                .collect();

            conflicts.push(Conflict {
                node_ids: vec![older.node_id.clone(), newer.node_id.clone()],
                memory_ids,
                conflict_type: "contradiction".to_string(),
                evidence: vec![StrategyEvidence {
                    source: "graph.association".to_string(),
                    summary: format!(
                        "Association edge weight {:.2} dropped to cosine similarity {:.2} across sessions",
                        edge_weight, current_similarity
                    ),
                    score: Some(current_similarity),
                    references: vec![
                        format!("node:{}", older.node_id),
                        format!("node:{}", newer.node_id),
                    ],
                }],
                severity: (edge_weight - current_similarity).clamp(0.0, 1.0),
            });
        }

        Ok(conflicts)
    }
}

#[derive(Debug)]
enum SourceIntegrityAction {
    Deactivate,
    ReduceConfidence,
}

#[derive(Debug)]
struct SourceIntegrityFinding {
    scene_node_id: String,
    action: SourceIntegrityAction,
    confidence: f32,
    trust_tier: String,
    active_sources: usize,
    total_sources: usize,
}

pub struct DefaultConsolidationStrategy {
    detector: Arc<dyn ConflictDetector>,
    trust: Arc<dyn TrustLifecycleStrategy>,
}

impl std::fmt::Debug for DefaultConsolidationStrategy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DefaultConsolidationStrategy")
            .field("detector", &self.detector.detector_key())
            .field("trust", &self.trust.strategy_key())
            .finish()
    }
}

impl Default for DefaultConsolidationStrategy {
    fn default() -> Self {
        Self {
            detector: Arc::new(DefaultConflictDetector),
            trust: Arc::new(DefaultTrustLifecycleStrategy),
        }
    }
}

impl DefaultConsolidationStrategy {
    pub fn new(
        detector: Arc<dyn ConflictDetector>,
        trust: Arc<dyn TrustLifecycleStrategy>,
    ) -> Self {
        Self { detector, trust }
    }

    async fn apply_source_integrity(
        &self,
        store: &dyn GraphDomainStore,
        user_id: &str,
    ) -> Result<(Vec<StrategyDecision>, usize, usize), MemoriaError> {
        let scenes = store
            .get_user_nodes(user_id, &NodeType::Scene, true)
            .await?;
        let mut decisions = Vec::new();
        let mut deactivated = 0usize;
        let mut downgraded = 0usize;

        for scene in &scenes {
            if scene.source_nodes.is_empty() {
                continue;
            }
            let sources = store.get_nodes_by_ids(&scene.source_nodes).await?;
            let active_count = sources.iter().filter(|node| node.is_active).count();
            let Some(finding) = source_integrity_finding(scene, active_count) else {
                continue;
            };

            match finding.action {
                SourceIntegrityAction::Deactivate => {
                    store.deactivate_node(&finding.scene_node_id).await?;
                    deactivated += 1;
                    decisions.push(StrategyDecision {
                        action: "deactivate_orphaned_scene".to_string(),
                        confidence: Some(1.0),
                        rationale: "Scene lost all active source nodes and was deactivated"
                            .to_string(),
                        evidence: vec![StrategyEvidence {
                            source: "graph.source_integrity".to_string(),
                            summary: format!(
                                "Scene has {}/{} active source nodes",
                                finding.active_sources, finding.total_sources
                            ),
                            score: Some(0.0),
                            references: vec![format!("node:{}", finding.scene_node_id)],
                        }],
                        rollback_hint: None,
                    });
                }
                SourceIntegrityAction::ReduceConfidence => {
                    store
                        .update_confidence_and_tier(
                            &finding.scene_node_id,
                            finding.confidence,
                            &finding.trust_tier,
                        )
                        .await?;
                    downgraded += 1;
                    decisions.push(StrategyDecision {
                        action: "degrade_scene_confidence".to_string(),
                        confidence: Some(finding.confidence),
                        rationale:
                            "Scene retained partial support, so confidence was reduced in place"
                                .to_string(),
                        evidence: vec![StrategyEvidence {
                            source: "graph.source_integrity".to_string(),
                            summary: format!(
                                "Scene has {}/{} active source nodes",
                                finding.active_sources, finding.total_sources
                            ),
                            score: Some(
                                finding.active_sources as f32 / finding.total_sources as f32,
                            ),
                            references: vec![format!("node:{}", finding.scene_node_id)],
                        }],
                        rollback_hint: None,
                    });
                }
            }
        }

        Ok((decisions, deactivated, downgraded))
    }
}

#[async_trait]
impl ConsolidationStrategy for DefaultConsolidationStrategy {
    fn strategy_key(&self) -> &'static str {
        "consolidation:default:v1"
    }

    async fn consolidate(
        &self,
        store: &dyn GraphDomainStore,
        input: &ConsolidationInput,
    ) -> Result<StrategyReport, MemoriaError> {
        let conflicts = self
            .detector
            .detect(store, &ConflictInput::for_user(&input.user_id))
            .await?;

        let mut decisions = Vec::new();
        if !conflicts.is_empty() {
            let node_ids: Vec<String> = conflicts
                .iter()
                .flat_map(|conflict| conflict.node_ids.iter().cloned())
                .collect::<HashSet<_>>()
                .into_iter()
                .collect();
            let nodes = store.get_nodes_by_ids(&node_ids).await?;
            let node_map: HashMap<String, GraphNode> = nodes
                .into_iter()
                .map(|node| (node.node_id.clone(), node))
                .collect();

            for conflict in &conflicts {
                if conflict.node_ids.len() != 2 {
                    continue;
                }
                let older_id = &conflict.node_ids[0];
                let newer_id = &conflict.node_ids[1];
                let old_confidence = node_map
                    .get(older_id)
                    .map(|node| node.confidence)
                    .unwrap_or(1.0);
                store
                    .mark_conflict(
                        older_id,
                        newer_id,
                        CONFLICT_CONFIDENCE_FACTOR,
                        old_confidence,
                    )
                    .await?;
                decisions.push(StrategyDecision {
                    action: "mark_conflict".to_string(),
                    confidence: Some(conflict.severity),
                    rationale: "Conflicting semantic memories were flagged for later resolution"
                        .to_string(),
                    evidence: conflict.evidence.clone(),
                    rollback_hint: None,
                });
            }
        }

        let (mut integrity_decisions, orphaned_scenes, degraded_scenes) =
            self.apply_source_integrity(store, &input.user_id).await?;
        decisions.append(&mut integrity_decisions);

        let trust_report = self
            .trust
            .evaluate(store, &TrustEvaluationInput::for_user(&input.user_id))
            .await?;
        decisions.extend(trust_report.decisions.clone());

        let mut metrics = trust_report.metrics.clone();
        metrics.insert(
            "consolidation.conflicts_detected".to_string(),
            conflicts.len() as f64,
        );
        metrics.insert(
            "consolidation.orphaned_scenes".to_string(),
            orphaned_scenes as f64,
        );
        metrics.insert(
            "consolidation.degraded_scenes".to_string(),
            degraded_scenes as f64,
        );

        let status = if trust_report.status == StrategyStatus::Failed {
            StrategyStatus::Failed
        } else if trust_report.status == StrategyStatus::Degraded {
            StrategyStatus::Degraded
        } else {
            StrategyStatus::Success
        };

        Ok(StrategyReport {
            status,
            decisions,
            metrics,
            warnings: trust_report.warnings,
        })
    }
}

#[derive(Debug, Default)]
pub struct DefaultTrustLifecycleStrategy;

#[async_trait]
impl TrustLifecycleStrategy for DefaultTrustLifecycleStrategy {
    fn strategy_key(&self) -> &'static str {
        "trust:default:v1"
    }

    async fn evaluate(
        &self,
        store: &dyn GraphDomainStore,
        input: &TrustEvaluationInput,
    ) -> Result<StrategyReport, MemoriaError> {
        let scenes = store
            .get_user_nodes(&input.user_id, &NodeType::Scene, true)
            .await?;
        let mut decisions = Vec::new();
        let mut promoted = 0usize;
        let mut demoted = 0usize;

        for scene in &scenes {
            let age = scene.age_days();
            match scene.trust_tier.as_str() {
                "T4" => {
                    if scene.confidence >= T4_TO_T3_CONFIDENCE && age >= T4_TO_T3_MIN_AGE_DAYS {
                        store
                            .update_confidence_and_tier(&scene.node_id, scene.confidence, "T3")
                            .await?;
                        promoted += 1;
                        decisions.push(trust_decision(
                            "promote_trust_tier",
                            &scene.node_id,
                            scene.confidence,
                            format!(
                                "Scene aged {} days with confidence {:.2}; promoted from T4 to T3",
                                age, scene.confidence
                            ),
                            "T3",
                        ));
                    }
                }
                "T3" => {
                    if scene.confidence >= T3_TO_T2_MIN_CONFIDENCE
                        && age >= T3_TO_T2_MIN_AGE_DAYS
                        && scene.cross_session_count >= T3_TO_T2_MIN_CROSS_SESSION
                    {
                        store
                            .update_confidence_and_tier(&scene.node_id, scene.confidence, "T2")
                            .await?;
                        promoted += 1;
                        decisions.push(trust_decision(
                            "promote_trust_tier",
                            &scene.node_id,
                            scene.confidence,
                            format!(
                                "Scene reached cross-session threshold {}; promoted from T3 to T2",
                                scene.cross_session_count
                            ),
                            "T2",
                        ));
                    } else if age >= T3_DEMOTION_STALE_DAYS
                        && scene.confidence < T4_TO_T3_CONFIDENCE
                    {
                        store
                            .update_confidence_and_tier(&scene.node_id, scene.confidence, "T4")
                            .await?;
                        demoted += 1;
                        decisions.push(trust_decision(
                            "demote_trust_tier",
                            &scene.node_id,
                            scene.confidence,
                            format!(
                                "Scene is stale for {} days with confidence {:.2}; demoted from T3 to T4",
                                age, scene.confidence
                            ),
                            "T4",
                        ));
                    }
                }
                "T2" => {
                    if scene.confidence < T2_DEMOTION_CONFIDENCE {
                        store
                            .update_confidence_and_tier(&scene.node_id, scene.confidence, "T3")
                            .await?;
                        demoted += 1;
                        decisions.push(trust_decision(
                            "demote_trust_tier",
                            &scene.node_id,
                            scene.confidence,
                            format!(
                                "Scene confidence {:.2} fell below T2 guardrail; demoted to T3",
                                scene.confidence
                            ),
                            "T3",
                        ));
                    }
                }
                _ => {}
            }
        }

        let mut metrics = HashMap::new();
        metrics.insert("trust.promoted_count".to_string(), promoted as f64);
        metrics.insert("trust.demoted_count".to_string(), demoted as f64);

        Ok(StrategyReport {
            status: StrategyStatus::Success,
            decisions,
            metrics,
            warnings: Vec::new(),
        })
    }
}

fn source_integrity_finding(
    scene: &GraphNode,
    active_count: usize,
) -> Option<SourceIntegrityFinding> {
    if scene.source_nodes.is_empty() {
        return None;
    }
    let total_sources = scene.source_nodes.len();
    if active_count == 0 {
        return Some(SourceIntegrityFinding {
            scene_node_id: scene.node_id.clone(),
            action: SourceIntegrityAction::Deactivate,
            confidence: scene.confidence,
            trust_tier: scene.trust_tier.clone(),
            active_sources: active_count,
            total_sources,
        });
    }
    if (active_count as f32) < (total_sources as f32 * SOURCE_INTEGRITY_RATIO) {
        return Some(SourceIntegrityFinding {
            scene_node_id: scene.node_id.clone(),
            action: SourceIntegrityAction::ReduceConfidence,
            confidence: scene.confidence * 0.8,
            trust_tier: scene.trust_tier.clone(),
            active_sources: active_count,
            total_sources,
        });
    }
    None
}

fn trust_decision(
    action: &str,
    node_id: &str,
    confidence: f32,
    rationale: String,
    next_tier: &str,
) -> StrategyDecision {
    StrategyDecision {
        action: action.to_string(),
        confidence: Some(confidence),
        rationale,
        evidence: vec![StrategyEvidence {
            source: "trust.lifecycle".to_string(),
            summary: format!("Node transitioned to {next_tier}"),
            score: Some(confidence),
            references: vec![format!("node:{node_id}")],
        }],
        rollback_hint: None,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    #[derive(Default)]
    struct RecordingGraphStore {
        association_edges: Vec<(String, String, f32, f32)>,
        nodes: HashMap<String, GraphNode>,
        conflict_marks: Mutex<Vec<(String, String)>>,
        deactivated_nodes: Mutex<Vec<String>>,
        trust_updates: Mutex<Vec<(String, String)>>,
    }

    fn scene_node(id: &str, tier: &str, confidence: f32, age_days: i64) -> GraphNode {
        GraphNode {
            node_id: id.to_string(),
            user_id: "u1".to_string(),
            node_type: NodeType::Scene,
            content: format!("scene:{id}"),
            entity_type: None,
            embedding: None,
            memory_id: Some(format!("mem:{id}")),
            session_id: Some("s1".to_string()),
            confidence,
            trust_tier: tier.to_string(),
            importance: 0.0,
            source_nodes: Vec::new(),
            conflicts_with: None,
            conflict_resolution: None,
            access_count: 0,
            cross_session_count: 3,
            is_active: true,
            superseded_by: None,
            created_at: Some(chrono::Utc::now().naive_utc() - chrono::Duration::days(age_days)),
        }
    }

    fn semantic_node(id: &str, session_id: &str) -> GraphNode {
        GraphNode {
            node_id: id.to_string(),
            user_id: "u1".to_string(),
            node_type: NodeType::Semantic,
            content: format!("semantic:{id}"),
            entity_type: None,
            embedding: None,
            memory_id: Some(format!("mem:{id}")),
            session_id: Some(session_id.to_string()),
            confidence: 0.9,
            trust_tier: "T3".to_string(),
            importance: 0.0,
            source_nodes: Vec::new(),
            conflicts_with: None,
            conflict_resolution: None,
            access_count: 0,
            cross_session_count: 0,
            is_active: true,
            superseded_by: None,
            created_at: Some(chrono::Utc::now().naive_utc()),
        }
    }

    #[async_trait]
    impl GraphDomainStore for RecordingGraphStore {
        async fn get_association_edges_with_current_sim(
            &self,
            _: &str,
            _: f32,
            _: f32,
        ) -> Result<Vec<(String, String, f32, f32)>, MemoriaError> {
            Ok(self.association_edges.clone())
        }

        async fn get_nodes_by_ids(&self, ids: &[String]) -> Result<Vec<GraphNode>, MemoriaError> {
            Ok(ids
                .iter()
                .filter_map(|id| self.nodes.get(id).cloned())
                .collect())
        }

        async fn get_user_nodes(
            &self,
            _: &str,
            node_type: &NodeType,
            active_only: bool,
        ) -> Result<Vec<GraphNode>, MemoriaError> {
            Ok(self
                .nodes
                .values()
                .filter(|node| &node.node_type == node_type && (!active_only || node.is_active))
                .cloned()
                .collect())
        }

        async fn mark_conflict(
            &self,
            older_id: &str,
            newer_id: &str,
            _: f32,
            _: f32,
        ) -> Result<(), MemoriaError> {
            self.conflict_marks
                .lock()
                .unwrap()
                .push((older_id.to_string(), newer_id.to_string()));
            Ok(())
        }

        async fn deactivate_node(&self, node_id: &str) -> Result<(), MemoriaError> {
            self.deactivated_nodes
                .lock()
                .unwrap()
                .push(node_id.to_string());
            Ok(())
        }

        async fn update_confidence_and_tier(
            &self,
            node_id: &str,
            _: f32,
            tier: &str,
        ) -> Result<(), MemoriaError> {
            self.trust_updates
                .lock()
                .unwrap()
                .push((node_id.to_string(), tier.to_string()));
            Ok(())
        }
    }

    #[tokio::test]
    async fn default_conflict_detector_returns_cross_session_conflicts() {
        let store = RecordingGraphStore {
            association_edges: vec![("n1".into(), "n2".into(), 0.8, 0.3)],
            nodes: HashMap::from([
                ("n1".into(), semantic_node("n1", "s1")),
                ("n2".into(), semantic_node("n2", "s2")),
            ]),
            ..RecordingGraphStore::default()
        };

        let conflicts = DefaultConflictDetector
            .detect(&store, &ConflictInput::for_user("u1"))
            .await
            .expect("conflict detection should succeed");

        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].conflict_type, "contradiction");
        assert_eq!(conflicts[0].memory_ids, vec!["mem:n1", "mem:n2"]);
    }

    #[tokio::test]
    async fn trust_strategy_promotes_and_demotes_expected_tiers() {
        let mut t3_stale = scene_node("t3-stale", "T3", 0.6, 90);
        t3_stale.cross_session_count = 1;
        let mut t2_low = scene_node("t2-low", "T2", 0.5, 20);
        t2_low.cross_session_count = 1;
        let store = RecordingGraphStore {
            nodes: HashMap::from([
                ("t4".into(), scene_node("t4", "T4", 0.9, 10)),
                ("t3-promote".into(), scene_node("t3-promote", "T3", 0.9, 40)),
                ("t3-stale".into(), t3_stale),
                ("t2-low".into(), t2_low),
            ]),
            ..RecordingGraphStore::default()
        };

        let report = DefaultTrustLifecycleStrategy
            .evaluate(&store, &TrustEvaluationInput::for_user("u1"))
            .await
            .expect("trust evaluation should succeed");

        assert_eq!(report.metrics["trust.promoted_count"], 2.0);
        assert_eq!(report.metrics["trust.demoted_count"], 2.0);
        let updates = store.trust_updates.lock().unwrap().clone();
        assert!(updates.contains(&("t4".into(), "T3".into())));
        assert!(updates.contains(&("t3-promote".into(), "T2".into())));
        assert!(updates.contains(&("t3-stale".into(), "T4".into())));
        assert!(updates.contains(&("t2-low".into(), "T3".into())));
    }

    #[tokio::test]
    async fn consolidation_strategy_marks_conflicts_and_handles_orphaned_scenes() {
        let mut orphan = scene_node("orphan", "T3", 0.7, 5);
        orphan.source_nodes = vec!["missing1".into(), "missing2".into()];
        let store = RecordingGraphStore {
            association_edges: vec![("n1".into(), "n2".into(), 0.8, 0.3)],
            nodes: HashMap::from([
                ("n1".into(), semantic_node("n1", "s1")),
                ("n2".into(), semantic_node("n2", "s2")),
                ("orphan".into(), orphan),
            ]),
            ..RecordingGraphStore::default()
        };

        let report = DefaultConsolidationStrategy::default()
            .consolidate(&store, &ConsolidationInput::for_user("u1"))
            .await
            .expect("consolidation should succeed");

        assert_eq!(report.metrics["consolidation.conflicts_detected"], 1.0);
        assert_eq!(report.metrics["consolidation.orphaned_scenes"], 1.0);
        assert!(store
            .conflict_marks
            .lock()
            .unwrap()
            .contains(&("n1".into(), "n2".into())));
        assert!(store
            .deactivated_nodes
            .lock()
            .unwrap()
            .contains(&"orphan".to_string()));
    }
}
