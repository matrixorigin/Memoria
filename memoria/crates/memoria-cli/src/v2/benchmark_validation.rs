use crate::benchmark::Scenario;

pub(crate) fn validate_scenario(scenario: &Scenario, errors: &mut Vec<String>) {
    for (i, seed) in scenario.seed_memories.iter().enumerate() {
        if seed.age_days.is_some() {
            errors.push(format!(
                "{}: seed_memories[{i}] uses age_days, which V2 API benchmark datasets do not support",
                scenario.scenario_id
            ));
        }
        if seed.initial_confidence.is_some() {
            errors.push(format!(
                "{}: seed_memories[{i}] uses initial_confidence, which V2 API benchmark datasets do not support",
                scenario.scenario_id
            ));
        }
    }

    for (i, step) in scenario.steps.iter().enumerate() {
        if step.age_days.is_some() {
            errors.push(format!(
                "{}: step[{i}] uses age_days, which V2 API benchmark datasets do not support",
                scenario.scenario_id
            ));
        }
        if step.initial_confidence.is_some() {
            errors.push(format!(
                "{}: step[{i}] uses initial_confidence, which V2 API benchmark datasets do not support",
                scenario.scenario_id
            ));
        }
    }
}
