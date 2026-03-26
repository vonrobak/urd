# Arch-Adversary Review: Sentinel Design
**Project:** Urd — BTRFS Time Machine for Linux
**Date:** 2026-03-26
**Scope:** Design proposal — `docs/95-ideas/2026-03-26-design-sentinel.md`
**Review type:** Design review (pre-implementation)

---

## 1. Executive Summary

The design makes the right big decision — decomposing the Sentinel into three independently shippable components (5a/5b/5c) with strict dependency ordering — and it correctly separates the pure state machine from the I/O runner. However, the notification-before-heartbeat-write ordering has a crash-consistency gap that can cause duplicate or lost notifications, the circuit breaker lacks a time-based reset for the "permanent half-dead" state, and the `SentinelState` carries cached assessments that create a subtle divergence risk between the Sentinel's view and the awareness model's ground truth.

## 2. What Kills You (Catastrophic Failure Proximity)

The catastrophic failure mode for Urd is **silent data loss**. For the Sentinel specifically: **cascade-triggering that exhausts disk space**, or **lock contention that prevents legitimate backups**.

**Cascade risk.** The circuit breaker addresses this, but incompletely. The design caps at 3 failures with exponential backoff — good. But consider: what if the failure is *partial* (2 of 5 subvolumes fail)? The design doesn't define whether a partial success counts as a failure for circuit breaker purposes. If partial success resets the failure count, a consistently-partial-failing backup never trips the breaker and runs every hour indefinitely, creating snapshot congestion. Given Urd's catastrophic storage failure history from snapshot congestion, this is the closest thing to a live wire in this design.

**Lock preventing legitimate backups.** The `flock(2)` mechanism is crash-safe and sound for single-machine use. The real risk is the Sentinel holding the lock during a long external send (hours for multi-TB subvolumes) while the user manually runs `urd backup` and gets a terse rejection. The user has no way to know *why* the lock is held or *when* it will release. This isn't data loss, but it is a "trust failure" — the user loses confidence in the tool.

**Notification masking real problems.** If notification dispatch fails silently (webhook timeout, `notify-send` not installed), the user believes the Sentinel is watching when it is not. The design has a `Log` channel that's "always enabled," which mitigates this, but only if someone reads the log. The system should detect notification delivery failures and escalate — a meta-notification problem.

## 3. Scorecard

| Dimension | Score | Rationale |
|-----------|-------|-----------|
| **Correctness** | 4 | Pure state machine is well-bounded; crash-consistency gap in notification ordering and partial-failure circuit breaker semantics are fixable before implementation. |
| **Security** | 3 | `Command` notification channel executes arbitrary scripts from config — acceptable for a local tool, but the design doesn't discuss permission model, path validation, or what happens if config is writable by another user. `flock` is advisory-only. |
| **Architectural Excellence** | 4 | Clean decomposition, correct dependency ordering, pure/impure boundary is well-drawn. The `SentinelState` accumulation problem and the two-notification-path issue (5a-standalone vs 5b-integrated) slightly muddy the architecture. |
| **Systems Design** | 3 | Missing several operational concerns: no observability story for the daemon itself, no health check endpoint, no way to inspect circuit breaker state, no graceful degradation when inotify watches are exhausted. Assessment interval is fixed when it should be adaptive. |

## 4. Design Tensions

### Tension 1: Cached State vs. Recomputation

`SentinelState.last_assessments` caches promise states in the daemon's memory. The awareness model is a pure function that recomputes from filesystem truth. If anything changes the filesystem between Sentinel ticks (manual snapshot deletion, another tool touching btrfs), the cached state diverges from truth. The design partially addresses this with the assessment tick, but the tick interval (5 minutes) means up to 5 minutes of stale state. The real question: should `SentinelState` store assessments at all, or should every event that might trigger a notification force a fresh `assess()` call? The latter is simpler and truer to "filesystem is truth."

### Tension 2: 5a Standalone vs. 5a-in-5b

