---
inclusion: agent_requested
---

<!-- memoria-version: 0.1.0-->

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
