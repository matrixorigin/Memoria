use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RememberRequest {
    pub content: String,
    #[serde(default = "default_memory_type", rename = "type")]
    pub memory_type: String,
    pub session_id: Option<String>,
    pub importance: Option<f64>,
    pub trust_tier: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    pub source: Option<serde_json::Value>,
    #[serde(default)]
    pub sync_enrich: bool,
    #[serde(default)]
    pub enrich_timeout_secs: Option<i64>,
}

fn default_memory_type() -> String {
    "semantic".to_string()
}

#[derive(Debug, Serialize)]
pub struct RememberResponse {
    pub memory_id: String,
    #[serde(rename = "abstract")]
    pub abstract_text: String,
    pub has_overview: bool,
    pub has_detail: bool,
}

#[derive(Debug, Deserialize)]
pub struct BatchRememberRequest {
    #[serde(default)]
    pub memories: Vec<RememberRequest>,
}

#[derive(Debug, Serialize)]
pub struct BatchRememberResponse {
    pub memories: Vec<RememberResponse>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecallTimeRangeRequest {
    #[serde(default)]
    pub start_at: Option<String>,
    #[serde(default)]
    pub end_at: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecallRequest {
    pub query: String,
    #[serde(default = "default_top_k")]
    pub top_k: i64,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: usize,
    #[serde(default = "default_scope")]
    pub scope: String,
    pub session_id: Option<String>,
    #[serde(rename = "type")]
    pub memory_type: Option<String>,
    #[serde(default)]
    pub expand_links: Option<bool>,
    #[serde(default = "default_recall_view")]
    pub view: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default = "default_tag_filter_mode")]
    pub tag_filter_mode: String,
    #[serde(default)]
    pub start_at: Option<String>,
    #[serde(default)]
    pub end_at: Option<String>,
    #[serde(default)]
    pub time_range: Option<RecallTimeRangeRequest>,
    #[serde(default)]
    pub min_confidence: Option<f64>,
    #[serde(default)]
    pub min_importance: Option<f64>,
    #[serde(default)]
    pub exclude_memory_ids: Vec<String>,
    #[serde(default)]
    pub prefer_recent: Option<bool>,
    #[serde(default)]
    pub diversity_factor: Option<f64>,
}

fn default_top_k() -> i64 {
    20
}

fn default_max_tokens() -> usize {
    500
}

fn default_scope() -> String {
    "all".to_string()
}

fn default_tag_filter_mode() -> String {
    "any".to_string()
}

