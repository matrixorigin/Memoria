# Memory Slot Operations

Use this when OpenClaw should actively use Memoria as the durable memory slot.

## When To Retrieve

Retrieve first when:

- the user asks "what do you remember" or refers to prior work
- the task resumes after a gap
- the user asks for previous preferences, decisions, or project context
- the current prompt likely depends on earlier context

Preferred order:

1. `memory_retrieve` for prompt-relevant context
2. `memory_search` for broader semantic lookup
3. `memory_list` when the user wants a bounded inventory

## What To Store

Store only durable, reusable information:

- stable user preferences and profile facts
- important project decisions
- successful workflows and procedures
- durable facts learned during work
- milestone progress worth resuming later

Do not store:

- every conversational turn
- throwaway small talk
- temporary scratch reasoning
- secrets unless the user explicitly wants them remembered

## Which Write Tool To Use

- `memory_profile`: stable user traits and preferences
- `memory_store`: general durable facts, procedures, lessons, decisions
- `memory_correct`: fix a wrong memory
- `memory_forget` or `memory_purge`: remove memory that should no longer exist

After important writes or repairs, verify with `memory_retrieve`, `memory_search`, or `memory_list`.

## Rules

- Prefer short, atomic memories over long blended paragraphs.
- Prefer Memoria over workspace note files for cross-session memory.
- If the user asks for file-based memory, follow that explicitly instead.
