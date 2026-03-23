use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde::{de::DeserializeOwned, Serialize};
use std::collections::HashSet;

use crate::{
    auth::AuthUser,
    models::{parse_memory_type, parse_trust_tier},
    state::AppState,
    v2::models::*,
};

type ApiResult<T> = Result<Json<T>, (StatusCode, String)>;
const V2_BATCH_LIMIT: usize = 100;

#[derive(Debug, Clone, Default)]
struct RecallExtraFilters {
    min_confidence: Option<f64>,
    min_importance: Option<f64>,
    exclude_memory_ids: Vec<String>,
    prefer_recent: Option<bool>,
    diversity_factor: Option<f64>,
}

const RECALL_FILTER_FETCH_MULTIPLIER: i64 = 3;

fn v2_store(state: &AppState) -> Result<memoria_storage::MemoryV2Store, (StatusCode, String)> {
    state
        .service
        .sql_store
        .as_ref()
        .map(|s| s.v2_store())
        .ok_or_else(|| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "SQL store not available".to_string(),
            )
        })
}

fn decode_cursor<T: DeserializeOwned>(encoded: &str) -> Result<T, String> {
    let bytes = URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|e| format!("invalid cursor: {e}"))?;
    serde_json::from_slice(&bytes).map_err(|e| format!("invalid cursor: {e}"))
}

fn encode_cursor<T: Serialize>(cursor: &T) -> String {
    URL_SAFE_NO_PAD.encode(serde_json::to_vec(cursor).unwrap_or_default())
}

fn normalize_history_payload(mut payload: serde_json::Value) -> serde_json::Value {
    if let Some(object) = payload.as_object_mut() {
        if let Some(value) = object.remove("memory_type") {
            object.entry("type".to_string()).or_insert(value);
        }
    }
    payload
}

fn parse_expand_level(level: &str) -> Result<memoria_storage::ExpandLevel, (StatusCode, String)> {
    match level {
        "overview" => Ok(memoria_storage::ExpandLevel::Overview),
        "detail" => Ok(memoria_storage::ExpandLevel::Detail),
        "links" => Ok(memoria_storage::ExpandLevel::Links),
        _ => Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid expand level".into(),
        )),
    }
}

fn parse_link_direction(
    direction: &str,
) -> Result<memoria_storage::LinkDirection, (StatusCode, String)> {
    match direction {
        "outbound" => Ok(memoria_storage::LinkDirection::Outbound),
        "inbound" => Ok(memoria_storage::LinkDirection::Inbound),
        "both" => Ok(memoria_storage::LinkDirection::Both),
        _ => Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            "direction must be 'outbound', 'inbound', or 'both'".into(),
        )),
    }
}

fn map_link(link: memoria_storage::LinkV2Ref) -> RecallLinkResponse {
    RecallLinkResponse {
        memory_id: link.memory_id,
        abstract_text: link.abstract_text,
        link_type: link.link_type,
        strength: link.strength,
        provenance: map_link_provenance(link.provenance),
    }
}

fn map_feedback_impact(impact: memoria_storage::MemoryV2FeedbackImpact) -> FeedbackImpactResponse {
    FeedbackImpactResponse {
        useful: impact.counts.useful,
        irrelevant: impact.counts.irrelevant,
        outdated: impact.counts.outdated,
        wrong: impact.counts.wrong,
        multiplier: impact.multiplier,
    }
}

fn map_link_item(link: memoria_storage::MemoryV2LinkItem) -> LinkItemResponse {
    LinkItemResponse {
        id: link.memory_id,
        abstract_text: link.abstract_text,
        memory_type: link.memory_type.to_string(),
        session_id: link.session_id,
        has_overview: link.has_overview,
        has_detail: link.has_detail,
        link_type: link.link_type,
        strength: link.strength,
        direction: link.direction.as_str().to_string(),
        provenance: map_link_provenance(link.provenance),
    }
}

fn map_link_provenance(
    provenance: memoria_storage::MemoryV2LinkProvenance,
) -> LinkProvenanceResponse {
    LinkProvenanceResponse {
        evidence_types: provenance.evidence_types,
        primary_evidence_type: provenance.primary_evidence_type,
        primary_evidence_strength: provenance.primary_evidence_strength,
        refined: provenance.refined,
        evidence: provenance
            .evidence
            .into_iter()
            .map(|detail| LinkEvidenceDetailResponse {
                evidence_type: detail.evidence_type,
                strength: detail.strength,
                overlap_count: detail.overlap_count,
                source_tag_count: detail.source_tag_count,
                target_tag_count: detail.target_tag_count,
                vector_distance: detail.vector_distance,
            })
            .collect(),
        extraction_trace: provenance
            .extraction_trace
            .map(|trace| LinkExtractionTraceResponse {
                content_version_id: trace.content_version_id,
                derivation_state: trace.derivation_state,
                latest_job_status: trace.latest_job_status,
                latest_job_attempts: trace.latest_job_attempts,
                latest_job_updated_at: trace.latest_job_updated_at.map(|ts| ts.to_rfc3339()),
                latest_job_error: trace.latest_job_error,
            }),
    }
}

fn map_related_lineage_step(
    step: memoria_storage::MemoryV2RelatedLineageStep,
) -> RelatedLineageStepResponse {
    RelatedLineageStepResponse {
        from_memory_id: step.from_memory_id,
        to_memory_id: step.to_memory_id,
        direction: step.direction.as_str().to_string(),
        link_type: step.link_type,
        strength: step.strength,
        provenance: map_link_provenance(step.provenance),
    }
}

fn map_related_path(path: memoria_storage::MemoryV2RelatedPath) -> RelatedPathResponse {
    RelatedPathResponse {
        hop_distance: path.hop_distance,
        strength: path.strength,
        via_memory_ids: path.via_memory_ids,
        lineage: path
            .lineage
            .into_iter()
            .map(map_related_lineage_step)
            .collect(),
        path_rank: path.path_rank,
        selected: path.selected,
        selection_reason: path.selection_reason,
    }
}

fn map_related_ranking(ranking: memoria_storage::MemoryV2RelatedRanking) -> RelatedRankingResponse {
    RelatedRankingResponse {
        same_hop_score: ranking.same_hop_score,
        base_strength: ranking.base_strength,
        session_affinity_applied: ranking.session_affinity_applied,
        session_affinity_multiplier: ranking.session_affinity_multiplier,
        access_count: ranking.access_count,
        access_multiplier: ranking.access_multiplier,
        feedback_multiplier: ranking.feedback_multiplier,
        content_multiplier: ranking.content_multiplier,
        focus_boost: ranking.focus_boost,
        focus_matches: ranking
            .focus_matches
            .into_iter()
            .map(|focus| RelatedFocusMatchResponse {
                focus_type: focus.focus_type,
                value: focus.value,
                boost: focus.boost,
            })
            .collect(),
    }
}

