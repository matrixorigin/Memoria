use super::benchmark_schema::{CategoryBreakdown, Scenario};
use serde_json::Value;
use std::collections::HashMap;

pub fn grade(score: f64) -> &'static str {
    if score >= 90.0 {
        "S"
    } else if score >= 80.0 {
        "A"
    } else if score >= 70.0 {
        "B"
    } else if score >= 60.0 {
        "C"
    } else {
        "D"
    }
}

fn normalize_key(value: &str) -> String {
    value
        .trim()
        .to_lowercase()
        .replace([' ', '/', '-'], "_")
        .replace("__", "_")
}

pub fn scenario_source_family(dataset_id: &str, scenario: &Scenario) -> Option<String> {
    if let Some(value) = scenario.source_family.as_ref() {
        let normalized = normalize_key(value);
        if !normalized.is_empty() {
            return Some(if normalized == "longmem" {
                "longmemeval".into()
            } else {
                normalized
            });
        }
    }
    if let Some(Value::String(value)) = scenario.metadata.get("source_family") {
        let normalized = normalize_key(value);
        if !normalized.is_empty() {
            return Some(if normalized == "longmem" {
                "longmemeval".into()
            } else {
                normalized
            });
        }
    }
    let domain = normalize_key(&scenario.domain);
    if domain == "beam" {
        return Some("beam".into());
    }
    if domain == "longmem" || domain == "longmemeval" {
        return Some("longmemeval".into());
    }
    let dataset_id = normalize_key(dataset_id);
    if dataset_id.starts_with("beam") {
        return Some("beam".into());
    }
    if dataset_id.starts_with("longmemeval") || dataset_id.contains("longmemeval") {
        return Some("longmemeval".into());
    }
    None
}

pub fn scenario_question_type(scenario: &Scenario) -> Option<String> {
    if let Some(value) = scenario.question_type.as_ref() {
        let normalized = normalize_key(value);
        if !normalized.is_empty() {
            return Some(normalized);
        }
    }
    if let Some(Value::String(value)) = scenario.metadata.get("question_type") {
        let normalized = normalize_key(value);
        if !normalized.is_empty() {
            return Some(normalized);
        }
    }
    if scenario.scenario_id.to_lowercase().ends_with("_abs") {
        return Some("abstention".into());
    }
    None
}

fn longmemeval_category_label(category: &str) -> &'static str {
    match category {
        "single-session-user" => "Single-Session User",
        "single-session-assistant" => "Single-Session Assistant",
        "single-session-preference" => "Single-Session Preference",
        "multi-session" => "Multi-Session",
        "temporal-reasoning" => "Temporal Reasoning",
        "knowledge-update" => "Knowledge Update",
        "abstention" => "Abstention",
        _ => "Unknown",
    }
}

fn beam_ability_label(category: &str) -> &'static str {
    match category {
        "information_extraction" => "Information Extraction",
        "preference_following" => "Preference Following",
        "multi_session_reasoning" => "Multi-Session Reasoning",
        "summarization" => "Summarization",
        "temporal_reasoning" => "Temporal Reasoning",
        "event_ordering" => "Event Ordering",
        "knowledge_update" => "Knowledge Update",
        "contradiction_resolution" => "Contradiction Resolution",
        "abstention" => "Abstention",
        "instruction_following" => "Instruction Following",
        _ => "Unknown",
    }
}

pub fn official_category(
    source_family: Option<&str>,
    question_type: Option<&str>,
) -> Option<(String, String)> {
    let family = source_family?;
    let qtype = normalize_key(question_type?);
    if family == "longmemeval" {
        let canonical = match qtype.as_str() {
            "single_session_user" => "single-session-user",
            "single_session_assistant" => "single-session-assistant",
            "single_session_preference" => "single-session-preference",
            "multi_session" => "multi-session",
            "temporal_reasoning" => "temporal-reasoning",
            "knowledge_update" => "knowledge-update",
            "abstention" => "abstention",
            _ if qtype.ends_with("_abs") => "abstention",
            _ => return None,
        };
        return Some((
            canonical.into(),
            longmemeval_category_label(canonical).into(),
        ));
    }
    if family == "beam" {
        let canonical = match qtype.as_str() {
            "information_extraction" => "information_extraction",
            "preference_following" => "preference_following",
            "multi_session_reasoning" => "multi_session_reasoning",
            "summarization" => "summarization",
            "temporal_reasoning" => "temporal_reasoning",
            "event_ordering" => "event_ordering",
            "knowledge_update" => "knowledge_update",
            "contradiction_resolution" => "contradiction_resolution",
            "abstention" => "abstention",
            "instruction_following" => "instruction_following",
            _ => return None,
        };
        return Some((canonical.into(), beam_ability_label(canonical).into()));
    }
    None
}

pub fn category_breakdown(
    values: &HashMap<String, Vec<f64>>,
    labels: &HashMap<String, String>,
) -> HashMap<String, CategoryBreakdown> {
    values
        .iter()
        .map(|(key, scores)| {
            let avg = if scores.is_empty() {
                0.0
            } else {
                scores.iter().sum::<f64>() / scores.len() as f64
            };
            (
                key.clone(),
                CategoryBreakdown {
                    label: labels.get(key).cloned().unwrap_or_else(|| key.clone()),
                    scenario_count: scores.len(),
                    score: (avg * 100.0).round() / 100.0,
                    grade: grade(avg).into(),
                },
            )
        })
        .collect()
}
