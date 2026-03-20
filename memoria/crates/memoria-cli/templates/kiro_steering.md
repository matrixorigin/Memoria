---
inclusion: always
---

<!-- memoria-version: 0.1.0-->

# Memory Integration (Memoria Lite)

You have persistent memory via MCP tools. Memory survives across conversations.

## 🔴 MANDATORY: Every conversation start

Call `memory_retrieve` with a **semantic query** derived from the user's message BEFORE responding.

**Query rules:**
- ✅ Extract key concepts → "benchmark optimization", "graph retrieval bug"
- ❌ Don't use meta-queries → "all memories", "everything", "list all"

**After retrieval:**
- Results → use as reference, verify against current context
- "No relevant memories" → normal for new users, proceed
- ⚠️ warnings → inform user, offer `memory_governance`

## 🔴 MANDATORY: Every conversation turn
After responding, decide if anything is worth remembering:
- User stated a preference, fact, or decision → `memory_store`
- User corrected a previously stored fact → `memory_correct` (not `memory_store` + `memory_purge`)
- You learned something new about the project/workflow → `memory_store`
- Do NOT store: greetings, trivial questions, things already in memory.

**Deduplication is automatic.** The system detects semantically similar memories and supersedes old ones. You do not need to check for duplicates before storing.

If `memory_store` or `memory_correct` response contains ⚠️, tell the user — it means the embedding service is down and retrieval will degrade to keyword-only search.

## 🟡 When NOT to store (noise reduction)
Do NOT call `memory_store` for:
- **Transient debug context**: temporary print statements, one-off test values, ephemeral error messages
- **Vague or low-confidence observations**: "might be using X", "probably prefers Y" — wait for confirmation
- **Conversation-specific context** that won't matter next session: "currently looking at line 42", "just ran the test"
- **Information already in memory**: if `memory_retrieve` already returned it, don't store again
- **Trivial or obvious facts**: "user is writing code", "user asked a question"

## 🟡 Working memory lifecycle — CRITICAL for long debug sessions
`working` memories are session-scoped temporary context. They **persist and will be retrieved in future sessions** unless explicitly cleaned up.

**When to purge working memories:**
- Task or debug session is complete → `memory_purge(topic="<task keyword>", reason="task complete")`
- You stored a working memory that turned out to be wrong → `memory_purge(memory_id="...", reason="incorrect conclusion")`
- User says "start fresh", "forget what we tried", "let's try a different approach"
- Only purge completed tasks — leave active task working memories for next session

**Promote or purge as you go:**
- Hypothesis confirmed → `memory_store` the conclusion as `semantic`, then `memory_purge` the working memory
- Hypothesis disproven → `memory_purge` the working memory immediately
- Don't wait until session end to promote — do it as soon as you know

**When a working memory contradicts current findings:**
- Do NOT keep both. Purge the stale one immediately: `memory_purge(memory_id="...", reason="superseded by new finding")`
- Then store the correct conclusion as `semantic` (not `working`) if it's a durable fact

**Anti-pattern to avoid:** Storing "current bug is X" as working memory, then later finding out it's Y, but keeping both. The stale "bug is X" memory will keep surfacing and misleading future retrieval.

## 🟡 Correction workflow (prefer correct over store+purge)
When the user contradicts a previously stored fact:
1. **Always use `memory_correct`** — not `memory_store` + `memory_purge`. This preserves the audit trail.
2. **Prefer query-based correction**: `memory_correct(query="formatting tool", new_content="Uses ruff for formatting", reason="switched from black")` — no need to look up memory_id first.
3. **Only use `memory_purge`** when the user explicitly asks to forget something entirely, not when updating a fact.

## 🟡 Deduplication before storing
Before storing a new memory, consider:
- Did `memory_retrieve` at conversation start already return a similar fact? → skip or `memory_correct` instead
- Is this a refinement of something already stored? → use `memory_correct` with the original as query
- When in doubt, `memory_search` with the key phrase first — if a match exists, correct it rather than creating a duplicate

## Tool reference

### Write tools
| Tool | When to use | Key params |
|------|-------------|------------|
| `memory_store` | User shares a fact, preference, or decision | `content`, `memory_type` (default: semantic), `session_id` (optional) |
| `memory_correct` | User says a stored memory is wrong | `memory_id` or `query` (one required), `new_content`, `reason` |
| `memory_purge` | User asks to forget something | `memory_id` (single or comma-separated batch, e.g. `"id1,id2"`) or `topic` (bulk keyword match), `reason` |

`memory_purge` automatically creates a safety snapshot before deleting. The response includes the snapshot name — tell the user they can `memory_rollback` to undo. If the response contains a ⚠️ warning about snapshot quota, relay it and suggest `memory_snapshot_delete(prefix="pre_")`.

### Read tools
| Tool | When to use | Key params |
|------|-------------|------------|
| `memory_retrieve` | Conversation start, or when context is needed | `query`, `top_k` (default 5), `session_id` (optional), `explain` (false = no debug, true = show timing) |
| `memory_search` | User asks "what do you know about X" or you need to browse | `query`, `top_k` (default 10), `explain` (false = no debug, true = show timing) |
| `memory_profile` | User asks "what do you know about me" | — |
| `memory_feedback` | After using a retrieved memory, record if it was helpful | `memory_id`, `signal` (useful/irrelevant/outdated/wrong), `context` (optional) |

**`memory_feedback`**: Call this after retrieval when you can assess whether a memory was helpful. Signals:
- `useful` — memory helped answer the question or complete the task
- `irrelevant` — memory was retrieved but not relevant to the query
- `outdated` — memory contains stale information (consider `memory_correct` instead if you know the new value)
- `wrong` — memory contains incorrect information (consider `memory_correct` instead if you know the correct value)