fn map_recall_summary(summary: memoria_storage::MemoryV2RecallSummary) -> RecallSummaryResponse {
    RecallSummaryResponse {
        discovered_count: summary.discovered_count,
        returned_count: summary.returned_count,
        truncated: summary.truncated,
        by_retrieval_path: summary
            .by_retrieval_path
            .into_iter()
            .map(|bucket| RecallPathSummaryResponse {
                retrieval_path: bucket.retrieval_path.as_str().to_string(),
                discovered_count: bucket.discovered_count,
                returned_count: bucket.returned_count,
            })
            .collect(),
    }
}

fn map_recall_ranking(ranking: memoria_storage::MemoryV2RecallRanking) -> RecallRankingResponse {
    RecallRankingResponse {
        final_score: ranking.final_score,
        base_score: ranking.base_score,
        vector_component: ranking.vector_component,
        keyword_component: ranking.keyword_component,
        confidence_component: ranking.confidence_component,
        importance_component: ranking.importance_component,
        entity_component: ranking.entity_component,
        link_bonus: ranking.link_bonus,
        linked_expansion_applied: ranking.linked_expansion_applied,
        temporal_decay_applied: ranking.temporal_decay_applied,
        age_hours: ranking.age_hours,
        temporal_half_life_hours: ranking.temporal_half_life_hours,
        temporal_multiplier: ranking.temporal_multiplier,
        session_affinity_applied: ranking.session_affinity_applied,
        session_affinity_multiplier: ranking.session_affinity_multiplier,
        access_count: ranking.access_count,
        access_multiplier: ranking.access_multiplier,
        feedback_multiplier: ranking.feedback_multiplier,
        focus_boost: ranking.focus_boost,
        type_affinity_boost: ranking.type_affinity_boost,
        focus_matches: ranking
            .focus_matches
            .into_iter()
            .map(|focus| RelatedFocusMatchResponse {
                focus_type: focus.focus_type,
                value: focus.value,
                boost: focus.boost,
            })
            .collect(),
        expansion_sources: ranking
            .expansion_sources
            .into_iter()
            .map(|source| RecallExpansionSourceResponse {
                seed_memory_id: source.seed_memory_id,
                seed_score: source.seed_score,
                link_type: source.link_type,
                link_strength: source.link_strength,
                bonus: source.bonus,
            })
            .collect(),
    }
}

fn map_remembered(remembered: memoria_storage::MemoryV2RememberResult) -> RememberResponse {
    RememberResponse {
        memory_id: remembered.memory_id,
        abstract_text: remembered.abstract_text,
        has_overview: remembered.has_overview,
        has_detail: remembered.has_detail,
    }
}

fn dedupe_memory_ids(memory_ids: Vec<String>) -> Result<Vec<String>, (StatusCode, String)> {
    let mut seen = HashSet::new();
    let mut unique = Vec::new();
    for memory_id in memory_ids {
        let memory_id = memory_id.trim();
        if memory_id.is_empty() {
            return Err((
                StatusCode::UNPROCESSABLE_ENTITY,
                "memory_id must not be empty".into(),
            ));
        }
        if seen.insert(memory_id.to_string()) {
            unique.push(memory_id.to_string());
        }
    }
    Ok(unique)
}

fn parse_recall_extra_filters(
    req: &RecallRequest,
) -> Result<RecallExtraFilters, (StatusCode, String)> {
    Ok(RecallExtraFilters {
        min_confidence: req
            .resolved_min_confidence()
            .map_err(|e| (StatusCode::UNPROCESSABLE_ENTITY, e))?,
        min_importance: req
            .resolved_min_importance()
            .map_err(|e| (StatusCode::UNPROCESSABLE_ENTITY, e))?,
        exclude_memory_ids: req.resolved_exclude_memory_ids(),
        prefer_recent: req.prefer_recent,
        diversity_factor: req
            .resolved_diversity_factor()
            .map_err(|e| (StatusCode::UNPROCESSABLE_ENTITY, e))?,
    })
}

