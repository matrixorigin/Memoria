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
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::{auth::AuthUser, state::AppState};

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
fn default_mode() -> String { "full".to_string() }
fn default_true() -> bool { true }

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
    pub status: String, // "processing" | "completed" | "failed"
    pub created_at: String,
    pub updated_at: String,
    pub result: Option<serde_json::Value>,
    pub error: Option<serde_json::Value>,
}

// ── In-memory task store ──────────────────────────────────────────────────────

type TaskStore = Arc<Mutex<std::collections::HashMap<String, TaskStatus>>>;

pub fn new_task_store() -> TaskStore {
    Arc::new(Mutex::new(std::collections::HashMap::new()))
}

// ── LLM prompts ───────────────────────────────────────────────────────────────

const EPISODIC_PROMPT: &str = "You are analyzing a conversation session to create an episodic memory summary.\n\n\
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
    // Strip markdown fences
    let text = text.trim_start_matches("```json").trim_start_matches("```").trim_end_matches("```").trim();
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
    let sql = state.service.sql_store.as_ref()
        .ok_or("SQL store required")?;
    let llm = state.service.llm.as_ref()
        .ok_or("LLM not configured — set LLM_API_KEY")?;

    // Fetch session memories
    let rows = sqlx::query(
        "SELECT memory_id, content, memory_type FROM mem_memories \
         WHERE user_id = ? AND session_id = ? AND is_active = 1 \
         ORDER BY created_at ASC"
    )
    .bind(user_id).bind(session_id)
    .fetch_all(sql.pool()).await
    .map_err(|e| e.to_string())?;

    if rows.is_empty() {
        return Err(format!("No memories found for session {session_id}"));
    }

    // Build message text (truncate at 200 messages, 16k tokens)
    let messages: Vec<(String, String, String)> = rows.iter().filter_map(|r| {
        let mid: String = r.try_get("memory_id").ok()?;
        let content: String = r.try_get("content").ok()?;
        let mtype: String = r.try_get("memory_type").ok()?;
        Some((mid, content, mtype))
    }).take(200).collect();

    let truncated = messages.len() < rows.len();
    let msg_text = messages.iter()
        .map(|(_, c, t)| format!("user: [{t}] {}", &c[..c.len().min(500)]))
        .collect::<Vec<_>>().join("\n");

    if mode == "lightweight" {
        let prompt = LIGHTWEIGHT_PROMPT.replace("{messages}", &msg_text);
        let msgs = vec![memoria_embedding::ChatMessage { role: "user".to_string(), content: prompt }];
        let raw = llm.chat(&msgs, 0.3, Some(300)).await.map_err(|e| e.to_string())?;
        let json_str = extract_json(&raw);
        let data: serde_json::Value = serde_json::from_str(json_str)
            .map_err(|e| format!("LLM returned invalid JSON: {e}"))?;
        let points = data["points"].as_array()
            .ok_or("Expected 'points' array")?;
        let content = format!("Session Highlights:\n{}",
            points.iter().map(|p| format!("• {}", p.as_str().unwrap_or(""))).collect::<Vec<_>>().join("\n"));
        let metadata = json!({"mode": "lightweight", "points": points});
        let m = state.service.store_memory(user_id, &content, memoria_core::MemoryType::Episodic, None, None, None, None)
            .await.map_err(|e| e.to_string())?;
        Ok((m.memory_id, content, truncated, metadata))
    } else {
        let focus_clause = focus_topics.map(|t| format!("\nPay special attention to these topics: {}.\n", t.join(", "))).unwrap_or_default();
        let prompt = EPISODIC_PROMPT
            .replace("{messages}", &msg_text)
            .replace("{focus_clause}", &focus_clause);
        let msgs = vec![memoria_embedding::ChatMessage { role: "user".to_string(), content: prompt }];
        let raw = llm.chat(&msgs, 0.3, Some(500)).await.map_err(|e| e.to_string())?;
        let json_str = extract_json(&raw);
        let data: serde_json::Value = serde_json::from_str(json_str)
            .map_err(|e| format!("LLM returned invalid JSON: {e}"))?;
        let topic = data["topic"].as_str().unwrap_or("").to_string();
        let action = data["action"].as_str().unwrap_or("").to_string();
        let outcome = data["outcome"].as_str().unwrap_or("").to_string();
        if topic.is_empty() { return Err("LLM returned empty topic".to_string()); }
        let content = format!("Session Summary: {topic}\n\nActions: {action}\n\nOutcome: {outcome}");
        let metadata = json!({"mode": "full", "topic": topic, "action": action, "outcome": outcome, "session_id": session_id});
        let m = state.service.store_memory(user_id, &content, memoria_core::MemoryType::Episodic, None, None, None, None)
            .await.map_err(|e| e.to_string())?;
        Ok((m.memory_id, content, truncated, metadata))
    }
}

// ── Handlers ──────────────────────────────────────────────────────────────────

pub async fn create_session_summary(
    State(state): State<AppState>,
    AuthUser(user_id): AuthUser,
    Path(session_id): Path<String>,
    Json(req): Json<SessionSummaryRequest>,
) -> Result<Json<SessionSummaryResponse>, (StatusCode, String)> {
    if state.service.llm.is_none() {
        return Err((StatusCode::SERVICE_UNAVAILABLE,
            "LLM not configured — set LLM_API_KEY to enable episodic memory".to_string()));
    }

    if req.sync {
        // Synchronous: generate and return immediately
        let result = generate_and_store(
            &state, &user_id, &session_id, &req.mode,
            req.focus_topics.as_deref(),
        ).await.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;

        return Ok(Json(SessionSummaryResponse {
            memory_id: Some(result.0),
            task_id: None,
            content: Some(result.1),
            truncated: result.2,
            metadata: Some(result.3),
            mode: req.mode,
        }));
    }

    // Async: spawn task
    let task_id = uuid::Uuid::new_v4().simple().to_string();
    let now = chrono::Utc::now().to_rfc3339();
    let task = TaskStatus {
        task_id: task_id.clone(),
        status: "processing".to_string(),
        created_at: now.clone(),
        updated_at: now,
        result: None,
        error: None,
    };

    state.tasks.lock().await.insert(task_id.clone(), task);

    // Spawn background task
    let state_clone = state.clone();
    let tid = task_id.clone();
    let uid = user_id.clone();
    let sid = session_id.clone();
    let mode = req.mode.clone();
    let focus = req.focus_topics.clone();

    tokio::spawn(async move {
        let result = generate_and_store(&state_clone, &uid, &sid, &mode, focus.as_deref()).await;
        let now = chrono::Utc::now().to_rfc3339();
        let mut tasks = state_clone.tasks.lock().await;
        if let Some(task) = tasks.get_mut(&tid) {
            task.updated_at = now;
            match result {
                Ok((mid, content, truncated, metadata)) => {
                    task.status = "completed".to_string();
                    task.result = Some(json!({
                        "memory_id": mid, "content": content,
                        "truncated": truncated, "metadata": metadata
                    }));
                }
                Err(e) => {
                    task.status = "failed".to_string();
                    task.error = Some(json!({"code": "GENERATION_ERROR", "message": e}));
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
    AuthUser(_): AuthUser,
    Path(task_id): Path<String>,
) -> Result<Json<TaskStatus>, (StatusCode, String)> {
    let tasks = state.tasks.lock().await;
    tasks.get(&task_id)
        .cloned()
        .map(Json)
        .ok_or_else(|| (StatusCode::NOT_FOUND, format!("Task {task_id} not found")))
}