The design ships 5a as a post-backup hook embedded in `commands/backup.rs`. When 5b ships, the Sentinel also dispatches notifications. Now there are two notification paths: backup-embedded and Sentinel-driven. The design acknowledges this in Open Question 6 but doesn't resolve it. If both paths exist, the user can receive duplicate notifications (backup finishes, dispatches notification; Sentinel detects heartbeat change, re-assesses, dispatches again if state changed). The mitigation is that the Sentinel compares cached state to new state and only notifies on *change*, but the timing is fragile.

### Tension 3: Simplicity vs. Completeness in Drive Tracking

The `drive_connections` table is forward-looking infrastructure for promise achievability. But the awareness model doesn't use it yet, and the design doesn't specify when it will. This is speculative schema. The counter-argument — that it's cheap to record and expensive to retrofit — is valid, but it's also how tables accumulate without consumers. The design should commit: either define the achievability query that uses this data (even as a TODO with a concrete signature), or defer the table until a consumer exists.

### Tension 4: Timer Coexistence vs. Timer Replacement

The design correctly defers timer replacement. But the coexistence model has an unaddressed edge case: the systemd timer fires at 04:00, and the Sentinel's active mode also decides to trigger a backup at 03:58 (because a drive was just mounted). The lock prevents concurrent runs, so the 04:00 timer run gets rejected. The user's expected daily backup didn't happen from the timer's perspective. systemd will report the timer unit as failed. This is cosmetically bad and operationally confusing.

## 5. Findings

### Critical

*None.* The design avoids the worst traps. No finding rises to "do not build this."

### Significant

**S1. Partial-failure circuit breaker semantics are undefined.**
The circuit breaker counts "consecutive failures" but the design doesn't define what constitutes a failure when some subvolumes succeed and others fail. In the executor, `ExecutionResult` has an `overall` field that can be "partial." If partial success doesn't increment the failure counter, a consistently-partial-failing configuration runs auto-triggered backups every `min_interval` forever, potentially creating snapshot congestion. If partial success *does* increment it, a single flaky subvolume disables auto-backups for all subvolumes.

*Recommendation:* Define circuit breaker failure as "the specific trigger condition was not resolved." If the trigger was `DriveMounted` and the external sends all failed, that's a failure. If some sends succeeded, the trigger condition is partially resolved — reset the counter but don't re-trigger for the same drive within `min_interval`.

**S2. Crash window between notification computation and heartbeat write.**
The design reorders to: read old heartbeat, compute assessments, compute notifications, write new heartbeat, dispatch. If the process crashes after computing notifications but before writing the heartbeat, the next run will re-read the old heartbeat, re-compute the same state transition, and send duplicate notifications. If the process crashes after writing the heartbeat but before dispatching, the notification is lost entirely (the state transition is recorded but never communicated). The design acknowledges this subtlety but doesn't resolve it.

*Recommendation:* Accept this as a known trade-off and document it explicitly. Duplicate notifications on crash are better than lost notifications. Add a "last notification state" field to the heartbeat so that on recovery, the system can detect "I wrote a heartbeat but never confirmed notification dispatch" and re-send.

**S3. `SentinelState.backup_in_progress` has no timeout.**
The state tracks whether a backup is running, but there's no mechanism to detect a hung backup. If `urd backup` acquires the flock and then hangs (waiting on a stuck btrfs send, for example), `backup_in_progress` stays true indefinitely. The Sentinel will never auto-trigger, and the user gets no notification that something is wrong. The `flock` will be held until the process is killed.

*Recommendation:* The Sentinel should monitor the heartbeat file's `stale_after` timestamp. If the current time exceeds `stale_after` and no new heartbeat has appeared, emit a `BackupOverdue` notification regardless of lock state. This doesn't require knowing about the lock — it uses the existing staleness mechanism.

### Moderate

**M1. `inotify` on `/proc/mounts` is not universally reliable.**
The design recommends `inotify` on `/proc/mounts` for instant drive detection. `/proc/mounts` is a procfs virtual file. While Linux does generate `inotify` events on it, this behavior is an implementation detail, not a POSIX guarantee. Some container runtimes (Flatpak, certain Docker configurations) present a static `/proc/mounts`. FUSE mounts and automount entries may not trigger inotify. Additionally, LUKS unlock + mount is a two-step process; the inotify fires on the mount step but the design should handle the case where inotify fires but the mount isn't yet fully settled (UUID verification may fail transiently).

