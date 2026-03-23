use async_trait::async_trait;
use std::collections::{HashMap, HashSet};
use std::str::FromStr;
use std::sync::{Arc, Mutex, OnceLock, RwLock};

use chrono::{DateTime, NaiveDateTime, Utc};
use memoria_core::{truncate_utf8, MemoriaError, MemoryType, TrustTier};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{mysql::MySqlPool, Row};
use tokio::{
    time::{sleep, Duration, Instant},
};

use crate::store::{
    db_err, mo_to_vec, sanitize_fulltext_query, sanitize_like_pattern, sanitize_sql_literal,
    vec_to_mo, FeedbackStats, MemoryFeedback, SqlMemoryStore, TierFeedback,
};

const REGISTRY_TABLE: &str = "mem_v2_user_tables";
const ABSTRACT_BYTES: usize = 280;
const DEFAULT_TOP_K: i64 = 20;
const MAX_TOP_K: i64 = 100;
const MAX_MAX_TOKENS: usize = 8_000;
const DEFAULT_FOCUS_BOOST: f64 = 1.25;
const DEFAULT_FOCUS_TTL_SECS: i64 = 3600;
const MAX_FOCUS_TTL_SECS: i64 = 7 * 24 * 3600;
const JOB_LEASE_SECS: i64 = 60;
const MAX_JOB_ATTEMPTS: i32 = 3;
const MAX_JOB_DETAIL_BYTES: usize = 4000;
const MAX_DERIVED_OVERVIEW_BYTES: usize = 320;
const MAX_LINKS_PER_MEMORY: usize = 6;
const MAX_RECALL_LINK_EXPANSION_SEEDS: usize = 5;
const MAX_RELATED_PATHS_PER_ITEM: usize = 3;
const DEFAULT_V2_FEEDBACK_WEIGHT: f64 = 0.1;
const DEFAULT_V2_ENTITY_WEIGHT: f64 = 0.08;
const DEFAULT_V2_LINK_EXPANSION_DECAY: f64 = 0.6;
const DEFAULT_V2_TYPE_AFFINITY_BOOST: f64 = 1.15;
const DEFAULT_SYNC_ENRICH_TIMEOUT_SECS: i64 = 30;
const REFLECT_V2_SOURCE_KIND: &str = "reflect_v2";
const REFLECT_V2_LINK_TYPE: &str = "reflection";
const REFLECT_V2_ACTOR: &str = "reflect_v2_internal";

fn uuid7_id() -> String {
    uuid::Uuid::now_v7().simple().to_string()
}

fn uuid_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

fn v2_entity_id(name: &str, entity_type: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(name.as_bytes());
    hasher.update(b":");
    hasher.update(entity_type.as_bytes());
    let digest = hasher.finalize();
    digest[..16]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>()
}

fn should_attempt_schema_patch(table: &str) -> bool {
    static ATTEMPTED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let attempted = ATTEMPTED.get_or_init(|| Mutex::new(HashSet::new()));
    let mut attempted = attempted.lock().expect("schema patch mutex poisoned");
    attempted.insert(table.to_string())
}

fn estimate_tokens(s: &str) -> i32 {
    s.chars().count().div_ceil(4).max(1) as i32
}

fn source_field(source: &Option<serde_json::Value>, key: &str) -> Option<String> {
    source
        .as_ref()
        .and_then(|v| v.as_object())
        .and_then(|m| m.get(key))
        .and_then(|v| v.as_str())
        .map(ToOwned::to_owned)
}

fn to_utc(dt: NaiveDateTime) -> DateTime<Utc> {
    DateTime::<Utc>::from_naive_utc_and_offset(dt, Utc)
}

fn derive_overview_text(source_text: &str, abstract_text: &str) -> String {
    let normalized = source_text.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.is_empty() {
        return abstract_text.trim().to_string();
    }

    let mut overview = String::new();
    let mut sentences = 0usize;
    for segment in normalized.split_inclusive(['.', '!', '?']) {
        let segment = segment.trim();
        if segment.is_empty() {
            continue;
        }
        let candidate = if overview.is_empty() {
            segment.to_string()
        } else {
            format!("{overview} {segment}")
        };
        if candidate.len() > MAX_DERIVED_OVERVIEW_BYTES {
            break;
        }
        overview = candidate;
        sentences += 1;
        if sentences >= 2 {
            break;
        }
    }

    if overview.is_empty() {
        truncate_utf8(&normalized, MAX_DERIVED_OVERVIEW_BYTES)
            .trim()
            .to_string()
    } else {
        overview
    }
}

fn derive_detail_text(source_text: &str, abstract_text: &str) -> String {
    let detail = truncate_utf8(source_text.trim(), MAX_JOB_DETAIL_BYTES)
        .trim()
        .to_string();
    if detail.is_empty() {
        abstract_text.trim().to_string()
    } else {
        detail
    }
}

fn normalize_tags(tags: Vec<String>) -> Vec<String> {
    let mut normalized = tags
        .into_iter()
        .map(|tag| tag.trim().to_lowercase())
        .filter(|tag| !tag.is_empty())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    normalized.sort();
    normalized
}

fn normalize_reflect_source_ids<I>(ids: I) -> Vec<String>
where
    I: IntoIterator<Item = String>,
{
    let mut normalized = ids
        .into_iter()
        .map(|id| id.trim().to_string())
        .filter(|id| !id.is_empty())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    normalized.sort();
    normalized
}

fn reflect_source_key(source_ids: &[String]) -> String {
    source_ids.join("|")
}

fn reflect_common_session_id(candidate: &ReflectV2Candidate) -> Option<String> {
    let sessions = candidate
        .memories
        .iter()
        .map(|memory| {
            memory
                .session_id
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToOwned::to_owned)
        })
        .collect::<Vec<_>>();
    let first = sessions.first().cloned().flatten()?;
    if sessions
        .iter()
        .all(|value| value.as_deref() == Some(first.as_str()))
    {
        Some(first)
    } else {
        None
    }
}

fn synthesize_reflect_content(candidate: &ReflectV2Candidate) -> String {
    let prefix = match candidate.signal.as_str() {
        "cross_session_linked_cluster" => "Cross-session reflection",
        "linked_cluster" => "Linked reflection",
        "session_cluster" => "Session reflection",
        _ => "Reflection",
    };
    let snippets = candidate
        .memories
        .iter()
        .take(4)
        .map(|memory| memory.abstract_text.trim())
        .filter(|snippet| !snippet.is_empty())
        .collect::<Vec<_>>();
    let mut content = if snippets.is_empty() {
        format!(
            "{prefix} across {} related memories.",
            candidate.memory_count
        )
    } else {
        format!("{prefix}: {}", snippets.join(" | "))
    };
    if candidate.memories.len() > snippets.len() {
        content.push_str(&format!(
            " | +{} more related memories",
            candidate.memories.len() - snippets.len()
        ));
    }
    truncate_utf8(content.trim(), MAX_JOB_DETAIL_BYTES)
        .trim()
        .to_string()
}

fn build_recall_tag_clause(
    family: &MemoryV2TableFamily,
    tags: &[String],
    tag_filter_mode: &str,
) -> Option<String> {
    if tags.is_empty() {
        return None;
    }
    let tags_sql = tags
        .iter()
        .map(|tag| format!("'{}'", sanitize_sql_literal(tag)))
        .collect::<Vec<_>>()
        .join(", ");
    Some(match tag_filter_mode {
        "all" => format!(
            "h.memory_id IN (SELECT t.memory_id FROM {} t WHERE t.tag IN ({}) GROUP BY t.memory_id HAVING COUNT(DISTINCT t.tag) = {})",
            family.tags_table,
            tags_sql,
            tags.len()
        ),
        _ => format!(
            "h.memory_id IN (SELECT DISTINCT t.memory_id FROM {} t WHERE t.tag IN ({}))",
            family.tags_table, tags_sql
        ),
    })
}

fn build_recall_time_clauses(
    created_after: Option<DateTime<Utc>>,
    created_before: Option<DateTime<Utc>>,
) -> Vec<String> {
    let mut clauses = Vec::new();
    if let Some(created_after) = created_after {
        clauses.push(format!(
            "h.created_at >= '{}'",
            sanitize_sql_literal(
                &created_after
                    .naive_utc()
                    .format("%Y-%m-%d %H:%M:%S%.6f")
                    .to_string()
            )
        ));
    }
    if let Some(created_before) = created_before {
        clauses.push(format!(
            "h.created_at <= '{}'",
            sanitize_sql_literal(
                &created_before
                    .naive_utc()
                    .format("%Y-%m-%d %H:%M:%S%.6f")
                    .to_string()
            )
        ));
    }
    clauses
}

fn normalize_job_status(status: &str) -> &str {
    match status {
        "leased" => "in_progress",
        other => other,
    }
}

fn same_related_lineage_step(
    left: &MemoryV2RelatedLineageStep,
    right: &MemoryV2RelatedLineageStep,
) -> bool {
    left.from_memory_id == right.from_memory_id
        && left.to_memory_id == right.to_memory_id
        && left.direction == right.direction
        && left.link_type == right.link_type
}

fn same_related_path(left: &MemoryV2RelatedPath, right: &MemoryV2RelatedPath) -> bool {
    left.hop_distance == right.hop_distance
        && left.via_memory_ids == right.via_memory_ids
        && left.lineage.len() == right.lineage.len()
        && left
            .lineage
            .iter()
            .zip(right.lineage.iter())
            .all(|(left, right)| same_related_lineage_step(left, right))
}

