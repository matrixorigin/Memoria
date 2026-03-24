# Role-Scoped Memory Spaces with Inheritance

## Summary

This document proposes **role-scoped memory spaces** as a first-class collaboration primitive in Memoria.

Example:

```text
create role "writer" based on main
```

The goal is to let clients share a common base memory space while keeping role-specific procedural rules, heuristics, and working preferences separate.

## Motivation

Multi-agent systems often need two things at the same time:

1. **Shared user/base memory**
   - preferences
   - stable facts
   - global boundaries
   - long-term operating context

2. **Role-specific overlays**
   - writer rules
   - reviewer rules
   - github-ops rules
   - planner/executor differences

Today, clients can approximate this by:

- encoding role labels in raw content
- splitting into separate user IDs
- overloading branch/snapshot semantics

All three are awkward.

## Why role scope is not the same as branch/snapshot

Memoria already has strong Git-like ideas such as snapshots, rollback, branch, merge, and diff.

Those are best suited for:

- experimentation
- recovery from mistakes
- version control
- rollback
- comparing alternate evolution paths

By contrast, **role scope** is better suited for:

- collaboration
- stable inherited overlays
- role separation without cloning the full user memory base
- retrieving a shared base plus role-specific deltas

A useful distinction is:

- **branch/snapshot answers**: _which version?_
- **role answers**: _which collaborator / viewpoint?_

## Proposed model

### Base scope

Every user has a shared base scope:

- `main`

### Role scopes

Clients can create named role scopes that inherit from `main`:

- `writer`
- `reviewer`
- `github-ops`
- `tech-analyst`

A recall against `writer` would effectively resolve as:

- `main` + `writer`

where:

- `main` provides shared/global memory
- `writer` provides role-local delta memory

## Desired capabilities

### Scope lifecycle

- create role based on a parent scope
- list available role scopes
- inspect role parent
- delete role scope

### Memory operations

- store memory into a specific scope
- recall from `main` only
- recall from `main + role`
- optionally recall from role-only or parent-only

### Introspection

- stats per scope
- diff role vs parent
- snapshot / rollback an individual role scope

## Example CLI ideas

```bash
memoria role create writer --based-on main
memoria role list
memoria role show writer
memoria role delete writer

memoria store --scope writer --type procedural "Prefer investor-ready writing"
memoria search "deck narrative" --scope writer
memoria stats --scope writer
memoria diff --scope writer --against main
```

## Example API ideas

### Create role

```http
POST /v1/roles
{
  "name": "writer",
  "based_on": "main"
}
```

### List roles

```http
GET /v1/roles
```

### Scoped store

```http
POST /v1/memories
{
  "scope": "writer",
  "memory_type": "procedural",
  "content": "Prefer investor-ready writing"
}
```

### Scoped recall

```http
POST /v1/memories/search
{
  "query": "deck narrative",
  "scope": "writer"
}
```

Server-side semantics:

- include `main`
- include `writer`
- rank across both

## Data model direction

A generic scope abstraction is likely more future-proof than a hard-coded `role` field.

### Option A: scope fields

Possible fields:

- `scope_kind`
- `scope_value`
- `parent_scope_id`

This can support more than roles later:

- role
- project
- session
- workspace overlays

### Option B: named memory spaces

Alternatively, Memoria could expose a more direct abstraction:

- memory spaces
- inherited spaces
- role is a special case of a named inherited space

## Suggested retrieval semantics

For a scoped recall against `writer`:

1. load base scope `main`
2. load child scope `writer`
3. combine both result sets
4. rank with a preference for role-local memories when scores are close

This keeps role-local guidance strong without losing the shared base.

## Suggested governance semantics

Role scopes should remain compatible with existing Memoria governance ideas:

- snapshots
- rollback
- diff
- merge

That means a role can be both:

- collaboration-oriented (through inheritance)
- versioned (through branch/snapshot)

## Non-goals

This proposal does **not** replace:

- snapshots
- rollback
- branches
- diff/merge

Those remain the right abstraction for version control and recovery.

The goal here is to introduce an additional abstraction for collaboration-oriented inherited memory.

## Why this matters

This would make Memoria more useful for real multi-agent coordination where:

- a shared memory base is needed
- specialized roles need their own stable local rules
- clients should not have to encode scope in raw text
- role separation should not require duplicating the whole user memory graph
