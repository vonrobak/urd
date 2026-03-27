# Arch-Adversary Review: Sentinel Implementation Plan (5b + 5c)

**Project:** Urd — BTRFS Time Machine for Linux
**Date:** 2026-03-27
**Scope:** Design proposal — `docs/95-ideas/2026-03-27-design-sentinel-implementation.md`
**Review type:** Design review (pre-implementation)
**Prior review:** `docs/99-reports/2026-03-26-sentinel-design-review.md` (addressed all findings)
**Commit:** 5dedec7 (master)
**Reviewer:** Claude (arch-adversary)

---

## 1. Executive Summary

The implementation plan is well-sequenced and makes disciplined choices — subprocess spawning for active mode, no new crate dependencies, polling fallback for unreliable inotify. But the design has a structural gap in the Sentinel's `Assess` action: it calls `awareness::assess()` which requires a `&dyn FileSystemState`, which in production requires `RealFileSystemState` which takes an optional `&StateDb` reference. The Sentinel runner will need to hold a `StateDb` connection open for the lifetime of the daemon — that's an open database connection for hours or days. The design doesn't discuss this, and it has implications for lock contention with `urd backup` (which also opens `StateDb`). The circuit breaker and notification deduplication are correctly designed. The session sequencing is sound.

## 2. What Kills You (Catastrophic Failure Proximity)

The catastrophic failure mode for Urd is **silent data loss**. For the Sentinel specifically:

**Cascade-triggering that exhausts disk space** — The circuit breaker design correctly addresses this, including the hardest case (partial failures on `ScheduledRun` count as failures). The catastrophic storage failure history is clearly shaping these decisions. **Distance: 3+ bugs away.** The circuit breaker would need to fail, the min_interval would need to be bypassed, AND space estimation would need to fail. This is well-defended.

**Lock contention preventing legitimate backups** — The subprocess approach makes this safer: the Sentinel never holds the backup lock itself. It only spawns `urd backup`, which acquires and releases the lock normally. The Sentinel can't accidentally hold the lock forever. **Distance: not reachable.** This is a good consequence of the subprocess decision.

**False confidence from a dead Sentinel** — The Sentinel crashes, the state file goes stale, but the user believes it's still watching. `urd backup` then defers notification dispatch because `sentinel_is_running()` reads the stale state file and finds the PID alive (but it's been reassigned to a different process). Notifications are lost. **Distance: 2 bugs away.** This is the closest thing to a live wire in this design.

## 3. Scorecard

| Dimension | Score | Rationale |
|-----------|-------|-----------|
| **Correctness** | 4 | Pure state machine is well-bounded. Circuit breaker semantics resolved. The `FileSystemState` lifetime question is a real gap but solvable. |
| **Security** | 4 | Subprocess spawning avoids giving the Sentinel sudo. No new attack surface beyond what `urd backup` already has. Lock metadata is advisory, not security-critical. |
| **Architectural Excellence** | 4 | Clean separation of pure state machine from I/O runner. Subprocess approach maintains the single-entry-point invariant for backup execution. Module boundaries are right. One concern: `sentinel.rs` mixes state machine logic and circuit breaker logic — these are independently testable and might warrant separation if the file grows. |
| **Systems Design** | 3 | Session 2 underestimates the difficulty of epoll+inotify+timerfd multiplexing. The design names the OS primitives but doesn't address the interaction patterns (what if two inotify events arrive in the same epoll_wait? what ordering guarantees exist?). The PID-based Sentinel detection has a known race condition that the design acknowledges but doesn't resolve. |

## 4. Design Tensions

### Tension 1: Subprocess vs. In-Process Backup Triggering

The design spawns `urd backup` as a subprocess for active mode rather than calling `commands::backup::run()` directly. This is the right call for three reasons:

1. **sudo isolation** — the Sentinel runs as a user daemon without special privileges. `urd backup` has its own sudoers configuration for btrfs operations. In-process execution would require the Sentinel to inherit sudo context.
2. **Single entry point** — all backup execution flows through `urd backup`, regardless of trigger source. Testing, logging, and lock acquisition all use the same code path.
3. **Crash isolation** — if the backup panics or gets killed by OOM, the Sentinel survives. In-process execution would bring down the entire daemon.

