# API Key Authentication — Change Notes

## Overview

This change introduces API key authentication to the Memoria API, allowing regular users to access
the API using `sk-...` format keys, while establishing clear permission isolation between master key
and API key callers.

**7 files changed, ~150 lines net.**

---

## Changes

### 1. `AuthUser` struct refactor (`auth.rs`)

**Before:** `AuthUser(pub String)` — carried only `user_id`, no distinction between auth methods.

**After:**

```rust
pub struct AuthUser {
    pub user_id: String,
    pub is_master: bool,  // true = authenticated via master key or open dev mode
}
```

Added `require_master()` method. Admin and key-management routes call this at the top of each
handler to enforce master-only access.

### 2. Bearer parsing decoupled from `master_key` presence (`auth.rs`)

**Before:** Bearer token was only parsed when `master_key` was configured. API keys could not work
in environments without a master key.

**After:** Bearer token is checked first, independent of whether `master_key` is set:

```
Has Bearer token?
  ├─ yes → check master key (if configured) → check API key in DB → neither matches → 401
  └─ no  → master_key configured?
             ├─ yes → 401 "Missing Bearer token"
             └─ no  → X-User-Id fallback (open dev mode, backwards compatible)
```

### 3. API key validation function (`auth.rs`)

New `validate_api_key()` function:
- SHA-256 hashes the raw token, queries `mem_api_keys`
- Checks `is_active = 1` and `expires_at` (`NULL` = never expires, non-`NULL` = must not be past)
- Updates `last_used_at` asynchronously after successful auth (fire-and-forget, non-blocking)
- DB query failures are logged via `tracing::warn!` instead of silently returning `None`

### 4. Memory ownership check (`routes/memory.rs`)

**Before:** `get_memory` and `correct_memory` discarded `user_id` entirely (`AuthUser { .. }`),
allowing API key users to read or modify any user's memory by guessing the memory ID.

**After:** Non-master callers are checked against the memory's `user_id`; mismatches return 403.
Master key callers retain full cross-user access.

```rust
// get_memory: check ownership before returning
if !is_master && mem.user_id != user_id {
    return Err((StatusCode::FORBIDDEN, "Not your memory".to_string()));
}

// correct_memory: fetch + check ownership before mutating
if !is_master {
    let existing = state.service.get(&id).await...?;
    if existing.user_id != user_id {
        return Err((StatusCode::FORBIDDEN, "Not your memory".to_string()));
    }
}
```

### 5. Admin route authorization (`routes/admin.rs`)

All 10 `/admin/*` handlers now call `auth.require_master()?` at entry:

| Route | Handler |
|---|---|
| `GET /admin/stats` | `system_stats` |
| `GET /admin/users` | `list_users` |
| `GET /admin/users/:id/stats` | `user_stats` |
| `DELETE /admin/users/:id` | `delete_user` |
| `POST /admin/users/:id/reset-access-counts` | `reset_access_counts` |
| `POST /admin/governance/:id/trigger` | `trigger_governance` |
| `POST /admin/users/:id/strategy` | `set_user_strategy` |
| `GET /admin/users/:id/keys` | `list_user_keys` |
| `DELETE /admin/users/:id/keys` | `revoke_all_user_keys` |
| `POST /admin/users/:id/params` | `set_user_params` |

`/v1/health/*` routes do not require master; only struct destructuring was updated.

### 6. Key management route permission fixes (`routes/auth.rs`)

| Route | Change |
|---|---|
| `POST /auth/keys` (create) | Added `require_master()?` — only master can create keys |
| `GET /auth/keys/:id` (get) | Ownership check changed from hardcoded `user_id != "admin"` to `!is_master` |
| `PUT /auth/keys/:id/rotate` | Guard changed from `!state.master_key.is_empty()` to `!is_master` |
| `DELETE /auth/keys/:id` (revoke) | Same fix as rotate |

**Bug fixed:** `rotate_key` and `revoke_key` previously used `!state.master_key.is_empty()` to
decide whether to allow cross-user operations. This only checked whether a master key was
*configured*, not whether the *current request* was authenticated with it. Any valid API key could
rotate or revoke other users' keys.

### 7. Open-mode startup warning (`state.rs`)

`AppState::new` now emits a `tracing::warn!` when `master_key` is empty, so open-mode deployments
are visible in logs.

### 8. Mechanical route adaptations

`AuthUser(user_id)` → `AuthUser { user_id, .. }` and `AuthUser(_)` → `AuthUser { .. }` across
`memory.rs`, `governance.rs`, `sessions.rs`, `snapshots.rs`. Behaviour unchanged.

---

## Auth matrix

### With `master_key` configured (production)

| Request | `user_id` source | `is_master` | `/admin/*` | `/v1/*` |
|---|---|---|---|---|
| `Bearer <master_key>` + `X-User-Id: alice` | `X-User-Id` header | `true` | ✓ | ✓ |
| `Bearer sk-xxxx` (valid, not expired) | DB key owner | `false` | ✗ 403 | ✓ |
| `Bearer sk-xxxx` (expired) | — | — | ✗ 401 | ✗ 401 |
| `Bearer wrong-token` | — | — | ✗ 401 | ✗ 401 |
| No `Authorization` header | — | — | ✗ 401 | ✗ 401 |

### Without `master_key` (dev / test)

| Request | `user_id` source | `is_master` | `/admin/*` | `/v1/*` |
|---|---|---|---|---|
| `Bearer sk-xxxx` (valid) | DB key owner | `false` | ✗ 403 | ✓ |
| No `Authorization` + `X-User-Id: alice` | `X-User-Id` header | `true` | ✓ | ✓ |
| No `Authorization`, no `X-User-Id` | `"default"` | `true` | ✓ | ✓ |

A startup `WARN` log is emitted when running without a master key.

---

## TODO

### Anonymous requests receive `is_master: true` in open dev mode

**Status:** Known, intentional for now.

**Current behaviour:** When `master_key` is not configured and no Bearer token is present, the
request falls through to the `X-User-Id` / query-param / `"default"` path and is tagged
`is_master: true`. This means anonymous callers can reach `/admin/*` and `POST /auth/keys` in
open mode.

**Why it exists:** This has been the project's behaviour from the start — no `master_key` means
"dev mode, everything open". Dozens of test cases rely on it (`spawn_server()` with no master key
hitting `/admin/*` directly).

**Risk:** If a production deployment forgets to set `master_key`, all admin endpoints are
unauthenticated. The startup `WARN` log added in this change is the current mitigation.

**Options for a future fix:**

1. **`AuthPrincipal` enum (recommended):** Replace `is_master: bool` with an explicit enum
   `Master | ApiKey | LegacyOpen`. `require_master()` only passes `Master`, so anonymous
   open-mode requests are rejected from admin routes. Requires updating affected tests.

2. **Env-var gate:** Introduce `ALLOW_OPEN_MODE=true`; refuse to start (or reject admin routes)
   when `master_key` is empty and the var is not set.

3. **CI enforcement:** Keep the current behaviour in code; enforce that `master_key` is always
   set in production via deployment configuration or CI checks.
