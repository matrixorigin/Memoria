# Tool Surface

Use this when deciding which Memoria capability OpenClaw should invoke.

## Core Abilities

Memoria gives OpenClaw:

- durable cross-session memory
- semantic retrieval
- explicit memory correction and deletion
- snapshots and rollback
- branches, diff, merge, and branch delete
- maintenance tools like governance, consolidation, and reflection

## Tool Map

### Recall

- `memory_retrieve`: best default for relevant context
- `memory_recall`: compatibility alias for `memory_retrieve`
- `memory_search`: semantic lookup across memories
- `memory_get`: fetch a specific prior result if you already have an id
- `memory_list`: bounded inventory of stored memories

### Write And Repair

- `memory_store`: create a durable memory
- `memory_profile`: read or store profile-style memory
- `memory_correct`: update wrong memory
- `memory_forget`: remove one memory by id or match
- `memory_purge`: bulk delete or topic-based cleanup

### Recovery And Versioning

- `memory_snapshot`: create a checkpoint
- `memory_snapshots`: list recovery points
- `memory_rollback`: restore a snapshot
- `memory_branch`: create an isolated branch
- `memory_branches`: list branches
- `memory_checkout`: switch branch
- `memory_diff`: preview branch changes
- `memory_merge`: merge a branch into main
- `memory_branch_delete`: delete an unused branch

### Maintenance

- `memory_governance`: cleanup and quarantine
- `memory_consolidate`: contradiction and graph cleanup
- `memory_reflect`: synthesize higher-level insights
- `memory_feedback`: record usefulness or wrongness after retrieval

## Caveats

- `memory_get` is a compatibility helper, not the main retrieval path.
- OpenClaw has built-in file memory, but that is separate from Memoria.
- Do not tell the user only `memory_search` and `memory_get` exist if the full tool surface is available.