fn default_recall_view() -> String {
    "compact".to_string()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecallView {
    Compact,
    Overview,
    Full,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecallResponseMode {
    CompactAbstract,
    CompactOverview,
    Verbose,
}

impl RecallRequest {
    fn parse_optional_rfc3339(
        value: Option<&str>,
        field: &str,
    ) -> Result<Option<DateTime<Utc>>, String> {
        let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
            return Ok(None);
        };
        chrono::DateTime::parse_from_rfc3339(value)
            .map(|dt| Some(dt.with_timezone(&Utc)))
            .map_err(|e| format!("{field} must be RFC3339: {e}"))
    }

    pub fn resolved_view(&self) -> Result<RecallView, String> {
        match self.view.trim() {
            "" | "compact" => Ok(RecallView::Compact),
            "overview" => Ok(RecallView::Overview),
            "full" => Ok(RecallView::Full),
            _ => Err("view must be 'compact', 'overview', or 'full'".to_string()),
        }
    }

    pub fn resolved_with_overview(&self) -> Result<bool, String> {
        Ok(matches!(
            self.resolved_view()?,
            RecallView::Overview | RecallView::Full
        ))
    }

    pub fn resolved_with_links(&self) -> Result<bool, String> {
        Ok(matches!(self.resolved_view()?, RecallView::Full))
    }

    pub fn resolved_response_mode(&self) -> Result<RecallResponseMode, String> {
        match self.resolved_view()? {
            RecallView::Compact => Ok(RecallResponseMode::CompactAbstract),
            RecallView::Overview => Ok(RecallResponseMode::CompactOverview),
            RecallView::Full => Ok(RecallResponseMode::Verbose),
        }
    }

    pub fn resolved_expand_links(&self) -> bool {
        self.expand_links.unwrap_or(true)
    }

    pub fn resolved_tag_filter_mode(&self) -> Result<String, String> {
        match self.tag_filter_mode.trim() {
            "" | "any" => Ok("any".to_string()),
            "all" => Ok("all".to_string()),
            _ => Err("tag_filter_mode must be 'any' or 'all'".to_string()),
        }
    }

    #[allow(clippy::type_complexity)]
    pub fn resolved_time_range(
        &self,
    ) -> Result<(Option<DateTime<Utc>>, Option<DateTime<Utc>>), String> {
        let start_at = self.start_at.as_deref().or_else(|| {
            self.time_range
                .as_ref()
                .and_then(|time_range| time_range.start_at.as_deref())
        });
        let end_at = self.end_at.as_deref().or_else(|| {
            self.time_range
                .as_ref()
                .and_then(|time_range| time_range.end_at.as_deref())
        });
        let start_at = Self::parse_optional_rfc3339(start_at, "start_at")?;
        let end_at = Self::parse_optional_rfc3339(end_at, "end_at")?;
        if let (Some(start_at), Some(end_at)) = (start_at.as_ref(), end_at.as_ref()) {
            if start_at > end_at {
                return Err("start_at must be less than or equal to end_at".to_string());
            }
        }
        Ok((start_at, end_at))
    }

    pub fn resolved_min_confidence(&self) -> Result<Option<f64>, String> {
        match self.min_confidence {
            Some(v) if !(0.0..=1.0).contains(&v) => {
                Err("min_confidence must be between 0.0 and 1.0".to_string())
            }
            other => Ok(other),
        }
    }

    pub fn resolved_min_importance(&self) -> Result<Option<f64>, String> {
        match self.min_importance {
            Some(v) if !(0.0..=1.0).contains(&v) => {
                Err("min_importance must be between 0.0 and 1.0".to_string())
            }
            other => Ok(other),
        }
    }

    pub fn resolved_diversity_factor(&self) -> Result<Option<f64>, String> {
        match self.diversity_factor {
            Some(v) if !(0.0..=1.0).contains(&v) => {
                Err("diversity_factor must be between 0.0 and 1.0".to_string())
            }
            other => Ok(other),
        }
    }

    pub fn resolved_exclude_memory_ids(&self) -> Vec<String> {
        self.exclude_memory_ids
            .iter()
            .map(|id| id.trim())
            .filter(|id| !id.is_empty())
            .map(ToOwned::to_owned)
            .collect()
    }
}

#[derive(Debug, Deserialize)]
pub struct BatchRecallRequest {
    #[serde(default)]
    pub queries: Vec<RecallRequest>,
}

#[derive(Debug, Serialize)]
pub struct BatchRecallResponse {
    pub results: Vec<RecallResponse>,
}

#[derive(Debug, Serialize)]
pub struct RecallLinkResponse {
    pub memory_id: String,
    #[serde(rename = "abstract")]
    pub abstract_text: String,
    pub link_type: String,
    pub strength: f64,
    pub provenance: LinkProvenanceResponse,
}

#[derive(Debug, Serialize)]
pub struct FeedbackImpactResponse {
    pub useful: i32,
    pub irrelevant: i32,
    pub outdated: i32,
    pub wrong: i32,
    pub multiplier: f64,
}

#[derive(Debug, Serialize)]
pub struct RecallCompactItemResponse {
    pub id: String,
    pub text: String,
    #[serde(rename = "type")]
    pub memory_type: String,
    pub score: f64,
    pub related: bool,
}

#[derive(Debug, Serialize)]
pub struct RecallVerboseItemResponse {
    pub id: String,
    #[serde(rename = "abstract")]
    pub abstract_text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overview: Option<String>,
    pub score: f64,
    #[serde(rename = "type")]
    pub memory_type: String,
    pub confidence: f64,
    pub has_overview: bool,
    pub has_detail: bool,
    pub access_count: i32,
    pub link_count: i64,
    pub has_related: bool,
    pub retrieval_path: String,
    pub feedback_impact: FeedbackImpactResponse,
    pub ranking: RecallRankingResponse,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub links: Option<Vec<RecallLinkResponse>>,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum RecallItemResponse {
    Compact(RecallCompactItemResponse),
    Verbose(Box<RecallVerboseItemResponse>),
}

#[derive(Debug, Serialize)]
pub struct RecallPathSummaryResponse {
    pub retrieval_path: String,
    pub discovered_count: i64,
    pub returned_count: i64,
}

#[derive(Debug, Serialize)]
pub struct RecallSummaryResponse {
    pub discovered_count: i64,
    pub returned_count: i64,
    pub truncated: bool,
    pub by_retrieval_path: Vec<RecallPathSummaryResponse>,
}

#[derive(Debug, Serialize)]
pub struct RecallResponse {
    pub summary: RecallSummaryResponse,
    pub memories: Vec<RecallItemResponse>,
    pub token_used: usize,
    pub has_more: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExpandRequest {
    pub memory_id: String,
    pub level: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BatchExpandRequest {
    #[serde(default)]
    pub memory_ids: Vec<String>,
    pub level: String,
}

#[derive(Debug, Serialize)]
pub struct ExpandResponse {
    pub memory_id: String,
    pub level: String,
    #[serde(rename = "abstract")]
    pub abstract_text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overview: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub links: Option<Vec<RecallLinkResponse>>,
}

#[derive(Debug, Serialize)]
pub struct BatchExpandResponse {
    pub items: Vec<ExpandResponse>,
}

#[derive(Debug, Deserialize)]
pub struct ForgetRequest {
    pub memory_id: String,
    pub reason: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ForgetResponse {
    pub memory_id: String,
    pub forgotten: bool,
}

#[derive(Debug, Deserialize)]
pub struct BatchForgetRequest {
    #[serde(default)]
    pub memory_ids: Vec<String>,
    pub reason: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct BatchForgetResponse {
    pub memories: Vec<ForgetResponse>,
}

#[derive(Debug, Deserialize, Default)]
pub struct HistoryQuery {
    #[serde(default = "default_list_limit")]
    pub limit: i64,
}

#[derive(Debug, Serialize)]
pub struct HistoryItemResponse {
    pub event_id: String,
    pub event_type: String,
    pub actor: String,
    pub processing_state: String,
    pub payload: serde_json::Value,
    pub created_at: String,
}

#[derive(Debug, Serialize)]
pub struct HistoryResponse {
    pub memory_id: String,
    pub items: Vec<HistoryItemResponse>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateRequest {
    pub memory_id: String,
    pub content: Option<String>,
    pub importance: Option<f64>,
    pub trust_tier: Option<String>,
    #[serde(default)]
    pub tags_add: Vec<String>,
    #[serde(default)]
    pub tags_remove: Vec<String>,
    pub reason: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct UpdateResponse {
    pub memory_id: String,
    #[serde(rename = "abstract")]
    pub abstract_text: String,
    pub updated_at: String,
    pub has_overview: bool,
    pub has_detail: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FocusRequest {
    #[serde(rename = "type")]
    pub focus_type: String,
    pub value: String,
    pub boost: Option<f64>,
    #[serde(default)]
    pub ttl_secs: Option<i64>,
}

impl FocusRequest {
    pub fn resolved_focus_type(&self) -> Result<String, String> {
        match self.focus_type.trim() {
            "topic" | "tag" | "memory_id" | "session" => Ok(self.focus_type.trim().to_string()),
            _ => Err("focus type must be 'topic', 'tag', 'memory_id', or 'session'".to_string()),
        }
    }

    pub fn resolved_value(&self) -> Result<String, String> {
        let value = self.value.trim();
        if value.is_empty() {
            Err("focus value must not be empty".to_string())
        } else {
            Ok(value.to_string())
        }
    }

    pub fn resolved_ttl_secs(&self) -> Result<Option<i64>, String> {
        Ok(self.ttl_secs)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        FocusRequest, ListQuery, RecallRequest, RecallResponseMode, RecallView, RememberRequest,
    };

    #[test]
    fn recall_request_expand_links_defaults_on() {
        let req: RecallRequest =
            serde_json::from_str(r#"{"query":"oauth","scope":"all"}"#).expect("parse recall");
        assert!(req.resolved_expand_links());
    }

    #[test]
    fn recall_request_expand_links_honors_explicit_false() {
        let req: RecallRequest = serde_json::from_str(r#"{"query":"oauth","expand_links":false}"#)
            .expect("parse recall");
        assert!(!req.resolved_expand_links());
    }

    #[test]
    fn recall_request_view_full_enables_verbose_response() {
        let req: RecallRequest =
            serde_json::from_str(r#"{"query":"oauth","view":"full"}"#).expect("parse recall");
        assert!(req.resolved_with_links().expect("with links"));
        assert_eq!(
            req.resolved_response_mode().expect("response mode"),
            RecallResponseMode::Verbose
        );
    }

    #[test]
    fn recall_request_defaults_to_compact_response() {
        let req: RecallRequest =
            serde_json::from_str(r#"{"query":"oauth"}"#).expect("parse recall");
        assert_eq!(req.resolved_view().expect("view"), RecallView::Compact);
        assert_eq!(
            req.resolved_response_mode().expect("response mode"),
            RecallResponseMode::CompactAbstract
        );
    }

    #[test]
    fn recall_request_view_overview_enables_compact_overview_response() {
        let req: RecallRequest =
            serde_json::from_str(r#"{"query":"oauth","view":"overview"}"#).expect("parse recall");
        assert_eq!(
            req.resolved_response_mode().expect("response mode"),
            RecallResponseMode::CompactOverview
        );
        assert!(req.resolved_with_overview().expect("with overview"));
        assert!(!req.resolved_with_links().expect("with links"));
    }

    #[test]
    fn recall_request_rejects_invalid_view() {
        let req: RecallRequest =
            serde_json::from_str(r#"{"query":"oauth","view":"ranking"}"#).expect("parse recall");
        assert_eq!(
            req.resolved_response_mode().unwrap_err(),
            "view must be 'compact', 'overview', or 'full'"
        );
    }

    #[test]
    fn recall_request_rejects_legacy_detail_field() {
        let err = serde_json::from_str::<RecallRequest>(r#"{"query":"oauth","detail":"full"}"#)
            .expect_err("legacy detail should fail");
        assert!(err.to_string().contains("unknown field `detail`"));
    }

    #[test]
    fn remember_request_rejects_legacy_memory_type_field() {
        let err = serde_json::from_str::<RememberRequest>(
            r#"{"content":"oauth","memory_type":"semantic"}"#,
        )
        .expect_err("legacy memory_type should fail");
        assert!(err.to_string().contains("unknown field `memory_type`"));
    }

    #[test]
    fn focus_request_rejects_legacy_ttl_alias() {
        let err = serde_json::from_str::<FocusRequest>(
            r#"{"type":"session","value":"sess-1","ttl":"1h"}"#,
        )
        .expect_err("legacy ttl should fail");
        assert!(err.to_string().contains("unknown field `ttl`"));
    }

    #[test]
    fn list_query_rejects_legacy_memory_type_param() {
        let err = serde_urlencoded::from_str::<ListQuery>("memory_type=semantic&limit=10")
            .expect_err("legacy list query should fail");
        assert!(err.to_string().contains("unknown field"));
    }
}

#[derive(Debug, Serialize)]
pub struct FocusResponse {
    pub focus_id: String,
    #[serde(rename = "type")]
    pub focus_type: String,
    pub value: String,
    pub boost: f64,
    pub active_until: String,
}

#[derive(Debug, Deserialize)]
pub struct FeedbackRequest {
    pub signal: String,
    pub context: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct FeedbackResponse {
    pub feedback_id: String,
    pub memory_id: String,
    pub signal: String,
}

#[derive(Debug, Serialize)]
pub struct MemoryFeedbackCountsResponse {
    pub useful: i32,
    pub irrelevant: i32,
    pub outdated: i32,
    pub wrong: i32,
}

#[derive(Debug, Serialize)]
pub struct MemoryFeedbackSummaryResponse {
    pub memory_id: String,
    pub feedback: MemoryFeedbackCountsResponse,
    pub last_feedback_at: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct FeedbackHistoryQuery {
    #[serde(default = "default_list_limit")]
    pub limit: i64,
}

#[derive(Debug, Serialize)]
pub struct FeedbackHistoryItemResponse {
    pub feedback_id: String,
    pub signal: String,
    pub context: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Serialize)]
pub struct FeedbackHistoryResponse {
    pub memory_id: String,
    pub items: Vec<FeedbackHistoryItemResponse>,
}

#[derive(Debug, Deserialize, Default)]
pub struct FeedbackFeedQuery {
    #[serde(default = "default_list_limit")]
    pub limit: i64,
    pub memory_id: Option<String>,
    pub signal: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct FeedbackFeedItemResponse {
    pub feedback_id: String,
    pub memory_id: String,
    #[serde(rename = "abstract", skip_serializing_if = "Option::is_none")]
    pub abstract_text: Option<String>,
    pub signal: String,
    pub context: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Serialize)]
pub struct FeedbackFeedResponse {
    pub items: Vec<FeedbackFeedItemResponse>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ListQuery {
    #[serde(default = "default_list_limit")]
    pub limit: i64,
    pub cursor: Option<String>,
    #[serde(rename = "type")]
    pub memory_type: Option<String>,
    pub session_id: Option<String>,
}

fn default_list_limit() -> i64 {
    50
}

#[derive(Debug, Serialize)]
pub struct ListItemResponse {
    pub id: String,
    #[serde(rename = "abstract")]
    pub abstract_text: String,
    #[serde(rename = "type")]
    pub memory_type: String,
    pub session_id: Option<String>,
    pub created_at: String,
    pub has_overview: bool,
    pub has_detail: bool,
}

#[derive(Debug, Serialize)]
pub struct ListResponse {
    pub items: Vec<ListItemResponse>,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct ProfileQuery {
    #[serde(default = "default_list_limit")]
    pub limit: i64,
    pub cursor: Option<String>,
    pub session_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ProfileItemResponse {
    pub id: String,
    pub content: String,
    #[serde(rename = "abstract")]
    pub abstract_text: String,
    pub session_id: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub trust_tier: String,
    pub confidence: f64,
    pub importance: f64,
    pub has_overview: bool,
    pub has_detail: bool,
}

#[derive(Debug, Serialize)]
pub struct ProfileResponse {
    pub items: Vec<ProfileItemResponse>,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ExtractEntitiesRequest {
    #[serde(default = "default_list_limit")]
    pub limit: i64,
    pub memory_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ExtractEntitiesResponse {
    pub processed_memories: i64,
    pub entities_found: i64,
    pub links_written: i64,
}

#[derive(Debug, Deserialize, Default)]
pub struct EntitiesQuery {
    #[serde(default = "default_list_limit")]
    pub limit: i64,
    pub cursor: Option<String>,
    pub query: Option<String>,
    pub entity_type: Option<String>,
    pub memory_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct EntityItemResponse {
    pub id: String,
    pub name: String,
    pub display_name: String,
    #[serde(rename = "type")]
    pub entity_type: String,
    pub memory_count: i64,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Serialize)]
pub struct EntitiesResponse {
    pub items: Vec<EntityItemResponse>,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ReflectRequest {
    #[serde(default)]
    pub force: bool,
    #[serde(default = "default_reflect_mode")]
    pub mode: String,
    #[serde(default = "default_list_limit")]
    pub limit: i64,
    pub session_id: Option<String>,
    #[serde(default = "default_reflect_min_cluster_size")]
    pub min_cluster_size: i64,
    #[serde(default = "default_reflect_min_link_strength")]
    pub min_link_strength: f64,
}

fn default_reflect_mode() -> String {
    "auto".to_string()
}

fn default_reflect_min_cluster_size() -> i64 {
    2
}

fn default_reflect_min_link_strength() -> f64 {
    0.35
}

#[derive(Debug, Serialize)]
pub struct ReflectMemoryItemResponse {
    pub id: String,
    #[serde(rename = "abstract")]
    pub abstract_text: String,
    #[serde(rename = "type")]
    pub memory_type: String,
    pub session_id: Option<String>,
    pub importance: f64,
}

#[derive(Debug, Serialize)]
pub struct ReflectCandidateResponse {
    pub signal: String,
    pub importance: f64,
    pub memory_count: i64,
    pub session_count: i64,
    pub link_count: i64,
    pub memories: Vec<ReflectMemoryItemResponse>,
}

#[derive(Debug, Serialize)]
pub struct ReflectResponse {
    pub mode: String,
    pub synthesized: bool,
    pub scenes_created: i64,
    pub candidates: Vec<ReflectCandidateResponse>,
}

#[derive(Debug, Deserialize, Default)]
pub struct TagsQuery {
    #[serde(default = "default_list_limit")]
    pub limit: i64,
    pub query: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct TagItemResponse {
    pub tag: String,
    pub memory_count: i64,
}

#[derive(Debug, Serialize)]
pub struct TagsResponse {
    pub items: Vec<TagItemResponse>,
}

#[derive(Debug, Deserialize)]
pub struct LinksQuery {
    pub memory_id: String,
    #[serde(default = "default_link_direction")]
    pub direction: String,
    #[serde(default = "default_list_limit")]
    pub limit: i64,
    pub link_type: Option<String>,
    #[serde(default)]
    pub min_strength: f64,
}

fn default_link_direction() -> String {
    "both".to_string()
}

#[derive(Debug, Serialize)]
pub struct LinkEvidenceDetailResponse {
    #[serde(rename = "type")]
    pub evidence_type: String,
    pub strength: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overlap_count: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_tag_count: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_tag_count: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vector_distance: Option<f64>,
}

#[derive(Debug, Serialize)]
pub struct LinkExtractionTraceResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_version_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub derivation_state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest_job_status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest_job_attempts: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest_job_updated_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest_job_error: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct LinkProvenanceResponse {
    pub evidence_types: Vec<String>,
    pub primary_evidence_type: Option<String>,
    pub primary_evidence_strength: Option<f64>,
    pub refined: bool,
    pub evidence: Vec<LinkEvidenceDetailResponse>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extraction_trace: Option<LinkExtractionTraceResponse>,
}

#[derive(Debug, Serialize)]
pub struct LinkItemResponse {
    pub id: String,
    #[serde(rename = "abstract")]
    pub abstract_text: String,
    #[serde(rename = "type")]
    pub memory_type: String,
    pub session_id: Option<String>,
    pub has_overview: bool,
    pub has_detail: bool,
    pub link_type: String,
    pub strength: f64,
    pub direction: String,
    pub provenance: LinkProvenanceResponse,
}

#[derive(Debug, Serialize)]
pub struct LinkTypeSummaryResponse {
    #[serde(rename = "type")]
    pub link_type: String,
    pub outbound_count: i64,
    pub inbound_count: i64,
}

#[derive(Debug, Serialize)]
pub struct LinkSummaryResponse {
    pub outbound_count: i64,
    pub inbound_count: i64,
    pub total_count: i64,
    pub link_types: Vec<LinkTypeSummaryResponse>,
}

#[derive(Debug, Serialize)]
pub struct LinksResponse {
    pub memory_id: String,
    pub summary: LinkSummaryResponse,
    pub items: Vec<LinkItemResponse>,
}

#[derive(Debug, Deserialize)]
pub struct RelatedQuery {
    pub memory_id: String,
    #[serde(default = "default_list_limit")]
    pub limit: i64,
    #[serde(default)]
    pub min_strength: f64,
    #[serde(default = "default_max_hops")]
    pub max_hops: i64,
}

fn default_max_hops() -> i64 {
    1
}

#[derive(Debug, Serialize)]
pub struct RelatedItemResponse {
    pub id: String,
    #[serde(rename = "abstract")]
    pub abstract_text: String,
    #[serde(rename = "type")]
    pub memory_type: String,
    pub session_id: Option<String>,
    pub has_overview: bool,
    pub has_detail: bool,
    pub hop_distance: i64,
    pub strength: f64,
    pub via_memory_ids: Vec<String>,
    pub directions: Vec<String>,
    pub link_types: Vec<String>,
    pub lineage: Vec<RelatedLineageStepResponse>,
    pub supporting_path_count: i64,
    pub supporting_paths_truncated: bool,
    pub supporting_paths: Vec<RelatedPathResponse>,
    pub feedback_impact: FeedbackImpactResponse,
    pub ranking: RelatedRankingResponse,
}

#[derive(Debug, Serialize)]
pub struct RelatedLineageStepResponse {
    pub from_memory_id: String,
    pub to_memory_id: String,
    pub direction: String,
    pub link_type: String,
    pub strength: f64,
    pub provenance: LinkProvenanceResponse,
}

#[derive(Debug, Serialize)]
pub struct RelatedPathResponse {
    pub hop_distance: i64,
    pub strength: f64,
    pub via_memory_ids: Vec<String>,
    pub lineage: Vec<RelatedLineageStepResponse>,
    pub path_rank: i64,
    pub selected: bool,
    pub selection_reason: String,
}

#[derive(Debug, Serialize)]
pub struct RelatedFocusMatchResponse {
    #[serde(rename = "type")]
    pub focus_type: String,
    pub value: String,
    pub boost: f64,
}

#[derive(Debug, Serialize)]
pub struct RecallRankingResponse {
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
    pub focus_matches: Vec<RelatedFocusMatchResponse>,
    pub expansion_sources: Vec<RecallExpansionSourceResponse>,
}

#[derive(Debug, Serialize)]
pub struct RecallExpansionSourceResponse {
    pub seed_memory_id: String,
    pub seed_score: f64,
    pub link_type: String,
    pub link_strength: f64,
    pub bonus: f64,
}

#[derive(Debug, Serialize)]
pub struct RelatedRankingResponse {
    pub same_hop_score: f64,
    pub base_strength: f64,
    pub session_affinity_applied: bool,
    pub session_affinity_multiplier: f64,
    pub access_count: i32,
    pub access_multiplier: f64,
    pub feedback_multiplier: f64,
    pub content_multiplier: f64,
    pub focus_boost: f64,
    pub focus_matches: Vec<RelatedFocusMatchResponse>,
}

#[derive(Debug, Serialize)]
pub struct RelatedHopSummaryResponse {
    pub hop_distance: i64,
    pub count: i64,
}

#[derive(Debug, Serialize)]
pub struct RelatedLinkTypeSummaryResponse {
    #[serde(rename = "type")]
    pub link_type: String,
    pub count: i64,
}

#[derive(Debug, Serialize)]
pub struct RelatedSummaryResponse {
    pub discovered_count: i64,
    pub returned_count: i64,
    pub truncated: bool,
    pub by_hop: Vec<RelatedHopSummaryResponse>,
    pub link_types: Vec<RelatedLinkTypeSummaryResponse>,
}

#[derive(Debug, Serialize)]
pub struct RelatedResponse {
    pub memory_id: String,
    pub summary: RelatedSummaryResponse,
    pub items: Vec<RelatedItemResponse>,
}

#[derive(Debug, Deserialize)]
pub struct JobsQuery {
    pub memory_id: String,
    #[serde(default = "default_list_limit")]
    pub limit: i64,
}

#[derive(Debug, Serialize)]
pub struct JobItemResponse {
    pub id: String,
    #[serde(rename = "type")]
    pub job_type: String,
    pub status: String,
    pub attempts: i32,
    pub available_at: String,
    pub leased_until: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub last_error: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct JobTypeSummaryResponse {
    #[serde(rename = "type")]
    pub job_type: String,
    pub pending_count: i64,
    pub in_progress_count: i64,
    pub done_count: i64,
    pub failed_count: i64,
    pub latest_status: String,
    pub latest_error: Option<String>,
    pub latest_updated_at: String,
}

#[derive(Debug, Serialize)]
pub struct JobsResponse {
    pub memory_id: String,
    pub derivation_state: String,
    pub has_overview: bool,
    pub has_detail: bool,
    pub link_count: i64,
    pub pending_count: i64,
    pub in_progress_count: i64,
    pub done_count: i64,
    pub failed_count: i64,
    pub job_types: Vec<JobTypeSummaryResponse>,
    pub items: Vec<JobItemResponse>,
}

#[derive(Debug, Serialize)]
pub struct JobMetricsResponse {
    pub pending_count: i64,
    pub in_progress_count: i64,
    pub failed_count: i64,
    pub avg_processing_time_ms: f64,
    pub oldest_pending_age_secs: i64,
}

#[derive(Debug, Serialize)]
pub struct StatsByTypeResponse {
    #[serde(rename = "type")]
    pub memory_type: String,
    pub total_count: i64,
    pub active_count: i64,
    pub forgotten_count: i64,
}

#[derive(Debug, Serialize)]
pub struct TagStatsResponse {
    pub unique_count: i64,
    pub assignment_count: i64,
}

#[derive(Debug, Serialize)]
pub struct JobStatsResponse {
    pub total_count: i64,
    pub pending_count: i64,
    pub in_progress_count: i64,
    pub done_count: i64,
    pub failed_count: i64,
}

#[derive(Debug, Serialize)]
pub struct FeedbackStatsResponse {
    pub total: i64,
    pub useful: i64,
    pub irrelevant: i64,
    pub outdated: i64,
    pub wrong: i64,
}

#[derive(Debug, Serialize)]
pub struct StatsResponse {
    pub total_memories: i64,
    pub active_memories: i64,
    pub forgotten_memories: i64,
    pub distinct_sessions: i64,
    pub has_overview_count: i64,
    pub has_detail_count: i64,
    pub active_direct_links: i64,
    pub active_focus_count: i64,
    pub tags: TagStatsResponse,
    pub jobs: JobStatsResponse,
    pub feedback: FeedbackStatsResponse,
    pub by_type: Vec<StatsByTypeResponse>,
}
