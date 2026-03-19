//! Spreading Activation engine — DB-backed iterative expansion.
//!
//! Mirrors Python's graph/activation.py: each propagation round fetches
//! only the edges needed from DB, not the full graph.

use std::collections::HashMap;

use crate::graph::store::GraphStore;
use crate::graph::types::edge_type;
use memoria_core::MemoriaError;

// ── Default hyperparameters ──────────────────────────────────────────

const DECAY_RATE: f32 = 0.5;
const SPREADING_FACTOR: f32 = 0.8;
const INHIBITION_BETA: f32 = 0.15;
const INHIBITION_TOP_M: usize = 7;
const SIGMOID_GAMMA: f32 = 5.0;
const SIGMOID_THETA: f32 = 0.1;

/// Edge-type multipliers for spreading activation.
fn edge_type_multiplier(etype: &str) -> f32 {
    match etype {
        edge_type::TEMPORAL => 1.5,
        edge_type::ABSTRACTION => 0.8,
        edge_type::ASSOCIATION => 1.0,
        edge_type::CAUSAL => 2.0,
        edge_type::CONSOLIDATION => 1.2,
        edge_type::ENTITY_LINK => 1.2,
        _ => 1.0,
    }
}

/// Task-type edge boosts (applied on top of edge_type_multiplier).
fn task_edge_boost(task_type: Option<&str>, etype: &str) -> f32 {
    match task_type {
        Some("code_review") => match etype {
            edge_type::CAUSAL => 1.5,
            edge_type::TEMPORAL => 0.5,
            _ => 1.0,
        },
        Some("debugging") => match etype {
            edge_type::CAUSAL => 2.0,
            edge_type::TEMPORAL => 1.5,
            edge_type::ASSOCIATION => 0.5,
            _ => 1.0,
        },
        Some("planning") => match etype {
            edge_type::ASSOCIATION => 1.2,
            edge_type::TEMPORAL => 0.8,
            _ => 1.0,
        },
        _ => 1.0,
    }
}

fn edge_weight(
    etype: &str,
    base_weight: f32,
    task_type: Option<&str>,
    entity_link_mult: f32,
) -> f32 {
    let mut mult = edge_type_multiplier(etype);
    if etype == edge_type::ENTITY_LINK {
        mult = entity_link_mult;
    }
    base_weight * mult * task_edge_boost(task_type, etype)
}

pub struct SpreadingActivation<'a> {
    store: &'a GraphStore,
    activation: HashMap<String, f32>,
    out_degree: HashMap<String, usize>,
    task_type: Option<String>,
    entity_link_mult: f32,
}

impl<'a> SpreadingActivation<'a> {
    pub fn new(store: &'a GraphStore, task_type: Option<&str>) -> Self {
        Self {
            store,
            activation: HashMap::new(),
            out_degree: HashMap::new(),
            task_type: task_type.map(String::from),
            entity_link_mult: 1.8,
        }
    }

    pub fn set_anchors(&mut self, anchors: HashMap<String, f32>) {
        self.activation = anchors;
    }

    pub async fn propagate(&mut self, iterations: usize) -> Result<(), MemoriaError> {
        for _ in 0..iterations {
            self.propagation_step().await?;
        }
        Ok(())
    }

    pub fn get_activated(&self, min_activation: f32) -> HashMap<String, f32> {
        self.activation
            .iter()
            .filter(|(_, &a)| a >= min_activation)
            .map(|(k, &v)| (k.clone(), v))
            .collect()
    }

