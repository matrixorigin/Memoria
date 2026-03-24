---
name: thememoria
description: |
  Use Memoria as OpenClaw's durable memory slot.
  Triggers: "remember this", "save to memory", "what do you remember",
  "continue from last time", "forget this", "correct memory", "take a snapshot",
  "rollback memory", "branch memory", "merge memory", "use long-term memory".
version: 0.2.0
metadata:
  openclaw:
    emoji: "🧠"
    homepage: https://github.com/matrixorigin/Memoria
---

# Memoria

Use this skill when OpenClaw should treat Memoria as the durable external memory system for the current user or project.

## Routing

Pick the smallest reference that matches the task:

- Install or verify the OpenClaw plugin: `references/setup.md`
- Decide which memory tool to use: `references/tool-surface.md`
- Daily store, recall, correct, forget behavior: `references/operations.md`
- Session lifecycle, goals, recovery, branches, rollback: `references/patterns.md`

## Core Rules

1. Prefer Memoria tools over `MEMORY.md` or `memory/YYYY-MM-DD.md` unless the user explicitly asks for file-based notes.
2. Do not auto-store every turn. Save durable facts, preferences, decisions, workflows, and meaningful progress.
3. On task resume or "what do you remember" prompts, retrieve relevant memory first.
4. Use the most specific tool available: `memory_profile` for stable preferences, `memory_store` for general durable memory, `memory_correct` or `memory_forget` for repairs.
5. Before bulk delete, purge, or large rewrites, create a snapshot first.
6. Use branches for risky or reversible memory experiments, then diff and merge or delete.
7. After important writes, repairs, rollback, or merges, verify with retrieval or list tools.
8. Do not claim only `memory_search` or `memory_get` exist when other `memory_*` tools are available.

## Default Flow

1. At conversation start or task resume, use `memory_retrieve` or `memory_search` for relevant context.
2. During the conversation, store only the durable facts worth keeping.
3. If the user corrects or removes memory, repair it immediately with `memory_correct`, `memory_forget`, or `memory_purge`.
4. For risky memory maintenance, create a snapshot or branch before mutating state.
5. At the end of meaningful work, store the durable outcome and clean up obsolete working memory.

## Important Notes

- OpenClaw's built-in file memory (`openclaw memory`) is separate from Memoria (`openclaw memoria`).
- The plugin defaults to explicit writes, not silent auto-capture.
- `memory_get` is a compatibility helper; when in doubt, prefer `memory_retrieve`, `memory_search`, or `memory_list`.
- Memoria's core strengths are semantic retrieval, durable cross-session memory, snapshots, rollback, branches, merge, and governance.

## Quick Start

```bash
openclaw plugins install @matrixorigin/thememoria
openclaw memoria setup --mode cloud --api-url <MEMORIA_API_URL> --api-key <API_KEY>
openclaw memoria health
```
