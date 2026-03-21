//! Episodic memory generation from session memories.
//! POST /v1/sessions/{session_id}/summary
//! GET  /v1/tasks/{task_id}

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::Row;

use crate::{auth::AuthUser, state::AppState};
use memoria_core::truncate_utf8;

// ── Request / Response ────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct SessionSummaryRequest {
    #[serde(default = "default_mode")]
    pub mode: String,
    #[serde(default)]
    pub sync: bool,
    pub focus_topics: Option<Vec<String>>,
    #[serde(default = "default_true")]
    pub generate_embedding: bool,
}
fn default_mode() -> String {
    "full".to_string()
}
fn default_true() -> bool {
    true
}

#[derive(Serialize)]
pub struct SessionSummaryResponse {
    pub memory_id: Option<String>,
    pub task_id: Option<String>,
    pub content: Option<String>,
    pub truncated: bool,
    pub metadata: Option<serde_json::Value>,
    pub mode: String,
}

#[derive(Serialize, Clone)]
pub struct TaskStatus {
    pub task_id: String,
    pub status: String,
    pub created_at: String,
    pub updated_at: String,
    pub result: Option<serde_json::Value>,
    pub error: Option<serde_json::Value>,
}

// ── LLM prompts ───────────────────────────────────────────────────────────────

const EPISODIC_PROMPT: &str =
    "You are analyzing a conversation session to create an episodic memory summary.\n\n\
Extract the following information from the conversation:\n\
1. **Topic**: The main subject or theme discussed (1-2 sentences)\n\
2. **Action**: Key actions, decisions, or activities performed (2-3 sentences)\n\
3. **Outcome**: Results, conclusions, or current state (1-2 sentences)\n\n\
Be concise and factual. Focus on what was accomplished, not how the conversation flowed.\n\
{focus_clause}\n\
Conversation messages:\n{messages}\n\n\
Respond with a JSON object containing: topic, action, outcome";

const LIGHTWEIGHT_PROMPT: &str = "Summarize this conversation segment into 3-5 key points.\n\n\
Focus on:\n- What was discussed or decided\n- Actions taken or planned\n- Important facts or conclusions\n\n\
Be extremely concise (each point max 10 words).\n\n\
Conversation:\n{messages}\n\n\
Respond with a JSON object: {\"points\": [\"point 1\", \"point 2\", ...]}";

fn extract_json(text: &str) -> &str {
    let text = text.trim();
    let text = text
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();
    text
}

// ── Core generation logic ─────────────────────────────────────────────────────

async fn generate_and_store(
    state: &AppState,
    user_id: &str,
    session_id: &str,
    mode: &str,
    focus_topics: Option<&[String]>,
) -> Result<(String, String, bool, serde_json::Value), String> {
    let sql = state
        .service
        .sql_store
        .as_ref()
        .ok_or("SQL store required")?;
    let llm = state
        .service
        .llm
        .as_ref()
        .ok_or("LLM not configured — set LLM_API_KEY")?;

    let rows = sqlx::query(
        "SELECT memory_id, content, memory_type FROM mem_memories \
         WHERE user_id = ? AND session_id = ? AND is_active = 1 \
         ORDER BY created_at ASC",
    )
    .bind(user_id)
    .bind(session_id)
    .fetch_all(sql.pool())
    .await
    .map_err(|e| e.to_string())?;

    if rows.is_empty() {
        return Err(format!("No memories found for session {session_id}"));
    }

    let messages: Vec<(String, String, String)> = rows
        .iter()
        .filter_map(|r| {
            let mid: String = r.try_get("memory_id").ok()?;
            let content: String = r.try_get("content").ok()?;
            let mtype: String = r.try_get("memory_type").ok()?;
            Some((mid, content, mtype))
        })
        .take(200)
        .collect();

    let truncated = messages.len() < rows.len();
    let msg_text = messages
        .iter()
        .map(|(_, c, t)| format!("user: [{t}] {}", truncate_utf8(c, 500)))
        .collect::<Vec<_>>()
        .join("\n");

    if mode == "lightweight" {
        let prompt = LIGHTWEIGHT_PROMPT.replace("{messages}", &msg_text);
        let msgs = vec![memoria_embedding::ChatMessage {
            role: "user".to_string(),
            content: prompt,
        }];
        let raw = llm
            .chat(&msgs, 0.3, Some(300))
            .await
            .map_err(|e| e.to_string())?;
        let json_str = extract_json(&raw);
        let data: serde_json::Value = serde_json::from_str(json_str)
            .map_err(|e| format!("LLM returned invalid JSON: {e}"))?;
        let points = data["points"].as_array().ok_or("Expected 'points' array")?;
        let content = format!(
            "Session Highlights:\n{}",
            points
                .iter()
                .map(|p| format!("• {}", p.as_str().unwrap_or("")))
                .collect::<Vec<_>>()
                .join("\n")
        );
        let metadata = json!({"mode": "lightweight", "points": points});
        let m = state
            .service
            .store_memory(
                user_id,
                &content,
                memoria_core::MemoryType::Episodic,
                None,
                None,
                None,
                None,
            )
            .await
            .map_err(|e| e.to_string())?;
        Ok((m.memory_id, content, truncated, metadata))
    } else {
        let focus_clause = focus_topics
            .map(|t| {
                format!(
                    "\nPay special attention to these topics: {}.\n",
                    t.join(", ")
                )
            })
            .unwrap_or_default();
        let prompt = EPISODIC_PROMPT
            .replace("{messages}", &msg_text)
            .replace("{focus_clause}", &focus_clause);
        let msgs = vec![memoria_embedding::ChatMessage {
            role: "user".to_string(),
            content: prompt,
        }];
        let raw = llm
            .chat(&msgs, 0.3, Some(500))
            .await
            .map_err(|e| e.to_string())?;
        let json_str = extract_json(&raw);
        let data: serde_json::Value = serde_json::from_str(json_str)
            .map_err(|e| format!("LLM returned invalid JSON: {e}"))?;
        let topic = data["topic"].as_str().unwrap_or("").to_string();
        let action = data["action"].as_str().unwrap_or("").to_string();
        let outcome = data["outcome"].as_str().unwrap_or("").to_string();
        if topic.is_empty() {
            return Err("LLM returned empty topic".to_string());
        }
        let content =
            format!("Session Summary: {topic}\n\nActions: {action}\n\nOutcome: {outcome}");
        let metadata = json!({"mode": "full", "topic": topic, "action": action, "outcome": outcome, "session_id": session_id});
        let m = state
            .service
            .store_memory(
                user_id,
                &content,
                memoria_core::MemoryType::Episodic,
                None,
                None,
                None,
                None,
            )
            .await
            .map_err(|e| e.to_string())?;
        Ok((m.memory_id, content, truncated, metadata))
    }
}

