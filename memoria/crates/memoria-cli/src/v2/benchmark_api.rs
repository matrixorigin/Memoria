use crate::benchmark::benchmark_executor::RecallMatch;
use crate::benchmark::ScenarioStep;
use anyhow::{anyhow, bail, Context, Result};
use reqwest::blocking::Client;
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::thread;
use std::time::Duration;

#[cfg(test)]
use crate::benchmark::{BenchmarkExecutor, DatasetApiVersion, MemoryAssertion, Scenario};

pub(crate) struct V2BenchmarkApi<'a> {
    base_url: &'a str,
    run_id: &'a str,
}

impl<'a> V2BenchmarkApi<'a> {
    pub(crate) fn new(base_url: &'a str, run_id: &'a str) -> Self {
        Self { base_url, run_id }
    }

    pub(crate) fn run_maturation(
        &self,
        client: &Client,
        tracked_memory_ids: &[String],
        op: &str,
    ) -> Result<()> {
        match op {
            "extract_entities" => {
                client
                    .post(format!("{}/v2/memory/entities/extract", self.base_url))
                    .json(&json!({
                        "limit": tracked_memory_ids.len().max(1) as i64,
                    }))
                    .send()?
                    .error_for_status()?;
                self.wait_for_memory_jobs(client, tracked_memory_ids)
            }
            "consolidate" => self.wait_for_memory_jobs(client, tracked_memory_ids),
            "reflect" => {
                client
                    .post(format!("{}/v2/memory/reflect", self.base_url))
                    .json(&json!({
                        "mode": "auto",
                        "limit": 20,
                    }))
                    .send()?
                    .error_for_status()?;
                Ok(())
            }
            other => bail!("unsupported V2 maturation op: {other}"),
        }
    }

    pub(crate) fn store(
        &self,
        client: &Client,
        content: &str,
        memory_type: &str,
        session_id: &str,
        trust_tier: Option<&str>,
    ) -> Result<String> {
        let mut body = json!({
            "content": content,
            "type": memory_type,
            "session_id": session_id,
            "source": {
                "kind": "benchmark",
                "run_id": self.run_id,
            },
        });
        if let Some(t) = trust_tier {
            body["trust_tier"] = json!(t);
        }
        let data = client
            .post(format!("{}/v2/memory/remember", self.base_url))
            .json(&body)
            .send()?
            .error_for_status()?
            .json::<Value>()?;
        Ok(data["memory_id"].as_str().unwrap_or("").to_string())
    }

    pub(crate) fn retrieve_matches(
        &self,
        client: &Client,
        query: &str,
        session_id: &str,
        top_k: i64,
    ) -> Vec<RecallMatch> {
        let resp = client
            .post(format!("{}/v2/memory/recall", self.base_url))
            .json(&json!({
                "query": query,
                "top_k": top_k,
                "session_id": session_id,
                "scope": "all",
                "view": "compact",
            }))
            .send();
        let data: Value = match resp.and_then(|r| r.json()) {
            Ok(v) => v,
            Err(_) => return vec![],
        };
        parse_recall_response(&data)
    }

