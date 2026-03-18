fn memoria_plugin(ctx) {
    if ctx["phase"] == "plan" {
        let review = decision(
            "plugin:review-required",
            "Template governance plugin requested a lightweight review",
            0.75
        );
        review["evidence"] = [
            evidence(
                "plugin-template",
                "Replace this template evidence with domain-specific facts"
            )
        ];

        return #{
            requires_approval: false,
            actions: [review],
            estimated_impact: #{ "plugin.template.plan": 1.0 }
        };
    }

    if ctx["phase"] == "execute" {
        return #{
            warnings: ["Template governance plugin executed"],
            metrics: #{ "plugin.template.execute": 1.0 }
        };
    }

    return #{};
}
