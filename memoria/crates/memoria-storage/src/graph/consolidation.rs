//! Graph consolidation — mirrors Python's graph/consolidation.py
//!
//! 1. Detect cross-session contradictions via association edges + cosine sim drop
//! 2. Check scene node source integrity (orphaned scenes)
//! 3. Trust tier lifecycle: T4→T3, T3→T2, T3→T4, T2→T3

use crate::graph::store::GraphStore;
use crate::graph::types::NodeType;
use memoria_core::MemoriaError;

const CONTRADICTION_ASSOCIATION_THRESHOLD: f32 = 0.7;
const SOURCE_INTEGRITY_RATIO: f32 = 0.5;
const T3_DEMOTION_STALE_DAYS: i64 = 60;
const T3_TO_T2_MIN_AGE_DAYS: i64 = 30;
const T3_TO_T2_MIN_CONFIDENCE: f32 = 0.85;
const T3_TO_T2_MIN_CROSS_SESSION: i32 = 3;
const T2_DEMOTION_CONFIDENCE: f32 = 0.7;

#[derive(Debug, Default)]
pub struct ConsolidationResult {
    pub conflicts_detected: usize,
    pub orphaned_scenes: usize,
    pub promoted: usize,
    pub demoted: usize,
    pub errors: Vec<String>,
}

pub struct GraphConsolidator<'a> {
    store: &'a GraphStore,
}

impl<'a> GraphConsolidator<'a> {
    pub fn new(store: &'a GraphStore) -> Self {
        Self { store }
    }

    pub async fn consolidate(&self, user_id: &str) -> ConsolidationResult {
        let mut result = ConsolidationResult::default();

        match self.detect_conflicts(user_id).await {
            Ok(n) => result.conflicts_detected = n,
            Err(e) => result.errors.push(format!("conflicts: {e}")),
        }
        match self.check_source_integrity(user_id).await {
            Ok(n) => result.orphaned_scenes = n,
            Err(e) => result.errors.push(format!("integrity: {e}")),
        }
        match self.trust_tier_lifecycle(user_id).await {
            Ok((p, d)) => {
                result.promoted = p;
                result.demoted = d;
            }
            Err(e) => result.errors.push(format!("tier_lifecycle: {e}")),
        }

        result
    }

    async fn detect_conflicts(&self, user_id: &str) -> Result<usize, MemoriaError> {
        let candidates = self
            .store
            .get_association_edges_with_current_sim(
                user_id,
                CONTRADICTION_ASSOCIATION_THRESHOLD,
                0.4,
            )
            .await?;

        if candidates.is_empty() {
            return Ok(0);
        }

        let candidate_ids: Vec<String> = candidates
            .iter()
            .flat_map(|(src, tgt, _, _)| [src.clone(), tgt.clone()])
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();

        let nodes = self.store.get_nodes_by_ids(&candidate_ids).await?;
        let node_map: std::collections::HashMap<String, _> =
            nodes.into_iter().map(|n| (n.node_id.clone(), n)).collect();

        let mut conflicts_found = 0;
        for (src_id, tgt_id, _ew, _cs) in &candidates {
            let node = match node_map.get(src_id) {
                Some(n) => n,
                None => continue,
            };
            let neighbor = match node_map.get(tgt_id) {
                Some(n) => n,
                None => continue,
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

            self.store
                .mark_conflict(&older.node_id, &newer.node_id, 0.5, older.confidence)
                .await?;
            conflicts_found += 1;
        }
        Ok(conflicts_found)
    }

    async fn check_source_integrity(&self, user_id: &str) -> Result<usize, MemoriaError> {
        let scenes = self
            .store
            .get_user_nodes(user_id, &NodeType::Scene, true)
            .await?;
        let mut orphaned = 0;
        for scene in &scenes {
            if scene.source_nodes.is_empty() {
                continue;
            }
            let sources = self.store.get_nodes_by_ids(&scene.source_nodes).await?;
            let active_count = sources.iter().filter(|n| n.is_active).count();
            if active_count == 0 {
                self.store.deactivate_node(&scene.node_id).await?;
                orphaned += 1;
            } else if (active_count as f32)
                < (scene.source_nodes.len() as f32 * SOURCE_INTEGRITY_RATIO)
            {
                self.store
                    .update_confidence_and_tier(
                        &scene.node_id,
                        scene.confidence * 0.8,
                        &scene.trust_tier,
                    )
                    .await?;
            }
        }
        Ok(orphaned)
    }

    async fn trust_tier_lifecycle(&self, user_id: &str) -> Result<(usize, usize), MemoriaError> {
        let scenes = self
            .store
            .get_user_nodes(user_id, &NodeType::Scene, true)
            .await?;
        let mut promoted = 0usize;
        let mut demoted = 0usize;

        // Config defaults (matches Python DEFAULT_CONFIG)
        let t4_to_t3_min_age_days: i64 = 7;
        let t4_to_t3_confidence: f32 = 0.8;

        for scene in &scenes {
            let age = scene.age_days();
            match scene.trust_tier.as_str() {
                "T4" => {
                    if scene.confidence >= t4_to_t3_confidence && age >= t4_to_t3_min_age_days {
                        self.store
                            .update_confidence_and_tier(&scene.node_id, scene.confidence, "T3")
                            .await?;
                        promoted += 1;
                    }
                }
                "T3" => {
                    if scene.confidence >= T3_TO_T2_MIN_CONFIDENCE
                        && age >= T3_TO_T2_MIN_AGE_DAYS
                        && scene.cross_session_count >= T3_TO_T2_MIN_CROSS_SESSION
                    {
                        self.store
                            .update_confidence_and_tier(&scene.node_id, scene.confidence, "T2")
                            .await?;
                        promoted += 1;
                    } else if age >= T3_DEMOTION_STALE_DAYS
                        && scene.confidence < t4_to_t3_confidence
                    {
                        self.store
                            .update_confidence_and_tier(&scene.node_id, scene.confidence, "T4")
                            .await?;
                        demoted += 1;
                    }
                }
                "T2" => {
                    if scene.confidence < T2_DEMOTION_CONFIDENCE {
                        self.store
                            .update_confidence_and_tier(&scene.node_id, scene.confidence, "T3")
                            .await?;
                        demoted += 1;
                    }
                }
                _ => {}
            }
        }
        Ok((promoted, demoted))
    }
}