*Recommendation:* Use inotify as the primary mechanism but implement polling as a fallback. On each inotify event, wait 500ms before checking (debounce for LUKS settle time). Run a polling sweep every 60 seconds regardless of inotify to catch missed events. Document the container/FUSE limitations.

**M2. `Command` notification channel is an unaudited execution path.**
`NotificationChannel::Command { path, args }` executes an arbitrary binary with arbitrary arguments. The config file is user-owned, so this is "user executes their own code" — acceptable. But: if the config file is world-writable (misconfiguration), or if the Sentinel runs as a systemd user service with broader permissions than expected, this becomes a privilege escalation vector. The design doesn't validate the command path or check file permissions.

*Recommendation:* At config load time, verify the command path exists and is owned by the current user. Warn (don't block) if the config file itself has overly permissive permissions. Document that the `Command` channel runs with the Sentinel's privileges.

**M3. No observability for the Sentinel daemon itself.**
The design covers Prometheus metrics for backup operations but doesn't define any metrics or health indicators for the Sentinel process. There's no way to answer: "Is the Sentinel running? When did it last assess? Is the circuit breaker open? How many events has it processed?" For a daemon that's supposed to be the "invisible worker," this is a gap — the user needs to trust that it's working.

*Recommendation:* Write a Sentinel-specific heartbeat (or extend the existing one) with: last assessment time, circuit breaker state, event count since startup, current mounted drives. Expose via `urd sentinel status` subcommand. This is low effort and high diagnostic value.

**M4. Assessment tick interval should be adaptive, not fixed.**
A 5-minute tick when all promises are PROTECTED is wasteful, especially on laptops (battery, disk wake). A 5-minute tick when promises are UNPROTECTED may be too slow. The design raises this in Open Question 2 but doesn't propose a solution.

*Recommendation:* Three tiers: all PROTECTED = 15 minutes, any AT RISK = 5 minutes, any UNPROTECTED = 2 minutes. Simple, no config needed, aligns urgency with polling frequency.

### Minor

**m1. `flock(2)` doesn't identify the lock holder.**
The design notes this and suggests writing PID to the lock file after acquiring it. This should be a firm requirement, not a suggestion. When the user runs `urd backup` and gets "Another backup is running," the next question is always "what backup? since when?" The lock file should contain PID, start time, and trigger source (manual/timer/sentinel).

**m2. Notification deduplication across 5a and 5b needs an explicit contract.**
When 5b ships, the backup-embedded notification path (5a) should be disabled if the Sentinel is running, or the Sentinel should detect that notifications were already dispatched for a given heartbeat write. Without this contract, the two paths will produce duplicates. A simple mechanism: include a `notifications_dispatched: bool` field in the heartbeat. The Sentinel skips notification if the flag is true.

### Commendation

**C1. The 3-component decomposition is the right call.**
Shipping 5a as a post-backup hook is the fastest path to user value. The strict 5a-then-5b-then-5c ordering means each component is proven before the next adds complexity. This is disciplined engineering that resists the temptation to build the exciting part (active mode) first.

**C2. Pure state machine with I/O runner separation.**
`sentinel_transition()` as a pure function is exactly right. It makes the hardest part of the Sentinel (the decision logic) trivially testable. The 20+ pure tests for the state machine will catch edge cases that integration tests would miss. This follows the planner/executor pattern that already works well in the codebase.

**C3. Coexistence with the systemd timer.**
Not replacing the timer in v1 is a mature decision. The timer is proven. The Sentinel is new. Running both with a lock file is safer than a hard cutover. This shows lessons learned from the catastrophic storage failure.

## 6. The Simplicity Question

> "Should 5a be part of 5b instead of standalone?"

No. 5a standalone is correct. The notification dispatcher is a pure function + I/O dispatch layer. Embedding it in the backup command gives immediate value (users get notifications today) with zero daemon complexity. The cost is a second notification path when 5b arrives, but that's solvable with a simple deduplication flag (see m2). The alternative — waiting for 5b to ship any notifications — delays user value by 2-3 sessions for no architectural benefit.

> "Is `SentinelState` carrying too much?"

Yes, slightly. `last_assessments` should not be cached in state. The awareness model is designed to be called on demand, and caching its output creates a divergence risk. The Sentinel should store only: mounted drives (for event deduplication), last assessment *time* (for tick scheduling), and circuit breaker state (for trigger decisions). Promise states should be recomputed on every Assess action by calling `awareness::assess()` fresh. This makes the state machine simpler and the truth model cleaner.

> "Is the `drive_connections` table premature?"

Borderline. It's cheap to implement and harmless to have. But it has no consumer in this design. Recommendation: implement the table as part of 5b (it's ~20 lines of schema + insert), but do not build any query infrastructure until the achievability model is designed. If the table sits unused for two priority cycles, reconsider.

