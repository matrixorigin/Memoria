//! Native Rust benchmark — schema, executor, scorer.
//! Replaces the Python benchmark dependency.

use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

// ── Schema ────────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct ScenarioDataset {
    pub dataset_id: String,
    pub version: String,
    pub scenarios: Vec<Scenario>,
}

#[derive(Deserialize)]
pub struct Scenario {
    pub scenario_id: String,
    pub title: String,
    pub difficulty: String,
    pub horizon: String,
    pub tags: Vec<String>,
    pub seed_memories: Vec<SeedMemory>,
    #[serde(default)]
    pub maturation: Vec<String>,
    #[serde(default)]
    pub steps: Vec<ScenarioStep>,
    pub assertions: Vec<MemoryAssertion>,
}

#[derive(Deserialize)]
pub struct SeedMemory {
    pub content: String,
    #[serde(default = "default_semantic")]
    pub memory_type: String,
    #[serde(default)]
    pub is_outdated: bool,
    pub age_days: Option<f64>,
    pub initial_confidence: Option<f64>,
    pub trust_tier: Option<String>,
}

fn default_semantic() -> String { "semantic".into() }

#[derive(Deserialize)]
pub struct ScenarioStep {
    pub action: String,
    pub content: Option<String>,
    pub memory_type: Option<String>,
    pub query: Option<String>,
    pub top_k: Option<i64>,
    pub reason: Option<String>,
    pub topic: Option<String>,
    pub age_days: Option<f64>,
    pub initial_confidence: Option<f64>,
    pub trust_tier: Option<String>,
}

#[derive(Deserialize)]
pub struct MemoryAssertion {
    pub query: String,
    #[serde(default = "default_top_k")]
    pub top_k: i64,
    pub expected_contents: Vec<String>,
    #[serde(default)]
    pub excluded_contents: Vec<String>,
}

fn default_top_k() -> i64 { 5 }

// ── Execution results ─────────────────────────────────────────────────────────

pub struct StepResult {
    pub _action: String,
    pub success: bool,
    pub _error: Option<String>,
}

pub struct AssertionResult {
    pub _query: String,
    pub returned_contents: Vec<String>,
    pub _error: Option<String>,
}

pub struct ScenarioExecution {
    pub _scenario_id: String,
    pub step_results: Vec<StepResult>,
    pub assertion_results: Vec<AssertionResult>,
    pub error: Option<String>,
}

// ── Scoring results ───────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct ScenarioResult {
    pub scenario_id: String,
    pub title: String,
    pub difficulty: String,
    pub horizon: String,
    pub tags: Vec<String>,
    pub total_score: f64,
    pub grade: String,
    pub mqs_precision: f64,
    pub mqs_recall: f64,
    pub mqs_noise_rejection: f64,
    pub aus_step_success: f64,
    pub aus_assertion_pass: f64,
}

#[derive(Serialize)]
pub struct BenchmarkReport {
    pub dataset_id: String,
    pub version: String,
    pub scenario_count: usize,
    pub overall_score: f64,
    pub overall_grade: String,
    pub by_difficulty: HashMap<String, f64>,
    pub by_tag: HashMap<String, f64>,
    pub results: Vec<ScenarioResult>,
}

fn grade(score: f64) -> &'static str {
    if score >= 90.0 { "S" } else if score >= 80.0 { "A" }
    else if score >= 70.0 { "B" } else if score >= 60.0 { "C" } else { "D" }
}

// ── Executor ──────────────────────────────────────────────────────────────────

pub struct BenchmarkExecutor {
    base_url: String,
    token: String,
    run_id: String,
}

