//! GraphStore — CRUD for memory_graph_nodes and memory_graph_edges.
//! Mirrors Python's graph/graph_store.py core methods needed for consolidation.

use crate::store::db_err;
use crate::graph::types::{GraphEdge, GraphNode, NodeType, edge_type};
use memoria_core::MemoriaError;
use sqlx::{MySqlPool, Row};
use uuid::Uuid;

fn new_id() -> String {
    Uuid::new_v4().simple().to_string()
}

pub struct GraphStore {
    pool: MySqlPool,
    embedding_dim: usize,
}

impl GraphStore {
    pub fn new(pool: MySqlPool, embedding_dim: usize) -> Self {
        Self { pool, embedding_dim }
    }

    pub fn pool(&self) -> &MySqlPool { &self.pool }

    // ── DDL ──────────────────────────────────────────────────────────────────

    pub async fn migrate(&self) -> Result<(), MemoriaError> {
        // memory_graph_nodes
        let sql = format!(
            r#"CREATE TABLE IF NOT EXISTS memory_graph_nodes (
                node_id             VARCHAR(32)  NOT NULL,
                user_id             VARCHAR(64)  NOT NULL,
                node_type           VARCHAR(10)  NOT NULL,
                content             TEXT         NOT NULL,
                entity_type         VARCHAR(20)  DEFAULT NULL,
                embedding           vecf32({dim}) DEFAULT NULL,
                memory_id           VARCHAR(64)  DEFAULT NULL,
                session_id          VARCHAR(64)  DEFAULT NULL,
                confidence          FLOAT        DEFAULT 0.75,
                trust_tier          VARCHAR(4)   DEFAULT 'T3',
                importance          FLOAT        NOT NULL DEFAULT 0.0,
                source_nodes        TEXT         DEFAULT NULL,
                conflicts_with      VARCHAR(32)  DEFAULT NULL,
                conflict_resolution VARCHAR(10)  DEFAULT NULL,
                access_count        INT          DEFAULT 0,
                cross_session_count INT          DEFAULT 0,
                is_active           SMALLINT     NOT NULL DEFAULT 1,
                superseded_by       VARCHAR(32)  DEFAULT NULL,
                created_at          DATETIME(6)  NOT NULL,
                PRIMARY KEY (node_id),
                KEY idx_graph_memory (memory_id),
                KEY idx_graph_conflicts (user_id, conflicts_with),
                KEY idx_graph_user_active (user_id, is_active, node_type),
                FULLTEXT ft_graph_content (content) WITH PARSER ngram
            )"#,
            dim = self.embedding_dim
        );
        sqlx::query(&sql).execute(&self.pool).await.map_err(db_err)?;