fn apply_recall_extra_filters(
    mut result: memoria_storage::MemoryV2RecallResult,
    filters: &RecallExtraFilters,
) -> memoria_storage::MemoryV2RecallResult {
    if let Some(min_confidence) = filters.min_confidence {
        result.memories.retain(|m| m.confidence >= min_confidence);
    }
    if let Some(min_importance) = filters.min_importance {
        result
            .memories
            .retain(|m| m.ranking.importance_component >= (0.05 * min_importance));
    }
    if !filters.exclude_memory_ids.is_empty() {
        let excluded: HashSet<&str> = filters
            .exclude_memory_ids
            .iter()
            .map(String::as_str)
            .collect();
        result
            .memories
            .retain(|m| !excluded.contains(m.memory_id.as_str()));
    }
    if filters.prefer_recent.unwrap_or(false) {
        result.memories.sort_by(|a, b| {
            a.ranking
                .age_hours
                .partial_cmp(&b.ranking.age_hours)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    }
    if let Some(diversity) = filters.diversity_factor {
        if diversity > 0.0 && result.memories.len() > 2 {
            let mut by_type = std::collections::HashMap::<String, usize>::new();
            result.memories.sort_by(|a, b| {
                b.score
                    .partial_cmp(&a.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            let mut diversified = Vec::with_capacity(result.memories.len());
            for item in result.memories {
                let t = item.memory_type.to_string();
                let seen = by_type.get(&t).copied().unwrap_or(0);
                let allow = seen == 0 || ((seen as f64) < (1.0 / diversity.max(0.1)));
                if allow {
                    *by_type.entry(t).or_insert(0) += 1;
                    diversified.push(item);
                }
            }
            result.memories = diversified;
        }
    }
    result.summary.returned_count = result.memories.len() as i64;
    result
}

fn has_active_recall_extra_filters(filters: &RecallExtraFilters) -> bool {
    filters.min_confidence.is_some()
        || filters.min_importance.is_some()
        || !filters.exclude_memory_ids.is_empty()
        || filters.prefer_recent.unwrap_or(false)
        || filters.diversity_factor.unwrap_or(0.0) > 0.0
}

fn recall_top_k_for_store(top_k: i64, filters: &RecallExtraFilters) -> i64 {
    if has_active_recall_extra_filters(filters) {
        (top_k.saturating_mul(RECALL_FILTER_FETCH_MULTIPLIER)).clamp(1, 200)
    } else {
        top_k
    }
}

fn finalize_filtered_recall(
    mut result: memoria_storage::MemoryV2RecallResult,
    requested_top_k: i64,
) -> memoria_storage::MemoryV2RecallResult {
    let limit = requested_top_k.clamp(1, 200) as usize;
    let truncated = result.memories.len() > limit;
    if truncated {
        result.memories.truncate(limit);
    }
    result.has_more = result.has_more || truncated;
    result.summary.returned_count = result.memories.len() as i64;
    result
}

pub async fn remember(
    State(state): State<AppState>,
    AuthUser { user_id, .. }: AuthUser,
    Json(req): Json<RememberRequest>,
) -> Result<(StatusCode, Json<RememberResponse>), (StatusCode, String)> {
    if req.content.trim().is_empty() {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            "content must not be empty".into(),
        ));
    }
    let store = v2_store(&state)?;
    let memory_type =
        parse_memory_type(&req.memory_type).map_err(|e| (StatusCode::UNPROCESSABLE_ENTITY, e))?;
    let trust_tier = req
        .trust_tier
        .as_deref()
        .map(parse_trust_tier)
        .transpose()
        .map_err(|e| (StatusCode::UNPROCESSABLE_ENTITY, e))?;
    let embedding = state
        .service
        .embed(&req.content)
        .await
        .map_err(crate::routes::memory::api_err_typed)?;
    let remembered = store
        .remember_with_options(
            &user_id,
            memoria_storage::MemoryV2RememberInput {
                content: req.content,
                memory_type,
                session_id: req.session_id,
                importance: req.importance,
                trust_tier,
                tags: req.tags,
                source: req.source,
                embedding,
                actor: user_id.clone(),
            },
            memoria_storage::RememberV2Options {
                sync_enrich: req.sync_enrich,
                enrich_timeout_secs: req.enrich_timeout_secs,
            },
        )
        .await
        .map_err(|e| match e {
            memoria_core::MemoriaError::Blocked(message)
                if message.contains("sync enrichment timeout") =>
            {
                (StatusCode::CONFLICT, message)
            }
            other => crate::routes::memory::api_err_typed(other),
        })?;
    Ok((StatusCode::CREATED, Json(map_remembered(remembered))))
}

pub async fn batch_remember(
    State(state): State<AppState>,
    AuthUser { user_id, .. }: AuthUser,
    Json(req): Json<BatchRememberRequest>,
) -> Result<(StatusCode, Json<BatchRememberResponse>), (StatusCode, String)> {
    if req.memories.len() > V2_BATCH_LIMIT {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("batch exceeds {V2_BATCH_LIMIT} items"),
        ));
    }
    let store = v2_store(&state)?;
    let mut contents = Vec::with_capacity(req.memories.len());
    let mut inputs = Vec::with_capacity(req.memories.len());
    for req in req.memories {
        if req.content.trim().is_empty() {
            return Err((
                StatusCode::UNPROCESSABLE_ENTITY,
                "content must not be empty".into(),
            ));
        }
        if req.content.len() > 32_768 {
            return Err((
                StatusCode::UNPROCESSABLE_ENTITY,
                "content exceeds 32 KiB limit".into(),
            ));
        }
        let memory_type = parse_memory_type(&req.memory_type)
            .map_err(|e| (StatusCode::UNPROCESSABLE_ENTITY, e))?;
        let trust_tier = req
            .trust_tier
            .as_deref()
            .map(parse_trust_tier)
            .transpose()
            .map_err(|e| (StatusCode::UNPROCESSABLE_ENTITY, e))?;
        contents.push(req.content.clone());
        inputs.push((
            req.content,
            memory_type,
            req.session_id,
            req.importance,
            trust_tier,
            req.tags,
            req.source,
        ));
    }

    let embeddings = state
        .service
        .embed_batch(&contents)
        .await
        .map_err(crate::routes::memory::api_err_typed)?;
    if let Some(ref embeddings) = embeddings {
        if embeddings.len() != inputs.len() {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                "batch embedding count mismatch".into(),
            ));
        }
    }

    let remembered = store
        .remember_batch(
            &user_id,
            inputs
                .into_iter()
                .enumerate()
                .map(
                    |(
                        idx,
                        (content, memory_type, session_id, importance, trust_tier, tags, source),
                    )| {
                        memoria_storage::MemoryV2RememberInput {
                            content,
                            memory_type,
                            session_id,
                            importance,
                            trust_tier,
                            tags,
                            source,
                            embedding: embeddings.as_ref().map(|items| items[idx].clone()),
                            actor: user_id.clone(),
                        }
                    },
                )
                .collect(),
        )
        .await
        .map_err(crate::routes::memory::api_err_typed)?;
    Ok((
        StatusCode::CREATED,
        Json(BatchRememberResponse {
            memories: remembered.into_iter().map(map_remembered).collect(),
        }),
    ))
}

pub async fn list(
    State(state): State<AppState>,
    AuthUser { user_id, .. }: AuthUser,
    Query(q): Query<ListQuery>,
) -> ApiResult<ListResponse> {
    let store = v2_store(&state)?;
    let memory_type = q
        .memory_type
        .as_deref()
        .map(parse_memory_type)
        .transpose()
        .map_err(|e| (StatusCode::UNPROCESSABLE_ENTITY, e))?;
    let cursor = q
        .cursor
        .as_deref()
        .map(decode_cursor)
        .transpose()
        .map_err(|e| (StatusCode::UNPROCESSABLE_ENTITY, e))?;
    let result = store
        .list(
            &user_id,
            memoria_storage::ListV2Filter {
                limit: q.limit,
                cursor,
                memory_type,
                session_id: q.session_id,
            },
        )
        .await
        .map_err(crate::routes::memory::api_err_typed)?;
    Ok(Json(ListResponse {
        items: result
            .items
            .into_iter()
            .map(|item| ListItemResponse {
                id: item.memory_id,
                abstract_text: item.abstract_text,
                memory_type: item.memory_type.to_string(),
                session_id: item.session_id,
                created_at: item.created_at.to_rfc3339(),
                has_overview: item.has_overview,
                has_detail: item.has_detail,
            })
            .collect(),
        next_cursor: result.next_cursor.as_ref().map(encode_cursor),
    }))
}

