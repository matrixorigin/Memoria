# Memory Branching Patterns

Use Memoria's Git-like branching to isolate experiments, evaluate alternatives, and protect stable memory state.

## Selective Apply

When only part of a branch should land on `main`, or you want conflict-by-conflict control, prefer `memory_apply` over merging the whole branch.

```md
memory_diff(source="experiment_notes")

memory_apply(
  source="experiment_notes",
  adds=["mem_new_1"],
  updates=[{"old_id": "mem_old_1", "new_id": "mem_new_1"}],
  removes=["mem_delete_1"],
  accept_branch_conflicts=["mem_conflict_1"]
)
```

Rules:
- Omit an item from `adds` / `updates` / `removes` to leave `main` unchanged for that item.
- Omit a conflict from `accept_branch_conflicts` to keep the `main` version.
- Use `memory_merge` only when the entire branch should land together.