        // memory_graph_edges
        sqlx::query(
            r#"CREATE TABLE IF NOT EXISTS memory_graph_edges (
                source_id  VARCHAR(32)  NOT NULL,
                target_id  VARCHAR(32)  NOT NULL,
                edge_type  VARCHAR(15)  NOT NULL,
                weight     FLOAT        NOT NULL,
                user_id    VARCHAR(64)  NOT NULL,
                PRIMARY KEY (source_id, target_id, edge_type),
                KEY idx_edge_user (user_id),
                KEY idx_edge_target (target_id)
            )"#,
        ).execute(&self.pool).await.map_err(db_err)?;

        // mem_entities
        let sql2 = format!(
            r#"CREATE TABLE IF NOT EXISTS mem_entities (
                entity_id    VARCHAR(32)  NOT NULL,
                user_id      VARCHAR(64)  NOT NULL,
                name         VARCHAR(200) NOT NULL,
                display_name VARCHAR(200) DEFAULT NULL,
                entity_type  VARCHAR(20)  NOT NULL DEFAULT 'concept',
                embedding    vecf32({dim}) DEFAULT NULL,
                created_at   DATETIME(6)  NOT NULL DEFAULT NOW(),
                PRIMARY KEY (entity_id),
                UNIQUE KEY uidx_entity_user_name (user_id, name),
                KEY idx_entity_user (user_id)
            )"#,
            dim = self.embedding_dim
        );
        sqlx::query(&sql2).execute(&self.pool).await.map_err(db_err)?;

        // mem_memory_entity_links
        sqlx::query(
            r#"CREATE TABLE IF NOT EXISTS mem_memory_entity_links (
                memory_id  VARCHAR(64) NOT NULL,
                entity_id  VARCHAR(32) NOT NULL,
                user_id    VARCHAR(64) NOT NULL,
                source     VARCHAR(10) NOT NULL DEFAULT 'regex',
                weight     FLOAT       NOT NULL DEFAULT 0.8,
                created_at DATETIME(6) NOT NULL DEFAULT NOW(),
                PRIMARY KEY (memory_id, entity_id),
                KEY idx_link_user_entity (user_id, entity_id),
                KEY idx_link_entity_user (entity_id, user_id)
            )"#,
        ).execute(&self.pool).await.map_err(db_err)?;

        Ok(())
    }

    // ── Node reads ───────────────────────────────────────────────────────────

    pub async fn get_node(&self, node_id: &str) -> Result<Option<GraphNode>, MemoriaError> {
        let row = sqlx::query(
            "SELECT * FROM memory_graph_nodes WHERE node_id = ?"
        )
        .bind(node_id)
        .fetch_optional(&self.pool).await.map_err(db_err)?;
        Ok(row.map(|r| row_to_node(&r)))
    }

    pub async fn get_nodes_by_ids(&self, ids: &[String]) -> Result<Vec<GraphNode>, MemoriaError> {
        if ids.is_empty() { return Ok(vec![]); }
        let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!("SELECT * FROM memory_graph_nodes WHERE node_id IN ({placeholders})");
        let mut q = sqlx::query(&sql);
        for id in ids { q = q.bind(id); }
        let rows = q.fetch_all(&self.pool).await.map_err(db_err)?;
        Ok(rows.iter().map(row_to_node).collect())
    }

    pub async fn get_node_by_memory_id(&self, memory_id: &str) -> Result<Option<GraphNode>, MemoriaError> {
        let row = sqlx::query(
            "SELECT * FROM memory_graph_nodes WHERE memory_id = ? AND is_active = 1 LIMIT 1"
        )
        .bind(memory_id)
        .fetch_optional(&self.pool).await.map_err(db_err)?;
        Ok(row.map(|r| row_to_node(&r)))
    }

    /// Get all active nodes of a given type for a user (no embedding loaded).
    pub async fn get_user_nodes(
        &self,
        user_id: &str,
        node_type: &NodeType,
        active_only: bool,
    ) -> Result<Vec<GraphNode>, MemoriaError> {
        let active_clause = if active_only { " AND is_active = 1" } else { "" };
        let sql = format!(
            "SELECT node_id, user_id, node_type, content, entity_type, \
             memory_id, session_id, confidence, trust_tier, importance, \
             source_nodes, conflicts_with, conflict_resolution, \
             access_count, cross_session_count, is_active, superseded_by, created_at \
             FROM memory_graph_nodes \
             WHERE user_id = ? AND node_type = ?{active_clause}"
        );
        let rows = sqlx::query(&sql)
            .bind(user_id).bind(node_type.as_str())
            .fetch_all(&self.pool).await.map_err(db_err)?;
        Ok(rows.iter().map(row_to_node_no_emb).collect())
    }

    // ── Node writes ──────────────────────────────────────────────────────────

    /// Get all active nodes for a user, including embeddings.
    pub async fn get_user_nodes_with_embeddings(
        &self, user_id: &str, limit: i64,
    ) -> Result<Vec<GraphNode>, MemoriaError> {
        // Must alias embedding to force MO to return vecf32 as text
        let sql = "SELECT node_id, user_id, node_type, content, entity_type, \
                   embedding AS embedding, memory_id, session_id, confidence, trust_tier, importance, \
                   source_nodes, conflicts_with, conflict_resolution, \
                   access_count, cross_session_count, is_active, superseded_by, created_at \
                   FROM memory_graph_nodes \
                   WHERE user_id = ? AND is_active = 1 \
                     AND embedding IS NOT NULL AND vector_dims(embedding) > 0 \
                   LIMIT ?";
        let rows = sqlx::query(sql).bind(user_id).bind(limit)
            .fetch_all(&self.pool).await.map_err(db_err)?;
        Ok(rows.iter().map(row_to_node).collect())
    }

    pub async fn create_node(&self, node: &GraphNode) -> Result<(), MemoriaError> {
        let source_nodes_str = if node.source_nodes.is_empty() {
            None
        } else {
            Some(node.source_nodes.join(","))
        };
        let now = node.created_at.unwrap_or_else(|| chrono::Utc::now().naive_utc());
        let emb_lit = node.embedding.as_ref().map(|v|
            format!("[{}]", v.iter().map(|f| f.to_string()).collect::<Vec<_>>().join(","))
        );
        let sql = if let Some(emb) = &emb_lit {
            format!(
                "INSERT INTO memory_graph_nodes \
                 (node_id, user_id, node_type, content, entity_type, embedding, \
                  memory_id, session_id, confidence, trust_tier, importance, \
                  source_nodes, conflicts_with, conflict_resolution, \
                  access_count, cross_session_count, is_active, superseded_by, created_at) \
                 VALUES (?,?,?,?,?,'{}',?,?,?,?,?,?,?,?,?,?,?,?,?)",
                emb
            )
        } else {
            "INSERT INTO memory_graph_nodes \
             (node_id, user_id, node_type, content, entity_type, \
              memory_id, session_id, confidence, trust_tier, importance, \
              source_nodes, conflicts_with, conflict_resolution, \
              access_count, cross_session_count, is_active, superseded_by, created_at) \
             VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)".to_string()
        };
        sqlx::query(&sql)
        .bind(&node.node_id).bind(&node.user_id).bind(node.node_type.as_str())
        .bind(&node.content).bind(&node.entity_type)
        .bind(&node.memory_id).bind(&node.session_id)
        .bind(node.confidence).bind(&node.trust_tier).bind(node.importance)
        .bind(source_nodes_str).bind(&node.conflicts_with).bind(&node.conflict_resolution)
        .bind(node.access_count).bind(node.cross_session_count)
        .bind(if node.is_active { 1i8 } else { 0i8 })
        .bind(&node.superseded_by).bind(now)
        .execute(&self.pool).await.map_err(db_err)?;
        Ok(())
    }

    pub async fn deactivate_node(&self, node_id: &str) -> Result<(), MemoriaError> {
        sqlx::query("UPDATE memory_graph_nodes SET is_active = 0 WHERE node_id = ?")
            .bind(node_id).execute(&self.pool).await.map_err(db_err)?;
        Ok(())
    }

    /// Deactivate all graph nodes linked to a memory_id.
    pub async fn deactivate_by_memory_id(&self, memory_id: &str) -> Result<(), MemoriaError> {
        sqlx::query("UPDATE memory_graph_nodes SET is_active = 0 WHERE memory_id = ?")
            .bind(memory_id).execute(&self.pool).await.map_err(db_err)?;
        Ok(())
    }

    /// Update content (and optionally confidence) of a graph node by memory_id.
    pub async fn update_content_by_memory_id(
        &self,
        memory_id: &str,
        new_content: &str,
    ) -> Result<(), MemoriaError> {
        sqlx::query(
            "UPDATE memory_graph_nodes SET content = ? WHERE memory_id = ? AND is_active = 1"
        )
        .bind(new_content).bind(memory_id)
        .execute(&self.pool).await.map_err(db_err)?;
        Ok(())
    }

    pub async fn update_confidence_and_tier(
        &self, node_id: &str, confidence: f32, tier: &str,
    ) -> Result<(), MemoriaError> {
        sqlx::query(
            "UPDATE memory_graph_nodes SET confidence = ?, trust_tier = ? WHERE node_id = ?"
        )
        .bind(confidence).bind(tier).bind(node_id)
        .execute(&self.pool).await.map_err(db_err)?;
        Ok(())
    }

    pub async fn mark_conflict(
        &self,
        older_id: &str,
        newer_id: &str,
        confidence_factor: f32,
        old_confidence: f32,
    ) -> Result<(), MemoriaError> {
        // Mark older as conflicting with newer, reduce confidence
        sqlx::query(
            "UPDATE memory_graph_nodes \
             SET conflicts_with = ?, confidence = ? \
             WHERE node_id = ?"
        )
        .bind(newer_id)
        .bind(old_confidence * confidence_factor)
        .bind(older_id)
        .execute(&self.pool).await.map_err(db_err)?;
        Ok(())
    }

    // ── Edge reads ───────────────────────────────────────────────────────────

    /// Get association edges with current cosine similarity between node embeddings.
    /// Returns (source_id, target_id, edge_weight, current_cosine_sim).
    /// Only returns pairs where both nodes have embeddings and cosine sim dropped below threshold.
    pub async fn get_association_edges_with_current_sim(
        &self,
        user_id: &str,
        min_edge_weight: f32,
        max_current_sim: f32,
    ) -> Result<Vec<(String, String, f32, f32)>, MemoriaError> {
        // MatrixOne doesn't support HAVING on computed aliases — use subquery
        let rows = sqlx::query(
            "SELECT source_id, target_id, edge_weight, cur_sim FROM ( \
               SELECT e.source_id, e.target_id, e.weight as edge_weight, \
               cosine_similarity(n1.embedding, n2.embedding) as cur_sim \
               FROM memory_graph_edges e \
               JOIN memory_graph_nodes n1 ON e.source_id = n1.node_id \
               JOIN memory_graph_nodes n2 ON e.target_id = n2.node_id \
               WHERE e.user_id = ? AND e.edge_type = ? AND e.weight >= ? \
               AND n1.embedding IS NOT NULL AND n2.embedding IS NOT NULL \
               AND vector_dims(n1.embedding) > 0 AND vector_dims(n2.embedding) > 0 \
             ) t WHERE t.cur_sim <= ?"
        )
        .bind(user_id).bind(edge_type::ASSOCIATION)
        .bind(min_edge_weight).bind(max_current_sim)
        .fetch_all(&self.pool).await.map_err(db_err)?;

        Ok(rows.iter().filter_map(|r| {
            let src: String = r.try_get("source_id").ok()?;
            let tgt: String = r.try_get("target_id").ok()?;
            let ew: f32 = r.try_get("edge_weight").ok()?;
            let cs: f32 = r.try_get("cur_sim").ok()?;
            Some((src, tgt, ew, cs))
        }).collect::<Vec<_>>())
    }

    // ── Edge writes ──────────────────────────────────────────────────────────

    pub async fn add_edge(&self, edge: &GraphEdge) -> Result<(), MemoriaError> {
        sqlx::query(
            "INSERT INTO memory_graph_edges (source_id, target_id, edge_type, weight, user_id) \
             VALUES (?,?,?,?,?) \
             ON DUPLICATE KEY UPDATE weight = VALUES(weight)"
        )
        .bind(&edge.source_id).bind(&edge.target_id)
        .bind(&edge.edge_type).bind(edge.weight).bind(&edge.user_id)
        .execute(&self.pool).await.map_err(db_err)?;
        Ok(())
    }

    // ── Entity / link writes ─────────────────────────────────────────────────

    /// Upsert entity, return (entity_id, is_new).
    pub async fn upsert_entity(
        &self,
        user_id: &str,
        name: &str,
        display_name: &str,
        entity_type: &str,
    ) -> Result<(String, bool), MemoriaError> {
        // Check existing
        let existing = sqlx::query(
            "SELECT entity_id FROM mem_entities WHERE user_id = ? AND name = ?"
        )
        .bind(user_id).bind(name)
        .fetch_optional(&self.pool).await.map_err(db_err)?;

        if let Some(row) = existing {
            let id: String = row.try_get("entity_id").map_err(db_err)?;
            return Ok((id, false));
        }

        let entity_id = new_id()[..32].to_string();
        let now = chrono::Utc::now().naive_utc();
        sqlx::query(
            "INSERT INTO mem_entities (entity_id, user_id, name, display_name, entity_type, created_at) \
             VALUES (?,?,?,?,?,?)"
        )
        .bind(&entity_id).bind(user_id).bind(name).bind(display_name)
        .bind(entity_type).bind(now)
        .execute(&self.pool).await.map_err(db_err)?;
        Ok((entity_id, true))
    }

    pub async fn upsert_memory_entity_link(
        &self,
        memory_id: &str,
        entity_id: &str,
        user_id: &str,
        source: &str,
    ) -> Result<(), MemoriaError> {
        let now = chrono::Utc::now().naive_utc();
        // Weight by extraction source — higher = more confident
        let weight: f32 = match source {
            "manual" => 1.0,
            "llm" => 0.9,
            _ => 0.8, // regex, unknown
        };
        sqlx::query(
            "INSERT INTO mem_memory_entity_links \
             (memory_id, entity_id, user_id, source, weight, created_at) \
             VALUES (?,?,?,?,?,?) \
             ON DUPLICATE KEY UPDATE source = VALUES(source), weight = VALUES(weight)"
        )
        .bind(memory_id).bind(entity_id).bind(user_id).bind(source).bind(weight).bind(now)
        .execute(&self.pool).await.map_err(db_err)?;
        Ok(())
    }

    /// Get entity candidates: memories without entity links (for extract_entities candidates mode).
    pub async fn get_unlinked_memories(
        &self,
        user_id: &str,
        limit: i64,
    ) -> Result<Vec<(String, String)>, MemoriaError> {
        // Memories that have no entry in mem_memory_entity_links
        let rows = sqlx::query(
            "SELECT m.memory_id, m.content FROM mem_memories m \
             WHERE m.user_id = ? AND m.is_active = 1 \
             AND NOT EXISTS (SELECT 1 FROM mem_memory_entity_links l \
                             WHERE l.memory_id = m.memory_id AND l.user_id = m.user_id) \
             ORDER BY m.created_at DESC LIMIT ?"
        )
        .bind(user_id).bind(limit)
        .fetch_all(&self.pool).await.map_err(db_err)?;
        Ok(rows.iter().filter_map(|r| {
            let mid: String = r.try_get("memory_id").ok()?;
            let content: String = r.try_get("content").ok()?;
            Some((mid, content))
        }).collect::<Vec<_>>())
    }

    pub async fn get_user_entities(
        &self,
        user_id: &str,
    ) -> Result<Vec<(String, String)>, MemoriaError> {
        let rows = sqlx::query(
            "SELECT name, entity_type FROM mem_entities WHERE user_id = ? ORDER BY name"
        )
        .bind(user_id)
        .fetch_all(&self.pool).await.map_err(db_err)?;
        Ok(rows.iter().filter_map(|r| {
            let name: String = r.try_get("name").ok()?;
            let etype: String = r.try_get("entity_type").ok()?;
            Some((name, etype))
        }).collect::<Vec<_>>())
    }

    // ── Graph retrieval queries ─────────────────────────────────────────

    /// Vector similarity search on graph nodes (cosine via l2_distance).
    pub async fn search_nodes_vector(
        &self,
        user_id: &str,
        embedding: &[f32],
        top_k: i64,
    ) -> Result<Vec<(GraphNode, f32)>, MemoriaError> {
        let vec_lit = format!(
            "[{}]",
            embedding.iter().map(|f| f.to_string()).collect::<Vec<_>>().join(",")
        );
        let sql = format!(
            "SELECT *, l2_distance(embedding, '{vec_lit}') AS l2_dist \
             FROM memory_graph_nodes \
             WHERE user_id = ? AND is_active = 1 \
               AND embedding IS NOT NULL AND vector_dims(embedding) > 0 \
             ORDER BY l2_dist ASC \
             LIMIT ?"
        );
        let rows = sqlx::query(&sql)
            .bind(user_id)
            .bind(top_k)
            .fetch_all(&self.pool)
            .await
            .map_err(db_err)?;
        Ok(rows
            .iter()
            .map(|r| {
                // Convert L2 distance to cosine similarity for normalized vectors:
                // cos = 1 - L2² / 2
                let l2: f32 = r.try_get::<f32, _>("l2_dist")
                    .or_else(|_| r.try_get::<f64, _>("l2_dist").map(|v| v as f32))
                    .unwrap_or(f32::MAX);
                let sim = (1.0 - l2 * l2 / 2.0).max(0.0);
                (row_to_node(r), sim)
            })
            .collect())
    }

    /// Fulltext (BM25) search on graph nodes.
    pub async fn search_nodes_fulltext(
        &self,
        user_id: &str,
        query: &str,
        top_k: i64,
    ) -> Result<Vec<(GraphNode, f32)>, MemoriaError> {
        if query.trim().is_empty() {
            return Ok(vec![]);
        }
        // Sanitize query for MATCH AGAINST — strip boolean-mode operators and SQL chars
        let safe: String = query
            .chars()
            .filter(|c| *c != '\0')
            .map(|c| if "+-<>()~*\"@'\\".contains(c) { ' ' } else { c })
            .collect();
        let safe: String = safe.split_whitespace().collect::<Vec<_>>().join(" ");
        if safe.is_empty() {
            return Ok(vec![]);
        }
        let sql = format!(
            "SELECT *, MATCH(content) AGAINST('{safe}' IN BOOLEAN MODE) AS ft_score \
             FROM memory_graph_nodes \
             WHERE user_id = ? AND is_active = 1 \
             AND MATCH(content) AGAINST('{safe}' IN BOOLEAN MODE) \
             ORDER BY ft_score DESC LIMIT ?"
        );
        let rows = sqlx::query(&sql)
            .bind(user_id)
            .bind(top_k)
            .fetch_all(&self.pool)
            .await
            .map_err(db_err)?;
        Ok(rows
            .iter()
            .map(|r| {
                let score: f32 = r.try_get("ft_score").unwrap_or(0.0);
                (row_to_node(r), score)
            })
            .collect())
    }

    /// Find entity_id by exact name match.
    pub async fn find_entity_by_name(
        &self,
        user_id: &str,
        name: &str,
    ) -> Result<Option<String>, MemoriaError> {
        let row = sqlx::query(
            "SELECT entity_id FROM mem_entities WHERE user_id = ? AND name = ?",
        )
        .bind(user_id)
        .bind(name)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(row.and_then(|r| r.try_get("entity_id").ok()))
    }

    /// Reverse lookup: entity → memory_ids with weights.
    pub async fn get_memories_by_entity(
        &self,
        entity_id: &str,
        user_id: &str,
        limit: i64,
    ) -> Result<Vec<(String, f32)>, MemoriaError> {
        let rows = sqlx::query(
            "SELECT l.memory_id, l.weight \
             FROM mem_memory_entity_links l \
             JOIN mem_memories m ON l.memory_id = m.memory_id \
             WHERE l.entity_id = ? AND l.user_id = ? AND m.is_active = 1 \
             ORDER BY l.weight DESC, m.created_at DESC \
             LIMIT ?",
        )
        .bind(entity_id)
        .bind(user_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(rows
            .iter()
            .filter_map(|r| {
                let mid: String = r.try_get("memory_id").ok()?;
                let w: f32 = r.try_get("weight").ok()?;
                Some((mid, w))
            })
            .collect())
    }

    /// Bidirectional edge fetch for a set of node IDs.
    /// Returns (incoming, outgoing) maps: node_id → Vec<(peer_id, edge_type, weight)>.
    pub async fn get_edges_bidirectional(
        &self,
        node_ids: &[String],
    ) -> Result<(
        std::collections::HashMap<String, Vec<(String, String, f32)>>,
        std::collections::HashMap<String, Vec<(String, String, f32)>>,
    ), MemoriaError> {
        if node_ids.is_empty() {
            return Ok((Default::default(), Default::default()));
        }
        let ph = node_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        // Outgoing: source_id IN node_ids, peer = target_id
        // Incoming: target_id IN node_ids, peer = source_id
        let sql = format!(
            "SELECT e.source_id, e.target_id, e.edge_type, e.weight, 0 AS direction \
             FROM memory_graph_edges e \
             JOIN memory_graph_nodes n ON n.node_id = e.target_id AND n.is_active = 1 \
             WHERE e.source_id IN ({ph}) \
             UNION ALL \
             SELECT e.source_id, e.target_id, e.edge_type, e.weight, 1 AS direction \
             FROM memory_graph_edges e \
             JOIN memory_graph_nodes n ON n.node_id = e.source_id AND n.is_active = 1 \
             WHERE e.target_id IN ({ph})"
        );
        let mut q = sqlx::query(&sql);
        for id in node_ids {
            q = q.bind(id);
        }
        for id in node_ids {
            q = q.bind(id);
        }
        let rows = q.fetch_all(&self.pool).await.map_err(db_err)?;

        let mut incoming: std::collections::HashMap<String, Vec<(String, String, f32)>> =
            Default::default();
        let mut outgoing: std::collections::HashMap<String, Vec<(String, String, f32)>> =
            Default::default();
        for r in &rows {
            let src: String = r.try_get("source_id").unwrap_or_default();
            let tgt: String = r.try_get("target_id").unwrap_or_default();
            let etype: String = r.try_get("edge_type").unwrap_or_default();
            let w: f32 = r.try_get("weight").unwrap_or(1.0);
            let dir: i32 = r.try_get("direction").unwrap_or(0);
            if dir == 0 {
                // outgoing: anchor=source_id, peer=target_id
                outgoing.entry(src).or_default().push((tgt, etype, w));
            } else {
                // incoming: anchor=target_id, peer=source_id
                incoming.entry(tgt).or_default().push((src, etype, w));
            }
        }
        Ok((incoming, outgoing))
    }

    /// Count active nodes for a user.
    pub async fn count_user_nodes(&self, user_id: &str) -> Result<i64, MemoriaError> {
        let row = sqlx::query(
            "SELECT COUNT(*) as cnt FROM memory_graph_nodes WHERE user_id = ? AND is_active = 1"
        )
        .bind(user_id)
        .fetch_one(&self.pool).await.map_err(db_err)?;
        Ok(row.try_get("cnt").unwrap_or(0))
    }

    /// Get edges between a set of node IDs (for connected components).
    pub async fn get_edges_for_nodes(
        &self,
        node_ids: &[String],
    ) -> Result<Vec<(String, String)>, MemoriaError> {
        if node_ids.is_empty() { return Ok(vec![]); }
        let placeholders = node_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!(
            "SELECT source_id, target_id FROM memory_graph_edges \
             WHERE source_id IN ({placeholders}) OR target_id IN ({placeholders})"
        );
        let mut q = sqlx::query(&sql);
        for id in node_ids { q = q.bind(id); }
        for id in node_ids { q = q.bind(id); }
        let rows = q.fetch_all(&self.pool).await.map_err(db_err)?;
        Ok(rows.iter().filter_map(|r| {
            let src: String = r.try_get("source_id").ok()?;
            let tgt: String = r.try_get("target_id").ok()?;
            Some((src, tgt))
        }).collect::<Vec<_>>())
    }
}

