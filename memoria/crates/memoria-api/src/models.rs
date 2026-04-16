//! Request/Response types for the REST API.
//! Mirrors Python's api/models.py and api/_model_types.py.

use memoria_core::{Memory, MemoryType, TrustTier};
use serde::{Deserialize, Serialize};
use std::str::FromStr;

// ── Memory ────────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct StoreRequest {
    pub content: String,
    #[serde(default = "default_memory_type")]
    pub memory_type: String,
    pub session_id: Option<String>,
    pub trust_tier: Option<String>,
    pub initial_confidence: Option<f64>,
    pub observed_at: Option<String>,
    pub source: Option<String>,
}
fn default_memory_type() -> String {
    "semantic".to_string()
}

#[derive(Deserialize)]
pub struct BatchStoreRequest {
    pub memories: Vec<StoreRequest>,
}

#[derive(Deserialize)]
pub struct RetrieveRequest {
    pub query: String,
    #[serde(default = "default_top_k")]
    pub top_k: i64,
    pub session_id: Option<String>,
    /// Explicit strict session filter. Overrides include_cross_session when provided.
    #[serde(default)]
    pub filter_session: Option<bool>,
    /// When false and session_id is set, only return memories from that session.
    #[serde(default = "default_true")]
    pub include_cross_session: bool,
    /// Explain level: false/"none" = off, true/"basic" = basic, "verbose" = per-candidate scores, "analyze" = full
    #[serde(default, deserialize_with = "deserialize_explain")]
    pub explain: String,
}
fn default_top_k() -> i64 {
    5
}
fn default_true() -> bool {
    true
}

impl RetrieveRequest {
    pub fn retrieve_options(&self) -> memoria_service::RetrieveOptions {
        memoria_service::RetrieveOptions::from_session_scope(
            self.session_id.as_deref(),
            self.filter_session,
            Some(self.include_cross_session),
        )
    }
}

fn deserialize_explain<'de, D: serde::Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    use serde::Deserialize;
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum ExplainInput {
        Bool(bool),
        Str(String),
    }
    Ok(match ExplainInput::deserialize(d)? {
        ExplainInput::Bool(true) => "basic".to_string(),
        ExplainInput::Bool(false) => "none".to_string(),
        ExplainInput::Str(s) => s,
    })
}

#[derive(Deserialize)]
pub struct CorrectRequest {
    pub new_content: String,
    pub reason: Option<String>,
}

#[derive(Deserialize)]
pub struct CorrectByQueryRequest {
    pub query: String,
    pub new_content: String,
    pub reason: Option<String>,
}

#[derive(Deserialize)]
pub struct PurgeRequest {
    pub memory_ids: Option<Vec<String>>,
    pub topic: Option<String>,
    pub session_id: Option<String>,
    pub memory_types: Option<Vec<String>>,
    pub reason: Option<String>,
}

pub enum PurgeSelector {
    MemoryIds(Vec<String>),
    Topic(String),
    Session {
        session_id: String,
        memory_types: Option<Vec<MemoryType>>,
    },
    None,
}

impl PurgeRequest {
    pub fn selector(&self) -> Result<PurgeSelector, String> {
        let memory_ids = self.memory_ids.as_ref().filter(|ids| !ids.is_empty());
        let memory_types = self.memory_types.as_ref().filter(|types| !types.is_empty());
        let topic = self
            .topic
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let session_id = self
            .session_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        if memory_types.is_some() && session_id.is_none() {
            return Err("memory_types requires session_id".to_string());
        }

        let selector_count = usize::from(memory_ids.is_some())
            + usize::from(topic.is_some())
            + usize::from(session_id.is_some());
        if selector_count > 1 {
            return Err("provide only one of memory_ids, topic, or session_id".to_string());
        }

        if let Some(ids) = memory_ids {
            return Ok(PurgeSelector::MemoryIds(ids.clone()));
        }
        if let Some(topic) = topic {
            return Ok(PurgeSelector::Topic(topic.to_string()));
        }
        if let Some(session_id) = session_id {
            let memory_types = memory_types
                .map(|types| {
                    types
                        .iter()
                        .map(|memory_type| parse_memory_type(memory_type))
                        .collect::<Result<Vec<_>, _>>()
                })
                .transpose()?
                .filter(|types| !types.is_empty());
            return Ok(PurgeSelector::Session {
                session_id: session_id.to_string(),
                memory_types,
            });
        }
        Ok(PurgeSelector::None)
    }
}

