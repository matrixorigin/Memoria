use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use serde::Deserialize;
use sqlx::Row;

use crate::{auth::AuthUser, models::*, state::AppState};

use memoria_core::nullable_str_from_row;

type ApiResult<T> = Result<Json<T>, (StatusCode, String)>;
pub fn api_err(e: impl std::fmt::Display) -> (StatusCode, String) {
    tracing::error!(error = %e, "internal server error");
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

/// Map MemoriaError to proper HTTP status codes.
pub fn api_err_typed(e: memoria_core::MemoriaError) -> (StatusCode, String) {
    use memoria_core::MemoriaError::*;
    let status = match &e {
        NotFound(_) => StatusCode::NOT_FOUND,
        Validation(_) | InvalidMemoryType(_) | InvalidTrustTier(_) => {
            StatusCode::UNPROCESSABLE_ENTITY
        }
        Blocked(_) => StatusCode::FORBIDDEN,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    if status.is_server_error() {
        tracing::error!(error = %e, "internal server error");
    }
    (status, e.to_string())
}

#[derive(Deserialize, Default)]
pub struct ListQuery {
    pub memory_type: Option<String>,
    #[serde(default = "default_limit")]
    pub limit: i64,
    pub cursor: Option<String>,
}
fn default_limit() -> i64 {
    100
}

pub async fn health() -> &'static str {
    "ok"
}

pub async fn health_instance(
    State(state): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    let db_ok = if let Some(sql) = &state.service.sql_store {
        sqlx::query("SELECT 1").execute(sql.pool()).await.is_ok()
    } else {
        false
    };
    let status = if db_ok { "ok" } else { "degraded" };
    let code = if db_ok {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (
        code,
        Json(serde_json::json!({
            "status": status,
            "instance_id": state.instance_id,
            "db": db_ok,
        })),
    )
}

pub async fn list_memories(
    State(state): State<AppState>,
    AuthUser { user_id, .. }: AuthUser,
    Query(q): Query<ListQuery>,
) -> ApiResult<ListResponse> {
    let limit = q.limit.clamp(1, 500);
    // Cursor is a memory_id; validate it looks like a hex string
    let cursor = q
        .cursor
        .as_deref()
        .filter(|c| c.len() == 32 && c.chars().all(|ch| ch.is_ascii_hexdigit()));
    let fetch_limit = limit + 1;
    let mut memories = state
        .service
        .list_active_paged(&user_id, fetch_limit, q.memory_type.as_deref(), cursor)
        .await
        .map_err(api_err)?;
    let has_more = memories.len() > limit as usize;
    memories.truncate(limit as usize);
    let next_cursor = if has_more {
        memories.last().map(|m| m.memory_id.clone())
    } else {
        None
    };
    let items: Vec<MemoryResponse> = memories.into_iter().map(Into::into).collect();
    Ok(Json(ListResponse { items, next_cursor }))
}

pub async fn store_memory(
    State(state): State<AppState>,
    AuthUser { user_id, .. }: AuthUser,
    Json(req): Json<StoreRequest>,
) -> Result<(StatusCode, Json<MemoryResponse>), (StatusCode, String)> {
    if req.content.is_empty() {
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
    let mt =
        parse_memory_type(&req.memory_type).map_err(|e| (StatusCode::UNPROCESSABLE_ENTITY, e))?;
    let tier = req
        .trust_tier
        .as_deref()
        .map(parse_trust_tier)
        .transpose()
        .map_err(|e| (StatusCode::UNPROCESSABLE_ENTITY, e))?;
    let observed_at = req
        .observed_at
        .as_deref()
        .map(|s| chrono::DateTime::parse_from_rfc3339(s).map(|dt| dt.with_timezone(&chrono::Utc)))
        .transpose()
        .map_err(|e| (StatusCode::UNPROCESSABLE_ENTITY, e.to_string()))?;
    let m = state
        .service
        .store_memory(
            &user_id,
            &req.content,
            mt,
            req.session_id,
            tier,
            observed_at,
            req.initial_confidence,
        )
        .await
        .map_err(|e| {
            if matches!(e, memoria_core::MemoriaError::Blocked(_)) {
                crate::metrics::registry().security.sensitivity_blocks.inc();
            }
            api_err_typed(e)
        })?;
    Ok((StatusCode::CREATED, Json(m.into())))
}

pub async fn batch_store(
    State(state): State<AppState>,
    AuthUser { user_id, .. }: AuthUser,
    Json(req): Json<BatchStoreRequest>,
) -> Result<(StatusCode, Json<Vec<MemoryResponse>>), (StatusCode, String)> {
    if req.memories.len() > 100 {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            "batch exceeds 100 items".into(),
        ));
    }
    for m in &req.memories {
        if m.content.len() > 32_768 {
            return Err((
                StatusCode::UNPROCESSABLE_ENTITY,
                "content exceeds 32 KiB limit".into(),
            ));
        }
    }
    let items: Vec<_> = req
        .memories
        .into_iter()
        .map(|r| {
            let mt = parse_memory_type(&r.memory_type)
                .map_err(|e| (StatusCode::UNPROCESSABLE_ENTITY, e));
            let tier = r
                .trust_tier
                .as_deref()
                .map(parse_trust_tier)
                .transpose()
                .map_err(|e| (StatusCode::UNPROCESSABLE_ENTITY, e));
            (r.content, mt, tier, r.session_id)
        })
        .collect();

    // Validate all types upfront
    let mut validated = Vec::with_capacity(items.len());
    for (content, mt_result, tier_result, session_id) in items {
        let mt = mt_result?;
        let tier = tier_result?;
        validated.push((content, mt, session_id, tier));
    }

    let results = state
        .service
        .store_batch(&user_id, validated)
        .await
        .map_err(api_err)?;
    Ok((
        StatusCode::CREATED,
        Json(results.into_iter().map(Into::into).collect()),
    ))
}

pub async fn retrieve(
    State(state): State<AppState>,
    AuthUser { user_id, .. }: AuthUser,
    Json(req): Json<RetrieveRequest>,
) -> ApiResult<serde_json::Value> {
    if req.session_id.is_none() && (req.filter_session == Some(true) || !req.include_cross_session)
    {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            "session_id is required for strict session retrieval".to_string(),
        ));
    }
    let top_k = req.top_k.clamp(1, 100);
    let level = memoria_service::ExplainLevel::from_str_or_bool(&req.explain);
    let retrieve_options = req.retrieve_options();

    if level != memoria_service::ExplainLevel::None {
        let (results, explain) = state
            .service
            .retrieve_explain_level_with_options(
                &user_id,
                &req.query,
                top_k,
                level,
                &retrieve_options,
            )
            .await
            .map_err(api_err)?;
        let items: Vec<MemoryResponse> = results.into_iter().map(Into::into).collect();
        Ok(Json(
            serde_json::json!({"results": items, "explain": explain}),
        ))
    } else {
        let results = state
            .service
            .retrieve_with_options(&user_id, &req.query, top_k, &retrieve_options)
            .await
            .map_err(api_err)?;
        let items: Vec<MemoryResponse> = results.into_iter().map(Into::into).collect();
        Ok(Json(serde_json::json!(items)))
    }
}

