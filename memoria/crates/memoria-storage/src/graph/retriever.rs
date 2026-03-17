//! ActivationRetriever — graph-based memory retrieval via spreading activation.
//!
//! Mirrors Python's graph/retriever.py: dual-trigger anchor selection (vector + BM25),
//! entity recall, spreading activation, multi-factor scoring.

use std::collections::{HashMap, HashSet};

use crate::graph::activation::SpreadingActivation;
use crate::graph::ner;
use crate::graph::store::GraphStore;
use crate::graph::types::GraphNode;
use memoria_core::MemoriaError;

const MIN_GRAPH_NODES: i64 = 3;
const TEMPORAL_DECAY_HOURS: f64 = 720.0;

// Scoring weights
const LAMBDA_SEMANTIC: f32 = 0.35;
const LAMBDA_ACTIVATION: f32 = 0.35;
const LAMBDA_CONFIDENCE: f32 = 0.20;
const LAMBDA_IMPORTANCE: f32 = 0.10;

const ENTITY_BOOST: f32 = 1.8;

fn node_type_weight(nt: &str) -> f32 {
    match nt {
        "scene" => 1.5,
        "semantic" => 1.0,
        "episodic" => 0.6,
        "entity" => 0.8,
        _ => 1.0,
    }
}

fn conflict_penalty(resolution: Option<&str>) -> f32 {
    match resolution {
        Some("superseded") => 0.5,
        Some("pending") => 0.7,
        _ => 1.0,
    }
}

/// Half-lives for confidence decay (days).
fn half_life_days(tier: &str) -> f64 {
    match tier {
        "T1" => 365.0,
        "T2" => 180.0,
        "T3" => 60.0,
        "T4" => 30.0,
        _ => 60.0,
    }
}

fn effective_confidence(node: &GraphNode) -> f32 {
    let Some(created) = node.created_at else {
        return node.confidence;
    };
    let hl = half_life_days(&node.trust_tier);
    let now = chrono::Utc::now().naive_utc();
    let age_days = (now - created).num_seconds().max(0) as f64 / 86400.0;
    node.confidence * ((-age_days * 2.0f64.ln() / hl).exp() as f32)
}

/// Task type → (iterations, anchor_k)
fn task_activation_params(task_type: Option<&str>) -> (usize, i64) {
    match task_type {
        Some("planning") => (2, 5),
        _ => (3, 10), // code_review, debugging, general, default
    }
}

pub struct ActivationRetriever<'a> {
    store: &'a GraphStore,
}

impl<'a> ActivationRetriever<'a> {
    pub fn new(store: &'a GraphStore) -> Self {
        Self { store }
    }

