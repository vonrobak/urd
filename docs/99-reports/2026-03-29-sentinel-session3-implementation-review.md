# Sentinel Session 3 Implementation Review

**Project:** Urd — BTRFS Time Machine for Linux
**Date:** 2026-03-29
**Scope:** Sentinel Session 3 — notification deduplication, drive connection recording, edge case hardening
**Base commit:** 7de2c32 (unstaged diff)
**Test count:** 444 (8 new), clippy clean
**Files reviewed:** `src/commands/backup.rs`, `src/commands/sentinel.rs`, `src/output.rs`, `src/sentinel_runner.rs`, `src/state.rs`

---

## 1. Executive Summary

Solid session. The notification deduplication is correctly fail-open, the drive connection
recording follows established patterns, and the `SentinelStateFile::read()` move resolves
a real ADR-108 violation. One significant finding: when backup defers notification dispatch
to the sentinel, the heartbeat's `notifications_dispatched` flag is left in an ambiguous
state that could cause the sentinel to skip notifications it was supposed to handle.

## 2. What Kills You

**Catastrophic failure mode:** Silent data loss — deleting snapshots that shouldn't be deleted.

**Proximity:** This session is **far from the catastrophic failure mode**. Notification
deduplication, drive event recording, and log improvements are all observability-layer
changes. No code path in this diff touches retention, pin protection, or btrfs operations.
The worst outcome is missed or duplicate notifications — annoying, not destructive.

**Secondary failure mode for this session:** Missed notifications when the sentinel is
supposed to handle them but doesn't. This is the scenario to scrutinize.

## 3. Scorecard

| # | Dimension | Score | Rationale |
|---|-----------|-------|-----------|
| 1 | Correctness | 3 | One real gap in the dedup handoff (S1). Logic otherwise sound. |
| 2 | Security | 5 | No new privilege boundaries. PID check is /proc, not kill(). |
| 3 | Architectural Excellence | 4 | Clean module boundaries. `sentinel_is_running` correctly placed. |
| 4 | Systems Design | 3 | Heartbeat dispatched-flag ambiguity needs resolution (S1). |
| 5 | Rust Idioms | 4 | Enums for event types, `#[must_use]`, proper error handling. |
| 6 | Code Quality | 4 | Consistent with existing patterns. Tests cover the right things. |

## 4. Design Tensions

### Tension 1: Heartbeat-based vs. assessment-based notification paths

The backup command computes notifications from heartbeat transitions
(`notify::compute_notifications`). The sentinel computes notifications from assessment
diffs (`build_notifications`). These are two independent implementations of "did promises
change?" that must agree. The deduplication design says "when sentinel is running, let
sentinel handle it" — but the sentinel's notification path is fundamentally different from
backup's.

**Resolution:** Correct. The sentinel's assessment-based path is more authoritative (it
runs `awareness::assess()` directly). When backup defers, the sentinel picks up the
heartbeat mtime change via `detect_heartbeat_event()`, triggers `BackupCompleted`,
which triggers `Assess`, which runs the full assessment pipeline. The sentinel doesn't
use the heartbeat's `notifications_dispatched` flag at all for its assessment-based
notifications — it uses its own `last_promise_states` diff. This is the right design:
the sentinel is the authority when it's running.

### Tension 2: Fail-open dedup vs. notification reliability

`sentinel_is_running` is fail-open: any I/O error returns false, causing backup to
dispatch normally. This means: worst case on detection failure = duplicate notifications.
Worst case on false positive (sentinel just died) = missed notifications for one run.

**Resolution:** Correct trade-off. Duplicates are annoying; missed notifications self-heal
on the next backup run (sentinel is detected as dead). The one-run window is acceptable
for a system with nightly backups and 2-15 minute assessment ticks.

### Tension 3: SQLite open per drive event vs. persistent connection

The runner opens a fresh SQLite connection for each drive mount/unmount event, while
`execute_assess()` opens its own connection per assessment tick. This means the runner
never holds a persistent connection.

**Resolution:** Correct for a daemon. Drive events are rare (a few per day). Assessment
ticks are every 2-15 minutes. The short-lived connection pattern avoids WAL checkpoint
accumulation and stale-handle concerns that plague long-running daemons with persistent
SQLite connections. The cost (~1ms per open) is invisible against the 5-second poll
interval.

## 5. Findings

### S1. Heartbeat `notifications_dispatched` flag left `false` when backup defers to sentinel

**Severity: Significant**

When `sentinel_is_running()` returns true, backup skips `dispatch_notifications()` entirely.
This means `heartbeat::mark_dispatched()` is never called. The heartbeat on disk has
`notifications_dispatched: false`.

The sentinel's current notification path doesn't read this flag — it uses its own
assessment diff via `build_notifications()`. So today, this works. The sentinel detects
the heartbeat mtime change, fires `BackupCompleted → Assess`, runs its own assessment,
and dispatches based on promise state changes.

**But the flag was designed for crash recovery.** The doc comment on the field says:
"Used for crash recovery: if false on next read, re-compute and re-send." If a future
session implements crash recovery by reading `notifications_dispatched`, it will find
`false` after every deferred-to-sentinel run and re-dispatch — even though the sentinel
already handled it. The flag's semantic contract ("false means notifications weren't
sent") is now violated: false means "either not sent, or sent by sentinel."

**Consequence:** No immediate bug. But a latent semantic mismatch that will cause
duplicate notifications if anyone implements the crash recovery path documented in the
field's own comment. This is one feature away from a real bug.