impl BenchmarkExecutor {
    pub fn new(api_url: &str, token: &str) -> Self {
        let run_id = SystemTime::now().duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs().to_string()).unwrap_or_default();
        Self { base_url: api_url.trim_end_matches('/').into(), token: token.into(), run_id }
    }

    fn client(&self, scenario_suffix: &str) -> Client {
        let user_id = format!("bench-{}-{}", self.run_id, scenario_suffix);
        Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .no_proxy()
            .default_headers({
                let mut h = reqwest::header::HeaderMap::new();
                h.insert("Authorization", format!("Bearer {}", self.token).parse().unwrap());
                h.insert("X-Impersonate-User", user_id.parse().unwrap());
                h
            })
            .build().unwrap()
    }

    pub fn execute(&self, scenario: &Scenario) -> ScenarioExecution {
        let sid = scenario.scenario_id.to_lowercase();
        let client = self.client(&sid);
        let session_id = format!("bench-{}-{}", self.run_id, sid);
        let user_id = format!("bench-{}-{}", self.run_id, sid);
        let mut exec = ScenarioExecution {
            _scenario_id: scenario.scenario_id.clone(),
            step_results: vec![], assertion_results: vec![], error: None,
        };

        // Phase 1: seed
        for seed in &scenario.seed_memories {
            match self.store(&client, &seed.content, &seed.memory_type, &session_id,
                seed.age_days, seed.initial_confidence, seed.trust_tier.as_deref()) {
                Ok(mid) => {
                    if seed.is_outdated && !mid.is_empty() {
                        let _ = self.purge_ids(&client, &[mid], "seed is_outdated");
                    }
                }
                Err(e) => { exec.error = Some(format!("seed failed: {e}")); return exec; }
            }
        }

        // Phase 2: maturation
        for op in &scenario.maturation {
            let _ = client.post(format!("{}/admin/governance/{}/trigger", self.base_url, user_id))
                .query(&[("op", op.as_str())]).send();
        }

        // Phase 3: steps
        for step in &scenario.steps {
            exec.step_results.push(self.run_step(&client, step, &session_id));
        }

        // Phase 4: assertions
        for assertion in &scenario.assertions {
            exec.assertion_results.push(self.run_assertion(&client, assertion, &session_id));
        }
        exec
    }

    #[allow(clippy::too_many_arguments)]
    fn store(&self, client: &Client, content: &str, memory_type: &str, session_id: &str,
        age_days: Option<f64>, confidence: Option<f64>, trust_tier: Option<&str>,
    ) -> anyhow::Result<String> {
        let mut body = json!({
            "content": content, "memory_type": memory_type,
            "session_id": session_id, "source": "benchmark",
        });
        if let Some(days) = age_days {
            let secs = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs_f64()
                - days * 86400.0;
            let dt = chrono_like_iso(secs);
            body["observed_at"] = json!(dt);
        }
        if let Some(c) = confidence { body["initial_confidence"] = json!(c); }
        if let Some(t) = trust_tier { body["trust_tier"] = json!(t); }

        let resp = client.post(format!("{}/v1/memories", self.base_url))
            .json(&body).send()?;
        let data: Value = resp.json()?;
        Ok(data["memory_id"].as_str().unwrap_or("").to_string())
    }

    fn retrieve(&self, client: &Client, query: &str, session_id: &str, top_k: i64) -> Vec<String> {
        let resp = client.post(format!("{}/v1/memories/retrieve", self.base_url))
            .json(&json!({"query": query, "top_k": top_k, "session_id": session_id}))
            .send();
        let data: Value = match resp.and_then(|r| r.json()) { Ok(v) => v, Err(_) => return vec![] };
        let items = if data.is_array() { data.as_array() } else { data["results"].as_array() };
        items.map(|arr| arr.iter().filter_map(|i| i["content"].as_str().map(String::from)).collect())
            .unwrap_or_default()
    }

    fn run_step(&self, client: &Client, step: &ScenarioStep, session_id: &str) -> StepResult {
        let action = step.action.clone();
        let result = (|| -> anyhow::Result<()> {
            match action.as_str() {
                "store" => {
                    self.store(client, step.content.as_deref().unwrap_or(""),
                        step.memory_type.as_deref().unwrap_or("semantic"), session_id,
                        step.age_days, step.initial_confidence, step.trust_tier.as_deref())?;
                }
                "retrieve" => {
                    self.retrieve(client, step.query.as_deref().unwrap_or(""), session_id, step.top_k.unwrap_or(5));
                }
                "search" => {
                    client.post(format!("{}/v1/memories/search", self.base_url))
                        .json(&json!({"query": step.query, "top_k": step.top_k.unwrap_or(10)}))
                        .send()?.error_for_status()?;
                }
                "correct" => {
                    client.post(format!("{}/v1/memories/correct", self.base_url))
                        .json(&json!({"query": step.query, "new_content": step.content,
                            "reason": step.reason.as_deref().unwrap_or("benchmark")}))
                        .send()?.error_for_status()?;
                }
                "purge" => {
                    let mut body = json!({"reason": step.reason.as_deref().unwrap_or("benchmark")});
                    if let Some(t) = &step.topic { body["topic"] = json!(t); }
                    client.post(format!("{}/v1/memories/purge", self.base_url))
                        .json(&body).send()?.error_for_status()?;
                }
                _ => {}
            }
            Ok(())
        })();
        match result {
            Ok(()) => StepResult { _action: action, success: true, _error: None },
            Err(e) => StepResult { _action: action, success: false, _error: Some(e.to_string()) },
        }
    }

    fn run_assertion(&self, client: &Client, assertion: &MemoryAssertion, session_id: &str) -> AssertionResult {
        let contents = self.retrieve(client, &assertion.query, session_id, assertion.top_k);
        AssertionResult { _query: assertion.query.clone(), returned_contents: contents, _error: None }
    }

    fn purge_ids(&self, client: &Client, ids: &[String], reason: &str) -> anyhow::Result<()> {
        client.post(format!("{}/v1/memories/purge", self.base_url))
            .json(&json!({"memory_ids": ids, "reason": reason}))
            .send()?.error_for_status()?;
        Ok(())
    }
}