pub async fn search(
    State(state): State<AppState>,
    AuthUser { user_id, .. }: AuthUser,
    Json(req): Json<RetrieveRequest>,
) -> ApiResult<serde_json::Value> {
    let top_k = req.top_k.clamp(1, 100);
    let level = memoria_service::ExplainLevel::from_str_or_bool(&req.explain);
    if level != memoria_service::ExplainLevel::None {
        let (results, explain) = state
            .service
            .search_explain_level(&user_id, &req.query, top_k, level)
            .await
            .map_err(api_err)?;
        let items: Vec<MemoryResponse> = results.into_iter().map(Into::into).collect();
        Ok(Json(
            serde_json::json!({"results": items, "explain": explain}),
        ))
    } else {
        let results = state
            .service
            .search(&user_id, &req.query, top_k)
            .await
            .map_err(api_err)?;
        Ok(Json(serde_json::json!(results
            .into_iter()
            .map(Into::into)
            .collect::<Vec<MemoryResponse>>())))
    }
}

pub async fn get_memory(
    State(state): State<AppState>,
    AuthUser { user_id, is_master }: AuthUser,
    Path(id): Path<String>,
) -> ApiResult<Option<MemoryResponse>> {
    let m = state
        .service
        .get_for_user(&user_id, &id)
        .await
        .map_err(api_err)?;
    if let Some(ref mem) = m {
        if !is_master && mem.user_id != user_id {
            return Err((StatusCode::FORBIDDEN, "Not your memory".to_string()));
        }
    }
    Ok(Json(m.map(Into::into)))
}

