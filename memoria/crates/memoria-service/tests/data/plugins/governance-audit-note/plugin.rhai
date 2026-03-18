fn memoria_plugin(ctx) {
    if ctx["phase"] == "plan" && ctx["task"] == "daily" {
        return #{
            estimated_impact: #{ "plugin.daily_audit": 0.5 }
        };
    }

    if ctx["phase"] == "execute" {
        return #{
            decisions: [decision("plugin:audit-note", "Added plugin audit note", 0.75)],
            warnings: ["Audit-note plugin annotated the governance report"],
            metrics: #{ "plugin.audit_note.executed": 1.0 }
        };
    }

    return #{};
}
