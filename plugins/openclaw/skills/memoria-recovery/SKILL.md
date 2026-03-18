---
name: memoria-recovery
description: |
  Recover from accidental memory loss with snapshots and rollback.
  Triggers: "restore memory", "rollback memory", "undo deleted memory",
  "recover deleted memory", "take a memory snapshot".
---

# Memoria Recovery

Use Memoria snapshot tools when memory state needs a checkpoint or a rollback.

## Before risky changes

1. If you are about to bulk-delete, purge, or rewrite memory, create a snapshot first with `memory_snapshot`.
2. Tell the user a checkpoint exists and can be restored.

## After accidental deletion or corruption

1. Inspect available recovery points with `memory_snapshots`.
2. Choose the most recent good snapshot.
3. Restore with `memory_rollback`.
4. Verify recovery with `memory_recall`, `memory_list`, or `memory_stats`.

## Rules

- Prefer rollback over manually re-creating many deleted memories.
- If the user wants a selective logical fix rather than full restore, use `memory_correct` or `memory_forget` instead.
- After rollback, state what was restored and what you verified.
