# Memoria Guidance

For branch-based memory workflows, use `memory_apply` when only selected branch changes should be promoted back to `main`.

Rules:
- Run `memory_diff(source="...")` first to inspect adds / updates / removes / conflicts.
- Use `memory_apply(source="...", ...)` to promote only the chosen items.
- Omit a conflict from `accept_branch_conflicts` to keep the `main` version.
- Use `memory_merge` only when the entire branch should land together.