pub async fn profile(
    State(state): State<AppState>,
    AuthUser { user_id, .. }: AuthUser,
    Query(q): Query<ProfileQuery>,
) -> ApiResult<ProfileResponse> {
    let store = v2_store(&state)?;
    let cursor = q
        .cursor
        .as_deref()
        .map(decode_cursor)
        .transpose()
        .map_err(|e| (StatusCode::UNPROCESSABLE_ENTITY, e))?;
    let result = store
        .profile(
            &user_id,
            memoria_storage::ProfileV2Filter {
                limit: q.limit,
                cursor,
                session_id: q.session_id,
            },
        )
        .await
        .map_err(crate::routes::memory::api_err_typed)?;
    Ok(Json(ProfileResponse {
        items: result
            .items
            .into_iter()
            .map(|item| ProfileItemResponse {
                id: item.memory_id,
                content: item.content,
                abstract_text: item.abstract_text,
                session_id: item.session_id,
                created_at: item.created_at.to_rfc3339(),
                updated_at: item.updated_at.to_rfc3339(),
                trust_tier: item.trust_tier.to_string(),
                confidence: item.confidence,
                importance: item.importance,
                has_overview: item.has_overview,
                has_detail: item.has_detail,
            })
            .collect(),
        next_cursor: result.next_cursor.as_ref().map(encode_cursor),
    }))
}

pub async fn extract_entities(
    State(state): State<AppState>,
    AuthUser { user_id, .. }: AuthUser,
    Json(req): Json<ExtractEntitiesRequest>,
) -> ApiResult<ExtractEntitiesResponse> {
    let store = v2_store(&state)?;
    let result = store
        .extract_entities(&user_id, req.limit, req.memory_id.as_deref())
        .await
        .map_err(crate::routes::memory::api_err_typed)?;
    Ok(Json(ExtractEntitiesResponse {
        processed_memories: result.processed_memories,
        entities_found: result.entities_found,
        links_written: result.links_written,
    }))
}

pub async fn reflect(
    State(state): State<AppState>,
    AuthUser { user_id, .. }: AuthUser,
    Json(req): Json<ReflectRequest>,
) -> ApiResult<ReflectResponse> {
    let store = v2_store(&state)?;
    let mode = req.mode.trim();
    if !mode.is_empty() && mode != "auto" && mode != "candidates" && mode != "internal" {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            "mode must be 'auto', 'candidates', or 'internal'".into(),
        ));
    }
    let result = store
        .reflect(
            &user_id,
            memoria_storage::ReflectV2Filter {
                limit: req.limit,
                mode: if mode.is_empty() {
                    "auto".to_string()
                } else {
                    mode.to_string()
                },
                session_id: req.session_id,
                min_cluster_size: req.min_cluster_size,
                min_link_strength: req.min_link_strength,
            },
        )
        .await
        .map_err(crate::routes::memory::api_err_typed)?;
    Ok(Json(ReflectResponse {
        mode: result.mode,
        synthesized: result.synthesized,
        scenes_created: result.scenes_created,
        candidates: result
            .candidates
            .into_iter()
            .map(|candidate| ReflectCandidateResponse {
                signal: candidate.signal,
                importance: candidate.importance,
                memory_count: candidate.memory_count,
                session_count: candidate.session_count,
                link_count: candidate.link_count,
                memories: candidate
                    .memories
                    .into_iter()
                    .map(|memory| ReflectMemoryItemResponse {
                        id: memory.memory_id,
                        abstract_text: memory.abstract_text,
                        memory_type: memory.memory_type.to_string(),
                        session_id: memory.session_id,
                        importance: memory.importance,
                    })
                    .collect(),
            })
            .collect(),
    }))
}

pub async fn list_entities(
    State(state): State<AppState>,
    AuthUser { user_id, .. }: AuthUser,
    Query(q): Query<EntitiesQuery>,
) -> ApiResult<EntitiesResponse> {
    let store = v2_store(&state)?;
    let cursor = q
        .cursor
        .as_deref()
        .map(decode_cursor)
        .transpose()
        .map_err(|e| (StatusCode::UNPROCESSABLE_ENTITY, e))?;
    let result = store
        .list_entities(
            &user_id,
            memoria_storage::EntityV2Filter {
                limit: q.limit,
                cursor,
                query: q.query,
                entity_type: q.entity_type,
                memory_id: q.memory_id,
            },
        )
        .await
        .map_err(crate::routes::memory::api_err_typed)?;
    Ok(Json(EntitiesResponse {
        items: result
            .items
            .into_iter()
            .map(|item| EntityItemResponse {
                id: item.entity_id,
                name: item.name,
                display_name: item.display_name,
                entity_type: item.entity_type,
                memory_count: item.memory_count,
                created_at: item.created_at.to_rfc3339(),
                updated_at: item.updated_at.to_rfc3339(),
            })
            .collect(),
        next_cursor: result.next_cursor.as_ref().map(encode_cursor),
    }))
}

pub async fn list_tags(
    State(state): State<AppState>,
    AuthUser { user_id, .. }: AuthUser,
    Query(q): Query<TagsQuery>,
) -> ApiResult<TagsResponse> {
    let store = v2_store(&state)?;
    let items = store
        .list_tags(&user_id, q.limit, q.query.as_deref())
        .await
        .map_err(crate::routes::memory::api_err_typed)?;
    Ok(Json(TagsResponse {
        items: items
            .into_iter()
            .map(|item| TagItemResponse {
                tag: item.tag,
                memory_count: item.memory_count,
            })
            .collect(),
    }))
}

pub async fn jobs(
    State(state): State<AppState>,
    AuthUser { user_id, .. }: AuthUser,
    Query(q): Query<JobsQuery>,
) -> ApiResult<JobsResponse> {
    let store = v2_store(&state)?;
    let result = store
        .jobs(
            &user_id,
            memoria_storage::MemoryV2JobsRequest {
                memory_id: q.memory_id,
                limit: q.limit,
            },
        )
        .await
        .map_err(crate::routes::memory::api_err_typed)?;
    Ok(Json(JobsResponse {
        memory_id: result.memory_id,
        derivation_state: result.derivation_state,
        has_overview: result.has_overview,
        has_detail: result.has_detail,
        link_count: result.link_count,
        pending_count: result.pending_count,
        in_progress_count: result.in_progress_count,
        done_count: result.done_count,
        failed_count: result.failed_count,
        job_types: result
            .job_types
            .into_iter()
            .map(|item| JobTypeSummaryResponse {
                job_type: item.job_type,
                pending_count: item.pending_count,
                in_progress_count: item.in_progress_count,
                done_count: item.done_count,
                failed_count: item.failed_count,
                latest_status: item.latest_status,
                latest_error: item.latest_error,
                latest_updated_at: item.latest_updated_at.to_rfc3339(),
            })
            .collect(),
        items: result
            .items
            .into_iter()
            .map(|item| JobItemResponse {
                id: item.job_id,
                job_type: item.job_type,
                status: item.status,
                attempts: item.attempts,
                available_at: item.available_at.to_rfc3339(),
                leased_until: item.leased_until.map(|ts| ts.to_rfc3339()),
                created_at: item.created_at.to_rfc3339(),
                updated_at: item.updated_at.to_rfc3339(),
                last_error: item.last_error,
            })
            .collect(),
    }))
}

