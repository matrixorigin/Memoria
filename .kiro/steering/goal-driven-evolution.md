---
inclusion: always
---

# Goal-Driven Iterative Evolution via Memory

Use the memory system as a persistent feedback loop to track goals, plans, progress, and lessons — enabling continuous improvement across conversations.

## 1. Goal Registration

When user sets a goal or objective, immediately store it:

```
memory_store(
  content="🎯 GOAL: [goal description]\nSuccess Criteria: [measurable criteria]\nStatus: ACTIVE\nCreated: [timestamp]",
  memory_type="procedural"
)
```

Then call `memory_retrieve("GOAL")` to check for existing related goals — avoid duplicates, link related goals.

For complex goals, decompose into sub-goals with dependencies:
```
memory_store(
  content="🎯 SUB-GOAL: [sub-goal] (parent: [parent goal])\nDepends on: [other sub-goals if any]\nSuccess Criteria: [...]\nStatus: ACTIVE",
  memory_type="procedural"
)
```

## 2. Plan Creation

Before executing, create a plan and store it linked to the goal:

```
memory_store(
  content="📋 PLAN for GOAL [goal name]\nApproach: [strategy]\nSteps:\n1. [step] — ⏳\n2. [step] — ⏳\n...\nRisks: [known risks]\nIteration: #1",
  memory_type="procedural"
)
```

For high-risk iterations, create a memory branch first:
```
memory_branch(name="goal_[name]_iter_[N]")
memory_checkout(name="goal_[name]_iter_[N]")
# Execute on branch — main is untouched until validated
```

## 3. Step Execution Tracking

After each step completes or fails, log it:

**On success:**
```
memory_store(
  content="✅ STEP [N/total] for GOAL [name] (Iteration #X)\nAction: [what was done]\nResult: [outcome]\nInsight: [what we learned]",
  memory_type="working"
)
```

**On failure:**
```
memory_store(
  content="❌ STEP [N/total] for GOAL [name] (Iteration #X)\nAction: [what was attempted]\nError: [what went wrong]\nRoot Cause: [analysis]\nNext: [adjusted approach]",
  memory_type="working"
)
```

## 4. Iteration Review & Evolution

When a plan iteration completes (all steps done, or blocked), do a review:

1. `memory_search("STEP for GOAL [name] Iteration #X")` — gather all step logs
2. Synthesize a retrospective:

```
memory_store(
  content="🔄 RETRO for GOAL [name] Iteration #X\nCompleted: [M/N steps]\nWhat worked: [...]\nWhat failed: [...]\nKey insight: [...]\nNext iteration should: [concrete improvements]",
  memory_type="procedural"
)
```

3. Update the goal status:
```
memory_correct(
  query="GOAL: [name]",
  new_content="🎯 GOAL: [name]\nSuccess Criteria: [criteria]\nStatus: ITERATION #X COMPLETE — [progress %]\nLessons: [accumulated lessons]\nNext: [what to try next]",
  reason="Iteration #X completed, updating progress"
)
```

4. If on a branch, validate then merge:
```
memory_diff(source="goal_[name]_iter_[N]")        # preview changes (LCA-based diff)
memory_checkout(name="main")
memory_merge(source="goal_[name]_iter_[N]", strategy="append")  # merge validated results
memory_branch_delete(name="goal_[name]_iter_[N]")
```

## 5. New Conversation Bootstrap

At the START of every conversation, proactively check for active goals:

1. `memory_search("GOAL ACTIVE")` — find all active goals
2. For the most relevant goal: `memory_search("RETRO for GOAL [name]")` — get latest retrospective
3. Summarize current state to user: "You have active goals: ... Here's where we left off on [goal]..."
4. Propose the next iteration plan incorporating all accumulated lessons

## 6. Goal Completion

When success criteria are met:

```
memory_correct(
  query="GOAL: [name]",
  new_content="🎯 GOAL: [name] — ✅ ACHIEVED\nIterations: [N]\nFinal approach: [what worked]\nKey lessons: [reusable insights]",
  reason="Goal achieved"
)
```

Store reusable lessons as permanent knowledge:
```
memory_store(
  content="💡 LESSON from [goal name]: [reusable insight that applies to future work]",
  memory_type="procedural"
)
```

Clean up step logs (working memories accumulate fast):
```
memory_purge(topic="STEP for GOAL [name]", reason="Goal achieved, steps archived in RETRO")
```

## 7. User Feedback & Corrections

User feedback is the highest-value signal for evolution. Capture it immediately.

**When user corrects your approach:**
```
memory_store(
  content="🔧 CORRECTION for GOAL [name]: User said [what user corrected]. Was doing: [old approach]. Should do: [corrected approach]. Why: [user's reasoning if given]",
  memory_type="procedural"
)
```

**When user gives positive feedback:**
```
memory_store(
  content="👍 FEEDBACK for GOAL [name]: User confirmed [what worked well]. Context: [when/why it worked]. Reuse: [when to apply this again]",
  memory_type="procedural"
)
```

**When user changes direction or priorities:**
```
memory_correct(
  query="GOAL: [name]",
  new_content="🎯 GOAL: [name]\n...\nPivot: User redirected from [old direction] to [new direction]. Reason: [why]",
  reason="User changed direction"
)
```

**When user expresses frustration or dissatisfaction:**
```
memory_store(
  content="⚠️ ANTIPATTERN for GOAL [name]: [what I did that user didn't like]. User expected: [what they wanted instead]. Rule: NEVER do [this] again for this user.",
  memory_type="procedural"
)
```

Before proposing any approach, always `memory_search("CORRECTION ANTIPATTERN [goal name]")` to avoid repeating mistakes the user already corrected.

## Behavior Rules

- **Always check memory first**: Before starting work on any goal, retrieve existing progress
- **Never repeat failed approaches**: Search for previous failure logs before proposing a plan
- **Never repeat corrected mistakes**: Search for CORRECTION and ANTIPATTERN logs before acting
- **User corrections override all**: If user corrects something, that correction has highest priority in all future iterations
- **Accumulate, don't replace**: Each iteration builds on previous lessons, never discard history
- **Be specific in insights**: "Tests failed" is useless; "pytest fixtures don't work with async DB connections, use factory pattern instead" is valuable
- **Tag everything**: Use emoji prefixes (🎯📋✅❌🔄💡🔧👍⚠️) for easy retrieval and scanning