fn chrono_like_iso(epoch_secs: f64) -> String {
    let secs = epoch_secs as i64;
    let d = secs / 86400 + 719468;
    let era = if d >= 0 { d } else { d - 146096 } / 146097;
    let doe = (d - era * 146097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };
    let rem = secs.rem_euclid(86400);
    let h = rem / 3600; let m = (rem % 3600) / 60; let s = rem % 60;
    format!("{year:04}-{month:02}-{day:02}T{h:02}:{m:02}:{s:02}Z")
}

// ── Scorer ────────────────────────────────────────────────────────────────────

fn score_contents(assertion: &MemoryAssertion, returned: &[String]) -> (f64, f64, f64, bool) {
    let hits = assertion.expected_contents.iter()
        .filter(|exp| returned.iter().any(|c| c.to_lowercase().contains(&exp.to_lowercase())))
        .count();
    let recall = if assertion.expected_contents.is_empty() { 100.0 }
        else { 100.0 * hits as f64 / assertion.expected_contents.len() as f64 };

    let precision = if returned.is_empty() { 0.0 } else {
        let relevant = returned.iter()
            .filter(|c| assertion.expected_contents.iter().any(|exp| c.to_lowercase().contains(&exp.to_lowercase())))
            .count();
        100.0 * relevant as f64 / returned.len() as f64
    };

    let noise_rejection = if assertion.excluded_contents.is_empty() { 100.0 } else {
        let noise_hits = assertion.excluded_contents.iter()
            .filter(|exc| returned.iter().any(|c| c.to_lowercase().contains(&exc.to_lowercase())))
            .count();
        100.0 * (assertion.excluded_contents.len() - noise_hits) as f64 / assertion.excluded_contents.len() as f64
    };

    let passed = recall >= 80.0 && noise_rejection >= 80.0;
    (precision, recall, noise_rejection, passed)
}

pub fn score_scenario(scenario: &Scenario, exec: &ScenarioExecution) -> ScenarioResult {
    if let Some(_err) = &exec.error {
        return ScenarioResult {
            scenario_id: scenario.scenario_id.clone(), title: scenario.title.clone(),
            difficulty: scenario.difficulty.clone(), horizon: scenario.horizon.clone(),
            tags: scenario.tags.clone(), total_score: 0.0, grade: "D".into(),
            mqs_precision: 0.0, mqs_recall: 0.0, mqs_noise_rejection: 100.0,
            aus_step_success: 0.0, aus_assertion_pass: 0.0,
        };
    }

    let mut precisions = vec![]; let mut recalls = vec![]; let mut noises = vec![];
    let mut passed_count = 0usize;
    for (i, assertion) in scenario.assertions.iter().enumerate() {
        let returned = exec.assertion_results.get(i)
            .map(|r| r.returned_contents.as_slice()).unwrap_or(&[]);
        let (p, r, n, ok) = score_contents(assertion, returned);
        precisions.push(p); recalls.push(r); noises.push(n);
        if ok { passed_count += 1; }
    }

    let avg = |v: &[f64]| if v.is_empty() { 0.0 } else { v.iter().sum::<f64>() / v.len() as f64 };
    let mqs_p = avg(&precisions); let mqs_r = avg(&recalls); let mqs_n = avg(&noises);
    let assertion_pass = if scenario.assertions.is_empty() { 0.0 }
        else { 100.0 * passed_count as f64 / scenario.assertions.len() as f64 };
    let step_success = if exec.step_results.is_empty() { 100.0 }
        else { 100.0 * exec.step_results.iter().filter(|s| s.success).count() as f64 / exec.step_results.len() as f64 };

    let mqs = (mqs_p + mqs_r + mqs_n) / 3.0;
    let aus = (step_success + assertion_pass) / 2.0;
    let total = 0.65 * mqs + 0.35 * aus;

    ScenarioResult {
        scenario_id: scenario.scenario_id.clone(), title: scenario.title.clone(),
        difficulty: scenario.difficulty.clone(), horizon: scenario.horizon.clone(),
        tags: scenario.tags.clone(),
        total_score: (total * 100.0).round() / 100.0,
        grade: grade(total).into(),
        mqs_precision: (mqs_p * 100.0).round() / 100.0,
        mqs_recall: (mqs_r * 100.0).round() / 100.0,
        mqs_noise_rejection: (mqs_n * 100.0).round() / 100.0,
        aus_step_success: (step_success * 100.0).round() / 100.0,
        aus_assertion_pass: (assertion_pass * 100.0).round() / 100.0,
    }
}