fn related_path_signature(path: &MemoryV2RelatedPath) -> String {
    let via = path.via_memory_ids.join(">");
    let lineage = path
        .lineage
        .iter()
        .map(|step| {
            format!(
                "{}:{}:{}:{}",
                step.from_memory_id,
                step.to_memory_id,
                step.direction.as_str(),
                step.link_type
            )
        })
        .collect::<Vec<_>>()
        .join("|");
    format!("{via}#{lineage}")
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MemoryV2TableFamily {
    pub suffix: String,
    pub events_table: String,
    pub heads_table: String,
    pub content_versions_table: String,
    pub index_docs_table: String,
    pub links_table: String,
    pub entities_table: String,
    pub memory_entities_table: String,
    pub focus_table: String,
    pub jobs_table: String,
    pub tags_table: String,
    pub stats_table: String,
    pub feedback_table: String,
}

impl MemoryV2TableFamily {
    pub fn for_user(user_id: &str) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(user_id.as_bytes());
        let digest = hasher.finalize();
        let suffix: String = digest[..10].iter().map(|b| format!("{b:02x}")).collect();
        Self {
            suffix: suffix.clone(),
            events_table: format!("mem_v2_evt_{suffix}"),
            heads_table: format!("mem_v2_head_{suffix}"),
            content_versions_table: format!("mem_v2_cver_{suffix}"),
            index_docs_table: format!("mem_v2_idx_{suffix}"),
            links_table: format!("mem_v2_link_{suffix}"),
            entities_table: format!("mem_v2_entity_{suffix}"),
            memory_entities_table: format!("mem_v2_ment_{suffix}"),
            focus_table: format!("mem_v2_focus_{suffix}"),
            jobs_table: format!("mem_v2_job_{suffix}"),
            tags_table: format!("mem_v2_tag_{suffix}"),
            stats_table: format!("mem_v2_stat_{suffix}"),
            feedback_table: format!("mem_v2_feedback_{suffix}"),
        }
    }

    fn validate(&self) -> Result<(), MemoriaError> {
        for table in [
            &self.events_table,
            &self.heads_table,
            &self.content_versions_table,
            &self.index_docs_table,
            &self.links_table,
            &self.entities_table,
            &self.memory_entities_table,
            &self.focus_table,
            &self.jobs_table,
            &self.tags_table,
            &self.stats_table,
            &self.feedback_table,
        ] {
            SqlMemoryStore::validate_table_name(table)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct MemoryV2RememberInput {
    pub content: String,
    pub memory_type: MemoryType,
    pub session_id: Option<String>,
    pub importance: Option<f64>,
    pub trust_tier: Option<TrustTier>,
    pub tags: Vec<String>,
    pub source: Option<serde_json::Value>,
    pub embedding: Option<Vec<f32>>,
    pub actor: String,
}

#[derive(Debug, Clone)]
pub struct MemoryV2RememberResult {
    pub memory_id: String,
    pub abstract_text: String,
    pub has_overview: bool,
    pub has_detail: bool,
}

#[derive(Debug, Clone)]
pub struct MemoryV2UpdateInput {
    pub memory_id: String,
    pub content: Option<String>,
    pub importance: Option<f64>,
    pub trust_tier: Option<TrustTier>,
    pub tags_add: Vec<String>,
    pub tags_remove: Vec<String>,
    pub embedding: Option<Vec<f32>>,
    pub actor: String,
    pub reason: Option<String>,
}

#[derive(Debug, Clone)]
pub struct MemoryV2UpdateResult {
    pub memory_id: String,
    pub abstract_text: String,
    pub updated_at: DateTime<Utc>,
    pub has_overview: bool,
    pub has_detail: bool,
}

#[derive(Debug, Clone)]
pub struct TagV2Summary {
    pub tag: String,
    pub memory_count: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListV2Cursor {
    pub created_at: DateTime<Utc>,
    pub memory_id: String,
}

#[derive(Debug, Clone)]
pub struct ListV2Filter {
    pub limit: i64,
    pub cursor: Option<ListV2Cursor>,
    pub memory_type: Option<MemoryType>,
    pub session_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ListV2Item {
    pub memory_id: String,
    pub abstract_text: String,
    pub memory_type: MemoryType,
    pub session_id: Option<String>,
    pub created_at: DateTime<Utc>,
    pub has_overview: bool,
    pub has_detail: bool,
}

#[derive(Debug, Clone)]
pub struct ListV2Result {
    pub items: Vec<ListV2Item>,
    pub next_cursor: Option<ListV2Cursor>,
}

#[derive(Debug, Clone)]
pub struct ProfileV2Filter {
    pub limit: i64,
    pub cursor: Option<ListV2Cursor>,
    pub session_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ProfileV2Item {
    pub memory_id: String,
    pub content: String,
    pub abstract_text: String,
    pub session_id: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub trust_tier: TrustTier,
    pub confidence: f64,
    pub importance: f64,
    pub has_overview: bool,
    pub has_detail: bool,
}

#[derive(Debug, Clone)]
pub struct ProfileV2Result {
    pub items: Vec<ProfileV2Item>,
    pub next_cursor: Option<ListV2Cursor>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntityV2Cursor {
    pub updated_at: DateTime<Utc>,
    pub entity_id: String,
}

#[derive(Debug, Clone)]
pub struct EntityV2Filter {
    pub limit: i64,
    pub cursor: Option<EntityV2Cursor>,
    pub query: Option<String>,
    pub entity_type: Option<String>,
    pub memory_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct EntityV2Item {
    pub entity_id: String,
    pub name: String,
    pub display_name: String,
    pub entity_type: String,
    pub memory_count: i64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct EntityV2ListResult {
    pub items: Vec<EntityV2Item>,
    pub next_cursor: Option<EntityV2Cursor>,
}

#[derive(Debug, Clone)]
pub struct EntityV2ExtractResult {
    pub processed_memories: i64,
    pub entities_found: i64,
    pub links_written: i64,
}

#[derive(Debug, Clone)]
pub struct ReflectV2Filter {
    pub limit: i64,
    pub mode: String,
    pub session_id: Option<String>,
    pub min_cluster_size: i64,
    pub min_link_strength: f64,
}

#[derive(Debug, Clone)]
pub struct ReflectV2MemoryItem {
    pub memory_id: String,
    pub abstract_text: String,
    pub memory_type: MemoryType,
    pub session_id: Option<String>,
    pub importance: f64,
}

#[derive(Debug, Clone)]
pub struct ReflectV2Candidate {
    pub signal: String,
    pub importance: f64,
    pub memory_count: i64,
    pub session_count: i64,
    pub link_count: i64,
    pub memories: Vec<ReflectV2MemoryItem>,
}

#[derive(Debug, Clone)]
pub struct ReflectV2Result {
    pub mode: String,
    pub synthesized: bool,
    pub scenes_created: i64,
    pub candidates: Vec<ReflectV2Candidate>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpandLevel {
    Overview,
    Detail,
    Links,
}

impl ExpandLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Overview => "overview",
            Self::Detail => "detail",
            Self::Links => "links",
        }
    }
}

#[derive(Debug, Clone)]
pub struct LinkV2Ref {
    pub memory_id: String,
    pub abstract_text: String,
    pub link_type: String,
    pub strength: f64,
    pub provenance: MemoryV2LinkProvenance,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LinkDirection {
    Outbound,
    Inbound,
    Both,
}

impl LinkDirection {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Outbound => "outbound",
            Self::Inbound => "inbound",
            Self::Both => "both",
        }
    }
}

#[derive(Debug, Clone)]
pub struct MemoryV2LinksRequest {
    pub memory_id: String,
    pub direction: LinkDirection,
    pub limit: i64,
    pub link_type: Option<String>,
    pub min_strength: f64,
}

#[derive(Debug, Clone)]
pub struct MemoryV2LinkEvidenceDetail {
    pub evidence_type: String,
    pub strength: f64,
    pub overlap_count: Option<i64>,
    pub source_tag_count: Option<i64>,
    pub target_tag_count: Option<i64>,
    pub vector_distance: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct MemoryV2LinkExtractionTrace {
    pub content_version_id: Option<String>,
    pub derivation_state: Option<String>,
    pub latest_job_status: Option<String>,
    pub latest_job_attempts: Option<i32>,
    pub latest_job_updated_at: Option<DateTime<Utc>>,
    pub latest_job_error: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct MemoryV2LinkProvenance {
    pub evidence_types: Vec<String>,
    pub primary_evidence_type: Option<String>,
    pub primary_evidence_strength: Option<f64>,
    pub refined: bool,
    pub evidence: Vec<MemoryV2LinkEvidenceDetail>,
    pub extraction_trace: Option<MemoryV2LinkExtractionTrace>,
}

#[derive(Debug, Clone)]
pub struct MemoryV2LinkItem {
    pub memory_id: String,
    pub abstract_text: String,
    pub memory_type: MemoryType,
    pub session_id: Option<String>,
    pub has_overview: bool,
    pub has_detail: bool,
    pub link_type: String,
    pub strength: f64,
    pub direction: LinkDirection,
    pub provenance: MemoryV2LinkProvenance,
}

#[derive(Debug, Clone)]
pub struct MemoryV2LinkTypeSummary {
    pub link_type: String,
    pub outbound_count: i64,
    pub inbound_count: i64,
}

#[derive(Debug, Clone)]
pub struct MemoryV2LinkSummary {
    pub outbound_count: i64,
    pub inbound_count: i64,
    pub total_count: i64,
    pub link_types: Vec<MemoryV2LinkTypeSummary>,
}

#[derive(Debug, Clone)]
pub struct MemoryV2LinksResult {
    pub memory_id: String,
    pub summary: MemoryV2LinkSummary,
    pub items: Vec<MemoryV2LinkItem>,
}

#[derive(Debug, Clone)]
pub struct MemoryV2RelatedRequest {
    pub memory_id: String,
    pub limit: i64,
    pub min_strength: f64,
    pub max_hops: i64,
}

#[derive(Debug, Clone)]
pub struct MemoryV2RelatedLineageStep {
    pub from_memory_id: String,
    pub to_memory_id: String,
    pub direction: LinkDirection,
    pub link_type: String,
    pub strength: f64,
    pub provenance: MemoryV2LinkProvenance,
}

#[derive(Debug, Clone)]
pub struct MemoryV2RelatedPath {
    pub hop_distance: i64,
    pub strength: f64,
    pub via_memory_ids: Vec<String>,
    pub lineage: Vec<MemoryV2RelatedLineageStep>,
    pub path_rank: i64,
    pub selected: bool,
    pub selection_reason: String,
}

#[derive(Debug, Clone)]
pub struct MemoryV2FocusMatch {
    pub focus_type: String,
    pub value: String,
    pub boost: f64,
}

#[derive(Debug, Clone)]
pub struct MemoryV2RecallExpansionSource {
    pub seed_memory_id: String,
    pub seed_score: f64,
    pub link_type: String,
    pub link_strength: f64,
    pub bonus: f64,
}

#[derive(Debug, Clone)]
pub struct MemoryV2RecallRanking {
    pub final_score: f64,
    pub base_score: f64,
    pub vector_component: f64,
    pub keyword_component: f64,
    pub confidence_component: f64,
    pub importance_component: f64,
    pub entity_component: f64,
    pub link_bonus: f64,
    pub linked_expansion_applied: bool,
    pub temporal_decay_applied: bool,
    pub age_hours: f64,
    pub temporal_half_life_hours: f64,
    pub temporal_multiplier: f64,
    pub session_affinity_applied: bool,
    pub session_affinity_multiplier: f64,
    pub access_count: i32,
    pub access_multiplier: f64,
    pub feedback_multiplier: f64,
    pub focus_boost: f64,
    pub type_affinity_boost: f64,
    pub focus_matches: Vec<MemoryV2FocusMatch>,
    pub expansion_sources: Vec<MemoryV2RecallExpansionSource>,
}

impl Default for MemoryV2RecallRanking {
    fn default() -> Self {
        Self {
            final_score: 0.0,
            base_score: 0.0,
            vector_component: 0.0,
            keyword_component: 0.0,
            confidence_component: 0.0,
            importance_component: 0.0,
            entity_component: 0.0,
            link_bonus: 0.0,
            linked_expansion_applied: false,
            temporal_decay_applied: false,
            age_hours: 0.0,
            temporal_half_life_hours: 0.0,
            temporal_multiplier: 1.0,
            session_affinity_applied: false,
            session_affinity_multiplier: 1.0,
            access_count: 0,
            access_multiplier: 1.0,
            feedback_multiplier: 1.0,
            focus_boost: 1.0,
            type_affinity_boost: 1.0,
            focus_matches: Vec::new(),
            expansion_sources: Vec::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct MemoryV2RelatedRanking {
    pub same_hop_score: f64,
    pub base_strength: f64,
    pub session_affinity_applied: bool,
    pub session_affinity_multiplier: f64,
    pub access_count: i32,
    pub access_multiplier: f64,
    pub feedback_multiplier: f64,
    pub content_multiplier: f64,
    pub focus_boost: f64,
    pub focus_matches: Vec<MemoryV2FocusMatch>,
}

impl Default for MemoryV2RelatedRanking {
    fn default() -> Self {
        Self {
            same_hop_score: 0.0,
            base_strength: 0.0,
            session_affinity_applied: false,
            session_affinity_multiplier: 1.0,
            access_count: 0,
            access_multiplier: 1.0,
            feedback_multiplier: 1.0,
            content_multiplier: 1.0,
            focus_boost: 1.0,
            focus_matches: Vec::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct MemoryV2RelatedItem {
    pub memory_id: String,
    pub abstract_text: String,
    pub memory_type: MemoryType,
    pub session_id: Option<String>,
    pub has_overview: bool,
    pub has_detail: bool,
    pub hop_distance: i64,
    pub strength: f64,
    pub via_memory_ids: Vec<String>,
    pub directions: Vec<LinkDirection>,
    pub link_types: Vec<String>,
    pub lineage: Vec<MemoryV2RelatedLineageStep>,
    pub supporting_path_count: i64,
    pub supporting_paths_truncated: bool,
    pub supporting_paths: Vec<MemoryV2RelatedPath>,
    pub feedback_impact: MemoryV2FeedbackImpact,
    pub ranking: MemoryV2RelatedRanking,
}

#[derive(Debug, Clone)]
pub struct MemoryV2RelatedHopSummary {
    pub hop_distance: i64,
    pub count: i64,
}

#[derive(Debug, Clone)]
pub struct MemoryV2RelatedLinkTypeSummary {
    pub link_type: String,
    pub count: i64,
}

#[derive(Debug, Clone)]
pub struct MemoryV2RelatedSummary {
    pub discovered_count: i64,
    pub returned_count: i64,
    pub truncated: bool,
    pub by_hop: Vec<MemoryV2RelatedHopSummary>,
    pub link_types: Vec<MemoryV2RelatedLinkTypeSummary>,
}

#[derive(Debug, Clone)]
pub struct MemoryV2RelatedResult {
    pub memory_id: String,
    pub summary: MemoryV2RelatedSummary,
    pub items: Vec<MemoryV2RelatedItem>,
}

#[derive(Debug, Clone)]
pub struct MemoryV2JobsRequest {
    pub memory_id: String,
    pub limit: i64,
}

#[derive(Debug, Clone)]
pub struct MemoryV2JobItem {
    pub job_id: String,
    pub job_type: String,
    pub status: String,
    pub attempts: i32,
    pub available_at: DateTime<Utc>,
    pub leased_until: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct MemoryV2JobTypeSummary {
    pub job_type: String,
    pub pending_count: i64,
    pub in_progress_count: i64,
    pub done_count: i64,
    pub failed_count: i64,
    pub latest_status: String,
    pub latest_error: Option<String>,
    pub latest_updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct MemoryV2JobsResult {
    pub memory_id: String,
    pub derivation_state: String,
    pub has_overview: bool,
    pub has_detail: bool,
    pub link_count: i64,
    pub pending_count: i64,
    pub in_progress_count: i64,
    pub done_count: i64,
    pub failed_count: i64,
    pub job_types: Vec<MemoryV2JobTypeSummary>,
    pub items: Vec<MemoryV2JobItem>,
}

#[derive(Debug, Clone)]
pub struct MemoryV2JobMetrics {
    pub pending_count: i64,
    pub in_progress_count: i64,
    pub failed_count: i64,
    pub avg_processing_time_ms: f64,
    pub oldest_pending_age_secs: i64,
}

#[derive(Debug, Clone)]
pub struct MemoryV2StatsByType {
    pub memory_type: String,
    pub total_count: i64,
    pub active_count: i64,
    pub forgotten_count: i64,
}

#[derive(Debug, Clone)]
pub struct MemoryV2TagStats {
    pub unique_count: i64,
    pub assignment_count: i64,
}

#[derive(Debug, Clone)]
pub struct MemoryV2JobStats {
    pub total_count: i64,
    pub pending_count: i64,
    pub in_progress_count: i64,
    pub done_count: i64,
    pub failed_count: i64,
}

#[derive(Debug, Clone)]
pub struct MemoryV2StatsResult {
    pub total_memories: i64,
    pub active_memories: i64,
    pub forgotten_memories: i64,
    pub distinct_sessions: i64,
    pub has_overview_count: i64,
    pub has_detail_count: i64,
    pub active_direct_links: i64,
    pub active_focus_count: i64,
    pub tags: MemoryV2TagStats,
    pub jobs: MemoryV2JobStats,
    pub feedback: FeedbackStats,
    pub by_type: Vec<MemoryV2StatsByType>,
}

#[derive(Debug, Clone)]
pub struct MemoryV2ExpandResult {
    pub memory_id: String,
    pub level: ExpandLevel,
    pub abstract_text: String,
    pub overview_text: Option<String>,
    pub detail_text: Option<String>,
    pub links: Option<Vec<LinkV2Ref>>,
}

#[derive(Debug, Clone)]
pub struct FocusV2Input {
    pub focus_type: String,
    pub value: String,
    pub boost: Option<f64>,
    pub ttl_secs: Option<i64>,
    pub actor: String,
}

#[derive(Debug, Clone)]
pub struct FocusV2Result {
    pub focus_id: String,
    pub focus_type: String,
    pub value: String,
    pub boost: f64,
    pub active_until: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct MemoryV2FeedbackSummary {
    pub memory_id: String,
    pub feedback: MemoryFeedback,
    pub last_feedback_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone)]
pub struct MemoryV2FeedbackEntry {
    pub feedback_id: String,
    pub signal: String,
    pub context: Option<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct MemoryV2FeedbackHistoryResult {
    pub memory_id: String,
    pub items: Vec<MemoryV2FeedbackEntry>,
}

#[derive(Debug, Clone)]
pub struct MemoryV2FeedbackFeedItem {
    pub feedback_id: String,
    pub memory_id: String,
    pub abstract_text: Option<String>,
    pub signal: String,
    pub context: Option<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct MemoryV2FeedbackFeedResult {
    pub items: Vec<MemoryV2FeedbackFeedItem>,
}

#[derive(Debug, Clone)]
pub struct MemoryV2FeedbackImpact {
    pub counts: MemoryFeedback,
    pub multiplier: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryV2RecallPath {
    Direct,
    ExpandedOnly,
    DirectAndExpanded,
}

impl MemoryV2RecallPath {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::ExpandedOnly => "expanded_only",
            Self::DirectAndExpanded => "direct_and_expanded",
        }
    }
}

#[derive(Debug, Clone)]
pub struct MemoryV2RecallPathSummary {
    pub retrieval_path: MemoryV2RecallPath,
    pub discovered_count: i64,
    pub returned_count: i64,
}

#[derive(Debug, Clone)]
pub struct MemoryV2RecallSummary {
    pub discovered_count: i64,
    pub returned_count: i64,
    pub truncated: bool,
    pub by_retrieval_path: Vec<MemoryV2RecallPathSummary>,
}

impl Default for MemoryV2RecallSummary {
    fn default() -> Self {
        Self {
            discovered_count: 0,
            returned_count: 0,
            truncated: false,
            by_retrieval_path: vec![
                MemoryV2RecallPathSummary {
                    retrieval_path: MemoryV2RecallPath::Direct,
                    discovered_count: 0,
                    returned_count: 0,
                },
                MemoryV2RecallPathSummary {
                    retrieval_path: MemoryV2RecallPath::ExpandedOnly,
                    discovered_count: 0,
                    returned_count: 0,
                },
                MemoryV2RecallPathSummary {
                    retrieval_path: MemoryV2RecallPath::DirectAndExpanded,
                    discovered_count: 0,
                    returned_count: 0,
                },
            ],
        }
    }
}

#[derive(Debug, Clone)]
pub struct MemoryV2HistoryEntry {
    pub event_id: String,
    pub event_type: String,
    pub actor: String,
    pub processing_state: String,
    pub payload: serde_json::Value,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct MemoryV2HistoryResult {
    pub memory_id: String,
    pub items: Vec<MemoryV2HistoryEntry>,
}

impl Default for MemoryV2FeedbackImpact {
    fn default() -> Self {
        Self {
            counts: MemoryFeedback::default(),
            multiplier: 1.0,
        }
    }
}

#[derive(Debug, Clone)]
pub struct RecallV2Request {
    pub query: String,
    pub top_k: i64,
    pub max_tokens: usize,
    pub session_only: bool,
    pub session_id: Option<String>,
    pub memory_type: Option<MemoryType>,
    pub tags: Vec<String>,
    pub tag_filter_mode: String,
    pub created_after: Option<DateTime<Utc>>,
    pub created_before: Option<DateTime<Utc>>,
    pub with_overview: bool,
    pub with_links: bool,
    pub expand_links: bool,
    pub query_embedding: Option<Vec<f32>>,
}

#[derive(Debug, Clone, Default)]
pub struct RememberV2Options {
    pub sync_enrich: bool,
    pub enrich_timeout_secs: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct MemoryV2RecallItem {
    pub memory_id: String,
    pub abstract_text: String,
    pub overview_text: Option<String>,
    pub score: f64,
    pub memory_type: MemoryType,
    pub confidence: f64,
    pub has_overview: bool,
    pub has_detail: bool,
    pub access_count: i32,
    pub link_count: i64,
    pub has_related: bool,
    pub retrieval_path: MemoryV2RecallPath,
    pub links: Option<Vec<LinkV2Ref>>,
    pub feedback_impact: MemoryV2FeedbackImpact,
    pub ranking: MemoryV2RecallRanking,
}

#[derive(Debug, Clone)]
pub struct MemoryV2RecallResult {
    pub summary: MemoryV2RecallSummary,
    pub memories: Vec<MemoryV2RecallItem>,
    pub token_used: usize,
    pub has_more: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct V2DerivedViews {
    pub overview_text: String,
    pub detail_text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct V2LinkCandidate {
    pub target_memory_id: String,
    pub abstract_text: String,
    pub link_type: String,
    pub strength: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct V2LinkSuggestion {
    pub target_memory_id: String,
    pub link_type: String,
    pub strength: f64,
}

/// An entity candidate passed to `MemoryV2JobEnricher::refine_entities`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct V2EntityCandidate {
    pub name: String,
    pub display: String,
    pub entity_type: String,
}

/// A refined or additional entity returned by `MemoryV2JobEnricher::refine_entities`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct V2EntitySuggestion {
    pub name: String,
    pub display: String,
    pub entity_type: String,
}

#[async_trait]
pub trait MemoryV2JobEnricher: Send + Sync {
    async fn derive_views(
        &self,
        source_text: &str,
        abstract_text: &str,
    ) -> Result<Option<V2DerivedViews>, MemoriaError>;

    async fn refine_links(
        &self,
        source_abstract: &str,
        candidates: &[V2LinkCandidate],
    ) -> Result<Option<Vec<V2LinkSuggestion>>, MemoriaError>;

    /// Optionally refine or augment the regex-extracted entities for a memory.
    /// Return `Ok(None)` to accept the regex baseline as-is.
    /// Return `Ok(Some(entities))` to replace the baseline with the refined set.
    async fn refine_entities(
        &self,
        _source_text: &str,
        _regex_entities: &[V2EntityCandidate],
    ) -> Result<Option<Vec<V2EntitySuggestion>>, MemoriaError> {
        Ok(None)
    }
}

#[derive(Debug, Clone)]
struct RecallCandidate {
    memory_id: String,
    abstract_text: String,
    overview_text: Option<String>,
    memory_type: MemoryType,
    session_id: Option<String>,
    created_at: DateTime<Utc>,
    confidence: f64,
    importance: f64,
    has_overview: bool,
    has_detail: bool,
    abstract_tokens: i32,
    overview_tokens: i32,
    direct_match: bool,
    vector_score: f64,
    keyword_score: f64,
    entity_score: f64,
    link_bonus: f64,
    expansion_sources: Vec<MemoryV2RecallExpansionSource>,
    access_count: i32,
    link_count: i64,
    session_boost: f64,
    focus_boost: f64,
    type_affinity_boost: f64,
    focus_matches: Vec<MemoryV2FocusMatch>,
    feedback: MemoryFeedback,
}

#[derive(Debug, Clone)]
struct ActiveFocus {
    focus_type: String,
    value: String,
    boost: f64,
}

#[derive(Debug, Clone)]
struct ClaimedV2Job {
    family: MemoryV2TableFamily,
    job_id: String,
    job_type: String,
    memory_id: String,
    content_version_id: String,
    attempts: i32,
}

#[derive(Clone)]
pub struct MemoryV2Store {
    pool: MySqlPool,
    embedding_dim: usize,
    family_cache: Arc<RwLock<HashSet<String>>>,
    family_init_inflight: Arc<Mutex<HashSet<String>>>,
}

struct FamilyInitGuard {
    inflight: Arc<Mutex<HashSet<String>>>,
    user_id: String,
}

impl Drop for FamilyInitGuard {
    fn drop(&mut self) {
        match self.inflight.lock() {
            Ok(mut inflight) => {
                inflight.remove(&self.user_id);
            }
            Err(poisoned) => {
                let mut inflight = poisoned.into_inner();
                inflight.remove(&self.user_id);
            }
        }
    }
}

impl MemoryV2Store {
    pub fn new(pool: MySqlPool, embedding_dim: usize) -> Self {
        Self {
            pool,
            embedding_dim,
            family_cache: Arc::new(RwLock::new(HashSet::new())),
            family_init_inflight: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    fn validate_feedback_signal(signal: &str) -> Result<(), MemoriaError> {
        if matches!(signal, "useful" | "irrelevant" | "outdated" | "wrong") {
            return Ok(());
        }
        Err(MemoriaError::Validation(format!(
            "Invalid signal '{}'. Must be one of: useful, irrelevant, outdated, wrong",
            signal
        )))
    }

    pub fn preview_abstract(content: &str) -> String {
        truncate_utf8(content.trim(), ABSTRACT_BYTES)
            .trim()
            .to_string()
    }

    fn validate_embedding_dim(&self, embedding: &[f32], field: &str) -> Result<(), MemoriaError> {
        if !embedding.is_empty() && embedding.len() != self.embedding_dim {
            return Err(MemoriaError::Validation(format!(
                "{field} embedding dimension {} does not match configured dimension {}",
                embedding.len(),
                self.embedding_dim
            )));
        }
        Ok(())
    }

    async fn insert_tags_tx(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
        family: &MemoryV2TableFamily,
        memory_id: &str,
        tags: &[String],
        now: NaiveDateTime,
    ) -> Result<(), MemoriaError> {
        if tags.is_empty() {
            return Ok(());
        }
        let placeholders = tags
            .iter()
            .map(|_| "(?, ?, ?)")
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "INSERT INTO {} (memory_id, tag, created_at) VALUES {} \
             ON DUPLICATE KEY UPDATE created_at = created_at",
            family.tags_table, placeholders
        );
        let mut q = sqlx::query(&sql);
        for tag in tags {
            q = q.bind(memory_id).bind(tag).bind(now);
        }
        q.execute(&mut **tx).await.map_err(db_err)?;
        Ok(())
    }

    async fn insert_links_tx(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
        family: &MemoryV2TableFamily,
        links: &[(String, String, String, f64)],
        now: NaiveDateTime,
    ) -> Result<(), MemoriaError> {
        if links.is_empty() {
            return Ok(());
        }
        let placeholders = links
            .iter()
            .map(|_| "(?, ?, ?, ?, ?, ?)")
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "INSERT INTO {} (link_id, memory_id, target_memory_id, link_type, strength, created_at) VALUES {}",
            family.links_table, placeholders
        );
        let mut q = sqlx::query(&sql);
        for (memory_id, target_memory_id, link_type, strength) in links {
            q = q
                .bind(uuid7_id())
                .bind(memory_id)
                .bind(target_memory_id)
                .bind(link_type)
                .bind(strength.clamp(0.0, 1.0) as f32)
                .bind(now);
        }
        q.execute(&mut **tx).await.map_err(db_err)?;
        Ok(())
    }

    async fn delete_tags_tx(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
        family: &MemoryV2TableFamily,
        memory_id: &str,
        tags: &[String],
    ) -> Result<(), MemoriaError> {
        if tags.is_empty() {
            return Ok(());
        }
        let placeholders = tags.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
        let sql = format!(
            "DELETE FROM {} WHERE memory_id = ? AND tag IN ({})",
            family.tags_table, placeholders
        );
        let mut q = sqlx::query(&sql).bind(memory_id);
        for tag in tags {
            q = q.bind(tag);
        }
        q.execute(&mut **tx).await.map_err(db_err)?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn enqueue_jobs_tx(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
        family: &MemoryV2TableFamily,
        memory_id: &str,
        content_version_id: &str,
        event_id: &str,
        job_types: &[&str],
        now: NaiveDateTime,
    ) -> Result<(), MemoriaError> {
        for job_type in job_types {
            let job_payload = serde_json::json!({
                "memory_id": memory_id,
                "content_version_id": content_version_id,
                "event_id": event_id,
            });
            let dedupe_key = format!("{job_type}:{memory_id}:{event_id}");
            sqlx::query(&format!(
                "INSERT INTO {} \
                 (job_id, job_type, aggregate_id, payload_json, dedupe_key, status, available_at, leased_until, attempts, last_error, created_at, updated_at) \
                 VALUES (?, ?, ?, ?, ?, 'pending', ?, NULL, 0, NULL, ?, ?) \
                 ON DUPLICATE KEY UPDATE updated_at = VALUES(updated_at)",
                family.jobs_table
            ))
            .bind(uuid7_id())
            .bind(job_type)
            .bind(memory_id)
            .bind(job_payload.to_string())
            .bind(dedupe_key)
            .bind(now)
            .bind(now)
            .bind(now)
            .execute(&mut **tx)
            .await
            .map_err(db_err)?;
        }
        Ok(())
    }

    pub async fn ensure_user_tables(
        &self,
        user_id: &str,
    ) -> Result<MemoryV2TableFamily, MemoriaError> {
        let cached = self
            .family_cache
            .read()
            .map_err(|_| MemoriaError::Internal("family cache poisoned".into()))?
            .contains(user_id);
        if cached {
            let family = MemoryV2TableFamily::for_user(user_id);
            family.validate()?;
            return Ok(family);
        }

        // Coordinate first-time initialization per user to avoid DDL stampede.
        let _init_guard = loop {
            let should_initialize = {
                let mut inflight = self
                    .family_init_inflight
                    .lock()
                    .map_err(|_| MemoriaError::Internal("family init lock poisoned".into()))?;
                if inflight.contains(user_id) {
                    false
                } else {
                    inflight.insert(user_id.to_string());
                    true
                }
            };
            if should_initialize {
                break FamilyInitGuard {
                    inflight: Arc::clone(&self.family_init_inflight),
                    user_id: user_id.to_string(),
                };
            }
            sleep(Duration::from_millis(20)).await;
            let cached = self
                .family_cache
                .read()
                .map_err(|_| MemoriaError::Internal("family cache poisoned".into()))?
                .contains(user_id);
            if cached {
                let family = MemoryV2TableFamily::for_user(user_id);
                family.validate()?;
                return Ok(family);
            }
        };

        let result = async {
            self.ensure_registry_table().await?;
            let family = MemoryV2TableFamily::for_user(user_id);
            family.validate()?;

            self.create_events_table(&family).await?;
            self.create_heads_table(&family).await?;
            self.create_content_versions_table(&family).await?;
            self.create_index_docs_table(&family).await?;
            self.create_links_table(&family).await?;
            self.create_entities_table(&family).await?;
            self.create_memory_entities_table(&family).await?;
            self.create_focus_table(&family).await?;
            self.create_jobs_table(&family).await?;
            self.create_tags_table(&family).await?;
            self.create_stats_table(&family).await?;
            self.create_feedback_table(&family).await?;
            self.register_family(user_id, &family).await?;
            self.family_cache
                .write()
                .map_err(|_| MemoriaError::Internal("family cache poisoned".into()))?
                .insert(user_id.to_string());
            Ok(family)
        }
        .await;
        result
    }

    fn validate_remember_input(&self, input: &MemoryV2RememberInput) -> Result<(), MemoriaError> {
        if input.content.trim().is_empty() {
            return Err(MemoriaError::Validation("content must not be empty".into()));
        }
        if input.content.len() > 32_768 {
            return Err(MemoriaError::Validation(
                "content exceeds 32 KiB limit".into(),
            ));
        }
        if let Some(ref embedding) = input.embedding {
            self.validate_embedding_dim(embedding, "memory")?;
        }
        Ok(())
    }

    async fn remember_tx(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
        family: &MemoryV2TableFamily,
        input: MemoryV2RememberInput,
        now: NaiveDateTime,
    ) -> Result<MemoryV2RememberResult, MemoriaError> {
        let memory_id = uuid_id();
        let event_id = uuid7_id();
        let content_version_id = uuid_id();
        let index_doc_id = uuid_id();
        let abstract_text = Self::preview_abstract(&input.content);
        let abstract_tokens = estimate_tokens(&abstract_text);
        let importance = input.importance.unwrap_or(0.0).clamp(0.0, 1.0);
        let trust_tier = input.trust_tier.unwrap_or(TrustTier::T2Curated);
        let confidence = trust_tier.initial_confidence();
        let source_json = input
            .source
            .clone()
            .unwrap_or_else(|| serde_json::json!({}));
        let source_json_str = serde_json::to_string(&source_json)?;
        let source_kind = source_field(&input.source, "kind");
        let source_app = source_field(&input.source, "app");
        let source_message_id = source_field(&input.source, "message_id");
        let source_turn_id = source_field(&input.source, "turn_id");
        let embedding = input.embedding.as_deref().map(vec_to_mo);

        let tags = normalize_tags(input.tags.clone());
        let event_payload = serde_json::json!({
            "memory_id": memory_id,
            "content_version_id": content_version_id,
            "index_doc_id": index_doc_id,
            "type": input.memory_type.to_string(),
            "session_id": input.session_id,
            "importance": importance,
            "trust_tier": trust_tier.to_string(),
            "tags": tags.clone(),
            "source": source_json,
        });

        sqlx::query(&format!(
            "INSERT INTO {} \
             (event_id, aggregate_type, aggregate_id, event_type, event_version, causation_id, correlation_id, actor, payload_json, processing_state, created_at) \
             VALUES (?, 'memory', ?, 'remembered', 1, NULL, NULL, ?, ?, 'committed', ?)",
            family.events_table
        ))
        .bind(&event_id)
        .bind(&memory_id)
        .bind(&input.actor)
        .bind(event_payload.to_string())
        .bind(now)
        .execute(&mut **tx)
        .await
        .map_err(db_err)?;

        // Short content: abstract == source, so overview/detail views add nothing.
        // Mark derivation complete immediately to skip the derive_views job.
        let is_short = input.content.trim().len() <= ABSTRACT_BYTES;
        let initial_derivation_state = if is_short { "complete" } else { "pending" };

        sqlx::query(&format!(
            "INSERT INTO {} \
             (content_version_id, memory_id, source_text, abstract_text, overview_text, detail_text, has_overview, has_detail, \
              abstract_token_estimate, overview_token_estimate, detail_token_estimate, derivation_state, created_at) \
             VALUES (?, ?, ?, ?, NULL, NULL, 0, 0, ?, 0, 0, ?, ?)",
            family.content_versions_table
        ))
        .bind(&content_version_id)
        .bind(&memory_id)
        .bind(&input.content)
        .bind(&abstract_text)
        .bind(abstract_tokens)
        .bind(initial_derivation_state)
        .bind(now)
        .execute(&mut **tx)
        .await
        .map_err(db_err)?;

        sqlx::query(&format!(
            "INSERT INTO {} \
             (index_doc_id, memory_id, content_version_id, recall_text, embedding, memory_type, session_id, confidence, created_at, published_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            family.index_docs_table
        ))
        .bind(&index_doc_id)
        .bind(&memory_id)
        .bind(&content_version_id)
        .bind(&abstract_text)
        .bind(embedding)
        .bind(input.memory_type.to_string())
        .bind(input.session_id.clone())
        .bind(confidence as f32)
        .bind(now)
        .bind(now)
        .execute(&mut **tx)
        .await
        .map_err(db_err)?;

        sqlx::query(&format!(
            "INSERT INTO {} \
             (memory_id, memory_type, session_id, trust_tier, confidence, importance, \
              source_kind, source_app, source_message_id, source_turn_id, source_json, \
              is_active, forgotten_at, current_content_version_id, current_index_doc_id, latest_event_id, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 1, NULL, ?, ?, ?, ?, ?)",
            family.heads_table
        ))
        .bind(&memory_id)
        .bind(input.memory_type.to_string())
        .bind(input.session_id.clone())
        .bind(trust_tier.to_string())
        .bind(confidence as f32)
        .bind(importance as f32)
        .bind(source_kind)
        .bind(source_app)
        .bind(source_message_id)
        .bind(source_turn_id)
        .bind(source_json_str)
        .bind(&content_version_id)
        .bind(&index_doc_id)
        .bind(&event_id)
        .bind(now)
        .bind(now)
        .execute(&mut **tx)
        .await
        .map_err(db_err)?;

        self.insert_tags_tx(tx, family, &memory_id, &tags, now)
            .await?;

        sqlx::query(&format!(
            "INSERT INTO {} (memory_id, access_count, last_accessed_at) VALUES (?, 0, NULL)",
            family.stats_table
        ))
        .bind(&memory_id)
        .execute(&mut **tx)
        .await
        .map_err(db_err)?;

        self.enqueue_jobs_tx(
            tx,
            family,
            &memory_id,
            &content_version_id,
            &event_id,
            if is_short {
                &["extract_links", "extract_entities"]
            } else {
                &["derive_views", "extract_links", "extract_entities"]
            },
            now,
        )
        .await?;

        Ok(MemoryV2RememberResult {
            memory_id,
            abstract_text,
            has_overview: false,
            has_detail: false,
        })
    }

    pub async fn remember(
        &self,
        user_id: &str,
        input: MemoryV2RememberInput,
    ) -> Result<MemoryV2RememberResult, MemoriaError> {
        self.remember_with_options(user_id, input, RememberV2Options::default())
            .await
    }

    pub async fn remember_with_options(
        &self,
        user_id: &str,
        input: MemoryV2RememberInput,
        options: RememberV2Options,
    ) -> Result<MemoryV2RememberResult, MemoriaError> {
        self.validate_remember_input(&input)?;
        let family = self.ensure_user_tables(user_id).await?;
        let now = Utc::now().naive_utc();
        let mut tx = self.pool.begin().await.map_err(db_err)?;
        let remembered = self.remember_tx(&mut tx, &family, input, now).await?;
        tx.commit().await.map_err(db_err)?;
        if options.sync_enrich {
            let timeout_secs = options
                .enrich_timeout_secs
                .unwrap_or(DEFAULT_SYNC_ENRICH_TIMEOUT_SECS)
                .clamp(1, 300) as u64;
            self.wait_for_memory_jobs_complete(user_id, &remembered.memory_id, timeout_secs)
                .await?;
        }
        Ok(remembered)
    }

    pub async fn remember_batch(
        &self,
        user_id: &str,
        inputs: Vec<MemoryV2RememberInput>,
    ) -> Result<Vec<MemoryV2RememberResult>, MemoriaError> {
        if inputs.is_empty() {
            return Ok(vec![]);
        }
        for input in &inputs {
            self.validate_remember_input(input)?;
        }
        let family = self.ensure_user_tables(user_id).await?;
        let now = Utc::now().naive_utc();
        let mut tx = self.pool.begin().await.map_err(db_err)?;
        let mut remembered = Vec::with_capacity(inputs.len());
        for input in inputs {
            remembered.push(self.remember_tx(&mut tx, &family, input, now).await?);
        }
        tx.commit().await.map_err(db_err)?;
        Ok(remembered)
    }

    async fn forget_tx(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
        family: &MemoryV2TableFamily,
        memory_id: &str,
        reason: Option<&str>,
        actor: &str,
        now: NaiveDateTime,
    ) -> Result<(), MemoriaError> {
        if memory_id.trim().is_empty() {
            return Err(MemoriaError::Validation(
                "memory_id must not be empty".into(),
            ));
        }
        let event_id = uuid7_id();
        let payload = serde_json::json!({
            "memory_id": memory_id,
            "reason": reason,
        });
        sqlx::query(&format!(
            "INSERT INTO {} \
             (event_id, aggregate_type, aggregate_id, event_type, event_version, causation_id, correlation_id, actor, payload_json, processing_state, created_at) \
             VALUES (?, 'memory', ?, 'forgotten', 1, NULL, NULL, ?, ?, 'committed', ?)",
            family.events_table
        ))
        .bind(&event_id)
        .bind(memory_id)
        .bind(actor)
        .bind(payload.to_string())
        .bind(now)
        .execute(&mut **tx)
        .await
        .map_err(db_err)?;

        let result = sqlx::query(&format!(
            "UPDATE {} SET is_active = 0, forgotten_at = ?, latest_event_id = ?, updated_at = ? \
             WHERE memory_id = ? AND forgotten_at IS NULL",
            family.heads_table
        ))
        .bind(now)
        .bind(&event_id)
        .bind(now)
        .bind(memory_id)
        .execute(&mut **tx)
        .await
        .map_err(db_err)?;
        if result.rows_affected() == 0 {
            return Err(MemoriaError::NotFound(memory_id.to_string()));
        }
        Ok(())
    }

    pub async fn update(
        &self,
        user_id: &str,
        input: MemoryV2UpdateInput,
    ) -> Result<MemoryV2UpdateResult, MemoriaError> {
        let tags_add = normalize_tags(input.tags_add);
        let tags_remove = normalize_tags(input.tags_remove);
        let has_requested_change = input.content.is_some()
            || input.importance.is_some()
            || input.trust_tier.is_some()
            || !tags_add.is_empty()
            || !tags_remove.is_empty();
        if !has_requested_change {
            return Err(MemoriaError::Validation(
                "at least one field must be updated".into(),
            ));
        }
        if tags_add.iter().any(|tag| tags_remove.contains(tag)) {
            return Err(MemoriaError::Validation(
                "tags_add and tags_remove must not overlap".into(),
            ));
        }
        if let Some(ref content) = input.content {
            if content.trim().is_empty() {
                return Err(MemoriaError::Validation("content must not be empty".into()));
            }
            if content.len() > 32_768 {
                return Err(MemoriaError::Validation(
                    "content exceeds 32 KiB limit".into(),
                ));
            }
        }
        if input.embedding.is_some() && input.content.is_none() {
            return Err(MemoriaError::Validation(
                "embedding can only be updated together with content".into(),
            ));
        }
        if let Some(ref embedding) = input.embedding {
            self.validate_embedding_dim(embedding, "memory")?;
        }

        let family = self.ensure_user_tables(user_id).await?;
        let current = sqlx::query(&format!(
            "SELECT h.memory_id, h.memory_type, h.session_id, h.trust_tier, h.importance, \
                    h.current_content_version_id, h.current_index_doc_id, \
                    c.source_text, c.abstract_text, c.has_overview, c.has_detail \
             FROM {} h \
             JOIN {} c ON c.content_version_id = h.current_content_version_id \
             WHERE h.memory_id = ? AND h.forgotten_at IS NULL \
             LIMIT 1",
            family.heads_table, family.content_versions_table
        ))
        .bind(&input.memory_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?
        .ok_or_else(|| MemoriaError::NotFound(input.memory_id.clone()))?;

        let current_memory_type = MemoryType::from_str(
            &current
                .try_get::<String, _>("memory_type")
                .map_err(db_err)?,
        )
        .map_err(|e| MemoriaError::Validation(e.to_string()))?;
        let current_session_id: Option<String> = current.try_get("session_id").ok();
        let current_trust_tier =
            TrustTier::from_str(&current.try_get::<String, _>("trust_tier").map_err(db_err)?)?;
        let current_importance = current.try_get::<f32, _>("importance").unwrap_or(0.0) as f64;
        let current_content_version_id: String = current
            .try_get("current_content_version_id")
            .map_err(db_err)?;
        let current_source_text: String = current.try_get("source_text").map_err(db_err)?;
        let current_abstract_text: String = current.try_get("abstract_text").map_err(db_err)?;
        let current_has_overview = current.try_get::<i8, _>("has_overview").unwrap_or(0) != 0;
        let current_has_detail = current.try_get::<i8, _>("has_detail").unwrap_or(0) != 0;

        let content_changed = input
            .content
            .as_ref()
            .map(|content| content != &current_source_text)
            .unwrap_or(false);
        let next_importance = input
            .importance
            .unwrap_or(current_importance)
            .clamp(0.0, 1.0);
        let next_trust_tier = input.trust_tier.unwrap_or(current_trust_tier.clone());
        let metadata_changed = (next_importance - current_importance).abs() > f64::EPSILON
            || next_trust_tier != current_trust_tier;
        if !content_changed && !metadata_changed && tags_add.is_empty() && tags_remove.is_empty() {
            return Err(MemoriaError::Validation(
                "no effective changes requested".into(),
            ));
        }

        let now = Utc::now().naive_utc();
        let event_id = uuid7_id();
        let next_confidence = next_trust_tier.initial_confidence();
        let next_content = input.content.unwrap_or(current_source_text.clone());
        let mut abstract_text = current_abstract_text.clone();
        let mut has_overview = current_has_overview;
        let mut has_detail = current_has_detail;
        let mut content_version_id = current_content_version_id.clone();

        let event_payload = serde_json::json!({
            "memory_id": input.memory_id,
            "reason": input.reason,
            "content_updated": content_changed,
            "importance": next_importance,
            "trust_tier": next_trust_tier.to_string(),
            "tags_add": tags_add.clone(),
            "tags_remove": tags_remove.clone(),
        });

        let mut tx = self.pool.begin().await.map_err(db_err)?;
        sqlx::query(&format!(
            "INSERT INTO {} \
             (event_id, aggregate_type, aggregate_id, event_type, event_version, causation_id, correlation_id, actor, payload_json, processing_state, created_at) \
             VALUES (?, 'memory', ?, 'updated', 1, NULL, NULL, ?, ?, 'committed', ?)",
            family.events_table
        ))
        .bind(&event_id)
        .bind(&input.memory_id)
        .bind(&input.actor)
        .bind(event_payload.to_string())
        .bind(now)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;

        if content_changed {
            let new_content_version_id = uuid_id();
            let new_index_doc_id = uuid_id();
            let new_abstract_text = Self::preview_abstract(&next_content);
            let new_abstract_tokens = estimate_tokens(&new_abstract_text);
            let embedding = input.embedding.as_deref().map(vec_to_mo);
            let new_is_short = next_content.trim().len() <= ABSTRACT_BYTES;
            let new_derivation_state = if new_is_short { "complete" } else { "pending" };

            sqlx::query(&format!(
                "INSERT INTO {} \
                 (content_version_id, memory_id, source_text, abstract_text, overview_text, detail_text, has_overview, has_detail, \
                  abstract_token_estimate, overview_token_estimate, detail_token_estimate, derivation_state, created_at) \
                 VALUES (?, ?, ?, ?, NULL, NULL, 0, 0, ?, 0, 0, ?, ?)",
                family.content_versions_table
            ))
            .bind(&new_content_version_id)
            .bind(&input.memory_id)
            .bind(&next_content)
            .bind(&new_abstract_text)
            .bind(new_abstract_tokens)
            .bind(new_derivation_state)
            .bind(now)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;

            sqlx::query(&format!(
                "INSERT INTO {} \
                 (index_doc_id, memory_id, content_version_id, recall_text, embedding, memory_type, session_id, confidence, created_at, published_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                family.index_docs_table
            ))
            .bind(&new_index_doc_id)
            .bind(&input.memory_id)
            .bind(&new_content_version_id)
            .bind(&new_abstract_text)
            .bind(embedding)
            .bind(current_memory_type.to_string())
            .bind(current_session_id.clone())
            .bind(next_confidence as f32)
            .bind(now)
            .bind(now)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;

            sqlx::query(&format!(
                "UPDATE {} SET trust_tier = ?, confidence = ?, importance = ?, \
                     current_content_version_id = ?, current_index_doc_id = ?, latest_event_id = ?, updated_at = ? \
                 WHERE memory_id = ? AND forgotten_at IS NULL",
                family.heads_table
            ))
            .bind(next_trust_tier.to_string())
            .bind(next_confidence as f32)
            .bind(next_importance as f32)
            .bind(&new_content_version_id)
            .bind(&new_index_doc_id)
            .bind(&event_id)
            .bind(now)
            .bind(&input.memory_id)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;

            self.enqueue_jobs_tx(
                &mut tx,
                &family,
                &input.memory_id,
                &new_content_version_id,
                &event_id,
                if new_is_short {
                    &["extract_links", "extract_entities"]
                } else {
                    &["derive_views", "extract_links", "extract_entities"]
                },
                now,
            )
            .await?;

            abstract_text = new_abstract_text;
            has_overview = false;
            has_detail = false;
            content_version_id = new_content_version_id;
        } else {
            sqlx::query(&format!(
                "UPDATE {} SET trust_tier = ?, confidence = ?, importance = ?, latest_event_id = ?, updated_at = ? \
                 WHERE memory_id = ? AND forgotten_at IS NULL",
                family.heads_table
            ))
            .bind(next_trust_tier.to_string())
            .bind(next_confidence as f32)
            .bind(next_importance as f32)
            .bind(&event_id)
            .bind(now)
            .bind(&input.memory_id)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        }

        self.delete_tags_tx(&mut tx, &family, &input.memory_id, &tags_remove)
            .await?;
        self.insert_tags_tx(&mut tx, &family, &input.memory_id, &tags_add, now)
            .await?;

        if !content_changed && (!tags_add.is_empty() || !tags_remove.is_empty()) {
            self.enqueue_jobs_tx(
                &mut tx,
                &family,
                &input.memory_id,
                &content_version_id,
                &event_id,
                &["extract_links"],
                now,
            )
            .await?;
        }

        tx.commit().await.map_err(db_err)?;

        Ok(MemoryV2UpdateResult {
            memory_id: input.memory_id,
            abstract_text,
            updated_at: to_utc(now),
            has_overview,
            has_detail,
        })
    }

    pub async fn list_tags(
        &self,
        user_id: &str,
        limit: i64,
        query: Option<&str>,
    ) -> Result<Vec<TagV2Summary>, MemoriaError> {
        let family = self.ensure_user_tables(user_id).await?;
        let limit = limit.clamp(1, 200);
        let active_rows = sqlx::query(&format!(
            "SELECT memory_id FROM {} WHERE forgotten_at IS NULL",
            family.heads_table
        ))
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;
        let active_ids = active_rows
            .into_iter()
            .map(|row| row.try_get::<String, _>("memory_id").map_err(db_err))
            .collect::<Result<HashSet<_>, _>>()?;
        if active_ids.is_empty() {
            return Ok(Vec::new());
        }

        let mut sql = format!("SELECT tag, memory_id FROM {}", family.tags_table);
        let mut like_pattern = None;
        if let Some(query) = query.map(str::trim).filter(|query| !query.is_empty()) {
            sql.push_str(" WHERE tag LIKE ?");
            like_pattern = Some(format!("%{}%", sanitize_like_pattern(query)));
        }
        let mut q = sqlx::query(&sql);
        if let Some(pattern) = like_pattern {
            q = q.bind(pattern);
        }
        let rows = q.fetch_all(&self.pool).await.map_err(db_err)?;
        let mut counts: HashMap<String, HashSet<String>> = HashMap::new();
        for row in rows {
            let memory_id: String = row.try_get("memory_id").map_err(db_err)?;
            if !active_ids.contains(&memory_id) {
                continue;
            }
            let tag: String = row.try_get("tag").map_err(db_err)?;
            counts.entry(tag).or_default().insert(memory_id);
        }
        let mut items = counts
            .into_iter()
            .map(|(tag, memories)| TagV2Summary {
                tag,
                memory_count: memories.len() as i64,
            })
            .collect::<Vec<_>>();
        items.sort_by(|a, b| {
            b.memory_count
                .cmp(&a.memory_count)
                .then_with(|| a.tag.cmp(&b.tag))
        });
        items.truncate(limit as usize);
        Ok(items)
    }

    pub async fn list(
        &self,
        user_id: &str,
        filter: ListV2Filter,
    ) -> Result<ListV2Result, MemoriaError> {
        let family = self.ensure_user_tables(user_id).await?;
        let limit = filter.limit.clamp(1, 200);
        let mut clauses = vec!["h.forgotten_at IS NULL".to_string()];
        if let Some(mt) = filter.memory_type {
            clauses.push(format!(
                "h.memory_type = '{}'",
                sanitize_sql_literal(&mt.to_string())
            ));
        }
        if let Some(ref sid) = filter.session_id {
            clauses.push(format!("h.session_id = '{}'", sanitize_sql_literal(sid)));
        }
        if let Some(cursor) = filter.cursor.as_ref() {
            clauses.push(format!(
                "(h.created_at < '{}' OR (h.created_at = '{}' AND h.memory_id < '{}'))",
                cursor.created_at.naive_utc(),
                cursor.created_at.naive_utc(),
                sanitize_sql_literal(&cursor.memory_id)
            ));
        }
        let sql = format!(
            "SELECT h.memory_id, h.memory_type, h.session_id, h.created_at, \
             c.abstract_text, c.has_overview, c.has_detail \
             FROM {} h \
             JOIN {} c ON c.content_version_id = h.current_content_version_id \
             WHERE {} \
             ORDER BY h.created_at DESC, h.memory_id DESC \
             LIMIT {}",
            family.heads_table,
            family.content_versions_table,
            clauses.join(" AND "),
            limit
        );
        let rows = sqlx::query(&sql)
            .fetch_all(&self.pool)
            .await
            .map_err(db_err)?;
        let items: Vec<ListV2Item> = rows
            .into_iter()
            .map(|r| -> Result<ListV2Item, MemoriaError> {
                Ok(ListV2Item {
                    memory_id: r.try_get("memory_id").map_err(db_err)?,
                    abstract_text: r.try_get("abstract_text").map_err(db_err)?,
                    memory_type: MemoryType::from_str(
                        &r.try_get::<String, _>("memory_type").map_err(db_err)?,
                    )
                    .map_err(|e| MemoriaError::Validation(e.to_string()))?,
                    session_id: r.try_get("session_id").ok(),
                    created_at: to_utc(r.try_get("created_at").map_err(db_err)?),
                    has_overview: r.try_get::<i8, _>("has_overview").unwrap_or(0) != 0,
                    has_detail: r.try_get::<i8, _>("has_detail").unwrap_or(0) != 0,
                })
            })
            .collect::<Result<_, _>>()?;
        let next_cursor = if items.len() == limit as usize {
            items.last().map(|last| ListV2Cursor {
                created_at: last.created_at,
                memory_id: last.memory_id.clone(),
            })
        } else {
            None
        };
        Ok(ListV2Result { items, next_cursor })
    }

    pub async fn profile(
        &self,
        user_id: &str,
        filter: ProfileV2Filter,
    ) -> Result<ProfileV2Result, MemoriaError> {
        let family = self.ensure_user_tables(user_id).await?;
        let limit = filter.limit.clamp(1, 200);
        let mut clauses = vec![
            "h.forgotten_at IS NULL".to_string(),
            format!(
                "h.memory_type = '{}'",
                sanitize_sql_literal(&MemoryType::Profile.to_string())
            ),
        ];
        if let Some(ref sid) = filter.session_id {
            clauses.push(format!("h.session_id = '{}'", sanitize_sql_literal(sid)));
        }
        if let Some(cursor) = filter.cursor.as_ref() {
            clauses.push(format!(
                "(h.created_at < '{}' OR (h.created_at = '{}' AND h.memory_id < '{}'))",
                cursor.created_at.naive_utc(),
                cursor.created_at.naive_utc(),
                sanitize_sql_literal(&cursor.memory_id)
            ));
        }
        let sql = format!(
            "SELECT h.memory_id, h.session_id, h.created_at, h.updated_at, h.trust_tier, \
             h.confidence, h.importance, c.source_text, c.abstract_text, c.has_overview, c.has_detail \
             FROM {} h \
             JOIN {} c ON c.content_version_id = h.current_content_version_id \
             WHERE {} \
             ORDER BY h.created_at DESC, h.memory_id DESC \
             LIMIT {}",
            family.heads_table,
            family.content_versions_table,
            clauses.join(" AND "),
            limit
        );
        let rows = sqlx::query(&sql)
            .fetch_all(&self.pool)
            .await
            .map_err(db_err)?;
        let items: Vec<ProfileV2Item> = rows
            .into_iter()
            .map(|row| -> Result<ProfileV2Item, MemoriaError> {
                Ok(ProfileV2Item {
                    memory_id: row.try_get("memory_id").map_err(db_err)?,
                    content: row.try_get("source_text").map_err(db_err)?,
                    abstract_text: row.try_get("abstract_text").map_err(db_err)?,
                    session_id: row.try_get("session_id").ok(),
                    created_at: to_utc(row.try_get("created_at").map_err(db_err)?),
                    updated_at: to_utc(row.try_get("updated_at").map_err(db_err)?),
                    trust_tier: TrustTier::from_str(
                        &row.try_get::<String, _>("trust_tier").map_err(db_err)?,
                    )
                    .map_err(|e| MemoriaError::Validation(e.to_string()))?,
                    confidence: row.try_get::<f32, _>("confidence").map_err(db_err)? as f64,
                    importance: row.try_get::<f32, _>("importance").map_err(db_err)? as f64,
                    has_overview: row.try_get::<i8, _>("has_overview").unwrap_or(0) != 0,
                    has_detail: row.try_get::<i8, _>("has_detail").unwrap_or(0) != 0,
                })
            })
            .collect::<Result<_, _>>()?;
        let next_cursor = if items.len() == limit as usize {
            items.last().map(|last| ListV2Cursor {
                created_at: last.created_at,
                memory_id: last.memory_id.clone(),
            })
        } else {
            None
        };
        Ok(ProfileV2Result { items, next_cursor })
    }

    pub async fn extract_entities(
        &self,
        user_id: &str,
        limit: i64,
        memory_id: Option<&str>,
    ) -> Result<EntityV2ExtractResult, MemoriaError> {
        let family = self.ensure_user_tables(user_id).await?;
        let limit = limit.clamp(1, 200);
        let mut sql = format!(
            "SELECT h.memory_id, h.current_content_version_id, c.source_text \
             FROM {} h \
             JOIN {} c ON c.content_version_id = h.current_content_version_id \
             WHERE h.forgotten_at IS NULL",
            family.heads_table, family.content_versions_table
        );
        if memory_id.is_some() {
            sql.push_str(" AND h.memory_id = ?");
        }
        sql.push_str(&format!(
            " ORDER BY h.updated_at DESC, h.memory_id DESC LIMIT {}",
            limit
        ));
        let mut query = sqlx::query(&sql);
        if let Some(memory_id) = memory_id {
            query = query.bind(memory_id);
        }
        let rows = query.fetch_all(&self.pool).await.map_err(db_err)?;
        let mut processed_memories = 0i64;
        let mut entities_found = 0i64;
        let mut links_written = 0i64;
        let now = Utc::now().naive_utc();
        let mut tx = self.pool.begin().await.map_err(db_err)?;

        for row in rows {
            let memory_id: String = row.try_get("memory_id").map_err(db_err)?;
            let content_version_id: String =
                row.try_get("current_content_version_id").map_err(db_err)?;
            let source_text: String = row.try_get("source_text").map_err(db_err)?;
            processed_memories += 1;
            let (memory_entities_found, memory_links_written) = self
                .refresh_entities_for_memory_tx(
                    &mut tx,
                    &family,
                    &memory_id,
                    &content_version_id,
                    &source_text,
                    now,
                )
                .await?;
            entities_found += memory_entities_found;
            links_written += memory_links_written;
        }

        tx.commit().await.map_err(db_err)?;
        Ok(EntityV2ExtractResult {
            processed_memories,
            entities_found,
            links_written,
        })
    }

    pub async fn list_entities(
        &self,
        user_id: &str,
        filter: EntityV2Filter,
    ) -> Result<EntityV2ListResult, MemoriaError> {
        let family = self.ensure_user_tables(user_id).await?;
        let limit = filter.limit.clamp(1, 200);
        let mut sql = format!(
            "SELECT e.entity_id, e.name, e.display_name, e.entity_type, e.created_at, e.updated_at, \
                COUNT(DISTINCT me.memory_id) AS memory_count \
             FROM {} e \
             JOIN {} me ON me.entity_id = e.entity_id \
             JOIN {} h ON h.memory_id = me.memory_id \
                AND h.forgotten_at IS NULL \
                AND h.current_content_version_id = me.content_version_id",
            family.entities_table, family.memory_entities_table, family.heads_table
        );
        let mut clauses = Vec::new();
        let mut like_pattern = None;
        if let Some(query) = filter
            .query
            .as_deref()
            .map(str::trim)
            .filter(|query| !query.is_empty())
        {
            like_pattern = Some(format!("%{}%", sanitize_like_pattern(query)));
            clauses.push("(e.name LIKE ? OR e.display_name LIKE ?)");
        }
        if filter.entity_type.as_deref().is_some() {
            clauses.push("e.entity_type = ?");
        }
        if filter.memory_id.as_deref().is_some() {
            clauses.push("h.memory_id = ?");
        }
        if filter.cursor.as_ref().is_some() {
            clauses.push("(e.updated_at < ? OR (e.updated_at = ? AND e.entity_id < ?))");
        }
        if !clauses.is_empty() {
            sql.push_str(" WHERE ");
            sql.push_str(&clauses.join(" AND "));
        }
        sql.push_str(
            " GROUP BY e.entity_id, e.name, e.display_name, e.entity_type, e.created_at, e.updated_at",
        );
        sql.push_str(&format!(
            " ORDER BY e.updated_at DESC, e.entity_id DESC LIMIT {}",
            limit
        ));

        let mut query = sqlx::query(&sql);
        if let Some(ref like_pattern) = like_pattern {
            query = query.bind(like_pattern).bind(like_pattern);
        }
        if let Some(entity_type) = filter.entity_type.as_deref() {
            query = query.bind(entity_type.trim());
        }
        if let Some(memory_id) = filter.memory_id.as_deref() {
            query = query.bind(memory_id.trim());
        }
        if let Some(cursor) = filter.cursor.as_ref() {
            query = query
                .bind(cursor.updated_at.naive_utc())
                .bind(cursor.updated_at.naive_utc())
                .bind(&cursor.entity_id);
        }
        let rows = query.fetch_all(&self.pool).await.map_err(db_err)?;
        let items: Vec<EntityV2Item> = rows
            .into_iter()
            .map(|row| -> Result<EntityV2Item, MemoriaError> {
                Ok(EntityV2Item {
                    entity_id: row.try_get("entity_id").map_err(db_err)?,
                    name: row.try_get("name").map_err(db_err)?,
                    display_name: row.try_get("display_name").map_err(db_err)?,
                    entity_type: row.try_get("entity_type").map_err(db_err)?,
                    memory_count: row.try_get("memory_count").map_err(db_err)?,
                    created_at: to_utc(row.try_get("created_at").map_err(db_err)?),
                    updated_at: to_utc(row.try_get("updated_at").map_err(db_err)?),
                })
            })
            .collect::<Result<_, _>>()?;
        let next_cursor = if items.len() == limit as usize {
            items.last().map(|last| EntityV2Cursor {
                updated_at: last.updated_at,
                entity_id: last.entity_id.clone(),
            })
        } else {
            None
        };
        Ok(EntityV2ListResult { items, next_cursor })
    }

    pub async fn reflect(
        &self,
        user_id: &str,
        filter: ReflectV2Filter,
    ) -> Result<ReflectV2Result, MemoriaError> {
        let family = self.ensure_user_tables(user_id).await?;
        let response_mode = match filter.mode.as_str() {
            "internal" => "internal",
            "candidates" => "candidates",
            _ => "auto",
        }
        .to_string();
        let limit = filter.limit.clamp(1, 100);
        let min_cluster_size = filter.min_cluster_size.clamp(2, 20) as usize;
        let min_link_strength = filter.min_link_strength.clamp(0.0, 1.0) as f32;
        let memories = self
            .load_reflect_memories(&family, filter.session_id.as_deref())
            .await?;
        if memories.len() < min_cluster_size {
            return Ok(ReflectV2Result {
                mode: response_mode,
                synthesized: filter.mode == "internal",
                scenes_created: 0,
                candidates: Vec::new(),
            });
        }
        let (mut candidates, edges_by_pair) = self
            .build_reflect_linked_candidates(
                &family,
                &memories,
                min_cluster_size,
                min_link_strength,
            )
            .await?;
        if candidates.is_empty() {
            candidates =
                self.build_reflect_session_candidates(&memories, &edges_by_pair, min_cluster_size);
        }
        candidates.sort_by(|a, b| {
            b.importance
                .partial_cmp(&a.importance)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| b.memory_count.cmp(&a.memory_count))
                .then_with(|| b.link_count.cmp(&a.link_count))
                .then_with(|| a.signal.cmp(&b.signal))
        });
        candidates.truncate(limit as usize);
        let scenes_created = if filter.mode == "internal" {
            self.reflect_internal_writeback(&family, &candidates)
                .await?
        } else {
            0
        };
        Ok(ReflectV2Result {
            mode: response_mode,
            synthesized: filter.mode == "internal",
            scenes_created,
            candidates,
        })
    }

    pub async fn forget(
        &self,
        user_id: &str,
        memory_id: &str,
        reason: Option<&str>,
        actor: &str,
    ) -> Result<(), MemoriaError> {
        let family = self.ensure_user_tables(user_id).await?;
        let now = Utc::now().naive_utc();
        let mut tx = self.pool.begin().await.map_err(db_err)?;
        self.forget_tx(&mut tx, &family, memory_id, reason, actor, now)
            .await?;
        tx.commit().await.map_err(db_err)?;
        Ok(())
    }

    pub async fn forget_batch(
        &self,
        user_id: &str,
        memory_ids: &[String],
        reason: Option<&str>,
        actor: &str,
    ) -> Result<Vec<String>, MemoriaError> {
        if memory_ids.is_empty() {
            return Ok(vec![]);
        }
        let family = self.ensure_user_tables(user_id).await?;
        let now = Utc::now().naive_utc();
        let mut tx = self.pool.begin().await.map_err(db_err)?;
        let mut forgotten = Vec::with_capacity(memory_ids.len());
        for memory_id in memory_ids {
            self.forget_tx(&mut tx, &family, memory_id, reason, actor, now)
                .await?;
            forgotten.push(memory_id.clone());
        }
        tx.commit().await.map_err(db_err)?;
        Ok(forgotten)
    }

    pub async fn focus(
        &self,
        user_id: &str,
        input: FocusV2Input,
    ) -> Result<FocusV2Result, MemoriaError> {
        if input.value.trim().is_empty() {
            return Err(MemoriaError::Validation(
                "focus value must not be empty".into(),
            ));
        }
        let family = self.ensure_user_tables(user_id).await?;
        let now = Utc::now();
        let ttl_secs = input
            .ttl_secs
            .unwrap_or(DEFAULT_FOCUS_TTL_SECS)
            .clamp(1, MAX_FOCUS_TTL_SECS);
        let boost = input.boost.unwrap_or(DEFAULT_FOCUS_BOOST).clamp(1.0, 5.0);
        let expires_at = now + chrono::Duration::seconds(ttl_secs);
        let focus_id = sqlx::query(&format!(
            "SELECT focus_id FROM {} WHERE focus_type = ? AND focus_value = ? LIMIT 1",
            family.focus_table
        ))
        .bind(&input.focus_type)
        .bind(&input.value)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?
        .and_then(|row| row.try_get("focus_id").ok())
        .unwrap_or_else(uuid7_id);
        let event_id = uuid7_id();
        let event_payload = serde_json::json!({
            "focus_type": input.focus_type,
            "focus_value": input.value,
            "boost": boost,
            "expires_at": expires_at,
        });
        let mut tx = self.pool.begin().await.map_err(db_err)?;
        sqlx::query(&format!(
            "INSERT INTO {} \
             (event_id, aggregate_type, aggregate_id, event_type, event_version, causation_id, correlation_id, actor, payload_json, processing_state, created_at) \
             VALUES (?, 'focus', ?, 'focus_set', 1, NULL, NULL, ?, ?, 'committed', ?)",
            family.events_table
        ))
        .bind(&event_id)
        .bind(&focus_id)
        .bind(&input.actor)
        .bind(event_payload.to_string())
        .bind(now.naive_utc())
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;

        sqlx::query(&format!(
            "INSERT INTO {} \
             (focus_id, focus_type, focus_value, boost, state, expires_at, created_at, updated_at) \
             VALUES (?, ?, ?, ?, 'active', ?, ?, ?) \
             ON DUPLICATE KEY UPDATE boost = VALUES(boost), state = 'active', expires_at = VALUES(expires_at), updated_at = VALUES(updated_at)",
            family.focus_table
        ))
        .bind(&focus_id)
        .bind(&input.focus_type)
        .bind(&input.value)
        .bind(boost as f32)
        .bind(expires_at.naive_utc())
        .bind(now.naive_utc())
        .bind(now.naive_utc())
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
        tx.commit().await.map_err(db_err)?;
        Ok(FocusV2Result {
            focus_id,
            focus_type: input.focus_type,
            value: input.value,
            boost,
            active_until: expires_at,
        })
    }

    pub async fn expand(
        &self,
        user_id: &str,
        memory_id: &str,
        level: ExpandLevel,
    ) -> Result<MemoryV2ExpandResult, MemoriaError> {
        let family = self.ensure_user_tables(user_id).await?;
        let row = sqlx::query(&format!(
            "SELECT h.memory_id, c.abstract_text, c.overview_text, c.detail_text \
             FROM {} h JOIN {} c ON c.content_version_id = h.current_content_version_id \
             WHERE h.memory_id = ? AND h.forgotten_at IS NULL",
            family.heads_table, family.content_versions_table
        ))
        .bind(memory_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?
        .ok_or_else(|| MemoriaError::NotFound(memory_id.to_string()))?;
        let abstract_text: String = row.try_get("abstract_text").map_err(db_err)?;
        let overview_text: Option<String> = row.try_get("overview_text").ok();
        let detail_text: Option<String> = row.try_get("detail_text").ok();
        let links = if level == ExpandLevel::Links {
            Some(
                self.fetch_link_refs(&family, &[memory_id.to_string()])
                    .await?
                    .remove(memory_id)
                    .unwrap_or_default(),
            )
        } else {
            None
        };
        Ok(MemoryV2ExpandResult {
            memory_id: memory_id.to_string(),
            level,
            abstract_text,
            overview_text,
            detail_text,
            links,
        })
    }

    pub async fn links(
        &self,
        user_id: &str,
        req: MemoryV2LinksRequest,
    ) -> Result<MemoryV2LinksResult, MemoriaError> {
        let family = self.ensure_user_tables(user_id).await?;
        self.ensure_active_memory(&family, &req.memory_id).await?;
        let limit = req.limit.clamp(1, 200);
        let summary = self
            .load_direct_link_summary(&family, &req.memory_id)
            .await?;
        let mut items = self
            .query_link_items(
                &family,
                &req.memory_id,
                req.direction,
                req.link_type.as_deref(),
                req.min_strength,
                limit,
            )
            .await?;
        self.populate_link_provenance(&family, &req.memory_id, &mut items)
            .await?;
        self.bump_access_counts(
            &family,
            &items
                .iter()
                .map(|item| item.memory_id.clone())
                .collect::<Vec<_>>(),
        )
        .await?;
        Ok(MemoryV2LinksResult {
            memory_id: req.memory_id,
            summary,
            items,
        })
    }

    pub async fn related(
        &self,
        user_id: &str,
        req: MemoryV2RelatedRequest,
    ) -> Result<MemoryV2RelatedResult, MemoriaError> {
        let family = self.ensure_user_tables(user_id).await?;
        self.ensure_active_memory(&family, &req.memory_id).await?;
        let limit = req.limit.clamp(1, 200);
        let max_hops = req.max_hops.clamp(1, 3);
        #[derive(Debug, Clone)]
        struct RelatedFrontierItem {
            memory_id: String,
            strength: f64,
            via_memory_ids: Vec<String>,
            lineage: Vec<MemoryV2RelatedLineageStep>,
        }

        fn reorder_supporting_paths(item: &mut MemoryV2RelatedItem) {
            let best_path = MemoryV2RelatedPath {
                hop_distance: item.hop_distance,
                strength: item.strength,
                via_memory_ids: item.via_memory_ids.clone(),
                lineage: item.lineage.clone(),
                path_rank: 0,
                selected: true,
                selection_reason: "best_path".to_string(),
            };
            fn selection_reason(
                path: &MemoryV2RelatedPath,
                best_path: &MemoryV2RelatedPath,
            ) -> &'static str {
                if same_related_path(path, best_path) {
                    "best_path"
                } else if path.hop_distance > best_path.hop_distance {
                    "higher_hop_distance"
                } else if path.hop_distance == best_path.hop_distance
                    && path.strength < best_path.strength
                {
                    "lower_strength"
                } else {
                    "tie_break"
                }
            }
            item.supporting_paths.sort_by(|a, b| {
                let a_is_best = same_related_path(a, &best_path);
                let b_is_best = same_related_path(b, &best_path);
                b_is_best
                    .cmp(&a_is_best)
                    .then_with(|| a.hop_distance.cmp(&b.hop_distance))
                    .then_with(|| {
                        b.strength
                            .partial_cmp(&a.strength)
                            .unwrap_or(std::cmp::Ordering::Equal)
                    })
                    .then_with(|| related_path_signature(a).cmp(&related_path_signature(b)))
            });
            item.supporting_paths.truncate(MAX_RELATED_PATHS_PER_ITEM);
            for (index, path) in item.supporting_paths.iter_mut().enumerate() {
                path.path_rank = index as i64 + 1;
                path.selected = same_related_path(path, &best_path);
                path.selection_reason = selection_reason(path, &best_path).to_string();
            }
            item.supporting_paths_truncated =
                item.supporting_path_count as usize > item.supporting_paths.len();
        }

        let mut aggregated: HashMap<String, MemoryV2RelatedItem> = HashMap::new();
        let traversal_limit = (limit * 4).clamp(1, 200);
        let mut frontier = vec![RelatedFrontierItem {
            memory_id: req.memory_id.clone(),
            strength: 1.0,
            via_memory_ids: Vec::new(),
            lineage: Vec::new(),
        }];
        let mut expanded = HashSet::from([req.memory_id.clone()]);

        for hop in 1..=max_hops {
            if frontier.is_empty() {
                break;
            }
            let mut next_frontier: HashMap<String, RelatedFrontierItem> = HashMap::new();
            for frontier_item in frontier {
                let current_memory_id = frontier_item.memory_id;
                let current_strength = frontier_item.strength;
                let mut link_items = self
                    .query_link_items(
                        &family,
                        &current_memory_id,
                        LinkDirection::Both,
                        None,
                        req.min_strength,
                        traversal_limit,
                    )
                    .await?;
                self.populate_link_provenance(&family, &current_memory_id, &mut link_items)
                    .await?;
                for item in link_items {
                    if item.memory_id == req.memory_id {
                        continue;
                    }
                    let mut via_memory_ids = frontier_item.via_memory_ids.clone();
                    if current_memory_id != req.memory_id {
                        via_memory_ids.push(current_memory_id.clone());
                    }
                    let path_strength = if hop == 1 {
                        item.strength
                    } else {
                        (current_strength * item.strength).clamp(0.0, 1.0)
                    };
                    let mut lineage = frontier_item.lineage.clone();
                    lineage.push(MemoryV2RelatedLineageStep {
                        from_memory_id: current_memory_id.clone(),
                        to_memory_id: item.memory_id.clone(),
                        direction: item.direction,
                        link_type: item.link_type.clone(),
                        strength: item.strength,
                        provenance: item.provenance.clone(),
                    });
                    let candidate_path = MemoryV2RelatedPath {
                        hop_distance: hop,
                        strength: path_strength,
                        via_memory_ids: via_memory_ids.clone(),
                        lineage: lineage.clone(),
                        path_rank: 0,
                        selected: false,
                        selection_reason: String::new(),
                    };
                    aggregated
                        .entry(item.memory_id.clone())
                        .and_modify(|existing| {
                            let candidate_is_better = hop < existing.hop_distance
                                || (hop == existing.hop_distance
                                    && path_strength > existing.strength);
                            if candidate_is_better {
                                existing.hop_distance = hop;
                                existing.strength = path_strength;
                                existing.via_memory_ids = via_memory_ids.clone();
                                existing.lineage = lineage.clone();
                            } else {
                                existing.hop_distance = existing.hop_distance.min(hop);
                                existing.strength = existing.strength.max(path_strength);
                            }
                            if !existing.directions.contains(&item.direction) {
                                existing.directions.push(item.direction);
                            }
                            if !existing.link_types.contains(&item.link_type) {
                                existing.link_types.push(item.link_type.clone());
                            }
                            if !existing
                                .supporting_paths
                                .iter()
                                .any(|path| same_related_path(path, &candidate_path))
                            {
                                existing.supporting_path_count += 1;
                                existing.supporting_paths.push(candidate_path.clone());
                            }
                            reorder_supporting_paths(existing);
                        })
                        .or_insert_with(|| MemoryV2RelatedItem {
                            memory_id: item.memory_id.clone(),
                            abstract_text: item.abstract_text,
                            memory_type: item.memory_type,
                            session_id: item.session_id,
                            has_overview: item.has_overview,
                            has_detail: item.has_detail,
                            hop_distance: hop,
                            strength: path_strength,
                            via_memory_ids: via_memory_ids.clone(),
                            directions: vec![item.direction],
                            link_types: vec![item.link_type],
                            lineage: lineage.clone(),
                            supporting_path_count: 1,
                            supporting_paths_truncated: false,
                            supporting_paths: vec![candidate_path.clone()],
                            feedback_impact: MemoryV2FeedbackImpact::default(),
                            ranking: MemoryV2RelatedRanking::default(),
                        });
                    if let Some(existing) = aggregated.get_mut(&item.memory_id) {
                        reorder_supporting_paths(existing);
                    }
                    if hop < max_hops && !expanded.contains(&item.memory_id) {
                        next_frontier
                            .entry(item.memory_id.clone())
                            .and_modify(|best| {
                                if path_strength > best.strength {
                                    best.strength = path_strength;
                                    best.via_memory_ids = via_memory_ids.clone();
                                    best.lineage = lineage.clone();
                                }
                            })
                            .or_insert_with(|| RelatedFrontierItem {
                                memory_id: item.memory_id,
                                strength: path_strength,
                                via_memory_ids: via_memory_ids.clone(),
                                lineage: lineage.clone(),
                            });
                    }
                }
            }
            if hop >= max_hops {
                break;
            }
            frontier = next_frontier
                .into_iter()
                .map(|(memory_id, item)| {
                    expanded.insert(memory_id.clone());
                    item
                })
                .collect();
        }
        let mut items = aggregated.into_values().collect::<Vec<_>>();
        let ids = items
            .iter()
            .map(|item| item.memory_id.clone())
            .collect::<Vec<_>>();
        let access_counts = self.fetch_access_counts(&family, &ids).await?;
        let feedback_by_memory = self.fetch_feedback_batch(&family, &ids).await?;
        let focuses = self.load_active_focuses(&family).await?;
        let tag_focus_values: Vec<String> = focuses
            .iter()
            .filter(|f| f.focus_type == "tag")
            .map(|f| f.value.clone())
            .collect();
        let focused_by_tag = self
            .fetch_tag_focus_matches(&family, &ids, &tag_focus_values)
            .await?;
        let source_session_id = self
            .load_active_memory_session_id(&family, &req.memory_id)
            .await?;
        for item in &mut items {
            item.directions.sort_by_key(|direction| match direction {
                LinkDirection::Outbound => 0,
                LinkDirection::Inbound => 1,
                LinkDirection::Both => 2,
            });
            item.link_types.sort();
            let access_count = access_counts.get(&item.memory_id).copied().unwrap_or(0);
            let feedback = feedback_by_memory
                .get(&item.memory_id)
                .cloned()
                .unwrap_or_default();
            item.feedback_impact = self.feedback_impact(feedback);
            item.ranking = self.related_ranking(
                item,
                source_session_id.as_deref(),
                access_count,
                &item.feedback_impact,
                &focuses,
                &focused_by_tag,
            );
        }
        items.sort_by(|a, b| {
            a.hop_distance
                .cmp(&b.hop_distance)
                .then_with(|| {
                    b.ranking
                        .same_hop_score
                        .partial_cmp(&a.ranking.same_hop_score)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .then_with(|| {
                    b.strength
                        .partial_cmp(&a.strength)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .then_with(|| a.memory_id.cmp(&b.memory_id))
        });
        let discovered_count = items.len() as i64;
        let mut by_hop = HashMap::<i64, i64>::new();
        let mut by_link_type = HashMap::<String, i64>::new();
        for item in &items {
            *by_hop.entry(item.hop_distance).or_default() += 1;
            for link_type in &item.link_types {
                *by_link_type.entry(link_type.clone()).or_default() += 1;
            }
        }
        let mut by_hop = by_hop
            .into_iter()
            .map(|(hop_distance, count)| MemoryV2RelatedHopSummary {
                hop_distance,
                count,
            })
            .collect::<Vec<_>>();
        by_hop.sort_by(|a, b| a.hop_distance.cmp(&b.hop_distance));
        let mut link_types = by_link_type
            .into_iter()
            .map(|(link_type, count)| MemoryV2RelatedLinkTypeSummary { link_type, count })
            .collect::<Vec<_>>();
        link_types.sort_by(|a, b| {
            b.count
                .cmp(&a.count)
                .then_with(|| a.link_type.cmp(&b.link_type))
        });
        let truncated = discovered_count > limit;
        items.truncate(limit as usize);
        self.bump_access_counts(
            &family,
            &items
                .iter()
                .map(|item| item.memory_id.clone())
                .collect::<Vec<_>>(),
        )
        .await?;
        Ok(MemoryV2RelatedResult {
            memory_id: req.memory_id,
            summary: MemoryV2RelatedSummary {
                discovered_count,
                returned_count: items.len() as i64,
                truncated,
                by_hop,
                link_types,
            },
            items,
        })
    }

    pub async fn jobs(
        &self,
        user_id: &str,
        req: MemoryV2JobsRequest,
    ) -> Result<MemoryV2JobsResult, MemoriaError> {
        let family = self.ensure_user_tables(user_id).await?;
        let limit = req.limit.clamp(1, 200);
        let row = sqlx::query(&format!(
            "SELECT c.has_overview, c.has_detail, c.derivation_state \
             FROM {} h \
             JOIN {} c ON c.content_version_id = h.current_content_version_id \
             WHERE h.memory_id = ? AND h.forgotten_at IS NULL \
             LIMIT 1",
            family.heads_table, family.content_versions_table
        ))
        .bind(&req.memory_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?
        .ok_or_else(|| MemoriaError::NotFound(req.memory_id.clone()))?;
        let has_overview = row.try_get::<i8, _>("has_overview").unwrap_or(0) != 0;
        let has_detail = row.try_get::<i8, _>("has_detail").unwrap_or(0) != 0;
        let derivation_state = row
            .try_get::<String, _>("derivation_state")
            .unwrap_or_else(|_| "pending".to_string());
        let link_count = self
            .fetch_link_counts(&family, std::slice::from_ref(&req.memory_id))
            .await?
            .get(&req.memory_id)
            .copied()
            .unwrap_or(0);

        let rows = sqlx::query(&format!(
            "SELECT job_id, job_type, status, attempts, available_at, leased_until, created_at, updated_at, last_error \
             FROM {} \
             WHERE aggregate_id = ? \
             ORDER BY created_at DESC, job_id DESC",
            family.jobs_table
        ))
        .bind(&req.memory_id)
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;

        let mut pending_count = 0i64;
        let mut in_progress_count = 0i64;
        let mut done_count = 0i64;
        let mut failed_count = 0i64;
        let mut job_types = HashMap::<String, MemoryV2JobTypeSummary>::new();
        let mut items = Vec::new();

        for (idx, row) in rows.into_iter().enumerate() {
            let job_type: String = row.try_get("job_type").map_err(db_err)?;
            let status =
                normalize_job_status(row.try_get::<String, _>("status").map_err(db_err)?.as_str())
                    .to_string();
            let updated_at = to_utc(
                row.try_get::<NaiveDateTime, _>("updated_at")
                    .map_err(db_err)?,
            );
            let last_error: Option<String> = row.try_get("last_error").ok();
            match status.as_str() {
                "pending" => pending_count += 1,
                "in_progress" => in_progress_count += 1,
                "done" => done_count += 1,
                "failed" => failed_count += 1,
                _ => {}
            }
            let entry =
                job_types
                    .entry(job_type.clone())
                    .or_insert_with(|| MemoryV2JobTypeSummary {
                        job_type: job_type.clone(),
                        pending_count: 0,
                        in_progress_count: 0,
                        done_count: 0,
                        failed_count: 0,
                        latest_status: status.clone(),
                        latest_error: last_error.clone(),
                        latest_updated_at: updated_at,
                    });
            match status.as_str() {
                "pending" => entry.pending_count += 1,
                "in_progress" => entry.in_progress_count += 1,
                "done" => entry.done_count += 1,
                "failed" => entry.failed_count += 1,
                _ => {}
            }
            if updated_at >= entry.latest_updated_at {
                entry.latest_status = status.clone();
                entry.latest_error = last_error.clone();
                entry.latest_updated_at = updated_at;
            }

            if idx >= limit as usize {
                continue;
            }

            let available_at: NaiveDateTime = row.try_get("available_at").map_err(db_err)?;
            let leased_until: Option<NaiveDateTime> = row.try_get("leased_until").ok();
            let created_at: NaiveDateTime = row.try_get("created_at").map_err(db_err)?;
            items.push(MemoryV2JobItem {
                job_id: row.try_get("job_id").map_err(db_err)?,
                job_type,
                status,
                attempts: row.try_get("attempts").unwrap_or(0),
                available_at: to_utc(available_at),
                leased_until: leased_until.map(to_utc),
                created_at: to_utc(created_at),
                updated_at,
                last_error,
            });
        }
        let mut job_types = job_types.into_values().collect::<Vec<_>>();
        job_types.sort_by(|a, b| a.job_type.cmp(&b.job_type));

        Ok(MemoryV2JobsResult {
            memory_id: req.memory_id,
            derivation_state,
            has_overview,
            has_detail,
            link_count,
            pending_count,
            in_progress_count,
            done_count,
            failed_count,
            job_types,
            items,
        })
    }

    pub async fn job_metrics(&self, user_id: &str) -> Result<MemoryV2JobMetrics, MemoriaError> {
        let family = self.ensure_user_tables(user_id).await?;
        let counts: Vec<(String, i64)> = sqlx::query_as(&format!(
            "SELECT status, COUNT(*) AS cnt FROM {} GROUP BY status",
            family.jobs_table
        ))
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;
        let mut pending_count = 0i64;
        let mut in_progress_count = 0i64;
        let mut failed_count = 0i64;
        for (status, count) in counts {
            match normalize_job_status(status.as_str()) {
                "pending" => pending_count += count,
                "in_progress" => in_progress_count += count,
                "failed" => failed_count += count,
                _ => {}
            }
        }
        let avg_processing_time_secs: Option<f64> = sqlx::query_scalar(&format!(
            "SELECT AVG(TIMESTAMPDIFF(MICROSECOND, created_at, updated_at)) / 1000000.0 \
             FROM {} WHERE status = 'done'",
            family.jobs_table
        ))
        .fetch_one(&self.pool)
        .await
        .map_err(db_err)?;
        let oldest_pending_age_secs: i64 = sqlx::query_scalar(&format!(
            "SELECT COALESCE(MAX(TIMESTAMPDIFF(SECOND, created_at, NOW())), 0) \
             FROM {} WHERE status = 'pending'",
            family.jobs_table
        ))
        .fetch_one(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(MemoryV2JobMetrics {
            pending_count,
            in_progress_count,
            failed_count,
            avg_processing_time_ms: avg_processing_time_secs.unwrap_or(0.0) * 1000.0,
            oldest_pending_age_secs,
        })
    }

    pub async fn stats(&self, user_id: &str) -> Result<MemoryV2StatsResult, MemoriaError> {
        let family = self.ensure_user_tables(user_id).await?;
        let (total_memories, active_memories, forgotten_memories, distinct_sessions): (
            i64,
            i64,
            i64,
            i64,
        ) = sqlx::query_as(&format!(
            "SELECT \
               COUNT(*) AS total_memories, \
               COALESCE(SUM(CASE WHEN forgotten_at IS NULL THEN 1 ELSE 0 END), 0) AS active_memories, \
               COALESCE(SUM(CASE WHEN forgotten_at IS NOT NULL THEN 1 ELSE 0 END), 0) AS forgotten_memories, \
               COUNT(DISTINCT CASE \
                 WHEN forgotten_at IS NULL AND session_id IS NOT NULL AND session_id != '' THEN session_id \
               END) AS distinct_sessions \
             FROM {}",
            family.heads_table
        ))
        .fetch_one(&self.pool)
        .await
        .map_err(db_err)?;

        let (has_overview_count, has_detail_count): (i64, i64) = sqlx::query_as(&format!(
            "SELECT \
               COALESCE(SUM(CASE WHEN c.has_overview = 1 THEN 1 ELSE 0 END), 0) AS has_overview_count, \
               COALESCE(SUM(CASE WHEN c.has_detail = 1 THEN 1 ELSE 0 END), 0) AS has_detail_count \
             FROM {} h \
             JOIN {} c ON c.content_version_id = h.current_content_version_id \
             WHERE h.forgotten_at IS NULL",
            family.heads_table, family.content_versions_table
        ))
        .fetch_one(&self.pool)
        .await
        .map_err(db_err)?;

        let active_direct_links: i64 = sqlx::query_scalar(&format!(
            "SELECT COUNT(*) \
             FROM {} l \
             JOIN {} src ON src.memory_id = l.memory_id \
             JOIN {} dst ON dst.memory_id = l.target_memory_id \
             WHERE src.forgotten_at IS NULL AND dst.forgotten_at IS NULL",
            family.links_table, family.heads_table, family.heads_table
        ))
        .fetch_one(&self.pool)
        .await
        .map_err(db_err)?;

        let active_focus_count: i64 = sqlx::query_scalar(&format!(
            "SELECT COUNT(*) FROM {} WHERE state = 'active' AND expires_at > NOW()",
            family.focus_table
        ))
        .fetch_one(&self.pool)
        .await
        .map_err(db_err)?;

        let (tag_unique_count, tag_assignment_count): (i64, i64) = sqlx::query_as(&format!(
            "SELECT \
               COUNT(DISTINCT t.tag) AS unique_count, \
               COUNT(*) AS assignment_count \
             FROM {} t \
             JOIN {} h ON h.memory_id = t.memory_id \
             WHERE h.forgotten_at IS NULL",
            family.tags_table, family.heads_table
        ))
        .fetch_one(&self.pool)
        .await
        .map_err(db_err)?;

        let job_rows: Vec<(String, i64)> = sqlx::query_as(&format!(
            "SELECT status, COUNT(*) AS cnt \
             FROM {} \
             GROUP BY status",
            family.jobs_table
        ))
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;
        let mut jobs = MemoryV2JobStats {
            total_count: 0,
            pending_count: 0,
            in_progress_count: 0,
            done_count: 0,
            failed_count: 0,
        };
        for (status, count) in job_rows {
            jobs.total_count += count;
            match normalize_job_status(status.as_str()) {
                "pending" => jobs.pending_count += count,
                "in_progress" => jobs.in_progress_count += count,
                "done" => jobs.done_count += count,
                "failed" => jobs.failed_count += count,
                _ => {}
            }
        }

        let feedback = self.get_feedback_stats(user_id).await?;

        let mut by_type = sqlx::query_as::<_, (String, i64, i64, i64)>(&format!(
            "SELECT \
               memory_type, \
               COUNT(*) AS total_count, \
               COALESCE(SUM(CASE WHEN forgotten_at IS NULL THEN 1 ELSE 0 END), 0) AS active_count, \
               COALESCE(SUM(CASE WHEN forgotten_at IS NOT NULL THEN 1 ELSE 0 END), 0) AS forgotten_count \
             FROM {} \
             GROUP BY memory_type \
             ORDER BY memory_type",
            family.heads_table
        ))
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?
        .into_iter()
        .map(|(memory_type, total_count, active_count, forgotten_count)| MemoryV2StatsByType {
            memory_type,
            total_count,
            active_count,
            forgotten_count,
        })
        .collect::<Vec<_>>();
        by_type.sort_by(|a, b| a.memory_type.cmp(&b.memory_type));

        Ok(MemoryV2StatsResult {
            total_memories,
            active_memories,
            forgotten_memories,
            distinct_sessions,
            has_overview_count,
            has_detail_count,
            active_direct_links,
            active_focus_count,
            tags: MemoryV2TagStats {
                unique_count: tag_unique_count,
                assignment_count: tag_assignment_count,
            },
            jobs,
            feedback,
            by_type,
        })
    }

    async fn wait_for_memory_jobs_complete(
        &self,
        user_id: &str,
        memory_id: &str,
        timeout_secs: u64,
    ) -> Result<(), MemoriaError> {
        let deadline = Instant::now() + Duration::from_secs(timeout_secs);
        let mut poll_delay = Duration::from_millis(100);
        loop {
            let status = self
                .jobs(
                    user_id,
                    MemoryV2JobsRequest {
                        memory_id: memory_id.to_string(),
                        limit: 50,
                    },
                )
                .await?;
            if status.pending_count == 0 && status.in_progress_count == 0 {
                if status.failed_count > 0 {
                    return Err(MemoriaError::Blocked(format!(
                        "sync enrichment failed for memory_id={memory_id}; failed={}, derivation_state={}",
                        status.failed_count, status.derivation_state
                    )));
                }
                if status.derivation_state == "complete" {
                    return Ok(());
                }
            }
            if Instant::now() >= deadline {
                return Err(MemoriaError::Blocked(format!(
                    "sync enrichment timeout for memory_id={memory_id}; pending={}, in_progress={}, failed={}, derivation_state={}",
                    status.pending_count, status.in_progress_count, status.failed_count, status.derivation_state
                )));
            }
            sleep(poll_delay).await;
            poll_delay = std::cmp::min(poll_delay * 2, Duration::from_secs(2));
        }
    }

    pub async fn expand_batch(
        &self,
        user_id: &str,
        memory_ids: &[String],
        level: ExpandLevel,
    ) -> Result<Vec<MemoryV2ExpandResult>, MemoriaError> {
        if memory_ids.is_empty() {
            return Ok(vec![]);
        }
        let family = self.ensure_user_tables(user_id).await?;
        let placeholders = memory_ids.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
        let sql = format!(
            "SELECT h.memory_id, c.abstract_text, c.overview_text, c.detail_text \
             FROM {} h JOIN {} c ON c.content_version_id = h.current_content_version_id \
             WHERE h.memory_id IN ({}) AND h.forgotten_at IS NULL",
            family.heads_table, family.content_versions_table, placeholders
        );
        let mut q = sqlx::query(&sql);
        for memory_id in memory_ids {
            q = q.bind(memory_id);
        }
        let rows = q.fetch_all(&self.pool).await.map_err(db_err)?;
        let mut by_id = HashMap::<String, (String, Option<String>, Option<String>)>::new();
        for row in rows {
            let memory_id: String = row.try_get("memory_id").map_err(db_err)?;
            let abstract_text: String = row.try_get("abstract_text").map_err(db_err)?;
            let overview_text: Option<String> = row.try_get("overview_text").map_err(db_err)?;
            let detail_text: Option<String> = row.try_get("detail_text").map_err(db_err)?;
            by_id.insert(memory_id, (abstract_text, overview_text, detail_text));
        }
        for memory_id in memory_ids {
            if !by_id.contains_key(memory_id) {
                return Err(MemoriaError::NotFound(memory_id.clone()));
            }
        }
        let links_by_id = if level == ExpandLevel::Links {
            self.fetch_link_refs(&family, memory_ids).await?
        } else {
            HashMap::new()
        };
        let mut out = Vec::with_capacity(memory_ids.len());
        for memory_id in memory_ids {
            let (abstract_text, overview_text, detail_text) = by_id
                .remove(memory_id)
                .ok_or_else(|| MemoriaError::NotFound(memory_id.clone()))?;
            out.push(MemoryV2ExpandResult {
                memory_id: memory_id.clone(),
                level,
                abstract_text,
                overview_text,
                detail_text,
                links: if level == ExpandLevel::Links {
                    Some(links_by_id.get(memory_id).cloned().unwrap_or_default())
                } else {
                    None
                },
            });
        }
        Ok(out)
    }

    pub async fn memory_history(
        &self,
        user_id: &str,
        memory_id: &str,
        limit: i64,
    ) -> Result<MemoryV2HistoryResult, MemoriaError> {
        let family = self.ensure_user_tables(user_id).await?;
        let limit = limit.clamp(1, 200);
        let exists: i64 = sqlx::query_scalar(&format!(
            "SELECT COUNT(*) FROM {} WHERE memory_id = ?",
            family.heads_table
        ))
        .bind(memory_id)
        .fetch_one(&self.pool)
        .await
        .map_err(db_err)?;
        if exists == 0 {
            return Err(MemoriaError::NotFound(memory_id.to_string()));
        }

        let rows = sqlx::query(&format!(
            "SELECT event_id, event_type, actor, processing_state, payload_json, created_at \
             FROM {} \
             WHERE aggregate_type = 'memory' AND aggregate_id = ? \
             ORDER BY created_at DESC, event_id DESC \
             LIMIT {}",
            family.events_table, limit
        ))
        .bind(memory_id)
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;

        let items = rows
            .into_iter()
            .map(|row| -> Result<MemoryV2HistoryEntry, MemoriaError> {
                Ok(MemoryV2HistoryEntry {
                    event_id: row.try_get("event_id").map_err(db_err)?,
                    event_type: row.try_get("event_type").map_err(db_err)?,
                    actor: row.try_get("actor").map_err(db_err)?,
                    processing_state: row.try_get("processing_state").map_err(db_err)?,
                    payload: row.try_get("payload_json").map_err(db_err)?,
                    created_at: to_utc(row.try_get("created_at").map_err(db_err)?),
                })
            })
            .collect::<Result<Vec<_>, _>>()?;

        Ok(MemoryV2HistoryResult {
            memory_id: memory_id.to_string(),
            items,
        })
    }

    pub async fn recall(
        &self,
        user_id: &str,
        mut req: RecallV2Request,
    ) -> Result<MemoryV2RecallResult, MemoriaError> {
        if req.query.trim().is_empty() {
            return Err(MemoriaError::Validation("query must not be empty".into()));
        }
        if req.session_only && req.session_id.is_none() {
            return Err(MemoriaError::Validation(
                "session_id is required when scope=session".into(),
            ));
        }
        if let Some(ref embedding) = req.query_embedding {
            self.validate_embedding_dim(embedding, "query")?;
        }
        req.tags = normalize_tags(req.tags);
        req.tag_filter_mode = match req.tag_filter_mode.trim() {
            "" | "any" => "any".to_string(),
            "all" => "all".to_string(),
            other => {
                return Err(MemoriaError::Validation(format!(
                    "tag_filter_mode must be 'any' or 'all', got {other}"
                )))
            }
        };
        if let (Some(created_after), Some(created_before)) =
            (req.created_after.as_ref(), req.created_before.as_ref())
        {
            if created_after > created_before {
                return Err(MemoriaError::Validation(
                    "created_after must be less than or equal to created_before".into(),
                ));
            }
        }
        let family = self.ensure_user_tables(user_id).await?;
        let top_k = req.top_k.clamp(1, MAX_TOP_K);
        let max_tokens = req.max_tokens.clamp(1, MAX_MAX_TOKENS);
        let fetch_k = (top_k * 3).max(DEFAULT_TOP_K);

        let fulltext_future = self.search_fulltext_candidates(&family, &req, fetch_k);
        let vector_future = async {
            match req.query_embedding.as_ref() {
                Some(embedding) => {
                    self.search_vector_candidates(&family, &req, embedding, fetch_k)
                        .await
                }
                None => Ok(vec![]),
            }
        };
        let entity_future = self.search_entity_candidates(&family, &req, fetch_k);
        let (fulltext_candidates, vector_candidates, entity_candidates) =
            tokio::join!(fulltext_future, vector_future, entity_future);

        let mut merged: HashMap<String, RecallCandidate> = HashMap::new();
        for (candidate, score) in fulltext_candidates? {
            merged
                .entry(candidate.memory_id.clone())
                .and_modify(|existing| {
                    existing.keyword_score = existing.keyword_score.max(score);
                    existing.direct_match = true;
                })
                .or_insert(RecallCandidate {
                    direct_match: true,
                    keyword_score: score,
                    ..candidate
                });
        }
        for (candidate, score) in vector_candidates? {
            merged
                .entry(candidate.memory_id.clone())
                .and_modify(|existing| {
                    existing.vector_score = existing.vector_score.max(score);
                    existing.direct_match = true;
                })
                .or_insert(RecallCandidate {
                    direct_match: true,
                    vector_score: score,
                    ..candidate
                });
        }
        for (candidate, score) in entity_candidates? {
            merged
                .entry(candidate.memory_id.clone())
                .and_modify(|existing| {
                    existing.entity_score = existing.entity_score.max(score);
                    existing.direct_match = true;
                })
                .or_insert(RecallCandidate {
                    direct_match: true,
                    entity_score: score,
                    ..candidate
                });
        }

        if merged.is_empty() {
            return Ok(MemoryV2RecallResult {
                summary: MemoryV2RecallSummary::default(),
                memories: vec![],
                token_used: 0,
                has_more: false,
            });
        }

        self.populate_recall_candidate_metadata(&family, &req, &mut merged)
            .await?;
        if req.expand_links {
            self.expand_recall_candidates_via_links(&family, &req, &mut merged, top_k)
                .await?;
            self.populate_recall_candidate_metadata(&family, &req, &mut merged)
                .await?;
        }

        let mut candidates: Vec<RecallCandidate> = merged.into_values().collect();

        // Apply query intent type-affinity boost before sorting.
        if let Some(intent_type) = Self::detect_query_intent(&req.query) {
            for candidate in candidates.iter_mut() {
                if candidate.memory_type == intent_type {
                    candidate.type_affinity_boost = DEFAULT_V2_TYPE_AFFINITY_BOOST;
                }
            }
        }
        candidates.sort_by(|a, b| {
            self.final_score(b)
                .partial_cmp(&self.final_score(a))
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        let mut summary = MemoryV2RecallSummary {
            discovered_count: candidates.len() as i64,
            ..MemoryV2RecallSummary::default()
        };
        for candidate in &candidates {
            let retrieval_path = self.recall_path(candidate);
            if let Some(bucket) = summary
                .by_retrieval_path
                .iter_mut()
                .find(|bucket| bucket.retrieval_path == retrieval_path)
            {
                bucket.discovered_count += 1;
            }
        }

        let mut selected: Vec<MemoryV2RecallItem> = Vec::new();
        let mut token_used = 0usize;
        let mut has_more = false;
        for candidate in candidates.into_iter() {
            if selected.len() >= top_k as usize {
                has_more = true;
                break;
            }
            let mut item_tokens = candidate.abstract_tokens.max(1) as usize;
            let mut overview_text = None;
            if req.with_overview
                && candidate.has_overview
                && candidate.overview_text.is_some()
                && token_used + item_tokens + candidate.overview_tokens.max(1) as usize
                    <= max_tokens
            {
                item_tokens += candidate.overview_tokens.max(1) as usize;
                overview_text = candidate.overview_text.clone();
            }
            if token_used + item_tokens > max_tokens {
                has_more = true;
                break;
            }
            token_used += item_tokens;
            let score = self.final_score(&candidate);
            let ranking = self.recall_ranking(&candidate);
            let memory_type = candidate.memory_type.clone();
            let confidence = candidate.confidence;
            let has_overview = candidate.has_overview;
            let has_detail = candidate.has_detail;
            let access_count = candidate.access_count;
            let link_count = candidate.link_count;
            let retrieval_path = self.recall_path(&candidate);
            let feedback_impact = self.feedback_impact(candidate.feedback.clone());
            if let Some(bucket) = summary
                .by_retrieval_path
                .iter_mut()
                .find(|bucket| bucket.retrieval_path == retrieval_path)
            {
                bucket.returned_count += 1;
            }
            selected.push(MemoryV2RecallItem {
                memory_id: candidate.memory_id,
                abstract_text: candidate.abstract_text,
                overview_text,
                score,
                memory_type,
                confidence,
                has_overview,
                has_detail,
                access_count,
                link_count,
                has_related: link_count > 0,
                retrieval_path,
                links: None,
                feedback_impact,
                ranking,
            });
        }

        if req.with_links && !selected.is_empty() && token_used < max_tokens {
            let selected_ids: Vec<String> = selected.iter().map(|m| m.memory_id.clone()).collect();
            let refs = self.fetch_link_refs(&family, &selected_ids).await?;
            for item in &mut selected {
                if let Some(candidate_links) = refs.get(&item.memory_id) {
                    let mut accepted = Vec::new();
                    for link in candidate_links {
                        let link_tokens = estimate_tokens(&link.abstract_text).max(1) as usize;
                        if token_used + link_tokens > max_tokens {
                            has_more = true;
                            break;
                        }
                        token_used += link_tokens;
                        accepted.push(link.clone());
                    }
                    if !accepted.is_empty() {
                        item.links = Some(accepted);
                    }
                }
            }
        }

        summary.returned_count = selected.len() as i64;
        summary.truncated = has_more;

        self.bump_access_counts(
            &family,
            &selected
                .iter()
                .map(|m| m.memory_id.clone())
                .collect::<Vec<_>>(),
        )
        .await?;

        Ok(MemoryV2RecallResult {
            summary,
            memories: selected,
            token_used,
            has_more,
        })
    }

    pub async fn record_feedback(
        &self,
        user_id: &str,
        memory_id: &str,
        signal: &str,
        context: Option<&str>,
    ) -> Result<String, MemoriaError> {
        Self::validate_feedback_signal(signal)?;
        let family = self.ensure_user_tables(user_id).await?;
        let count: i64 = sqlx::query_scalar(&format!(
            "SELECT COUNT(*) FROM {} WHERE memory_id = ? AND forgotten_at IS NULL",
            family.heads_table
        ))
        .bind(memory_id)
        .fetch_one(&self.pool)
        .await
        .map_err(db_err)?;
        if count == 0 {
            return Err(MemoriaError::NotFound(format!(
                "Memory {} not found or not active for user",
                memory_id
            )));
        }

        let feedback_id = uuid_id();
        sqlx::query(&format!(
            "INSERT INTO {} (feedback_id, memory_id, signal, context, created_at) \
             VALUES (?, ?, ?, ?, NOW())",
            family.feedback_table
        ))
        .bind(&feedback_id)
        .bind(memory_id)
        .bind(signal)
        .bind(context)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;

        let col = match signal {
            "useful" => "feedback_useful",
            "irrelevant" => "feedback_irrelevant",
            "outdated" => "feedback_outdated",
            "wrong" => "feedback_wrong",
            _ => unreachable!(),
        };
        let sql = format!(
            "INSERT INTO {} (memory_id, {col}, last_feedback_at) VALUES (?, 1, NOW()) \
             ON DUPLICATE KEY UPDATE {col} = {col} + 1, last_feedback_at = VALUES(last_feedback_at)",
            family.stats_table
        );
        sqlx::query(&sql)
            .bind(memory_id)
            .execute(&self.pool)
            .await
            .map_err(db_err)?;

        Ok(feedback_id)
    }

    pub async fn get_feedback_stats(&self, user_id: &str) -> Result<FeedbackStats, MemoriaError> {
        let family = self.ensure_user_tables(user_id).await?;
        let row: (i64, i64, i64, i64, i64) = sqlx::query_as(&format!(
            "SELECT \
               COUNT(*) as total, \
               COALESCE(SUM(CASE WHEN signal = 'useful' THEN 1 ELSE 0 END), 0) as useful, \
               COALESCE(SUM(CASE WHEN signal = 'irrelevant' THEN 1 ELSE 0 END), 0) as irrelevant, \
               COALESCE(SUM(CASE WHEN signal = 'outdated' THEN 1 ELSE 0 END), 0) as outdated, \
               COALESCE(SUM(CASE WHEN signal = 'wrong' THEN 1 ELSE 0 END), 0) as wrong \
             FROM {}",
            family.feedback_table
        ))
        .fetch_one(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(FeedbackStats {
            total: row.0,
            useful: row.1,
            irrelevant: row.2,
            outdated: row.3,
            wrong: row.4,
        })
    }

    pub async fn get_feedback_by_tier(
        &self,
        user_id: &str,
    ) -> Result<Vec<TierFeedback>, MemoriaError> {
        let family = self.ensure_user_tables(user_id).await?;
        let rows: Vec<(String, String, i64)> = sqlx::query_as(&format!(
            "SELECT h.trust_tier, f.signal, COUNT(*) as cnt \
             FROM {} f \
             JOIN {} h ON h.memory_id = f.memory_id \
             GROUP BY h.trust_tier, f.signal \
             ORDER BY h.trust_tier, f.signal",
            family.feedback_table, family.heads_table
        ))
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(rows
            .into_iter()
            .map(|(tier, signal, count)| TierFeedback {
                tier,
                signal,
                count,
            })
            .collect())
    }

    pub async fn get_memory_feedback(
        &self,
        user_id: &str,
        memory_id: &str,
    ) -> Result<MemoryV2FeedbackSummary, MemoriaError> {
        let family = self.ensure_user_tables(user_id).await?;
        self.ensure_active_memory(&family, memory_id).await?;
        let row: Option<(i32, i32, i32, i32, Option<NaiveDateTime>)> = sqlx::query_as(&format!(
            "SELECT feedback_useful, feedback_irrelevant, feedback_outdated, feedback_wrong, last_feedback_at \
             FROM {} WHERE memory_id = ?",
            family.stats_table
        ))
        .bind(memory_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?;
        let (feedback, last_feedback_at) = row
            .map(|(useful, irrelevant, outdated, wrong, last_feedback_at)| {
                (
                    MemoryFeedback {
                        useful,
                        irrelevant,
                        outdated,
                        wrong,
                    },
                    last_feedback_at.map(to_utc),
                )
            })
            .unwrap_or((MemoryFeedback::default(), None));
        Ok(MemoryV2FeedbackSummary {
            memory_id: memory_id.to_string(),
            feedback,
            last_feedback_at,
        })
    }

    pub async fn get_memory_feedback_history(
        &self,
        user_id: &str,
        memory_id: &str,
        limit: i64,
    ) -> Result<MemoryV2FeedbackHistoryResult, MemoriaError> {
        let family = self.ensure_user_tables(user_id).await?;
        self.ensure_active_memory(&family, memory_id).await?;
        let limit = limit.clamp(1, 100);
        let rows = sqlx::query(&format!(
            "SELECT feedback_id, signal, context, created_at \
             FROM {} WHERE memory_id = ? \
             ORDER BY created_at DESC, feedback_id DESC \
             LIMIT {}",
            family.feedback_table, limit
        ))
        .bind(memory_id)
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;
        let items = rows
            .into_iter()
            .map(|row| -> Result<MemoryV2FeedbackEntry, MemoriaError> {
                Ok(MemoryV2FeedbackEntry {
                    feedback_id: row.try_get("feedback_id").map_err(db_err)?,
                    signal: row.try_get("signal").map_err(db_err)?,
                    context: row.try_get("context").ok(),
                    created_at: to_utc(row.try_get("created_at").map_err(db_err)?),
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(MemoryV2FeedbackHistoryResult {
            memory_id: memory_id.to_string(),
            items,
        })
    }

    pub async fn list_feedback_history(
        &self,
        user_id: &str,
        memory_id: Option<&str>,
        signal: Option<&str>,
        limit: i64,
    ) -> Result<MemoryV2FeedbackFeedResult, MemoriaError> {
        if let Some(signal) = signal {
            Self::validate_feedback_signal(signal)?;
        }
        let family = self.ensure_user_tables(user_id).await?;
        let limit = limit.clamp(1, 100);
        let mut sql = format!(
            "SELECT f.feedback_id, f.memory_id, c.abstract_text, f.signal, f.context, f.created_at \
             FROM {} f \
             LEFT JOIN {} h ON h.memory_id = f.memory_id \
             LEFT JOIN {} c ON c.content_version_id = h.current_content_version_id",
            family.feedback_table, family.heads_table, family.content_versions_table
        );
        let mut clauses = Vec::new();
        if memory_id.is_some() {
            clauses.push("f.memory_id = ?");
        }
        if signal.is_some() {
            clauses.push("f.signal = ?");
        }
        if !clauses.is_empty() {
            sql.push_str(" WHERE ");
            sql.push_str(&clauses.join(" AND "));
        }
        sql.push_str(&format!(
            " ORDER BY f.created_at DESC, f.feedback_id DESC LIMIT {}",
            limit
        ));

        let mut query = sqlx::query(&sql);
        if let Some(memory_id) = memory_id {
            query = query.bind(memory_id);
        }
        if let Some(signal) = signal {
            query = query.bind(signal);
        }

        let rows = query.fetch_all(&self.pool).await.map_err(db_err)?;
        let items = rows
            .into_iter()
            .map(|row| -> Result<MemoryV2FeedbackFeedItem, MemoriaError> {
                Ok(MemoryV2FeedbackFeedItem {
                    feedback_id: row.try_get("feedback_id").map_err(db_err)?,
                    memory_id: row.try_get("memory_id").map_err(db_err)?,
                    abstract_text: row.try_get("abstract_text").ok(),
                    signal: row.try_get("signal").map_err(db_err)?,
                    context: row.try_get("context").ok(),
                    created_at: to_utc(row.try_get("created_at").map_err(db_err)?),
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(MemoryV2FeedbackFeedResult { items })
    }

    pub async fn process_pending_jobs_pass(&self, max_jobs: usize) -> Result<usize, MemoriaError> {
        self.process_pending_jobs_with_enricher_pass(max_jobs, None)
            .await
    }

    pub async fn process_pending_jobs_with_enricher_pass(
        &self,
        max_jobs: usize,
        enricher: Option<&dyn MemoryV2JobEnricher>,
    ) -> Result<usize, MemoriaError> {
        if max_jobs == 0 {
            return Ok(0);
        }
        let user_ids = self.list_registered_users().await?;
        if user_ids.is_empty() {
            return Ok(0);
        }

        let mut processed = 0usize;
        let mut made_progress = true;
        while processed < max_jobs && made_progress {
            made_progress = false;
            for user_id in &user_ids {
                if processed >= max_jobs {
                    break;
                }
                let user_processed = self
                    .process_user_pending_jobs_with_enricher_pass(
                        user_id,
                        max_jobs - processed,
                        enricher,
                    )
                    .await?;
                if user_processed > 0 {
                    made_progress = true;
                    processed += user_processed;
                }
            }
        }
        Ok(processed)
    }

    pub async fn process_user_pending_jobs_pass(
        &self,
        user_id: &str,
        max_jobs: usize,
    ) -> Result<usize, MemoriaError> {
        self.process_user_pending_jobs_with_enricher_pass(user_id, max_jobs, None)
            .await
    }

    pub async fn process_user_pending_jobs_with_enricher_pass(
        &self,
        user_id: &str,
        max_jobs: usize,
        enricher: Option<&dyn MemoryV2JobEnricher>,
    ) -> Result<usize, MemoriaError> {
        if max_jobs == 0 {
            return Ok(0);
        }
        let mut processed = 0usize;
        while processed < max_jobs {
            let Some(job) = self.claim_next_job_for_user(user_id).await? else {
                break;
            };
            let outcome = self.process_claimed_job(&job, enricher).await;
            match outcome {
                Ok(()) => self.mark_job_done(&job).await?,
                Err(err) => self.mark_job_failed(&job, &err.to_string()).await?,
            }
            processed += 1;
        }
        Ok(processed)
    }

    async fn ensure_registry_table(&self) -> Result<(), MemoriaError> {
        sqlx::query(&format!(
            r#"CREATE TABLE IF NOT EXISTS {} (
                user_id VARCHAR(64) PRIMARY KEY,
                table_suffix VARCHAR(32) NOT NULL,
                events_table VARCHAR(64) NOT NULL,
                heads_table VARCHAR(64) NOT NULL,
                content_versions_table VARCHAR(64) NOT NULL,
                index_docs_table VARCHAR(64) NOT NULL,
                links_table VARCHAR(64) NOT NULL,
                entities_table VARCHAR(64) NOT NULL,
                memory_entities_table VARCHAR(64) NOT NULL,
                focus_table VARCHAR(64) NOT NULL,
                jobs_table VARCHAR(64) NOT NULL,
                tags_table VARCHAR(64) NOT NULL,
                stats_table VARCHAR(64) NOT NULL,
                feedback_table VARCHAR(64) NOT NULL,
                created_at DATETIME(6) NOT NULL,
                updated_at DATETIME(6) NOT NULL
            )"#,
            REGISTRY_TABLE
        ))
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        if should_attempt_schema_patch(REGISTRY_TABLE) {
            let _ = sqlx::query(&format!(
                "ALTER TABLE {} ADD COLUMN feedback_table VARCHAR(64) NOT NULL DEFAULT ''",
                REGISTRY_TABLE
            ))
            .execute(&self.pool)
            .await;
            let _ = sqlx::query(&format!(
                "ALTER TABLE {} ADD COLUMN entities_table VARCHAR(64) NOT NULL DEFAULT ''",
                REGISTRY_TABLE
            ))
            .execute(&self.pool)
            .await;
            let _ = sqlx::query(&format!(
                "ALTER TABLE {} ADD COLUMN memory_entities_table VARCHAR(64) NOT NULL DEFAULT ''",
                REGISTRY_TABLE
            ))
            .execute(&self.pool)
            .await;
        }
        Ok(())
    }

    async fn register_family(
        &self,
        user_id: &str,
        family: &MemoryV2TableFamily,
    ) -> Result<(), MemoriaError> {
        let now = Utc::now().naive_utc();
        sqlx::query(
            r#"INSERT INTO mem_v2_user_tables
               (user_id, table_suffix, events_table, heads_table, content_versions_table, index_docs_table,
                links_table, entities_table, memory_entities_table, focus_table, jobs_table, tags_table, stats_table, feedback_table, created_at, updated_at)
               VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
               ON DUPLICATE KEY UPDATE
                table_suffix = VALUES(table_suffix),
                events_table = VALUES(events_table),
                heads_table = VALUES(heads_table),
                content_versions_table = VALUES(content_versions_table),
                index_docs_table = VALUES(index_docs_table),
                links_table = VALUES(links_table),
                entities_table = VALUES(entities_table),
                memory_entities_table = VALUES(memory_entities_table),
                focus_table = VALUES(focus_table),
                jobs_table = VALUES(jobs_table),
                tags_table = VALUES(tags_table),
                stats_table = VALUES(stats_table),
                feedback_table = VALUES(feedback_table),
                updated_at = VALUES(updated_at)"#,
        )
        .bind(user_id)
        .bind(&family.suffix)
        .bind(&family.events_table)
        .bind(&family.heads_table)
        .bind(&family.content_versions_table)
        .bind(&family.index_docs_table)
        .bind(&family.links_table)
        .bind(&family.entities_table)
        .bind(&family.memory_entities_table)
        .bind(&family.focus_table)
        .bind(&family.jobs_table)
        .bind(&family.tags_table)
        .bind(&family.stats_table)
        .bind(&family.feedback_table)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn list_registered_users(&self) -> Result<Vec<String>, MemoriaError> {
        self.ensure_registry_table().await?;
        let rows = sqlx::query(&format!(
            "SELECT user_id FROM {} ORDER BY updated_at DESC",
            REGISTRY_TABLE
        ))
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(rows
            .into_iter()
            .filter_map(|row| row.try_get("user_id").ok())
            .collect())
    }

    async fn create_events_table(&self, family: &MemoryV2TableFamily) -> Result<(), MemoriaError> {
        sqlx::query(&format!(
            r#"CREATE TABLE IF NOT EXISTS {} (
                event_id VARCHAR(64) PRIMARY KEY,
                aggregate_type VARCHAR(32) NOT NULL,
                aggregate_id VARCHAR(64) NOT NULL,
                event_type VARCHAR(32) NOT NULL,
                event_version INT NOT NULL DEFAULT 1,
                causation_id VARCHAR(64),
                correlation_id VARCHAR(64),
                actor VARCHAR(64) NOT NULL,
                payload_json JSON NOT NULL,
                processing_state VARCHAR(16) NOT NULL,
                created_at DATETIME(6) NOT NULL,
                INDEX idx_aggregate (aggregate_id, created_at),
                INDEX idx_event_type (event_type, created_at),
                INDEX idx_processing (processing_state, created_at)
            )"#,
            family.events_table
        ))
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn create_heads_table(&self, family: &MemoryV2TableFamily) -> Result<(), MemoriaError> {
        sqlx::query(&format!(
            r#"CREATE TABLE IF NOT EXISTS {} (
                memory_id VARCHAR(64) PRIMARY KEY,
                memory_type VARCHAR(20) NOT NULL,
                session_id VARCHAR(64),
                trust_tier VARCHAR(10) NOT NULL,
                confidence FLOAT NOT NULL,
                importance FLOAT NOT NULL DEFAULT 0,
                source_kind VARCHAR(32),
                source_app VARCHAR(64),
                source_message_id VARCHAR(128),
                source_turn_id VARCHAR(64),
                source_json JSON,
                is_active TINYINT(1) NOT NULL DEFAULT 1,
                forgotten_at DATETIME(6),
                current_content_version_id VARCHAR(64) NOT NULL,
                current_index_doc_id VARCHAR(64) NOT NULL,
                latest_event_id VARCHAR(64) NOT NULL,
                created_at DATETIME(6) NOT NULL,
                updated_at DATETIME(6) NOT NULL,
                INDEX idx_active_recent (is_active, created_at, memory_id),
                INDEX idx_session_active (session_id, is_active, created_at),
                INDEX idx_type_active (memory_type, is_active, created_at),
                INDEX idx_current_index_doc (current_index_doc_id)
            )"#,
            family.heads_table
        ))
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn create_content_versions_table(
        &self,
        family: &MemoryV2TableFamily,
    ) -> Result<(), MemoriaError> {
        sqlx::query(&format!(
            r#"CREATE TABLE IF NOT EXISTS {} (
                content_version_id VARCHAR(64) PRIMARY KEY,
                memory_id VARCHAR(64) NOT NULL,
                source_text TEXT NOT NULL,
                abstract_text TEXT NOT NULL,
                overview_text TEXT,
                detail_text LONGTEXT,
                has_overview TINYINT(1) NOT NULL DEFAULT 0,
                has_detail TINYINT(1) NOT NULL DEFAULT 0,
                abstract_token_estimate INT NOT NULL DEFAULT 0,
                overview_token_estimate INT NOT NULL DEFAULT 0,
                detail_token_estimate INT NOT NULL DEFAULT 0,
                derivation_state VARCHAR(16) NOT NULL,
                created_at DATETIME(6) NOT NULL,
                INDEX idx_memory_created (memory_id, created_at)
            )"#,
            family.content_versions_table
        ))
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn create_index_docs_table(
        &self,
        family: &MemoryV2TableFamily,
    ) -> Result<(), MemoriaError> {
        sqlx::query(&format!(
            r#"CREATE TABLE IF NOT EXISTS {} (
                index_doc_id VARCHAR(64) PRIMARY KEY,
                memory_id VARCHAR(64) NOT NULL,
                content_version_id VARCHAR(64) NOT NULL,
                recall_text TEXT NOT NULL,
                embedding vecf32({}) DEFAULT NULL,
                memory_type VARCHAR(20) NOT NULL,
                session_id VARCHAR(64),
                confidence FLOAT NOT NULL,
                created_at DATETIME(6) NOT NULL,
                published_at DATETIME(6) NOT NULL,
                INDEX idx_memory_created (memory_id, created_at),
                INDEX idx_content_version (content_version_id),
                FULLTEXT INDEX ft_recall (recall_text) WITH PARSER ngram
            )"#,
            family.index_docs_table, self.embedding_dim
        ))
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn create_links_table(&self, family: &MemoryV2TableFamily) -> Result<(), MemoriaError> {
        sqlx::query(&format!(
            r#"CREATE TABLE IF NOT EXISTS {} (
                link_id VARCHAR(64) PRIMARY KEY,
                memory_id VARCHAR(64) NOT NULL,
                target_memory_id VARCHAR(64) NOT NULL,
                link_type VARCHAR(32) NOT NULL,
                strength FLOAT NOT NULL,
                created_at DATETIME(6) NOT NULL,
                INDEX idx_memory (memory_id),
                INDEX idx_target (target_memory_id)
            )"#,
            family.links_table
        ))
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn create_entities_table(
        &self,
        family: &MemoryV2TableFamily,
    ) -> Result<(), MemoriaError> {
        sqlx::query(&format!(
            r#"CREATE TABLE IF NOT EXISTS {} (
                entity_id VARCHAR(64) PRIMARY KEY,
                name VARCHAR(200) NOT NULL,
                display_name VARCHAR(200) NOT NULL,
                entity_type VARCHAR(32) NOT NULL,
                created_at DATETIME(6) NOT NULL,
                updated_at DATETIME(6) NOT NULL,
                UNIQUE KEY uq_name_type (name, entity_type),
                INDEX idx_updated (updated_at, entity_id),
                INDEX idx_name (name)
            )"#,
            family.entities_table
        ))
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn create_memory_entities_table(
        &self,
        family: &MemoryV2TableFamily,
    ) -> Result<(), MemoriaError> {
        sqlx::query(&format!(
            r#"CREATE TABLE IF NOT EXISTS {} (
                memory_id VARCHAR(64) NOT NULL,
                content_version_id VARCHAR(64) NOT NULL,
                entity_id VARCHAR(64) NOT NULL,
                source VARCHAR(16) NOT NULL,
                weight FLOAT NOT NULL,
                created_at DATETIME(6) NOT NULL,
                PRIMARY KEY (memory_id, content_version_id, entity_id),
                INDEX idx_entity (entity_id, created_at),
                INDEX idx_memory (memory_id, content_version_id)
            )"#,
            family.memory_entities_table
        ))
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn create_focus_table(&self, family: &MemoryV2TableFamily) -> Result<(), MemoriaError> {
        sqlx::query(&format!(
            r#"CREATE TABLE IF NOT EXISTS {} (
                focus_id VARCHAR(64) PRIMARY KEY,
                focus_type VARCHAR(32) NOT NULL,
                focus_value VARCHAR(200) NOT NULL,
                boost FLOAT NOT NULL,
                state VARCHAR(16) NOT NULL,
                expires_at DATETIME(6) NOT NULL,
                created_at DATETIME(6) NOT NULL,
                updated_at DATETIME(6) NOT NULL,
                UNIQUE KEY uq_focus_value (focus_type, focus_value),
                INDEX idx_state_expiry (state, expires_at)
            )"#,
            family.focus_table
        ))
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn create_jobs_table(&self, family: &MemoryV2TableFamily) -> Result<(), MemoriaError> {
        sqlx::query(&format!(
            r#"CREATE TABLE IF NOT EXISTS {} (
                job_id VARCHAR(64) PRIMARY KEY,
                job_type VARCHAR(32) NOT NULL,
                aggregate_id VARCHAR(64) NOT NULL,
                payload_json JSON NOT NULL,
                dedupe_key VARCHAR(128) NOT NULL,
                status VARCHAR(16) NOT NULL,
                available_at DATETIME(6) NOT NULL,
                leased_until DATETIME(6),
                attempts INT NOT NULL DEFAULT 0,
                last_error TEXT,
                created_at DATETIME(6) NOT NULL,
                updated_at DATETIME(6) NOT NULL,
                UNIQUE KEY uq_dedupe (dedupe_key),
                INDEX idx_status_available (status, available_at),
                INDEX idx_job_type_status (job_type, status, available_at)
            )"#,
            family.jobs_table
        ))
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn create_tags_table(&self, family: &MemoryV2TableFamily) -> Result<(), MemoriaError> {
        sqlx::query(&format!(
            r#"CREATE TABLE IF NOT EXISTS {} (
                memory_id VARCHAR(64) NOT NULL,
                tag VARCHAR(100) NOT NULL,
                created_at DATETIME(6) NOT NULL,
                PRIMARY KEY (memory_id, tag),
                INDEX idx_tag (tag)
            )"#,
            family.tags_table
        ))
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn create_stats_table(&self, family: &MemoryV2TableFamily) -> Result<(), MemoriaError> {
        sqlx::query(&format!(
            r#"CREATE TABLE IF NOT EXISTS {} (
                memory_id VARCHAR(64) PRIMARY KEY,
                access_count INT NOT NULL DEFAULT 0,
                last_accessed_at DATETIME(6),
                feedback_useful INT NOT NULL DEFAULT 0,
                feedback_irrelevant INT NOT NULL DEFAULT 0,
                feedback_outdated INT NOT NULL DEFAULT 0,
                feedback_wrong INT NOT NULL DEFAULT 0,
                last_feedback_at DATETIME(6)
            )"#,
            family.stats_table
        ))
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        if should_attempt_schema_patch(&family.stats_table) {
            let _ = sqlx::query(&format!(
                "ALTER TABLE {} ADD COLUMN feedback_useful INT NOT NULL DEFAULT 0",
                family.stats_table
            ))
            .execute(&self.pool)
            .await;
            let _ = sqlx::query(&format!(
                "ALTER TABLE {} ADD COLUMN feedback_irrelevant INT NOT NULL DEFAULT 0",
                family.stats_table
            ))
            .execute(&self.pool)
            .await;
            let _ = sqlx::query(&format!(
                "ALTER TABLE {} ADD COLUMN feedback_outdated INT NOT NULL DEFAULT 0",
                family.stats_table
            ))
            .execute(&self.pool)
            .await;
            let _ = sqlx::query(&format!(
                "ALTER TABLE {} ADD COLUMN feedback_wrong INT NOT NULL DEFAULT 0",
                family.stats_table
            ))
            .execute(&self.pool)
            .await;
            let _ = sqlx::query(&format!(
                "ALTER TABLE {} ADD COLUMN last_feedback_at DATETIME(6)",
                family.stats_table
            ))
            .execute(&self.pool)
            .await;
        }
        Ok(())
    }

    async fn create_feedback_table(
        &self,
        family: &MemoryV2TableFamily,
    ) -> Result<(), MemoriaError> {
        sqlx::query(&format!(
            r#"CREATE TABLE IF NOT EXISTS {} (
                feedback_id VARCHAR(64) PRIMARY KEY,
                memory_id VARCHAR(64) NOT NULL,
                signal VARCHAR(20) NOT NULL,
                context TEXT,
                created_at DATETIME(6) NOT NULL,
                INDEX idx_memory_created (memory_id, created_at),
                INDEX idx_signal_created (signal, created_at)
            )"#,
            family.feedback_table
        ))
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn claim_next_job_for_user(
        &self,
        user_id: &str,
    ) -> Result<Option<ClaimedV2Job>, MemoriaError> {
        let family = self.ensure_user_tables(user_id).await?;
        let now = Utc::now().naive_utc();
        let candidates = sqlx::query(&format!(
            "SELECT job_id, job_type, aggregate_id, payload_json, attempts \
             FROM {} \
             WHERE ((status = 'pending' AND available_at <= ?) \
                 OR (status = 'leased' AND leased_until IS NOT NULL AND leased_until <= ?)) \
             ORDER BY available_at ASC, created_at ASC \
             LIMIT 8",
            family.jobs_table
        ))
        .bind(now)
        .bind(now)
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;

        for row in candidates {
            let job_id: String = row.try_get("job_id").map_err(db_err)?;
            let attempts_before: i32 = row.try_get("attempts").unwrap_or(0);
            let lease_until = Utc::now() + chrono::Duration::seconds(JOB_LEASE_SECS.max(1));
            let update = sqlx::query(&format!(
                "UPDATE {} \
                 SET status = 'leased', leased_until = ?, attempts = attempts + 1, updated_at = ? \
                 WHERE job_id = ? \
                   AND ((status = 'pending' AND available_at <= ?) \
                     OR (status = 'leased' AND leased_until IS NOT NULL AND leased_until <= ?))",
                family.jobs_table
            ))
            .bind(lease_until.naive_utc())
            .bind(now)
            .bind(&job_id)
            .bind(now)
            .bind(now)
            .execute(&self.pool)
            .await
            .map_err(db_err)?;
            if update.rows_affected() == 0 {
                continue;
            }

            let payload: serde_json::Value = row.try_get("payload_json").map_err(db_err)?;
            let aggregate_id: String = row.try_get("aggregate_id").map_err(db_err)?;
            let memory_id = payload["memory_id"]
                .as_str()
                .or_else(|| payload["aggregate_id"].as_str())
                .unwrap_or(aggregate_id.as_str())
                .to_string();
            let content_version_id = payload["content_version_id"]
                .as_str()
                .unwrap_or_default()
                .to_string();

            return Ok(Some(ClaimedV2Job {
                family: family.clone(),
                job_id,
                job_type: row.try_get("job_type").map_err(db_err)?,
                memory_id,
                content_version_id,
                attempts: attempts_before + 1,
            }));
        }
        Ok(None)
    }

    async fn process_claimed_job(
        &self,
        job: &ClaimedV2Job,
        enricher: Option<&dyn MemoryV2JobEnricher>,
    ) -> Result<(), MemoriaError> {
        match job.job_type.as_str() {
            "derive_views" => self.process_derive_views(job, enricher).await,
            "extract_links" => self.process_extract_links(job, enricher).await,
            "extract_entities" => self.process_extract_entities(job, enricher).await,
            other => Err(MemoriaError::Validation(format!(
                "unsupported Memory V2 job type: {other}"
            ))),
        }
    }

    async fn process_derive_views(
        &self,
        job: &ClaimedV2Job,
        enricher: Option<&dyn MemoryV2JobEnricher>,
    ) -> Result<(), MemoriaError> {
        if job.content_version_id.is_empty() || job.memory_id.is_empty() {
            return Err(MemoriaError::Validation(
                "derive_views job missing content_version_id or memory_id".into(),
            ));
        }
        let row = sqlx::query(&format!(
            "SELECT source_text, abstract_text \
             FROM {} \
             WHERE content_version_id = ? AND memory_id = ? \
             LIMIT 1",
            job.family.content_versions_table
        ))
        .bind(&job.content_version_id)
        .bind(&job.memory_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?;

        let Some(row) = row else {
            return Ok(());
        };
        let source_text: String = row.try_get("source_text").map_err(db_err)?;
        let abstract_text: String = row.try_get("abstract_text").map_err(db_err)?;
        let mut overview_text = derive_overview_text(&source_text, &abstract_text);
        let mut detail_text = derive_detail_text(&source_text, &abstract_text);
        if let Some(enricher) = enricher {
            if let Some(enriched) = enricher.derive_views(&source_text, &abstract_text).await? {
                if !enriched.overview_text.trim().is_empty() {
                    overview_text =
                        truncate_utf8(enriched.overview_text.trim(), MAX_DERIVED_OVERVIEW_BYTES)
                            .trim()
                            .to_string();
                }
                if !enriched.detail_text.trim().is_empty() {
                    detail_text = truncate_utf8(enriched.detail_text.trim(), MAX_JOB_DETAIL_BYTES)
                        .trim()
                        .to_string();
                }
            }
        }

        sqlx::query(&format!(
            "UPDATE {} \
             SET overview_text = ?, detail_text = ?, has_overview = ?, has_detail = ?, \
                 overview_token_estimate = ?, detail_token_estimate = ?, derivation_state = 'complete' \
             WHERE content_version_id = ?",
            job.family.content_versions_table
        ))
        .bind(if overview_text.is_empty() {
            None::<String>
        } else {
            Some(overview_text.clone())
        })
        .bind(if detail_text.is_empty() {
            None::<String>
        } else {
            Some(detail_text.clone())
        })
        .bind((!overview_text.is_empty()) as i8)
        .bind((!detail_text.is_empty()) as i8)
        .bind(estimate_tokens(&overview_text))
        .bind(estimate_tokens(&detail_text))
        .bind(&job.content_version_id)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn process_extract_links(
        &self,
        job: &ClaimedV2Job,
        enricher: Option<&dyn MemoryV2JobEnricher>,
    ) -> Result<(), MemoriaError> {
        if job.memory_id.is_empty() {
            return Err(MemoriaError::Validation(
                "extract_links job missing memory_id".into(),
            ));
        }

        let source_abstract: String = sqlx::query(&format!(
            "SELECT c.abstract_text \
             FROM {} h \
             JOIN {} c ON c.content_version_id = h.current_content_version_id \
             WHERE h.memory_id = ? AND h.forgotten_at IS NULL \
             LIMIT 1",
            job.family.heads_table, job.family.content_versions_table
        ))
        .bind(&job.memory_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?
        .and_then(|row| row.try_get("abstract_text").ok())
        .unwrap_or_default();

        let mut links: HashMap<String, LinkV2Ref> = HashMap::new();

        let source_tag_count = sqlx::query(&format!(
            "SELECT CAST(COUNT(*) AS SIGNED) AS cnt FROM {} WHERE memory_id = ?",
            job.family.tags_table
        ))
        .bind(&job.memory_id)
        .fetch_one(&self.pool)
        .await
        .map_err(db_err)?
        .try_get::<i64, _>("cnt")
        .map_err(db_err)?
        .max(0) as f64;

        if source_tag_count > 0.0 {
            let rows = sqlx::query(&format!(
                "SELECT t2.memory_id AS target_memory_id, tc.abstract_text, \
                        CAST(COUNT(*) AS SIGNED) AS overlap \
                 FROM {} t1 \
                 JOIN {} t2 ON t1.tag = t2.tag AND t2.memory_id <> ? \
                 JOIN {} h ON h.memory_id = t2.memory_id \
                 JOIN {} tc ON tc.content_version_id = h.current_content_version_id \
                 WHERE t1.memory_id = ? AND h.forgotten_at IS NULL \
                 GROUP BY t2.memory_id, tc.abstract_text \
                 ORDER BY overlap DESC, t2.memory_id ASC \
                 LIMIT {}",
                job.family.tags_table,
                job.family.tags_table,
                job.family.heads_table,
                job.family.content_versions_table,
                MAX_LINKS_PER_MEMORY
            ))
            .bind(&job.memory_id)
            .bind(&job.memory_id)
            .fetch_all(&self.pool)
            .await
            .map_err(db_err)?;

            for row in rows {
                let target_memory_id: String = row.try_get("target_memory_id").map_err(db_err)?;
                let overlap = row.try_get::<i64, _>("overlap").map_err(db_err)?.max(0) as f64;
                let strength = (overlap / source_tag_count).clamp(0.0, 1.0);
                if strength <= 0.0 {
                    continue;
                }
                links.insert(
                    target_memory_id.clone(),
                    LinkV2Ref {
                        memory_id: target_memory_id,
                        abstract_text: row.try_get("abstract_text").map_err(db_err)?,
                        link_type: "tag_overlap".to_string(),
                        strength,
                        provenance: MemoryV2LinkProvenance::default(),
                    },
                );
            }
        }

        let vector_sql = format!(
            "SELECT h.memory_id AS target_memory_id, tc.abstract_text, \
                    l2_distance(d.embedding, sd.embedding) AS vec_dist \
             FROM {} sh \
             JOIN {} sd ON sd.index_doc_id = sh.current_index_doc_id \
             JOIN {} h ON h.memory_id <> sh.memory_id \
             JOIN {} d ON d.index_doc_id = h.current_index_doc_id \
             JOIN {} tc ON tc.content_version_id = h.current_content_version_id \
             WHERE sh.memory_id = ? AND h.forgotten_at IS NULL \
               AND sd.embedding IS NOT NULL AND d.embedding IS NOT NULL \
             ORDER BY l2_distance(d.embedding, sd.embedding) ASC \
             LIMIT {} by rank with option 'mode=post'",
            job.family.heads_table,
            job.family.index_docs_table,
            job.family.heads_table,
            job.family.index_docs_table,
            job.family.content_versions_table,
            MAX_LINKS_PER_MEMORY
        );
        let vector_rows = sqlx::query(&vector_sql)
            .bind(&job.memory_id)
            .fetch_all(&self.pool)
            .await;
        if let Ok(rows) = vector_rows {
            for row in rows {
                let target_memory_id: String = row.try_get("target_memory_id").map_err(db_err)?;
                let dist = row.try_get::<f64, _>("vec_dist").unwrap_or(1.0);
                let strength = (1.0 / (1.0 + dist.max(0.0))).clamp(0.0, 1.0);
                if strength < 0.55 {
                    continue;
                }
                let candidate = LinkV2Ref {
                    memory_id: target_memory_id.clone(),
                    abstract_text: row.try_get("abstract_text").map_err(db_err)?,
                    link_type: "semantic_related".to_string(),
                    strength,
                    provenance: MemoryV2LinkProvenance::default(),
                };
                links
                    .entry(target_memory_id)
                    .and_modify(|existing| {
                        if candidate.strength > existing.strength {
                            *existing = candidate.clone();
                        }
                    })
                    .or_insert(candidate);
            }
        }

        // entity_related: memories sharing named entities with the source memory.
        let entity_rows = sqlx::query(&format!(
            "SELECT m2.memory_id AS target_memory_id, cv.abstract_text, \
                    CAST(COUNT(*) AS SIGNED) AS shared_entities \
             FROM {ment} m1 \
             JOIN {ment} m2 ON m1.entity_id = m2.entity_id AND m2.memory_id <> ? \
             JOIN {heads} h ON h.memory_id = m2.memory_id AND h.forgotten_at IS NULL \
             JOIN {cvs} cv ON cv.content_version_id = h.current_content_version_id \
             WHERE m1.memory_id = ? \
             GROUP BY m2.memory_id, cv.abstract_text \
             ORDER BY shared_entities DESC, m2.memory_id ASC \
             LIMIT {limit}",
            ment = job.family.memory_entities_table,
            heads = job.family.heads_table,
            cvs = job.family.content_versions_table,
            limit = MAX_LINKS_PER_MEMORY
        ))
        .bind(&job.memory_id)
        .bind(&job.memory_id)
        .fetch_all(&self.pool)
        .await;
        if let Ok(rows) = entity_rows {
            for row in rows {
                let target_memory_id: String = row.try_get("target_memory_id").map_err(db_err)?;
                let shared = row.try_get::<i64, _>("shared_entities").unwrap_or(1).max(1) as f64;
                // 1 shared entity → ~0.5; grows logarithmically, capped at 0.9
                let strength = (0.5 * (1.0 + shared.ln())).clamp(0.3, 0.9);
                let candidate = LinkV2Ref {
                    memory_id: target_memory_id.clone(),
                    abstract_text: row.try_get("abstract_text").map_err(db_err)?,
                    link_type: "entity_related".to_string(),
                    strength,
                    provenance: MemoryV2LinkProvenance::default(),
                };
                links
                    .entry(target_memory_id)
                    .and_modify(|existing| {
                        if candidate.strength > existing.strength {
                            *existing = candidate.clone();
                        }
                    })
                    .or_insert(candidate);
            }
        }

        let mut selected: Vec<LinkV2Ref> = links.into_values().collect();
        selected.sort_by(|a, b| {
            b.strength
                .partial_cmp(&a.strength)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.memory_id.cmp(&b.memory_id))
        });
        if selected.len() > MAX_LINKS_PER_MEMORY {
            selected.truncate(MAX_LINKS_PER_MEMORY);
        }

        if let Some(enricher) = enricher {
            if !source_abstract.trim().is_empty() && !selected.is_empty() {
                let candidate_map: HashMap<String, LinkV2Ref> = selected
                    .iter()
                    .cloned()
                    .map(|link| (link.memory_id.clone(), link))
                    .collect();
                let candidates: Vec<V2LinkCandidate> = selected
                    .iter()
                    .map(|link| V2LinkCandidate {
                        target_memory_id: link.memory_id.clone(),
                        abstract_text: link.abstract_text.clone(),
                        link_type: link.link_type.clone(),
                        strength: link.strength,
                    })
                    .collect();
                if let Some(suggestions) =
                    enricher.refine_links(&source_abstract, &candidates).await?
                {
                    let mut refined_by_memory: HashMap<String, LinkV2Ref> = HashMap::new();
                    let mut seen = HashSet::new();
                    for suggestion in suggestions {
                        if !seen.insert(suggestion.target_memory_id.clone()) {
                            continue;
                        }
                        let Some(base) = candidate_map.get(&suggestion.target_memory_id) else {
                            continue;
                        };
                        refined_by_memory.insert(
                            suggestion.target_memory_id.clone(),
                            LinkV2Ref {
                                memory_id: suggestion.target_memory_id,
                                abstract_text: base.abstract_text.clone(),
                                link_type: if suggestion.link_type.trim().is_empty() {
                                    base.link_type.clone()
                                } else {
                                    truncate_utf8(suggestion.link_type.trim(), 32)
                                        .trim()
                                        .to_string()
                                },
                                strength: suggestion.strength.clamp(0.0, 1.0),
                                provenance: MemoryV2LinkProvenance::default(),
                            },
                        );
                    }
                    if !refined_by_memory.is_empty() {
                        let mut refined = selected
                            .iter()
                            .map(|base| {
                                refined_by_memory
                                    .remove(&base.memory_id)
                                    .unwrap_or_else(|| base.clone())
                            })
                            .collect::<Vec<_>>();
                        refined.sort_by(|a, b| {
                            b.strength
                                .partial_cmp(&a.strength)
                                .unwrap_or(std::cmp::Ordering::Equal)
                                .then_with(|| a.memory_id.cmp(&b.memory_id))
                        });
                        if refined.len() > MAX_LINKS_PER_MEMORY {
                            refined.truncate(MAX_LINKS_PER_MEMORY);
                        }
                        selected = refined;
                    }
                }
            }
        }

        let now = Utc::now().naive_utc();
        let mut tx = self.pool.begin().await.map_err(db_err)?;
        sqlx::query(&format!(
            "DELETE FROM {} WHERE memory_id = ?",
            job.family.links_table
        ))
        .bind(&job.memory_id)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
        if !selected.is_empty() {
            let placeholders = selected
                .iter()
                .map(|_| "(?, ?, ?, ?, ?, ?)")
                .collect::<Vec<_>>()
                .join(", ");
            let sql = format!(
                "INSERT INTO {} (link_id, memory_id, target_memory_id, link_type, strength, created_at) VALUES {}",
                job.family.links_table, placeholders
            );
            let mut q = sqlx::query(&sql);
            for link in &selected {
                q = q
                    .bind(uuid7_id())
                    .bind(&job.memory_id)
                    .bind(&link.memory_id)
                    .bind(&link.link_type)
                    .bind(link.strength as f32)
                    .bind(now);
            }
            q.execute(&mut *tx).await.map_err(db_err)?;
        }
        tx.commit().await.map_err(db_err)?;
        Ok(())
    }

    async fn process_extract_entities(
        &self,
        job: &ClaimedV2Job,
        enricher: Option<&dyn MemoryV2JobEnricher>,
    ) -> Result<(), MemoriaError> {
        if job.content_version_id.is_empty() || job.memory_id.is_empty() {
            return Err(MemoriaError::Validation(
                "extract_entities job missing content_version_id or memory_id".into(),
            ));
        }
        let row = sqlx::query(&format!(
            "SELECT h.current_content_version_id, c.source_text \
             FROM {} h \
             JOIN {} c ON c.content_version_id = h.current_content_version_id \
             WHERE h.memory_id = ? AND h.forgotten_at IS NULL \
             LIMIT 1",
            job.family.heads_table, job.family.content_versions_table
        ))
        .bind(&job.memory_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?;
        let Some(row) = row else {
            return Ok(());
        };
        let current_content_version_id: String =
            row.try_get("current_content_version_id").map_err(db_err)?;
        if current_content_version_id != job.content_version_id {
            return Ok(());
        }
        let source_text: String = row.try_get("source_text").map_err(db_err)?;
        let now = Utc::now().naive_utc();

        // Run regex NER baseline.
        let regex_entities = crate::graph::ner::extract_entities(&source_text);

        // Optionally refine via LLM enricher.
        let entity_triples: Vec<(String, String, String)> = if let Some(enricher) = enricher {
            let candidates: Vec<V2EntityCandidate> = regex_entities
                .iter()
                .map(|e| V2EntityCandidate {
                    name: e.name.clone(),
                    display: e.display.clone(),
                    entity_type: e.entity_type.clone(),
                })
                .collect();
            if let Some(refined) = enricher.refine_entities(&source_text, &candidates).await? {
                refined
                    .into_iter()
                    .map(|s| (s.name, s.display, s.entity_type))
                    .collect()
            } else {
                regex_entities
                    .into_iter()
                    .map(|e| (e.name, e.display, e.entity_type))
                    .collect()
            }
        } else {
            regex_entities
                .into_iter()
                .map(|e| (e.name, e.display, e.entity_type))
                .collect()
        };

        let mut tx = self.pool.begin().await.map_err(db_err)?;
        self.write_entities_for_memory_tx(
            &mut tx,
            &job.family,
            &job.memory_id,
            &job.content_version_id,
            &entity_triples,
            now,
        )
        .await?;
        tx.commit().await.map_err(db_err)?;
        Ok(())
    }

    async fn mark_job_done(&self, job: &ClaimedV2Job) -> Result<(), MemoriaError> {
        sqlx::query(&format!(
            "UPDATE {} SET status = 'done', leased_until = NULL, updated_at = ? WHERE job_id = ?",
            job.family.jobs_table
        ))
        .bind(Utc::now().naive_utc())
        .bind(&job.job_id)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn mark_job_failed(&self, job: &ClaimedV2Job, error: &str) -> Result<(), MemoriaError> {
        let now = Utc::now();
        let backoff_secs = (15 * job.attempts.max(1) as i64).min(300);
        let next_status = if job.attempts >= MAX_JOB_ATTEMPTS {
            "failed"
        } else {
            "pending"
        };
        let available_at = now + chrono::Duration::seconds(backoff_secs);
        sqlx::query(&format!(
            "UPDATE {} \
             SET status = ?, leased_until = NULL, available_at = ?, last_error = ?, updated_at = ? \
             WHERE job_id = ?",
            job.family.jobs_table
        ))
        .bind(next_status)
        .bind(available_at.naive_utc())
        .bind(truncate_utf8(error, 2000))
        .bind(now.naive_utc())
        .bind(&job.job_id)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn search_fulltext_candidates(
        &self,
        family: &MemoryV2TableFamily,
        req: &RecallV2Request,
        limit: i64,
    ) -> Result<Vec<(RecallCandidate, f64)>, MemoriaError> {
        let safe = sanitize_fulltext_query(&req.query);
        if safe.is_empty() {
            return Ok(vec![]);
        }
        let mut clauses = vec![
            "h.forgotten_at IS NULL".to_string(),
            format!("MATCH(d.recall_text) AGAINST('{safe}' IN BOOLEAN MODE)"),
        ];
        if let Some(mt) = req.memory_type.as_ref() {
            clauses.push(format!(
                "h.memory_type = '{}'",
                sanitize_sql_literal(&mt.to_string())
            ));
        }
        if req.session_only {
            if let Some(ref sid) = req.session_id {
                clauses.push(format!("h.session_id = '{}'", sanitize_sql_literal(sid)));
            }
        }
        if let Some(tag_clause) = build_recall_tag_clause(family, &req.tags, &req.tag_filter_mode) {
            clauses.push(tag_clause);
        }
        clauses.extend(build_recall_time_clauses(
            req.created_after,
            req.created_before,
        ));
        let sql = format!(
            "SELECT h.memory_id, h.memory_type, h.session_id, h.confidence, h.importance, h.created_at, \
             c.abstract_text, c.overview_text, c.has_overview, c.has_detail, c.abstract_token_estimate, c.overview_token_estimate, \
             MATCH(d.recall_text) AGAINST('{safe}' IN BOOLEAN MODE) AS ft_score \
             FROM {} h \
             JOIN {} d ON d.index_doc_id = h.current_index_doc_id \
             JOIN {} c ON c.content_version_id = h.current_content_version_id \
             WHERE {} \
             ORDER BY ft_score DESC, h.created_at DESC LIMIT {}",
            family.heads_table,
            family.index_docs_table,
            family.content_versions_table,
            clauses.join(" AND "),
            limit
        );
        let rows = match sqlx::query(&sql).fetch_all(&self.pool).await {
            Ok(rows) => rows,
            Err(e) => {
                let msg = e.to_string();
                if msg.contains("20101") && msg.contains("empty pattern") {
                    return Ok(vec![]);
                }
                if msg.contains("MATCH() AGAINST()")
                    || msg.contains("fulltext search is not supported")
                    || msg.contains("FULLTEXT INDEX")
                {
                    return Ok(vec![]);
                }
                return Err(db_err(e));
            }
        };
        rows.into_iter()
            .map(|r| {
                let score = r.try_get::<f64, _>("ft_score").unwrap_or(0.0);
                Ok((
                    self.row_to_candidate(r)?,
                    if score > 0.0 {
                        score / (score + 1.0)
                    } else {
                        0.0
                    },
                ))
            })
            .collect()
    }

    async fn search_vector_candidates(
        &self,
        family: &MemoryV2TableFamily,
        req: &RecallV2Request,
        embedding: &[f32],
        limit: i64,
    ) -> Result<Vec<(RecallCandidate, f64)>, MemoriaError> {
        if embedding.is_empty() {
            return Ok(vec![]);
        }
        let vec_literal = vec_to_mo(embedding);
        let mut clauses = vec![
            "h.forgotten_at IS NULL".to_string(),
            "d.embedding IS NOT NULL".to_string(),
        ];
        if let Some(mt) = req.memory_type.as_ref() {
            clauses.push(format!(
                "h.memory_type = '{}'",
                sanitize_sql_literal(&mt.to_string())
            ));
        }
        if req.session_only {
            if let Some(ref sid) = req.session_id {
                clauses.push(format!("h.session_id = '{}'", sanitize_sql_literal(sid)));
            }
        }
        if let Some(tag_clause) = build_recall_tag_clause(family, &req.tags, &req.tag_filter_mode) {
            clauses.push(tag_clause);
        }
        clauses.extend(build_recall_time_clauses(
            req.created_after,
            req.created_before,
        ));
        let sql = format!(
            "SELECT h.memory_id, h.memory_type, h.session_id, h.confidence, h.importance, h.created_at, \
             c.abstract_text, c.overview_text, c.has_overview, c.has_detail, c.abstract_token_estimate, c.overview_token_estimate, \
             l2_distance(d.embedding, '{vec_literal}') AS vec_dist \
             FROM {} h \
             JOIN {} d ON d.index_doc_id = h.current_index_doc_id \
             JOIN {} c ON c.content_version_id = h.current_content_version_id \
             WHERE {} \
             ORDER BY l2_distance(d.embedding, '{vec_literal}') ASC \
             LIMIT {} by rank with option 'mode=post'",
            family.heads_table,
            family.index_docs_table,
            family.content_versions_table,
            clauses.join(" AND "),
            limit
        );
        let rows = sqlx::query(&sql)
            .fetch_all(&self.pool)
            .await
            .map_err(db_err)?;
        rows.into_iter()
            .map(|r| {
                let dist = r.try_get::<f64, _>("vec_dist").unwrap_or(1.0);
                Ok((self.row_to_candidate(r)?, 1.0 / (1.0 + dist.max(0.0))))
            })
            .collect()
    }

    async fn search_entity_candidates(
        &self,
        family: &MemoryV2TableFamily,
        req: &RecallV2Request,
        limit: i64,
    ) -> Result<Vec<(RecallCandidate, f64)>, MemoriaError> {
        let query_entities = crate::graph::ner::extract_entities(&req.query)
            .into_iter()
            .filter(|entity| entity.entity_type != "person" && entity.entity_type != "time")
            .fold(Vec::<(String, String)>::new(), |mut acc, entity| {
                if !acc
                    .iter()
                    .any(|(name, kind)| name == &entity.name && kind == &entity.entity_type)
                {
                    acc.push((entity.name, entity.entity_type));
                }
                acc
            });
        if query_entities.is_empty() {
            return Ok(vec![]);
        }
        let mut clauses = vec!["h.forgotten_at IS NULL".to_string()];
        if let Some(mt) = req.memory_type.as_ref() {
            clauses.push(format!(
                "h.memory_type = '{}'",
                sanitize_sql_literal(&mt.to_string())
            ));
        }
        if req.session_only {
            if let Some(ref sid) = req.session_id {
                clauses.push(format!("h.session_id = '{}'", sanitize_sql_literal(sid)));
            }
        }
        if let Some(tag_clause) = build_recall_tag_clause(family, &req.tags, &req.tag_filter_mode) {
            clauses.push(tag_clause);
        }
        clauses.extend(build_recall_time_clauses(
            req.created_after,
            req.created_before,
        ));
        let entity_clause = query_entities
            .iter()
            .map(|_| "(e.name = ? AND e.entity_type = ?)")
            .collect::<Vec<_>>()
            .join(" OR ");
        let sql = format!(
            "SELECT h.memory_id, h.memory_type, h.session_id, h.confidence, h.importance, h.created_at, \
             c.abstract_text, c.overview_text, c.has_overview, c.has_detail, c.abstract_token_estimate, c.overview_token_estimate, \
             CAST(COUNT(DISTINCT me.entity_id) AS SIGNED) AS entity_matches \
             FROM {} h \
             JOIN {} c ON c.content_version_id = h.current_content_version_id \
             JOIN {} me ON me.memory_id = h.memory_id AND me.content_version_id = h.current_content_version_id \
             JOIN {} e ON e.entity_id = me.entity_id \
             WHERE {} AND ({}) \
             GROUP BY h.memory_id, h.memory_type, h.session_id, h.confidence, h.importance, h.created_at, \
                      c.abstract_text, c.overview_text, c.has_overview, c.has_detail, c.abstract_token_estimate, c.overview_token_estimate \
             ORDER BY entity_matches DESC, h.created_at DESC LIMIT {}",
            family.heads_table,
            family.content_versions_table,
            family.memory_entities_table,
            family.entities_table,
            clauses.join(" AND "),
            entity_clause,
            limit
        );
        let mut query = sqlx::query(&sql);
        for (name, entity_type) in &query_entities {
            query = query.bind(name).bind(entity_type);
        }
        let query_entity_count = query_entities.len().max(1) as f64;
        query
            .fetch_all(&self.pool)
            .await
            .map_err(db_err)?
            .into_iter()
            .map(|row| {
                let entity_matches = row.try_get::<i64, _>("entity_matches").unwrap_or(0).max(0);
                Ok((
                    self.row_to_candidate(row)?,
                    ((entity_matches as f64) / query_entity_count).clamp(0.0, 1.0),
                ))
            })
            .collect()
    }

    fn row_to_candidate(&self, r: sqlx::mysql::MySqlRow) -> Result<RecallCandidate, MemoriaError> {
        Ok(RecallCandidate {
            memory_id: r.try_get("memory_id").map_err(db_err)?,
            abstract_text: r.try_get("abstract_text").map_err(db_err)?,
            overview_text: r.try_get("overview_text").ok(),
            memory_type: MemoryType::from_str(
                &r.try_get::<String, _>("memory_type").map_err(db_err)?,
            )
            .map_err(|e| MemoriaError::Validation(e.to_string()))?,
            session_id: r.try_get("session_id").ok(),
            created_at: to_utc(r.try_get("created_at").map_err(db_err)?),
            confidence: r.try_get::<f32, _>("confidence").unwrap_or(0.0) as f64,
            importance: r.try_get::<f32, _>("importance").unwrap_or(0.0) as f64,
            has_overview: r.try_get::<i8, _>("has_overview").unwrap_or(0) != 0,
            has_detail: r.try_get::<i8, _>("has_detail").unwrap_or(0) != 0,
            abstract_tokens: r.try_get("abstract_token_estimate").unwrap_or(0),
            overview_tokens: r.try_get("overview_token_estimate").unwrap_or(0),
            direct_match: false,
            vector_score: 0.0,
            keyword_score: 0.0,
            entity_score: 0.0,
            link_bonus: 0.0,
            expansion_sources: Vec::new(),
            access_count: 0,
            link_count: 0,
            session_boost: 1.0,
            focus_boost: 1.0,
            type_affinity_boost: 1.0,
            focus_matches: Vec::new(),
            feedback: MemoryFeedback::default(),
        })
    }

    fn recall_session_multiplier(
        request_session_id: Option<&str>,
        candidate_session_id: Option<&str>,
    ) -> f64 {
        match (
            request_session_id
                .map(str::trim)
                .filter(|session_id| !session_id.is_empty()),
            candidate_session_id
                .map(str::trim)
                .filter(|session_id| !session_id.is_empty()),
        ) {
            (Some(request_session_id), Some(candidate_session_id))
                if request_session_id == candidate_session_id =>
            {
                1.12
            }
            _ => 1.0,
        }
    }

    fn feedback_multiplier(&self, feedback: &MemoryFeedback) -> f64 {
        let positive = feedback.useful as f64;
        let negative = (feedback.irrelevant + feedback.outdated + feedback.wrong) as f64;
        let feedback_delta = positive - 0.5 * negative;
        if feedback_delta.abs() > 0.01 {
            (1.0 + DEFAULT_V2_FEEDBACK_WEIGHT * feedback_delta).clamp(0.5, 2.0)
        } else {
            1.0
        }
    }

    fn feedback_impact(&self, feedback: MemoryFeedback) -> MemoryV2FeedbackImpact {
        MemoryV2FeedbackImpact {
            multiplier: self.feedback_multiplier(&feedback),
            counts: feedback,
        }
    }

    fn recall_vector_component(&self, candidate: &RecallCandidate) -> f64 {
        0.55 * candidate.vector_score
    }

    fn recall_keyword_component(&self, candidate: &RecallCandidate) -> f64 {
        0.22 * candidate.keyword_score
    }

    fn recall_confidence_component(&self, candidate: &RecallCandidate) -> f64 {
        0.1 * candidate.confidence
    }

    fn recall_importance_component(&self, candidate: &RecallCandidate) -> f64 {
        0.05 * candidate.importance
    }

    fn recall_entity_component(&self, candidate: &RecallCandidate) -> f64 {
        DEFAULT_V2_ENTITY_WEIGHT * candidate.entity_score
    }

    fn recall_temporal_half_life_hours(&self, memory_type: &MemoryType) -> f64 {
        match memory_type {
            MemoryType::Working => 48.0,
            MemoryType::Episodic => 168.0,
            MemoryType::Semantic => 2160.0,
            MemoryType::Procedural => 4320.0,
            MemoryType::Profile => 8760.0,
            MemoryType::ToolResult => 720.0,
        }
    }

    fn recall_age_hours(&self, created_at: DateTime<Utc>) -> f64 {
        (Utc::now() - created_at).num_seconds().max(0) as f64 / 3600.0
    }

    fn recall_temporal_multiplier(&self, candidate: &RecallCandidate) -> f64 {
        let age_hours = self.recall_age_hours(candidate.created_at);
        let half_life_hours = self.recall_temporal_half_life_hours(&candidate.memory_type);
        (-age_hours * 2.0_f64.ln() / half_life_hours).exp()
    }

    fn recall_base_score(&self, candidate: &RecallCandidate) -> f64 {
        self.recall_vector_component(candidate)
            + self.recall_keyword_component(candidate)
            + self.recall_confidence_component(candidate)
            + self.recall_importance_component(candidate)
            + self.recall_entity_component(candidate)
            + candidate.link_bonus.max(0.0)
    }

    fn recall_access_multiplier(&self, access_count: i32) -> f64 {
        if access_count > 0 {
            1.0 + 0.1 * ((1 + access_count) as f64).ln()
        } else {
            1.0
        }
    }

    fn recall_ranking(&self, candidate: &RecallCandidate) -> MemoryV2RecallRanking {
        let base_score = self.recall_base_score(candidate);
        let age_hours = self.recall_age_hours(candidate.created_at);
        let temporal_half_life_hours = self.recall_temporal_half_life_hours(&candidate.memory_type);
        let temporal_multiplier = self.recall_temporal_multiplier(candidate);
        let access_multiplier = self.recall_access_multiplier(candidate.access_count);
        let session_affinity_multiplier = candidate.session_boost.max(1.0);
        let feedback_multiplier = self.feedback_multiplier(&candidate.feedback);
        let focus_boost = candidate.focus_boost.max(1.0);
        let type_affinity_boost = candidate.type_affinity_boost.max(1.0);
        let final_score = base_score
            * temporal_multiplier
            * access_multiplier
            * session_affinity_multiplier
            * feedback_multiplier
            * focus_boost
            * type_affinity_boost;
        MemoryV2RecallRanking {
            final_score,
            base_score,
            vector_component: self.recall_vector_component(candidate),
            keyword_component: self.recall_keyword_component(candidate),
            confidence_component: self.recall_confidence_component(candidate),
            importance_component: self.recall_importance_component(candidate),
            entity_component: self.recall_entity_component(candidate),
            link_bonus: candidate.link_bonus.max(0.0),
            linked_expansion_applied: candidate.link_bonus > 0.0,
            temporal_decay_applied: temporal_multiplier < 0.999,
            age_hours,
            temporal_half_life_hours,
            temporal_multiplier,
            session_affinity_applied: session_affinity_multiplier > 1.0,
            session_affinity_multiplier,
            access_count: candidate.access_count,
            access_multiplier,
            feedback_multiplier,
            focus_boost,
            type_affinity_boost,
            focus_matches: candidate.focus_matches.clone(),
            expansion_sources: candidate.expansion_sources.clone(),
        }
    }

    fn recall_path(&self, candidate: &RecallCandidate) -> MemoryV2RecallPath {
        match (
            candidate.direct_match,
            !candidate.expansion_sources.is_empty(),
        ) {
            (true, true) => MemoryV2RecallPath::DirectAndExpanded,
            (true, false) => MemoryV2RecallPath::Direct,
            (false, true) => MemoryV2RecallPath::ExpandedOnly,
            (false, false) => MemoryV2RecallPath::Direct,
        }
    }

    fn final_score(&self, candidate: &RecallCandidate) -> f64 {
        self.recall_ranking(candidate).final_score
    }

    /// Rule-based query intent detection (no LLM).
    /// Returns the memory type that the query most likely targets, if any.
    fn detect_query_intent(query: &str) -> Option<MemoryType> {
        let q = query.to_lowercase();
        if q.contains("how to")
            || q.contains("how do")
            || q.contains("how can")
            || q.contains("deploy")
            || q.contains("workflow")
            || q.contains("step")
            || q.contains("procedure")
            || q.contains("process")
        {
            Some(MemoryType::Procedural)
        } else if q.contains("last session")
            || q.contains("last time")
            || q.contains("yesterday")
            || q.contains("what happened")
            || q.contains("previously")
        {
            Some(MemoryType::Episodic)
        } else if q.contains("prefer")
            || q.contains("preference")
            || q.contains("style")
            || q.contains("setting")
            || q.contains("profile")
            || q.contains("config")
        {
            Some(MemoryType::Profile)
        } else {
            None
        }
    }

    async fn refresh_entities_for_memory_tx(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
        family: &MemoryV2TableFamily,
        memory_id: &str,
        content_version_id: &str,
        source_text: &str,
        now: NaiveDateTime,
    ) -> Result<(i64, i64), MemoriaError> {
        let entities = crate::graph::ner::extract_entities(source_text);
        let triples: Vec<(String, String, String)> = entities
            .into_iter()
            .map(|e| (e.name, e.display, e.entity_type))
            .collect();
        self.write_entities_for_memory_tx(tx, family, memory_id, content_version_id, &triples, now)
            .await
    }

    async fn write_entities_for_memory_tx(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
        family: &MemoryV2TableFamily,
        memory_id: &str,
        content_version_id: &str,
        // (name, display, entity_type)
        entity_triples: &[(String, String, String)],
        now: NaiveDateTime,
    ) -> Result<(i64, i64), MemoriaError> {
        let current_content_version_id = sqlx::query_scalar::<_, String>(&format!(
            "SELECT current_content_version_id FROM {} \
             WHERE memory_id = ? AND forgotten_at IS NULL \
             LIMIT 1 FOR UPDATE",
            family.heads_table
        ))
        .bind(memory_id)
        .fetch_optional(&mut **tx)
        .await
        .map_err(db_err)?;
        let Some(current_content_version_id) = current_content_version_id else {
            return Ok((0, 0));
        };
        if current_content_version_id != content_version_id {
            return Ok((0, 0));
        }

        sqlx::query(&format!(
            "DELETE FROM {} WHERE memory_id = ?",
            family.memory_entities_table
        ))
        .bind(memory_id)
        .execute(&mut **tx)
        .await
        .map_err(db_err)?;
        let mut links_written = 0i64;
        for (name, display, entity_type) in entity_triples {
            let entity_id = v2_entity_id(name, entity_type);
            sqlx::query(&format!(
                "INSERT INTO {} \
                 (entity_id, name, display_name, entity_type, created_at, updated_at) \
                 VALUES (?, ?, ?, ?, ?, ?) \
                 ON DUPLICATE KEY UPDATE display_name = VALUES(display_name), updated_at = VALUES(updated_at)",
                family.entities_table
            ))
            .bind(&entity_id)
            .bind(name)
            .bind(display)
            .bind(entity_type)
            .bind(now)
            .bind(now)
            .execute(&mut **tx)
            .await
            .map_err(db_err)?;

            sqlx::query(&format!(
                "INSERT INTO {} \
                 (memory_id, content_version_id, entity_id, source, weight, created_at) \
                 VALUES (?, ?, ?, 'regex', 0.8, ?)",
                family.memory_entities_table
            ))
            .bind(memory_id)
            .bind(content_version_id)
            .bind(&entity_id)
            .bind(now)
            .execute(&mut **tx)
            .await
            .map_err(db_err)?;
            links_written += 1;
        }
        Ok((entity_triples.len() as i64, links_written))
    }

    async fn fetch_feedback_batch(
        &self,
        family: &MemoryV2TableFamily,
        ids: &[String],
    ) -> Result<HashMap<String, MemoryFeedback>, MemoriaError> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!(
            "SELECT memory_id, feedback_useful, feedback_irrelevant, feedback_outdated, feedback_wrong \
             FROM {} WHERE memory_id IN ({})",
            family.stats_table, placeholders
        );
        let mut q = sqlx::query(&sql);
        for id in ids {
            q = q.bind(id);
        }
        let rows = q.fetch_all(&self.pool).await.map_err(db_err)?;
        let mut out = HashMap::new();
        for row in rows {
            out.insert(
                row.try_get("memory_id").map_err(db_err)?,
                MemoryFeedback {
                    useful: row.try_get("feedback_useful").unwrap_or(0),
                    irrelevant: row.try_get("feedback_irrelevant").unwrap_or(0),
                    outdated: row.try_get("feedback_outdated").unwrap_or(0),
                    wrong: row.try_get("feedback_wrong").unwrap_or(0),
                },
            );
        }
        Ok(out)
    }

    async fn fetch_access_counts(
        &self,
        family: &MemoryV2TableFamily,
        ids: &[String],
    ) -> Result<HashMap<String, i32>, MemoriaError> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!(
            "SELECT memory_id, access_count FROM {} WHERE memory_id IN ({})",
            family.stats_table, placeholders
        );
        let mut q = sqlx::query(&sql);
        for id in ids {
            q = q.bind(id);
        }
        let rows = q.fetch_all(&self.pool).await.map_err(db_err)?;
        let mut out = HashMap::new();
        for row in rows {
            out.insert(
                row.try_get("memory_id").map_err(db_err)?,
                row.try_get("access_count").unwrap_or(0),
            );
        }
        Ok(out)
    }

    async fn fetch_link_counts(
        &self,
        family: &MemoryV2TableFamily,
        ids: &[String],
    ) -> Result<HashMap<String, i64>, MemoriaError> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!(
            "SELECT memory_id, CAST(COUNT(*) AS SIGNED) AS cnt \
             FROM {} WHERE memory_id IN ({}) GROUP BY memory_id",
            family.links_table, placeholders
        );
        let mut q = sqlx::query(&sql);
        for id in ids {
            q = q.bind(id);
        }
        let rows = q.fetch_all(&self.pool).await.map_err(db_err)?;
        let mut out = HashMap::new();
        for row in rows {
            out.insert(
                row.try_get("memory_id").map_err(db_err)?,
                row.try_get::<i64, _>("cnt").map_err(db_err)?,
            );
        }
        Ok(out)
    }

    async fn populate_recall_candidate_metadata(
        &self,
        family: &MemoryV2TableFamily,
        req: &RecallV2Request,
        merged: &mut HashMap<String, RecallCandidate>,
    ) -> Result<(), MemoriaError> {
        if merged.is_empty() {
            return Ok(());
        }
        let ids: Vec<String> = merged.keys().cloned().collect();
        let access_counts = self.fetch_access_counts(family, &ids).await?;
        let feedback_by_memory = self.fetch_feedback_batch(family, &ids).await?;
        let link_counts = self.fetch_link_counts(family, &ids).await?;
        let focuses = self.load_active_focuses(family).await?;
        let tag_focus_values: Vec<String> = focuses
            .iter()
            .filter(|focus| focus.focus_type == "tag")
            .map(|focus| focus.value.clone())
            .collect();
        let focused_by_tag = self
            .fetch_tag_focus_matches(family, &ids, &tag_focus_values)
            .await?;
        for candidate in merged.values_mut() {
            candidate.access_count = access_counts
                .get(&candidate.memory_id)
                .copied()
                .unwrap_or(0);
            candidate.feedback = feedback_by_memory
                .get(&candidate.memory_id)
                .cloned()
                .unwrap_or_default();
            candidate.link_count = link_counts.get(&candidate.memory_id).copied().unwrap_or(0);
            candidate.session_boost = Self::recall_session_multiplier(
                req.session_id.as_deref(),
                candidate.session_id.as_deref(),
            );
            candidate.focus_matches = self.focus_matches_for_memory(
                &candidate.memory_id,
                candidate.session_id.as_deref(),
                &candidate.abstract_text,
                &focuses,
                &focused_by_tag,
            );
            candidate.focus_boost = candidate
                .focus_matches
                .iter()
                .map(|focus| focus.boost.max(1.0))
                .fold(1.0, f64::max);
        }
        Ok(())
    }

    async fn expand_recall_candidates_via_links(
        &self,
        family: &MemoryV2TableFamily,
        req: &RecallV2Request,
        merged: &mut HashMap<String, RecallCandidate>,
        top_k: i64,
    ) -> Result<(), MemoriaError> {
        if merged.is_empty() {
            return Ok(());
        }
        let mut ranked = merged.values().cloned().collect::<Vec<_>>();
        ranked.sort_by(|a, b| {
            self.final_score(b)
                .partial_cmp(&self.final_score(a))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let seed_ids = ranked
            .into_iter()
            .take((top_k as usize).min(MAX_RECALL_LINK_EXPANSION_SEEDS))
            .map(|candidate| candidate.memory_id)
            .collect::<Vec<_>>();
        if seed_ids.is_empty() {
            return Ok(());
        }
        let refs = self.fetch_link_refs(family, &seed_ids).await?;
        let mut bonus_by_target = HashMap::<String, f64>::new();
        let mut sources_by_target = HashMap::<String, Vec<MemoryV2RecallExpansionSource>>::new();
        for seed_id in &seed_ids {
            let Some(seed_candidate) = merged.get(seed_id) else {
                continue;
            };
            let seed_score = self.final_score(seed_candidate);
            for link in refs.get(seed_id).into_iter().flatten() {
                if link.memory_id == *seed_id {
                    continue;
                }
                let bonus = (seed_score * link.strength.max(0.0) * DEFAULT_V2_LINK_EXPANSION_DECAY)
                    .max(0.0);
                sources_by_target
                    .entry(link.memory_id.clone())
                    .or_default()
                    .push(MemoryV2RecallExpansionSource {
                        seed_memory_id: seed_id.clone(),
                        seed_score,
                        link_type: link.link_type.clone(),
                        link_strength: link.strength.max(0.0),
                        bonus,
                    });
                bonus_by_target
                    .entry(link.memory_id.clone())
                    .and_modify(|existing| *existing = existing.max(bonus))
                    .or_insert(bonus);
            }
        }
        if bonus_by_target.is_empty() {
            return Ok(());
        }
        let target_ids = bonus_by_target.keys().cloned().collect::<Vec<_>>();
        let loaded = self
            .load_recall_candidates_by_ids(family, req, &target_ids)
            .await?;
        for (target_id, bonus) in bonus_by_target {
            let mut sources = sources_by_target.remove(&target_id).unwrap_or_default();
            sources.sort_by(|a, b| {
                b.bonus
                    .partial_cmp(&a.bonus)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| a.seed_memory_id.cmp(&b.seed_memory_id))
                    .then_with(|| a.link_type.cmp(&b.link_type))
            });
            if let Some(existing) = merged.get_mut(&target_id) {
                existing.link_bonus = existing.link_bonus.max(bonus);
                existing.expansion_sources = sources;
            } else if let Some(mut candidate) = loaded.get(&target_id).cloned() {
                candidate.link_bonus = bonus;
                candidate.expansion_sources = sources;
                merged.insert(target_id, candidate);
            }
        }
        Ok(())
    }

    async fn load_recall_candidates_by_ids(
        &self,
        family: &MemoryV2TableFamily,
        req: &RecallV2Request,
        ids: &[String],
    ) -> Result<HashMap<String, RecallCandidate>, MemoriaError> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let mut clauses = vec![
            "h.forgotten_at IS NULL".to_string(),
            format!("h.memory_id IN ({placeholders})"),
        ];
        if let Some(mt) = req.memory_type.as_ref() {
            clauses.push(format!(
                "h.memory_type = '{}'",
                sanitize_sql_literal(&mt.to_string())
            ));
        }
        if req.session_only {
            if let Some(ref sid) = req.session_id {
                clauses.push(format!("h.session_id = '{}'", sanitize_sql_literal(sid)));
            }
        }
        if let Some(tag_clause) = build_recall_tag_clause(family, &req.tags, &req.tag_filter_mode) {
            clauses.push(tag_clause);
        }
        clauses.extend(build_recall_time_clauses(
            req.created_after,
            req.created_before,
        ));
        let sql = format!(
            "SELECT h.memory_id, h.memory_type, h.session_id, h.confidence, h.importance, h.created_at, \
             c.abstract_text, c.overview_text, c.has_overview, c.has_detail, c.abstract_token_estimate, c.overview_token_estimate \
             FROM {} h \
             JOIN {} c ON c.content_version_id = h.current_content_version_id \
             WHERE {}",
            family.heads_table,
            family.content_versions_table,
            clauses.join(" AND ")
        );
        let mut query = sqlx::query(&sql);
        for id in ids {
            query = query.bind(id);
        }
        let rows = query.fetch_all(&self.pool).await.map_err(db_err)?;
        rows.into_iter()
            .map(|row| {
                let candidate = self.row_to_candidate(row)?;
                Ok((candidate.memory_id.clone(), candidate))
            })
            .collect()
    }

    async fn load_active_focuses(
        &self,
        family: &MemoryV2TableFamily,
    ) -> Result<Vec<ActiveFocus>, MemoriaError> {
        let rows = sqlx::query(&format!(
            "SELECT focus_type, focus_value, boost FROM {} \
             WHERE state = 'active' AND expires_at > NOW()",
            family.focus_table
        ))
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(rows
            .into_iter()
            .map(|r| ActiveFocus {
                focus_type: r.try_get("focus_type").unwrap_or_default(),
                value: r.try_get("focus_value").unwrap_or_default(),
                boost: r.try_get::<f32, _>("boost").unwrap_or(1.0) as f64,
            })
            .collect())
    }

    async fn fetch_tag_focus_matches(
        &self,
        family: &MemoryV2TableFamily,
        ids: &[String],
        focus_tags: &[String],
    ) -> Result<HashSet<String>, MemoriaError> {
        if ids.is_empty() || focus_tags.is_empty() {
            return Ok(HashSet::new());
        }
        let id_placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let tag_placeholders = focus_tags.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!(
            "SELECT DISTINCT memory_id FROM {} WHERE memory_id IN ({}) AND tag IN ({})",
            family.tags_table, id_placeholders, tag_placeholders
        );
        let mut q = sqlx::query(&sql);
        for id in ids {
            q = q.bind(id);
        }
        for tag in focus_tags {
            q = q.bind(tag);
        }
        let rows = q.fetch_all(&self.pool).await.map_err(db_err)?;
        Ok(rows
            .into_iter()
            .filter_map(|r| r.try_get("memory_id").ok())
            .collect())
    }

    fn focus_matches_for_memory(
        &self,
        memory_id: &str,
        session_id: Option<&str>,
        abstract_text: &str,
        focuses: &[ActiveFocus],
        focused_by_tag: &HashSet<String>,
    ) -> Vec<MemoryV2FocusMatch> {
        let abstract_lc = abstract_text.to_lowercase();
        let mut matches = focuses
            .iter()
            .filter(|focus| match focus.focus_type.as_str() {
                "memory_id" => memory_id == focus.value,
                "session" => session_id == Some(focus.value.as_str()),
                "tag" => focused_by_tag.contains(memory_id),
                "topic" => abstract_lc.contains(&focus.value.to_lowercase()),
                _ => false,
            })
            .map(|focus| MemoryV2FocusMatch {
                focus_type: focus.focus_type.clone(),
                value: focus.value.clone(),
                boost: focus.boost.max(1.0),
            })
            .collect::<Vec<_>>();
        matches.sort_by(|a, b| {
            b.boost
                .partial_cmp(&a.boost)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.focus_type.cmp(&b.focus_type))
                .then_with(|| a.value.cmp(&b.value))
        });
        matches
    }

    fn related_ranking(
        &self,
        item: &MemoryV2RelatedItem,
        source_session_id: Option<&str>,
        access_count: i32,
        feedback_impact: &MemoryV2FeedbackImpact,
        focuses: &[ActiveFocus],
        focused_by_tag: &HashSet<String>,
    ) -> MemoryV2RelatedRanking {
        let base_strength = item.strength.max(0.0);
        let session_affinity_applied = matches!(
            (source_session_id, item.session_id.as_deref()),
            (Some(source_session_id), Some(item_session_id)) if source_session_id == item_session_id
        );
        let session_affinity_multiplier = if session_affinity_applied { 1.08 } else { 1.0 };
        let access_multiplier = if access_count > 0 {
            1.0 + 0.05 * ((1 + access_count) as f64).ln()
        } else {
            1.0
        };
        let content_multiplier = if item.has_detail {
            1.02
        } else if item.has_overview {
            1.01
        } else {
            1.0
        };
        let focus_matches = self.focus_matches_for_memory(
            &item.memory_id,
            item.session_id.as_deref(),
            &item.abstract_text,
            focuses,
            focused_by_tag,
        );
        let focus_boost = focus_matches
            .iter()
            .map(|focus| focus.boost.max(1.0))
            .fold(1.0, f64::max);
        let same_hop_score = base_strength
            * session_affinity_multiplier
            * access_multiplier
            * feedback_impact.multiplier
            * content_multiplier
            * focus_boost;
        MemoryV2RelatedRanking {
            same_hop_score,
            base_strength,
            session_affinity_applied,
            session_affinity_multiplier,
            access_count,
            access_multiplier,
            feedback_multiplier: feedback_impact.multiplier,
            content_multiplier,
            focus_boost,
            focus_matches,
        }
    }

    async fn fetch_link_refs(
        &self,
        family: &MemoryV2TableFamily,
        ids: &[String],
    ) -> Result<HashMap<String, Vec<LinkV2Ref>>, MemoriaError> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!(
            "SELECT l.memory_id, l.target_memory_id, l.link_type, l.strength, tc.abstract_text \
             FROM {} l \
             JOIN {} th ON th.memory_id = l.target_memory_id AND th.forgotten_at IS NULL \
             JOIN {} tc ON tc.content_version_id = th.current_content_version_id \
             WHERE l.memory_id IN ({}) \
             ORDER BY l.strength DESC, l.target_memory_id ASC",
            family.links_table, family.heads_table, family.content_versions_table, placeholders
        );
        let mut q = sqlx::query(&sql);
        for id in ids {
            q = q.bind(id);
        }
        let rows = q.fetch_all(&self.pool).await.map_err(db_err)?;
        let mut out: HashMap<String, Vec<LinkV2Ref>> = HashMap::new();
        for row in rows {
            let memory_id: String = row.try_get("memory_id").map_err(db_err)?;
            out.entry(memory_id).or_default().push(LinkV2Ref {
                memory_id: row.try_get("target_memory_id").map_err(db_err)?,
                abstract_text: row.try_get("abstract_text").map_err(db_err)?,
                link_type: row.try_get("link_type").map_err(db_err)?,
                strength: row.try_get::<f32, _>("strength").unwrap_or(0.0) as f64,
                provenance: MemoryV2LinkProvenance::default(),
            });
        }
        self.populate_link_ref_provenance(family, &mut out).await?;
        Ok(out)
    }

    async fn populate_link_ref_provenance(
        &self,
        family: &MemoryV2TableFamily,
        refs_by_memory: &mut HashMap<String, Vec<LinkV2Ref>>,
    ) -> Result<(), MemoriaError> {
        if refs_by_memory.is_empty() {
            return Ok(());
        }
        let mut ids = HashSet::new();
        for (memory_id, refs) in refs_by_memory.iter() {
            ids.insert(memory_id.clone());
            for link in refs {
                ids.insert(link.memory_id.clone());
            }
        }
        let ids = ids.into_iter().collect::<Vec<_>>();
        let tags_by_memory = self.fetch_memory_tags(family, &ids).await?;
        let embeddings_by_memory = self.fetch_memory_embeddings(family, &ids).await?;
        let extraction_traces_by_memory = self.fetch_link_extraction_traces(family, &ids).await?;
        for (source_id, refs) in refs_by_memory.iter_mut() {
            for link in refs.iter_mut() {
                link.provenance = self.compute_link_provenance(
                    source_id,
                    &link.memory_id,
                    &link.link_type,
                    link.strength,
                    &tags_by_memory,
                    &embeddings_by_memory,
                    extraction_traces_by_memory.get(source_id.as_str()).cloned(),
                );
            }
        }
        Ok(())
    }

    async fn ensure_active_memory(
        &self,
        family: &MemoryV2TableFamily,
        memory_id: &str,
    ) -> Result<(), MemoriaError> {
        let exists = sqlx::query(&format!(
            "SELECT memory_id FROM {} WHERE memory_id = ? AND forgotten_at IS NULL LIMIT 1",
            family.heads_table
        ))
        .bind(memory_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?;
        if exists.is_some() {
            Ok(())
        } else {
            Err(MemoriaError::NotFound(memory_id.to_string()))
        }
    }

    async fn load_active_memory_session_id(
        &self,
        family: &MemoryV2TableFamily,
        memory_id: &str,
    ) -> Result<Option<String>, MemoriaError> {
        let row = sqlx::query(&format!(
            "SELECT session_id FROM {} WHERE memory_id = ? AND forgotten_at IS NULL LIMIT 1",
            family.heads_table
        ))
        .bind(memory_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?
        .ok_or_else(|| MemoriaError::NotFound(memory_id.to_string()))?;
        Ok(row.try_get("session_id").ok())
    }

    async fn load_reflect_memories(
        &self,
        family: &MemoryV2TableFamily,
        session_id: Option<&str>,
    ) -> Result<Vec<ReflectV2MemoryItem>, MemoriaError> {
        let mut sql = format!(
            "SELECT h.memory_id, h.memory_type, h.session_id, h.importance, c.abstract_text \
             FROM {} h \
             JOIN {} c ON c.content_version_id = h.current_content_version_id \
             WHERE h.forgotten_at IS NULL AND h.memory_type != '{}' \
               AND (h.source_kind IS NULL OR h.source_kind != '{}')",
            family.heads_table,
            family.content_versions_table,
            sanitize_sql_literal(&MemoryType::Profile.to_string()),
            sanitize_sql_literal(REFLECT_V2_SOURCE_KIND)
        );
        if session_id.is_some() {
            sql.push_str(" AND h.session_id = ?");
        }
        sql.push_str(" ORDER BY h.updated_at DESC, h.memory_id DESC");
        let mut query = sqlx::query(&sql);
        if let Some(session_id) = session_id {
            query = query.bind(session_id);
        }
        let rows = query.fetch_all(&self.pool).await.map_err(db_err)?;
        rows.into_iter()
            .map(|row| {
                Ok(ReflectV2MemoryItem {
                    memory_id: row.try_get("memory_id").map_err(db_err)?,
                    abstract_text: row.try_get("abstract_text").map_err(db_err)?,
                    memory_type: MemoryType::from_str(
                        &row.try_get::<String, _>("memory_type").map_err(db_err)?,
                    )
                    .map_err(|e| MemoriaError::Validation(e.to_string()))?,
                    session_id: row.try_get("session_id").ok(),
                    importance: row.try_get::<f32, _>("importance").unwrap_or(0.0) as f64,
                })
            })
            .collect()
    }

    async fn reflect_internal_writeback(
        &self,
        family: &MemoryV2TableFamily,
        candidates: &[ReflectV2Candidate],
    ) -> Result<i64, MemoriaError> {
        if candidates.is_empty() {
            return Ok(0);
        }
        let mut existing_source_keys = self.load_existing_reflect_source_keys(family).await?;
        let now = Utc::now().naive_utc();
        let mut tx = self.pool.begin().await.map_err(db_err)?;
        let mut scenes_created = 0i64;

        for candidate in candidates {
            let source_memory_ids = normalize_reflect_source_ids(
                candidate
                    .memories
                    .iter()
                    .map(|memory| memory.memory_id.clone())
                    .collect::<Vec<_>>(),
            );
            if source_memory_ids.len() < 2 {
                continue;
            }
            let source_key = reflect_source_key(&source_memory_ids);
            if !existing_source_keys.insert(source_key) {
                continue;
            }

            let remembered = self
                .remember_tx(
                    &mut tx,
                    family,
                    MemoryV2RememberInput {
                        content: synthesize_reflect_content(candidate),
                        memory_type: MemoryType::Semantic,
                        session_id: reflect_common_session_id(candidate),
                        importance: Some(candidate.importance),
                        trust_tier: Some(TrustTier::T3Inferred),
                        tags: vec!["reflect".to_string(), candidate.signal.clone()],
                        source: Some(serde_json::json!({
                            "kind": REFLECT_V2_SOURCE_KIND,
                            "mode": "internal",
                            "signal": candidate.signal,
                            "source_memory_ids": source_memory_ids,
                        })),
                        embedding: None,
                        actor: REFLECT_V2_ACTOR.to_string(),
                    },
                    now,
                )
                .await?;

            let links = candidate
                .memories
                .iter()
                .map(|memory| {
                    (
                        memory.memory_id.clone(),
                        remembered.memory_id.clone(),
                        REFLECT_V2_LINK_TYPE.to_string(),
                        candidate.importance.max(memory.importance),
                    )
                })
                .collect::<Vec<_>>();
            self.insert_links_tx(&mut tx, family, &links, now).await?;
            scenes_created += 1;
        }

        tx.commit().await.map_err(db_err)?;
        Ok(scenes_created)
    }

    async fn load_existing_reflect_source_keys(
        &self,
        family: &MemoryV2TableFamily,
    ) -> Result<HashSet<String>, MemoriaError> {
        let rows = sqlx::query(&format!(
            "SELECT source_json FROM {} WHERE forgotten_at IS NULL AND source_kind = ?",
            family.heads_table
        ))
        .bind(REFLECT_V2_SOURCE_KIND)
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;

        let mut keys = HashSet::new();
        for row in rows {
            let source_json: serde_json::Value = row.try_get("source_json").map_err(db_err)?;
            let Some(source_memory_ids) = source_json
                .get("source_memory_ids")
                .and_then(|value| value.as_array())
            else {
                continue;
            };
            let normalized = normalize_reflect_source_ids(
                source_memory_ids
                    .iter()
                    .filter_map(|value| value.as_str().map(ToOwned::to_owned))
                    .collect::<Vec<_>>(),
            );
            if normalized.len() >= 2 {
                keys.insert(reflect_source_key(&normalized));
            }
        }
        Ok(keys)
    }

    async fn build_reflect_linked_candidates(
        &self,
        family: &MemoryV2TableFamily,
        memories: &[ReflectV2MemoryItem],
        min_cluster_size: usize,
        min_link_strength: f32,
    ) -> Result<(Vec<ReflectV2Candidate>, HashMap<(String, String), f64>), MemoriaError> {
        let memory_by_id = memories
            .iter()
            .cloned()
            .map(|memory| (memory.memory_id.clone(), memory))
            .collect::<HashMap<_, _>>();
        let rows = sqlx::query(&format!(
            "SELECT l.memory_id, l.target_memory_id, l.strength \
             FROM {} l \
             JOIN {} sh ON sh.memory_id = l.memory_id AND sh.forgotten_at IS NULL \
             JOIN {} th ON th.memory_id = l.target_memory_id AND th.forgotten_at IS NULL \
             WHERE l.memory_id != l.target_memory_id AND l.strength >= ?",
            family.links_table, family.heads_table, family.heads_table
        ))
        .bind(min_link_strength)
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;

        let mut adjacency = HashMap::<String, HashSet<String>>::new();
        let mut edges_by_pair = HashMap::<(String, String), f64>::new();
        for row in rows {
            let source_id: String = row.try_get("memory_id").map_err(db_err)?;
            let target_id: String = row.try_get("target_memory_id").map_err(db_err)?;
            if !memory_by_id.contains_key(&source_id) || !memory_by_id.contains_key(&target_id) {
                continue;
            }
            let strength = row.try_get::<f32, _>("strength").unwrap_or(0.0) as f64;
            adjacency
                .entry(source_id.clone())
                .or_default()
                .insert(target_id.clone());
            adjacency
                .entry(target_id.clone())
                .or_default()
                .insert(source_id.clone());
            let pair = if source_id < target_id {
                (source_id, target_id)
            } else {
                (target_id, source_id)
            };
            edges_by_pair
                .entry(pair)
                .and_modify(|existing| *existing = existing.max(strength))
                .or_insert(strength);
        }

        let mut visited = HashSet::<String>::new();
        let mut candidates = Vec::<ReflectV2Candidate>::new();
        for memory_id in memory_by_id.keys() {
            if visited.contains(memory_id) || !adjacency.contains_key(memory_id) {
                continue;
            }
            let mut stack = vec![memory_id.clone()];
            let mut component = Vec::<String>::new();
            while let Some(current) = stack.pop() {
                if !visited.insert(current.clone()) {
                    continue;
                }
                component.push(current.clone());
                if let Some(neighbors) = adjacency.get(&current) {
                    for neighbor in neighbors {
                        if !visited.contains(neighbor) {
                            stack.push(neighbor.clone());
                        }
                    }
                }
            }
            if component.len() < min_cluster_size {
                continue;
            }
            component.sort();
            let component_set = component.iter().cloned().collect::<HashSet<_>>();
            let link_count = edges_by_pair
                .keys()
                .filter(|(left, right)| {
                    component_set.contains(left) && component_set.contains(right)
                })
                .count() as i64;
            let total_link_strength = edges_by_pair
                .iter()
                .filter(|((left, right), _)| {
                    component_set.contains(left) && component_set.contains(right)
                })
                .map(|(_, strength)| *strength)
                .sum::<f64>();
            let mut sessions = HashSet::<String>::new();
            let mut items = component
                .iter()
                .filter_map(|memory_id| memory_by_id.get(memory_id).cloned())
                .collect::<Vec<_>>();
            items.sort_by(|a, b| {
                b.importance
                    .partial_cmp(&a.importance)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| a.memory_id.cmp(&b.memory_id))
            });
            for item in &items {
                if let Some(session_id) =
                    item.session_id.as_deref().filter(|value| !value.is_empty())
                {
                    sessions.insert(session_id.to_string());
                }
            }
            let avg_importance =
                items.iter().map(|item| item.importance).sum::<f64>() / items.len() as f64;
            let avg_link_strength = if link_count > 0 {
                total_link_strength / link_count as f64
            } else {
                0.0
            };
            let importance = (avg_importance * 0.6
                + avg_link_strength * 0.4
                + if sessions.len() > 1 { 0.1 } else { 0.0 })
            .clamp(0.0, 1.0);
            candidates.push(ReflectV2Candidate {
                signal: if sessions.len() > 1 {
                    "cross_session_linked_cluster".to_string()
                } else {
                    "linked_cluster".to_string()
                },
                importance,
                memory_count: items.len() as i64,
                session_count: sessions.len() as i64,
                link_count,
                memories: items,
            });
        }
        Ok((candidates, edges_by_pair))
    }

    fn build_reflect_session_candidates(
        &self,
        memories: &[ReflectV2MemoryItem],
        edges_by_pair: &HashMap<(String, String), f64>,
        min_cluster_size: usize,
    ) -> Vec<ReflectV2Candidate> {
        let mut by_session = HashMap::<String, Vec<ReflectV2MemoryItem>>::new();
        for memory in memories {
            let Some(session_id) = memory
                .session_id
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
            else {
                continue;
            };
            by_session
                .entry(session_id.to_string())
                .or_default()
                .push(memory.clone());
        }
        let mut candidates = Vec::new();
        for (_, mut items) in by_session {
            if items.len() < min_cluster_size {
                continue;
            }
            items.sort_by(|a, b| {
                b.importance
                    .partial_cmp(&a.importance)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| a.memory_id.cmp(&b.memory_id))
            });
            let component_set = items
                .iter()
                .map(|item| item.memory_id.clone())
                .collect::<HashSet<_>>();
            let link_count = edges_by_pair
                .keys()
                .filter(|(left, right)| {
                    component_set.contains(left) && component_set.contains(right)
                })
                .count() as i64;
            let avg_importance =
                items.iter().map(|item| item.importance).sum::<f64>() / items.len() as f64;
            candidates.push(ReflectV2Candidate {
                signal: "session_cluster".to_string(),
                importance: avg_importance.clamp(0.0, 1.0),
                memory_count: items.len() as i64,
                session_count: 1,
                link_count,
                memories: items,
            });
        }
        candidates
    }

    async fn query_link_items(
        &self,
        family: &MemoryV2TableFamily,
        memory_id: &str,
        direction: LinkDirection,
        link_type: Option<&str>,
        min_strength: f64,
        limit: i64,
    ) -> Result<Vec<MemoryV2LinkItem>, MemoriaError> {
        let limit = limit.clamp(1, 200);
        let min_strength = min_strength.clamp(0.0, 1.0) as f32;
        let mut items = Vec::new();
        if matches!(direction, LinkDirection::Outbound | LinkDirection::Both) {
            items.extend(
                self.query_links_for_direction(
                    family,
                    memory_id,
                    LinkDirection::Outbound,
                    link_type,
                    min_strength,
                    limit,
                )
                .await?,
            );
        }
        if matches!(direction, LinkDirection::Inbound | LinkDirection::Both) {
            items.extend(
                self.query_links_for_direction(
                    family,
                    memory_id,
                    LinkDirection::Inbound,
                    link_type,
                    min_strength,
                    limit,
                )
                .await?,
            );
        }
        items.sort_by(|a, b| {
            b.strength
                .partial_cmp(&a.strength)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.memory_id.cmp(&b.memory_id))
                .then_with(|| a.direction.as_str().cmp(b.direction.as_str()))
        });
        items.truncate(limit as usize);
        Ok(items)
    }

    async fn load_direct_link_summary(
        &self,
        family: &MemoryV2TableFamily,
        memory_id: &str,
    ) -> Result<MemoryV2LinkSummary, MemoriaError> {
        let outbound_rows = sqlx::query(&format!(
            "SELECT l.link_type, CAST(COUNT(*) AS SIGNED) AS cnt \
             FROM {} l \
             JOIN {} h ON h.memory_id = l.target_memory_id AND h.forgotten_at IS NULL \
             WHERE l.memory_id = ? \
             GROUP BY l.link_type",
            family.links_table, family.heads_table
        ))
        .bind(memory_id)
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;
        let inbound_rows = sqlx::query(&format!(
            "SELECT l.link_type, CAST(COUNT(*) AS SIGNED) AS cnt \
             FROM {} l \
             JOIN {} h ON h.memory_id = l.memory_id AND h.forgotten_at IS NULL \
             WHERE l.target_memory_id = ? \
             GROUP BY l.link_type",
            family.links_table, family.heads_table
        ))
        .bind(memory_id)
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;

        let mut outbound_count = 0i64;
        let mut inbound_count = 0i64;
        let mut by_type = HashMap::<String, MemoryV2LinkTypeSummary>::new();

        for row in outbound_rows {
            let link_type: String = row.try_get("link_type").map_err(db_err)?;
            let cnt = row.try_get::<i64, _>("cnt").map_err(db_err)?;
            outbound_count += cnt;
            by_type
                .entry(link_type.clone())
                .and_modify(|item| item.outbound_count += cnt)
                .or_insert(MemoryV2LinkTypeSummary {
                    link_type,
                    outbound_count: cnt,
                    inbound_count: 0,
                });
        }
        for row in inbound_rows {
            let link_type: String = row.try_get("link_type").map_err(db_err)?;
            let cnt = row.try_get::<i64, _>("cnt").map_err(db_err)?;
            inbound_count += cnt;
            by_type
                .entry(link_type.clone())
                .and_modify(|item| item.inbound_count += cnt)
                .or_insert(MemoryV2LinkTypeSummary {
                    link_type,
                    outbound_count: 0,
                    inbound_count: cnt,
                });
        }

        let mut link_types = by_type.into_values().collect::<Vec<_>>();
        link_types.sort_by(|a, b| {
            (b.outbound_count + b.inbound_count)
                .cmp(&(a.outbound_count + a.inbound_count))
                .then_with(|| a.link_type.cmp(&b.link_type))
        });

        Ok(MemoryV2LinkSummary {
            outbound_count,
            inbound_count,
            total_count: outbound_count + inbound_count,
            link_types,
        })
    }

    async fn query_links_for_direction(
        &self,
        family: &MemoryV2TableFamily,
        memory_id: &str,
        direction: LinkDirection,
        link_type: Option<&str>,
        min_strength: f32,
        limit: i64,
    ) -> Result<Vec<MemoryV2LinkItem>, MemoriaError> {
        let (related_select, link_filter) = match direction {
            LinkDirection::Outbound => ("l.target_memory_id", "l.memory_id = ?"),
            LinkDirection::Inbound => ("l.memory_id", "l.target_memory_id = ?"),
            LinkDirection::Both => unreachable!("both is handled by query_link_items"),
        };
        let mut sql = format!(
            "SELECT {related_select} AS related_memory_id, h.memory_type, h.session_id, \
                    c.abstract_text, c.has_overview, c.has_detail, l.link_type, l.strength \
             FROM {} l \
             JOIN {} h ON h.memory_id = {related_select} AND h.forgotten_at IS NULL \
             JOIN {} c ON c.content_version_id = h.current_content_version_id \
             WHERE {link_filter} AND l.strength >= ?",
            family.links_table, family.heads_table, family.content_versions_table
        );
        if link_type.is_some() {
            sql.push_str(" AND l.link_type = ?");
        }
        sql.push_str(&format!(
            " ORDER BY l.strength DESC, related_memory_id ASC LIMIT {}",
            limit
        ));
        let mut q = sqlx::query(&sql).bind(memory_id).bind(min_strength);
        if let Some(link_type) = link_type {
            q = q.bind(link_type);
        }
        let rows = q.fetch_all(&self.pool).await.map_err(db_err)?;
        rows.into_iter()
            .map(|row| {
                Ok(MemoryV2LinkItem {
                    memory_id: row.try_get("related_memory_id").map_err(db_err)?,
                    abstract_text: row.try_get("abstract_text").map_err(db_err)?,
                    memory_type: MemoryType::from_str(
                        &row.try_get::<String, _>("memory_type").map_err(db_err)?,
                    )
                    .map_err(|e| MemoriaError::Validation(e.to_string()))?,
                    session_id: row.try_get("session_id").ok(),
                    has_overview: row.try_get::<i8, _>("has_overview").unwrap_or(0) != 0,
                    has_detail: row.try_get::<i8, _>("has_detail").unwrap_or(0) != 0,
                    link_type: row.try_get("link_type").map_err(db_err)?,
                    strength: row.try_get::<f32, _>("strength").unwrap_or(0.0) as f64,
                    direction,
                    provenance: MemoryV2LinkProvenance::default(),
                })
            })
            .collect()
    }

    async fn populate_link_provenance(
        &self,
        family: &MemoryV2TableFamily,
        anchor_memory_id: &str,
        items: &mut [MemoryV2LinkItem],
    ) -> Result<(), MemoriaError> {
        if items.is_empty() {
            return Ok(());
        }
        let mut ids = HashSet::from([anchor_memory_id.to_string()]);
        for item in items.iter() {
            ids.insert(item.memory_id.clone());
        }
        let ids = ids.into_iter().collect::<Vec<_>>();
        let tags_by_memory = self.fetch_memory_tags(family, &ids).await?;
        let embeddings_by_memory = self.fetch_memory_embeddings(family, &ids).await?;
        let extraction_traces_by_memory = self.fetch_link_extraction_traces(family, &ids).await?;
        for item in items.iter_mut() {
            let (source_id, target_id) = match item.direction {
                LinkDirection::Outbound => (anchor_memory_id, item.memory_id.as_str()),
                LinkDirection::Inbound => (item.memory_id.as_str(), anchor_memory_id),
                LinkDirection::Both => (anchor_memory_id, item.memory_id.as_str()),
            };
            item.provenance = self.compute_link_provenance(
                source_id,
                target_id,
                &item.link_type,
                item.strength,
                &tags_by_memory,
                &embeddings_by_memory,
                extraction_traces_by_memory.get(source_id).cloned(),
            );
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn compute_link_provenance(
        &self,
        source_id: &str,
        target_id: &str,
        final_link_type: &str,
        final_strength: f64,
        tags_by_memory: &HashMap<String, HashSet<String>>,
        embeddings_by_memory: &HashMap<String, Vec<f32>>,
        extraction_trace: Option<MemoryV2LinkExtractionTrace>,
    ) -> MemoryV2LinkProvenance {
        let mut evidence = Vec::<MemoryV2LinkEvidenceDetail>::new();
        if let Some(source_tags) = tags_by_memory.get(source_id) {
            if !source_tags.is_empty() {
                let target_tag_count = tags_by_memory
                    .get(target_id)
                    .map(|target_tags| target_tags.len())
                    .unwrap_or(0);
                let overlap = tags_by_memory
                    .get(target_id)
                    .map(|target_tags| source_tags.intersection(target_tags).count())
                    .unwrap_or(0);
                if overlap > 0 {
                    evidence.push(MemoryV2LinkEvidenceDetail {
                        evidence_type: "tag_overlap".to_string(),
                        strength: (overlap as f64 / source_tags.len() as f64).clamp(0.0, 1.0),
                        overlap_count: Some(overlap as i64),
                        source_tag_count: Some(source_tags.len() as i64),
                        target_tag_count: Some(target_tag_count as i64),
                        vector_distance: None,
                    });
                }
            }
        }
        if let (Some(source_embedding), Some(target_embedding)) = (
            embeddings_by_memory.get(source_id),
            embeddings_by_memory.get(target_id),
        ) {
            if !source_embedding.is_empty() && source_embedding.len() == target_embedding.len() {
                let dist = source_embedding
                    .iter()
                    .zip(target_embedding.iter())
                    .map(|(left, right)| {
                        let delta = *left as f64 - *right as f64;
                        delta * delta
                    })
                    .sum::<f64>()
                    .sqrt();
                let strength = (1.0 / (1.0 + dist.max(0.0))).clamp(0.0, 1.0);
                if strength >= 0.55 {
                    evidence.push(MemoryV2LinkEvidenceDetail {
                        evidence_type: "semantic_related".to_string(),
                        strength,
                        overlap_count: None,
                        source_tag_count: None,
                        target_tag_count: None,
                        vector_distance: Some(dist),
                    });
                }
            }
        }
        evidence.sort_by(|a, b| {
            b.strength
                .partial_cmp(&a.strength)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.evidence_type.cmp(&b.evidence_type))
        });
        let mut evidence_types = evidence
            .iter()
            .map(|detail| detail.evidence_type.clone())
            .collect::<Vec<_>>();
        evidence_types.sort();
        evidence_types.dedup();
        let primary = evidence.first().cloned();
        let refined = primary
            .as_ref()
            .map(|detail| {
                detail.evidence_type != final_link_type
                    || (detail.strength - final_strength).abs() > 0.0001
            })
            .unwrap_or(false);
        MemoryV2LinkProvenance {
            evidence_types,
            primary_evidence_type: primary.as_ref().map(|detail| detail.evidence_type.clone()),
            primary_evidence_strength: primary.as_ref().map(|detail| detail.strength),
            refined,
            evidence,
            extraction_trace,
        }
    }

    async fn fetch_link_extraction_traces(
        &self,
        family: &MemoryV2TableFamily,
        ids: &[String],
    ) -> Result<HashMap<String, MemoryV2LinkExtractionTrace>, MemoriaError> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let mut traces = HashMap::<String, MemoryV2LinkExtractionTrace>::new();
        let mut current_versions = HashMap::<String, String>::new();

        let projection_sql = format!(
            "SELECT h.memory_id, h.current_content_version_id, c.derivation_state \
             FROM {} h \
             JOIN {} c ON c.content_version_id = h.current_content_version_id \
             WHERE h.memory_id IN ({}) AND h.forgotten_at IS NULL",
            family.heads_table, family.content_versions_table, placeholders
        );
        let mut projection_q = sqlx::query(&projection_sql);
        for id in ids {
            projection_q = projection_q.bind(id);
        }
        let projection_rows = projection_q.fetch_all(&self.pool).await.map_err(db_err)?;
        for row in projection_rows {
            let memory_id: String = row.try_get("memory_id").map_err(db_err)?;
            let current_content_version_id: String =
                row.try_get("current_content_version_id").map_err(db_err)?;
            let derivation_state: String = row
                .try_get("derivation_state")
                .unwrap_or_else(|_| "pending".to_string());
            current_versions.insert(memory_id.clone(), current_content_version_id.clone());
            traces.insert(
                memory_id,
                MemoryV2LinkExtractionTrace {
                    content_version_id: Some(current_content_version_id),
                    derivation_state: Some(derivation_state),
                    latest_job_status: None,
                    latest_job_attempts: None,
                    latest_job_updated_at: None,
                    latest_job_error: None,
                },
            );
        }

        let job_sql = format!(
            "SELECT job_id, aggregate_id, payload_json, status, attempts, last_error, updated_at \
             FROM {} \
             WHERE job_type = 'extract_links' AND aggregate_id IN ({}) \
             ORDER BY updated_at DESC, job_id DESC",
            family.jobs_table, placeholders
        );
        let mut job_q = sqlx::query(&job_sql);
        for id in ids {
            job_q = job_q.bind(id);
        }
        let job_rows = job_q.fetch_all(&self.pool).await.map_err(db_err)?;
        let mut matched_current_version = HashSet::<String>::new();
        for row in job_rows {
            let memory_id: String = row.try_get("aggregate_id").map_err(db_err)?;
            let payload: serde_json::Value = row.try_get("payload_json").map_err(db_err)?;
            let payload_content_version_id = payload["content_version_id"]
                .as_str()
                .map(|value| value.to_string());
            let current_content_version_id = current_versions.get(&memory_id).cloned();
            let matches_current_version = current_content_version_id.is_some()
                && payload_content_version_id.is_some()
                && payload_content_version_id == current_content_version_id;
            if matched_current_version.contains(&memory_id) && !matches_current_version {
                continue;
            }

            let trace = traces
                .entry(memory_id.clone())
                .or_insert(MemoryV2LinkExtractionTrace {
                    content_version_id: payload_content_version_id.clone(),
                    derivation_state: None,
                    latest_job_status: None,
                    latest_job_attempts: None,
                    latest_job_updated_at: None,
                    latest_job_error: None,
                });
            if trace.content_version_id.is_none() {
                trace.content_version_id = payload_content_version_id.clone();
            }
            trace.latest_job_status = Some(
                normalize_job_status(row.try_get::<String, _>("status").map_err(db_err)?.as_str())
                    .to_string(),
            );
            trace.latest_job_attempts = Some(row.try_get("attempts").unwrap_or(0));
            trace.latest_job_updated_at = Some(to_utc(
                row.try_get::<NaiveDateTime, _>("updated_at")
                    .map_err(db_err)?,
            ));
            trace.latest_job_error = row.try_get("last_error").ok();
            if matches_current_version {
                matched_current_version.insert(memory_id);
            }
        }

        Ok(traces)
    }

    async fn fetch_memory_tags(
        &self,
        family: &MemoryV2TableFamily,
        ids: &[String],
    ) -> Result<HashMap<String, HashSet<String>>, MemoriaError> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!(
            "SELECT memory_id, tag FROM {} WHERE memory_id IN ({})",
            family.tags_table, placeholders
        );
        let mut q = sqlx::query(&sql);
        for id in ids {
            q = q.bind(id);
        }
        let rows = q.fetch_all(&self.pool).await.map_err(db_err)?;
        let mut out = HashMap::<String, HashSet<String>>::new();
        for row in rows {
            let memory_id: String = row.try_get("memory_id").map_err(db_err)?;
            let tag: String = row.try_get("tag").map_err(db_err)?;
            out.entry(memory_id).or_default().insert(tag);
        }
        Ok(out)
    }

    async fn fetch_memory_embeddings(
        &self,
        family: &MemoryV2TableFamily,
        ids: &[String],
    ) -> Result<HashMap<String, Vec<f32>>, MemoriaError> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!(
            "SELECT h.memory_id, d.embedding AS emb_str \
             FROM {} h \
             JOIN {} d ON d.index_doc_id = h.current_index_doc_id \
             WHERE h.memory_id IN ({}) AND h.forgotten_at IS NULL AND d.embedding IS NOT NULL",
            family.heads_table, family.index_docs_table, placeholders
        );
        let mut q = sqlx::query(&sql);
        for id in ids {
            q = q.bind(id);
        }
        let rows = q.fetch_all(&self.pool).await.map_err(db_err)?;
        let mut out = HashMap::new();
        for row in rows {
            let memory_id: String = row.try_get("memory_id").map_err(db_err)?;
            let emb_str: Option<String> = row.try_get("emb_str").map_err(db_err)?;
            if let Some(emb_str) = emb_str {
                out.insert(memory_id, mo_to_vec(&emb_str)?);
            }
        }
        Ok(out)
    }

    async fn bump_access_counts(
        &self,
        family: &MemoryV2TableFamily,
        ids: &[String],
    ) -> Result<(), MemoriaError> {
        if ids.is_empty() {
            return Ok(());
        }
        let now = Utc::now().naive_utc();
        let placeholders = ids
            .iter()
            .map(|_| "(?, 1, ?)")
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "INSERT INTO {} (memory_id, access_count, last_accessed_at) VALUES {} \
             ON DUPLICATE KEY UPDATE access_count = access_count + 1, last_accessed_at = VALUES(last_accessed_at)",
            family.stats_table, placeholders
        );
        let mut q = sqlx::query(&sql);
        for id in ids {
            q = q.bind(id).bind(now);
        }
        q.execute(&self.pool).await.map_err(db_err)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_table_family_is_deterministic() {
        let a = MemoryV2TableFamily::for_user("alice");
        let b = MemoryV2TableFamily::for_user("alice");
        assert_eq!(a, b);
        assert!(a.events_table.starts_with("mem_"));
        assert!(a.index_docs_table.starts_with("mem_"));
    }

    #[test]
    fn test_table_family_differs_per_user() {
        let a = MemoryV2TableFamily::for_user("alice");
        let b = MemoryV2TableFamily::for_user("bob");
        assert_ne!(a.suffix, b.suffix);
        assert_ne!(a.heads_table, b.heads_table);
    }

    #[test]
    fn test_preview_abstract_truncates() {
        let content = "x".repeat(500);
        let abstract_text = MemoryV2Store::preview_abstract(&content);
        assert!(abstract_text.len() <= ABSTRACT_BYTES);
    }

    #[test]
    fn test_derive_overview_prefers_first_sentences() {
        let text =
            "Rust makes systems programming safer. Ownership catches mistakes. Extra detail.";
        let overview = derive_overview_text(text, "fallback");
        assert!(overview.contains("Rust makes systems programming safer."));
        assert!(overview.contains("Ownership catches mistakes."));
        assert!(!overview.contains("Extra detail."));
    }

    #[test]
    fn test_derive_detail_falls_back_to_abstract() {
        let detail = derive_detail_text("", "fallback abstract");
        assert_eq!(detail, "fallback abstract");
    }
}