**When to call feedback vs other tools**:
- Memory helped → `memory_feedback(signal="useful")`
- Memory irrelevant but correct → `memory_feedback(signal="irrelevant")`
- Memory outdated and you know new value → `memory_correct` (not feedback)
- Memory outdated but you don't know new value → `memory_feedback(signal="outdated")`
- Memory wrong and you know correct value → `memory_correct` (not feedback)
- Memory should be deleted → `memory_purge` (not feedback)

**Example flow**:
```
# 1. Retrieve memories
memories = memory_retrieve(query="database config")

# 2. Use memories to answer user's question
# ... (memory about "Uses PostgreSQL" helped answer)

# 3. Record feedback for the helpful memory
memory_feedback(memory_id="abc123", signal="useful", context="answered DB question")
```

**Impact**: Feedback accumulates over time. With default settings, a memory with 3 `useful` signals ranks ~30% higher in future retrievals. Don't call for every memory — only when you have clear signal.

**`memory_retrieve` vs `memory_search`**: In MCP mode, both use the same retrieval pipeline (graph → hybrid vector+fulltext → fulltext fallback). The differences are:
- `memory_retrieve` accepts `session_id` for session-scoped boosting; `memory_search` does not
- `memory_retrieve` defaults to `top_k=5` (focused); `memory_search` defaults to `top_k=10` (broader)
- Use `memory_retrieve` when you have a `session_id` or want focused results; use `memory_search` for broader exploration

**Debug parameter:** `explain=true` shows execution timing and retrieval path. **ONLY use when user explicitly asks** to debug performance or investigate why certain memories were/weren't retrieved. **DO NOT use proactively** — it adds overhead and clutters output.

**When to use explain:**
- ✅ User says: "why is this slow", "show me the retrieval path", "debug this query"
- ❌ Normal retrieval — never add explain unless user asks

### Memory types
| Type | Use for | Examples |
|------|---------|---------|
| `semantic` | Project facts, technical decisions (default) | "Uses MatrixOne as primary DB", "API follows REST conventions" |
| `profile` | User/agent identity and preferences | "Prefers concise answers", "Works on mo-dev-agent project" |
| `procedural` | How-to knowledge, workflows | "Deploy with: make dev-start", "Run tests with pytest -n auto" |
| `working` | Temporary context for current task | "Currently debugging embedding issue" |
| `tool_result` | Tool execution results worth caching | "Last CI run: 126 passed, 0 failed" |
| `episodic` | Session summaries (topic/action/outcome) | "Session Summary: Database optimization\n\nActions: Added indexes\n\nOutcome: 93% faster" |

### Snapshots (save/restore/cleanup)
Use before risky changes. `memory_snapshot(name)` saves state, `memory_rollback(name)` restores it, `memory_snapshots(limit, offset)` lists with pagination, `memory_snapshot_delete(names|prefix|older_than)` cleans up.

When `memory_governance` reports snapshot_health with high auto_ratio (>50%), suggest cleanup:
- `memory_snapshot_delete(prefix="auto:")` — remove auto-generated snapshots
- `memory_snapshot_delete(prefix="pre_")` — remove safety snapshots from purge/correct
- `memory_snapshot_delete(older_than="2026-01-01")` — remove snapshots before a date

### Branches (isolated experiments)
Git-like workflow for memory. `memory_branch(name)` creates, `memory_checkout(name)` switches, `memory_diff(source)` previews changes, `memory_merge(source)` merges back, `memory_branch_delete(name)` cleans up. `memory_branches()` lists all.

### Entity graph (proactive — call when conditions are met)
| Tool | When to call | Key params |
|------|-------------|------------|
| `memory_extract_entities` | **Proactively** after storing ≥ 5 new memories in a session, OR when user discusses a new project/technology/person not yet in the graph | `mode` (default: auto) |
| `memory_link_entities` | After `extract_entities(mode='candidates')` returns memories — extract entities yourself, then call this | `entities` (JSON string) |

**Trigger heuristics — call `memory_extract_entities` when ANY of these are true:**
- You stored ≥ 5 memories this session and haven't extracted entities yet
- User mentions a project, technology, or person by name that you haven't seen in previous `memory_retrieve` results
- User asks about relationships between concepts ("how does X relate to Y")
- User starts working on a new codebase or topic area

**Do NOT extract entities when:**
- Conversation is short (< 3 turns) and no new named entities appeared
- User is only asking questions, not sharing new information
- You already ran extraction this session

### Maintenance (proactive triggers in [memory-hygiene](memory-hygiene.md), manual triggers below)
| Tool | Trigger phrase | Cooldown |
|------|---------------|----------|
| `memory_governance` | "clean up memories", "check memory health", or proactively per [memory-hygiene](memory-hygiene.md) | 1 hour |
| `memory_consolidate` | "check for contradictions", "fix conflicts" | 30 min |
| `memory_reflect` | "find patterns", "summarize what you know" | 2 hours |
| `memory_rebuild_index` | Only when governance reports `needs_rebuild=True` | — |
| `memory_snapshot_delete` | When governance reports high snapshot auto_ratio, or user asks to clean snapshots | — |

`memory_reflect` and `memory_extract_entities` support `mode` parameter:
- `auto` (default): uses Memoria's internal LLM if configured, otherwise returns candidates for YOU to process
- `candidates`: always returns raw data for YOU to synthesize/extract, then store results via `memory_store` or `memory_link_entities`
- `internal`: always uses Memoria's internal LLM (fails if not configured)
