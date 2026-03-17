use axum::{extract::State, http::StatusCode, Json};
use serde_json::json;
use tracing::warn;

use crate::{auth::AuthUser, models::*, routes::memory::api_err, state::AppState};

pub async fn governance(
    State(state): State<AppState>,
    AuthUser(user_id): AuthUser,
    Json(req): Json<GovernanceRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let sql = state.service.sql_store.as_ref()
        .ok_or_else(|| (StatusCode::SERVICE_UNAVAILABLE, "SQL store required".to_string()))?;

    const COOLDOWN_SECS: i64 = 3600;
    if !req.force {
        if let Some(remaining) = sql.check_cooldown(&user_id, "governance", COOLDOWN_SECS).await.map_err(api_err)? {
            return Ok(Json(json!({ "skipped": true, "cooldown_remaining_s": remaining })));
        }
    }
    let quarantined = sql.quarantine_low_confidence(&user_id).await.map_err(api_err)?;
    let cleaned = sql.cleanup_stale(&user_id).await.map_err(api_err)?;
    if quarantined > 0 {
        sql.log_edit(&user_id, "governance:quarantine", &[], &format!("quarantined {quarantined}"), None).await;
    }
    if cleaned > 0 {
        sql.log_edit(&user_id, "governance:cleanup_stale", &[], &format!("cleaned {cleaned}"), None).await;
    }
    sql.set_cooldown(&user_id, "governance").await.map_err(api_err)?;
    Ok(Json(json!({ "quarantined": quarantined, "cleaned_stale": cleaned })))
}

pub async fn consolidate(
    State(state): State<AppState>,
    AuthUser(user_id): AuthUser,
    Json(req): Json<GovernanceRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let sql = state.service.sql_store.as_ref()
        .ok_or_else(|| (StatusCode::SERVICE_UNAVAILABLE, "SQL store required".to_string()))?;

    const COOLDOWN_SECS: i64 = 1800;
    if !req.force {
        if let Some(remaining) = sql.check_cooldown(&user_id, "consolidate", COOLDOWN_SECS).await.map_err(api_err)? {
            return Ok(Json(json!({ "skipped": true, "cooldown_remaining_s": remaining })));
        }
    }
    let graph = sql.graph_store();
    let consolidator = memoria_storage::GraphConsolidator::new(&graph);
    let result = consolidator.consolidate(&user_id).await;
    sql.set_cooldown(&user_id, "consolidate").await.map_err(api_err)?;
    Ok(Json(json!({
        "conflicts_detected": result.conflicts_detected,
        "orphaned_scenes": result.orphaned_scenes,
        "promoted": result.promoted,
        "demoted": result.demoted,
        "errors": result.errors,
    })))
}

pub async fn reflect(
    State(state): State<AppState>,
    AuthUser(user_id): AuthUser,
    Json(req): Json<ReflectRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let sql = state.service.sql_store.as_ref()
        .ok_or_else(|| (StatusCode::SERVICE_UNAVAILABLE, "SQL store required".to_string()))?;

    if req.mode == "internal" && state.service.llm.is_none() {
        return Err((StatusCode::SERVICE_UNAVAILABLE, "LLM_API_KEY not configured".to_string()));
    }

    const COOLDOWN_SECS: i64 = 7200;
    if req.mode != "candidates" && !req.force {
        if let Some(remaining) = sql.check_cooldown(&user_id, "reflect", COOLDOWN_SECS).await.map_err(api_err)? {
            return Ok(Json(json!({ "skipped": true, "cooldown_remaining_s": remaining })));
        }
    }

    let graph = sql.graph_store();
    let clusters = memoria_mcp::tools::build_reflect_clusters(&graph, &user_id)
        .await.map_err(api_err)?;

    if clusters.is_empty() {
        return Ok(Json(json!({ "candidates": [], "scenes_created": 0 })));
    }

    if req.mode == "candidates" || state.service.llm.is_none() {
        let candidates: Vec<serde_json::Value> = clusters.iter().map(|(signal, importance, mems)| {
            json!({
                "signal": signal,
                "importance": importance,
                "memories": mems.iter().map(|(mid, content, _)| json!({"memory_id": mid, "content": content})).collect::<Vec<_>>()
            })
        }).collect();
        return Ok(Json(json!({ "candidates": candidates })));
    }

    // LLM synthesis
    let llm = state.service.llm.as_ref().unwrap();
    let mut scenes_created = 0usize;
    for (_, _, mems) in &clusters {
        let experiences = mems.iter().map(|(_, c, _)| format!("- {c}")).collect::<Vec<_>>().join("\n");
        let prompt = memoria_mcp::tools::reflection_prompt(&experiences, "");
        let msgs = vec![memoria_embedding::ChatMessage { role: "user".to_string(), content: prompt }];
        let raw = match llm.chat(&msgs, 0.3, Some(400)).await {
            Ok(r) => r, Err(e) => { warn!("reflect LLM chat failed: {e}"); continue },
        };
        let start = raw.find('[').unwrap_or(raw.len());
        let end = raw.rfind(']').map(|i| i + 1).unwrap_or(raw.len());
        if start >= end { continue; }
        let items: Vec<serde_json::Value> = match serde_json::from_str(&raw[start..end]) {
            Ok(v) => v, Err(e) => { warn!("reflect LLM response parse failed: {e}"); continue },
        };
        for item in &items {
            let content = item["content"].as_str().unwrap_or("").trim().to_string();
            if content.is_empty() { continue; }
            let mt_str = item["type"].as_str().unwrap_or("semantic");
            let mt = memoria_core::MemoryType::from_str(mt_str).unwrap_or(memoria_core::MemoryType::Semantic);
            let _ = state.service.store_memory(&user_id, &content, mt, None,
                Some(memoria_core::TrustTier::T4Unverified), None, None).await;
            scenes_created += 1;
        }
    }
    sql.set_cooldown(&user_id, "reflect").await.map_err(api_err)?;
    Ok(Json(json!({ "scenes_created": scenes_created, "candidates_found": clusters.len() })))
}

