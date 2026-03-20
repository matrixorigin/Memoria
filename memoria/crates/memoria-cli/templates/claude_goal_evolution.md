<!-- memoria-version: 0.1.0-->

# Goal-Driven Evolution + Plan Integration

Track goals, plans, progress, lessons, and user feedback across conversations.

## Before Starting Any Multi-Step Task

Query memory first:

```
memory_search(query="GOAL [topic]")                    # existing related goals
memory_search(query="LESSON [topic]")                  # past learnings
memory_search(query="CORRECTION ANTIPATTERN [topic]") # what NOT to do
```

If an active goal exists, continue it instead of creating a new one.

## Register Goal

For multi-session work (skip for trivial single-session tasks):

```
memory_store(
  content="🎯 GOAL: [description]\nSuccess Criteria: [measurable]\nStatus: ACTIVE",
  memory_type="procedural"
)
```

## During Execution

Track steps (use `working` type — will be cleaned up later):

```
memory_store(content="✅ STEP [N/total] for GOAL [name]\nAction: [done]\nInsight: [learned]", memory_type="working")
memory_store(content="❌ STEP [N/total] for GOAL [name]\nAction: [tried]\nRoot Cause: [why]\nNext: [adjust]", memory_type="working")
```

Only store non-obvious insights. Don't store "ran tests, passed".

For high-risk work, isolate on a branch:
```
memory_branch(name="goal_[name]_iter_[N]")
memory_checkout(name="goal_[name]_iter_[N]")
# work... then merge or delete
```

## Capture User Feedback (immediately)

User corrections are highest-value — always store as `procedural`:

```
memory_store(content="🔧 CORRECTION for GOAL [name]: [old] → [new]. Reason: [why]", memory_type="procedural")
memory_store(content="⚠️ ANTIPATTERN for GOAL [name]: [what went wrong]. Rule: NEVER [this] again.", memory_type="procedural")
```

## Iteration Review

When an iteration completes:

```
memory_store(
  content="🔄 RETRO for GOAL [name] Iteration #X\nWorked: [...]\nFailed: [...]\nNext: [improvements]",
  memory_type="procedural"
)
memory_correct(query="GOAL: [name]", new_content="🎯 GOAL: [name]\nStatus: ITERATION #X COMPLETE", reason="iteration complete")
```

## New Conversation Bootstrap

```
memory_search(query="GOAL ACTIVE")
memory_search(query="CORRECTION ANTIPATTERN [name]")
```

## Goal Completion & Cleanup

```
memory_correct(query="GOAL: [name]", new_content="🎯 GOAL: [name] — ✅ ACHIEVED", reason="Goal achieved")
memory_store(content="💡 LESSON from [goal]: [reusable insight]", memory_type="procedural")
memory_purge(topic="STEP for GOAL [name]", reason="archived in RETRO")
```

## When Goal is Abandoned

```
memory_store(content="⚠️ ANTIPATTERN: [what didn't work]. Reason: [why abandoned]", memory_type="procedural")
memory_correct(query="GOAL: [name]", new_content="🎯 GOAL: [name] — ❌ ABANDONED\nReason: [why]", reason="abandoned")
```

## Rules

- **Search before acting**: check past failures, corrections, antipatterns
- **User corrections override all**
- **Be specific**: "pytest fixtures don't work with async DB" > "tests failed"
- **Don't create goals for quick fixes** (< 3 tasks, single session)
- **Emoji prefixes**: 🎯 goal, ✅❌ steps, 🔄 retro, 💡 lesson, 🔧 correction, ⚠️ antipattern
- **Type discipline**: GOAL/RETRO/LESSON/CORRECTION → `procedural`; STEP logs → `working`
