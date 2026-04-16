//! Remote mode: proxy all MCP tool calls to a Memoria REST API server.
//! Mirrors Python's HTTPBackend.

use anyhow::Result;
use reqwest::Client;
use serde_json::{json, Value};

pub struct RemoteClient {
    client: Client,
    base_url: String,
    #[allow(dead_code)]
    user_id: String,
}

impl RemoteClient {
    pub fn new(api_url: &str, token: Option<&str>, user_id: String, tool: Option<&str>) -> Self {
        let mut headers = reqwest::header::HeaderMap::new();
        if let Some(t) = token {
            if let Ok(v) = reqwest::header::HeaderValue::from_str(&format!("Bearer {t}")) {
                headers.insert(reqwest::header::AUTHORIZATION, v);
            }
        }
        // Set X-User-Id header
        if let Ok(v) = reqwest::header::HeaderValue::from_str(&user_id) {
            headers.insert("X-User-Id", v);
        }
        // Set X-Memoria-Tool header (kiro / cursor / claude / codex)
        if let Some(t) = tool {
            if let Ok(v) = reqwest::header::HeaderValue::from_str(t) {
                headers.insert("X-Memoria-Tool", v);
            }
        }
        let client = Client::builder()
            .default_headers(headers)
            .no_proxy()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .expect("http client");
        Self {
            client,
            base_url: api_url.trim_end_matches('/').to_string(),
            user_id,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    fn mcp_text(text: &str) -> Value {
        json!({"content": [{"type": "text", "text": text}]})
    }

    fn mcp_json(v: &Value) -> Value {
        Self::mcp_text(&serde_json::to_string_pretty(v).unwrap_or_default())
    }

    #[allow(dead_code)]
    fn mcp_err(e: impl std::fmt::Display) -> Value {
        Self::mcp_text(&format!("Error: {e}"))
    }

    async fn parse_response(r: reqwest::Response) -> Result<Value> {
        let status = r.status();
        if status.is_success() {
            return Ok(r.json().await?);
        }
        let body = r.text().await.unwrap_or_default();
        let msg = if body.is_empty() {
            status.to_string()
        } else {
            body
        };
        anyhow::bail!("API error {status}: {msg}")
    }

    pub async fn call(&self, name: &str, args: Value) -> Result<Value> {
        match name {
            "memory_store" => {
                let r = self
                    .client
                    .post(self.url("/v1/memories"))
                    .json(&json!({
                        "content": args["content"],
                        "memory_type": args["memory_type"].as_str().unwrap_or("semantic"),
                        "session_id": args["session_id"],
                        "trust_tier": args["trust_tier"],
                    }))
                    .send()
                    .await?;
                let body = Self::parse_response(r).await?;
                Ok(Self::mcp_text(&format!(
                    "Stored memory {}: {}",
                    body["memory_id"].as_str().unwrap_or(""),
                    body["content"].as_str().unwrap_or("")
                )))
            }

            "memory_retrieve" | "memory_search" => {
                let path = if name == "memory_search" {
                    "/v1/memories/search"
                } else {
                    "/v1/memories/retrieve"
                };
                let mut payload = json!({
                    "query": args["query"],
                    "top_k": args["top_k"].as_i64().unwrap_or(5),
                    "session_id": args["session_id"],
                });
                if name == "memory_retrieve" {
                    if let Some(filter_session) = args
                        .get("filter_session")
                        .and_then(serde_json::Value::as_bool)
                    {
                        payload["filter_session"] = json!(filter_session);
                    }
                    if let Some(include_cross_session) = args
                        .get("include_cross_session")
                        .and_then(serde_json::Value::as_bool)
                    {
                        payload["include_cross_session"] = json!(include_cross_session);
                    }
                }
                let r = self
                    .client
                    .post(self.url(path))
                    .json(&payload)
                    .send()
                    .await?;
                let body = Self::parse_response(r).await?;
                let mems = body.as_array().cloned().unwrap_or_default();
                if mems.is_empty() {
                    return Ok(Self::mcp_text("No relevant memories found."));
                }
                let text = mems
                    .iter()
                    .map(|m| {
                        format!(
                            "[{}] ({}) {}",
                            m["memory_id"].as_str().unwrap_or(""),
                            m["memory_type"].as_str().unwrap_or(""),
                            m["content"].as_str().unwrap_or("")
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                Ok(Self::mcp_text(&text))
            }

            "memory_correct" => {
                let new_content = args["new_content"].as_str().unwrap_or("");
                let memory_id = args["memory_id"].as_str().unwrap_or("");
                let query = args["query"].as_str().unwrap_or("");
                let r = if !memory_id.is_empty() {
                    self.client
                        .put(self.url(&format!("/v1/memories/{memory_id}/correct")))
                        .json(&json!({"new_content": new_content, "reason": args["reason"]}))
                        .send()
                        .await?
                } else {
                    self.client.post(self.url("/v1/memories/correct"))
                        .json(&json!({"query": query, "new_content": new_content, "reason": args["reason"]}))
                        .send().await?
                };
                let body = Self::parse_response(r).await?;
                if let Some(_err) = body.get("error") {
                    return Ok(Self::mcp_text(&format!(
                        "No matching memory found for query '{query}'"
                    )));
                }
                Ok(Self::mcp_text(&format!(
                    "Corrected memory {}: {}",
                    body["memory_id"].as_str().unwrap_or(""),
                    body["content"].as_str().unwrap_or("")
                )))
            }

            "memory_purge" => {
                let memory_id = args["memory_id"].as_str().unwrap_or("");
                let topic = args["topic"].as_str().unwrap_or("");
                if !memory_id.is_empty() {
                    let ids: Vec<&str> = memory_id
                        .split(',')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .collect();
                    let count = ids.len();
                    let r = self
                        .client
                        .post(self.url("/v1/memories/purge"))
                        .json(&json!({"memory_ids": ids}))
                        .send()
                        .await?;
                    let body = Self::parse_response(r).await?;
                    Ok(Self::mcp_text(&format!(
                        "Purged {} memory(s)",
                        body["purged"].as_i64().unwrap_or(count as i64)
                    )))
                } else if !topic.is_empty() {
                    let r = self
                        .client
                        .post(self.url("/v1/memories/purge"))
                        .json(&json!({"topic": topic}))
                        .send()
                        .await?;
                    let body = Self::parse_response(r).await?;
                    Ok(Self::mcp_text(&format!(
                        "Purged {} memory(s) matching '{topic}'",
                        body["purged"].as_i64().unwrap_or(0)
                    )))
                } else {
                    Ok(Self::mcp_text("Provide memory_id or topic"))
                }
            }

            "memory_profile" => {
                let r = self.client.get(self.url("/v1/profiles/me")).send().await?;
                let body = Self::parse_response(r).await?;
                let profile = body["profile"].as_str().unwrap_or("");
                if profile.is_empty() {
                    Ok(Self::mcp_text("No profile memories found."))
                } else {
                    Ok(Self::mcp_text(profile))
                }
            }

            "memory_list" => {
                let limit = args["limit"].as_i64().unwrap_or(20);
                let r = self
                    .client
                    .get(self.url(&format!("/v1/memories?limit={limit}")))
                    .send()
                    .await?;
                let body = Self::parse_response(r).await?;
                let items = body["items"].as_array().cloned().unwrap_or_default();
                if items.is_empty() {
                    return Ok(Self::mcp_text("No memories found."));
                }
                let text = items
                    .iter()
                    .map(|m| {
                        format!(
                            "[{}] ({}) {}",
                            m["memory_id"].as_str().unwrap_or(""),
                            m["memory_type"].as_str().unwrap_or(""),
                            m["content"].as_str().unwrap_or("")
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                Ok(Self::mcp_text(&text))
            }

            "memory_capabilities" => Ok(Self::mcp_text(&format!(
                "{}\n[remote mode — connected to Memoria API server]",
                crate::tools::MEMORY_CAPABILITIES_TEXT
            ))),

            "memory_get_retrieval_params" => {
                let r = self
                    .client
                    .get(self.url("/v1/retrieval-params"))
                    .send()
                    .await?;
                let body = Self::parse_response(r).await?;
                Ok(Self::mcp_text(&serde_json::to_string_pretty(&body)?))
            }

            "memory_tune_params" => {
                let r = self
                    .client
                    .post(self.url("/v1/retrieval-params/tune"))
                    .send()
                    .await?;
                let body = Self::parse_response(r).await?;
                if body["tuned"].as_bool().unwrap_or(false) {
                    let old = &body["old_params"];
                    let new = &body["new_params"];
                    Ok(Self::mcp_text(&format!(
                        "Parameters tuned:\n  feedback_weight: {:.3} → {:.3}\n  temporal_decay_hours: {:.1} → {:.1}\n  confidence_weight: {:.3} → {:.3}",
                        old["feedback_weight"].as_f64().unwrap_or(0.0),
                        new["feedback_weight"].as_f64().unwrap_or(0.0),
                        old["temporal_decay_hours"].as_f64().unwrap_or(0.0),
                        new["temporal_decay_hours"].as_f64().unwrap_or(0.0),
                        old["confidence_weight"].as_f64().unwrap_or(0.0),
                        new["confidence_weight"].as_f64().unwrap_or(0.0)
                    )))
                } else {
                    Ok(Self::mcp_text(
                        body["message"]
                            .as_str()
                            .unwrap_or("Not enough feedback to tune parameters"),
                    ))
                }
            }

            "memory_governance" => {
                let r = self
                    .client
                    .post(self.url("/v1/governance"))
                    .json(&json!({"force": args["force"].as_bool().unwrap_or(false)}))
                    .send()
                    .await?;
                let body = Self::parse_response(r).await?;
                if body["skipped"].as_bool().unwrap_or(false) {
                    return Ok(Self::mcp_text(&format!(
                        "Governance skipped (cooldown: {}s remaining).",
                        body["cooldown_remaining_s"].as_i64().unwrap_or(0)
                    )));
                }
                Ok(Self::mcp_text(&format!(
                    "Governance complete: quarantined={}, cleaned_stale={}",
                    body["quarantined"].as_i64().unwrap_or(0),
                    body["cleaned_stale"].as_i64().unwrap_or(0)
                )))
            }

            "memory_rebuild_index" => {
                let _r = self
                    .client
                    .post(self.url("/v1/governance"))
                    .json(&json!({"force": true}))
                    .send()
                    .await?;
                Ok(Self::mcp_text(
                    "Index rebuild requested via governance endpoint.",
                ))
            }

            "memory_consolidate" => {
                let r = self
                    .client
                    .post(self.url("/v1/consolidate"))
                    .json(&json!({"force": args["force"].as_bool().unwrap_or(false)}))
                    .send()
                    .await?;
                let body = Self::parse_response(r).await?;
                if body["skipped"].as_bool().unwrap_or(false) {
                    return Ok(Self::mcp_text(&format!(
                        "Consolidation skipped (cooldown: {}s remaining).",
                        body["cooldown_remaining_s"].as_i64().unwrap_or(0)
                    )));
                }
                Ok(Self::mcp_text(&format!("Consolidation complete: conflicts_detected={}, orphaned_scenes={}, promoted={}, demoted={}",
                    body["conflicts_detected"].as_i64().unwrap_or(0),
                    body["orphaned_scenes"].as_i64().unwrap_or(0),
                    body["promoted"].as_i64().unwrap_or(0),
                    body["demoted"].as_i64().unwrap_or(0))))
            }

            "memory_reflect" => {
                let r = self
                    .client
                    .post(self.url("/v1/reflect"))
                    .json(&json!({
                        "force": args["force"].as_bool().unwrap_or(false),
                        "mode": args["mode"].as_str().unwrap_or("auto"),
                    }))
                    .send()
                    .await?;
                let body = Self::parse_response(r).await?;
                if body["skipped"].as_bool().unwrap_or(false) {
                    return Ok(Self::mcp_text(&format!(
                        "Reflection skipped (cooldown: {}s remaining).",
                        body["cooldown_remaining_s"].as_i64().unwrap_or(0)
                    )));
                }
                if let Some(candidates) = body["candidates"].as_array() {
                    if !candidates.is_empty() {
                        let parts: Vec<String> = candidates
                            .iter()
                            .enumerate()
                            .map(|(i, c)| {
                                let signal = c["signal"].as_str().unwrap_or("cluster");
                                let importance = c["importance"].as_f64().unwrap_or(0.5);
                                let mems: Vec<String> = c["memories"]
                                    .as_array()
                                    .unwrap_or(&vec![])
                                    .iter()
                                    .map(|m| format!("  - {}", m["content"].as_str().unwrap_or("")))
                                    .collect();
                                format!(
                                    "Cluster {} ({signal}, importance={importance:.3}):\n{}",
                                    i + 1,
                                    mems.join("\n")
                                )
                            })
                            .collect();
                        return Ok(Self::mcp_text(&format!(
                            "Here are memory clusters for reflection. Synthesize 1-2 insights per cluster, \
                             then store each via memory_store.\n\n{}", parts.join("\n\n"))));
                    }
                }
                Ok(Self::mcp_text(&format!(
                    "Reflection complete: scenes_created={}, candidates_found={}",
                    body["scenes_created"].as_i64().unwrap_or(0),
                    body["candidates_found"].as_i64().unwrap_or(0)
                )))
            }

            "memory_extract_entities" => {
                let mode = args["mode"].as_str().unwrap_or("auto");
                let r = self
                    .client
                    .post(self.url("/v1/extract-entities"))
                    .json(&json!({"mode": mode}))
                    .send()
                    .await?;
                let body = Self::parse_response(r).await?;
                Ok(Self::mcp_text(&serde_json::to_string(&body)?))
            }

            "memory_link_entities" => {
                let entities_str = args["entities"].as_str().unwrap_or("[]");
                let entities: Value = serde_json::from_str(entities_str).unwrap_or(json!([]));
                let r = self
                    .client
                    .post(self.url("/v1/extract-entities/link"))
                    .json(&json!({"entities": entities}))
                    .send()
                    .await?;
                let body = Self::parse_response(r).await?;
                Ok(Self::mcp_text(&serde_json::to_string(&body)?))
            }

            // Git tools — fully supported (unlike Python which returns "Not available via HTTP")
            "memory_snapshot" => {
                let r = self
                    .client
                    .post(self.url("/v1/snapshots"))
                    .json(&json!({"name": args["name"], "description": args["description"]}))
                    .send()
                    .await?;
                let _body = Self::parse_response(r).await?;
                Ok(Self::mcp_text(&format!(
                    "Snapshot '{}' created.",
                    args["name"].as_str().unwrap_or("")
                )))
            }

            "memory_snapshots" => {
                let limit = args["limit"].as_i64().unwrap_or(20);
                let offset = args["offset"].as_i64().unwrap_or(0);
                let r = self
                    .client
                    .get(self.url(&format!("/v1/snapshots?limit={limit}&offset={offset}")))
                    .send()
                    .await?;
                let body = Self::parse_response(r).await?;
                Ok(Self::mcp_json(&body))
            }

            "memory_snapshot_delete" => {
                // names can be a single string or array
                let mut payload = args.clone();
                if let Some(names_str) = args["names"].as_str() {
                    // Convert comma-separated string to array
                    let names: Vec<&str> = names_str.split(',').map(str::trim).collect();
                    payload["names"] = json!(names);
                }
                let r = self
                    .client
                    .post(self.url("/v1/snapshots/delete"))
                    .json(&payload)
                    .send()
                    .await?;
                let body = Self::parse_response(r).await?;
                Ok(Self::mcp_text(&format!(
                    "Deleted {} snapshot(s).",
                    body["deleted"].as_i64().unwrap_or(0)
                )))
            }

            "memory_rollback" => {
                let name = args["name"].as_str().unwrap_or("");
                let r = self
                    .client
                    .post(self.url(&format!("/v1/snapshots/{name}/rollback")))
                    .json(&json!({}))
                    .send()
                    .await?;
                let _body = Self::parse_response(r).await?;
                Ok(Self::mcp_text(&format!(
                    "Rolled back to snapshot '{name}'."
                )))
            }

            "memory_branch" => {
                let r = self
                    .client
                    .post(self.url("/v1/branches"))
                    .json(&json!({
                        "name": args["name"],
                        "from_snapshot": args["from_snapshot"],
                        "from_timestamp": args["from_timestamp"],
                    }))
                    .send()
                    .await?;
                let _body = Self::parse_response(r).await?;
                Ok(Self::mcp_text(&format!(
                    "Branch '{}' created.",
                    args["name"].as_str().unwrap_or("")
                )))
            }

            "memory_branches" => {
                let r = self.client.get(self.url("/v1/branches")).send().await?;
                let body = Self::parse_response(r).await?;
                Ok(Self::mcp_json(&body))
            }

            "memory_checkout" => {
                let name = args["name"].as_str().unwrap_or("");
                let r = self
                    .client
                    .post(self.url(&format!("/v1/branches/{name}/checkout")))
                    .json(&json!({}))
                    .send()
                    .await?;
                let body = Self::parse_response(r).await?;
                Ok(Self::mcp_text(&format!(
                    "Switched to branch '{name}'. {} memories.",
                    body["memory_count"].as_i64().unwrap_or(0)
                )))
            }

            "memory_merge" => {
                let source = args["source"].as_str().unwrap_or("");
                let r = self
                    .client
                    .post(self.url(&format!("/v1/branches/{source}/merge")))
                    .json(&json!({"strategy": args["strategy"].as_str().unwrap_or("accept")}))
                    .send()
                    .await?;
                let body = Self::parse_response(r).await?;
                Ok(Self::mcp_json(&body))
            }

            "memory_diff" => {
                let source = args["source"].as_str().unwrap_or("");
                let r = self
                    .client
                    .get(self.url(&format!("/v1/branches/{source}/diff")))
                    .send()
                    .await?;
                let body = Self::parse_response(r).await?;
                Ok(Self::mcp_json(&body))
            }

            "memory_branch_delete" => {
                let name = args["name"].as_str().unwrap_or("");
                self.client
                    .delete(self.url(&format!("/v1/branches/{name}")))
                    .send()
                    .await?;
                Ok(Self::mcp_text(&format!("Branch '{name}' deleted.")))
            }

            "memory_observe" => {
                let r = self
                    .client
                    .post(self.url("/v1/memories/observe"))
                    .json(&json!({
                        "messages": args["messages"],
                        "session_id": args["session_id"],
                    }))
                    .send()
                    .await?;
                let body = Self::parse_response(r).await?;
                Ok(Self::mcp_json(&body))
            }

            "memory_feedback" => {
                let memory_id = args["memory_id"].as_str().unwrap_or("");
                let signal = args["signal"].as_str().unwrap_or("");
                let r = self
                    .client
                    .post(self.url(&format!("/v1/memories/{memory_id}/feedback")))
                    .json(&json!({
                        "signal": signal,
                        "context": args["context"],
                    }))
                    .send()
                    .await?;
                let body = Self::parse_response(r).await?;
                Ok(Self::mcp_text(&format!(
                    "Recorded feedback: memory={}, signal={}, feedback_id={}",
                    memory_id,
                    signal,
                    body["feedback_id"].as_str().unwrap_or("")
                )))
            }

            _ => Ok(Self::mcp_text(&format!("Unknown tool: {name}"))),
        }
    }

    pub async fn call_owned(self, name: String, args: Value) -> Result<Value> {
        self.call(&name, args).await
    }
}
