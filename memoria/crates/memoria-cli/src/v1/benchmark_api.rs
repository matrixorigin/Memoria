use crate::benchmark::benchmark_executor::RecallMatch;
use crate::benchmark::ScenarioStep;
use anyhow::Result;
use reqwest::blocking::Client;
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::time::{SystemTime, UNIX_EPOCH};

pub(crate) struct V1BenchmarkApi<'a> {
    base_url: &'a str,
}

impl<'a> V1BenchmarkApi<'a> {
    pub(crate) fn new(base_url: &'a str) -> Self {
        Self { base_url }
    }

    pub(crate) fn run_maturation(&self, client: &Client, user_id: &str, op: &str) -> Result<()> {
        client
            .post(format!(
                "{}/admin/governance/{}/trigger",
                self.base_url, user_id
            ))
            .query(&[("op", op)])
            .send()?
            .error_for_status()?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn store(
        &self,
        client: &Client,
        content: &str,
        memory_type: &str,
        session_id: &str,
        age_days: Option<f64>,
        confidence: Option<f64>,
        trust_tier: Option<&str>,
    ) -> Result<String> {
        let mut body = json!({
            "content": content,
            "memory_type": memory_type,
            "session_id": session_id,
            "source": "benchmark",
        });
        if let Some(days) = age_days {
            let secs = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs_f64()
                - days * 86400.0;
            body["observed_at"] = json!(chrono_like_iso(secs));
        }
        if let Some(c) = confidence {
            body["initial_confidence"] = json!(c);
        }
        if let Some(t) = trust_tier {
            body["trust_tier"] = json!(t);
        }

        let data = client
            .post(format!("{}/v1/memories", self.base_url))
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
            .post(format!("{}/v1/memories/retrieve", self.base_url))
            .json(&json!({"query": query, "top_k": top_k, "session_id": session_id}))
            .send();
        let data: Value = match resp.and_then(|r| r.json()) {
            Ok(v) => v,
            Err(_) => return vec![],
        };
        parse_retrieve_response(&data)
    }

    pub(crate) fn correct_step(&self, client: &Client, step: &ScenarioStep) -> Result<()> {
        client
            .post(format!("{}/v1/memories/correct", self.base_url))
            .json(&json!({
                "query": step.query,
                "new_content": step.content,
                "reason": step.reason.as_deref().unwrap_or("benchmark"),
            }))
            .send()?
            .error_for_status()?;
        Ok(())
    }

    pub(crate) fn purge_step(
        &self,
        client: &Client,
        step: &ScenarioStep,
    ) -> Result<BTreeSet<String>> {
        let mut body = json!({"reason": step.reason.as_deref().unwrap_or("benchmark")});
        if let Some(topic) = &step.topic {
            body["topic"] = json!(topic);
        }
        client
            .post(format!("{}/v1/memories/purge", self.base_url))
            .json(&body)
            .send()?
            .error_for_status()?;
        Ok(BTreeSet::new())
    }

    pub(crate) fn forget_ids(&self, client: &Client, ids: &[String], reason: &str) -> Result<()> {
        client
            .post(format!("{}/v1/memories/purge", self.base_url))
            .json(&json!({"memory_ids": ids, "reason": reason}))
            .send()?
            .error_for_status()?;
        Ok(())
    }
}

fn parse_retrieve_response(data: &Value) -> Vec<RecallMatch> {
    let items = if data.is_array() {
        data.as_array()
    } else {
        data["results"].as_array()
    };
    items
        .map(|arr| {
            arr.iter()
                .map(|item| RecallMatch {
                    id: item["memory_id"].as_str().unwrap_or_default().to_string(),
                    text: item["content"].as_str().unwrap_or_default().to_string(),
                })
                .collect()
        })
        .unwrap_or_default()
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
    let hh = rem / 3600;
    let mm = (rem % 3600) / 60;
    let ss = rem % 60;
    format!("{year:04}-{month:02}-{day:02}T{hh:02}:{mm:02}:{ss:02}Z")
}
