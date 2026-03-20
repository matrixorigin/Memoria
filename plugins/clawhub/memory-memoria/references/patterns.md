# Usage Patterns

Use this when OpenClaw needs a repeatable pattern for working with Memoria.

## Conversation Start

At the start of a task or after a long gap:

1. `memory_retrieve(query=<current task or question>)`
2. If the task sounds like a continuing project, also search for active goals or recent decisions.
3. Answer using retrieved context instead of guessing.

## During Work

- New durable fact or decision: `memory_store`
- Stable preference: `memory_profile`
- Wrong memory: `memory_correct`
- Memory should be deleted: `memory_forget` or `memory_purge`

## End Of Task

Store the durable outcome if it will help later:

- final decision
- key lesson
- reusable workflow
- unfinished next step worth resuming

Clean up obsolete working memory when it no longer helps.

## Risky Memory Changes

Before bulk delete, major cleanup, or large rewrite:

```text
memory_snapshot -> mutate -> verify
```

If the result is bad:

```text
memory_snapshots -> memory_rollback -> verify
```

## Isolated Experiments

For a risky or reversible memory experiment:

```text
memory_branch(name="experiment")
memory_checkout(name="experiment")
... changes ...
memory_diff(source="experiment")
memory_checkout(name="main")
memory_merge(source="experiment")
```

Delete the branch instead of merging if the experiment was not useful.

## Maintenance

Use maintenance tools intentionally, not on every run:

- `memory_governance` for cleanup
- `memory_consolidate` for contradictions
- `memory_reflect` for higher-level synthesis