pub async fn correct_memory(
    State(state): State<AppState>,
    AuthUser { user_id, is_master }: AuthUser,
    Path(id): Path<String>,
    Json(req): Json<CorrectRequest>,
) -> ApiResult<MemoryResponse> {
    if !is_master {
        let existing = state
            .service
            .get_for_user(&user_id, &id)
            .await
            .map_err(api_err)?
            .ok_or_else(|| (StatusCode::NOT_FOUND, "Memory not found".to_string()))?;
        if existing.user_id != user_id {
            return Err((StatusCode::FORBIDDEN, "Not your memory".to_string()));
        }
    }
    let m = state
        .service
        .correct(&user_id, &id, &req.new_content)
        .await
        .map_err(api_err)?;
    Ok(Json(m.into()))
}

pub async fn correct_by_query(
    State(state): State<AppState>,
    AuthUser { user_id, .. }: AuthUser,
    Json(req): Json<CorrectByQueryRequest>,
) -> ApiResult<MemoryResponse> {
    let results = state
        .service
        .retrieve(&user_id, &req.query, 1)
        .await
        .map_err(api_err)?;
    let found = results.into_iter().next().ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            "No matching memory found".to_string(),
        )
    })?;
    let m = state
        .service
        .correct(&user_id, &found.memory_id, &req.new_content)
        .await
        .map_err(api_err)?;
    Ok(Json(m.into()))
}