// ── Handlers ──────────────────────────────────────────────────────────────────

pub async fn create_session_summary(
    State(state): State<AppState>,
    AuthUser { user_id, .. }: AuthUser,
    Path(session_id): Path<String>,
    Json(req): Json<SessionSummaryRequest>,
) -> Result<Json<SessionSummaryResponse>, (StatusCode, String)> {
    if state.service.llm.is_none() {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "LLM not configured — set LLM_API_KEY to enable episodic memory".to_string(),
        ));
    }

    if req.sync {
        let result = generate_and_store(
            &state,
            &user_id,
            &session_id,
            &req.mode,
            req.focus_topics.as_deref(),
        )
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;

        return Ok(Json(SessionSummaryResponse {
            memory_id: Some(result.0),
            task_id: None,
            content: Some(result.1),
            truncated: result.2,
            metadata: Some(result.3),
            mode: req.mode,
        }));
    }

    // Async: create task in DB
    let task_store = state.task_store.as_ref().ok_or((
        StatusCode::INTERNAL_SERVER_ERROR,
        "Task store not available".to_string(),
    ))?;

    let task_id = uuid::Uuid::new_v4().simple().to_string();
    task_store
        .create_task(&task_id, &state.instance_id, &user_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let state_clone = state.clone();
    let tid = task_id.clone();
    let uid = user_id.clone();
    let sid = session_id.clone();
    let mode = req.mode.clone();
    let focus = req.focus_topics.clone();

    tokio::spawn(async move {
        let result = generate_and_store(&state_clone, &uid, &sid, &mode, focus.as_deref()).await;
        if let Some(ts) = &state_clone.task_store {
            match result {
                Ok((mid, content, truncated, metadata)) => {
                    let _ = ts
                        .complete_task(
                            &tid,
                            json!({
                                "memory_id": mid, "content": content,
                                "truncated": truncated, "metadata": metadata
                            }),
                        )
                        .await;
                }
                Err(e) => {
                    let _ = ts
                        .fail_task(&tid, json!({"code": "GENERATION_ERROR", "message": e}))
                        .await;
                }
            }
        }
    });

    Ok(Json(SessionSummaryResponse {
        memory_id: None,
        task_id: Some(task_id),
        content: None,
        truncated: false,
        metadata: None,
        mode: req.mode,
    }))
}

pub async fn get_task_status(
    State(state): State<AppState>,
    AuthUser { user_id, is_master }: AuthUser,
    Path(task_id): Path<String>,
) -> Result<Json<TaskStatus>, (StatusCode, String)> {
    let task_store = state.task_store.as_ref().ok_or((
        StatusCode::INTERNAL_SERVER_ERROR,
        "Task store not available".to_string(),
    ))?;

    let task = task_store
        .get_task(&task_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    match task {
        Some(t) => {
            if !is_master && t.user_id != user_id {
                return Err((StatusCode::FORBIDDEN, "Not your task".to_string()));
            }
            Ok(Json(TaskStatus {
                task_id: t.task_id,
                status: t.status,
                created_at: t.created_at,
                updated_at: t.updated_at,
                result: t.result,
                error: t.error,
            }))
        }
        None => Err((StatusCode::NOT_FOUND, format!("Task {task_id} not found"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_utf8_at_multibyte_boundary() {
        // '，' is 3 bytes (0xEF 0xBC 0x8C). Place it so byte 500 lands inside it.
        let prefix = "a".repeat(499); // 499 ASCII bytes
        let s = format!("{prefix}，after");
        // byte 500 is inside '，' (bytes 499..502), must round down to 499
        assert_eq!(truncate_utf8(&s, 500), &prefix);
    }

    #[test]
    fn truncate_utf8_ascii_exact() {
        let s = "a".repeat(600);
        assert_eq!(truncate_utf8(&s, 500).len(), 500);
    }

    #[test]
    fn truncate_utf8_short_string() {
        assert_eq!(truncate_utf8("hello", 500), "hello");
    }
}
