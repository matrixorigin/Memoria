//! Plugin management endpoints — publish, review, score, bind, audit.
//! All routes require master key auth.

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use memoria_service::plugin;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::{auth::AuthUser, routes::memory::api_err, state::AppState};

fn get_store(state: &AppState) -> Result<&memoria_storage::SqlMemoryStore, (StatusCode, String)> {
    state
        .service
        .sql_store
        .as_deref()
        .ok_or((StatusCode::INTERNAL_SERVER_ERROR, "No SQL store".into()))
}

// ── Types ────────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct SignerRequest {
    pub signer: String,
    pub public_key: String,
    #[serde(default = "default_actor")]
    pub actor: String,
}

fn default_actor() -> String {
    "api".into()
}

#[derive(Serialize)]
pub struct SignerResponse {
    pub signers: Vec<plugin::TrustedPluginSignerEntry>,
}

/// Publish request: files map (filename → base64 content). Must include `manifest.json`.
#[derive(Deserialize)]
pub struct PublishRequest {
    pub files: HashMap<String, String>,
    #[serde(default = "default_actor")]
    pub actor: String,
}

#[derive(Deserialize)]
pub struct ReviewRequest {
    pub status: String,
    pub notes: Option<String>,
    #[serde(default = "default_actor")]
    pub actor: String,
}

#[derive(Deserialize)]
pub struct ScoreRequest {
    pub score: f64,
    pub notes: Option<String>,
    #[serde(default = "default_actor")]
    pub actor: String,
}

#[derive(Deserialize)]
pub struct BindingRequest {
    #[serde(default = "default_binding")]
    pub binding_key: String,
    #[serde(default = "default_subject")]
    pub subject_key: String,
    #[serde(default = "default_priority")]
    pub priority: i64,
    pub plugin_key: String,
    #[serde(default = "default_selector_kind")]
    pub selector_kind: String,
    #[serde(default)]
    pub selector_value: String,
    #[serde(default = "default_rollout")]
    pub rollout_percent: i64,
    pub transport_endpoint: Option<String>,
    #[serde(default = "default_actor")]
    pub actor: String,
}

fn default_binding() -> String {
    "default".into()
}
fn default_subject() -> String {
    "*".into()
}
fn default_priority() -> i64 {
    100
}
fn default_selector_kind() -> String {
    "semver".into()
}
fn default_rollout() -> i64 {
    100
}

#[derive(Deserialize)]
pub struct ActivateRequest {
    pub plugin_key: String,
    pub version: String,
    #[serde(default = "default_binding")]
    pub binding_key: String,
    #[serde(default = "default_actor")]
    pub actor: String,
}

#[derive(Deserialize)]
pub struct ListQuery {
    pub domain: Option<String>,
}

#[derive(Deserialize)]
pub struct EventsQuery {
    pub domain: Option<String>,
    pub plugin_key: Option<String>,
    pub binding: Option<String>,
    #[serde(default = "default_events_limit")]
    pub limit: usize,
}

fn default_events_limit() -> usize {
    20
}

#[derive(Deserialize)]
pub struct RulesQuery {
    #[serde(default = "default_binding")]
    pub binding: String,
}

// ── Handlers ─────────────────────────────────────────────────────────────────

pub async fn list_signers(
    State(state): State<AppState>,
    _user: AuthUser,
) -> Result<Json<SignerResponse>, (StatusCode, String)> {
    let store = get_store(&state)?;
    let signers = plugin::list_trusted_plugin_signers(store)
        .await
        .map_err(api_err)?;
    Ok(Json(SignerResponse { signers }))
}

pub async fn upsert_signer(
    State(state): State<AppState>,
    _user: AuthUser,
    Json(req): Json<SignerRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let store = get_store(&state)?;
    plugin::upsert_trusted_plugin_signer(store, &req.signer, &req.public_key, &req.actor)
        .await
        .map_err(api_err)?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

pub async fn publish_package(
    State(state): State<AppState>,
    _user: AuthUser,
    Json(req): Json<PublishRequest>,
) -> Result<Json<plugin::PluginRepositoryEntry>, (StatusCode, String)> {
    if !req.files.contains_key("manifest.json") {
        return Err((
            StatusCode::BAD_REQUEST,
            "files must include manifest.json".into(),
        ));
    }
    let dir =
        tempfile::tempdir().map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    for (name, b64) in &req.files {
        // Reject path traversal
        if name.contains("..") || name.starts_with('/') {
            return Err((StatusCode::BAD_REQUEST, format!("invalid filename: {name}")));
        }
        let bytes = BASE64.decode(b64).map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                format!("base64 decode {name}: {e}"),
            )
        })?;
        std::fs::write(dir.path().join(name), bytes)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    }
    let store = get_store(&state)?;
    let entry = plugin::publish_plugin_package(store, dir.path(), &req.actor)
        .await
        .map_err(api_err)?;
    Ok(Json(entry))
}

