# Operations

Use this for normal Memoria behavior: storing memory, recalling it, repairing bad entries, taking snapshots, and isolating risky work on branches.

## When To Use Memoria

- The user asks you to remember a fact, preference, workflow, or decision.
- The user asks what you already know from prior sessions.
- The user wants to correct or delete stored memory.
- The task needs a checkpoint before destructive edits.
- The task benefits from branch, diff, merge, or rollback.

## Store

1. Keep entries short and atomic.
2. Use `memory_profile` for stable user traits and preferences.
3. Use `memory_store` for facts, procedures, project notes, and cross-session context.
4. Verify important writes with `memory_retrieve` or `memory_search`.

## Recall

1. Use `memory_retrieve` when the task needs relevant context for the current prompt.
2. Use `memory_search` for broader semantic lookup.
3. Use `memory_list` when the user wants a recent or bounded inventory.

## Repair

1. Use `memory_correct` when an entry is wrong but should still exist.
2. Use `memory_purge` or delete-by-id when the memory should be removed.
3. Re-run retrieval after repair so the user sees the corrected state.

## Recovery

Before bulk delete, purge, or large correction passes:

```text
memory_snapshot -> mutate -> verify
```

If something goes wrong:

```text
memory_snapshots -> choose snapshot -> memory_rollback -> verify
```

## Branching

Use branches for isolated experiments or risky memory rewrites.

```text
memory_branch(name="experiment")
memory_checkout(name="experiment")
... make changes ...
memory_diff(source="experiment")
memory_checkout(name="main")
memory_merge(source="experiment")
```

Delete the branch instead of merging when the experiment was not useful.

## Governance

Use these for maintenance, not normal user-facing recall:

- `memory_governance`: cleanup and quarantine
- `memory_consolidate`: contradiction and graph cleanup
- `memory_reflect`: synthesize higher-level insights

## Rules

- Do not store transient small talk unless the user asks or it is clearly durable.
- Snapshot before destructive bulk work.
- Prefer rollback over manually reconstructing a large deleted state.
- Prefer Memoria over scratch files for durable cross-session agent memory.