**Fix:** When deferring to sentinel, still call `heartbeat::mark_dispatched()`. The
sentinel doesn't use this flag for its assessment-based path — it will do its own
dispatch regardless. Marking it dispatched prevents the crash-recovery path from
double-firing. The backup is saying "I chose not to dispatch, and that's intentional"
rather than "I failed to dispatch." Alternative: add a third state
(`dispatched_by_sentinel`) but that's over-engineering for the current system.

### M1. `sentinel_is_running` does not check `schema_version`

**Severity: Moderate**

`read_sentinel_state_file` deserializes whatever JSON it finds. If a future sentinel
version changes the state file schema (adds required fields, changes semantics),
the old `sentinel_is_running` could parse a v2 file and misinterpret the PID field.

More concretely: if the state file is corrupt or from an incompatible version,
`serde_json::from_str` silently fails and returns `None`, which is the right behavior
(fail-open). But if it parses successfully with wrong semantics (unlikely but possible
with schema drift), the PID check could affirm a dead sentinel.

**Consequence:** Extremely unlikely with the current simple schema. But the pattern of
"parse JSON, trust the fields" without version checking is something to be aware of as
the state file evolves (VFM-B will add `visual_state`).

**Fix:** No code change needed now. Note for VFM-B: when `schema_version` increments,
add a version check to `read_sentinel_state_file` and return `None` for unknown versions.

### M2. `drive_connections` table grows unbounded

**Severity: Moderate**

The `drive_connections` table has no retention policy. Every mount and unmount event is
recorded forever. At a few events per day, this is ~1000 rows/year — negligible for
SQLite. But the table has no consumer that would notice if it grew large, and no cleanup
mechanism.

**Consequence:** No practical impact for years. But unlike the `operations` table (which
has `recent_runs(limit)` patterns suggesting bounded queries), `drive_connections` has a
`count(..., since)` query that scans the whole table if `since` is old enough.

**Fix:** Add a `PRAGMA` or periodic cleanup in a future session (e.g., delete events
older than 90 days during `init_schema` or `urd verify`). Not urgent.

### Minor. Stale blank line in backup.rs

`src/commands/backup.rs` line 583: extra blank line after `dispatch_notifications`.
Cosmetic.

### Commendation: C1. The `sentinel_is_running` placement is right

Moving this from backup.rs to sentinel_runner.rs (during simplify pass) was the correct
call. The function composes three sentinel-internal details (`sentinel_state_path`,
`read_sentinel_state_file`, `is_pid_alive`) into one intention-revealing API. Callers
don't need to know how liveness detection works — they ask a yes/no question. This is
the right abstraction level for a module boundary.

### Commendation: C2. Typed enums for drive events

`DriveEventType` and `DriveEventSource` replace raw strings for the SQLite layer. This
follows the project's "strong types over primitives" convention (CLAUDE.md) and prevents
a real class of bugs (swapped string parameters in a 3-`&str` function). The `as_str()`
pattern for SQLite serialization is clean and consistent.

### Commendation: C3. The dedup is genuinely fail-open

The `sentinel_is_running` function's error handling is correct at every level:
- File not found → `None` → false (no sentinel)
- Corrupt JSON → `None` → false (dispatch normally)
- Dead PID → false (stale file, dispatch normally)
- PID race → at worst, one run's notifications deferred to dead sentinel → self-heals next run

This matches ADR-107's "fail-open for backups" philosophy applied to the notification layer.

## 6. The Simplicity Question

**What could be removed?** The `drive_connection_count()` method has no consumer outside
tests. Its `since` parameter suggests a query pattern that doesn't exist yet. If sentinel
status doesn't need it, it's speculative. `last_drive_connection()` is also unused outside
tests but is the obvious query for a future `urd sentinel status` enhancement — more
defensible.

**What's earning its keep?** Everything else. The notification deduplication is ~15 lines
of meaningful code (the `sentinel_is_running` function + two guard clauses). The drive
recording is ~30 lines of wiring in the runner. The state.rs additions follow established
patterns exactly. The first-run heartbeat log is 8 lines that improve diagnostics. There's
no premature abstraction or speculative machinery.

## 7. For the Dev Team (Prioritized Action Items)

1. **Mark heartbeat dispatched when deferring to sentinel** (S1). In both guard clauses
   in `backup.rs` (lines 99-103 and 159-163), when `sentinel_is_running` is true, call
   `heartbeat::mark_dispatched()` before the log message. This preserves the flag's
   semantic contract ("false means no one handled notifications") and prevents the
   crash-recovery path from double-firing. ~4 lines added.

2. **Remove extra blank line** at `backup.rs:583`. Cosmetic.

3. **Consider removing `drive_connection_count`** if no consumer is planned for the near
   term. The `#[allow(dead_code)]` comment says "future" but doesn't name the feature.
   If it's for `urd sentinel status`, keep it. If it's speculative, delete it — it can be
   re-added in 30 seconds when needed.

## 8. Open Questions

1. **Should the sentinel mark `notifications_dispatched = true` after its assessment-based
   dispatch?** Currently the sentinel never writes to the heartbeat. If it did, the flag
   would be authoritative for both paths. But this adds a write dependency from sentinel
   to heartbeat that doesn't exist today. The simpler fix (S1: backup marks dispatched
   when deferring) avoids this coupling.

2. **Should `urd sentinel status` show last drive connection times?** The data is now
   being recorded. The `last_drive_connection()` method exists. Wiring it into the status
   display would give the feature a consumer and justify the `#[allow(dead_code)]`.
   Natural pairing with VFM-B.

---

*Reviewer: arch-adversary skill*
*Coverage: all changed files, 444 tests passing*