pub async fn list_packages(
    State(state): State<AppState>,
    _user: AuthUser,
    Query(q): Query<ListQuery>,
) -> Result<Json<Vec<plugin::PluginRepositoryEntry>>, (StatusCode, String)> {
    let store = get_store(&state)?;
    let entries = plugin::list_plugin_repository_entries(store, q.domain.as_deref())
        .await
        .map_err(api_err)?;
    Ok(Json(entries))
}

pub async fn review_package(
    State(state): State<AppState>,
    _user: AuthUser,
    Path((plugin_key, version)): Path<(String, String)>,
    Json(req): Json<ReviewRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let store = get_store(&state)?;
    plugin::review_plugin_package(
        store,
        &plugin_key,
        &version,
        &req.status,
        req.notes.as_deref(),
        &req.actor,
    )
    .await
    .map_err(api_err)?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

pub async fn score_package(
    State(state): State<AppState>,
    _user: AuthUser,
    Path((plugin_key, version)): Path<(String, String)>,
    Json(req): Json<ScoreRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let store = get_store(&state)?;
    plugin::score_plugin_package(
        store,
        &plugin_key,
        &version,
        req.score,
        req.notes.as_deref(),
        &req.actor,
    )
    .await
    .map_err(api_err)?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

pub async fn upsert_binding_rule(
    State(state): State<AppState>,
    _user: AuthUser,
    Path(domain): Path<String>,
    Json(req): Json<BindingRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let store = get_store(&state)?;
    plugin::upsert_plugin_binding_rule(
        store,
        plugin::BindingRuleInput {
            domain: &domain,
            binding_key: &req.binding_key,
            subject_key: &req.subject_key,
            priority: req.priority,
            plugin_key: &req.plugin_key,
            selector_kind: &req.selector_kind,
            selector_value: &req.selector_value,
            rollout_percent: req.rollout_percent,
            transport_endpoint: req.transport_endpoint.as_deref(),
            actor: &req.actor,
        },
    )
    .await
    .map_err(api_err)?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

pub async fn activate_binding(
    State(state): State<AppState>,
    _user: AuthUser,
    Path(domain): Path<String>,
    Json(req): Json<ActivateRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let store = get_store(&state)?;
    plugin::activate_plugin_binding(
        store,
        &domain,
        &req.binding_key,
        &req.plugin_key,
        &req.version,
        &req.actor,
    )
    .await
    .map_err(api_err)?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

pub async fn list_binding_rules(
    State(state): State<AppState>,
    _user: AuthUser,
    Path(domain): Path<String>,
    Query(q): Query<RulesQuery>,
) -> Result<Json<Vec<plugin::PluginBindingRule>>, (StatusCode, String)> {
    let store = get_store(&state)?;
    let rules = plugin::list_binding_rules(store, &domain, &q.binding)
        .await
        .map_err(api_err)?;
    Ok(Json(rules))
}

pub async fn list_compatibility_matrix(
    State(state): State<AppState>,
    _user: AuthUser,
    Query(q): Query<ListQuery>,
) -> Result<Json<Vec<plugin::PluginCompatibilityEntry>>, (StatusCode, String)> {
    let store = get_store(&state)?;
    let entries = plugin::list_plugin_compatibility_matrix(store, q.domain.as_deref())
        .await
        .map_err(api_err)?;
    Ok(Json(entries))
}

pub async fn list_audit_events(
    State(state): State<AppState>,
    _user: AuthUser,
    Query(q): Query<EventsQuery>,
) -> Result<Json<Vec<plugin::PluginAuditEvent>>, (StatusCode, String)> {
    let store = get_store(&state)?;
    let events = plugin::get_plugin_audit_events(
        store,
        q.domain.as_deref(),
        q.plugin_key.as_deref(),
        q.binding.as_deref(),
        q.limit,
    )
    .await
    .map_err(api_err)?;
    Ok(Json(events))
}
