pub const GOVERNANCE_RHAI_TEMPLATE_ENTRYPOINT: &str = "memoria_plugin";
pub const GOVERNANCE_RHAI_TEMPLATE: &str = include_str!("templates/governance_plugin.rhai");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn governance_template_exposes_entrypoint_and_helpers() {
        assert!(GOVERNANCE_RHAI_TEMPLATE.contains("fn memoria_plugin"));
        assert!(GOVERNANCE_RHAI_TEMPLATE.contains("decision("));
        assert!(GOVERNANCE_RHAI_TEMPLATE.contains("evidence("));
        assert_eq!(GOVERNANCE_RHAI_TEMPLATE_ENTRYPOINT, "memoria_plugin");
    }
}