## 7. For the Dev Team (Prioritized Action Items)

1. **Define partial-failure semantics for the circuit breaker** before implementing 5c. This is the closest thing to a cascade risk in the design. Write it into the design doc as a decision, not an open question.

2. **Remove `last_assessments` from `SentinelState`.** Recompute promise states on every Assess action. This eliminates the divergence risk and simplifies the state machine. The performance cost is negligible (assess reads a few directories and pin files).

3. **Add a `BackupOverdue` notification event** that fires based on heartbeat staleness, independent of lock state. This catches hung backups that neither the circuit breaker nor the lock mechanism can detect.

4. **Design the 5a/5b notification deduplication contract** before shipping 5b. A `notifications_dispatched` field in the heartbeat is the simplest mechanism. Document it now so 5a's implementation includes the field.

5. **Implement inotify-with-polling-fallback** for `/proc/mounts` rather than committing to either alone. The debounce delay for LUKS settle time should be explicit in the design.

6. **Write PID + timestamp + trigger source into the lock file** as a firm requirement, not a suggestion. This is a single-session addition that dramatically improves the "another backup is running" user experience.

7. **Define Sentinel observability** (last assessment time, circuit breaker state, event count) before shipping 5b. Even a simple `urd sentinel status` subcommand is sufficient for v1.

## 8. Open Questions

1. **What is the circuit breaker's reset mechanism when the Sentinel restarts?** The design says circuit state is "persisted in heartbeat or a separate state file" but doesn't decide. If persisted in the heartbeat, a manual `urd backup` that writes a new heartbeat could inadvertently reset the circuit breaker. If in a separate file, that's another backward-compatibility contract.

2. **Should the Sentinel hold sudo credentials?** Backup execution requires `sudo btrfs`. If the Sentinel auto-triggers a backup, does it need passwordless sudo? The systemd timer presumably runs with the user's sudoers configuration, but a user-level daemon may not have the same privilege context. This is an operational concern that affects whether 5c is deployable.

3. **What happens to in-flight notifications when the Sentinel receives SIGTERM?** The design handles SIGTERM as clean shutdown, but if a webhook POST is in-flight, does it complete or get interrupted? For `Command` channels, does the child process get orphaned?

4. **Is the 5-minute assessment tick the right default for the heartbeat file watch?** If the Sentinel already watches the heartbeat via inotify, and `urd backup` writes the heartbeat on every run, the assessment tick is only needed for detecting drive changes and time-based staleness transitions. The tick could be longer (15 minutes) with inotify handling the real-time cases.

5. **How does config reload interact with the circuit breaker?** If the user edits `urd.toml` to fix a broken drive path and sends SIGHUP, should that reset the circuit breaker? The failure was caused by misconfiguration, so continuing to penalize with the open circuit is wrong. But auto-resetting on config reload could mask persistent issues.

---

*Review conducted as architectural adversary. Findings reflect design-phase analysis; implementation may resolve or introduce additional concerns.*
