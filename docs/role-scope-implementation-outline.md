# Role / Scope Inheritance — Implementation Outline

## Goal

Add inherited role-scoped memory to Memoria without overloading raw memory content tags.

User-facing language should prefer **role**.
Internal storage can continue to use a more general **scope** abstraction.

Example:

```text
create role "writer" based on main
create role "bp-writer" based on writer
```

## Product model

- `main` is the default shared base memory space.
- A `role` is an inherited overlay on top of another role/scope.
- Recall against a role should include its parent chain.
- Branch/snapshot remain the mechanism for rollback and experimentation.
- Role/scope is the mechanism for collaboration and shared inheritance.

## Scope of the first implementation

Keep the first implementation intentionally small.

### In scope

1. Create role with optional parent
2. List/show roles
3. Store memory into a role
4. Recall/search within a role, including inherited parents
5. List/stats within a role
6. Migrate existing memories into a default `main`

### Out of scope for the first pass

- complex ACL / permissions
- UI-heavy diff tooling
- multi-parent inheritance
- branch + role deep integration
- advanced precedence policies beyond simple inherited recall

## Data model

### New table: `mem_scopes`

Suggested fields:

- `scope_id`
- `user_id`
- `name`
- `parent_scope_id` (nullable)
- `created_at`
- `updated_at`

Constraints:

- unique `(user_id, name)`
- parent must belong to same user

### Update `mem_memories`

Add:

- `scope_id`

Migration behavior:

- create a default `main` scope for each existing user
- backfill all existing memories to that user's `main`

## Retrieval semantics

If the active role is `bp-writer` and the lineage is:

```text
bp-writer -> writer -> main
```

then recall/search should retrieve across:

- `bp-writer`
- `writer`
- `main`

The first implementation can simply union candidates across the lineage and rank them with existing retrieval logic.

## API outline

### Role lifecycle

- `POST /v1/roles`
- `GET /v1/roles`
- `GET /v1/roles/:name`
- `DELETE /v1/roles/:name`

### Memory operations

Extend existing requests with optional role/scope target:

- `POST /v1/memories` with `scope`
- `POST /v1/memories/search` with `scope`
- `POST /v1/memories/retrieve` with `scope`
- `GET /v1/memories` with optional `scope`
- `GET /v1/memories/stats` with optional `scope`

## CLI outline

Prefer user-facing `role` terminology.

```bash
memoria role create writer --based-on main
memoria role create bp-writer --based-on writer
memoria role list
memoria role show writer
memoria role delete writer

memoria store --scope writer --type procedural "Prefer investor-ready writing"
memoria search "deck narrative" --scope bp-writer
memoria list --scope writer
memoria stats --scope writer
```

## Internal layering

### User-facing

- role

### Internal

- scope
- parent scope lineage
- inherited recall chain

This preserves future extensibility for non-role scopes while keeping the CLI natural.

## Suggested implementation order

1. Storage schema + migration
2. Core interfaces / models
3. Service lineage resolution + inherited recall
4. API support
5. CLI support
6. Tests for create/store/search/list/stats against inherited roles

## Suggested first tests

1. create `writer` based on `main`
2. create `bp-writer` based on `writer`
3. store one memory in `main`
4. store one memory in `writer`
5. store one memory in `bp-writer`
6. search `bp-writer` and verify all three scopes participate
7. stats `writer` and verify writer-local vs inherited behavior is documented or explicit

## Why this is worth doing

This would let Memoria support multi-agent collaboration patterns such as:

- writer / reviewer
- planner / executor
- github-ops / coding-agent

without forcing clients to:

- encode role tags in content
- duplicate shared memories across separate users
- misuse branches as steady-state collaboration scopes