pub async fn history(
    State(state): State<AppState>,
    AuthUser { user_id, .. }: AuthUser,
    Path(memory_id): Path<String>,
    Query(q): Query<HistoryQuery>,
) -> ApiResult<HistoryResponse> {
    let store = v2_store(&state)?;
    let result = store
        .memory_history(&user_id, &memory_id, q.limit)
        .await
        .map_err(crate::routes::memory::api_err_typed)?;
    Ok(Json(HistoryResponse {
        memory_id: result.memory_id,
        items: result
            .items
            .into_iter()
            .map(|item| HistoryItemResponse {
                event_id: item.event_id,
                event_type: item.event_type,
                actor: item.actor,
                processing_state: item.processing_state,
                payload: normalize_history_payload(item.payload),
                created_at: item.created_at.to_rfc3339(),
            })
            .collect(),
    }))
}

pub async fn stats(
    State(state): State<AppState>,
    AuthUser { user_id, .. }: AuthUser,
) -> ApiResult<StatsResponse> {
    let store = v2_store(&state)?;
    let result = store
        .stats(&user_id)
        .await
        .map_err(crate::routes::memory::api_err_typed)?;
    Ok(Json(StatsResponse {
        total_memories: result.total_memories,
        active_memories: result.active_memories,
        forgotten_memories: result.forgotten_memories,
        distinct_sessions: result.distinct_sessions,
        has_overview_count: result.has_overview_count,
        has_detail_count: result.has_detail_count,
        active_direct_links: result.active_direct_links,
        active_focus_count: result.active_focus_count,
        tags: TagStatsResponse {
            unique_count: result.tags.unique_count,
            assignment_count: result.tags.assignment_count,
        },
        jobs: JobStatsResponse {
            total_count: result.jobs.total_count,
            pending_count: result.jobs.pending_count,
            in_progress_count: result.jobs.in_progress_count,
            done_count: result.jobs.done_count,
            failed_count: result.jobs.failed_count,
        },
        feedback: FeedbackStatsResponse {
            total: result.feedback.total,
            useful: result.feedback.useful,
            irrelevant: result.feedback.irrelevant,
            outdated: result.feedback.outdated,
            wrong: result.feedback.wrong,
        },
        by_type: result
            .by_type
            .into_iter()
            .map(|item| StatsByTypeResponse {
                memory_type: item.memory_type,
                total_count: item.total_count,
                active_count: item.active_count,
                forgotten_count: item.forgotten_count,
            })
            .collect(),
    }))
}

pub async fn links(
    State(state): State<AppState>,
    AuthUser { user_id, .. }: AuthUser,
    Query(q): Query<LinksQuery>,
) -> ApiResult<LinksResponse> {
    let store = v2_store(&state)?;
    let direction = parse_link_direction(&q.direction)?;
    let result = store
        .links(
            &user_id,
            memoria_storage::MemoryV2LinksRequest {
                memory_id: q.memory_id,
                direction,
                limit: q.limit,
                link_type: q.link_type,
                min_strength: q.min_strength,
            },
        )
        .await
        .map_err(crate::routes::memory::api_err_typed)?;
    Ok(Json(LinksResponse {
        memory_id: result.memory_id,
        summary: LinkSummaryResponse {
            outbound_count: result.summary.outbound_count,
            inbound_count: result.summary.inbound_count,
            total_count: result.summary.total_count,
            link_types: result
                .summary
                .link_types
                .into_iter()
                .map(|item| LinkTypeSummaryResponse {
                    link_type: item.link_type,
                    outbound_count: item.outbound_count,
                    inbound_count: item.inbound_count,
                })
                .collect(),
        },
        items: result.items.into_iter().map(map_link_item).collect(),
    }))
}

pub async fn related(
    State(state): State<AppState>,
    AuthUser { user_id, .. }: AuthUser,
    Query(q): Query<RelatedQuery>,
) -> ApiResult<RelatedResponse> {
    let store = v2_store(&state)?;
    let result = store
        .related(
            &user_id,
            memoria_storage::MemoryV2RelatedRequest {
                memory_id: q.memory_id,
                limit: q.limit,
                min_strength: q.min_strength,
                max_hops: q.max_hops,
            },
        )
        .await
        .map_err(crate::routes::memory::api_err_typed)?;
    Ok(Json(RelatedResponse {
        memory_id: result.memory_id,
        summary: RelatedSummaryResponse {
            discovered_count: result.summary.discovered_count,
            returned_count: result.summary.returned_count,
            truncated: result.summary.truncated,
            by_hop: result
                .summary
                .by_hop
                .into_iter()
                .map(|item| RelatedHopSummaryResponse {
                    hop_distance: item.hop_distance,
                    count: item.count,
                })
                .collect(),
            link_types: result
                .summary
                .link_types
                .into_iter()
                .map(|item| RelatedLinkTypeSummaryResponse {
                    link_type: item.link_type,
                    count: item.count,
                })
                .collect(),
        },
        items: result
            .items
            .into_iter()
            .map(|item| RelatedItemResponse {
                id: item.memory_id,
                abstract_text: item.abstract_text,
                memory_type: item.memory_type.to_string(),
                session_id: item.session_id,
                has_overview: item.has_overview,
                has_detail: item.has_detail,
                hop_distance: item.hop_distance,
                strength: item.strength,
                via_memory_ids: item.via_memory_ids,
                directions: item
                    .directions
                    .into_iter()
                    .map(|direction| direction.as_str().to_string())
                    .collect(),
                link_types: item.link_types,
                lineage: item
                    .lineage
                    .into_iter()
                    .map(map_related_lineage_step)
                    .collect(),
                supporting_path_count: item.supporting_path_count,
                supporting_paths_truncated: item.supporting_paths_truncated,
                supporting_paths: item
                    .supporting_paths
                    .into_iter()
                    .map(map_related_path)
                    .collect(),
                feedback_impact: map_feedback_impact(item.feedback_impact),
                ranking: map_related_ranking(item.ranking),
            })
            .collect(),
    }))
}