#[derive(Serialize)]
pub struct MemoryResponse {
    pub memory_id: String,
    pub user_id: String,
    pub memory_type: String,
    pub content: String,
    pub trust_tier: String,
    pub initial_confidence: f64,
    pub is_active: bool,
    pub session_id: Option<String>,
    pub observed_at: Option<String>,
    pub created_at: Option<String>,
    pub retrieval_score: Option<f64>,
}

impl From<Memory> for MemoryResponse {
    fn from(m: Memory) -> Self {
        Self {
            memory_id: m.memory_id,
            user_id: m.user_id,
            memory_type: m.memory_type.to_string(),
            content: m.content,
            trust_tier: m.trust_tier.to_string(),
            initial_confidence: m.initial_confidence,
            is_active: m.is_active,
            session_id: m.session_id,
            observed_at: m.observed_at.map(|dt| dt.to_rfc3339()),
            created_at: m.created_at.map(|dt| dt.to_rfc3339()),
            retrieval_score: m.retrieval_score,
        }
    }
}

#[derive(Serialize)]
pub struct ListResponse {
    pub items: Vec<MemoryResponse>,
    pub next_cursor: Option<String>,
}

#[derive(Serialize)]
pub struct PurgeResponse {
    pub purged: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

// ── Governance ────────────────────────────────────────────────────────────────

#[derive(Deserialize, Default)]
pub struct GovernanceRequest {
    #[serde(default)]
    pub force: bool,
}

#[derive(Deserialize, Default)]
pub struct ReflectRequest {
    #[serde(default)]
    pub force: bool,
    #[serde(default = "default_mode")]
    pub mode: String,
}
fn default_mode() -> String {
    "auto".to_string()
}

#[derive(Deserialize, Default)]
pub struct ExtractEntitiesRequest {
    #[serde(default = "default_mode")]
    pub mode: String,
}

#[derive(Deserialize)]
pub struct LinkEntitiesRequest {
    pub entities: Vec<EntityLink>,
}

#[derive(Deserialize)]
pub struct EntityLink {
    pub memory_id: String,
    pub entities: Vec<EntityItem>,
}

#[derive(Deserialize)]
pub struct EntityItem {
    pub name: String,
    #[serde(rename = "type", default = "default_entity_type")]
    pub entity_type: String,
}
fn default_entity_type() -> String {
    "concept".to_string()
}

// ── Snapshots ─────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct CreateSnapshotRequest {
    pub name: String,
    pub description: Option<String>,
}

#[derive(Deserialize)]
pub struct DeleteSnapshotsRequest {
    pub names: Option<Vec<String>>,
    pub prefix: Option<String>,
    pub older_than: Option<String>,
}

// ── Branches ──────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct CreateBranchRequest {
    pub name: String,
    pub from_snapshot: Option<String>,
    pub from_timestamp: Option<String>,
}

#[derive(Deserialize)]
pub struct MergeRequest {
    #[serde(default = "default_strategy")]
    pub strategy: String,
}
fn default_strategy() -> String {
    "accept".to_string()
}

// ── Helpers ───────────────────────────────────────────────────────────────────

pub fn parse_memory_type(s: &str) -> Result<MemoryType, String> {
    MemoryType::from_str(s).map_err(|e| e.to_string())
}

pub fn parse_trust_tier(s: &str) -> Result<TrustTier, String> {
    TrustTier::from_str(s).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::{PurgeRequest, PurgeSelector};

    #[test]
    fn purge_selector_ignores_empty_arrays() {
        let request = PurgeRequest {
            memory_ids: Some(vec![]),
            topic: None,
            session_id: Some("sess-1".to_string()),
            memory_types: Some(vec![]),
            reason: None,
        };

        match request.selector().unwrap() {
            PurgeSelector::Session {
                session_id,
                memory_types,
            } => {
                assert_eq!(session_id, "sess-1");
                assert!(memory_types.is_none());
            }
            _ => panic!("expected session selector"),
        }
    }

    #[test]
    fn purge_selector_empty_memory_types_do_not_require_session() {
        let request = PurgeRequest {
            memory_ids: None,
            topic: None,
            session_id: None,
            memory_types: Some(vec![]),
            reason: None,
        };

        assert!(matches!(request.selector().unwrap(), PurgeSelector::None));
    }
}