pub async fn delete_memory(
    State(state): State<AppState>,
    AuthUser { user_id, is_master }: AuthUser,
    Path(id): Path<String>,
) -> Result<StatusCode, (StatusCode, String)> {
    if !is_master {
        let existing = state
            .service
            .get_for_user(&user_id, &id)
            .await
            .map_err(api_err)?
            .ok_or_else(|| (StatusCode::NOT_FOUND, "Memory not found".to_string()))?;
        if existing.user_id != user_id {
            return Err((StatusCode::FORBIDDEN, "Not your memory".to_string()));
        }
    }
    let _ = state.service.purge(&user_id, &id).await.map_err(api_err)?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn purge_memories(
    State(state): State<AppState>,
    AuthUser { user_id, is_master }: AuthUser,
    Json(req): Json<PurgeRequest>,
) -> ApiResult<PurgeResponse> {
    let result = if let Some(ids) = &req.memory_ids {
        if !is_master {
            for id in ids {
                let mem = state
                    .service
                    .get_for_user(&user_id, id)
                    .await
                    .map_err(api_err)?
                    .ok_or_else(|| (StatusCode::NOT_FOUND, format!("Memory not found: {id}")))?;
                if mem.user_id != user_id {
                    return Err((StatusCode::FORBIDDEN, format!("Not your memory: {id}")));
                }
            }
        }
        let id_refs: Vec<&str> = ids.iter().map(|s| s.as_str()).collect();
        state
            .service
            .purge_batch(&user_id, &id_refs)
            .await
            .map_err(api_err)?
    } else if let Some(topic) = &req.topic {
        state
            .service
            .purge_by_topic(&user_id, topic)
            .await
            .map_err(api_err)?
    } else {
        memoria_service::PurgeResult {
            purged: 0,
            snapshot_name: None,
            warning: None,
        }
    };
    Ok(Json(PurgeResponse {
        purged: result.purged,
        snapshot_name: result.snapshot_name,
        warning: result.warning,
    }))
}

pub async fn get_profile(
    State(state): State<AppState>,
    AuthUser { user_id, is_master }: AuthUser,
    Path(target): Path<String>,
) -> ApiResult<serde_json::Value> {
    let resolved = if target == "me" || target == user_id {
        user_id
    } else if is_master {
        target
    } else {
        return Err((
            StatusCode::FORBIDDEN,
            "Cannot access other users' profiles".to_string(),
        ));
    };
    let sql = state
        .service
        .user_sql_store(&resolved)
        .await
        .map_err(api_err)?;
    let table = sql.active_table(&resolved).await.map_err(api_err)?;
    let memories = state
        .service
        .list_active(&resolved, 50)
        .await
        .map_err(api_err)?;
    let profile: Vec<_> = memories
        .iter()
        .filter(|m| m.memory_type == memoria_core::MemoryType::Profile)
        .map(|m| m.content.as_str())
        .collect();

    // Stats enrichment follows the user's active branch table.
    let stats_query = format!(
        "SELECT memory_type, COUNT(*) as cnt, \
         ROUND(AVG(initial_confidence), 2) as avg_conf, \
         MIN(observed_at) as oldest, MAX(observed_at) as newest \
         FROM {table} WHERE user_id = ? AND is_active = 1 GROUP BY memory_type"
    );
    let stats: serde_json::Value = sqlx::query(&stats_query)
    .bind(&resolved)
    .fetch_all(sql.pool()).await
    .map(|rows| {
        let mut by_type = serde_json::Map::new();
        let mut total = 0i64;
        let mut oldest: Option<String> = None;
        let mut newest: Option<String> = None;
        let mut conf_sum = 0.0f64;
        let mut conf_n = 0i64;
        for r in &rows {
            let mt: String = r.try_get("memory_type").unwrap_or_default();
            let cnt: i64 = r.try_get("cnt").unwrap_or(0);
            by_type.insert(mt, serde_json::json!(cnt));
            total += cnt;
            if let Ok(c) = r.try_get::<f64, _>("avg_conf") { conf_sum += c * cnt as f64; conf_n += cnt; }
            if let Ok(Some(d)) = r.try_get::<Option<chrono::NaiveDateTime>, _>("oldest") {
                let s = d.to_string();
                if oldest.as_ref().is_none_or(|o| s < *o) { oldest = Some(s); }
            }
            if let Ok(Some(d)) = r.try_get::<Option<chrono::NaiveDateTime>, _>("newest") {
                let s = d.to_string();
                if newest.as_ref().is_none_or(|n| s > *n) { newest = Some(s); }
            }
        }
        let avg_conf = if conf_n > 0 { ((conf_sum / conf_n as f64) * 100.0).round() / 100.0 } else { 0.0 };
        serde_json::json!({"by_type": by_type, "total": total, "avg_confidence": avg_conf, "oldest": oldest, "newest": newest})
    }).unwrap_or_else(|_| serde_json::json!({}));

    Ok(Json(
        serde_json::json!({"user_id": resolved, "profile": profile.join("\n"), "stats": stats}),
    ))
}

#[derive(serde::Deserialize)]
pub struct ObserveRequest {
    pub messages: Vec<serde_json::Value>,
    pub source_event_ids: Option<Vec<String>>,
    pub session_id: Option<String>,
}

/// Extract and store memories from a conversation turn.
/// With LLM: uses structured extraction (type, content, confidence).
/// Without LLM: stores each non-empty assistant/user message as a semantic memory.
pub async fn observe_turn(
    State(state): State<AppState>,
    AuthUser { user_id, .. }: AuthUser,
    Json(req): Json<ObserveRequest>,
) -> ApiResult<serde_json::Value> {
    let (memories, has_llm) = state
        .service
        .observe_turn(&user_id, &req.messages, req.session_id)
        .await
        .map_err(api_err)?;

    let stored: Vec<_> = memories
        .iter()
        .map(|m| {
            serde_json::json!({
                "memory_id": m.memory_id,
                "content": m.content,
                "memory_type": m.memory_type.to_string(),
            })
        })
        .collect();

    let mut result = serde_json::json!({ "memories": stored });
    if !has_llm {
        result["warning"] = serde_json::json!("LLM not configured — storing messages as-is");
    }
    Ok(Json(result))
}

/// GET /v1/memories/:id/history — version chain via superseded_by links.
pub async fn get_memory_history(
    State(state): State<AppState>,
    AuthUser { user_id, .. }: AuthUser,
    Path(id): Path<String>,
) -> ApiResult<serde_json::Value> {
    use sqlx::Row;

    let sql = state
        .service
        .user_sql_store(&user_id)
        .await
        .map_err(api_err)?;
    let table = sql.active_table(&user_id).await.map_err(api_err)?;

    let mut chain = Vec::new();
    let mut visited = std::collections::HashSet::new();

    // Walk forward from the given id following superseded_by
    let mut current_id = Some(id.clone());
    while let Some(cid) = current_id {
        if !visited.insert(cid.clone()) {
            break;
        }
        let row = sqlx::query(&format!(
            "SELECT memory_id, content, is_active, superseded_by, observed_at, memory_type \
                 FROM {} WHERE memory_id = ? AND user_id = ?",
            table
        ))
        .bind(&cid)
        .bind(&user_id)
        .fetch_optional(sql.pool())
        .await
        .map_err(api_err)?;

        match row {
            Some(r) => {
                let mid: String = r.try_get("memory_id").unwrap_or_default();
                let sup: Option<String> =
                    nullable_str_from_row(r.try_get("superseded_by").ok().flatten());
                chain.push(serde_json::json!({
                    "memory_id": mid,
                    "content": r.try_get::<String, _>("content").unwrap_or_default(),
                    "is_active": r.try_get::<i8, _>("is_active").unwrap_or(0) != 0,
                    "superseded_by": sup,
                    "observed_at": r.try_get::<Option<String>, _>("observed_at").ok().flatten(),
                    "memory_type": r.try_get::<String, _>("memory_type").unwrap_or_default(),
                }));
                current_id = sup;
            }
            None => {
                if chain.is_empty() {
                    return Err((StatusCode::NOT_FOUND, "Memory not found".to_string()));
                }
                break;
            }
        }
    }

    // Walk backwards: find older versions that point to our root
    if let Some(root_id) = chain.first().and_then(|v| v["memory_id"].as_str()) {
        let mut prev_id = root_id.to_string();
        loop {
            let older = sqlx::query(&format!(
                "SELECT memory_id, content, is_active, superseded_by, observed_at, memory_type \
                     FROM {} WHERE superseded_by = ? AND user_id = ?",
                table
            ))
            .bind(&prev_id)
            .bind(&user_id)
            .fetch_optional(sql.pool())
            .await
            .map_err(api_err)?;

            match older {
                Some(r) => {
                    let mid: String = r.try_get("memory_id").unwrap_or_default();
                    if !visited.insert(mid.clone()) {
                        break;
                    }
                    prev_id = mid.clone();
                    chain.insert(0, serde_json::json!({
                        "memory_id": mid,
                        "content": r.try_get::<String, _>("content").unwrap_or_default(),
                        "is_active": r.try_get::<i8, _>("is_active").unwrap_or(0) != 0,
                        "superseded_by": r.try_get::<Option<String>, _>("superseded_by").ok().flatten(),
                        "observed_at": r.try_get::<Option<String>, _>("observed_at").ok().flatten(),
                        "memory_type": r.try_get::<String, _>("memory_type").unwrap_or_default(),
                    }));
                }
                None => break,
            }
        }
    }

    Ok(Json(serde_json::json!({
        "memory_id": id,
        "versions": chain,
        "total": chain.len(),
    })))
}

// ── Pipeline ──────────────────────────────────────────────────────────────────

#[derive(serde::Deserialize)]
pub struct PipelineRequest {
    pub candidates: Vec<PipelineCandidate>,
    pub sandbox_query: Option<String>,
}

#[derive(serde::Deserialize)]
pub struct PipelineCandidate {
    pub content: String,
    pub memory_type: Option<String>,
    pub trust_tier: Option<String>,
}

pub async fn run_pipeline(
    State(state): State<AppState>,
    AuthUser { user_id, .. }: AuthUser,
    Json(req): Json<PipelineRequest>,
) -> ApiResult<serde_json::Value> {
    use crate::models::{parse_memory_type, parse_trust_tier};
    use memoria_core::MemoryType;
    use memoria_service::MemoryPipeline;

    let candidates = req
        .candidates
        .into_iter()
        .map(|c| {
            let mt = c
                .memory_type
                .as_deref()
                .map(|s| parse_memory_type(s).unwrap_or(MemoryType::Semantic))
                .unwrap_or(MemoryType::Semantic);
            let tier = c
                .trust_tier
                .as_deref()
                .map(parse_trust_tier)
                .transpose()
                .ok()
                .flatten();
            (c.content, mt, tier)
        })
        .collect();

    let pipeline = MemoryPipeline::new(state.service.clone(), Some(state.git.clone()));
    let result = pipeline
        .run(&user_id, candidates, req.sandbox_query.as_deref())
        .await;

    Ok(Json(serde_json::json!({
        "memories_stored": result.memories_stored,
        "memories_rejected": result.memories_rejected,
        "memories_redacted": result.memories_redacted,
        "errors": result.errors,
    })))
}

// ── Feedback ──────────────────────────────────────────────────────────────────

#[derive(serde::Deserialize)]
pub struct FeedbackRequest {
    pub signal: String,
    pub context: Option<String>,
}

#[derive(serde::Serialize)]
pub struct FeedbackResponse {
    pub feedback_id: String,
    pub memory_id: String,
    pub signal: String,
}

/// POST /v1/memories/:id/feedback — record explicit relevance feedback.
pub async fn record_feedback(
    State(state): State<AppState>,
    AuthUser { user_id, .. }: AuthUser,
    Path(memory_id): Path<String>,
    Json(req): Json<FeedbackRequest>,
) -> Result<(StatusCode, Json<FeedbackResponse>), (StatusCode, String)> {
    let feedback_id = state
        .service
        .record_feedback(&user_id, &memory_id, &req.signal, req.context.as_deref())
        .await
        .map_err(api_err_typed)?;
    Ok((
        StatusCode::CREATED,
        Json(FeedbackResponse {
            feedback_id,
            memory_id,
            signal: req.signal,
        }),
    ))
}

/// GET /v1/feedback/stats — get feedback statistics for the user.
pub async fn get_feedback_stats(
    State(state): State<AppState>,
    AuthUser { user_id, .. }: AuthUser,
) -> ApiResult<serde_json::Value> {
    let stats = state
        .service
        .get_feedback_stats(&user_id)
        .await
        .map_err(api_err)?;
    Ok(Json(serde_json::json!(stats)))
}

/// GET /v1/feedback/by-tier — get feedback breakdown by trust tier.
pub async fn get_feedback_by_tier(
    State(state): State<AppState>,
    AuthUser { user_id, .. }: AuthUser,
) -> ApiResult<serde_json::Value> {
    let breakdown = state
        .service
        .get_feedback_by_tier(&user_id)
        .await
        .map_err(api_err)?;
    Ok(Json(serde_json::json!({"breakdown": breakdown})))
}

/// GET /v1/retrieval-params — get user's adaptive retrieval parameters.
pub async fn get_retrieval_params(
    State(state): State<AppState>,
    AuthUser { user_id, .. }: AuthUser,
) -> ApiResult<serde_json::Value> {
    let sql = state
        .service
        .user_sql_store(&user_id)
        .await
        .map_err(api_err)?;
    let params = sql
        .get_user_retrieval_params(&user_id)
        .await
        .map_err(api_err)?;
    Ok(Json(serde_json::to_value(params).unwrap()))
}

#[derive(Debug, serde::Deserialize)]
pub struct SetRetrievalParamsRequest {
    pub feedback_weight: Option<f64>,
    pub temporal_decay_hours: Option<f64>,
    pub confidence_weight: Option<f64>,
}

/// PUT /v1/retrieval-params — update user's adaptive retrieval parameters.
pub async fn set_retrieval_params(
    State(state): State<AppState>,
    AuthUser { user_id, .. }: AuthUser,
    Json(req): Json<SetRetrievalParamsRequest>,
) -> ApiResult<serde_json::Value> {
    let sql = state
        .service
        .user_sql_store(&user_id)
        .await
        .map_err(api_err)?;

    let mut params = sql
        .get_user_retrieval_params(&user_id)
        .await
        .map_err(api_err)?;

    if let Some(v) = req.feedback_weight {
        params.feedback_weight = v.clamp(0.01, 0.5);
    }
    if let Some(v) = req.temporal_decay_hours {
        params.temporal_decay_hours = v.clamp(1.0, 720.0);
    }
    if let Some(v) = req.confidence_weight {
        params.confidence_weight = v.clamp(0.0, 0.5);
    }

    sql.set_user_retrieval_params(&params)
        .await
        .map_err(api_err)?;
    Ok(Json(serde_json::to_value(params).unwrap()))
}

/// POST /v1/retrieval-params/tune — trigger auto-tuning of retrieval parameters.
pub async fn tune_retrieval_params(
    State(state): State<AppState>,
    AuthUser { user_id, .. }: AuthUser,
) -> ApiResult<serde_json::Value> {
    use memoria_service::scoring::{DefaultScoringPlugin, ScoringPlugin};

    let sql = state
        .service
        .user_sql_store(&user_id)
        .await
        .map_err(api_err)?;

    let old_params = sql
        .get_user_retrieval_params(&user_id)
        .await
        .map_err(api_err)?;

    let plugin = DefaultScoringPlugin;
    match plugin
        .tune_params(sql.as_ref(), &user_id)
        .await
        .map_err(api_err)?
    {
        Some(new_params) => Ok(Json(serde_json::json!({
            "tuned": true,
            "old_params": old_params,
            "new_params": new_params
        }))),
        None => Ok(Json(serde_json::json!({
            "tuned": false,
            "message": "Not enough feedback to tune parameters (minimum 10 feedback signals required)",
            "current_params": old_params
        }))),
    }
}

pub async fn get_tool_usage(
    State(state): State<AppState>,
    AuthUser { user_id, .. }: AuthUser,
) -> ApiResult<serde_json::Value> {
    let mut usage = state.tool_usage_batcher.get_user_tool_usage(&user_id);
    if usage.is_empty() {
        state
            .tool_usage_batcher
            .load_user_from_db(&state.service, &user_id)
            .await;
        usage = state.tool_usage_batcher.get_user_tool_usage(&user_id);
    }
    let items: Vec<serde_json::Value> = usage
        .into_iter()
        .map(|(tool, ts)| serde_json::json!({"tool_name": tool, "last_used_at": ts.to_rfc3339()}))
        .collect();
    Ok(Json(serde_json::json!(items)))
}