pub async fn forget(
    State(state): State<AppState>,
    AuthUser { user_id, .. }: AuthUser,
    Json(req): Json<ForgetRequest>,
) -> ApiResult<ForgetResponse> {
    let store = v2_store(&state)?;
    store
        .forget(&user_id, &req.memory_id, req.reason.as_deref(), &user_id)
        .await
        .map_err(crate::routes::memory::api_err_typed)?;
    Ok(Json(ForgetResponse {
        memory_id: req.memory_id,
        forgotten: true,
    }))
}

pub async fn batch_forget(
    State(state): State<AppState>,
    AuthUser { user_id, .. }: AuthUser,
    Json(req): Json<BatchForgetRequest>,
) -> ApiResult<BatchForgetResponse> {
    if req.memory_ids.len() > V2_BATCH_LIMIT {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("batch exceeds {V2_BATCH_LIMIT} items"),
        ));
    }
    let store = v2_store(&state)?;
    let memory_ids = dedupe_memory_ids(req.memory_ids)?;
    let forgotten = store
        .forget_batch(&user_id, &memory_ids, req.reason.as_deref(), &user_id)
        .await
        .map_err(crate::routes::memory::api_err_typed)?;
    Ok(Json(BatchForgetResponse {
        memories: forgotten
            .into_iter()
            .map(|memory_id| ForgetResponse {
                memory_id,
                forgotten: true,
            })
            .collect(),
    }))
}

pub async fn update(
    State(state): State<AppState>,
    AuthUser { user_id, .. }: AuthUser,
    Json(req): Json<UpdateRequest>,
) -> ApiResult<UpdateResponse> {
    let store = v2_store(&state)?;
    let trust_tier = req
        .trust_tier
        .as_deref()
        .map(parse_trust_tier)
        .transpose()
        .map_err(|e| (StatusCode::UNPROCESSABLE_ENTITY, e))?;
    let embedding = match req.content.as_deref() {
        Some(content) => state
            .service
            .embed(content)
            .await
            .map_err(crate::routes::memory::api_err_typed)?,
        None => None,
    };
    let updated = store
        .update(
            &user_id,
            memoria_storage::MemoryV2UpdateInput {
                memory_id: req.memory_id,
                content: req.content,
                importance: req.importance,
                trust_tier,
                tags_add: req.tags_add,
                tags_remove: req.tags_remove,
                embedding,
                actor: user_id.clone(),
                reason: req.reason,
            },
        )
        .await
        .map_err(crate::routes::memory::api_err_typed)?;
    Ok(Json(UpdateResponse {
        memory_id: updated.memory_id,
        abstract_text: updated.abstract_text,
        updated_at: updated.updated_at.to_rfc3339(),
        has_overview: updated.has_overview,
        has_detail: updated.has_detail,
    }))
}

pub async fn focus(
    State(state): State<AppState>,
    AuthUser { user_id, .. }: AuthUser,
    Json(req): Json<FocusRequest>,
) -> Result<(StatusCode, Json<FocusResponse>), (StatusCode, String)> {
    let store = v2_store(&state)?;
    let focus_type = req
        .resolved_focus_type()
        .map_err(|e| (StatusCode::UNPROCESSABLE_ENTITY, e))?;
    let value = req
        .resolved_value()
        .map_err(|e| (StatusCode::UNPROCESSABLE_ENTITY, e))?;
    let ttl_secs = req
        .resolved_ttl_secs()
        .map_err(|e| (StatusCode::UNPROCESSABLE_ENTITY, e))?;
    let focused = store
        .focus(
            &user_id,
            memoria_storage::FocusV2Input {
                focus_type,
                value,
                boost: req.boost,
                ttl_secs,
                actor: user_id.clone(),
            },
        )
        .await
        .map_err(crate::routes::memory::api_err_typed)?;
    Ok((
        StatusCode::CREATED,
        Json(FocusResponse {
            focus_id: focused.focus_id,
            focus_type: focused.focus_type,
            value: focused.value,
            boost: focused.boost,
            active_until: focused.active_until.to_rfc3339(),
        }),
    ))
}

pub async fn feedback(
    State(state): State<AppState>,
    AuthUser { user_id, .. }: AuthUser,
    Path(memory_id): Path<String>,
    Json(req): Json<FeedbackRequest>,
) -> Result<(StatusCode, Json<FeedbackResponse>), (StatusCode, String)> {
    let store = v2_store(&state)?;
    let feedback_id = store
        .record_feedback(&user_id, &memory_id, &req.signal, req.context.as_deref())
        .await
        .map_err(crate::routes::memory::api_err_typed)?;
    Ok((
        StatusCode::CREATED,
        Json(FeedbackResponse {
            feedback_id,
            memory_id,
            signal: req.signal,
        }),
    ))
}

pub async fn get_memory_feedback(
    State(state): State<AppState>,
    AuthUser { user_id, .. }: AuthUser,
    Path(memory_id): Path<String>,
) -> ApiResult<MemoryFeedbackSummaryResponse> {
    let store = v2_store(&state)?;
    let summary = store
        .get_memory_feedback(&user_id, &memory_id)
        .await
        .map_err(crate::routes::memory::api_err_typed)?;
    Ok(Json(MemoryFeedbackSummaryResponse {
        memory_id: summary.memory_id,
        feedback: MemoryFeedbackCountsResponse {
            useful: summary.feedback.useful,
            irrelevant: summary.feedback.irrelevant,
            outdated: summary.feedback.outdated,
            wrong: summary.feedback.wrong,
        },
        last_feedback_at: summary.last_feedback_at.map(|ts| ts.to_rfc3339()),
    }))
}

pub async fn get_memory_feedback_history(
    State(state): State<AppState>,
    AuthUser { user_id, .. }: AuthUser,
    Path(memory_id): Path<String>,
    Query(query): Query<FeedbackHistoryQuery>,
) -> ApiResult<FeedbackHistoryResponse> {
    let store = v2_store(&state)?;
    let history = store
        .get_memory_feedback_history(&user_id, &memory_id, query.limit)
        .await
        .map_err(crate::routes::memory::api_err_typed)?;
    Ok(Json(FeedbackHistoryResponse {
        memory_id: history.memory_id,
        items: history
            .items
            .into_iter()
            .map(|item| FeedbackHistoryItemResponse {
                feedback_id: item.feedback_id,
                signal: item.signal,
                context: item.context,
                created_at: item.created_at.to_rfc3339(),
            })
            .collect(),
    }))
}