    /// Retrieve graph nodes via spreading activation.
    /// Returns scored (GraphNode, score) pairs.
    pub async fn retrieve(
        &self,
        user_id: &str,
        query: &str,
        query_embedding: &[f32],
        top_k: i64,
        task_type: Option<&str>,
    ) -> Result<Vec<(GraphNode, f32)>, MemoriaError> {
        if query_embedding.is_empty() {
            return Ok(vec![]);
        }
        let node_count = self.store.count_user_nodes(user_id).await?;
        if node_count < MIN_GRAPH_NODES {
            return Ok(vec![]);
        }

        let (iterations, anchor_k) = task_activation_params(task_type);

        // 1. Dual-trigger anchor selection
        let mut anchors: HashMap<String, f32> = HashMap::new();
        let mut anchor_semantic: HashMap<String, f32> = HashMap::new();

        // 1a. Vector anchors
        let vector_results = self
            .store
            .search_nodes_vector(user_id, query_embedding, anchor_k)
            .await?;
        for (node, sim) in &vector_results {
            let s = sim.max(0.0);
            anchors.insert(node.node_id.clone(), s);
            anchor_semantic.insert(node.node_id.clone(), s);
        }

        // 1b. BM25 anchors
        if let Ok(bm25_results) = self
            .store
            .search_nodes_fulltext(user_id, query, anchor_k)
            .await
        {
            for (node, _) in &bm25_results {
                anchors.entry(node.node_id.clone()).or_insert(0.7);
            }
        }

        if anchors.is_empty() {
            return Ok(vec![]);
        }

        // 2. Entity recall via NER on query
        let (entity_anchors, entity_memory_ids) = self.entity_recall(user_id, query).await;
        for (nid, weight) in &entity_anchors {
            anchors.entry(nid.clone()).or_insert(0.8 * weight);
        }

        // 3. Spreading activation
        let mut sa = SpreadingActivation::new(self.store, task_type);
        sa.set_anchors(anchors.clone());
        sa.propagate(iterations).await?;
        let activation_map = sa.get_activated(0.01);

        // 4. Collect candidate IDs
        let mut candidate_ids: HashSet<String> = anchors.keys().cloned().collect();
        let anchor_count = candidate_ids.len();
        let mut sorted_activated: Vec<_> = activation_map.iter().collect();
        sorted_activated.sort_by(|a, b| b.1.partial_cmp(a.1).unwrap_or(std::cmp::Ordering::Equal));
        for (nid, _) in sorted_activated.iter().take((top_k * 3) as usize) {
            candidate_ids.insert((*nid).clone());
        }

        // If spreading activation didn't discover any new nodes beyond anchors
        // and entity recall found nothing, graph adds no value — let hybrid handle it.
        let new_from_activation = candidate_ids.len() - anchor_count;
        if new_from_activation == 0 && entity_memory_ids.is_empty() {
            return Ok(vec![]);
        }

        // Add graph nodes for entity-recalled memories
        for mid in &entity_memory_ids {
            if let Ok(Some(gnode)) = self.store.get_node_by_memory_id(mid).await {
                if gnode.is_active {
                    candidate_ids.insert(gnode.node_id.clone());
                }
            }
        }

        // 5. Fetch candidate nodes
        let id_vec: Vec<String> = candidate_ids.into_iter().collect();
        let candidates = self.store.get_nodes_by_ids(&id_vec).await?;

        // 6. Score
        let mut results: Vec<(GraphNode, f32)> = Vec::new();
        for node in candidates {
            let activation = activation_map.get(&node.node_id).copied().unwrap_or(0.0);
            let semantic = anchor_semantic.get(&node.node_id).copied().unwrap_or(0.0);
            let confidence = effective_confidence(&node);

            let mut score = LAMBDA_SEMANTIC * semantic
                + LAMBDA_ACTIVATION * activation
                + LAMBDA_CONFIDENCE * confidence
                + LAMBDA_IMPORTANCE * node.importance;

            // Temporal recency decay
            if let Some(created) = node.created_at {
                let now = chrono::Utc::now().naive_utc();
                let age_hours =
                    (now - created).num_seconds().max(0) as f64 / 3600.0;
                score *= (-age_hours / TEMPORAL_DECAY_HOURS).exp() as f32;
            }

            // Frequency boost
            if node.access_count > 0 {
                score *= 1.0 + 0.1 * (1.0 + node.access_count as f32).ln();
            }

            // Node type weight
            score *= node_type_weight(node.node_type.as_str());

            // Entity boost
            if let Some(ref mid) = node.memory_id {
                if entity_memory_ids.contains(mid) {
                    score *= ENTITY_BOOST;
                }
            }

            // Conflict penalty
            if node.conflicts_with.is_some() {
                score *= conflict_penalty(node.conflict_resolution.as_deref());
            }

            if score > 0.01 {
                results.push((node, score));
            }
        }

        results.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        results.truncate(top_k as usize);
        Ok(results)
    }

    /// Entity recall via NER on query text.
    async fn entity_recall(
        &self,
        user_id: &str,
        query: &str,
    ) -> (HashMap<String, f32>, HashSet<String>) {
        let mut entity_anchors: HashMap<String, f32> = HashMap::new();
        let mut memory_ids: HashSet<String> = HashSet::new();

        let entities = ner::extract_entities(query);
        for ent in &entities {
            if ent.entity_type == "time" || ent.entity_type == "person" {
                continue;
            }
            if let Ok(Some(entity_id)) = self
                .store
                .find_entity_by_name(user_id, &ent.name)
                .await
            {
                entity_anchors.entry(entity_id.clone()).or_insert(1.0);
                if let Ok(mems) = self
                    .store
                    .get_memories_by_entity(&entity_id, user_id, 20)
                    .await
                {
                    for (mid, _) in mems {
                        memory_ids.insert(mid);
                    }
                }
            }
        }
        (entity_anchors, memory_ids)
    }
}
