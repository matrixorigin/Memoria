use crate::benchmark::v1::benchmark_api::V1BenchmarkApi;
use crate::benchmark::v2::benchmark_api::V2BenchmarkApi;
use crate::benchmark::{
    AssertionResult, DatasetApiVersion, MemoryAssertion, Scenario, ScenarioExecution, ScenarioStep,
    StepResult,
};
use anyhow::Result;
use reqwest::blocking::Client;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone)]
pub(crate) struct RecallMatch {
    pub(crate) id: String,
    pub(crate) text: String,
}

pub struct BenchmarkExecutor {
    base_url: String,
    token: String,
    run_id: String,
    api_version: DatasetApiVersion,
}

impl BenchmarkExecutor {
    pub fn new(api_url: &str, token: &str, api_version: DatasetApiVersion) -> Self {
        let run_id = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs().to_string())
            .unwrap_or_default();
        Self {
            base_url: api_url.trim_end_matches('/').into(),
            token: token.into(),
            run_id,
            api_version,
        }
    }

    fn client(&self, scenario_suffix: &str) -> Client {
        let user_id = format!("bench-{}-{}", self.run_id, scenario_suffix);
        Client::builder()
            .timeout(Duration::from_secs(30))
            .no_proxy()
            .default_headers({
                let mut h = reqwest::header::HeaderMap::new();
                h.insert(
                    "Authorization",
                    format!("Bearer {}", self.token).parse().unwrap(),
                );
                h.insert("X-Impersonate-User", user_id.parse().unwrap());
                h
            })
            .build()
            .unwrap()
    }

    fn v2_api(&self) -> V2BenchmarkApi<'_> {
        V2BenchmarkApi::new(&self.base_url, &self.run_id)
    }

    fn v1_api(&self) -> V1BenchmarkApi<'_> {
        V1BenchmarkApi::new(&self.base_url)
    }

    pub fn execute(&self, scenario: &Scenario) -> ScenarioExecution {
        let sid = scenario.scenario_id.to_lowercase();
        let client = self.client(&sid);
        let session_id = format!("bench-{}-{}", self.run_id, sid);
        let user_id = format!("bench-{}-{}", self.run_id, sid);
        let mut exec = ScenarioExecution {
            _scenario_id: scenario.scenario_id.clone(),
            step_results: vec![],
            assertion_results: vec![],
            error: None,
        };
        let mut tracked_memory_ids = Vec::new();

        for seed in &scenario.seed_memories {
            match self.store(
                &client,
                &seed.content,
                &seed.memory_type,
                &session_id,
                seed.age_days,
                seed.initial_confidence,
                seed.trust_tier.as_deref(),
            ) {
                Ok(mid) => {
                    if !mid.is_empty() {
                        tracked_memory_ids.push(mid.clone());
                        if seed.is_outdated {
                            if let Err(err) = self.forget_ids(
                                &client,
                                std::slice::from_ref(&mid),
                                "seed is_outdated",
                            ) {
                                exec.error = Some(format!("seed purge failed: {err}"));
                                return exec;
                            }
                            tracked_memory_ids.retain(|tracked| tracked != &mid);
                        }
                    }
                }
                Err(e) => {
                    exec.error = Some(format!("seed failed: {e}"));
                    return exec;
                }
            }
        }

        if matches!(self.api_version, DatasetApiVersion::V2) {
            if let Err(err) = self.v2_api().after_writes(&client, &tracked_memory_ids) {
                exec.error = Some(format!("seed derivation failed: {err}"));
                return exec;
            }
        }

        for op in &scenario.maturation {
            if let Err(err) = self.run_maturation(&client, &user_id, &tracked_memory_ids, op) {
                exec.error = Some(format!("maturation '{op}' failed: {err}"));
                return exec;
            }
        }

        for step in &scenario.steps {
            exec.step_results.push(self.run_step(
                &client,
                step,
                &session_id,
                &mut tracked_memory_ids,
            ));
        }

        for assertion in &scenario.assertions {
            exec.assertion_results
                .push(self.run_assertion(&client, assertion, &session_id));
        }
        exec
    }

    fn run_maturation(
        &self,
        client: &Client,
        user_id: &str,
        tracked_memory_ids: &[String],
        op: &str,
    ) -> Result<()> {
        match self.api_version {
            DatasetApiVersion::V1 => self.v1_api().run_maturation(client, user_id, op),
            DatasetApiVersion::V2 => self.v2_api().run_maturation(client, tracked_memory_ids, op),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn store(
        &self,
        client: &Client,
        content: &str,
        memory_type: &str,
        session_id: &str,
        age_days: Option<f64>,
        confidence: Option<f64>,
        trust_tier: Option<&str>,
    ) -> Result<String> {
        match self.api_version {
            DatasetApiVersion::V1 => self.v1_api().store(
                client,
                content,
                memory_type,
                session_id,
                age_days,
                confidence,
                trust_tier,
            ),
            DatasetApiVersion::V2 => {
                self.v2_api()
                    .store(client, content, memory_type, session_id, trust_tier)
            }
        }
    }

    fn retrieve_matches(
        &self,
        client: &Client,
        query: &str,
        session_id: &str,
        top_k: i64,
    ) -> Vec<RecallMatch> {
        match self.api_version {
            DatasetApiVersion::V1 => self
                .v1_api()
                .retrieve_matches(client, query, session_id, top_k),
            DatasetApiVersion::V2 => self
                .v2_api()
                .retrieve_matches(client, query, session_id, top_k),
        }
    }

    fn run_step(
        &self,
        client: &Client,
        step: &ScenarioStep,
        session_id: &str,
        tracked_memory_ids: &mut Vec<String>,
    ) -> StepResult {
        let action = step.action.clone();
        let result = (|| -> Result<()> {
            match action.as_str() {
                "store" => {
                    let mid = self.store(
                        client,
                        step.content.as_deref().unwrap_or(""),
                        step.memory_type.as_deref().unwrap_or("semantic"),
                        session_id,
                        step.age_days,
                        step.initial_confidence,
                        step.trust_tier.as_deref(),
                    )?;
                    if !mid.is_empty() {
                        tracked_memory_ids.push(mid.clone());
                        if matches!(self.api_version, DatasetApiVersion::V2) {
                            self.v2_api().after_writes(client, &[mid])?;
                        }
                    }
                }
                "retrieve" | "search" => {
                    self.retrieve_matches(
                        client,
                        step.query.as_deref().unwrap_or(""),
                        session_id,
                        step.top_k.unwrap_or(5),
                    );
                }
                "correct" => match self.api_version {
                    DatasetApiVersion::V1 => self.v1_api().correct_step(client, step)?,
                    DatasetApiVersion::V2 => {
                        let _ = self.v2_api().correct_step(client, step, session_id)?;
                    }
                },
                "purge" => match self.api_version {
                    DatasetApiVersion::V1 => {
                        let forgotten = self.v1_api().purge_step(client, step)?;
                        tracked_memory_ids.retain(|id| !forgotten.contains(id));
                    }
                    DatasetApiVersion::V2 => {
                        let forgotten = self.v2_api().purge_step(client, step, session_id)?;
                        tracked_memory_ids.retain(|id| !forgotten.contains(id));
                    }
                },
                _ => {}
            }
            Ok(())
        })();
        match result {
            Ok(()) => StepResult {
                _action: action,
                success: true,
                _error: None,
            },
            Err(e) => StepResult {
                _action: action,
                success: false,
                _error: Some(e.to_string()),
            },
        }
    }

    fn run_assertion(
        &self,
        client: &Client,
        assertion: &MemoryAssertion,
        session_id: &str,
    ) -> AssertionResult {
        let contents = self
            .retrieve_matches(client, &assertion.query, session_id, assertion.top_k)
            .into_iter()
            .map(|item| item.text)
            .collect();
        AssertionResult {
            _query: assertion.query.clone(),
            returned_contents: contents,
            _error: None,
        }
    }

    fn forget_ids(&self, client: &Client, ids: &[String], reason: &str) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        match self.api_version {
            DatasetApiVersion::V1 => self.v1_api().forget_ids(client, ids, reason)?,
            DatasetApiVersion::V2 => self.v2_api().forget_ids(client, ids, reason)?,
        }
        Ok(())
    }
}