pub async fn get_feedback_history(
    State(state): State<AppState>,
    AuthUser { user_id, .. }: AuthUser,
    Query(query): Query<FeedbackFeedQuery>,
) -> ApiResult<FeedbackFeedResponse> {
    let store = v2_store(&state)?;
    let history = store
        .list_feedback_history(
            &user_id,
            query.memory_id.as_deref(),
            query.signal.as_deref(),
            query.limit,
        )
        .await
        .map_err(crate::routes::memory::api_err_typed)?;
    Ok(Json(FeedbackFeedResponse {
        items: history
            .items
            .into_iter()
            .map(|item| FeedbackFeedItemResponse {
                feedback_id: item.feedback_id,
                memory_id: item.memory_id,
                abstract_text: item.abstract_text,
                signal: item.signal,
                context: item.context,
                created_at: item.created_at.to_rfc3339(),
            })
            .collect(),
    }))
}

pub async fn get_feedback_stats(
    State(state): State<AppState>,
    AuthUser { user_id, .. }: AuthUser,
) -> ApiResult<serde_json::Value> {
    let store = v2_store(&state)?;
    let stats = store
        .get_feedback_stats(&user_id)
        .await
        .map_err(crate::routes::memory::api_err_typed)?;
    Ok(Json(serde_json::json!(stats)))
}

pub async fn get_feedback_by_tier(
    State(state): State<AppState>,
    AuthUser { user_id, .. }: AuthUser,
) -> ApiResult<serde_json::Value> {
    let store = v2_store(&state)?;
    let breakdown = store
        .get_feedback_by_tier(&user_id)
        .await
        .map_err(crate::routes::memory::api_err_typed)?;
    Ok(Json(serde_json::json!({ "breakdown": breakdown })))
}

pub async fn expand(
    State(state): State<AppState>,
    AuthUser { user_id, .. }: AuthUser,
    Json(req): Json<ExpandRequest>,
) -> ApiResult<ExpandResponse> {
    let store = v2_store(&state)?;
    let level = parse_expand_level(&req.level)?;
    let expanded = store
        .expand(&user_id, &req.memory_id, level)
        .await
        .map_err(crate::routes::memory::api_err_typed)?;
    Ok(Json(ExpandResponse {
        memory_id: expanded.memory_id,
        level: expanded.level.as_str().to_string(),
        abstract_text: expanded.abstract_text,
        overview: expanded.overview_text,
        detail: expanded.detail_text,
        links: expanded
            .links
            .map(|links| links.into_iter().map(map_link).collect()),
    }))
}

pub async fn recall(
    State(state): State<AppState>,
    AuthUser { user_id, .. }: AuthUser,
    Json(req): Json<RecallRequest>,
) -> ApiResult<RecallResponse> {
    let store = v2_store(&state)?;
    let response_mode = req
        .resolved_response_mode()
        .map_err(|e| (StatusCode::UNPROCESSABLE_ENTITY, e))?;
    let with_overview = req
        .resolved_with_overview()
        .map_err(|e| (StatusCode::UNPROCESSABLE_ENTITY, e))?;
    let with_links = req
        .resolved_with_links()
        .map_err(|e| (StatusCode::UNPROCESSABLE_ENTITY, e))?;
    let expand_links = req.resolved_expand_links();
    let tag_filter_mode = req
        .resolved_tag_filter_mode()
        .map_err(|e| (StatusCode::UNPROCESSABLE_ENTITY, e))?;
    let (created_after, created_before) = req
        .resolved_time_range()
        .map_err(|e| (StatusCode::UNPROCESSABLE_ENTITY, e))?;
    let session_only = match req.scope.as_str() {
        "all" => false,
        "session" => true,
        _ => {
            return Err((
                StatusCode::UNPROCESSABLE_ENTITY,
                "scope must be 'all' or 'session'".into(),
            ))
        }
    };
    let memory_type = req
        .memory_type
        .as_deref()
        .filter(|v| *v != "all")
        .map(parse_memory_type)
        .transpose()
        .map_err(|e| (StatusCode::UNPROCESSABLE_ENTITY, e))?;
    let query_embedding = state
        .service
        .embed(&req.query)
        .await
        .map_err(crate::routes::memory::api_err_typed)?;
    let filters = parse_recall_extra_filters(&req)?;
    let store_top_k = recall_top_k_for_store(req.top_k, &filters);
    let result = store
        .recall(
            &user_id,
            memoria_storage::RecallV2Request {
                query: req.query,
                top_k: store_top_k,
                max_tokens: req.max_tokens,
                session_only,
                session_id: req.session_id,
                memory_type,
                tags: req.tags,
                tag_filter_mode,
                created_after,
                created_before,
                with_overview,
                with_links,
                expand_links,
                query_embedding,
            },
        )
        .await
        .map_err(crate::routes::memory::api_err_typed)?;
    let result = finalize_filtered_recall(apply_recall_extra_filters(result, &filters), req.top_k);

    Ok(Json(RecallResponse {
        summary: map_recall_summary(result.summary),
        memories: result
            .memories
            .into_iter()
            .map(|m| match response_mode {
                RecallResponseMode::CompactAbstract => {
                    RecallItemResponse::Compact(RecallCompactItemResponse {
                        id: m.memory_id,
                        text: m.abstract_text,
                        memory_type: m.memory_type.to_string(),
                        score: m.score,
                        related: m.has_related,
                    })
                }
                RecallResponseMode::CompactOverview => {
                    RecallItemResponse::Compact(RecallCompactItemResponse {
                        id: m.memory_id,
                        text: m.overview_text.unwrap_or(m.abstract_text),
                        memory_type: m.memory_type.to_string(),
                        score: m.score,
                        related: m.has_related,
                    })
                }
                RecallResponseMode::Verbose => {
                    RecallItemResponse::Verbose(Box::new(RecallVerboseItemResponse {
                        id: m.memory_id,
                        abstract_text: m.abstract_text,
                        overview: m.overview_text,
                        score: m.score,
                        memory_type: m.memory_type.to_string(),
                        confidence: m.confidence,
                        has_overview: m.has_overview,
                        has_detail: m.has_detail,
                        access_count: m.access_count,
                        link_count: m.link_count,
                        has_related: m.has_related,
                        retrieval_path: m.retrieval_path.as_str().to_string(),
                        feedback_impact: map_feedback_impact(m.feedback_impact),
                        ranking: map_recall_ranking(m.ranking),
                        links: m
                            .links
                            .map(|links| links.into_iter().map(map_link).collect()),
                    }))
                }
            })
            .collect(),
        token_used: result.token_used,
        has_more: result.has_more,
    }))
}