    pub(crate) fn correct_step(
        &self,
        client: &Client,
        step: &ScenarioStep,
        session_id: &str,
    ) -> Result<String> {
        let target = self
            .retrieve_matches(client, step.query.as_deref().unwrap_or(""), session_id, 1)
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("no V2 recall match found for correction"))?;
        client
            .patch(format!("{}/v2/memory/update", self.base_url))
            .json(&json!({
                "memory_id": target.id,
                "content": step.content,
                "reason": step.reason.as_deref().unwrap_or("benchmark"),
            }))
            .send()?
            .error_for_status()?;
        self.after_writes(client, std::slice::from_ref(&target.id))?;
        Ok(target.id)
    }

    pub(crate) fn purge_step(
        &self,
        client: &Client,
        step: &ScenarioStep,
        session_id: &str,
    ) -> Result<BTreeSet<String>> {
        let query = step
            .topic
            .as_deref()
            .or(step.query.as_deref())
            .unwrap_or("");
        if query.is_empty() {
            return Ok(BTreeSet::new());
        }
        let lower_query = query.to_ascii_lowercase();
        let matches =
            self.retrieve_matches(client, query, session_id, step.top_k.unwrap_or(10).max(1));
        let ids: Vec<String> = matches
            .into_iter()
            .filter(|item| {
                step.topic.is_none() || item.text.to_ascii_lowercase().contains(&lower_query)
            })
            .map(|item| item.id)
            .collect();
        if ids.is_empty() {
            return Ok(BTreeSet::new());
        }
        self.forget_ids(client, &ids, step.reason.as_deref().unwrap_or("benchmark"))?;
        Ok(ids.into_iter().collect())
    }

    pub(crate) fn forget_ids(&self, client: &Client, ids: &[String], reason: &str) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        if ids.len() == 1 {
            client
                .post(format!("{}/v2/memory/forget", self.base_url))
                .json(&json!({"memory_id": ids[0], "reason": reason}))
                .send()?
                .error_for_status()?;
        } else {
            client
                .post(format!("{}/v2/memory/batch-forget", self.base_url))
                .json(&json!({"memory_ids": ids, "reason": reason}))
                .send()?
                .error_for_status()?;
        }
        Ok(())
    }

    pub(crate) fn after_writes(&self, client: &Client, memory_ids: &[String]) -> Result<()> {
        self.wait_for_memory_jobs(client, memory_ids)
    }

    pub(crate) fn wait_for_memory_jobs(
        &self,
        client: &Client,
        memory_ids: &[String],
    ) -> Result<()> {
        let mut pending: BTreeSet<String> = memory_ids
            .iter()
            .filter(|id| !id.is_empty())
            .cloned()
            .collect();
        if pending.is_empty() {
            return Ok(());
        }

        for attempt in 0..40 {
            let mut ready = Vec::new();
            for memory_id in &pending {
                let resp = client
                    .get(format!("{}/v2/memory/jobs", self.base_url))
                    .query(&[("memory_id", memory_id.as_str()), ("limit", "10")])
                    .send()
                    .with_context(|| format!("query V2 jobs for {memory_id}"))?
                    .error_for_status()
                    .with_context(|| format!("query V2 jobs for {memory_id}"))?;
                let body: Value = resp.json().context("parse V2 jobs response")?;
                if body["failed_count"].as_i64().unwrap_or_default() > 0 {
                    bail!("V2 jobs failed for {memory_id}: {body}");
                }
                let pending_count = body["pending_count"].as_i64().unwrap_or_default();
                let in_progress_count = body["in_progress_count"].as_i64().unwrap_or_default();
                let done_count = body["done_count"].as_i64().unwrap_or_default();
                if pending_count == 0 && in_progress_count == 0 && done_count >= 3 {
                    ready.push(memory_id.clone());
                }
            }
            for memory_id in ready {
                pending.remove(&memory_id);
            }
            if pending.is_empty() {
                return Ok(());
            }
            let backoff = 100 * (attempt.min(4) as u64 + 1);
            thread::sleep(Duration::from_millis(backoff));
        }

        bail!(
            "timed out waiting for V2 derivation jobs: {}",
            pending.into_iter().collect::<Vec<_>>().join(", ")
        );
    }
}

