# RIG-98: Disable orchestrator + watchdog + fix Linear push type error

**Status:** Ready for implementation
**Priority:** High
**Estimate:** 2 SP
**Author:** Analyst agent
**Date:** 2026-03-10

## Summary

Three daemon hygiene fixes in one chore: disable the orchestrator (dead weight), disable the watchdog (harmful to long-running agents), and fix the Linear `commentCreate` GraphQL type mismatch.

## Related Issues

| Issue | Relationship | Notes |
|-------|-------------|-------|
| RIG-43 | Related | Same daemon constants (orchestrator interval, stuck timeout) — "make configurable" vs "disable" |
| RIG-44 | Related | Same `linear.rs` file — error swallowing fix. Potential diff conflicts if parallel |
| RIG-86 | Related | daemon.rs structural refactor — same file, different scope |
| RIG-109 | Blocked by RIG-98 | JSONL activity detection — future smart watchdog replacement for what RIG-98 disables |
| RIG-112 | Related (companion) | Reviewer task hang — explains urgency of watchdog disable; requests smarter replacement |

No duplicates found.

## Current State (Code Analysis)

### File: `engine/src/daemon.rs`

**Tick loop (lines 53-106)** runs every 5 seconds:
1. `check_schedules()` — fires cron-triggered tasks
2. **`check_stuck_tasks()`** — kills tasks running > 30 min ← DISABLE THIS
3. `process_completed_pipeline_tasks()` — Linear push / pipeline callback
4. `drain_queue()` — launches pending tasks (max 3 concurrent)
5. `rotate_logs()`
6. Every 30s: `pipeline::poll()`
7. Every 60s: `check_merged_prs()`
8. **Every 900s: `run_orchestrator()`** ← DISABLE THIS

**`run_orchestrator()` (lines 509-567):**
- Enqueues orchestrator agent task with priority=3 (lowest)
- Task never actually runs — starved by higher-priority work
- Even if it ran: just reads its own character.md and writes memory — no operational value yet

**`check_stuck_tasks()` (lines 225-272):**
- Kills any task running > `DEFAULT_STUCK_TIMEOUT_MINS` (30 min)
- Reads `~/.werma/limits.json["timeout_minutes"]` for override
- Only uses global timeout — per-agent overrides in `limits.json` are NOT consumed
- Problem: complex agent tasks (engineer, pipeline) routinely take 30-60+ min → healthy agents get killed

### DB Schedule: `werma-orchestrator`
- Cron: `*/30 * * * *` (every 30 min)
- Status: enabled=1
- SEPARATE mechanism from daemon's hardcoded 15-min `run_orchestrator()` → double enqueue

### File: `engine/src/linear.rs`

**`comment()` mutation (line 432):**
```graphql
mutation($issueId: ID!, $body: String!) {
    commentCreate(input: { issueId: $issueId, body: $body }) { success }
}
```

**Bug:** Linear's GraphQL schema types `CommentCreateInput.issueId` as `String`, not `ID`. The variable declaration `$issueId: ID!` doesn't match, causing:
```
Variable "$issueId" of type "ID!" used in position expecting type "String"
```

The other mutations (`move_issue`, `update_estimate`) use `$id: ID!` for `issueUpdate(id: ...)` which IS correct — `IssueUpdateInput.id` accepts `ID`.

## Implementation Plan

### Step 1: Disable orchestrator tick in daemon.rs

**File:** `engine/src/daemon.rs`

Comment out the `run_orchestrator()` call in the tick loop (around the 900s interval block). Add a `// TODO: re-enable when orchestrator has real work (see RIG-43, RIG-109)` comment.

Do NOT delete the `run_orchestrator()` function itself — keep it for future use.

### Step 2: Disable `werma-orchestrator` cron schedule

Run SQL against `~/.werma/werma.db`:
```sql
UPDATE schedules SET enabled = 0 WHERE id = 'werma-orchestrator';
```

Alternatively, add this to the daemon startup or expose via `werma sched off werma-orchestrator`. The `werma sched off` CLI command already exists — this can be done as a one-time manual step documented in the PR.

**Recommendation:** Do both — disable in DB via migration/startup code AND document the manual `werma sched off` step.

### Step 3: Disable watchdog in daemon.rs

**File:** `engine/src/daemon.rs`

Comment out the `check_stuck_tasks()` call in the tick loop (line ~62). Add a `// TODO: re-enable with JSONL-based activity detection (see RIG-109, RIG-112)` comment.

Do NOT delete the `check_stuck_tasks()` function — it's the basis for the future smart watchdog.

### Step 4: Fix Linear `commentCreate` GraphQL type

**File:** `engine/src/linear.rs`, line ~432

Change:
```graphql
mutation($issueId: ID!, $body: String!) {
```
To:
```graphql
mutation($issueId: String!, $body: String!) {
```

This is the only mutation affected — `move_issue()` and `update_estimate()` use `issueUpdate(id: ID!)` which is correct per Linear's schema.

### Step 5: Verify

1. `cargo build` — compiles clean
2. `cargo test` — all tests pass
3. Start daemon, observe logs for 2-3 minutes:
   - No orchestrator task enqueued
   - No stuck-task kills
   - No Linear GraphQL errors
4. Test `werma linear push <task-id>` with a completed task — comment should appear in Linear

## Acceptance Criteria

- [ ] Orchestrator tick in daemon.rs disabled (commented out with TODO)
- [ ] Cron schedule `werma-orchestrator` disabled (`enabled=0` in DB)
- [ ] Watchdog (`check_stuck_tasks`) disabled in daemon tick loop (commented out with TODO)
- [ ] Linear `commentCreate` mutation: `$issueId` type changed from `ID!` to `String!`
- [ ] `cargo build` + `cargo test` pass
- [ ] Daemon runs clean: no orchestrator spam, no watchdog kills, no Linear errors in logs

## Risks

| Risk | Impact | Mitigation |
|------|--------|-----------|
| No stuck task detection at all | Zombie agents consume resources indefinitely | RIG-112 P0 fix (startup timeout + zombie detection) should follow soon. Manual `werma kill` available |
| Stale orchestrator code bitrot | Function becomes outdated while disabled | Keep function body, tracked by RIG-43/RIG-109 for re-enablement |
| Other Linear mutations may have type issues | Push failures on other endpoints | Verified: `move_issue` and `update_estimate` use correct `ID!` type. Only `comment` is affected |
| `watchdog` schedule still running | DB schedule `watchdog` (*/30, haiku) is separate from `check_stuck_tasks()` and may still enqueue watchdog tasks | Out of scope for RIG-98 — watchdog schedule is a different mechanism (agent-based, not daemon code). Review in RIG-112 |

## Out of Scope

- Smart watchdog replacement (→ RIG-109, RIG-112)
- Daemon config file for tuning constants (→ RIG-43)
- daemon.rs structural refactor (→ RIG-86)
- Linear error swallowing fix (→ RIG-44)
- Pipeline auto-polling integration