pub async fn batch_recall(
    State(state): State<AppState>,
    AuthUser { user_id, .. }: AuthUser,
    Json(req): Json<BatchRecallRequest>,
) -> ApiResult<BatchRecallResponse> {
    if req.queries.len() > V2_BATCH_LIMIT {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("batch exceeds {V2_BATCH_LIMIT} items"),
        ));
    }
    let store = v2_store(&state)?;
    let mut results = Vec::with_capacity(req.queries.len());
    for query in req.queries {
        let filters = parse_recall_extra_filters(&query)?;
        let response_mode = query
            .resolved_response_mode()
            .map_err(|e| (StatusCode::UNPROCESSABLE_ENTITY, e))?;
        let with_overview = query
            .resolved_with_overview()
            .map_err(|e| (StatusCode::UNPROCESSABLE_ENTITY, e))?;
        let with_links = query
            .resolved_with_links()
            .map_err(|e| (StatusCode::UNPROCESSABLE_ENTITY, e))?;
        let expand_links = query.resolved_expand_links();
        let tag_filter_mode = query
            .resolved_tag_filter_mode()
            .map_err(|e| (StatusCode::UNPROCESSABLE_ENTITY, e))?;
        let (created_after, created_before) = query
            .resolved_time_range()
            .map_err(|e| (StatusCode::UNPROCESSABLE_ENTITY, e))?;
        let session_only = match query.scope.as_str() {
            "all" => false,
            "session" => true,
            _ => {
                return Err((
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "scope must be 'all' or 'session'".into(),
                ))
            }
        };
        let memory_type = query
            .memory_type
            .as_deref()
            .filter(|v| *v != "all")
            .map(parse_memory_type)
            .transpose()
            .map_err(|e| (StatusCode::UNPROCESSABLE_ENTITY, e))?;
        let query_embedding = state
            .service
            .embed(&query.query)
            .await
            .map_err(crate::routes::memory::api_err_typed)?;
        let store_top_k = recall_top_k_for_store(query.top_k, &filters);
        let recalled = store
            .recall(
                &user_id,
                memoria_storage::RecallV2Request {
                    query: query.query,
                    top_k: store_top_k,
                    max_tokens: query.max_tokens,
                    session_only,
                    session_id: query.session_id,
                    memory_type,
                    tags: query.tags,
                    tag_filter_mode,
                    created_after,
                    created_before,
                    with_overview,
                    with_links,
                    expand_links,
                    query_embedding,
                },
            )
            .await
            .map_err(crate::routes::memory::api_err_typed)?;
        let recalled = finalize_filtered_recall(
            apply_recall_extra_filters(recalled, &filters),
            query.top_k,
        );
        results.push(RecallResponse {
            summary: map_recall_summary(recalled.summary),
            memories: recalled
                .memories
                .into_iter()
                .map(|m| match response_mode {
                    RecallResponseMode::CompactAbstract => {
                        RecallItemResponse::Compact(RecallCompactItemResponse {
                            id: m.memory_id,
                            text: m.abstract_text,
                            memory_type: m.memory_type.to_string(),
                            score: m.score,
                            related: m.has_related,
                        })
                    }
                    RecallResponseMode::CompactOverview => {
                        RecallItemResponse::Compact(RecallCompactItemResponse {
                            id: m.memory_id,
                            text: m.overview_text.unwrap_or(m.abstract_text),
                            memory_type: m.memory_type.to_string(),
                            score: m.score,
                            related: m.has_related,
                        })
                    }
                    RecallResponseMode::Verbose => {
                        RecallItemResponse::Verbose(Box::new(RecallVerboseItemResponse {
                            id: m.memory_id,
                            abstract_text: m.abstract_text,
                            overview: m.overview_text,
                            score: m.score,
                            memory_type: m.memory_type.to_string(),
                            confidence: m.confidence,
                            has_overview: m.has_overview,
                            has_detail: m.has_detail,
                            access_count: m.access_count,
                            link_count: m.link_count,
                            has_related: m.has_related,
                            retrieval_path: m.retrieval_path.as_str().to_string(),
                            feedback_impact: map_feedback_impact(m.feedback_impact),
                            ranking: map_recall_ranking(m.ranking),
                            links: m
                                .links
                                .map(|links| links.into_iter().map(map_link).collect()),
                        }))
                    }
                })
                .collect(),
            token_used: recalled.token_used,
            has_more: recalled.has_more,
        });
    }
    Ok(Json(BatchRecallResponse { results }))
}

pub async fn batch_expand(
    State(state): State<AppState>,
    AuthUser { user_id, .. }: AuthUser,
    Json(req): Json<BatchExpandRequest>,
) -> ApiResult<BatchExpandResponse> {
    if req.memory_ids.len() > V2_BATCH_LIMIT {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("batch exceeds {V2_BATCH_LIMIT} items"),
        ));
    }
    let level = parse_expand_level(&req.level)?;
    let store = v2_store(&state)?;
    let memory_ids = dedupe_memory_ids(req.memory_ids)?;
    let expanded_items = store
        .expand_batch(&user_id, &memory_ids, level)
        .await
        .map_err(crate::routes::memory::api_err_typed)?;
    let mut items = Vec::with_capacity(expanded_items.len());
    for expanded in expanded_items {
        items.push(ExpandResponse {
            memory_id: expanded.memory_id,
            level: expanded.level.as_str().to_string(),
            abstract_text: expanded.abstract_text,
            overview: expanded.overview_text,
            detail: expanded.detail_text,
            links: expanded
                .links
                .map(|links| links.into_iter().map(map_link).collect()),
        });
    }
    Ok(Json(BatchExpandResponse { items }))
}

pub async fn job_metrics(
    State(state): State<AppState>,
    AuthUser { user_id, .. }: AuthUser,
) -> ApiResult<JobMetricsResponse> {
    let store = v2_store(&state)?;
    let metrics = store
        .job_metrics(&user_id)
        .await
        .map_err(crate::routes::memory::api_err_typed)?;
    Ok(Json(JobMetricsResponse {
        pending_count: metrics.pending_count,
        in_progress_count: metrics.in_progress_count,
        failed_count: metrics.failed_count,
        avg_processing_time_ms: metrics.avg_processing_time_ms,
        oldest_pending_age_secs: metrics.oldest_pending_age_secs,
    }))
}