**The cost** is process coordination: the Sentinel must wait for the subprocess, read the heartbeat to evaluate results, and handle cases where the process exits abnormally. But this cost is well-bounded and the benefits clearly outweigh it.

### Tension 2: epoll Complexity vs. Simple Polling

The design proposes epoll+inotify+timerfd multiplexing — three distinct Linux kernel APIs wired together through a fourth. This is the "correct" architecture for a daemon, but it's also the highest-risk session in the plan. The alternative — a simple `loop { sleep(5); check_everything(); }` — is crude but trivially correct and testable.

The design acknowledges this with "If session 2 reveals inotify problems: Fall back to polling-only." This is good risk awareness. But the fallback should be the *starting* implementation, not the contingency. Build the simple poll loop first, prove everything works, then add inotify as an optimization. The state machine doesn't care how events arrive — that's the whole point of the pure/impure separation.

**Recommendation:** Session 2 should implement a poll loop first. inotify+epoll becomes Session 2b or gets folded into Session 3's "hardening" scope. This de-risks the highest-uncertainty session.

### Tension 3: PID-Based Sentinel Detection vs. Explicit Protocol

Backup.rs checks if the Sentinel is running by reading `sentinel-state.json` and verifying the PID is alive. This is a heuristic, not a protocol. The race conditions are real: PID reuse (a different process now has that PID), file staleness (Sentinel wrote the file then crashed, systemd hasn't restarted it yet), and timing (Sentinel is between state file writes when backup checks).

The alternative is explicit coordination: a Unix socket, a shared flag file with flock, or simply making the decision static via config (`sentinel_manages_notifications = true`). The last option is the simplest and has no race conditions — if the user has enabled the Sentinel, backup always defers notifications. The Sentinel is responsible for dispatching even if it was briefly down (it re-sends on startup for `notifications_dispatched: false` heartbeats).

### Tension 4: Drive Connections Table — Consumer-Driven vs. Speculative

The prior review flagged this as "borderline premature." This implementation plan puts it in Session 3 without adding a consumer. The table records events that nothing queries. The counter-argument (cheap to record, expensive to retrofit) is valid *if* the consumer is on the roadmap. It is — drive topology constraints and promise achievability are listed in status.md's Priority 3d gate. But "on the roadmap" and "designed" are different things. The recording side can't be tested end-to-end without the query side.

**Recommendation:** Implement the recording, but add one concrete query to prove the schema works: `urd sentinel status` should show "WD-18TB: last connected 5 days ago" using `last_drive_connection()`. That's a real consumer that exercises the table and provides immediate user value.

## 5. Findings

### Significant

**S1. The Sentinel's `Assess` action requires `FileSystemState`, which requires `StateDb`.**

The `awareness::assess()` function takes `&dyn FileSystemState`. In production, this is `RealFileSystemState<'a>` which holds an `Option<&'a StateDb>`. The Sentinel runner calls `assess()` on every tick (potentially every 2 minutes). This means either:

(a) The runner holds a `StateDb` connection open for the daemon's lifetime, or
(b) The runner opens and closes `StateDb` on every assess tick.

Option (a) risks SQLite lock contention with `urd backup` (which also opens `StateDb`). SQLite's default locking (journal mode = delete) allows concurrent readers but only one writer. If the Sentinel is reading while backup is writing, that's fine. But if both try to write simultaneously (Sentinel recording drive events, backup recording operations), one will get SQLITE_BUSY.

Option (b) has overhead but avoids contention. Given that assess ticks are at most every 2 minutes and the DB open is <1ms, this is the safer choice.

*Consequence:* If ignored, the Sentinel could intermittently fail to record drive events or (worse) cause `urd backup` to fail a state DB write. Per ADR-102, SQLite failures don't prevent backups, so the blast radius is contained — but it's still a source of spurious warnings.

*Recommendation:* Design decision needed before Session 2. Document that the runner opens `StateDb` per-tick (option b), not on startup. For the `drive_connections` insert (Session 3), use a short-lived connection: open, insert, close. This follows the "SQLite is history, not truth" principle — a failed insert is logged and dropped.

**S2. PID reuse makes `sentinel_is_running()` unreliable for notification deduplication.**

Linux PIDs are reused after a process dies. On a system with moderate process churn, a PID can be reassigned within seconds. The scenario:

1. Sentinel writes `sentinel-state.json` with PID 12345
2. Sentinel crashes
3. systemd hasn't restarted it yet (RestartSec=30)
4. Some unrelated process starts with PID 12345
5. `urd backup` reads state file, checks `kill(12345, 0)` → alive
6. Backup defers notification dispatch to a Sentinel that doesn't exist
7. Notifications are lost for this run

The prior design's `notifications_dispatched` field mitigates this: the *next* Sentinel startup will re-send for `notifications_dispatched: false`. But between the crash and the restart (up to 30 seconds), any backup run loses its notifications.

*Consequence:* Up to one backup run's notifications are lost per Sentinel crash. Not catastrophic, but undermines the "impossible to miss failures" UX principle.

*Recommendation:* Replace PID detection with a **config-driven decision**: add `notifications.dispatch_by = "backup"` (default, current 5a behavior) vs `"sentinel"`. When the user enables the Sentinel, they set `dispatch_by = "sentinel"`. No race condition, no PID checking, no stale state files. The trade-off is that the user must edit config — but they already edit config to enable the Sentinel. This could be a single field on the `[sentinel]` section rather than a separate notifications field: if `[sentinel]` exists and the daemon is meant to be running, backup defers. The "meant to be running" part is a config declaration, not a runtime detection.

**S3. `should_trigger_backup()` needs assessments that the state machine doesn't have.**

The data flow for active mode (5c) has a sequencing problem:

```
sentinel_transition(state, DriveMounted) → actions: [Assess, LogDriveChange, ...]
                                                      ↓
runner.execute_assess() → calls awareness::assess() → gets assessments
                                                      ↓
should_trigger_backup(state, event, assessments) → trigger decision
```

But `should_trigger_backup()` is listed as a pure function in `sentinel.rs` (Session 1), while the assessments it needs are only available after the runner executes the `Assess` action (Session 2+). The function is defined in Session 1 but can't be meaningfully tested until the runner provides real assessments.

More importantly, the trigger decision happens *after* the state machine transition returns. This means the trigger is not part of the state machine — it's a second decision layer in the runner. This is fine architecturally, but the design presents it as if it's part of the pure state machine when it's actually part of the impure runner.

*Recommendation:* Make this explicit in Session 1's scope. `should_trigger_backup()` belongs in `sentinel.rs` as a pure function, but it's not called by `sentinel_transition()` — it's called by the runner after executing the `Assess` action. Tests in Session 1 can exercise it with synthetic assessment data. The design doc should clarify this is a runner-level decision, not a state machine transition.

### Moderate

**M1. Session 2 underestimates epoll+inotify+timerfd complexity.**

The design budgets Session 2 as "1 session" and compares it to "heartbeat + notification work combined." But the I/O multiplexing layer has different complexity characteristics:

- inotify returns *raw event buffers* that must be parsed. Events for different watches arrive in the same buffer.
- epoll can return multiple ready fds in a single `epoll_wait()` call. The design doesn't discuss event batching or ordering.
- timerfd needs `TFD_TIMER_ABSTIME` for predictable scheduling. The nix crate's timerfd API has footguns around `TimerSpec` construction.
- Signal handling via `ctrlc` crate is callback-based, which doesn't compose cleanly with epoll's fd-based model. The design may need `signalfd` instead (another nix feature).

None of these are blockers, but together they represent significantly more syscall-level debugging than any prior Urd session.

*Recommendation:* Either (a) budget Session 2 as 1.5–2 sessions, or (b) start with a poll loop as recommended in Tension 2 and add inotify optimization later. Option (b) is strongly preferred — it lets the Sentinel ship and stabilize with polling while inotify becomes a performance improvement, not a launch blocker.

**M2. Lock file metadata write is not atomic with lock acquisition.**

The design says: "Both write PID + timestamp + trigger after acquiring." But there's a window between acquiring the flock and writing the metadata. If the process crashes in that window, the lock file exists (empty or with stale data) and another process reading `read_lock_info()` gets garbage.

*Consequence:* `urd backup`'s "Another backup is running" message shows wrong or missing metadata. Not dangerous — the lock itself (flock) is correct — but the UX degrades.

*Recommendation:* `read_lock_info()` must handle: empty file, invalid JSON, and valid JSON with a PID that no longer exists. All three should produce a reasonable message: "Another backup may be running (lock held, details unavailable)." This is a test case, not a redesign.

**M3. The design doesn't address Sentinel startup initialization.**

When the Sentinel starts, it needs to:
1. Scan currently mounted drives (initial `mounted_drives` state)
2. Read the current heartbeat (to establish `last_promise_states` baseline)
3. Load persisted circuit breaker state from `sentinel-state.json`
4. Create inotify watches (which may fail if paths don't exist yet)

If the heartbeat doesn't exist (fresh installation, first run before any backup), `last_promise_states` is empty. The first assessment tick will compute promise states and compare against an empty map — every subvolume will appear to have "transitioned" from nothing to its current state. This triggers spurious notifications on Sentinel startup.

*Consequence:* User gets a flood of notifications every time the Sentinel starts or restarts.

*Recommendation:* On first assessment (when `last_promise_states` is empty), populate the map from the assessment results *without* dispatching notifications. This is the same logic as `compute_notifications(None, current)` — when there's no previous state, don't notify. But the Sentinel's notification path doesn't go through `compute_notifications()` — it compares `last_promise_states` directly. The runner must replicate this "first run = no notifications" logic. Add an explicit test: `test_first_assessment_after_startup_no_notifications`.

**M4. `SuccessExitStatus=4` in the timer unit is fragile.**

The design proposes making exit code 4 a success in the systemd timer unit to handle lock contention. But exit code 4 is currently `urd backup`'s code for "lock held" — it's an application-level convention, not a standard. If any other error path accidentally returns exit code 4, systemd will silently swallow it. And the user must remember to add this to their service unit — it's not in the existing `urd-backup.service`.

*Recommendation:* Instead of `SuccessExitStatus=4`, have `urd backup` distinguish "lock held by Sentinel" (which is expected and fine) from "lock held by unknown" (which might be a real problem). When the lock is held and `read_lock_info()` returns `trigger: "sentinel"`, exit 0 with a message "Backup already in progress (Sentinel-triggered), skipping." When the lock is held by anything else, exit 4 as today. This way, timer+Sentinel overlap produces a clean exit without requiring systemd unit changes.

### Commendation

**C1. Subprocess spawning for active mode is the right call.**

This was the first question in the "Ready for Review" section, and the answer is clearly yes. The design correctly identifies the three benefits (sudo isolation, single entry point, crash isolation) and the costs are bounded. In the prior review, "lock contention preventing legitimate backups" was identified as a catastrophic risk. The subprocess approach eliminates it entirely — the Sentinel never holds the backup lock. This is a design choice that makes an entire failure category unreachable.

**C2. No new crate dependencies.**

Adding `nix` feature flags instead of new crates is disciplined. The project has 12 dependencies (counting Cargo.toml); each new one adds supply chain risk, compile time, and API surface to learn. Using `nix::sys::inotify` instead of the `inotify` crate means one fewer dependency to audit and one fewer API to learn. The `curl` subprocess decision follows the same principle.

**C3. The fallback strategy is honest.**

"If session 2 reveals inotify problems: Fall back to polling-only" is the kind of sentence that makes a design trustworthy. It acknowledges uncertainty without hand-waving. The state machine's indifference to event sources makes this fallback structurally sound, not just aspirational.

**C4. Open questions are genuinely resolved, not deferred with new names.**

The five open questions from the prior design are each resolved with a clear decision and rationale. "Require restart" for config reload is the right v1 choice. "Defer" for tray icon/Unix socket avoids building infrastructure without a consumer. These are the simplicity decisions that keep the project moving.

## 6. The Simplicity Question

> **What could be removed?**

**The `drive_connections` table (Session 3)** has no consumer in this design. The recording infrastructure is ~60 lines of code (schema + insert + queries) that exercises no production code path. If the table is implemented, it should have at least one visible consumer (`urd sentinel status` showing last connection time, as recommended in Tension 4).

**`WriteState` as a separate action** — the state file write happens on every Assess action, and only on Assess actions. It's not a decision the state machine makes; it's a consequence of assessment. The runner can write the state file as part of `execute_assess()` rather than as a separate action. This removes an enum variant and simplifies the transition function.

> **What's earning its keep?**

**The pure state machine** (`sentinel_transition`) is earning its keep. It makes the hardest part of the daemon — "what should happen in response to this event?" — testable without inotify, epoll, or timers. This is the same pattern as the planner/executor separation, which has proven itself across 318 tests.

**The circuit breaker** is earning its keep despite being 5c infrastructure built in Session 1. The partial-failure semantics (evaluate against the trigger condition, not global pass/fail) are the kind of detail that only surfaces in design review and would be a bug in production.

## 7. For the Dev Team (Prioritized Action Items)

1. **Decide on `StateDb` connection lifetime for the Sentinel runner** (S1). Document that the runner opens `StateDb` per-tick, not on startup. This avoids SQLite lock contention with concurrent `urd backup` runs. This is a design decision, not a code change — document it in the implementation plan before Session 2.

2. **Replace PID-based `sentinel_is_running()` with a config-driven approach** (S2). Add `notifications.dispatch_by = "backup" | "sentinel"` (or a field on `[sentinel]` that implies notification ownership). This eliminates the PID reuse race condition. The trade-off (user must edit config) is acceptable because users already edit config to enable the Sentinel.

3. **Start Session 2 with a poll loop, not epoll** (M1, Tension 2). Implement `loop { sleep(adaptive_tick); check_mounts(); check_heartbeat(); }` first. Prove the runner works. Add inotify+epoll as an optimization in Session 3 or later. This de-risks the highest-uncertainty session and lets passive mode ship sooner.

4. **Handle Sentinel startup without spurious notifications** (M3). On first assessment after startup (when `last_promise_states` is empty), populate the map without dispatching. Add test: `test_first_assessment_after_startup_no_notifications`.

5. **Make `should_trigger_backup()` explicitly a runner-level decision** (S3). The function lives in `sentinel.rs` (pure), but it's called by the runner after executing `Assess`, not by `sentinel_transition()`. Clarify this boundary in the design and in code comments. Session 1 tests use synthetic assessment data; Session 4 tests use the full flow.

6. **Handle lock metadata gracefully** (M2). `read_lock_info()` must handle empty file, invalid JSON, and stale PID. Tests: `test_read_lock_info_empty_file`, `test_read_lock_info_corrupt_json`, `test_read_lock_info_dead_pid`.

7. **Use exit code 0 for "lock held by Sentinel"** instead of `SuccessExitStatus=4` (M4). When `read_lock_info()` returns `trigger: "sentinel"`, `urd backup` exits 0 with a message. This avoids fragile systemd unit configuration.

8. **Wire `last_drive_connection()` into `urd sentinel status`** (Tension 4). Gives the `drive_connections` table a real consumer and provides immediate user value.

9. **Consider removing `WriteState` from `SentinelAction` enum** (Simplicity). The state file write is always a consequence of `Assess`, never an independent action. The runner writes it as part of `execute_assess()`.

## 8. Open Questions

1. **SQLite WAL mode.** If both the Sentinel and `urd backup` open `StateDb` (even briefly and non-overlapping), should Urd switch to WAL mode for better concurrent read/write behavior? WAL mode is a one-line change (`PRAGMA journal_mode=WAL`) but changes the on-disk format (`.db-wal` and `.db-shm` files appear). This is a backward-compatibility consideration per ADR-105. Probably the right move, but needs a conscious decision.

2. **Sentinel state file and heartbeat file atomicity.** Both use temp+rename for atomic writes. But the Sentinel reads the heartbeat and writes its own state file. If the Sentinel crashes between reading the heartbeat and acting on it, is there a consistency gap? Specifically: if `urd backup` writes a heartbeat with `notifications_dispatched: false` and the Sentinel reads it but crashes before dispatching, the next Sentinel startup should re-dispatch. Does the current design handle this? (Answer: yes, because on startup the Sentinel reads the heartbeat fresh and dispatches if `notifications_dispatched: false`. But this should be an explicit test.)

3. **What happens if `std::env::current_exe()` fails?** The Sentinel spawns `urd backup` via `current_exe()`. On some Linux systems with non-standard `/proc` configurations, this can return an error. The design should have a fallback (config-specified binary path, or just hardcode `"urd"`).

---

*Review conducted as architectural adversary. Findings reflect design-phase analysis; implementation may resolve or introduce additional concerns.*