pub(crate) fn parse_recall_response(data: &Value) -> Vec<RecallMatch> {
    data["memories"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .map(|item| RecallMatch {
                    id: item["id"].as_str().unwrap_or_default().to_string(),
                    text: item["text"]
                        .as_str()
                        .or_else(|| item["overview"].as_str())
                        .or_else(|| item["abstract"].as_str())
                        .or_else(|| item["content"].as_str())
                        .unwrap_or_default()
                        .to_string(),
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        extract::State,
        http::StatusCode,
        routing::{get, patch, post},
        Json, Router,
    };
    use serde_json::Value;
    use std::net::SocketAddr;
    use std::sync::{Arc, Mutex};
    use tokio::net::TcpListener;

    #[derive(Clone, Default)]
    struct MockState {
        paths: Arc<Mutex<Vec<String>>>,
        memory: Arc<Mutex<Option<(String, String)>>>,
    }

    async fn spawn_mock_v2_server() -> String {
        async fn remember(
            State(state): State<MockState>,
            Json(body): Json<Value>,
        ) -> (StatusCode, Json<Value>) {
            state
                .paths
                .lock()
                .expect("paths")
                .push("/v2/memory/remember".to_string());
            let content = body["content"].as_str().unwrap_or_default().to_string();
            *state.memory.lock().expect("memory") = Some(("mem-1".to_string(), content.clone()));
            (
                StatusCode::CREATED,
                Json(json!({
                    "memory_id": "mem-1",
                    "abstract": content,
                    "has_overview": true,
                    "has_detail": true,
                })),
            )
        }

        async fn recall(State(state): State<MockState>, Json(_body): Json<Value>) -> Json<Value> {
            state
                .paths
                .lock()
                .expect("paths")
                .push("/v2/memory/recall".to_string());
            let memories = state
                .memory
                .lock()
                .expect("memory")
                .clone()
                .map(|(id, content)| {
                    vec![json!({
                        "id": id,
                        "text": content,
                        "type": "semantic",
                        "score": 1.0,
                        "related": false,
                    })]
                })
                .unwrap_or_default();
            Json(json!({
                "summary": {
                    "discovered_count": memories.len(),
                    "returned_count": memories.len(),
                    "truncated": false,
                    "by_retrieval_path": []
                },
                "memories": memories,
                "token_used": 1,
                "has_more": false
            }))
        }

        async fn update(State(state): State<MockState>, Json(body): Json<Value>) -> Json<Value> {
            state
                .paths
                .lock()
                .expect("paths")
                .push("/v2/memory/update".to_string());
            let new_content = body["content"].as_str().unwrap_or_default().to_string();
            *state.memory.lock().expect("memory") =
                Some(("mem-1".to_string(), new_content.clone()));
            Json(json!({
                "memory_id": "mem-1",
                "abstract": new_content,
                "updated_at": "2026-01-01T00:00:00Z",
                "has_overview": true,
                "has_detail": true
            }))
        }

        async fn batch_forget(
            State(state): State<MockState>,
            Json(_body): Json<Value>,
        ) -> Json<Value> {
            state
                .paths
                .lock()
                .expect("paths")
                .push("/v2/memory/batch-forget".to_string());
            *state.memory.lock().expect("memory") = None;
            Json(json!({"memories": [{"memory_id": "mem-1", "forgotten": true}]}))
        }

        async fn forget(State(state): State<MockState>, Json(_body): Json<Value>) -> Json<Value> {
            state
                .paths
                .lock()
                .expect("paths")
                .push("/v2/memory/forget".to_string());
            *state.memory.lock().expect("memory") = None;
            Json(json!({"memory_id": "mem-1", "forgotten": true}))
        }

        async fn extract_entities(
            State(state): State<MockState>,
            Json(_body): Json<Value>,
        ) -> Json<Value> {
            state
                .paths
                .lock()
                .expect("paths")
                .push("/v2/memory/entities/extract".to_string());
            Json(json!({"processed_memories": 1, "entities_found": 1, "links_written": 1}))
        }

        async fn jobs(State(state): State<MockState>) -> Json<Value> {
            state
                .paths
                .lock()
                .expect("paths")
                .push("/v2/memory/jobs".to_string());
            Json(json!({
                "memory_id": "mem-1",
                "derivation_state": "ready",
                "has_overview": true,
                "has_detail": true,
                "link_count": 0,
                "pending_count": 0,
                "in_progress_count": 0,
                "done_count": 3,
                "failed_count": 0,
                "job_types": [],
                "items": []
            }))
        }

        let state = MockState::default();
        let app = Router::new()
            .route("/v2/memory/remember", post(remember))
            .route("/v2/memory/recall", post(recall))
            .route("/v2/memory/update", patch(update))
            .route("/v2/memory/forget", post(forget))
            .route("/v2/memory/batch-forget", post(batch_forget))
            .route("/v2/memory/entities/extract", post(extract_entities))
            .route("/v2/memory/jobs", get(jobs))
            .with_state(state);
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind listener");
        let addr: SocketAddr = listener.local_addr().expect("listener addr");
        tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve mock v2 benchmark");
        });
        format!("http://{}", addr)
    }

    #[tokio::test]
    async fn v2_executor_hits_v2_routes() {
        let base = spawn_mock_v2_server().await;
        let scenario = Scenario {
            scenario_id: "CASE-V2-001".into(),
            title: "v2 benchmark smoke".into(),
            description: String::new(),
            domain: "test".into(),
            difficulty: "L1".into(),
            horizon: "short".into(),
            tags: vec![],
            source_family: None,
            question_type: None,
            metadata: Default::default(),
            seed_memories: vec![crate::benchmark::SeedMemory {
                content: "original memory".into(),
                memory_type: "semantic".into(),
                is_outdated: false,
                age_days: None,
                initial_confidence: None,
                trust_tier: None,
            }],
            maturation: vec!["extract_entities".into(), "consolidate".into()],
            steps: vec![
                ScenarioStep {
                    action: "correct".into(),
                    content: Some("updated memory".into()),
                    memory_type: None,
                    query: Some("original".into()),
                    top_k: None,
                    reason: Some("test".into()),
                    topic: None,
                    age_days: None,
                    initial_confidence: None,
                    trust_tier: None,
                },
                ScenarioStep {
                    action: "purge".into(),
                    content: None,
                    memory_type: None,
                    query: None,
                    top_k: Some(5),
                    reason: Some("cleanup".into()),
                    topic: Some("updated".into()),
                    age_days: None,
                    initial_confidence: None,
                    trust_tier: None,
                },
            ],
            assertions: vec![MemoryAssertion {
                query: "updated".into(),
                top_k: 3,
                expected_contents: vec![],
                excluded_contents: vec![],
            }],
        };

        let exec = tokio::task::spawn_blocking(move || {
            let executor = BenchmarkExecutor::new(&base, "token", DatasetApiVersion::V2);
            executor.execute(&scenario)
        })
        .await
        .expect("join executor");

        assert!(exec.error.is_none(), "unexpected error: {:?}", exec.error);
        assert!(exec.step_results.iter().all(|step| step.success));
        assert!(exec.assertion_results[0].returned_contents.is_empty());
    }

    #[test]
    fn parses_v2_compact_recall_response() {
        let data = json!({
            "summary": {
                "discovered_count": 1,
                "returned_count": 1,
                "truncated": false,
                "by_retrieval_path": []
            },
            "memories": [{
                "id": "mem-1",
                "text": "hello world",
                "type": "semantic",
                "score": 0.9,
                "related": false
            }],
            "token_used": 10,
            "has_more": false
        });
        let items = parse_recall_response(&data);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, "mem-1");
        assert_eq!(items[0].text, "hello world");
    }
}
