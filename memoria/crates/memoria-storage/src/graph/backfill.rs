//! Build graph nodes + association edges + entity links from existing memories.
//! Equivalent to Python's ActivationIndexManager.backfill().

use crate::graph::ner;
use crate::graph::types::{GraphNode, NodeType};
use crate::store::SqlMemoryStore;
use memoria_core::MemoriaError;
use uuid::Uuid;

#[derive(Debug, Default)]
pub struct BackfillResult {
    pub processed: usize,
    pub skipped: usize,
    pub edges_created: usize,
    pub entities_linked: usize,
}

/// Build graph from all active memories for a user.
/// Idempotent — skips memories that already have graph nodes.
pub async fn backfill_graph(
    sql: &SqlMemoryStore,
    user_id: &str,
) -> Result<BackfillResult, MemoriaError> {
    let mut result = BackfillResult::default();
    let graph = sql.graph_store();
    let table = sql.active_table(user_id).await?;
    let memories = sql.list_active_from(&table, user_id, 500).await?;

    // Phase 1: Create semantic graph nodes for memories without one
    for mem in &memories {
        if graph.get_node_by_memory_id(&mem.memory_id).await?.is_some() {
            result.skipped += 1;
            continue;
        }
        let importance = if mem.initial_confidence >= 0.85 {
            0.6
        } else {
            0.5
        };
        let node = GraphNode {
            node_id: Uuid::new_v4().simple().to_string(),
            user_id: user_id.to_string(),
            node_type: NodeType::Semantic,
            content: mem.content.clone(),
            entity_type: None,
            embedding: mem.embedding.clone(),
            memory_id: Some(mem.memory_id.clone()),
            session_id: mem.session_id.clone(),
            confidence: mem.initial_confidence as f32,
            trust_tier: mem.trust_tier.to_string(),
            importance,
            source_nodes: vec![],
            conflicts_with: None,
            conflict_resolution: None,
            access_count: 0,
            cross_session_count: 0,
            is_active: true,
            superseded_by: None,
            created_at: mem.observed_at.map(|dt| dt.naive_utc()),
        };
        graph.create_node(&node).await?;
        result.processed += 1;

        // Entity linking via NER + graph edges for spreading activation
        let entities = ner::extract_entities(&mem.content);
        let mut links: Vec<(String, String, &str)> = Vec::new();
        for ent in &entities {
            let name = ent.name.to_lowercase();
            if let Ok((entity_id, _created)) = graph
                .upsert_entity(user_id, &name, &ent.name, &ent.entity_type)
                .await
            {
                links.push((mem.memory_id.clone(), entity_id, "ner"));
                result.entities_linked += 1;

                // Create entity graph node if new (so spreading activation can reach it)
                // NOTE: disabled — entity graph nodes without proper edges
                // can interfere with graph retrieval scoring

                // Add entity_link edge (skip person/time — too generic for activation)
                // NOTE: disabled — entity_link edges hurt some scenarios more than they help
                // if ent.entity_type != "person" && ent.entity_type != "time" {
                //     ...
                // }
            }
        }
        if !links.is_empty() {
            let refs: Vec<(&str, &str, &str)> = links
                .iter()
                .map(|(m, e, s)| (m.as_str(), e.as_str(), *s))
                .collect();
            let _ = graph
                .batch_upsert_memory_entity_links(user_id, &refs)
                .await;
        }
    }

    Ok(result)
}