pub fn score_dataset(dataset: &ScenarioDataset, executions: &HashMap<String, ScenarioExecution>) -> BenchmarkReport {
    let mut results = vec![];
    let mut by_diff: HashMap<String, Vec<f64>> = HashMap::new();
    let mut by_tag: HashMap<String, Vec<f64>> = HashMap::new();

    for scenario in &dataset.scenarios {
        let empty = ScenarioExecution {
            _scenario_id: scenario.scenario_id.clone(),
            step_results: vec![], assertion_results: vec![],
            error: Some("no execution".into()),
        };
        let exec = executions.get(&scenario.scenario_id).unwrap_or(&empty);
        let result = score_scenario(scenario, exec);
        by_diff.entry(scenario.difficulty.clone()).or_default().push(result.total_score);
        for tag in &scenario.tags { by_tag.entry(tag.clone()).or_default().push(result.total_score); }
        results.push(result);
    }

    let avg = |v: &[f64]| if v.is_empty() { 0.0 } else { v.iter().sum::<f64>() / v.len() as f64 };
    let all: Vec<f64> = results.iter().map(|r| r.total_score).collect();
    let overall = avg(&all);

    BenchmarkReport {
        dataset_id: dataset.dataset_id.clone(), version: dataset.version.clone(),
        scenario_count: results.len(),
        overall_score: (overall * 100.0).round() / 100.0,
        overall_grade: grade(overall).into(),
        by_difficulty: by_diff.iter().map(|(k, v)| (k.clone(), (avg(v) * 100.0).round() / 100.0)).collect(),
        by_tag: by_tag.iter().map(|(k, v)| (k.clone(), (avg(v) * 100.0).round() / 100.0)).collect(),
        results,
    }
}

// ── Validator ─────────────────────────────────────────────────────────────────

pub fn validate_dataset(content: &str) -> Vec<String> {
    let mut errors = vec![];
    let dataset: ScenarioDataset = match serde_json::from_str(content) {
        Ok(d) => d,
        Err(e) => { errors.push(format!("JSON parse error: {e}")); return errors; }
    };
    let mut ids = std::collections::HashSet::new();
    for s in &dataset.scenarios {
        if !ids.insert(&s.scenario_id) {
            errors.push(format!("duplicate scenario_id: {}", s.scenario_id));
        }
        if s.seed_memories.is_empty() {
            errors.push(format!("{}: no seed_memories", s.scenario_id));
        }
        if s.assertions.is_empty() {
            errors.push(format!("{}: no assertions", s.scenario_id));
        }
        for (i, a) in s.assertions.iter().enumerate() {
            if a.expected_contents.is_empty() {
                errors.push(format!("{}: assertion[{i}] has no expected_contents", s.scenario_id));
            }
        }
        for (i, step) in s.steps.iter().enumerate() {
            match step.action.as_str() {
                "retrieve" | "search" if step.query.is_none() =>
                    errors.push(format!("{}: step[{i}] {} requires query", s.scenario_id, step.action)),
                "store" if step.content.is_none() =>
                    errors.push(format!("{}: step[{i}] store requires content", s.scenario_id)),
                "correct" if step.content.is_none() || step.query.is_none() =>
                    errors.push(format!("{}: step[{i}] correct requires content+query", s.scenario_id)),
                _ => {}
            }
        }
    }
    errors
}
