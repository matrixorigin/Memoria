//! Native Rust benchmark — schema, executor, taxonomy, and scorer.
//! Kept inside `memoria-cli` so benchmark evolution stays isolated from core service logic.

#[path = "benchmark_executor.rs"]
mod benchmark_executor;
#[path = "benchmark_schema.rs"]
mod benchmark_schema;
#[path = "benchmark_scoring.rs"]
mod benchmark_scoring;
#[path = "benchmark_taxonomy.rs"]
mod benchmark_taxonomy;

pub use benchmark_executor::BenchmarkExecutor;
#[allow(unused_imports)]
pub use benchmark_schema::{
    AssertionResult, BenchmarkReport, CategoryBreakdown, MemoryAssertion, Scenario,
    ScenarioDataset, ScenarioExecution, ScenarioResult, ScenarioStep, SeedMemory, StepResult,
};
pub use benchmark_scoring::{score_dataset, score_scenario};
#[allow(unused_imports)]
pub use benchmark_taxonomy::{official_category, scenario_question_type, scenario_source_family};

use std::collections::HashSet;

pub fn validate_dataset(content: &str) -> Vec<String> {
    let mut errors = vec![];
    let dataset: ScenarioDataset = match serde_json::from_str(content) {
        Ok(d) => d,
        Err(e) => {
            errors.push(format!("JSON parse error: {e}"));
            return errors;
        }
    };
    let mut ids = HashSet::new();
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
                errors.push(format!(
                    "{}: assertion[{i}] has no expected_contents",
                    s.scenario_id
                ));
            }
        }
        for (i, step) in s.steps.iter().enumerate() {
            match step.action.as_str() {
                "retrieve" | "search" if step.query.is_none() => errors.push(format!(
                    "{}: step[{i}] {} requires query",
                    s.scenario_id, step.action
                )),
                "store" if step.content.is_none() => errors.push(format!(
                    "{}: step[{i}] store requires content",
                    s.scenario_id
                )),
                "correct" if step.content.is_none() || step.query.is_none() => {
                    errors.push(format!(
                        "{}: step[{i}] correct requires content+query",
                        s.scenario_id
                    ))
                }
                _ => {}
            }
        }
    }
    errors
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;

    fn scenario_with_metadata(
        scenario_id: &str,
        source_family: &str,
        question_type: &str,
    ) -> Scenario {
        Scenario {
            scenario_id: scenario_id.into(),
            title: "test".into(),
            description: String::new(),
            domain: if source_family == "beam" {
                "beam"
            } else {
                "longmem"
            }
            .into(),
            difficulty: "L1".into(),
            horizon: "short".into(),
            tags: vec![],
            source_family: None,
            question_type: None,
            metadata: HashMap::from([
                ("source_family".into(), json!(source_family)),
                ("question_type".into(), json!(question_type)),
            ]),
            seed_memories: vec![SeedMemory {
                content: "memory".into(),
                memory_type: "semantic".into(),
                is_outdated: false,
                age_days: None,
                initial_confidence: None,
                trust_tier: None,
            }],
            maturation: vec![],
            steps: vec![],
            assertions: vec![MemoryAssertion {
                query: "query".into(),
                top_k: 3,
                expected_contents: vec!["memory".into()],
                excluded_contents: vec![],
            }],
        }
    }

    fn pass_exec(id: &str) -> ScenarioExecution {
        ScenarioExecution {
            _scenario_id: id.into(),
            step_results: vec![],
            assertion_results: vec![AssertionResult {
                _query: "query".into(),
                returned_contents: vec!["memory".into()],
                _error: None,
            }],
            error: None,
        }
    }

    #[test]
    fn groups_longmemeval_by_official_category() {
        let scenarios = vec![
            scenario_with_metadata("lme-1", "longmemeval", "single-session-preference"),
            scenario_with_metadata("lme-2", "longmemeval", "knowledge-update"),
        ];
        let dataset = ScenarioDataset {
            dataset_id: "longmemeval-oracle".into(),
            version: "v1".into(),
            scenarios,
        };
        let executions = HashMap::from([
            ("lme-1".into(), pass_exec("lme-1")),
            (
                "lme-2".into(),
                ScenarioExecution {
                    _scenario_id: "lme-2".into(),
                    step_results: vec![],
                    assertion_results: vec![AssertionResult {
                        _query: "query".into(),
                        returned_contents: vec![],
                        _error: None,
                    }],
                    error: None,
                },
            ),
        ]);
        let report = score_dataset(&dataset, &executions);
        assert!(report
            .by_longmemeval_category
            .contains_key("single-session-preference"));
        assert!(report
            .by_longmemeval_category
            .contains_key("knowledge-update"));
        assert_eq!(
            report.results[0].official_category.as_deref(),
            Some("single-session-preference")
        );
    }

    #[test]
    fn groups_beam_by_official_ability() {
        let scenarios = vec![
            scenario_with_metadata("beam-1", "beam", "event_ordering"),
            scenario_with_metadata("beam-2", "beam", "instruction_following"),
        ];
        let dataset = ScenarioDataset {
            dataset_id: "beam-100k".into(),
            version: "v1".into(),
            scenarios,
        };
        let executions = HashMap::from([
            ("beam-1".into(), pass_exec("beam-1")),
            ("beam-2".into(), pass_exec("beam-2")),
        ]);
        let report = score_dataset(&dataset, &executions);
        assert!(report.by_beam_ability.contains_key("event_ordering"));
        assert!(report.by_beam_ability.contains_key("instruction_following"));
        assert_eq!(
            report.results[1].official_category_label.as_deref(),
            Some("Instruction Following")
        );
    }
}
