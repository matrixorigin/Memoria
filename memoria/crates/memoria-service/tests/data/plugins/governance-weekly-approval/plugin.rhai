fn memoria_plugin(ctx) {
    if ctx["phase"] == "plan" && ctx["task"] == "weekly" {
        let approval = decision("plugin:weekly-approval", "Weekly governance requires review", 0.92);
        approval["evidence"] = [evidence("plugin.rhai", "Weekly task should emit an approval signal")];
        return #{
            requires_approval: true,
            actions: [approval],
            estimated_impact: #{ "plugin.approval_required": 1.0 }
        };
    }

    if ctx["phase"] == "execute" {
        return #{
            warnings: ["Weekly approval plugin reviewed builtin execution"],
            metrics: #{ "plugin.weekly_approval.executed": 1.0 }
        };
    }

    return #{};
}