pub async fn extract_entities(
    State(state): State<AppState>,
    AuthUser(user_id): AuthUser,
    Json(req): Json<ExtractEntitiesRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let sql = state.service.sql_store.as_ref()
        .ok_or_else(|| (StatusCode::SERVICE_UNAVAILABLE, "SQL store required".to_string()))?;

    if req.mode == "internal" && state.service.llm.is_none() {
        return Err((StatusCode::SERVICE_UNAVAILABLE, "LLM_API_KEY not configured".to_string()));
    }

    let graph = sql.graph_store();
    let unlinked = graph.get_unlinked_memories(&user_id, 50).await.map_err(api_err)?;
    if unlinked.is_empty() {
        return Ok(Json(json!({ "status": "complete", "unlinked": 0 })));
    }

    if req.mode == "candidates" || state.service.llm.is_none() {
        let existing = graph.get_user_entities(&user_id).await.map_err(api_err)?;
        return Ok(Json(json!({
            "status": "candidates",
            "unlinked": unlinked.len(),
            "memories": unlinked.iter().map(|(mid, c)| json!({"memory_id": mid, "content": c})).collect::<Vec<_>>(),
            "existing_entities": existing.iter().map(|(n, t)| json!({"name": n, "entity_type": t})).collect::<Vec<_>>(),
        })));
    }

    let llm = state.service.llm.as_ref().unwrap();
    let mut total_created = 0usize;
    for (memory_id, content) in &unlinked {
        let prompt = memoria_mcp::tools::entity_extract_prompt(content);
        let msgs = vec![memoria_embedding::ChatMessage { role: "user".to_string(), content: prompt }];
        let raw = match llm.chat(&msgs, 0.0, Some(300)).await { Ok(r) => r, Err(e) => { warn!("entity extract LLM failed: {e}"); continue } };
        let start = raw.find('[').unwrap_or(raw.len());
        let end = raw.rfind(']').map(|i| i + 1).unwrap_or(raw.len());
        if start >= end { continue; }
        let items: Vec<serde_json::Value> = match serde_json::from_str(&raw[start..end]) { Ok(v) => v, Err(e) => { warn!("entity extract parse failed: {e}"); continue } };
        for item in &items {
            let name = item["name"].as_str().unwrap_or("").trim().to_lowercase();
            if name.is_empty() { continue; }
            let display = item["name"].as_str().unwrap_or("").trim().to_string();
            let etype = item["type"].as_str().unwrap_or("concept").to_string();
            if let Ok((entity_id, is_new)) = graph.upsert_entity(&user_id, &name, &display, &etype).await {
                let _ = graph.upsert_memory_entity_link(memory_id, &entity_id, &user_id, "llm").await;
                if is_new { total_created += 1; }
            }
        }
    }
    Ok(Json(json!({ "status": "done", "total_memories": unlinked.len(), "entities_found": total_created })))
}

pub async fn link_entities(
    State(state): State<AppState>,
    AuthUser(user_id): AuthUser,
    Json(req): Json<LinkEntitiesRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let sql = state.service.sql_store.as_ref()
        .ok_or_else(|| (StatusCode::SERVICE_UNAVAILABLE, "SQL store required".to_string()))?;
    let graph = sql.graph_store();
    let mut created = 0usize;
    let mut reused = 0usize;
    for link in &req.entities {
        for ent in &link.entities {
            let name = ent.name.trim().to_lowercase();
            if name.is_empty() { continue; }
            let (entity_id, is_new) = graph.upsert_entity(&user_id, &name, &ent.name, &ent.entity_type)
                .await.map_err(api_err)?;
            graph.upsert_memory_entity_link(&link.memory_id, &entity_id, &user_id, "manual")
                .await.map_err(api_err)?;
            if is_new { created += 1; } else { reused += 1; }
        }
    }
    Ok(Json(json!({ "entities_created": created, "entities_reused": reused, "edges_created": created })))
}

pub async fn get_entities(
    State(state): State<AppState>,
    AuthUser(user_id): AuthUser,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let sql = state.service.sql_store.as_ref()
        .ok_or_else(|| (StatusCode::SERVICE_UNAVAILABLE, "SQL store required".to_string()))?;
    let graph = sql.graph_store();
    let entities = graph.get_user_entities(&user_id).await.map_err(api_err)?;
    Ok(Json(json!({
        "entities": entities.iter().map(|(n, t)| json!({"name": n, "entity_type": t})).collect::<Vec<_>>()
    })))
}

use std::str::FromStr;
