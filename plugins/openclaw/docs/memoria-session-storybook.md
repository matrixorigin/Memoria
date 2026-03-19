# Memoria Session Storybook

Use this manual storybook to run a real OpenClaw session and verify that Memoria is doing the work, not `MEMORY.md`.

This is a human-driven test plan. It is intentionally repetitive and uses high-entropy tokens so you can tell the difference between:

- model guessing
- local file memory
- real Memoria recall

## Before you start

Open two terminals.

Terminal A:
- run `openclaw`
- do the interactive chat there

Terminal B:
- run the CLI checks there

Start from your home directory, not from a plugin checkout:

```bash
cd ~
```

Preflight checks:

```bash
openclaw memoria capabilities
openclaw memoria stats
openclaw ltm list --limit 10
openclaw config get 'tools.allow'
ls ~/.openclaw/skills
```

Expected:

- `memory_store` appears in `tools.allow`
- `memoria-memory` and `memoria-recovery` exist under `~/.openclaw/skills`
- `openclaw memoria capabilities` shows the full tool surface

If the assistant still says it only has `memory_search` and `memory_get`, stop here and fix install/tool policy first.

## Test data

Use these exact values:

- user preference: `I like basketball.`
- travel city: `Hangzhou`
- project codename: `Atlas Delta`
- rollback token: `atlas-rb-91c4`
- A/B token: `nebula-ans-7f2a`
- snapshot name: `storybook-before-delete`

## Phase 1: Explicit durable store

In Terminal A, start a new OpenClaw session and paste this:

```text
Use Memoria, not MEMORY.md.
Store these as durable memory with the appropriate Memoria tools, then verify with memory_recall:
1. I like basketball.
2. My travel city is Hangzhou.
3. The project codename is Atlas Delta.
4. The rollback token is atlas-rb-91c4.
After storing, tell me which Memoria tools you used.
```

Expected:

- the assistant explicitly mentions `memory_store`, `memory_profile`, or `memory_recall`
- it does not say it only has read-only Memoria access
- it does not fall back to `MEMORY.md`

In Terminal B, verify:

```bash
openclaw ltm search "atlas-rb-91c4" --limit 5
openclaw ltm search "Atlas Delta" --limit 5
openclaw ltm search "basketball" --limit 5
openclaw memoria stats
```

Expected:

- all three searches return active memories
- `activeMemoryCount` is higher than before

## Phase 2: Same-session recall

Still in Terminal A, ask:

```text
What is the rollback token? If you do not know, reply exactly UNKNOWN.
```

Expected:

- exact answer contains `atlas-rb-91c4`

Then ask:

```text
What sport do I like?
```

Expected:

- answer contains `basketball`

## Phase 3: New-session recall

Fully exit OpenClaw. Start it again. Open a fresh session.

Ask:

```text
Use Memoria. What is the project codename and what is the rollback token? If unknown, say UNKNOWN.
```

Expected:

- answer contains `Atlas Delta`
- answer contains `atlas-rb-91c4`

This is the simplest proof that the memory is not just current-context retention.

## Phase 4: Correct an existing memory

In Terminal A:

```text
Use Memoria to correct this stored fact: my travel city is Shanghai, not Hangzhou.
After correcting it, verify with memory_recall.
```

Expected:

- assistant uses `memory_correct` or another explicit Memoria repair path
- assistant confirms the corrected value

In Terminal B:

```bash
openclaw ltm search "Hangzhou" --limit 10
openclaw ltm search "Shanghai" --limit 10
```

Expected:

- `Shanghai` should be retrievable
- the stale `Hangzhou` fact should no longer be the active answer path

Then in Terminal A ask:

```text
What is my travel city?
```

Expected:

- answer is `Shanghai`

## Phase 5: Snapshot, delete, rollback

In Terminal A:

```text
Use Memoria to create a snapshot named storybook-before-delete.
Then forget the rollback token atlas-rb-91c4.
Then verify whether the rollback token is still retrievable.
```

Expected:

- assistant creates a snapshot
- assistant deletes or purges the token memory
- assistant reports the token is no longer retrievable

In Terminal B:

```bash
openclaw ltm search "atlas-rb-91c4" --limit 5
openclaw memoria stats
```

Expected:

- search should now be empty or no longer show an active hit for the token

Now in Terminal A:

```text
Use Memoria rollback to restore snapshot storybook-before-delete.
Then verify the rollback token again.
```

Expected:

- assistant uses `memory_rollback`
- assistant verifies that `atlas-rb-91c4` is back

In Terminal B:

```bash
openclaw ltm search "atlas-rb-91c4" --limit 5
```

Expected:

- search returns the token again

## Phase 6: With-memory vs no-memory A/B

This phase proves the answer changes because of Memoria, not because the model happened to know something.

First ask in Terminal A:

```text
What is the secret answer token for Project Nebula? If you do not know, reply exactly UNKNOWN.
```

Expected:

- answer should be `UNKNOWN`

Now ask:

```text
Use Memoria to store this exact fact: The secret answer token for Project Nebula is nebula-ans-7f2a.
Then verify with memory_recall.
```

Then ask again:

```text
What is the secret answer token for Project Nebula? If you do not know, reply exactly UNKNOWN.
```

Expected:

- answer should now contain `nebula-ans-7f2a`

That is your cleanest manual A/B check.

## Phase 7: Long-session drift check

Stay in the same session and talk about unrelated things for 10 to 20 turns:

- ask for a travel plan
- ask for a code review checklist
- ask for a deployment strategy
- ask for a database migration outline

Then return to:

```text
Before answering, use Memoria.
Summarize what you know about me and this project in 4 bullets.
```

Expected:

- answer should still recover the durable facts
- it should mention basketball, Shanghai, Atlas Delta, and the restored token if still present

## Direct CLI checks

Use these at any point:

```bash
openclaw memoria capabilities
openclaw memoria stats
openclaw ltm list --limit 20
openclaw ltm search "basketball" --limit 10
openclaw ltm search "Atlas Delta" --limit 10
openclaw ltm search "atlas-rb-91c4" --limit 10
```

## Pass criteria

You can consider Memoria working if all of these are true:

1. The assistant explicitly uses Memoria write tools instead of claiming read-only access.
2. A fresh session can recall `Atlas Delta` and `atlas-rb-91c4`.
3. Correcting `Hangzhou` to `Shanghai` changes the later answer.
4. Deleting the rollback token makes it disappear.
5. Rolling back `storybook-before-delete` restores it.
6. Before storing `nebula-ans-7f2a`, the answer is `UNKNOWN`; after storing, the answer contains the token.

## Failure signatures

- Assistant says it only has `memory_search` and `memory_get`
  - `tools.allow` is still missing or stale
- Assistant says it saved to `MEMORY.md`
  - prompt guidance or companion skills are not active yet
- `openclaw memoria stats` fails with MatrixOne connection errors
  - local MatrixOne is not running, or `dbUrl` is wrong
- `openclaw ltm search` returns nothing after a claimed store
  - the assistant did not actually use Memoria write tools

## Optional reset between runs

If you want to start over with a clean database state for this storybook, use a fresh MatrixOne database name in `MEMORIA_DB_URL`, for example:

```text
mysql+pymysql://root:111@127.0.0.1:6001/memoria_storybook_01
```

That is usually cleaner than trying to purge every old memory by hand.
