---
name: memoria-memory
description: |
  Use Memoria for durable user/project memory in OpenClaw.
  Triggers: "remember this", "save to memory", "what do you remember",
  "forget this", "correct memory", "update memory", "use memoria".
---

# Memoria Memory

Use Memoria tools for durable memory. Do not default to `MEMORY.md` or `memory/YYYY-MM-DD.md` unless the user explicitly asks for file-based memory.

## When to use

- The user asks you to remember a fact, preference, decision, or workflow.
- The user asks what you already know about them, the project, or a prior session.
- The user asks to correct, update, or delete stored memory.

## Store

1. Choose the smallest durable fact worth keeping.
2. Use the most specific tool available:
   - `memory_profile` for stable user preferences or profile traits
   - `memory_store` for facts, procedures, and project knowledge
3. Prefer short, atomic entries.
4. After storing something important, verify with `memory_recall` or `memory_search`.

## Recall

1. When the user asks "what do you know", "what do you remember", or refers to a prior session, query Memoria first.
2. Use `memory_recall` for semantic retrieval and `memory_get` only when you already have a specific memory id.

## Repair

1. If the user says a memory is wrong, use `memory_correct`.
2. If the user wants it removed, use `memory_forget` or `memory_purge`.
3. After repair, verify with `memory_recall` or `memory_search`.

## Rules

- Do not claim only `memory_search` and `memory_get` exist when other `memory_*` tools are available.
- Do not store transient small talk unless the user asks or it is clearly a stable preference.
- Prefer Memoria for durable cross-session memory; prefer workspace files only for explicit file-based notes.