// ── Row helpers ──────────────────────────────────────────────────────────────

fn row_to_node(r: &sqlx::mysql::MySqlRow) -> GraphNode {
    let emb_str: Option<String> = r.try_get("embedding").ok().flatten();
    let embedding = emb_str.map(|s| mo_to_vec(&s));
    row_to_node_inner(r, embedding)
}

fn row_to_node_no_emb(r: &sqlx::mysql::MySqlRow) -> GraphNode {
    row_to_node_inner(r, None)
}

fn row_to_node_inner(r: &sqlx::mysql::MySqlRow, embedding: Option<Vec<f32>>) -> GraphNode {
    let node_type_str: String = r.try_get("node_type").unwrap_or_default();
    let source_nodes_str: Option<String> = r.try_get("source_nodes").ok().flatten();
    let source_nodes = source_nodes_str
        .map(|s| s.split(',').map(String::from).collect())
        .unwrap_or_default();
    GraphNode {
        node_id: r.try_get("node_id").unwrap_or_default(),
        user_id: r.try_get("user_id").unwrap_or_default(),
        node_type: node_type_str.parse().unwrap(),
        content: r.try_get("content").unwrap_or_default(),
        entity_type: r.try_get("entity_type").ok().flatten(),
        embedding,
        memory_id: r.try_get("memory_id").ok().flatten(),
        session_id: r.try_get("session_id").ok().flatten(),
        confidence: r.try_get::<f32, _>("confidence").unwrap_or(0.75),
        trust_tier: r.try_get("trust_tier").unwrap_or_else(|_| "T3".to_string()),
        importance: r.try_get::<f32, _>("importance").unwrap_or(0.0),
        source_nodes,
        conflicts_with: r.try_get("conflicts_with").ok().flatten(),
        conflict_resolution: r.try_get("conflict_resolution").ok().flatten(),
        access_count: r.try_get::<i32, _>("access_count").unwrap_or(0),
        cross_session_count: r.try_get::<i32, _>("cross_session_count").unwrap_or(0),
        is_active: r.try_get::<i16, _>("is_active").unwrap_or(1) != 0,
        superseded_by: r.try_get("superseded_by").ok().flatten(),
        created_at: r.try_get("created_at").ok(),
    }
}

fn mo_to_vec(s: &str) -> Vec<f32> {
    let s = s.trim_start_matches('[').trim_end_matches(']');
    s.split(',').filter_map(|x| x.trim().parse().ok()).collect()
}
