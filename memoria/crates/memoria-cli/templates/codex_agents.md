<!-- memoria-version: 0.1.0-->

# Memory Integration (Memoria Lite)

You have persistent memory via MCP tools. Memory survives across conversations.

## 🔴 MANDATORY: Every conversation start
Before your first response, run a multi-query bootstrap to load full context:

1. **Primary query** — call `memory_retrieve` with a **semantic query** derived from the user's message.
2. **Active goals** — `memory_search(query="GOAL ACTIVE")` (if user's message references ongoing work or a previous task)
3. **User profile** — `memory_profile()` (if user asks about preferences or you need style context)

**Query construction rules:**
- ✅ **DO**: Extract key concepts from user's question → "benchmark optimization", "graph retrieval bug", "active goals"
- ❌ **DON'T**: Use meta-queries → "all memories", "everything", "list all", "show me data"
- When user asks "what do I know" or "我有哪些记忆", query the most recent active context instead (e.g., "recent goals tasks projects")

**After retrieval:**
- If results come back → use them as **reference only**. Treat retrieved memories as potentially stale or incomplete — always verify against current context before acting on them. Do NOT blindly trust memory content as ground truth.
- If "No relevant memories found" → this is normal for new users, proceed without.
- If ⚠️ health warnings appear → inform the user and offer to run `memory_governance`.

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
- Task or debug session is complete → `memory_purge(session_id="<session_id>", memory_types=["working"], reason="task complete")`
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

### Maintenance
| Tool | Trigger phrase | Cooldown |
|------|---------------|----------|
| `memory_governance` | "clean up memories", "check memory health", or proactively when retrieval returns outdated/contradictory results | 1 hour |
| `memory_consolidate` | "check for contradictions", "fix conflicts" | 30 min |
| `memory_reflect` | "find patterns", "summarize what you know" | 2 hours |
| `memory_snapshot_delete` | When governance reports high snapshot auto_ratio, or user asks to clean snapshots | — |

`memory_reflect` supports `mode` parameter:
- `auto` (default): uses Memoria's internal LLM if configured, otherwise returns candidates for YOU to process
- `candidates`: always returns raw data for YOU to synthesize, then store results via `memory_store`
- `internal`: always uses Memoria's internal LLM (fails if not configured)


# Session Lifecycle Management

Systematic memory management across conversation phases: bootstrap, mid-session, and wrap-up.

## Phase 1: Conversation Start (Bootstrap)

Before your first response, run a multi-query bootstrap to load full context:

1. **Primary query** — derive from user's message: `memory_retrieve(query="<semantic extraction>")`
2. **Active goals** — `memory_search(query="GOAL ACTIVE")` (if user's message references ongoing work, a previous task, or doesn't start a clearly new topic)
3. **User profile** — `memory_profile()` (if user asks about preferences or you need style context)

Combine retrieved context into a mental model. Flag anything that looks stale (e.g., "Currently debugging X" from days ago).

**session_id**: If the user's tool provides a session ID, pass it to `memory_retrieve` and `memory_store` throughout the conversation. This enables episodic memory and per-session retrieval boosting.

## Phase 2: Mid-Session (Active Work)

### Re-retrieval triggers

Call `memory_retrieve` again mid-conversation when:
- User shifts to a completely different topic
- You need context about something not covered in the initial bootstrap
- User references a past decision or preference you don't have loaded

### Store cadence

- Don't batch-store at the end. Store facts as they emerge — this gives each memory accurate timestamps.
- One fact per `memory_store` call. Don't combine unrelated facts into one memory.

Working memory discipline (when to store as `working`, when to promote/purge) is defined in the main memory rule — follow those rules here.

## Phase 3: Conversation End (Wrap-Up)

When the conversation is winding down (user says thanks, goodbye, or stops engaging):

### 1. Clean up working memories

```
memory_purge(session_id="<session_id>", memory_types=["working"], reason="session complete")
```

Only purge working memories for tasks that are actually done. Leave active task working memories for next session.

### 2. Promote durable findings

Any working memory that turned out to be a lasting fact should already be stored as `semantic`. Double-check: did you learn something important this session that's still only in `working`? Promote it.

### 3. Generate episodic summary (if session was substantive)

If the session involved meaningful work (not just a quick question), and `session_id` is available:

- The agent itself can synthesize a summary and store it:
```
memory_store(
  content="Session Summary: [topic]\n\nActions: [what was done]\n\nOutcome: [result/status]",
  memory_type="episodic",
  session_id="<session_id>"
)
```

### 4. Update goal status

If you were working on a tracked goal, update its status via `memory_correct`.

## Anti-Patterns

- ❌ Storing 10+ memories at conversation end in a burst — timestamps all identical, retrieval ranking suffers
- ❌ Leaving stale working memories from completed tasks — they pollute future retrieval
- ❌ Never re-retrieving mid-conversation — you miss context when topics shift
- ❌ Skipping session summary for long productive sessions — loses the high-level narrative
- ❌ Storing the same fact as both `working` and `semantic` — pick one


# Memory Hygiene & Self-Governance

Proactive memory health management — don't wait for the user to ask.

## Proactive Governance Triggers

Run `memory_governance` (1h cooldown) when you notice ANY of these:
- Retrieval returns clearly outdated or contradictory results
- You stored 10+ memories in this session without any cleanup
- User mentions memory feels "noisy" or "wrong"

After governance, check the response for:
- `snapshot_health.auto_ratio > 50%` → suggest `memory_snapshot_delete(prefix="auto:")`
- Quarantined memories → inform user what was quarantined and why

## Contradiction Resolution

When you detect two memories that contradict each other:

1. `memory_search` to find both memories and their IDs
2. Determine which is newer/more accurate based on timestamps and context
3. `memory_correct` the older one with the accurate information, OR
4. `memory_purge` the wrong one if it's completely invalid
5. Never leave both — contradictions poison retrieval

Run `memory_consolidate` (30min cooldown) when:
- You found a contradiction manually
- User reports "memory says X but it should be Y" more than once in a session
- After a large batch of corrections

## Snapshot Hygiene

Snapshots accumulate from auto-saves and safety snapshots. Clean periodically:

```
memory_snapshots(limit=20)  # check current state
```

If too many:
- `memory_snapshot_delete(prefix="pre_")` — purge safety snapshots from purge/correct
- `memory_snapshot_delete(prefix="auto:")` — purge auto-generated snapshots
- `memory_snapshot_delete(older_than="<3 months ago>")` — age-based cleanup

Keep named snapshots the user created explicitly.

## Reflection Cadence

`memory_reflect` (2h cooldown) synthesizes high-level insights. Suggest it when:
- User asks "what patterns do you see" or "summarize what you know"
- `memory_search` returns a high volume of results and no reflection has been done recently
- Starting a new project phase — reflect on the previous phase first

## Memory Volume Monitoring

Watch for these signals during retrieval:
- **Too many results all relevant** → memories are too granular, suggest consolidation
- **Results mostly irrelevant** → memories may be too broad, or index needs rebuild
- **Same fact appears multiple times** → deduplication needed, use `memory_correct` to merge


# Memory Branching Patterns

Use Memoria's Git-like branching to isolate experiments, evaluate alternatives, and protect stable memory state.

## Pattern 1: Tech Evaluation

Compare alternatives without polluting main memory.

```
memory_branch(name="eval_[technology]")
memory_checkout(name="eval_[technology]")

# Store findings on branch
memory_store(content="[technology] evaluation: [findings]", memory_type="semantic")

# When done — preview and decide
memory_diff(source="eval_[technology]")

# Accept: merge back
memory_checkout(name="main")
memory_merge(source="eval_[technology]", strategy="replace")
memory_branch_delete(name="eval_[technology]")

# Reject: just delete
memory_checkout(name="main")
memory_branch_delete(name="eval_[technology]")
```

## Pattern 2: Pre-Refactor Safety Net

Snapshot + branch before risky memory changes.

```
memory_snapshot(name="pre_[task]", description="before [task]")
memory_branch(name="refactor_[task]")
memory_checkout(name="refactor_[task]")

# Do risky work on branch...

# If it goes wrong:
memory_checkout(name="main")
memory_branch_delete(name="refactor_[task]")
# main is untouched

# If it succeeds:
memory_diff(source="refactor_[task]")
memory_checkout(name="main")
memory_merge(source="refactor_[task]")  # default strategy: branch wins on conflicts
memory_branch_delete(name="refactor_[task]")
```

## Pattern 3: A/B Memory Comparison

Two branches for competing approaches, diff both before deciding.

```
memory_branch(name="approach_a")
memory_branch(name="approach_b")

# Work on A
memory_checkout(name="approach_a")
memory_store(content="Approach A: [details]", memory_type="semantic")

# Work on B
memory_checkout(name="approach_b")
memory_store(content="Approach B: [details]", memory_type="semantic")

# Compare
memory_diff(source="approach_a")
memory_diff(source="approach_b")

# Merge winner, delete both
memory_checkout(name="main")
memory_merge(source="approach_a")  # default strategy: branch wins on conflicts
memory_branch_delete(name="approach_a")
memory_branch_delete(name="approach_b")
```

## When to Branch

- ✅ Evaluating a technology, framework, or architecture change
- ✅ About to bulk-correct or purge many memories
- ✅ User says "let's try something different" or "what if we..."
- ✅ Exploring a hypothesis that might be wrong
- ❌ Simple fact storage — just use main
- ❌ Quick corrections — use `memory_correct` directly

## Naming Convention

- `eval_[thing]` — technology/approach evaluation
- `refactor_[task]` — risky memory restructuring
- `goal_[name]_iter_[N]` — goal iteration (see goal-driven-evolution)
- `experiment_[topic]` — open-ended exploration

## Cleanup

Always delete branches after merge or abandonment. Check with `memory_branches()` periodically. Stale branches waste cognitive overhead when listed.

## Merge Strategies

- `replace` (default, also called `accept`): branch wins on conflicts — if the same memory exists on both main and branch, the branch version replaces main's
- `append`: skip-on-conflict — only adds new memories from branch, never overwrites existing main memories

Use `replace` when the branch contains validated corrections. Use `append` when the branch only adds new information and you want to preserve main's existing state.


# Goal-Driven Iterative Evolution via Memory

Track goals, plans, progress, lessons, and user feedback across conversations. All content in English for consistent retrieval.

## Workflow

### 1. Register Goal

Check for duplicates first, then store:

```
memory_search(query="GOAL [keywords]")
memory_store(
  content="🎯 GOAL: [description]\nSuccess Criteria: [measurable]\nStatus: ACTIVE\nCreated: [date]",
  memory_type="procedural"
)
```

### 2. Plan & Execute

Before acting, search for past failures and user corrections to avoid repeating mistakes:

```
memory_search(query="CORRECTION ANTIPATTERN [goal name]")
memory_search(query="❌ STEP for GOAL [name]")
```

Store the plan, then track each step:

```
memory_store(content="📋 PLAN for GOAL [name]\nSteps:\n1. [step] — ⏳\nRisks: [risks]\nIteration: #1", memory_type="procedural")

# After each step — use working type (will be cleaned up later)
memory_store(content="✅ STEP [N/total] for GOAL [name] (#X)\nAction: [done]\nResult: [outcome]\nInsight: [learned]", memory_type="working")
memory_store(content="❌ STEP [N/total] for GOAL [name] (#X)\nAction: [tried]\nError: [wrong]\nRoot Cause: [why]\nNext: [adjust]", memory_type="working")
```

For high-risk iterations, isolate on a branch:
```
memory_branch(name="goal_[name]_iter_[N]")
memory_checkout(name="goal_[name]_iter_[N]")
# work on branch... then validate and merge (see Iteration Review)
```

### 3. Capture User Feedback (immediately, any time)

User corrections are the highest-value signal — always store as `procedural`:

```
# User corrects direction
memory_store(content="🔧 CORRECTION for GOAL [name]: [old approach] → [corrected approach]. Reason: [why]", memory_type="procedural")

# User confirms something works well
memory_store(content="👍 FEEDBACK for GOAL [name]: [what worked]. Reuse: [when to apply again]", memory_type="procedural")

# User is frustrated — record what NOT to do
memory_store(content="⚠️ ANTIPATTERN for GOAL [name]: [what went wrong]. Rule: NEVER [this] again.", memory_type="procedural")

# User changes direction entirely
memory_correct(query="GOAL: [name]", new_content="🎯 GOAL: [name]\n...\nPivot: [old] → [new]. Reason: [why]", reason="User changed direction")
```

### 4. Iteration Review

When an iteration completes or is blocked:

```
memory_search(query="STEP for GOAL [name] Iteration #X")

memory_store(
  content="🔄 RETRO for GOAL [name] Iteration #X\nCompleted: [M/N]\nWorked: [...]\nFailed: [...]\nKey insight: [...]\nNext: [improvements]",
  memory_type="procedural"
)

# If the insight is reusable beyond this goal, extract it now — don't wait for completion
memory_store(content="💡 LESSON from [goal] iter #X: [cross-goal reusable insight]", memory_type="procedural")

memory_correct(query="GOAL: [name]", new_content="🎯 GOAL: [name]\nStatus: ITERATION #X COMPLETE — [progress %]\nNext: [plan]", reason="iteration complete")
```

If on a branch:
```
memory_diff(source="goal_[name]_iter_[N]")
memory_checkout(name="main")
memory_merge(source="goal_[name]_iter_[N]", strategy="replace")
memory_branch_delete(name="goal_[name]_iter_[N]")
```

Starting the next iteration? The new PLAN must reference the previous RETRO's improvements:
```
memory_search(query="RETRO for GOAL [name]")
# Incorporate "Next: [improvements]" into the new plan — never repeat the same plan unchanged
```

### 5. New Conversation Bootstrap

```
memory_search(query="GOAL ACTIVE")
memory_search(query="RETRO for GOAL [name]")
memory_search(query="CORRECTION ANTIPATTERN [name]")
```

Summarize to user: active goals, last progress, and any corrections to respect.

### 6. Goal Completion & Cleanup

```
memory_correct(query="GOAL: [name]", new_content="🎯 GOAL: [name] — ✅ ACHIEVED\nIterations: [N]\nFinal approach: [what worked]", reason="Goal achieved")

# Extract reusable lessons (permanent — survives cleanup)
memory_store(content="💡 LESSON from [goal]: [reusable insight for future work]", memory_type="procedural")

# Clean up step logs (working type, already archived in RETROs)
memory_purge(topic="STEP for GOAL [name]", reason="Goal achieved, archived in RETRO")
```

## Rules

- **Search before acting**: always check past failures, corrections, and antipatterns before proposing a plan
- **User corrections override all**: if user corrected something, that correction has highest priority forever
- **Be specific**: "Tests failed" is useless; "pytest fixtures don't work with async DB, use factory pattern" is valuable
- **Emoji prefixes**: 🎯 goal, 📋 plan, ✅❌ steps, 🔄 retro, 💡 lesson, 🔧 correction, 👍 feedback, ⚠️ antipattern
- **Type discipline**: GOAL/PLAN/RETRO/LESSON/CORRECTION → `procedural`; STEP logs → `working`