    async fn propagation_step(&mut self) -> Result<(), MemoriaError> {
        let active_ids: Vec<String> = self.activation.keys().cloned().collect();
        if active_ids.is_empty() {
            return Ok(());
        }

        let (incoming, outgoing) = self.store.get_edges_bidirectional(&active_ids).await?;

        // Collect contributor IDs for out-degree caching
        let mut contributor_ids: Vec<String> = Vec::new();
        for edges in incoming.values() {
            for (peer, _, _) in edges {
                if !self.out_degree.contains_key(peer) {
                    contributor_ids.push(peer.clone());
                }
            }
        }
        // Fetch out-degree for uncached contributors
        if !contributor_ids.is_empty() {
            contributor_ids.sort();
            contributor_ids.dedup();
            // Use outgoing edges count as proxy for out-degree
            let out_edges = self.store.get_edges_for_nodes(&contributor_ids).await?;
            let mut degree_map: HashMap<String, usize> = HashMap::new();
            for (src, _) in &out_edges {
                *degree_map.entry(src.clone()).or_default() += 1;
            }
            for id in &contributor_ids {
                self.out_degree
                    .entry(id.clone())
                    .or_insert_with(|| degree_map.get(id).copied().unwrap_or(1).max(1));
            }
        }
        for (nid, edges) in &outgoing {
            self.out_degree
                .entry(nid.clone())
                .or_insert(edges.len().max(1));
        }

        let task_type = self.task_type.as_deref();
        let mut raw: HashMap<String, f32> = HashMap::new();

        // Retention + incoming spread
        for nid in &active_ids {
            let retention = (1.0 - DECAY_RATE) * self.activation.get(nid).copied().unwrap_or(0.0);
            let mut spread = 0.0f32;
            if let Some(edges) = incoming.get(nid) {
                for (peer, etype, w) in edges {
                    let neighbor_act = self.activation.get(peer).copied().unwrap_or(0.0);
                    if neighbor_act <= 0.0 {
                        continue;
                    }
                    let fan = *self.out_degree.get(peer).unwrap_or(&1) as f32;
                    spread += SPREADING_FACTOR
                        * edge_weight(etype, *w, task_type, self.entity_link_mult)
                        * neighbor_act
                        / fan;
                }
            }
            raw.insert(nid.clone(), retention + spread);
        }

        // Outgoing spread to new nodes
        for nid in &active_ids {
            if let Some(edges) = outgoing.get(nid) {
                let neighbor_act = self.activation.get(nid).copied().unwrap_or(0.0);
                if neighbor_act <= 0.0 {
                    continue;
                }
                let fan = *self.out_degree.get(nid).unwrap_or(&1) as f32;
                for (peer, etype, w) in edges {
                    if !raw.contains_key(peer) {
                        let spread_val = SPREADING_FACTOR
                            * edge_weight(etype, *w, task_type, self.entity_link_mult)
                            * neighbor_act
                            / fan;
                        raw.insert(peer.clone(), spread_val);
                    }
                }
            }
        }

        // Lateral inhibition
        let inhibited = lateral_inhibition(&raw);

        // Sigmoid + threshold
        self.activation.clear();
        for (nid, val) in inhibited {
            let s = sigmoid(val);
            if s > 0.01 {
                self.activation.insert(nid, s);
            }
        }
        Ok(())
    }
}

fn sigmoid(x: f32) -> f32 {
    let z = SIGMOID_GAMMA * (x - SIGMOID_THETA);
    if z < -20.0 {
        return 0.0;
    }
    if z > 20.0 {
        return 1.0;
    }
    1.0 / (1.0 + (-z).exp())
}

fn lateral_inhibition(raw: &HashMap<String, f32>) -> HashMap<String, f32> {
    if raw.is_empty() {
        return raw.clone();
    }
    let top_m = INHIBITION_TOP_M.min((raw.len() / 3).max(1));
    let mut sorted_vals: Vec<f32> = raw.values().copied().collect();
    sorted_vals.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));
    let top_m_values: Vec<f32> = sorted_vals.into_iter().take(top_m).collect();

    let mut result = HashMap::new();
    for (nid, &val) in raw {
        let inhibition: f32 = top_m_values
            .iter()
            .filter(|&&top_val| top_val > val)
            .map(|&top_val| INHIBITION_BETA * (top_val - val))
            .sum();
        result.insert(nid.clone(), (val - inhibition).max(0.0));
    }
    result
}
